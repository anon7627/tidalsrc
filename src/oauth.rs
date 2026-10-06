//! TIDAL sign-in, two ways:
//!
//! - **Device code** (`auth.tidal.com/v1/oauth2/device_authorization`): the
//!   user opens `link.tidal.com/XXXXX`, confirms, and the plugin, polling
//!   `oauth2/token` with `grant_type=urn:ietf:params:oauth:grant-type:device_code`,
//!   gets the tokens. Nothing to paste. TIDAL caps these tokens at CD
//!   quality (16-bit/44.1 kHz).
//! - **Browser with PKCE** (`login.tidal.com/authorize`): the user signs in,
//!   lands on `tidal.com/android/login/auth?code=…`, and pastes that
//!   address; the code is traded with the PKCE verifier. These tokens carry
//!   the hi-res entitlement.
//!
//! The token answer holds the account (`user`): its id and country, and a
//! name for the host. Its e-mail address (`username` is often one) is never
//! kept.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::{now_unix_s, Api, HttpError, SCOPE};
use crate::auth::{Credentials, LoginKind};

/// Where the browser sign-in lands, as registered for the PKCE client.
pub const PKCE_REDIRECT: &str = "https://tidal.com/android/login/auth";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// A pending device-code sign-in.
#[derive(Clone, Debug)]
pub struct DeviceAuth {
    pub device_code: String,
    pub user_code: String,
    /// `https://link.tidal.com/XXXXX`.
    pub url: String,
    pub expires_at: i64,
    pub interval_s: u64,
}

/// One poll of the token endpoint.
#[derive(Debug)]
pub enum Poll {
    Pending,
    Expired,
    Done(Box<Credentials>),
}

pub async fn device_authorization(api: &Api) -> Result<DeviceAuth> {
    let client = &api.config().device;
    let url = format!("{}/v1/oauth2/device_authorization", api.config().auth);
    let resp = api
        .http()
        .post(url)
        .form(&[("client_id", client.id.as_str()), ("scope", SCOPE)])
        .send()
        .await
        .context("device authorization")?;
    if !resp.status().is_success() {
        return Err(anyhow::Error::from(HttpError::from_response(resp).await).context("TIDAL refused the device sign-in"));
    }
    let v: Value = resp.json().await?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let device_code = s("deviceCode").ok_or_else(|| anyhow!("device authorization: no deviceCode"))?;
    let user_code = s("userCode").unwrap_or_default();
    let mut url = s("verificationUriComplete")
        .or_else(|| s("verificationUri"))
        .unwrap_or_else(|| format!("link.tidal.com/{user_code}"));
    if !url.starts_with("http") {
        url = format!("https://{url}");
    }
    Ok(DeviceAuth {
        device_code,
        user_code,
        url,
        expires_at: now_unix_s() + v.get("expiresIn").and_then(Value::as_i64).unwrap_or(300),
        interval_s: v.get("interval").and_then(Value::as_u64).unwrap_or(2).clamp(1, 30),
    })
}

/// Ask once whether the user has confirmed the code.
pub async fn poll_device(api: &Api, auth: &DeviceAuth) -> Result<Poll> {
    if now_unix_s() > auth.expires_at {
        return Ok(Poll::Expired);
    }
    let client = &api.config().device;
    let form = [
        ("client_id", client.id.as_str()),
        ("client_secret", client.secret.as_str()),
        ("device_code", auth.device_code.as_str()),
        ("grant_type", DEVICE_GRANT),
        ("scope", SCOPE),
    ];
    let resp = api.http().post(format!("{}/v1/oauth2/token", api.config().auth)).form(&form).send().await?;
    if resp.status().is_success() {
        let v: Value = resp.json().await?;
        return Ok(Poll::Done(Box::new(credentials(api, &v, LoginKind::Device).await?)));
    }
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let error = serde_json::from_str::<Value>(&body).ok().and_then(|v| v.get("error")?.as_str().map(str::to_string));
    match error.as_deref() {
        Some("authorization_pending") | Some("slow_down") => Ok(Poll::Pending),
        Some("expired_token") | Some("access_denied") => Ok(Poll::Expired),
        _ => Err(HttpError::parse(status, None, &body).into()),
    }
}

