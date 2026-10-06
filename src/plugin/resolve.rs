//! `track.resolve`: a stream in a format the output plays natively.
//!
//! `tracks/<id>/playbackinfo?audioquality=…&playbackmode=STREAM&assetpresentation=FULL`
//! answers the format TIDAL picked (`audioQuality`, `sampleRate`,
//! `bitDepth`), ReplayGain and a manifest:
//! - BTS, unencrypted: the file's URL, handed to the host as is (`direct`);
//! - DASH with FLAC (hi-res): segments rebuilt into one FLAC file by the
//!   local relay (`proxied`);
//! - anything encrypted (`OLD_AES`, DRM): `unavailable`, with the reason.
//!
//! The quality asked first is the best the output takes (hi-res, or CD
//! quality for an output limited to 16 bits) within the `quality` setting.
//! When TIDAL hands out a format the output would have to convert (rate
//! missing from `rates`, above `max_rate` or `max_bits`), the next lower
//! quality is asked; when none fits, `unavailable`.

use serde::Deserialize;
use serde_json::{json, Value};

use super::relay::{self, Relay};
use super::rpc::{RpcError, Unavailable};
use crate::api::{now_unix_s, Api, Req};
use crate::dash::{self, Manifest};
use crate::fmp4;

/// Margin taken off an announced expiry.
const EXPIRY_MARGIN_S: i64 = 60;
/// Lifetime assumed for relayed segment URLs that do not say.
const RELAY_TTL_S: i64 = 3 * 3600;

/// TIDAL's qualities, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    /// AAC 320 kbit/s.
    High,
    /// FLAC 16-bit/44.1 kHz.
    Lossless,
    /// FLAC up to 24-bit/192 kHz.
    HiRes,
}

impl Quality {
    pub fn api(self) -> &'static str {
        match self {
            Quality::High => "HIGH",
            Quality::Lossless => "LOSSLESS",
            Quality::HiRes => "HI_RES_LOSSLESS",
        }
    }

    /// `HI_RES` is the old MQA tier, served as 16-bit FLAC nowadays.
    pub fn from_api(s: &str) -> Option<Self> {
        Some(match s {
            "HI_RES_LOSSLESS" => Quality::HiRes,
            "LOSSLESS" | "HI_RES" => Quality::Lossless,
            "HIGH" | "LOW" => Quality::High,
            _ => return None,
        })
    }

    fn lower(self) -> Option<Self> {
        match self {
            Quality::HiRes => Some(Quality::Lossless),
            Quality::Lossless => Some(Quality::High),
            Quality::High => None,
        }
    }
}

/// What the host's current output accepts natively (`initialize.output`,
/// `output.changed`).
#[derive(Deserialize, Default, Clone, Debug, PartialEq)]
pub struct Output {
    #[serde(default)]
    pub max_rate: Option<u32>,
    #[serde(default)]
    pub max_bits: Option<u32>,
    #[serde(default)]
    pub rates: Vec<u32>,
}

impl Output {
    /// Quality to ask first. Hi-res files come at 44.1 to 192 kHz: the rate
    /// is only known from TIDAL's answer, then checked with `accepts`.
    pub fn best(&self) -> Quality {
        if self.max_bits.is_some_and(|b| b < 24) {
            Quality::Lossless
        } else {
            Quality::HiRes
        }
    }

    /// Whether a stream of this format plays natively (0 = not announced).
    pub fn accepts(&self, rate: u32, bits: u32) -> bool {
        let rate_ok = rate == 0
            || if self.rates.is_empty() { self.max_rate.is_none_or(|m| rate <= m) } else { self.rates.contains(&rate) };
        let bits_ok = bits == 0 || self.max_bits.is_none_or(|m| bits <= m);
        rate_ok && bits_ok
    }
}

fn unavailable(message: impl Into<String>) -> RpcError {
    RpcError::from(anyhow::Error::from(Unavailable(message.into())))
}

/// Expiry of a signed CDN URL (`exp=<unix seconds>`, Akamai and CloudFront
/// style), minus a margin; `None` when it does not say.
pub fn expires_at(url: &str, now: i64) -> Option<i64> {
    let mut rest = url.split_once('?')?.1;
    while let Some(i) = rest.find("exp") {
        let after = &rest[i + 3..];
        let after = after.strip_prefix("ires").unwrap_or(after);
        let boundary = i == 0 || matches!(rest.as_bytes()[i - 1], b'&' | b'~' | b'=' | b'?');
        if let (true, Some(v)) = (boundary, after.strip_prefix('=')) {
            let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(t) = digits.parse::<i64>() {
                if t > now {
                    return Some((t - EXPIRY_MARGIN_S).max(now + 1));
                }
            }
        }
        rest = after;
    }
    None
}

/// A stream ready for the host.
#[derive(Debug)]
struct Stream {
    url: String,
    expires_at: Option<i64>,
    sample_rate: u32,
    bits: u32,
    channels: u32,
    codec: &'static str,
    proxied: bool,
}

/// What `playbackinfo` said about the format.
struct Info {
    quality: Quality,
    sample_rate: u32,
    bits: u32,
}

