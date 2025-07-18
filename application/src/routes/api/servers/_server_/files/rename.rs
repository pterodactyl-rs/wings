use super::State;
use utoipa_axum::{router::OpenApiRouter, routes};

mod put {
    use crate::routes::{ApiError, api::servers::_server_::GetServer};
    use axum::http::StatusCode;
    use serde::{Deserialize, Serialize};
    use std::path::Path;
    use utoipa::ToSchema;

    #[derive(ToSchema, Deserialize)]
    pub struct RenameFile {
        pub to: String,
        pub from: String,
    }

    #[derive(ToSchema, Deserialize)]
    pub struct Payload {
        #[serde(default)]
        pub root: String,

        #[schema(inline)]
        pub files: Vec<RenameFile>,
    }

    #[derive(ToSchema, Serialize)]
    struct Response {
        renamed: usize,
    }

    #[utoipa::path(put, path = "/", responses(
        (status = OK, body = inline(Response)),
        (status = NOT_FOUND, body = ApiError),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ), request_body = inline(Payload))]
    pub async fn route(
        server: GetServer,
        axum::Json(data): axum::Json<Payload>,
    ) -> (StatusCode, axum::Json<serde_json::Value>) {
        let root = Path::new(&data.root);

        let metadata = server.filesystem.metadata(&root).await;
        if !metadata.map(|m| m.is_dir()).unwrap_or(true) {
            return (
                StatusCode::EXPECTATION_FAILED,
                axum::Json(ApiError::new("root is not a directory").to_json()),
            );
        }

        let mut renamed_count = 0;
        for file in data.files {
            let from = root.join(file.from);
            if from == root {
                continue;
            }

            let to = root.join(file.to);
            if to == root {
                continue;
            }

            if from == to {
                continue;
            }

            let from_metadata = match server.filesystem.metadata(&from).await {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };

            if server.filesystem.metadata(&to).await.is_ok()
                || server
                    .filesystem
                    .is_ignored(&from, from_metadata.is_dir())
                    .await
                || server
                    .filesystem
                    .is_ignored(&to, from_metadata.is_dir())
                    .await
            {
                continue;
            }

            if let Err(err) = server.filesystem.rename_path(from, to).await {
                tracing::debug!(
                    server = %server.uuid,
                    "failed to rename file: {:#?}",
                    err
                );
            } else {
                renamed_count += 1;
            }
        }

        (
            StatusCode::OK,
            axum::Json(
                serde_json::to_value(Response {
                    renamed: renamed_count,
                })
                .unwrap(),
            ),
        )
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .routes(routes!(put::route))
        .with_state(state.clone())
}