/// PKCE secret of one browser sign-in.
#[derive(Clone, Debug)]
pub struct Pkce {
    pub verifier: String,
    pub unique_key: String,
}

fn b64url(b: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

impl Pkce {
    pub fn new() -> Self {
        let mut random = uuid::Uuid::new_v4().as_bytes().to_vec();
        random.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        let key = uuid::Uuid::new_v4().simple().to_string();
        Pkce { verifier: b64url(&random), unique_key: key[..16].to_string() }
    }

    pub fn challenge(&self) -> String {
        b64url(&Sha256::digest(self.verifier.as_bytes()))
    }

    /// The sign-in page.
    pub fn url(&self, api: &Api) -> String {
        let q = [
            ("response_type", "code"),
            ("redirect_uri", PKCE_REDIRECT),
            ("client_id", api.config().pkce.id.as_str()),
            ("lang", "EN"),
            ("appMode", "android"),
            ("client_unique_key", self.unique_key.as_str()),
            ("code_challenge", &self.challenge()),
            ("code_challenge_method", "S256"),
            ("restrict_signup", "true"),
        ];
        let q: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(v))).collect();
        format!("{}/authorize?{}", api.config().login, q.join("&"))
    }

    pub async fn exchange(&self, api: &Api, code: &str) -> Result<Credentials> {
        let client = &api.config().pkce;
        let form = [
            ("code", code),
            ("client_id", client.id.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", PKCE_REDIRECT),
            ("scope", SCOPE),
            ("code_verifier", self.verifier.as_str()),
            ("client_unique_key", self.unique_key.as_str()),
        ];
        let resp = api.http().post(format!("{}/v1/oauth2/token", api.config().auth)).form(&form).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow::Error::from(HttpError::from_response(resp).await).context("TIDAL refused the sign-in code"));
        }
        let v: Value = resp.json().await?;
        credentials(api, &v, LoginKind::Pkce).await
    }
}

/// The `code` of a pasted address (`…/login/auth?code=…&state=…`), a query
/// string or a bare code.
pub fn extract_code(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if let Some(i) = input.find("code=") {
        let ok = i == 0 || matches!(input.as_bytes()[i - 1], b'?' | b'&');
        if ok {
            let code = input[i + 5..].split(['&', '#', ' ']).next().unwrap_or("");
            return (!code.is_empty()).then(|| urlencoding::decode(code).map_or(code.to_string(), |c| c.into_owned()));
        }
    }
    (!input.contains(['/', '?', '=', ' '])).then(|| input.to_string())
}

/// A name for the host: first or full name, never something that looks
/// like an e-mail address.
pub fn display_name(user: &Value) -> String {
    let s = |k: &str| user.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty() && !s.contains('@'));
    let full = match (s("firstName"), s("lastName")) {
        (Some(f), Some(l)) => Some(format!("{f} {l}")),
        (Some(f), None) => Some(f.to_string()),
        _ => None,
    };
    full.or_else(|| s("fullName").map(str::to_string))
        .or_else(|| s("nickname").map(str::to_string))
        .unwrap_or_else(|| "TIDAL account".into())
}

