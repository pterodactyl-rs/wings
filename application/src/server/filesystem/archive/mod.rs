use cap_std::fs::PermissionsExt as _;
use chrono::{Datelike, Timelike};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::Permissions,
    io::{Seek, SeekFrom, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufReader},
};
use utoipa::ToSchema;

pub mod multi_reader;

#[derive(Clone, Copy)]
pub enum CompressionType {
    None,
    Gz,
    Xz,
    Bz2,
    Lz4,
    Zstd,
}

#[derive(Clone, Copy, ToSchema, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
#[schema(rename_all = "snake_case")]
pub enum CompressionLevel {
    #[default]
    BestSpeed,
    GoodSpeed,
    GoodCompression,
    BestCompression,
}

impl CompressionLevel {
    #[inline]
    pub fn flate2_compression_level(self) -> flate2::Compression {
        match self {
            CompressionLevel::BestSpeed => flate2::Compression::new(1),
            CompressionLevel::GoodSpeed => flate2::Compression::new(3),
            CompressionLevel::GoodCompression => flate2::Compression::new(6),
            CompressionLevel::BestCompression => flate2::Compression::new(9),
        }
    }

    #[inline]
    pub fn zstd_compression_level(self) -> i32 {
        match self {
            CompressionLevel::BestSpeed => 1,
            CompressionLevel::GoodSpeed => 7,
            CompressionLevel::GoodCompression => 13,
            CompressionLevel::BestCompression => 22,
        }
    }
}

#[derive(Clone, Copy)]
pub enum ArchiveType {
    None,
    Tar,
    Zip,
    Rar,
    SevenZip,
    Ddup,
}

#[inline]
pub fn zip_entry_get_modified_time(
    entry: &zip::read::ZipFile<impl std::io::Read>,
) -> Option<std::time::SystemTime> {
    for field in entry.extra_data_fields() {
        if let zip::extra_fields::ExtraField::ExtendedTimestamp(ext) = field
            && let Some(mod_time) = ext.mod_time()
        {
            return Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mod_time as u64));
        }
    }

    if let Some(time) = entry.last_modified()
        && time.is_valid()
    {
        let chrono_date = chrono::NaiveDate::from_ymd_opt(
            time.year() as i32,
            time.month() as u32,
            time.day() as u32,
        )?;
        let chrono_time = chrono::NaiveTime::from_hms_opt(
            time.hour() as u32,
            time.minute() as u32,
            time.second() as u32,
        )?;

        return Some(
            std::time::UNIX_EPOCH
                + std::time::Duration::from_secs(
                    chrono_date.and_time(chrono_time).and_utc().timestamp() as u64,
                ),
        );
    }

    None
}

pub struct Archive {
    pub compression: CompressionType,
    pub archive: ArchiveType,

    pub server: crate::server::Server,
    pub header: [u8; 16],

    pub file: File,
    pub path: PathBuf,
}

impl Archive {
    pub async fn open(server: crate::server::Server, path: PathBuf) -> Option<Self> {
        let mut file = server.filesystem.open(&path).await.ok()?;

        let mut header = [0; 16];
        #[allow(clippy::unused_io_amount)]
        file.read(&mut header).await.ok()?;

        let inferred = infer::get(&header);
        let compression_format = match inferred.map(|f| f.mime_type()) {
            Some("application/gzip") => CompressionType::Gz,
            Some("application/x-bzip2") => CompressionType::Bz2,
            Some("application/x-xz") => CompressionType::Xz,
            Some("application/x-lz4") => CompressionType::Lz4,
            Some("application/zstd") => CompressionType::Zstd,
            _ => CompressionType::None,
        };

        let archive_format = match path.extension() {
            Some(ext) if ext == "tar" => ArchiveType::Tar,
            Some(ext) if ext == "zip" => ArchiveType::Zip,
            Some(ext) if ext == "rar" => ArchiveType::Rar,
            Some(ext) if ext == "7z" => ArchiveType::SevenZip,
            Some(ext) if ext == "ddup" => ArchiveType::Ddup,
            _ => path.file_stem().map_or(ArchiveType::None, |stem| {
                if stem.to_str().is_some_and(|s| s.ends_with(".tar")) {
                    ArchiveType::Tar
                } else {
                    ArchiveType::None
                }
            }),
        };

        Some(Self {
            compression: compression_format,
            archive: archive_format,
            server,
            header,
            file,
            path,
        })
    }

