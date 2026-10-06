//! `item.details {ref}` (capability `details`): what a page shows beyond the
//! item's children, as `{biography?, related?, facts?}`:
//! - artist: biography (`artists/<id>/bio`); top tracks, similar artists;
//! - album: review (`albums/<id>/review`); release date, discs, quality,
//!   copyright, credits gathered over its tracks
//!   (`albums/<id>/items/credits`); similar albums;
//! - track: credits (`tracks/<id>/credits`), one fact per role.
//!
//! Everything optional may be missing (404): what is there is shown.
//! Texts are plain: TIDAL's `[wimpLink …]…[/wimpLink]` and HTML markup are
//! stripped here.

use anyhow::Result;
use serde_json::{json, Value};

use super::items::{self, tr, Ref};
use super::rpc::{RpcError, RpcResult};
use crate::api::{status_of, Api, Req};

/// Items per related shelf.
const SHELF_ITEMS: usize = 20;
const MAX_FACTS: usize = 30;
const MAX_FACT_VALUE: usize = 500;

/// `[wimpLink artistId="1"]Name[/wimpLink]` → `Name`.
fn strip_links(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let tag = after.trim_start_matches('/');
        match after.find(']') {
            Some(end) if tag.starts_with("wimpLink") => rest = &after[end + 1..],
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Plain text from HTML: tags dropped, `<br>` and paragraph ends as
/// line breaks, common entities decoded, blank lines collapsed.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        let name = tag.trim_start_matches('/').split([' ', '/']).next().unwrap_or("");
        if matches!(name, "br" | "p" | "div" | "li") && (name == "br" || tag.starts_with('/')) {
            out.push('\n');
            if name == "p" {
                out.push('\n');
            }
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let decoded = decode_entities(&out);
    let mut text = String::new();
    let mut blank = 0;
    for line in decoded.lines().map(str::trim) {
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !text.is_empty() {
            text.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        text.push_str(line);
    }
    text
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';').filter(|&i| i <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let entity = &after[1..semi];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "rsquo" | "lsquo" => Some('’'),
            "hellip" => Some('…'),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

fn text_of(v: Option<&Value>) -> Option<String> {
    let t = plain_text(&strip_links(v?.as_str()?));
    (!t.is_empty()).then_some(t)
}

fn shelf(title: String, items: Vec<Value>) -> Option<Value> {
    (!items.is_empty()).then(|| json!({"title": title, "items": items}))
}

fn mapped(list: &Value, map: impl Fn(&Value) -> Option<items::Item>) -> Vec<Value> {
    let list = list.get("items").unwrap_or(list);
    list.as_array().into_iter().flatten().filter_map(|v| map(items::unwrap(v))).take(SHELF_ITEMS).map(|i| i.to_json()).collect()
}

fn fact(label: String, value: impl Into<String>) -> Option<Value> {
    let value: String = value.into().trim().chars().take(MAX_FACT_VALUE).collect();
    let label: String = label.trim().chars().take(80).collect();
    (!value.is_empty() && !label.is_empty()).then(|| json!({"label": label, "value": value}))
}

/// `2026-09-25` as the host's language writes dates.
fn date(iso: &str) -> String {
    match (iso.get(..4), iso.get(5..7), iso.get(8..10), tr("en", "fr").as_str()) {
        (Some(y), Some(m), Some(d), "fr") => format!("{d}/{m}/{y}"),
        _ => iso.get(..10).unwrap_or(iso).to_string(),
    }
}

/// `[{type, contributors: [{name}]}]` → one fact per role, names merged
/// across lists (an album's tracks), in order of appearance.
pub fn credit_facts<'a>(lists: impl IntoIterator<Item = &'a Value>) -> Vec<Value> {
    let mut roles: Vec<(String, Vec<String>)> = Vec::new();
    for list in lists {
        for credit in list.as_array().into_iter().flatten() {
            let Some(role) = credit.get("type").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()) else { continue };
            let names = credit.get("contributors").and_then(Value::as_array).into_iter().flatten();
            let names = names.filter_map(|c| c.get("name").and_then(Value::as_str)).map(str::trim).filter(|n| !n.is_empty());
            let at = match roles.iter().position(|(r, _)| r == role) {
                Some(at) => at,
                None => {
                    roles.push((role.to_string(), Vec::new()));
                    roles.len() - 1
                }
            };
            for name in names {
                if !roles[at].1.iter().any(|n| n == name) {
                    roles[at].1.push(name.to_string());
                }
            }
        }
    }
    roles.into_iter().filter_map(|(role, names)| fact(role, names.join(", "))).collect()
}

/// `path`, or `None` when TIDAL has nothing there (404).
async fn optional(api: &Api, req: Req) -> Result<Option<Value>> {
    match api.call(req).await {
        Ok(v) => Ok(Some(v)),
        Err(e) if status_of(&e) == Some(404) => Ok(None),
        Err(e) => Err(e),
    }
}

fn quality_label(album: &Value) -> Option<String> {
    let f = items::format_of(album)?;
    Some(match (f.codec, f.bits) {
        ("flac", Some(24)) => tr("Hi-res (24-bit FLAC)", "Haute résolution (FLAC 24 bits)"),
        ("flac", _) => tr("CD quality (16-bit FLAC)", "Qualité CD (FLAC 16 bits)"),
        _ => "AAC".into(),
    })
}

pub async fn get(api: &Api, reference: &str) -> RpcResult {
    let r = Ref::parse(reference).ok_or_else(|| RpcError::not_found(format!("unknown ref {reference:?}")))?;
    let mut related: Vec<Value> = Vec::new();
    let mut facts: Vec<Value> = Vec::new();
    let mut biography = None;
    match &r {
        Ref::Artist(id) => {
            let (bio, top, similar) = tokio::join!(
                optional(api, Req::get(format!("artists/{id}/bio"))),
                optional(api, Req::get(format!("artists/{id}/toptracks")).q("limit", SHELF_ITEMS)),
                optional(api, Req::get(format!("artists/{id}/similar")).q("limit", SHELF_ITEMS)),
            );
            if let Some(bio) = bio? {
                biography = text_of(bio.get("text")).map(|t| (t, bio.get("source").and_then(Value::as_str).map(str::to_string)));
            }
            if let Some(top) = top? {
                related.extend(shelf(tr("Top tracks", "Titres phares"), mapped(&top, |t| items::track(t, None))));
            }
            // Similar artists are a nicety: an error leaves them out.
            if let Ok(Some(similar)) = similar {
                related.extend(shelf(tr("Similar artists", "Artistes similaires"), mapped(&similar, items::artist)));
            }
        }
        Ref::Album(id) => {
            let (album, review, credits, similar) = tokio::join!(
                api.get(format!("albums/{id}")),
                optional(api, Req::get(format!("albums/{id}/review"))),
                optional(api, Req::get(format!("albums/{id}/items/credits")).q("replace", true).q("includeContributors", true).q("limit", 100)),
                optional(api, Req::get(format!("albums/{id}/similar")).q("limit", SHELF_ITEMS)),
            );
            let album = album?;
            if let Ok(Some(review)) = review {
                biography = text_of(review.get("text")).map(|t| (t, review.get("source").and_then(Value::as_str).map(str::to_string)));
            }
            let s = |key: &str| album.get(key).and_then(Value::as_str).map(str::to_string);
            facts.extend(s("releaseDate").and_then(|d| fact(tr("Released", "Sortie"), date(&d))));
            if let Some(n) = album.get("numberOfVolumes").and_then(Value::as_u64).filter(|&n| n > 1) {
                facts.extend(fact(tr("Discs", "Disques"), n.to_string()));
            }
            facts.extend(quality_label(&album).and_then(|q| fact(tr("Quality", "Qualité"), q)));
            facts.extend(s("copyright").and_then(|c| fact("©".into(), c)));
            if let Ok(Some(credits)) = credits {
                let lists = credits.get("items").and_then(Value::as_array).into_iter().flatten().filter_map(|i| i.get("credits"));
                facts.extend(credit_facts(lists));
            }
            if let Ok(Some(similar)) = similar {
                related.extend(shelf(tr("Similar albums", "Albums similaires"), mapped(&similar, items::album)));
            }
        }
        Ref::Track(id) => {
            if let Some(credits) = optional(api, Req::get(format!("tracks/{id}/credits")).q("includeContributors", true)).await? {
                facts.extend(credit_facts([&credits]));
            }
        }
        _ => {}
    }
    facts.truncate(MAX_FACTS);
    let mut out = json!({});
    if let Some((text, source)) = biography {
        out["biography"] = json!({"text": text, "source": source.unwrap_or_else(|| "TIDAL".into())});
    }
    if !related.is_empty() {
        out["related"] = related.into();
    }
    if !facts.is_empty() {
        out["facts"] = facts.into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_becomes_plain_text() {
        assert_eq!(
            text_of(Some(&json!("Formed by [wimpLink artistId=\"1\"]John[/wimpLink] &amp; co.<br/>In 1960."))).as_deref(),
            Some("Formed by John & co.\nIn 1960.")
        );
        assert_eq!(strip_links("[1] a [b]"), "[1] a [b]", "other brackets stay");
        assert_eq!(plain_text("<p>a&nbsp;b</p><p>c</p>"), "a b\n\nc");
    }

    #[test]
    fn credits_become_facts() {
        let a = json!([{"type": "Composer", "contributors": [{"name": "Paul McCartney"}, {"name": "John Lennon"}]},
                       {"type": "Producer", "contributors": [{"name": "George Martin"}]}]);
        let b = json!([{"type": "Composer", "contributors": [{"name": "John Lennon"}, {"name": "George Harrison"}]},
                       {"type": "", "contributors": [{"name": "x"}]}]);
        assert_eq!(credit_facts([&a, &b]), [
            json!({"label": "Composer", "value": "Paul McCartney, John Lennon, George Harrison"}),
            json!({"label": "Producer", "value": "George Martin"}),
        ]);
    }
}
