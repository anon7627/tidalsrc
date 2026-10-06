//! `tidalsrc plugin`: source plugin (protocol v1, see `docs/plugin.md`).
//! JSON-RPC on stdin/stdout.

mod catalog;
mod details;
mod items;
mod lyrics;
mod pages;
mod playlists;
mod relay;
mod resolve;
mod rpc;
mod settings;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::task::AbortHandle;

use crate::api::{find, now_unix_s, status_of, Api, Config, SignInNeeded};
use crate::auth::{Credentials, LoginKind};
use crate::oauth::{self, DeviceAuth, Pkce, Poll};
use resolve::Output;
use rpc::{Incoming, Out, RpcError, RpcResult, AUTH_REQUIRED};
use settings::SignIn;

pub const PROTOCOL: u64 = 1;
/// Favourites may change in other apps: read their ids again after this.
const FAVORITES_MAX_AGE: Duration = Duration::from_secs(10 * 60);

/// A sign-in started by `auth.begin`.
enum Pending {
    Device { auth: DeviceAuth, poller: AbortHandle },
    Browser(Pkce),
}

struct State {
    data_dir: Option<PathBuf>,
    output: Output,
    pending: Option<Pending>,
    /// `auth.changed {expired}` already sent for the current sign-in.
    expiry_notified: bool,
    /// Language of the host's `locale` (`fr`), for the labels we write.
    lang: String,
    settings: settings::Settings,
    /// When the favourite ids were last requested.
    favorites_at: Option<std::time::Instant>,
}

struct Plugin {
    out: Out,
    state: Mutex<State>,
    api: Api,
    /// Serves DASH streams as plain FLAC files.
    relay: relay::Relay,
}

pub async fn run(config: Config) -> Result<()> {
    let (out, writer) = Out::stdout();
    let plugin = Arc::new(Plugin {
        out: out.clone(),
        state: Mutex::new(State {
            data_dir: None,
            output: Output::default(),
            pending: None,
            expiry_notified: false,
            lang: "en".into(),
            settings: settings::Settings::default(),
            favorites_at: None,
        }),
        api: Api::new(config),
        relay: relay::Relay::default(),
    });
    tracing::info!("plugin mode, protocol v{PROTOCOL}");

    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match rpc::parse(&line) {
            Incoming::Request { id, method, params } => match method.as_str() {
                // Handled in order: nothing else makes sense before it ends.
                "initialize" => out.respond(id, plugin.clone().initialize(params).await),
                "shutdown" => {
                    out.respond(id, Ok(Value::Null));
                    break;
                }
                _ => {
                    let plugin = plugin.clone();
                    tokio::spawn(async move {
                        let result = plugin.clone().handle(&method, params).await;
                        plugin.note_auth_failure(&method, &result).await;
                        plugin.out.respond(id, result);
                    });
                }
            },
            Incoming::Notification { method, params } => plugin.notification(&method, params).await,
            Incoming::Response { id, .. } => tracing::debug!("answer to unknown request {id}"),
            Incoming::Invalid { id, error } => out.respond(id, Err(error)),
        }
    }
    tracing::info!("plugin stopping");
    plugin.cancel_login();
    drop(out);
    drop(plugin);
    // In-flight tasks may still hold a sender; give the writer a moment.
    let _ = tokio::time::timeout(Duration::from_millis(500), writer).await;
    // The blocking stdin reader would keep the runtime alive.
    std::process::exit(0)
}

/// Language of a locale: `fr-FR`, `fr_FR.UTF-8`, `fr` → `fr`; `en` if none.
fn lang_of(locale: Option<&str>) -> String {
    let lang = locale.unwrap_or("").split(['-', '_', '.']).next().unwrap_or("").to_ascii_lowercase();
    if lang.is_empty() || !lang.chars().all(|c| c.is_ascii_lowercase()) {
        "en".into()
    } else {
        lang
    }
}