    pub async fn estimated_size(&mut self) -> Option<u64> {
        match self.compression {
            CompressionType::None => Some(self.file.metadata().await.ok()?.len()),
            CompressionType::Gz => {
                let file_size = self.file.metadata().await.ok()?.len();

                if file_size < 4 {
                    return None;
                }

                if self.file.seek(SeekFrom::End(-4)).await.is_err() {
                    return None;
                }

                let mut buffer = [0; 4];
                if self.file.read_exact(&mut buffer).await.is_err() {
                    return None;
                }

                Some(u32::from_le_bytes(buffer) as u64)
            }
            CompressionType::Xz => None,
            CompressionType::Bz2 => None,
            CompressionType::Lz4 => {
                if self.header[0..4] != [0x04, 0x22, 0x4D, 0x18] {
                    return None;
                }

                let flags = self.header[4];
                let has_content_size = (flags & 0x08) != 0;

                if !has_content_size || self.header.len() < 13 {
                    return None;
                }

                Some(u64::from_le_bytes(self.header[5..13].try_into().ok()?))
            }
            CompressionType::Zstd => {
                if self.header[0..4] != [0x28, 0xB5, 0x2F, 0xFD] {
                    return None;
                }

                let frame_header_descriptor = self.header[4];

                let fcs_flag = frame_header_descriptor & 0x03;
                let single_segment = (frame_header_descriptor & 0x20) != 0;

                if fcs_flag == 0 && !single_segment {
                    return None;
                }

                let size_bytes = match fcs_flag {
                    0 => {
                        if single_segment {
                            1
                        } else {
                            return None;
                        }
                    }
                    1 => 2,
                    2 => 4,
                    3 => 8,
                    _ => return None,
                };

                let size_buffer = &self.header[5..13];

                match size_bytes {
                    1 => Some(size_buffer[0] as u64),
                    2 => Some(u16::from_le_bytes([size_buffer[0], size_buffer[1]]) as u64),
                    4 => Some(u32::from_le_bytes([
                        size_buffer[0],
                        size_buffer[1],
                        size_buffer[2],
                        size_buffer[3],
                    ]) as u64),
                    8 => Some(u64::from_le_bytes(size_buffer.try_into().ok()?)),
                    _ => None,
                }
            }
        }
    }

    pub async fn reader(mut self) -> Result<Box<dyn AsyncRead + Send + Unpin>, anyhow::Error> {
        self.file.seek(SeekFrom::Start(0)).await?;
        let file = BufReader::new(self.file);

        Ok(match self.compression {
            CompressionType::None => Box::new(file),
            CompressionType::Gz => {
                Box::new(async_compression::tokio::bufread::GzipDecoder::new(file))
            }
            CompressionType::Xz => {
                Box::new(async_compression::tokio::bufread::XzDecoder::new(file))
            }
            CompressionType::Bz2 => {
                Box::new(async_compression::tokio::bufread::BzDecoder::new(file))
            }
            CompressionType::Lz4 => {
                Box::new(async_compression::tokio::bufread::Lz4Decoder::new(file))
            }
            CompressionType::Zstd => {
                Box::new(async_compression::tokio::bufread::ZstdDecoder::new(file))
            }
        })
    }

