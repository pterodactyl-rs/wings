use crate::{
    io::{
        counting_reader::{AsyncCountingReader, CountingReader},
        counting_writer::CountingWriter,
        limited_reader::AsyncLimitedReader,
        limited_writer::LimitedWriter,
    },
    remote::backups::RawServerBackup,
};
use futures::{StreamExt, TryStreamExt};
use ignore::WalkBuilder;
use sha1::Digest;
use std::{
    fs::Permissions,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader, ReadBuf},
    sync::RwLock,
};

static CLIENT: RwLock<Option<Arc<reqwest::Client>>> = RwLock::const_new(None);

#[inline]
async fn get_client(server: &crate::server::Server) -> Arc<reqwest::Client> {
    if let Some(client) = CLIENT.read().await.as_ref() {
        return Arc::clone(client);
    }

    let client = Arc::new(
        reqwest::ClientBuilder::new()
            .timeout(std::time::Duration::from_secs(2 * 60 * 60))
            .danger_accept_invalid_certs(server.config.ignore_certificate_errors)
            .build()
            .unwrap(),
    );

    *CLIENT.write().await = Some(Arc::clone(&client));
    client
}

struct BoundedReader {
    file: tokio::fs::File,
    size: u64,
    position: u64,

    bytes_written: Arc<AtomicU64>,
}

impl BoundedReader {
    async fn new_with_bytes_written(
        file: &mut tokio::fs::File,
        offset: u64,
        size: u64,
        bytes_written: Arc<AtomicU64>,
    ) -> Self {
        file.seek(std::io::SeekFrom::Start(offset)).await.unwrap();

        Self {
            file: file.try_clone().await.unwrap(),
            size,
            position: 0,
            bytes_written,
        }
    }
}

impl AsyncRead for BoundedReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        if this.position >= this.size {
            return Poll::Ready(Ok(()));
        }

        let remaining = this.size - this.position;
        let buffer_space = buf.remaining();
        let to_read = std::cmp::min(buffer_space, remaining as usize);

        let mut temp_buf = vec![0u8; to_read];

        let read_future = this.file.read(&mut temp_buf);

        match Pin::new(&mut Box::pin(read_future)).poll(cx) {
            Poll::Ready(Ok(bytes_read)) => {
                this.position += bytes_read as u64;
                this.bytes_written
                    .fetch_add(bytes_read as u64, Ordering::Relaxed);
                buf.put_slice(&temp_buf[..bytes_read]);

                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[inline]
fn get_file_name(server: &crate::server::Server, uuid: uuid::Uuid) -> PathBuf {
    Path::new(&server.config.system.backup_directory).join(format!("{uuid}.s3.tar.gz"))
}

pub async fn create_backup(
    server: crate::server::Server,
    uuid: uuid::Uuid,
    progress: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
    overrides: ignore::overrides::Override,
) -> Result<RawServerBackup, anyhow::Error> {
    let file_name = get_file_name(&server, uuid);
    let writer = tokio::fs::File::create(&file_name).await?.into_std().await;
    let filesystem = server.filesystem.base_dir().await?;

    let compression_level = server.config.system.backups.compression_level;
    tokio::task::spawn_blocking({
        let progress = Arc::clone(&progress);
        let server = server.clone();

        move || -> Result<(), anyhow::Error> {
            let writer = LimitedWriter::new_with_bytes_per_second(
                writer,
                server.config.system.backups.write_limit * 1024 * 1024,
            );
            let writer = CountingWriter::new_with_bytes_written(writer, Arc::clone(&total));
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                writer,
                compression_level.flate2_compression_level(),
            ));

            tar.mode(tar::HeaderMode::Complete);
            tar.follow_symlinks(false);

            for entry in WalkBuilder::new(&server.filesystem.base_path)
                .overrides(overrides)
                .add_custom_ignore_filename(".pteroignore")
                .follow_links(false)
                .git_global(false)
                .hidden(false)
                .build()
                .flatten()
            {
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };

                if let Ok(relative) = entry.path().strip_prefix(&server.filesystem.base_path) {
                    if relative.components().count() == 0 {
                        continue;
                    }

                    let mut header = tar::Header::new_gnu();
                    header.set_size(0);
                    header.set_mode(metadata.permissions().mode());
                    header.set_mtime(
                        metadata
                            .modified()
                            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default())
                            .unwrap_or_default()
                            .as_secs(),
                    );

                    if metadata.is_dir() {
                        header.set_entry_type(tar::EntryType::Directory);

                        progress.fetch_add(metadata.len(), Ordering::SeqCst);
                        tar.append_data(&mut header, relative, std::io::empty())?;
                    } else if metadata.is_file() {
                        let file = match filesystem.open(relative) {
                            Ok(file) => file,
                            Err(_) => continue,
                        };
                        let reader =
                            CountingReader::new_with_bytes_read(file, Arc::clone(&progress));

                        header.set_size(metadata.len());
                        header.set_entry_type(tar::EntryType::Regular);

                        tar.append_data(&mut header, relative, reader)?;
                    } else if let Ok(link_target) = filesystem.read_link_contents(relative) {
                        header.set_entry_type(tar::EntryType::Symlink);

                        progress.fetch_add(metadata.len(), Ordering::SeqCst);
                        tar.append_link(&mut header, relative, link_target)?;
                    }
                }
            }

            tar.finish()?;
            let mut inner = tar.into_inner()?;
            inner.flush()?;

            Ok(())
        }
    })
    .await??;

    let mut sha1 = sha1::Sha1::new();
    let mut file = tokio::fs::File::open(&file_name).await?;

    let mut buffer = [0; 8192];
    loop {
        let bytes_read = file.read(&mut buffer).await?;
        if bytes_read == 0 {
            break;
        }

        sha1.update(&buffer[..bytes_read]);
    }

    let size = file.metadata().await?.len();
    let (part_size, part_urls) = server.config.client.backup_upload_urls(uuid, size).await?;

    let mut remaining_size = size;
    let mut parts = Vec::with_capacity(part_urls.len());
    for (i, url) in part_urls.into_iter().enumerate() {
        let offset = size - remaining_size;
        let part_size = std::cmp::min(remaining_size, part_size);

        let etag;
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > 50 {
                return Err(anyhow::anyhow!("Failed to upload part after 50 attempts"));
            }

            tracing::debug!(
                "uploading s3 backup part {} of size {} for backup {} for {}",
                i + 1,
                part_size,
                uuid,
                server.uuid
            );

            match get_client(&server)
                .await
                .put(&url)
                .header("Content-Length", part_size)
                .header("Content-Type", "application/gzip")
                .body(reqwest::Body::wrap_stream(
                    tokio_util::io::ReaderStream::new(
                        AsyncLimitedReader::new_with_bytes_per_second(
                            BoundedReader::new_with_bytes_written(
                                &mut file,
                                offset,
                                part_size,
                                Arc::clone(&progress),
                            )
                            .await,
                            server.config.system.backups.write_limit * 1024 * 1024,
                        ),
                    ),
                ))
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        etag = response
                            .headers()
                            .get("ETag")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();

                        break;
                    } else {
                        tracing::error!(
                            backup = %uuid,
                            server = %server.uuid,
                            "failed to upload s3 backup part {}: status code {}",
                            i + 1,
                            response.status()
                        );
                    }
                }
                Err(err) => {
                    tracing::error!(
                        backup = %uuid,
                        server = %server.uuid,
                        "failed to upload s3 backup part {}: {:#?}",
                        i + 1,
                        err
                    );

                    tokio::time::sleep(std::time::Duration::from_secs(attempts * 2)).await;
                }
            }
        }

        parts.push(crate::remote::backups::RawServerBackupPart {
            etag,
            part_number: i + 1,
        });
        remaining_size -= part_size;
    }

    if remaining_size > 0 {
        return Err(anyhow::anyhow!("failed to upload all parts"));
    }

    tokio::fs::remove_file(&file_name).await?;

    Ok(RawServerBackup {
        checksum: format!("{:x}", sha1.finalize()),
        checksum_type: "sha1".to_string(),
        size,
        successful: true,
        parts,
    })
}