/// One playable stream from a `playbackinfo` answer; `Ok(None)` when its
/// format does not suit the output (ask a lower quality).
async fn stream_of(api: &Api, relay: &Relay, id: &str, info: &Info, manifest: Manifest, output: &Output) -> Result<Option<Stream>, RpcError> {
    let now = now_unix_s();
    match manifest {
        Manifest::Bts(b) => {
            if !b.encryption.eq_ignore_ascii_case("NONE") {
                return Err(unavailable(format!("TIDAL sent track {id} encrypted ({}); this sign-in cannot play it", b.encryption)));
            }
            let codec = dash::codec_name(&b.codecs, &b.mime_type);
            let rate = if info.sample_rate == 0 { 44_100 } else { info.sample_rate };
            let bits = if codec == "flac" { info.bits } else { 0 };
            if matches!(codec, "dolby" | "unknown") || !output.accepts(rate, bits) {
                tracing::debug!("track {id}: {codec} {rate} Hz / {bits} bits not played natively");
                return Ok(None);
            }
            let url = b.urls[0].clone();
            Ok(Some(Stream { expires_at: expires_at(&url, now), url, sample_rate: rate, bits, channels: 2, codec, proxied: false }))
        }
        Manifest::Dash(m) => {
            if m.protected {
                return Err(unavailable(format!("TIDAL sent track {id} with DRM (Widevine); it cannot be played")));
            }
            if dash::codec_name(&m.codecs, "") != "flac" {
                tracing::debug!("track {id}: DASH stream in {}, not supported", m.codecs);
                return Ok(None);
            }
            let rate_hint = if info.sample_rate == 0 { m.sample_rate.unwrap_or(0) } else { info.sample_rate };
            if !output.accepts(rate_hint, info.bits) {
                tracing::debug!("track {id}: {rate_hint} Hz / {} bits not played natively", info.bits);
                return Ok(None);
            }
            let (_, init) = api.cdn(&m.init_url, None).await.map_err(|e| e.context("init segment"))?;
            let total = (m.timescale == u64::from(rate_hint)).then_some(m.duration);
            let header = fmp4::flac_header(&init, total).map_err(|e| unavailable(format!("track {id}: {e:#}")))?;
            if !output.accepts(header.sample_rate, header.bits) {
                tracing::debug!("track {id}: {} Hz / {} bits not played natively", header.sample_rate, header.bits);
                return Ok(None);
            }
            let sizes = relay::measure(api.http(), &m.segment_urls).await.map_err(|e| e.context("measuring segments"))?;
            let expires = expires_at(&m.init_url, now).unwrap_or(now + RELAY_TTL_S);
            let (sample_rate, bits, channels) = (header.sample_rate, header.bits, header.channels);
            let segments = m.segment_urls.into_iter().zip(sizes).collect();
            let stream = relay::Stream { track_id: id.to_string(), header: header.bytes, segments, expires_at: expires };
            let url = relay.register(stream).await?;
            Ok(Some(Stream { url, expires_at: Some(expires), sample_rate, bits, channels, codec: "flac", proxied: true }))
        }
    }
}

/// The best stream within `cap` that the output plays natively.
async fn best_stream(api: &Api, relay: &Relay, id: &str, output: &Output, cap: Quality) -> Result<(Stream, Value), RpcError> {
    let mut quality = output.best().min(cap);
    loop {
        let req = Req::get(format!("tracks/{id}/playbackinfo"))
            .q("audioquality", quality.api())
            .q("playbackmode", "STREAM")
            .q("assetpresentation", "FULL");
        let v = api.call(req).await?;
        if v.get("assetPresentation").and_then(Value::as_str).is_some_and(|p| p != "FULL") {
            return Err(unavailable("only a preview is available with this subscription"));
        }
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default();
        let info = Info {
            quality: Quality::from_api(s("audioQuality")).unwrap_or(quality).min(quality),
            sample_rate: v.get("sampleRate").and_then(Value::as_u64).unwrap_or(0) as u32,
            bits: v.get("bitDepth").and_then(Value::as_u64).unwrap_or(0) as u32,
        };
        let manifest = dash::parse(s("manifestMimeType"), s("manifest"))
            .map_err(|e| unavailable(format!("track {id}: unreadable manifest ({e:#})")))?;
        if let Some(stream) = stream_of(api, relay, id, &info, manifest, output).await? {
            return Ok((stream, v));
        }
        quality = info.quality.lower().ok_or_else(|| {
            unavailable(format!("no format of track {id} plays natively on this output (last offered: {} Hz, {} bits)", info.sample_rate, info.bits))
        })?;
    }
}