/// Credentials from a token answer. The country comes from `user`, else
/// from `GET /v1/sessions`.
pub async fn credentials(api: &Api, v: &Value, kind: LoginKind) -> Result<Credentials> {
    let access_token = v.get("access_token").and_then(Value::as_str).ok_or_else(|| anyhow!("token answer without access_token"))?;
    let refresh_token = v.get("refresh_token").and_then(Value::as_str).unwrap_or_default();
    let user = v.get("user").cloned().unwrap_or_default();
    let mut creds = Credentials {
        kind,
        user_id: user.get("userId").or_else(|| v.get("user_id")).and_then(Value::as_u64).unwrap_or(0),
        country_code: user.get("countryCode").and_then(Value::as_str).unwrap_or_default().to_string(),
        display_name: display_name(&user),
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now_unix_s() + v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600),
    };
    if creds.country_code.is_empty() || creds.user_id == 0 {
        let resp = api
            .http()
            .get(format!("{}/v1/sessions", api.config().api))
            .bearer_auth(&creds.access_token)
            .send()
            .await
            .context("sessions")?;
        if !resp.status().is_success() {
            return Err(anyhow::Error::from(HttpError::from_response(resp).await).context("signed in, but the session is unreadable"));
        }
        let s: Value = resp.json().await?;
        if creds.country_code.is_empty() {
            creds.country_code = s.get("countryCode").and_then(Value::as_str).unwrap_or("US").to_string();
        }
        if creds.user_id == 0 {
            creds.user_id = s.get("userId").and_then(Value::as_u64).unwrap_or(0);
        }
    }
    Ok(creds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Against TIDAL: `TIDALSRC_DATA=<dir> cargo test live_device_sign_in --
    /// --ignored --nocapture`. Prints the link to open, waits for the code
    /// to be confirmed and writes `<dir>/account.json` for the other live
    /// tests.
    #[tokio::test]
    #[ignore]
    async fn live_device_sign_in() {
        let dir = std::path::PathBuf::from(std::env::var("TIDALSRC_DATA").expect("TIDALSRC_DATA"));
        let api = Api::new(crate::api::Config::default());
        let auth = device_authorization(&api).await.unwrap();
        println!("open {} and confirm {}", auth.url, auth.user_code);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(auth.interval_s)).await;
            match poll_device(&api, &auth).await.unwrap() {
                Poll::Pending => continue,
                Poll::Expired => panic!("code expired"),
                Poll::Done(creds) => {
                    creds.save(&dir).unwrap();
                    println!("signed in: {creds}");
                    break;
                }
            }
        }
    }

    #[test]
    fn codes_are_extracted_from_addresses() {
        assert_eq!(extract_code("https://tidal.com/android/login/auth?code=abc_123&state=x").as_deref(), Some("abc_123"));
        assert_eq!(extract_code(" ?code=a%2Bb ").as_deref(), Some("a+b"));
        assert_eq!(extract_code("rawCode42").as_deref(), Some("rawCode42"));
        assert_eq!(extract_code("https://tidal.com/android/login/auth?error=denied"), None);
        assert_eq!(extract_code("https://x/?xcode=1"), None, "only a whole `code` parameter");
        assert_eq!(extract_code(""), None);
    }

    #[test]
    fn pkce_challenge_is_the_verifier_hash() {
        let p = Pkce { verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(), unique_key: "k".into() };
        // RFC 7636, appendix B.
        assert_eq!(p.challenge(), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let a = Pkce::new();
        assert_eq!(a.verifier.len(), 43);
        assert_ne!(a.verifier, Pkce::new().verifier);
        let api = Api::new(crate::api::Config::default());
        let url = a.url(&api);
        assert!(url.starts_with("https://login.tidal.com/authorize?response_type=code&redirect_uri=https%3A%2F%2Ftidal.com"));
        assert!(url.contains(&format!("code_challenge={}", a.challenge())));
    }

    #[test]
    fn names_never_show_an_email() {
        assert_eq!(display_name(&json!({"firstName": "Ada", "lastName": "L", "username": "ada@x.org"})), "Ada L");
        assert_eq!(display_name(&json!({"username": "ada@x.org", "firstName": "ada@x.org"})), "TIDAL account");
        assert_eq!(display_name(&json!({"nickname": "ada"})), "ada");
    }
}
