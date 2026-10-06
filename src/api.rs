//! TIDAL REST API client (`api.tidal.com/v1` and `/v2`), authenticated with
//! the account's OAuth access token.
//!
//! - Every request carries `countryCode` (from the session): TIDAL answers
//!   with what is available in that country.
//! - The access token is refreshed shortly before it expires, and once more
//!   when the API refuses it (401 about the token); the new tokens are saved
//!   at once.
//! - Tokens, the user id and signed CDN URLs are never logged.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Method;
use serde_json::Value;

use crate::auth::{Credentials, LoginKind};

/// Client of the device-code sign-in (`link.tidal.com`). The same one as
/// the open-source `tidalapi` library. Since late 2025 TIDAL caps the
/// tokens of this flow at CD quality.
pub const DEVICE_CLIENT_ID: &str = "fX2JxdmntZWK0ixT";
pub const DEVICE_CLIENT_SECRET: &str = "1Nn9AfDAjxrgJFJbKNWLeAyKGVGmINuXPPLHVXAvxAg=";
/// Client of the browser sign-in with PKCE (the Android app's, as
/// `tidalapi` uses it): its tokens carry the hi-res entitlement.
pub const PKCE_CLIENT_ID: &str = "6BDSRdpK9hqEBTgU";
pub const PKCE_CLIENT_SECRET: &str = "xeuPmY7nbpZ9IIbLAcQ93shka1VNheUAqN6IcszjTG8=";
pub const SCOPE: &str = "r_usr w_usr w_sub";
const USER_AGENT: &str = concat!("tidalsrc/", env!("CARGO_PKG_VERSION"));
/// Largest page most v1 list endpoints accept.
const API_PAGE: u32 = 100;
/// Refresh the access token when it has less than this left.
const REFRESH_MARGIN_S: i64 = 120;

#[derive(Clone, Debug, PartialEq)]
pub struct ClientId {
    pub id: String,
    pub secret: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Holds `/v1` and `/v2`.
    pub api: String,
    /// OAuth server: `/v1/oauth2/device_authorization`, `/v1/oauth2/token`.
    pub auth: String,
    /// Browser sign-in: `/authorize`.
    pub login: String,
    pub device: ClientId,
    pub pkce: ClientId,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api: "https://api.tidal.com".into(),
            auth: "https://auth.tidal.com".into(),
            login: "https://login.tidal.com".into(),
            device: ClientId { id: DEVICE_CLIENT_ID.into(), secret: DEVICE_CLIENT_SECRET.into() },
            pkce: ClientId { id: PKCE_CLIENT_ID.into(), secret: PKCE_CLIENT_SECRET.into() },
        }
    }
}

impl Config {
    pub fn client(&self, kind: LoginKind) -> &ClientId {
        match kind {
            LoginKind::Device => &self.device,
            LoginKind::Pkce => &self.pkce,
        }
    }
}

/// Non-2xx answer, kept typed so callers can tell an expired token (401)
/// from a missing item (404) or rate limiting (429).
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: u16,
    /// `Retry-After` in seconds, when sent.
    pub retry_after: Option<u64>,
    /// TIDAL's `subStatus` (`11003`: token expired, `4005`: not playable…).
    pub sub_status: Option<i64>,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.status)?;
        if let Some(s) = self.sub_status {
            write!(f, " ({s})")?;
        }
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpError {}

