//! Stream manifests from `playbackinfo` (`manifest`, base64):
//!
//! - `application/vnd.tidal.bts`: JSON `{mimeType, codecs, encryptionType,
//!   urls}`, one URL of the whole file;
//! - `application/dash+xml`: an MPEG-DASH MPD. TIDAL serves hi-res FLAC this
//!   way, as fragmented MP4 segments: one `SegmentTemplate` with absolute
//!   `initialization` and `media` (`$Number$`) URLs and a `SegmentTimeline`
//!   (`<S d="…" r="…"/>`) that gives the number of segments.

use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::Engine;
use serde_json::Value;

pub const BTS: &str = "application/vnd.tidal.bts";
pub const DASH: &str = "application/dash+xml";
/// Segments a manifest may list.
const MAX_SEGMENTS: u64 = 20_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Manifest {
    Bts(Bts),
    Dash(Mpd),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bts {
    pub mime_type: String,
    pub codecs: String,
    /// `NONE`, or the scheme (`OLD_AES`…).
    pub encryption: String,
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mpd {
    /// `flac`, `mp4a.40.2`…
    pub codecs: String,
    pub sample_rate: Option<u32>,
    /// `ContentProtection` present: DRM.
    pub protected: bool,
    pub init_url: String,
    pub segment_urls: Vec<String>,
    /// Sum of the timeline, in `timescale` units, and the timescale.
    pub duration: u64,
    pub timescale: u64,
}

/// Codec name for the host, from a `codecs` value.
pub fn codec_name(codecs: &str, mime: &str) -> &'static str {
    let c = codecs.to_ascii_lowercase();
    if c.contains("flac") {
        "flac"
    } else if c.starts_with("mp4a") || c.contains("aac") {
        "aac"
    } else if c.contains("mp3") || mime == "audio/mpeg" {
        "mp3"
    } else if c.contains("vorbis") || c.contains("ogg") {
        "vorbis"
    } else if c.contains("ec-3") || c.contains("eac3") || c.contains("ac-4") || c.contains("ac4") {
        "dolby"
    } else if mime.contains("flac") {
        "flac"
    } else {
        "unknown"
    }
}

pub fn parse(mime: &str, manifest_b64: &str) -> Result<Manifest> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(manifest_b64.trim())
        .context("manifest is not base64")?;
    let text = String::from_utf8(raw).context("manifest is not UTF-8")?;
    match mime {
        BTS => parse_bts(&text).map(Manifest::Bts),
        DASH => parse_mpd(&text).map(Manifest::Dash),
        other => bail!("unknown manifest type {other}"),
    }
}

pub fn parse_bts(text: &str) -> Result<Bts> {
    let v: Value = serde_json::from_str(text).context("BTS manifest")?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let urls: Vec<String> =
        v.get("urls").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
    ensure!(!urls.is_empty(), "BTS manifest without URLs");
    let encryption = match s("encryptionType") {
        e if e.is_empty() => "NONE".to_string(),
        e => e,
    };
    Ok(Bts { mime_type: s("mimeType"), codecs: s("codecs"), encryption, urls })
}

fn attr<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.attributes().find(|a| a.name() == name).map(|a| a.value())
}

/// The nearest element named `name` among `node` and its ancestors' direct
/// children (DASH inherits `SegmentTemplate` and `BaseURL` downwards).
fn inherited<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    node.ancestors().find_map(|n| n.children().find(|c| c.is_element() && c.tag_name().name() == name))
}

