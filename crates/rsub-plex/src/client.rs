//! Thin Plex HTTP client. The token always travels in the `X-Plex-Token`
//! header, never in the URL, so it cannot leak into logs.

use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::{Method, Response, StatusCode};
use rsub_core::backend::BackendError;
use serde::de::DeserializeOwned;

use crate::model::Envelope;

const PRODUCT: &str = "rs-subsonic";
const JSON_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct PlexClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl PlexClient {
    /// `base` is the server URL (e.g. `http://192.168.1.10:32400`); `client_id`
    /// identifies this bridge to Plex and should be stable across restarts.
    pub fn new(base: &str, token: &str, client_id: &str) -> Result<Self, BackendError> {
        // rustls needs a process-wide provider; ignore "already installed".
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut headers = HeaderMap::new();
        let hv = |v: &str| {
            HeaderValue::from_str(v)
                .map_err(|_| BackendError::Protocol(format!("invalid header value: {v}")))
        };
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert("X-Plex-Client-Identifier", hv(client_id)?);
        headers.insert("X-Plex-Product", HeaderValue::from_static(PRODUCT));
        headers.insert(
            "X-Plex-Version",
            HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
        );
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .build()
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;
        Ok(PlexClient {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Send a request. `token` overrides the configured (admin) token.
    pub async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        extra: HeaderMap,
        timeout: Option<Duration>,
    ) -> Result<Response, BackendError> {
        let token = HeaderValue::from_str(token.unwrap_or(&self.token))
            .map_err(|_| BackendError::Unauthorized)?;
        let mut req = self
            .http
            .request(method, self.url(path))
            .header("X-Plex-Token", token)
            .headers(extra);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req.send().await.map_err(|e| {
            // Never include the URL: some Plex URLs carry ids users may consider private,
            // and the error chain would repeat it on every retry.
            BackendError::Unavailable(format!("request to Plex failed: {}", e.without_url()))
        })?;
        match resp.status() {
            s if s.is_success() || s == StatusCode::PARTIAL_CONTENT => Ok(resp),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(BackendError::Unauthorized),
            StatusCode::NOT_FOUND => Err(BackendError::NotFound),
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(resp),
            s => Err(BackendError::Unavailable(format!("Plex returned HTTP {s}"))),
        }
    }

    /// GET a `MediaContainer` document, optionally paged.
    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        page: Option<(u64, u64)>,
    ) -> Result<T, BackendError> {
        self.get_json_as(path, page, None).await
    }

    /// [`PlexClient::get_json`] as `token` (or the admin token).
    pub async fn get_json_as<T: DeserializeOwned>(
        &self,
        path: &str,
        page: Option<(u64, u64)>,
        token: Option<&str>,
    ) -> Result<T, BackendError> {
        let mut extra = HeaderMap::new();
        if let Some((start, size)) = page {
            extra.insert("X-Plex-Container-Start", HeaderValue::from(start));
            extra.insert("X-Plex-Container-Size", HeaderValue::from(size));
        }
        let resp = self
            .send(Method::GET, path, token, extra, Some(JSON_TIMEOUT))
            .await?;
        parse(path, resp).await
    }

    /// Send a request as `token` (or the admin token) and parse the
    /// `MediaContainer` it returns.
    pub async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
    ) -> Result<T, BackendError> {
        let resp = self
            .send(method, path, token, HeaderMap::new(), Some(JSON_TIMEOUT))
            .await?;
        parse(path, resp).await
    }

    /// Send a request whose response body doesn't matter.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
    ) -> Result<(), BackendError> {
        let resp = self
            .send(method, path, token, HeaderMap::new(), Some(JSON_TIMEOUT))
            .await?;
        // Read the body so the connection can be reused.
        let _ = resp.bytes().await;
        Ok(())
    }
}

async fn parse<T: DeserializeOwned>(path: &str, resp: Response) -> Result<T, BackendError> {
    let body = resp.bytes().await.map_err(|e| {
        BackendError::Unavailable(format!("reading Plex response: {}", e.without_url()))
    })?;
    serde_json::from_slice::<Envelope<T>>(&body)
        .map(|e| e.container)
        .map_err(|e| {
            BackendError::Protocol(format!("unexpected Plex JSON for {}: {e}", path_only(path)))
        })
}

/// Path without the query string, for error messages.
fn path_only(p: &str) -> &str {
    p.split('?').next().unwrap_or(p)
}
