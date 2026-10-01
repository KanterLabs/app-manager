use crate::models::{BuildRecord, RepositoryMapping};
use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;

pub struct BrokerClient {
    base: url::Url,
    token: String,
    http: reqwest::Client,
}

impl BrokerClient {
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let mut base = url::Url::parse(base).context("Enter a valid broker URL")?;
        let loopback = matches!(
            base.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if base.scheme() != "https" && !(base.scheme() == "http" && loopback) {
            bail!("Broker connections require HTTPS, except local development")
        }
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            bail!("Broker URL must not contain credentials, a query, or fragment")
        }
        if token.trim().is_empty() || token.len() > 4096 || token.chars().any(char::is_control) {
            bail!("A valid account credential is required")
        }
        base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base,
            token: token.into(),
            http,
        })
    }
    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(self.base.join(path)?)
            .bearer_auth(&self.token)
            .send()
            .await
            .context("Cannot reach the integration broker")?;
        decode(response).await
    }
    pub async fn repositories(&self) -> Result<Vec<RepositoryMapping>> {
        self.get("api/repositories").await
    }
    pub async fn builds(&self) -> Result<Vec<BuildRecord>> {
        self.get("api/builds").await
    }
    pub async fn request_build(
        &self,
        repository_id: &str,
        reference: &str,
        commit: &str,
    ) -> Result<BuildRecord> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            repository_id: &'a str,
            reference: &'a str,
            commit: &'a str,
        }
        let response = self
            .http
            .post(self.base.join("api/builds")?)
            .bearer_auth(&self.token)
            .json(&Request {
                repository_id,
                reference,
                commit,
            })
            .send()
            .await
            .context(
                "Build dispatch response is unknown; inspect provider status before retrying",
            )?;
        decode(response).await
    }
}

async fn decode<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        bail!("Broker account credential is invalid or repository access was removed")
    }
    if !status.is_success() {
        bail!("Broker refused the request (HTTP {})", status.as_u16())
    }
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if data.len() + chunk.len() > 4 * 1024 * 1024 {
            bail!("Broker response exceeds the allowed size")
        }
        data.extend_from_slice(&chunk)
    }
    serde_json::from_slice(&data).context("Broker returned an incompatible response")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_credential_urls_and_unprotected_remote_brokers() {
        for url in [
            "http://example.com",
            "https://user:password@example.com",
            "https://example.com/?token=secret",
            "file:///tmp/broker",
        ] {
            assert!(BrokerClient::new(url, "credential").is_err())
        }
        assert!(BrokerClient::new("https://example.com", "credential").is_ok());
        assert!(BrokerClient::new("http://127.0.0.1:9876", "credential").is_ok());
    }
}