pub async fn resolve(api: &Api, relay: &Relay, reference: &str, output: &Output, cap: Quality) -> Result<Value, RpcError> {
    let id = reference
        .strip_prefix("track/")
        .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        .ok_or_else(|| RpcError::not_found(format!("{reference} is not a track")))?;
    let (stream, track) = tokio::join!(best_stream(api, relay, id, output, cap), api.get(format!("tracks/{id}")));
    let (stream, info) = stream?;
    let mut format = json!({"sample_rate": stream.sample_rate, "channels": stream.channels, "codec": stream.codec});
    if stream.bits > 0 {
        format["bits"] = stream.bits.into();
    }
    let mut out = json!({
        "url": stream.url,
        "format": format,
        "live": false,
        "delivery": if stream.proxied { "proxied" } else { "direct" },
    });
    if let Some(t) = stream.expires_at {
        out["expires_at"] = t.into();
    }
    match track {
        Ok(track) => {
            if let Some(secs) = track.get("duration").and_then(Value::as_u64) {
                out["duration_ms"] = (secs * 1000).into();
            }
        }
        Err(e) => tracing::debug!("tracks/{id}: {e:#}"),
    }
    let gain = info.get("trackReplayGain").and_then(Value::as_f64);
    let peak = info.get("trackPeakAmplitude").and_then(Value::as_f64);
    if let (Some(gain), Some(peak)) = (gain, peak) {
        out["replaygain"] = json!({"track_gain": gain, "track_peak": peak});
    }
    tracing::info!("track {id}: {} {} Hz/{} bits, {}", stream.codec, stream.sample_rate, stream.bits, out["delivery"]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against TIDAL: `TIDALSRC_DATA=<dir holding account.json>
    /// TIDALSRC_TRACK=<id> TIDALSRC_OUT=<file> cargo test live_resolve --
    /// --ignored --nocapture`. Resolves the track for a 24-bit/192 kHz
    /// output, downloads it (through the relay for DASH) and writes it.
    #[tokio::test]
    #[ignore]
    async fn live_resolve() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k}"));
        let dir = std::path::PathBuf::from(env("TIDALSRC_DATA"));
        let api = Api::new(crate::api::Config::default());
        api.set_data_dir(Some(dir.clone()));
        api.set_credentials(crate::auth::Credentials::load(&dir).unwrap()).await;
        let relay = Relay::default();
        let output = Output { max_rate: Some(192_000), max_bits: Some(24), rates: vec![] };
        let out = resolve(&api, &relay, &format!("track/{}", env("TIDALSRC_TRACK")), &output, Quality::HiRes).await.unwrap();
        println!("{} {}", out["delivery"], out["format"]);
        let resp = reqwest::get(out["url"].as_str().unwrap()).await.unwrap();
        let length: usize = resp.headers()["content-length"].to_str().unwrap().parse().unwrap();
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.len(), length);
        if out["format"]["codec"] == "flac" {
            assert!(body.starts_with(b"fLaC"));
        }
        std::fs::write(env("TIDALSRC_OUT"), &body).unwrap();
        println!("{} bytes", body.len());
    }

    fn out(max_rate: u32, max_bits: u32, rates: &[u32]) -> Output {
        Output { max_rate: Some(max_rate), max_bits: Some(max_bits), rates: rates.to_vec() }
    }

    #[test]
    fn quality_follows_the_dac() {
        assert_eq!(Output::default().best(), Quality::HiRes);
        assert_eq!(out(192_000, 24, &[]).best(), Quality::HiRes);
        assert_eq!(out(48_000, 24, &[]).best(), Quality::HiRes, "44.1 and 48 kHz hi-res files exist");
        assert_eq!(out(192_000, 16, &[]).best(), Quality::Lossless);
        assert_eq!(Quality::HiRes.min(Quality::Lossless), Quality::Lossless, "the setting caps it");
        assert_eq!(Quality::from_api("HI_RES"), Some(Quality::Lossless));
        assert_eq!(Quality::Lossless.lower(), Some(Quality::High));
        assert_eq!(Quality::High.lower(), None);
    }

    #[test]
    fn accepted_formats() {
        let dac = out(192_000, 24, &[44_100, 48_000, 96_000, 192_000]);
        assert!(dac.accepts(96_000, 24));
        assert!(!dac.accepts(88_200, 24), "rate missing from the list");
        assert!(!dac.accepts(96_000, 32));
        assert!(dac.accepts(0, 0));
        assert!(out(96_000, 24, &[]).accepts(88_200, 24));
        assert!(!out(96_000, 24, &[]).accepts(192_000, 24));
        assert!(!out(48_000, 24, &[48_000]).accepts(44_100, 16), "a 48 kHz-only output plays no CD-quality file");
    }

    #[test]
    fn expiry_comes_from_the_url_when_present() {
        let now = 1_000_000;
        assert_eq!(expires_at("https://cdn/f.flac?token=exp=1000600~acl=/*~hmac=x", now), Some(1_000_540));
        assert_eq!(expires_at("https://cdn/f.flac?Expires=1000600&Signature=x", now), None, "case matters: not ours");
        assert_eq!(expires_at("https://cdn/f.flac?expires=1000600&sig=x", now), Some(1_000_540));
        assert_eq!(expires_at("https://cdn/f.flac?token=exp=999", now), None, "already past");
        assert_eq!(expires_at("https://cdn/f.flac?xexp=1000600", now), None);
        assert_eq!(expires_at("https://cdn/f.flac", now), None);
    }
}