/// TIDAL's `locale` parameter: `fr-BE` → `fr_BE`, `fr` → `fr_FR`, `en` →
/// `en_US`.
fn api_locale(locale: Option<&str>) -> String {
    let lang = lang_of(locale);
    let region = locale
        .and_then(|l| l.split('.').next())
        .and_then(|l| l.split(['-', '_']).nth(1))
        .filter(|r| r.len() == 2 && r.chars().all(|c| c.is_ascii_alphabetic()))
        .map(str::to_ascii_uppercase);
    let region = region.unwrap_or_else(|| if lang == "en" { "US".into() } else { lang.to_ascii_uppercase() });
    format!("{lang}_{region}")
}

fn params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
}

#[derive(Deserialize)]
struct InitializeParams {
    data_dir: PathBuf,
    #[serde(default)]
    output: Option<Output>,
    #[serde(default)]
    protocol: Option<u64>,
    #[serde(default)]
    locale: Option<String>,
    /// Values the host stored for our settings.
    #[serde(default)]
    settings: Option<Value>,
}

#[derive(Deserialize)]
struct RefParams {
    #[serde(rename = "ref")]
    reference: String,
}

#[derive(Deserialize)]
struct FavoriteParams {
    #[serde(rename = "ref")]
    reference: String,
    on: bool,
}

#[derive(Deserialize)]
struct CompleteParams {
    #[serde(default)]
    input: String,
}

