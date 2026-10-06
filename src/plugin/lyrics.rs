//! `lyrics.get {ref}` (capability `lyrics`): `tracks/<id>/lyrics` answers
//!
//! ```json
//! {"trackId": 77646958, "lyricsProvider": "…", "isRightToLeft": false,
//!  "lyrics": "Hey Jude, don't make it bad\n…",
//!  "subtitles": "[00:00.53] Hey Jude, don't make it bad\n[00:06.47] …"}
//! ```
//!
//! `subtitles` (LRC lines) become `synced`, `lyrics` becomes `plain`.
//! Tracks without lyrics answer 404; they are remembered for a while so
//! that the host's retries stay local.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::items::Ref;
use super::rpc::{RpcError, RpcResult};
use crate::api::{status_of, Api};

/// How long a track without lyrics is not asked about again.
const MISSING_TTL: Duration = Duration::from_secs(6 * 3600);
const MAX_MISSING: usize = 2000;

static MISSING: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

fn known_missing(id: &str) -> bool {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    m.retain(|_, at| at.elapsed() < MISSING_TTL);
    m.contains_key(id)
}

fn note_missing(id: &str) {
    let mut m = MISSING.lock().unwrap_or_else(|e| e.into_inner());
    let m = m.get_or_insert_with(HashMap::new);
    if m.len() >= MAX_MISSING {
        m.clear();
    }
    m.insert(id.to_string(), Instant::now());
}

fn not_found(id: &str) -> RpcError {
    RpcError::not_found(format!("no lyrics for track {id}"))
}

/// `[mm:ss.xx]` (or `[mm:ss]`, `[mm:ss.xxx]`) → ms.
fn lrc_time(tag: &str) -> Option<u64> {
    let (m, s) = tag.split_once(':')?;
    let m: u64 = m.trim().parse().ok()?;
    let (s, frac) = s.split_once('.').unwrap_or((s, ""));
    let s: u64 = s.trim().parse().ok()?;
    let frac_ms = match frac.len() {
        0 => 0,
        1 => frac.parse::<u64>().ok()? * 100,
        2 => frac.parse::<u64>().ok()? * 10,
        _ => frac.get(..3)?.parse::<u64>().ok()?,
    };
    (s < 60).then_some(m * 60_000 + s * 1000 + frac_ms)
}

/// Timed lines of an LRC text; lines without a time are skipped.
pub fn parse_lrc(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line.trim();
        let mut times = Vec::new();
        while let Some(after) = rest.strip_prefix('[') {
            let Some(end) = after.find(']') else { break };
            match lrc_time(&after[..end]) {
                Some(t) => times.push(t),
                None => break,
            }
            rest = &after[end + 1..];
        }
        for t in times {
            out.push(json!({"time_ms": t, "text": rest.trim()}));
        }
    }
    out
}

/// The protocol answer, `None` when the document holds no lyrics.
pub fn convert(doc: &Value) -> Option<Value> {
    let mut out = json!({});
    if let Some(sub) = doc.get("subtitles").and_then(Value::as_str) {
        let synced = parse_lrc(sub);
        if !synced.is_empty() {
            out["synced"] = synced.into();
        }
    }
    if let Some(plain) = doc.get("lyrics").and_then(Value::as_str).map(str::trim).filter(|l| !l.is_empty()) {
        out["plain"] = plain.into();
    }
    (out.as_object().is_some_and(|o| !o.is_empty())).then_some(out)
}

pub async fn get(api: &Api, reference: &str) -> RpcResult {
    let Some(Ref::Track(id)) = Ref::parse(reference) else {
        return Err(RpcError::not_found(format!("{reference} has no lyrics")));
    };
    if known_missing(&id) {
        return Err(not_found(&id));
    }
    let doc = match api.get(format!("tracks/{id}/lyrics")).await {
        Ok(v) => v,
        Err(e) if status_of(&e) == Some(404) => {
            note_missing(&id);
            return Err(not_found(&id));
        }
        Err(e) => return Err(e.into()),
    };
    match convert(&doc) {
        Some(lyrics) => Ok(lyrics),
        None => {
            note_missing(&id);
            Err(not_found(&id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitles_and_plain_lyrics() {
        let doc = json!({"trackId": 1, "lyrics": "Hey Jude\nDon't make it bad\n",
                         "subtitles": "[00:00.53] Hey Jude\n[00:06.4]Don't make it bad\n[01:02.123] \nno time here\n[ar:Someone]"});
        assert_eq!(convert(&doc), Some(json!({
            "synced": [
                {"time_ms": 530, "text": "Hey Jude"},
                {"time_ms": 6400, "text": "Don't make it bad"},
                {"time_ms": 62_123, "text": ""}
            ],
            "plain": "Hey Jude\nDon't make it bad"
        })));
        assert_eq!(convert(&json!({"lyrics": "only text", "subtitles": ""})), Some(json!({"plain": "only text"})));
        assert_eq!(convert(&json!({"lyrics": " ", "subtitles": null})), None);
        assert_eq!(parse_lrc("[00:01.00][00:05.00]chorus").len(), 2, "one line, two times");
    }
}