impl HttpError {
    pub async fn from_response(resp: reqwest::Response) -> Self {
        let status = resp.status().as_u16();
        let retry_after = resp.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()?.trim().parse().ok());
        let body = resp.text().await.unwrap_or_default();
        Self::parse(status, retry_after, &body)
    }

    /// From TIDAL's error body: `{status, subStatus, userMessage}` (API) or
    /// `{error, error_description}` (OAuth).
    pub fn parse(status: u16, retry_after: Option<u64>, body: &str) -> Self {
        let v: Value = serde_json::from_str(body).unwrap_or_default();
        let sub_status = v.get("subStatus").or_else(|| v.get("sub_status")).and_then(Value::as_i64);
        let message = ["userMessage", "error_description", "error", "description"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| if v.is_null() { body.chars().take(200).collect() } else { String::new() });
        HttpError { status, retry_after, sub_status, message: message.chars().take(300).collect() }
    }

    /// A 401 about the token itself (expired, invalid), as opposed to one
    /// about the item (TIDAL answers 401 for some unplayable tracks).
    pub fn is_token_problem(&self) -> bool {
        self.status == 401 && self.sub_status.is_none_or(|s| (11000..12000).contains(&s))
    }
}

/// No account, or TIDAL no longer accepts its refresh token: sign in again.
#[derive(Debug, Clone)]
pub struct SignInNeeded(pub &'static str);

impl std::fmt::Display for SignInNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for SignInNeeded {}

/// A `T` among the error, its contexts and its sources.
pub fn find<T: std::error::Error + Send + Sync + 'static>(e: &anyhow::Error) -> Option<&T> {
    e.downcast_ref::<T>().or_else(|| e.chain().find_map(|c| c.downcast_ref::<T>()))
}

/// HTTP status of a failed API request.
pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    find::<HttpError>(e).map(|h| h.status)
}