/// `HIFI_PLUS` → `HiFi Plus`.
fn subscription_name(kind: &str) -> String {
    kind.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| match w {
            "HIFI" => "HiFi".to_string(),
            w => {
                let lower = w.to_ascii_lowercase();
                let mut c = lower.chars();
                c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Plugin {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn data_dir(&self) -> Result<PathBuf, RpcError> {
        self.state().data_dir.clone().ok_or_else(|| RpcError::new(rpc::INVALID_REQUEST, "initialize first"))
    }

    async fn initialize(self: Arc<Self>, p: Value) -> RpcResult {
        let p: InitializeParams = params(p)?;
        if p.protocol.is_some_and(|v| v != PROTOCOL) {
            tracing::warn!("host speaks protocol {:?}, this plugin v{PROTOCOL}", p.protocol);
        }
        let creds = Credentials::load(&p.data_dir).map_err(RpcError::from)?;
        let lang = lang_of(p.locale.as_deref());
        {
            let mut st = self.state();
            st.output = p.output.unwrap_or_default();
            st.data_dir = Some(p.data_dir.clone());
            st.expiry_notified = false;
            st.lang = lang.clone();
            st.settings = settings::Settings::from_values(p.settings.as_ref());
            st.favorites_at = None;
            tracing::info!("settings: {:?}", st.settings);
        }
        items::set_lang(&lang);
        items::set_user(creds.as_ref().map(|c| c.user_id));
        pages::clear();
        self.api.set_data_dir(Some(p.data_dir));
        self.api.set_locale(&api_locale(p.locale.as_deref()));
        match &creds {
            Some(c) => tracing::info!("initialized, signed in ({c})"),
            None => tracing::info!("initialized, signed out"),
        }
        let signed_in = creds.is_some();
        self.api.set_credentials(creds).await;
        if signed_in {
            self.refresh_favorites();
        }
        Ok(json!({
            "protocol": PROTOCOL,
            "plugin": {"id": "tidal", "name": "TIDAL", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "auth": true, "browse": true, "search": true, "resolve": true,
                "favorites": true, "reporting": false, "remote_control": false, "library": true,
                "lyrics": true, "radio": true, "details": true, "playlist_edit": true
            },
            "settings": settings::declaration(&lang),
        }))
    }

    async fn handle(self: Arc<Self>, method: &str, p: Value) -> RpcResult {
        if matches!(method, "browse.list" | "search" | "item.get") || method.starts_with("library.") {
            self.refresh_favorites();
        }
        let api = &self.api;
        match method {
            "auth.status" => self.auth_status().await,
            "auth.begin" => self.auth_begin().await,
            "auth.complete" => self.auth_complete(params(p)?).await,
            "auth.sign_out" => {
                self.cancel_login();
                let data_dir = self.data_dir()?;
                Credentials::delete(&data_dir).map_err(RpcError::from)?;
                api.set_credentials(None).await;
                items::set_user(None);
                pages::clear();
                self.state().favorites_at = None;
                tracing::info!("signed out");
                Ok(json!({"state": "signed_out"}))
            }
            "browse.root" => catalog::root(api).await,
            "browse.list" => catalog::list(api, params(p)?).await,
            "search" => catalog::search(api, params(p)?).await,
            "library.albums" | "library.artists" | "library.tracks" | "library.playlists" => {
                catalog::library(api, method, params(p)?).await
            }
            "item.get" => catalog::get(api, &params::<RefParams>(p)?.reference).await,
            "item.details" => details::get(api, &params::<RefParams>(p)?.reference).await,
            "playlists.create" | "playlists.rename" | "playlists.delete" | "playlists.add" | "playlists.remove"
            | "playlists.move" => playlists::edit(api, method, params(p)?).await,
            "radio.next" => catalog::radio_next(api, params(p)?).await,
            "lyrics.get" => lyrics::get(api, &params::<RefParams>(p)?.reference).await,
            "favorites.set" => {
                let p: FavoriteParams = params(p)?;
                catalog::set_favorite(api, &p.reference, p.on).await
            }
            "track.resolve" => {
                let p: RefParams = params(p)?;
                let (output, cap) = {
                    let st = self.state();
                    (st.output.clone(), st.settings.max_quality)
                };
                resolve::resolve(api, &self.relay, &p.reference, &output, cap).await
            }
            _ => Err(RpcError::new(rpc::METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }

    async fn notification(self: &Arc<Self>, method: &str, p: Value) {
        match method {
            "settings.changed" => {
                let s = settings::Settings::from_values(p.get("settings"));
                tracing::info!("settings changed: {s:?}");
                self.state().settings = s;
            }
            "locale.changed" => {
                let locale = p.get("locale").and_then(Value::as_str);
                let lang = lang_of(locale);
                tracing::info!("host language: {lang}");
                items::set_lang(&lang);
                self.api.set_locale(&api_locale(locale));
                pages::clear();
                self.state().lang = lang.clone();
                // Settings' labels follow.
                self.out.notify("settings.declared", json!({"settings": settings::declaration(&lang)}));
            }
            "output.changed" => {
                // Accept the output object itself or wrapped in `output`.
                let p = p.get("output").cloned().unwrap_or(p);
                match serde_json::from_value::<Output>(p) {
                    Ok(output) => {
                        tracing::info!("output changed: max {:?} Hz / {:?} bits", output.max_rate, output.max_bits);
                        self.state().output = output;
                    }
                    Err(e) => tracing::warn!("output.changed: {e}"),
                }
            }
            _ => tracing::debug!("ignored notification {method}"),
        }
    }

    /// Read the favourite ids again in the background, when they are older
    /// than `FAVORITES_MAX_AGE` (or were never read).
    fn refresh_favorites(self: &Arc<Self>) {
        {
            let mut st = self.state();
            if st.favorites_at.is_some_and(|t| t.elapsed() < FAVORITES_MAX_AGE) {
                return;
            }
            st.favorites_at = Some(std::time::Instant::now());
        }
        let me = self.clone();
        tokio::spawn(async move {
            if !me.api.signed_in().await {
                me.state().favorites_at = None;
                return;
            }
            match catalog::favorite_ids(&me.api).await {
                Ok(ids) => items::set_favorites(Some(ids)),
                Err(e) => {
                    tracing::info!("favourite ids not read: {e:#}");
                    me.state().favorites_at = None;
                }
            }
        });
    }

    /// The sign-in refused mid-session: tell the host once, without
    /// waiting for its next `auth.status`.
    async fn note_auth_failure(&self, method: &str, result: &RpcResult) {
        if !matches!(result, Err(e) if e.code == AUTH_REQUIRED) || method.starts_with("auth.") || !self.api.signed_in().await {
            return;
        }
        let mut st = self.state();
        if !st.expiry_notified {
            st.expiry_notified = true;
            self.out.notify("auth.changed", json!({"state": "expired"}));
        }
    }

    /// Account shown by the host: a name (never the e-mail address: hosts
    /// keep this in logs and diagnostic reports) and the subscription.
    async fn account_json(&self, creds: &Credentials) -> Value {
        let mut account = json!({"display_name": creds.display_name});
        let sub = self.api.get(format!("users/{}/subscription", creds.user_id)).await;
        let mut detail = match sub {
            Ok(v) => v.pointer("/subscription/type").and_then(Value::as_str).map(subscription_name),
            Err(e) => {
                tracing::debug!("subscription unreadable: {e:#}");
                None
            }
        };
        if creds.kind == LoginKind::Device {
            let cap = items::tr("up to CD quality (code sign-in)", "qualité CD au mieux (connexion par code)");
            detail = Some(match detail {
                Some(d) => format!("{d} · {cap}"),
                None => cap,
            });
        }
        if let Some(d) = detail {
            account["detail"] = d.into();
        }
        account
    }

    async fn auth_status(&self) -> RpcResult {
        let data_dir = self.data_dir()?;
        if self.api.credentials().await.is_none() {
            // Signed in from elsewhere since (another process, by hand).
            let creds = Credentials::load(&data_dir).map_err(RpcError::from)?;
            if creds.is_none() {
                return Ok(json!({"state": "signed_out"}));
            }
            items::set_user(creds.as_ref().map(|c| c.user_id));
            self.api.set_credentials(creds).await;
        }
        match self.api.get("sessions").await {
            Ok(_) => {
                self.state().expiry_notified = false;
                let creds = self.api.session().await?;
                Ok(json!({"state": "signed_in", "account": self.account_json(&creds).await}))
            }
            Err(e)
                if find::<SignInNeeded>(&e).is_some() || matches!(status_of(&e), Some(401 | 403)) =>
            {
                tracing::info!("stored sign-in refused: {e:#}");
                let name = self.api.credentials().await.map(|c| c.display_name).unwrap_or_default();
                Ok(json!({"state": "expired", "account": {"display_name": name}}))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn auth_begin(self: Arc<Self>) -> RpcResult {
        self.data_dir()?;
        self.cancel_login();
        let method = self.state().settings.sign_in;
        match method {
            SignIn::Device => {
                let auth = oauth::device_authorization(&self.api).await?;
                let me = self.clone();
                let polled = auth.clone();
                let poller = tokio::spawn(async move { me.poll_device(polled).await }).abort_handle();
                let url = auth.url.clone();
                let instructions = items::tr(
                    &format!(
                        "Open the page, sign in to TIDAL and confirm the code {}. Sign-in then completes by itself, \
                         within a few seconds. This sign-in plays CD quality at most; for hi-res, pick \"With the TIDAL \
                         page\" in the plugin's settings.",
                        auth.user_code
                    ),
                    &format!(
                        "Ouvrez la page, connectez-vous à TIDAL et confirmez le code {}. La connexion se termine \
                         ensuite d'elle-même en quelques secondes. Cette connexion lit en qualité CD au mieux ; pour \
                         la haute résolution, choisissez « Avec la page TIDAL » dans les réglages du plugin.",
                        auth.user_code
                    ),
                );
                self.state().pending = Some(Pending::Device { auth, poller });
                Ok(json!({"url": url, "instructions": instructions, "expects_input": false}))
            }
            SignIn::Browser => {
                let pkce = Pkce::new();
                let url = pkce.url(&self.api);
                self.state().pending = Some(Pending::Browser(pkce));
                let instructions = items::tr(
                    "Sign in on the TIDAL page. You then land on a TIDAL page that may say it was not found: copy its \
                     full address (it contains \"code=\") and paste it here.",
                    "Connectez-vous sur la page TIDAL. Vous arrivez ensuite sur une page TIDAL qui peut indiquer \
                     qu'elle est introuvable : copiez son adresse complète (elle contient « code= ») et collez-la ici.",
                );
                Ok(json!({"url": url, "instructions": instructions, "expects_input": true}))
            }
        }
    }

    /// Poll the token endpoint until the user confirms the code, or it
    /// expires.
    async fn poll_device(self: Arc<Self>, auth: DeviceAuth) {
        loop {
            tokio::time::sleep(Duration::from_secs(auth.interval_s)).await;
            match oauth::poll_device(&self.api, &auth).await {
                Ok(Poll::Pending) => continue,
                Ok(Poll::Expired) => {
                    tracing::info!("device code expired before it was confirmed");
                    break;
                }
                Ok(Poll::Done(creds)) => {
                    self.state().pending = None;
                    match self.sign_in(*creds).await {
                        Ok(status) => self.out.notify("auth.changed", status),
                        Err(e) => tracing::warn!("sign-in failed: {}", e.message),
                    }
                    break;
                }
                Err(e) => {
                    tracing::info!("device sign-in poll: {e:#}");
                    if now_unix_s() > auth.expires_at {
                        break;
                    }
                }
            }
        }
    }

    async fn auth_complete(self: &Arc<Self>, p: CompleteParams) -> RpcResult {
        let pending = self.state().pending.take();
        match pending {
            Some(Pending::Browser(pkce)) => {
                let Some(code) = oauth::extract_code(&p.input) else {
                    self.state().pending = Some(Pending::Browser(pkce));
                    return Err(RpcError::invalid_params("no sign-in code in the input (the address contains \"code=\")"));
                };
                let creds = pkce.exchange(&self.api, &code).await?;
                self.sign_in(creds).await
            }
            Some(Pending::Device { auth, poller }) => match oauth::poll_device(&self.api, &auth).await? {
                Poll::Done(creds) => {
                    poller.abort();
                    self.sign_in(*creds).await
                }
                Poll::Pending => {
                    self.state().pending = Some(Pending::Device { auth, poller });
                    Err(RpcError::invalid_params("the code is not confirmed yet: confirm it on the TIDAL page first"))
                }
                Poll::Expired => {
                    poller.abort();
                    Err(RpcError::invalid_params("the code expired: start the sign-in again"))
                }
            },
            None => Err(RpcError::invalid_params("no sign-in in progress: call auth.begin first")),
        }
    }

    async fn sign_in(self: &Arc<Self>, creds: Credentials) -> RpcResult {
        let data_dir = self.data_dir()?;
        creds.save(&data_dir).map_err(RpcError::from)?;
        items::set_user(Some(creds.user_id));
        pages::clear();
        self.api.set_credentials(Some(creds.clone())).await;
        {
            let mut st = self.state();
            st.expiry_notified = false;
            st.favorites_at = None;
        }
        self.refresh_favorites();
        tracing::info!("signed in ({creds})");
        Ok(json!({"state": "signed_in", "account": self.account_json(&creds).await}))
    }

    fn cancel_login(&self) {
        if let Some(Pending::Device { poller, .. }) = self.state().pending.take() {
            poller.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales() {
        assert_eq!(lang_of(Some("fr-FR")), "fr");
        assert_eq!(lang_of(Some("fr_FR.UTF-8")), "fr");
        assert_eq!(lang_of(None), "en");
        assert_eq!(api_locale(Some("fr-BE")), "fr_BE");
        assert_eq!(api_locale(Some("fr_FR.UTF-8")), "fr_FR");
        assert_eq!(api_locale(Some("de")), "de_DE");
        assert_eq!(api_locale(Some("en")), "en_US");
        assert_eq!(api_locale(Some("")), "en_US");
    }

    #[test]
    fn subscription_names() {
        assert_eq!(subscription_name("HIFI_PLUS"), "HiFi Plus");
        assert_eq!(subscription_name("PREMIUM"), "Premium");
    }
}
