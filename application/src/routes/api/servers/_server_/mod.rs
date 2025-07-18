use super::State;
use crate::routes::{ApiError, GetState};
use axum::{
    body::Body,
    extract::{Path, Request},
    http::{Response, StatusCode},
    middleware::Next,
};
use utoipa_axum::{router::OpenApiRouter, routes};

mod backup;
mod commands;
mod files;
mod logs;
mod power;
mod reinstall;
mod script;
mod sync;
mod transfer;
mod version;
mod ws;

pub type GetServer = axum::extract::Extension<crate::server::Server>;

pub async fn auth(
    state: GetState,
    Path(parts): Path<Vec<String>>,
    mut req: Request,
    next: Next,
) -> Result<Response<Body>, StatusCode> {
    let uuid = match parts.first() {
        Some(uuid) => match uuid.parse::<uuid::Uuid>() {
            Ok(uuid) => uuid,
            Err(_) => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        serde_json::to_string(&ApiError::new("invalid server uuid")).unwrap(),
                    ))
                    .unwrap());
            }
        },
        None => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ApiError::new("missing server uuid")).unwrap(),
                ))
                .unwrap());
        }
    };

    let server = match state
        .server_manager
        .get_servers()
        .await
        .iter()
        .find(|s| s.uuid == uuid)
        .cloned()
    {
        Some(server) => server,
        None => {
            return Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&ApiError::new("server not found")).unwrap(),
                ))
                .unwrap());
        }
    };

    req.extensions_mut().insert(server);

    Ok(next.run(req).await)
}

mod get {
    use crate::routes::api::servers::_server_::GetServer;

    #[utoipa::path(get, path = "/", responses(
        (status = OK, body = crate::models::Server),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ))]
    pub async fn route(server: GetServer) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::to_value(server.to_api_response().await).unwrap())
    }
}

mod delete {
    use crate::routes::{GetState, api::servers::_server_::GetServer};
    use serde::Serialize;
    use utoipa::ToSchema;

    #[derive(ToSchema, Serialize)]
    struct Response {}

    #[utoipa::path(delete, path = "/", responses(
        (status = OK, body = inline(Response)),
    ), params(
        (
            "server" = uuid::Uuid,
            description = "The server uuid",
            example = "123e4567-e89b-12d3-a456-426614174000",
        ),
    ))]
    pub async fn route(state: GetState, server: GetServer) -> axum::Json<serde_json::Value> {
        state.server_manager.delete_server(&server).await;

        axum::Json(serde_json::to_value(Response {}).unwrap())
    }
}

pub fn router(state: &State) -> OpenApiRouter<State> {
    OpenApiRouter::new()
        .nest("/logs", logs::router(state))
        .nest("/transfer", transfer::router(state))
        .nest("/script", script::router(state))
        .nest("/power", power::router(state))
        .nest("/version", version::router(state))
        .nest("/commands", commands::router(state))
        .nest("/sync", sync::router(state))
        .nest("/reinstall", reinstall::router(state))
        .nest("/ws", ws::router(state))
        .nest("/files", files::router(state))
        .nest("/backup", backup::router(state))
        .routes(routes!(get::route))
        .routes(routes!(delete::route))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state.clone())
}
