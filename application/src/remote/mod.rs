use crate::server::{activity::ApiActivity, permissions::Permissions};
use client::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;

pub mod backups;
pub mod client;
pub mod jwt;
pub mod servers;

#[inline]
fn into_json<T: DeserializeOwned>(value: String) -> Result<T, anyhow::Error> {
    match serde_json::from_str(&value) {
        Ok(json) => Ok(json),
        Err(err) => Err(anyhow::anyhow!(
            "failed to parse JSON: {:#?} <- {value}",
            err
        )),
    }
}

#[derive(Deserialize, Serialize, Default)]
pub struct Pagination {
    current_page: usize,
    last_page: usize,
    total: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationType {
    Password,
    PublicKey,
}

pub async fn get_sftp_auth(
    client: &Client,
    r#type: AuthenticationType,
    username: &str,
    password: &str,
) -> Result<(uuid::Uuid, uuid::Uuid, Permissions, Vec<String>), anyhow::Error> {
    let response: Response = into_json(
        client
            .client
            .post(format!("{}/sftp/auth", client.url))
            .json(&json!({
                "type": r#type,
                "username": username,
                "password": password,
            }))
            .send()
            .await?
            .text()
            .await?,
    )?;

    #[derive(Deserialize)]
    pub struct Response {
        user: uuid::Uuid,
        server: uuid::Uuid,

        permissions: Permissions,
        #[serde(default)]
        ignored_files: Vec<String>,
    }

    Ok((
        response.user,
        response.server,
        response.permissions,
        response.ignored_files,
    ))
}

pub async fn send_activity(
    client: &Client,
    activity: Vec<ApiActivity>,
) -> Result<(), anyhow::Error> {
    client
        .client
        .post(format!("{}/activity", client.url))
        .json(&json!({
            "data": activity,
        }))
        .send()
        .await?;

    Ok(())
}

pub async fn reset_state(client: &Client) -> Result<(), anyhow::Error> {
    client
        .client
        .post(format!("{}/servers/reset", client.url))
        .send()
        .await?;

    Ok(())
}