    pub async fn extract(self, destination: PathBuf) -> Result<(), anyhow::Error> {
        match self.archive {
            ArchiveType::None => {
                let file_name = match self.path.file_stem() {
                    Some(stem) => destination.join(stem),
                    None => return Err(anyhow::anyhow!("Invalid file name")),
                };

                let metadata = self.file.metadata().await?;

                let mut writer = super::writer::AsyncFileSystemWriter::new(
                    self.server.clone(),
                    file_name,
                    Some(metadata.permissions()),
                    metadata.modified().ok(),
                )
                .await?;

                tokio::io::copy(&mut self.reader().await?, &mut writer).await?;
                writer.flush().await?;
            }
            ArchiveType::Tar => {
                let server = self.server.clone();

                let mut archive = tokio_tar::Archive::new(self.reader().await?);
                let mut entries = archive.entries()?;

                while let Some(Ok(mut entry)) = entries.next().await {
                    let path = entry.path()?;

                    if path.is_absolute() {
                        continue;
                    }

                    let destination_path = destination.join(path.as_ref());
                    let header = entry.header();

                    if server
                        .filesystem
                        .is_ignored(&destination_path, header.entry_type().is_dir())
                        .await
                    {
                        continue;
                    }

                    match header.entry_type() {
                        tokio_tar::EntryType::Directory => {
                            server.filesystem.create_dir_all(&destination_path).await?;
                            if let Ok(permissions) = header.mode().map(Permissions::from_mode) {
                                server
                                    .filesystem
                                    .set_permissions(
                                        &destination_path,
                                        cap_std::fs::Permissions::from_std(permissions),
                                    )
                                    .await?;
                            }
                        }
                        tokio_tar::EntryType::Regular => {
                            if let Some(parent) = destination_path.parent() {
                                server.filesystem.create_dir_all(parent).await?;
                            }

                            let mut writer = super::writer::AsyncFileSystemWriter::new(
                                server.clone(),
                                destination_path,
                                header.mode().map(Permissions::from_mode).ok(),
                                header
                                    .mtime()
                                    .map(|t| {
                                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(t)
                                    })
                                    .ok(),
                            )
                            .await?;

                            tokio::io::copy(&mut entry, &mut writer).await?;
                            writer.flush().await?;
                        }
                        tokio_tar::EntryType::Symlink => {
                            let link = entry.link_name().unwrap_or_default().unwrap_or_default();

                            server
                                .filesystem
                                .symlink(link.as_ref(), destination_path)
                                .await
                                .unwrap_or_else(|err| {
                                    tracing::debug!(
                                        "failed to create symlink from archive: {:#?}",
                                        err
                                    );
                                });
                        }
                        _ => {}
                    }
                }
            }
            ArchiveType::Zip => {
                let filesystem = self.server.filesystem.base_dir().await?;

                tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
                    let file = Arc::new(self.file.try_into_std().unwrap());
                    let archive = zip::ZipArchive::new(multi_reader::MultiReader::new(file)?)?;
                    let entry_index = Arc::new(AtomicUsize::new(0));

                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(self.server.config.api.file_decompression_threads)
                        .build()
                        .unwrap();

                    let error = Arc::new(RwLock::new(None));

                    pool.in_place_scope(|scope| {
                        let error_clone = Arc::clone(&error);

                        scope.spawn_broadcast(move |_, _| {
                            let mut archive = archive.clone();
                            let entry_index = Arc::clone(&entry_index);
                            let filesystem = Arc::clone(&filesystem);
                            let error_clone2 = Arc::clone(&error_clone);
                            let destination = destination.clone();
                            let server = self.server.clone();

                            let mut run = move || -> Result<(), anyhow::Error> {
                                loop {
                                    if error_clone2.read().unwrap().is_some() {
                                        return Ok(());
                                    }

                                    let i = entry_index
                                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    if i >= archive.len() {
                                        return Ok(());
                                    }

                                    let mut entry = archive.by_index(i)?;
                                    let path = match entry.enclosed_name() {
                                        Some(path) => path,
                                        None => continue,
                                    };

                                    if path.is_absolute() {
                                        continue;
                                    }

                                    let destination_path = destination.join(path);

                                    if server
                                        .filesystem
                                        .is_ignored_sync(&destination_path, entry.is_dir())
                                    {
                                        continue;
                                    }

                                    if entry.is_dir() {
                                        filesystem.create_dir_all(&destination_path)?;
                                        filesystem.set_permissions(
                                            &destination_path,
                                            cap_std::fs::Permissions::from_std(
                                                Permissions::from_mode(
                                                    entry.unix_mode().unwrap_or(0o755),
                                                ),
                                            ),
                                        )?;
                                    } else if entry.is_file() {
                                        if let Some(parent) = destination_path.parent() {
                                            filesystem.create_dir_all(parent)?;
                                        }

                                        let mut writer = super::writer::FileSystemWriter::new(
                                            server.clone(),
                                            destination_path,
                                            entry.unix_mode().map(Permissions::from_mode),
                                            zip_entry_get_modified_time(&entry),
                                        )?;

                                        std::io::copy(&mut entry, &mut writer)?;
                                        writer.flush()?;
                                    } else if entry.is_symlink()
                                        && (1..=2048).contains(&entry.size())
                                    {
                                        let link =
                                            std::io::read_to_string(entry).unwrap_or_default();
                                        filesystem.symlink(link, destination_path).unwrap_or_else(
                                            |err| {
                                                tracing::debug!(
                                                    "failed to create symlink from archive: {:#?}",
                                                    err
                                                );
                                            },
                                        );
                                    }
                                }
                            };

                            if let Err(err) = run() {
                                error_clone.write().unwrap().replace(err);
                            }
                        });
                    });

                    if let Some(err) = error.write().unwrap().take() {
                        Err(err)
                    } else {
                        Ok(())
                    }
                })
                .await??;
            }
            ArchiveType::Rar => {
                let filesystem = self.server.filesystem.base_dir().await?;

                tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
                    drop(self.file);
                    let mut archive =
                        unrar::Archive::new_owned(self.server.filesystem.base_path.join(self.path))
                            .open_for_processing()?;

                    loop {
                        let entry = match archive.read_header()? {
                            Some(entry) => entry,
                            None => break,
                        };

                        let path = &entry.entry().filename;
                        if path.is_absolute() {
                            archive = entry.skip()?;
                            continue;
                        }

                        let destination_path = destination.join(path);

                        if entry.entry().is_directory() {
                            filesystem.create_dir_all(&destination_path)?;

                            archive = entry.skip()?;
                            continue;
                        } else {
                            if let Some(parent) = destination_path.parent() {
                                filesystem.create_dir_all(parent)?;
                            }

                            let writer = super::writer::FileSystemWriter::new(
                                self.server.clone(),
                                destination_path,
                                None,
                                None,
                            )?;

                            let (unrar::Stream(writer, err), processed_archive) =
                                entry.read_to_stream(Box::new(writer))?;
                            if let Some(mut writer) = writer {
                                writer.flush()?;
                            }

                            if let Some(err) = err {
                                return Err(err.into());
                            }

                            archive = processed_archive;
                        }
                    }

                    Ok(())
                })
                .await??;
            }
            ArchiveType::SevenZip => {
                let filesystem = self.server.filesystem.base_dir().await?;

                tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
                    let file = multi_reader::MultiReader::new(Arc::new(
                        self.file.try_into_std().unwrap(),
                    ))?;
                    let archive = sevenz_rust2::Archive::read(&mut file.clone(), &[])?;

                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(self.server.config.api.file_decompression_threads)
                        .build()
                        .unwrap();

                    let error = Arc::new(RwLock::new(None));

                    pool.in_place_scope(|scope| {
                        for folder_index in 0..archive.folders.len() {
                            let archive = archive.clone();
                            let mut file = file.clone();
                            let filesystem = Arc::clone(&filesystem);
                            let destination = destination.clone();
                            let server = self.server.clone();
                            let error_clone = Arc::clone(&error);

                            scope.spawn(move |_| {
                                if error_clone.read().unwrap().is_some() {
                                    return;
                                }

                                let folder = sevenz_rust2::BlockDecoder::new(
                                    folder_index,
                                    &archive,
                                    &[],
                                    &mut file,
                                );

                                let result = folder.for_each_entries(&mut |entry, reader| {
                                    let path = entry.name();
                                    if path.starts_with('/') || path.starts_with('\\') {
                                        return Ok(true);
                                    }

                                    let destination_path = destination.join(path);

                                    if server
                                        .filesystem
                                        .is_ignored_sync(&destination_path, entry.is_directory())
                                    {
                                        return Ok(true);
                                    }

                                    if entry.is_directory() {
                                        filesystem.create_dir_all(&destination_path)?;
                                    } else {
                                        if let Some(parent) = destination_path.parent() {
                                            filesystem.create_dir_all(parent)?;
                                        }

                                        let mut writer = super::writer::FileSystemWriter::new(
                                            server.clone(),
                                            destination_path,
                                            None,
                                            if entry.has_last_modified_date {
                                                Some(entry.last_modified_date.into())
                                            } else {
                                                None
                                            },
                                        )
                                        .map_err(|e| std::io::Error::other(e.to_string()))?;

                                        std::io::copy(reader, &mut writer)?;
                                        writer.flush()?;
                                    }

                                    Ok(true)
                                });

                                if let Err(err) = result {
                                    error_clone.write().unwrap().replace(err);
                                }
                            });
                        }
                    });

                    if let Some(err) = error.write().unwrap().take() {
                        Err(err.into())
                    } else {
                        Ok(())
                    }
                })
                .await??;
            }
            ArchiveType::Ddup => {
                let filesystem = self.server.filesystem.base_dir().await?;

                tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
                    let file = self.file.try_into_std().unwrap();
                    let archive = ddup_bak::archive::Archive::open_file(file)?;

                    let pool = rayon::ThreadPoolBuilder::new()
                        .num_threads(self.server.config.api.file_decompression_threads)
                        .build()
                        .unwrap();

                    fn recursive_traverse(
                        scope: &rayon::Scope,
                        filesystem: &std::sync::Arc<cap_std::fs::Dir>,
                        server: &crate::server::Server,
                        destination: &Path,
                        entry: ddup_bak::archive::entries::Entry,
                    ) -> Result<(), anyhow::Error> {
                        let destination_path = destination.join(entry.name());
                        if server
                            .filesystem
                            .is_ignored_sync(&destination_path, entry.is_directory())
                        {
                            return Ok(());
                        }

                        match entry {
                            ddup_bak::archive::entries::Entry::Directory(dir) => {
                                filesystem.create_dir_all(&destination_path)?;
                                filesystem.set_permissions(
                                    &destination_path,
                                    cap_std::fs::Permissions::from_std(dir.mode.into()),
                                )?;

                                for entry in dir.entries {
                                    recursive_traverse(
                                        scope,
                                        filesystem,
                                        server,
                                        &destination_path,
                                        entry,
                                    )?;
                                }
                            }
                            ddup_bak::archive::entries::Entry::File(mut file) => {
                                scope.spawn({
                                    let server = server.clone();

                                    move |_| {
                                        let mut writer = super::writer::FileSystemWriter::new(
                                            server,
                                            destination_path,
                                            Some(file.mode.into()),
                                            Some(file.mtime),
                                        )
                                        .unwrap();

                                        std::io::copy(&mut file, &mut writer).unwrap();
                                        writer.flush().unwrap();
                                    }
                                });
                            }
                            ddup_bak::archive::entries::Entry::Symlink(link) => {
                                filesystem
                                    .symlink(link.target, destination_path)
                                    .unwrap_or_else(|err| {
                                        tracing::debug!(
                                            "failed to create symlink from archive: {:#?}",
                                            err
                                        );
                                    });
                            }
                        }

                        Ok(())
                    }

                    pool.in_place_scope(|scope| {
                        for entry in archive.into_entries() {
                            recursive_traverse(
                                scope,
                                &filesystem,
                                &self.server,
                                &destination,
                                entry,
                            )?;
                        }

                        Ok(())
                    })
                })
                .await??;
            }
        }

        Ok(())
    }

    pub async fn create_tar(
        server: crate::server::Server,
        destination: impl AsyncWrite + Unpin + Send + 'static,
        base: &Path,
        sources: Vec<PathBuf>,
        compression_type: CompressionType,
        compression_level: CompressionLevel,
    ) -> Result<(), anyhow::Error> {
        let writer: Box<dyn AsyncWrite + Send + Unpin> = match compression_type {
            CompressionType::None => Box::new(destination),
            CompressionType::Gz => {
                Box::new(async_compression::tokio::write::GzipEncoder::with_quality(
                    destination,
                    async_compression::Level::Precise(
                        compression_level.flate2_compression_level().level() as i32,
                    ),
                ))
            }
            CompressionType::Bz2 => {
                Box::new(async_compression::tokio::write::BzEncoder::new(destination))
            }
            CompressionType::Xz => {
                Box::new(async_compression::tokio::write::XzEncoder::new(destination))
            }
            CompressionType::Lz4 => Box::new(async_compression::tokio::write::Lz4Encoder::new(
                destination,
            )),
            CompressionType::Zstd => {
                Box::new(async_compression::tokio::write::ZstdEncoder::with_quality(
                    destination,
                    async_compression::Level::Precise(compression_level.zstd_compression_level()),
                ))
            }
        };
        let mut archive = tokio_tar::Builder::new(writer);

        for source in sources {
            let source = base.join(source);

            let relative = match source.strip_prefix(base) {
                Ok(path) => path,
                Err(_) => continue,
            };

            let source_metadata = match server.filesystem.symlink_metadata(&source).await {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if server
                .filesystem
                .is_ignored(&source, source_metadata.is_dir())
                .await
            {
                continue;
            }

            let mut header = tokio_tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(source_metadata.permissions().mode());
            header.set_mtime(
                source_metadata
                    .modified()
                    .map(|t| {
                        t.into_std()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                    })
                    .unwrap_or_default()
                    .as_secs() as u64,
            );

            if source_metadata.is_dir() {
                header.set_entry_type(tokio_tar::EntryType::Directory);

                archive
                    .append_data(&mut header, relative, tokio::io::empty())
                    .await?;

                let mut walker =
                    crate::server::filesystem::walker::AsyncWalkDir::new(server.clone(), source)
                        .await?;
                while let Some(Ok((is_dir, path))) = walker.next_entry().await {
                    let relative = match path.strip_prefix(base) {
                        Ok(path) => path,
                        Err(_) => continue,
                    };

                    if server.filesystem.is_ignored(&path, is_dir).await {
                        continue;
                    }

                    let metadata = match server.filesystem.symlink_metadata(&path).await {
                        Ok(metadata) => metadata,
                        Err(_) => continue,
                    };

                    let mut header = tokio_tar::Header::new_gnu();
                    header.set_size(0);
                    header.set_mode(metadata.permissions().mode());
                    header.set_mtime(
                        metadata
                            .modified()
                            .map(|t| {
                                t.into_std()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default()
                            .as_secs() as u64,
                    );

                    if metadata.is_dir() {
                        header.set_entry_type(tokio_tar::EntryType::Directory);

                        archive
                            .append_data(&mut header, relative, tokio::io::empty())
                            .await?;
                    } else if metadata.is_file() {
                        let file = server.filesystem.open(&path).await?;

                        header.set_size(metadata.len());
                        header.set_entry_type(tokio_tar::EntryType::Regular);

                        archive.append_data(&mut header, relative, file).await?;
                    } else if let Ok(link_target) =
                        server.filesystem.read_link_contents(&path).await
                    {
                        header.set_entry_type(tokio_tar::EntryType::Symlink);

                        if header.set_link_name(link_target).is_ok() {
                            archive
                                .append_data(&mut header, relative, tokio::io::empty())
                                .await?;
                        }
                    }
                }
            } else if source_metadata.is_file() {
                header.set_size(source_metadata.len());
                header.set_entry_type(tokio_tar::EntryType::Regular);

                archive
                    .append_data(
                        &mut header,
                        relative,
                        server.filesystem.open(&source).await?,
                    )
                    .await?;
            } else if let Ok(link_target) = server.filesystem.read_link_contents(&source).await {
                header.set_entry_type(tokio_tar::EntryType::Symlink);

                if header.set_link_name(link_target).is_ok() {
                    archive
                        .append_data(&mut header, relative, tokio::io::empty())
                        .await?;
                }
            }
        }

        let mut inner = archive.into_inner().await?;
        inner.shutdown().await?;

        Ok(())
    }

    pub async fn create_zip(
        server: crate::server::Server,
        destination: impl Write + Seek + Send + 'static,
        base: PathBuf,
        sources: Vec<PathBuf>,
    ) -> Result<(), anyhow::Error> {
        let abort = Arc::new(AtomicBool::new(false));

        struct AbortGuard(Arc<AtomicBool>);
        impl Drop for AbortGuard {
            #[inline]
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }

        let guard = AbortGuard(Arc::clone(&abort));
        let filesystem = server.filesystem.base_dir().await?;

        tokio::task::spawn_blocking(move || -> Result<(), anyhow::Error> {
            let is_aborted = || abort.load(Ordering::Relaxed);
            let mut archive = zip::ZipWriter::new(destination);

            for source in sources {
                let source = base.join(&source);
                let source = server.filesystem.relative_path(&source);

                let relative = match source.strip_prefix(&base) {
                    Ok(path) => path,
                    Err(_) => continue,
                };

                let source_metadata = match filesystem.symlink_metadata(&source) {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if server
                    .filesystem
                    .is_ignored_sync(&source, source_metadata.is_dir())
                {
                    continue;
                }

                if is_aborted() {
                    return Err(anyhow::anyhow!("operation aborted"));
                }

                let mut options: zip::write::FileOptions<'_, ()> =
                    zip::write::FileOptions::default()
                        .compression_level(Some(
                            server
                                .config
                                .system
                                .backups
                                .compression_level
                                .flate2_compression_level()
                                .level() as i64,
                        ))
                        .unix_permissions(source_metadata.permissions().mode())
                        .large_file(source_metadata.len() >= u32::MAX as u64);

                if let Ok(mtime) = source_metadata.modified() {
                    let mtime: chrono::DateTime<chrono::Local> =
                        chrono::DateTime::from(mtime.into_std());

                    options = options.last_modified_time(zip::DateTime::from_date_and_time(
                        mtime.year() as u16,
                        mtime.month() as u8,
                        mtime.day() as u8,
                        mtime.hour() as u8,
                        mtime.minute() as u8,
                        mtime.second() as u8,
                    )?);
                }

                if source_metadata.is_dir() {
                    archive.add_directory(relative.to_string_lossy(), options)?;

                    let mut walker =
                        crate::server::filesystem::walker::WalkDir::new(server.clone(), source)?;
                    while let Some(Ok((is_dir, path))) = walker.next_entry() {
                        let relative = match path.strip_prefix(&base) {
                            Ok(path) => path,
                            Err(_) => continue,
                        };

                        if server.filesystem.is_ignored_sync(&path, is_dir) {
                            continue;
                        }

                        let metadata = match filesystem.symlink_metadata(&path) {
                            Ok(metadata) => metadata,
                            Err(_) => continue,
                        };

                        if is_aborted() {
                            return Err(anyhow::anyhow!("operation aborted"));
                        }

                        let mut options: zip::write::FileOptions<'_, ()> =
                            zip::write::FileOptions::default()
                                .compression_level(Some(
                                    server
                                        .config
                                        .system
                                        .backups
                                        .compression_level
                                        .flate2_compression_level()
                                        .level() as i64,
                                ))
                                .unix_permissions(metadata.permissions().mode())
                                .large_file(metadata.len() >= u32::MAX as u64);

                        if let Ok(mtime) = metadata.modified() {
                            let mtime: chrono::DateTime<chrono::Local> =
                                chrono::DateTime::from(mtime.into_std());

                            options =
                                options.last_modified_time(zip::DateTime::from_date_and_time(
                                    mtime.year() as u16,
                                    mtime.month() as u8,
                                    mtime.day() as u8,
                                    mtime.hour() as u8,
                                    mtime.minute() as u8,
                                    mtime.second() as u8,
                                )?);
                        }

                        if metadata.is_dir() {
                            archive.add_directory(relative.to_string_lossy(), options)?;
                        } else if metadata.is_file() {
                            let mut file = match filesystem.open(&path) {
                                Ok(file) => file,
                                Err(_) => continue,
                            };

                            archive.start_file(relative.to_string_lossy(), options)?;
                            std::io::copy(&mut file, &mut archive)?;
                        } else if let Ok(link_target) = filesystem.read_link_contents(&path) {
                            archive.add_symlink(
                                relative.to_string_lossy(),
                                link_target.to_string_lossy(),
                                options,
                            )?;
                        }
                    }
                } else if source_metadata.is_file() {
                    let mut file = match filesystem.open(&source) {
                        Ok(file) => file,
                        Err(_) => continue,
                    };

                    archive.start_file(relative.to_string_lossy(), options)?;
                    std::io::copy(&mut file, &mut archive)?;
                } else if let Ok(link_target) = filesystem.read_link_contents(&source) {
                    archive.add_symlink(
                        relative.to_string_lossy(),
                        link_target.to_string_lossy(),
                        options,
                    )?;
                }
            }

            let mut inner = archive.finish()?;
            inner.flush()?;

            Ok(())
        })
        .await??;

        drop(guard);

        Ok(())
    }
}