pub fn now_unix_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `users/123/favorites` → `users/…/favorites`: paths go to the log
/// without the account's id.
pub fn mask_path(path: &str) -> String {
    match path.split_once("users/") {
        Some((head, rest)) => {
            let tail = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            format!("{head}users/…{tail}")
        }
        None => path.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Base {
    V1,
    V2,
}

/// One API request.
#[derive(Clone, Debug)]
pub struct Req {
    method: Method,
    base: Base,
    path: String,
    query: Vec<(String, String)>,
    form: Option<Vec<(String, String)>>,
    etag: Option<String>,
}

impl Req {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Req { method, base: Base::V1, path: path.into(), query: Vec::new(), form: None, etag: None }
    }

    pub fn get(path: impl Into<String>) -> Self {
        Self::new(Method::GET, path)
    }

    pub fn v2(mut self) -> Self {
        self.base = Base::V2;
        self
    }

    pub fn q(mut self, key: &str, value: impl ToString) -> Self {
        self.query.push((key.to_string(), value.to_string()));
        self
    }

    pub fn form(mut self, fields: &[(&str, String)]) -> Self {
        self.form = Some(fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
        self
    }

    /// `If-None-Match`: TIDAL wants the playlist's current ETag on edits.
    pub fn etag(mut self, etag: Option<String>) -> Self {
        self.etag = etag;
        self
    }
}

struct Inner {
    http: reqwest::Client,
    config: Config,
    session: tokio::sync::Mutex<Option<Credentials>>,
    data_dir: Mutex<Option<PathBuf>>,
    /// `en_US`, for pages and texts.
    locale: Mutex<String>,
}

#[derive(Clone)]
pub struct Api {
    inner: Arc<Inner>,
}

impl Api {
    pub fn new(config: Config) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        Api {
            inner: Arc::new(Inner {
                http,
                config,
                session: tokio::sync::Mutex::new(None),
                data_dir: Mutex::new(None),
                locale: Mutex::new("en_US".into()),
            }),
        }
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn set_data_dir(&self, dir: Option<PathBuf>) {
        *self.inner.data_dir.lock().unwrap_or_else(|e| e.into_inner()) = dir;
    }

    pub fn set_locale(&self, locale: &str) {
        *self.inner.locale.lock().unwrap_or_else(|e| e.into_inner()) = locale.to_string();
    }

    fn locale(&self) -> String {
        self.inner.locale.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub async fn set_credentials(&self, creds: Option<Credentials>) {
        *self.inner.session.lock().await = creds;
    }

    pub async fn credentials(&self) -> Option<Credentials> {
        self.inner.session.lock().await.clone()
    }

    pub async fn signed_in(&self) -> bool {
        self.inner.session.lock().await.is_some()
    }

    /// The session, its access token refreshed first when it is about to
    /// expire.
    pub async fn session(&self) -> Result<Credentials> {
        let mut session = self.inner.session.lock().await;
        let creds = session.as_mut().ok_or(SignInNeeded("not signed in"))?;
        if creds.expires_at <= now_unix_s() + REFRESH_MARGIN_S {
            self.refresh_locked(creds).await?;
        }
        Ok(creds.clone())
    }

    /// Refresh after the API refused `used`, unless another request did it
    /// meanwhile.
    async fn refresh_after(&self, used: &str) -> Result<()> {
        let mut session = self.inner.session.lock().await;
        let creds = session.as_mut().ok_or(SignInNeeded("not signed in"))?;
        if creds.access_token == used {
            self.refresh_locked(creds).await?;
        }
        Ok(())
    }

    async fn refresh_locked(&self, creds: &mut Credentials) -> Result<()> {
        let client = self.config().client(creds.kind).clone();
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", creds.refresh_token.as_str()),
            ("client_id", client.id.as_str()),
            ("client_secret", client.secret.as_str()),
            ("scope", SCOPE),
        ];
        let url = format!("{}/v1/oauth2/token", self.config().auth);
        let resp = self.http().post(url).form(&form).send().await.context("token refresh")?;
        if !resp.status().is_success() {
            let e = HttpError::from_response(resp).await;
            tracing::warn!("token refresh refused: {e}");
            if matches!(e.status, 400 | 401 | 403) {
                return Err(anyhow::Error::from(e).context(SignInNeeded("TIDAL no longer accepts the stored sign-in")));
            }
            return Err(e.into());
        }
        let v: Value = resp.json().await.context("token refresh answer")?;
        let access = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("token refresh: no access_token"))?;
        creds.access_token = access.to_string();
        if let Some(r) = v.get("refresh_token").and_then(Value::as_str).filter(|r| !r.is_empty()) {
            creds.refresh_token = r.to_string();
        }
        creds.expires_at = now_unix_s() + v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
        tracing::info!("access token refreshed");
        let dir = self.inner.data_dir.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(dir) = dir {
            if let Err(e) = creds.save(&dir) {
                tracing::warn!("refreshed tokens not saved: {e:#}");
            }
        }
        Ok(())
    }

    /// Send `req` with the account's token; on a 401 about the token,
    /// refresh it and send once more. Non-2xx answers become [`HttpError`].
    pub async fn send(&self, req: &Req) -> Result<reqwest::Response> {
        let mut retried = false;
        loop {
            let creds = self.session().await?;
            let base = match req.base {
                Base::V1 => "v1",
                Base::V2 => "v2",
            };
            let url = format!("{}/{base}/{}", self.config().api, req.path);
            let mut query: Vec<(String, String)> = vec![("countryCode".into(), creds.country_code.clone())];
            if req.base == Base::V2 || req.path.starts_with("pages/") {
                query.push(("locale".into(), self.locale()));
                query.push(("deviceType".into(), "BROWSER".into()));
            }
            if req.base == Base::V2 {
                query.push(("platform".into(), "WEB".into()));
            }
            query.extend(req.query.iter().cloned());
            let mut b = self.http().request(req.method.clone(), url).bearer_auth(&creds.access_token).query(&query);
            if let Some(form) = &req.form {
                b = b.form(form);
            }
            if let Some(etag) = &req.etag {
                b = b.header(reqwest::header::IF_NONE_MATCH, etag);
            }
            tracing::debug!("{} {base}/{}", req.method, mask_path(&req.path));
            // Without the URL: it may hold the account's id.
            let resp = b.send().await.map_err(|e| e.without_url()).with_context(|| format!("{} {}", req.method, mask_path(&req.path)))?;
            if resp.status().is_success() {
                return Ok(resp);
            }
            let e = HttpError::from_response(resp).await;
            if e.is_token_problem() && !retried {
                tracing::info!("token refused ({e}), refreshing");
                self.refresh_after(&creds.access_token).await?;
                retried = true;
                continue;
            }
            return Err(anyhow::Error::from(e).context(format!("{} {}", req.method, mask_path(&req.path))));
        }
    }

    async fn json(resp: reqwest::Response) -> Result<Value> {
        let body = resp.bytes().await?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&body).context("unreadable JSON answer")
    }

    pub async fn call(&self, req: Req) -> Result<Value> {
        Self::json(self.send(&req).await?).await
    }

    pub async fn get(&self, path: impl Into<String>) -> Result<Value> {
        self.call(Req::get(path)).await
    }

    /// The answer and its `ETag`.
    pub async fn get_etag(&self, path: impl Into<String>) -> Result<(Value, Option<String>)> {
        let resp = self.send(&Req::get(path)).await?;
        let etag = resp.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok((Self::json(resp).await?, etag))
    }

    /// `{items, totalNumberOfItems}` of a v1 list, read in pages the API
    /// accepts until `limit` items (or the end).
    pub async fn page(&self, req: Req, offset: u32, limit: u32) -> Result<Value> {
        let mut items: Vec<Value> = Vec::new();
        let mut total = None;
        let mut at = offset;
        while (items.len() as u32) < limit {
            let want = (limit - items.len() as u32).min(API_PAGE);
            let v = self.call(req.clone().q("offset", at).q("limit", want)).await?;
            let got: Vec<Value> = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            total = v.get("totalNumberOfItems").and_then(Value::as_u64).or(total);
            let n = got.len() as u32;
            items.extend(got);
            items.truncate(limit as usize);
            at += n;
            if n < want || total.is_some_and(|t| u64::from(at) >= t) {
                break;
            }
        }
        let mut out = serde_json::json!({"items": items, "offset": offset});
        if let Some(t) = total {
            out["totalNumberOfItems"] = t.into();
        }
        Ok(out)
    }

    /// Bytes from a CDN URL handed out by the API (it carries its own
    /// authorisation): the whole file, or `range` (inclusive). Returns the
    /// status (200 or 206) and the body.
    pub async fn cdn(&self, url: &str, range: Option<(u64, u64)>) -> Result<(u16, axum::body::Bytes)> {
        let mut b = self.http().get(url);
        if let Some((a, z)) = range {
            b = b.header(reqwest::header::RANGE, format!("bytes={a}-{z}"));
        }
        // Signed URLs stay out of errors and logs.
        let resp = b.send().await.map_err(|e| e.without_url()).context("CDN request")?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(HttpError { status, retry_after: None, sub_status: None, message: "CDN".into() }.into());
        }
        Ok((status, resp.bytes().await.map_err(|e| e.without_url()).context("CDN body")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_read_from_tidal_bodies() {
        let e = HttpError::parse(401, None, r#"{"status":401,"subStatus":11003,"userMessage":"The token has expired."}"#);
        assert_eq!((e.sub_status, e.message.as_str()), (Some(11003), "The token has expired."));
        assert!(e.is_token_problem());
        let e = HttpError::parse(401, None, r#"{"status":401,"subStatus":4005,"userMessage":"Asset is not ready for playback"}"#);
        assert!(!e.is_token_problem(), "about the track, not the token");
        assert!(HttpError::parse(401, None, "").is_token_problem());
        let e = HttpError::parse(400, None, r#"{"error":"authorization_pending","error_description":"Device Authorization code is not authorized yet"}"#);
        assert_eq!(e.message, "Device Authorization code is not authorized yet");
        assert_eq!(HttpError::parse(502, Some(3), "<html>bad gateway</html>").message, "<html>bad gateway</html>");
    }

    #[test]
    fn user_ids_are_masked_in_paths() {
        assert_eq!(mask_path("users/123456/favorites/tracks"), "users/…/favorites/tracks");
        assert_eq!(mask_path("tracks/77"), "tracks/77");
    }
}