/// `$Number$`, `$Number%05d$`, `$RepresentationID$`, `$Bandwidth$`.
fn fill(template: &str, number: Option<u64>, rep_id: &str, bandwidth: &str) -> String {
    let mut out = String::with_capacity(template.len() + 8);
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        match (name, number) {
            ("", _) => out.push('$'),
            ("RepresentationID", _) => out.push_str(rep_id),
            ("Bandwidth", _) => out.push_str(bandwidth),
            ("Number", Some(num)) => out.push_str(&num.to_string()),
            (n, Some(num)) if n.starts_with("Number%0") && n.ends_with('d') => {
                let width: usize = n["Number%0".len()..n.len() - 1].parse().unwrap_or(1);
                out.push_str(&format!("{num:0width$}"));
            }
            _ => out.push_str(&rest[start..start + end + 2]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn join(base: Option<&str>, url: &str) -> String {
    match base {
        Some(base) if !url.starts_with("http://") && !url.starts_with("https://") => {
            if base.ends_with('/') {
                format!("{base}{url}")
            } else {
                match base.rfind('/') {
                    Some(i) => format!("{}/{url}", &base[..i]),
                    None => url.to_string(),
                }
            }
        }
        _ => url.to_string(),
    }
}

/// `PT3M25.5S` → seconds.
fn iso_duration_s(s: &str) -> Option<f64> {
    let s = s.strip_prefix("PT")?;
    let mut total = 0.0;
    let mut num = String::new();
    for c in s.chars() {
        match c {
            'H' => total += num.parse::<f64>().ok()? * 3600.0,
            'M' => total += num.parse::<f64>().ok()? * 60.0,
            'S' => total += num.parse::<f64>().ok()?,
            _ => {
                num.push(c);
                continue;
            }
        }
        num.clear();
    }
    Some(total)
}

pub fn parse_mpd(text: &str) -> Result<Mpd> {
    let doc = roxmltree::Document::parse(text).context("MPD is not XML")?;
    let rep = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "Representation")
        // Several representations: the highest bandwidth.
        .max_by_key(|n| attr(*n, "bandwidth").and_then(|b| b.parse::<u64>().ok()).unwrap_or(0))
        .ok_or_else(|| anyhow!("MPD without a Representation"))?;
    let adaptation = rep.parent().filter(|p| p.tag_name().name() == "AdaptationSet");
    let codecs = attr(rep, "codecs").or_else(|| adaptation.and_then(|a| attr(a, "codecs"))).unwrap_or_default().to_string();
    let sample_rate = attr(rep, "audioSamplingRate")
        .or_else(|| adaptation.and_then(|a| attr(a, "audioSamplingRate")))
        .and_then(|r| r.parse().ok());
    let protected = rep.ancestors().any(|n| n.children().any(|c| c.tag_name().name() == "ContentProtection"));
    let template = inherited(rep, "SegmentTemplate").ok_or_else(|| anyhow!("MPD without a SegmentTemplate"))?;
    let base = inherited(rep, "BaseURL").and_then(|b| b.text()).map(str::trim);
    let rep_id = attr(rep, "id").unwrap_or_default();
    let bandwidth = attr(rep, "bandwidth").unwrap_or_default();
    let timescale: u64 = attr(template, "timescale").and_then(|t| t.parse().ok()).filter(|&t| t > 0).unwrap_or(1);
    let start: u64 = attr(template, "startNumber").and_then(|t| t.parse().ok()).unwrap_or(1);
    let init = attr(template, "initialization").ok_or_else(|| anyhow!("MPD without an initialization segment"))?;
    let media = attr(template, "media").ok_or_else(|| anyhow!("MPD without a media template"))?;

    let mut count = 0u64;
    let mut duration = 0u64;
    if let Some(timeline) = template.children().find(|c| c.tag_name().name() == "SegmentTimeline") {
        for s in timeline.children().filter(|c| c.tag_name().name() == "S") {
            let d: u64 = attr(s, "d").and_then(|d| d.parse().ok()).ok_or_else(|| anyhow!("timeline entry without d"))?;
            let r: i64 = attr(s, "r").and_then(|r| r.parse().ok()).unwrap_or(0);
            ensure!(r >= 0, "open-ended timeline repeats are not supported");
            count += r as u64 + 1;
            duration += d * (r as u64 + 1);
        }
    } else {
        let d: u64 = attr(template, "duration").and_then(|d| d.parse().ok()).ok_or_else(|| anyhow!("no SegmentTimeline and no duration"))?;
        let total_s = doc
            .root_element()
            .attributes()
            .find(|a| a.name() == "mediaPresentationDuration")
            .and_then(|a| iso_duration_s(a.value()))
            .ok_or_else(|| anyhow!("no mediaPresentationDuration"))?;
        duration = (total_s * timescale as f64).round() as u64;
        count = duration.div_ceil(d.max(1));
    }
    ensure!(count > 0 && count <= MAX_SEGMENTS, "MPD with {count} segments");
    let url = |n: Option<u64>| join(base, &fill(media, n, rep_id, bandwidth));
    Ok(Mpd {
        codecs,
        sample_rate,
        protected,
        init_url: join(base, &fill(init, None, rep_id, bandwidth)),
        segment_urls: (start..start + count).map(|n| url(Some(n))).collect(),
        duration,
        timescale,
    })
}

#[cfg(test)]
pub mod testing {
    /// An MPD as TIDAL serves hi-res FLAC.
    pub fn mpd(base: &str, timeline: &str) -> String {
        format!(
            r#"<?xml version='1.0' encoding='UTF-8'?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="urn:mpeg:dash:profile:isoff-main:2011" type="static" minBufferTime="PT3.993S" mediaPresentationDuration="PT0M9.1S">
  <Period id="0">
    <AdaptationSet id="0" contentType="audio" mimeType="audio/mp4" segmentAlignment="true">
      <Representation id="FLAC,96000,24" codecs="flac" bandwidth="2620823" audioSamplingRate="96000">
        <SegmentTemplate timescale="96000" initialization="{base}/0.mp4?token=a&amp;b=1" media="{base}/$Number$.mp4?token=a&amp;b=1" startNumber="1">
          <SegmentTimeline>{timeline}</SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bts_manifests() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let m = parse(BTS, &b64(r#"{"mimeType":"audio/flac","codecs":"flac","encryptionType":"NONE","urls":["https://cdn/x.flac?token=1"]}"#)).unwrap();
        assert_eq!(
            m,
            Manifest::Bts(Bts { mime_type: "audio/flac".into(), codecs: "flac".into(), encryption: "NONE".into(), urls: vec!["https://cdn/x.flac?token=1".into()] })
        );
        let Manifest::Bts(enc) = parse(BTS, &b64(r#"{"mimeType":"audio/mp4","codecs":"mp4a.40.2","encryptionType":"OLD_AES","keyId":"k","urls":["u"]}"#)).unwrap() else { panic!() };
        assert_eq!(enc.encryption, "OLD_AES");
        assert!(parse(BTS, &b64(r#"{"urls":[]}"#)).is_err());
        assert!(parse("video/whatever", &b64("{}")).is_err());
        assert!(parse(BTS, "%%%").is_err());
    }

    #[test]
    fn dash_manifests() {
        let text = testing::mpd("https://cdn/t", r#"<S d="383232" r="1"/><S d="106368"/>"#);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&text);
        let Manifest::Dash(m) = parse(DASH, &b64).unwrap() else { panic!() };
        assert_eq!((m.codecs.as_str(), m.sample_rate, m.protected), ("flac", Some(96000), false));
        assert_eq!(m.init_url, "https://cdn/t/0.mp4?token=a&b=1", "entities decoded");
        assert_eq!(m.segment_urls, ["https://cdn/t/1.mp4?token=a&b=1", "https://cdn/t/2.mp4?token=a&b=1", "https://cdn/t/3.mp4?token=a&b=1"]);
        assert_eq!((m.duration, m.timescale), (383_232 * 2 + 106_368, 96_000));

        let drm = text.replace("<SegmentTemplate", r#"<ContentProtection schemeIdUri="urn:uuid:edef8ba9"/><SegmentTemplate"#);
        assert!(parse_mpd(&drm).unwrap().protected);
        assert!(parse_mpd("<MPD/>").is_err());
        assert!(parse_mpd("not xml").is_err());
    }

    #[test]
    fn templates_without_timeline_and_relative_urls() {
        let text = r#"<MPD mediaPresentationDuration="PT10S"><BaseURL>https://cdn/a/</BaseURL><Period><AdaptationSet codecs="flac">
            <SegmentTemplate timescale="1000" duration="4000" initialization="init-$RepresentationID$.mp4" media="seg-$Number%03d$.mp4"/>
            <Representation id="r1" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        let m = parse_mpd(text).unwrap();
        assert_eq!(m.codecs, "flac");
        assert_eq!(m.init_url, "https://cdn/a/init-r1.mp4");
        assert_eq!(m.segment_urls, ["https://cdn/a/seg-001.mp4", "https://cdn/a/seg-002.mp4", "https://cdn/a/seg-003.mp4"]);
        assert_eq!(iso_duration_s("PT1H2M3.5S"), Some(3723.5));
    }

    #[test]
    fn codecs() {
        assert_eq!(codec_name("flac", "audio/mp4"), "flac");
        assert_eq!(codec_name("mp4a.40.2", "audio/mp4"), "aac");
        assert_eq!(codec_name("", "audio/flac"), "flac");
        assert_eq!(codec_name("eac3", "audio/mp4"), "dolby");
    }
}