pub async fn restore_backup(
    server: crate::server::Server,
    download_url: Option<String>,
    progress: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
) -> Result<(), anyhow::Error> {
    let response = get_client(&server)
        .await
        .get(download_url.unwrap())
        .send()
        .await?;
    if let Some(content_length) = response.content_length() {
        total.store(content_length, Ordering::SeqCst);
    }

    let reader = tokio_util::io::StreamReader::new(Box::pin(
        response.bytes_stream().map_err(std::io::Error::other),
    ));
    let reader = AsyncLimitedReader::new_with_bytes_per_second(
        reader,
        server.config.system.backups.read_limit * 1024 * 1024,
    );
    let reader = AsyncCountingReader::new_with_bytes_read(reader, progress);
    let reader = BufReader::with_capacity(1024 * 1024, reader);

    let mut archive =
        tokio_tar::Archive::new(async_compression::tokio::bufread::GzipDecoder::new(reader));

    let mut entries = archive.entries()?;
    while let Some(entry) = entries.next().await {
        let mut entry = entry?;
        let path = entry.path()?;

        if path.is_absolute() {
            continue;
        }

        if server
            .filesystem
            .is_ignored(
                &path,
                entry.header().entry_type() == tokio_tar::EntryType::Directory,
            )
            .await
        {
            continue;
        }

        let header = entry.header();
        match header.entry_type() {
            tokio_tar::EntryType::Directory => {
                server.filesystem.create_dir_all(path.as_ref()).await?;
                server
                    .filesystem
                    .set_permissions(
                        path.as_ref(),
                        cap_std::fs::Permissions::from_std(Permissions::from_mode(
                            header.mode().unwrap_or(0o755),
                        )),
                    )
                    .await?;
            }
            tokio_tar::EntryType::Regular => {
                server
                    .log_daemon(format!("(restoring): {}", path.display()))
                    .await;

                if let Some(parent) = path.parent() {
                    server.filesystem.create_dir_all(parent).await?;
                }

                let mut writer = crate::server::filesystem::writer::AsyncFileSystemWriter::new(
                    server.clone(),
                    path.to_path_buf(),
                    Some(Permissions::from_mode(header.mode().unwrap_or(0o644))),
                    header
                        .mtime()
                        .map(|t| std::time::UNIX_EPOCH + std::time::Duration::from_secs(t))
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
                    .symlink(link, path)
                    .await
                    .unwrap_or_else(|err| {
                        tracing::debug!("failed to create symlink from archive: {:#?}", err);
                    });
            }
            _ => {}
        }
    }

    Ok(())
}

pub async fn delete_backup(
    server: &crate::server::Server,
    uuid: uuid::Uuid,
) -> Result<(), anyhow::Error> {
    let file_name = get_file_name(server, uuid);
    if file_name.exists() {
        tokio::fs::remove_file(&file_name).await?;
    }

    Ok(())
}
