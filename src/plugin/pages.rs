//! TIDAL's editorial pages, cut into shelves:
//!
//! - **Home**: `home/feed/static` of the v2 API (`{items: [{title, items:
//!   [{type, data}]}]}`), else the older v1 `pages/home`;
//! - **Explore** and the pages it links to (genres, moods…):
//!   `v1/pages/<name>` (`{rows: [{modules: [{type, title, pagedList:
//!   {items, dataApiPath}}]}]}`).
//!
//! Each shelf is a folder (`shelf/<n>/<page>`). A v1 shelf with more items
//! than it shows is read further from its `dataApiPath`; a shelf of links
//! (`PAGE_LINKS`) lists the linked pages (`page/<path>`). Pages are kept 10
//! minutes. TIDAL changes these pages often: a page that cannot be read
//! gives no shelves rather than an error.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use super::items::{self, tr, Ref};
use crate::api::{Api, Req};

const MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// Shelves kept per page.
const MAX_SHELVES: usize = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<Value>,
    /// v1 `dataApiPath`, to read the whole list.
    pub more: Option<String>,
    /// Items are `{title, apiPath}` links to other pages.
    pub links: bool,
    /// Kind of every item (`ALBUM`…), from the v1 module type.
    pub kind: Option<&'static str>,
}

type Cache = HashMap<String, (Instant, Vec<Shelf>)>;
static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Forget the pages (another account, another language).
pub fn clear() {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn link(v: &Value) -> Option<(String, String)> {
    let path = v.get("apiPath").and_then(Value::as_str).filter(|p| items::valid_page(p))?;
    let title = v.get("title").and_then(Value::as_str).unwrap_or("?");
    Some((title.to_string(), path.to_string()))
}

fn useful(s: &Shelf) -> bool {
    if s.links {
        s.items.iter().any(|v| link(v).is_some())
    } else {
        s.items.iter().any(|v| items::typed(s.kind, v).is_some())
    }
}

/// Shelves of a v2 feed.
pub fn parse_v2(v: &Value) -> Vec<Shelf> {
    let modules = v.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    modules
        .iter()
        .map(|m| Shelf {
            title: m.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
            items: m.get("items").and_then(Value::as_array).cloned().unwrap_or_default(),
            more: None,
            links: false,
            kind: None,
        })
        .filter(useful)
        .take(MAX_SHELVES)
        .collect()
}

/// Shelves of a v1 page.
pub fn parse_v1(v: &Value) -> Vec<Shelf> {
    let rows = v.get("rows").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    rows.iter()
        .flat_map(|r| r.get("modules").and_then(Value::as_array).cloned().unwrap_or_default())
        .map(|m| {
            let kind = m.get("type").and_then(Value::as_str).unwrap_or_default();
            let list = m.get("pagedList");
            let items = list.and_then(|l| l.get("items")).and_then(Value::as_array).cloned().unwrap_or_default();
            let total = list.and_then(|l| l.get("totalNumberOfItems")).and_then(Value::as_u64).unwrap_or(0);
            let more = list
                .and_then(|l| l.get("dataApiPath"))
                .and_then(Value::as_str)
                .filter(|p| items::valid_page(p) && total > items.len() as u64)
                .map(str::to_string);
            Shelf {
                title: m.get("title").and_then(Value::as_str).unwrap_or_default().to_string(),
                items,
                more,
                links: kind.starts_with("PAGE_LINKS"),
                kind: match kind {
                    "TRACK_LIST" => Some("TRACK"),
                    "ALBUM_LIST" => Some("ALBUM"),
                    "ARTIST_LIST" => Some("ARTIST"),
                    "PLAYLIST_LIST" => Some("PLAYLIST"),
                    "MIX_LIST" => Some("MIX"),
                    _ => None,
                },
            }
        })
        .filter(useful)
        .take(MAX_SHELVES)
        .collect()
}

async fn fetch(api: &Api, page: &str) -> Result<Vec<Shelf>> {
    if page == "home" {
        match api.call(Req::get("home/feed/static").v2()).await {
            Ok(v) if !parse_v2(&v).is_empty() => return Ok(parse_v2(&v)),
            Ok(_) => tracing::info!("v2 home feed: no shelves, trying v1"),
            Err(e) => tracing::info!("v2 home feed unreadable ({e:#}), trying v1"),
        }
        return Ok(parse_v1(&api.get("pages/home").await?));
    }
    Ok(parse_v1(&api.get(page).await?))
}

/// Shelves of `page` (`home` or a v1 api path), cached.
pub async fn shelves(api: &Api, page: &str) -> Result<Vec<Shelf>> {
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, shelves)) = cache.as_ref().and_then(|c| c.get(page)) {
            if at.elapsed() < MAX_AGE {
                return Ok(shelves.clone());
            }
        }
    }
    let shelves = fetch(api, page).await?;
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(page.to_string(), (Instant::now(), shelves.clone()));
    Ok(shelves)
}

fn shelf_title(s: &Shelf) -> String {
    if s.title.trim().is_empty() {
        tr("Selection", "Sélection")
    } else {
        s.title.trim().to_string()
    }
}

/// The page's shelves as folders.
pub fn folders(page: &str, shelves: &[Shelf]) -> Vec<Value> {
    shelves
        .iter()
        .enumerate()
        .map(|(i, s)| items::folder(&Ref::Shelf(page.to_string(), i), &shelf_title(s)).to_json())
        .collect()
}

/// Shelf `n` of `page`, one page of it.
pub async fn shelf(api: &Api, page: &str, n: usize, offset: u32, limit: u32) -> Result<Value> {
    let shelves = shelves(api, page).await?;
    let Some(s) = shelves.get(n) else {
        return Ok(json!({"items": [], "total": 0, "has_more": false}));
    };
    if s.links {
        let links: Vec<items::Item> =
            s.items.iter().filter_map(link).map(|(title, path)| items::folder(&Ref::Page(path), &title)).collect();
        return Ok(items::slice_items(links, offset, limit));
    }
    if let Some(path) = &s.more {
        match api.page(Req::get(path.clone()), offset, limit).await {
            Ok(v) => return Ok(items::page(&v, offset, |i| items::typed(s.kind, i))),
            Err(e) => tracing::info!("shelf {page}/{n}: whole list unreadable ({e:#})"),
        }
    }
    Ok(items::slice(&s.items, offset, limit, |i| items::typed(s.kind, i)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_feed_shelves() {
        let feed = json!({"items": [
            {"type": "HORIZONTAL_LIST", "title": "New albums", "moduleId": "m1", "items": [
                {"type": "ALBUM", "data": {"id": 5, "title": "A", "cover": "aa"}}]},
            {"type": "SHORTCUT_LIST", "title": "", "items": [{"type": "MIX", "data": {"id": "mx", "titleTextInfo": {"text": "Mix"}}}]},
            {"type": "HORIZONTAL_LIST", "title": "Videos", "items": [{"type": "VIDEO", "data": {"id": 1}}]},
            {"type": "TRACK_LIST", "title": "Empty", "items": []}
        ]});
        let shelves = parse_v2(&feed);
        assert_eq!(shelves.len(), 2, "shelves with nothing playable or browsable are dropped");
        assert_eq!(shelves[0].title, "New albums");
        let f = folders("home", &shelves);
        assert_eq!(f[0]["ref"], "shelf/0/home");
        assert_eq!(f[1]["title"], "Selection");
    }

    #[test]
    fn v1_page_shelves() {
        let page = json!({"rows": [
            {"modules": [{"type": "ALBUM_LIST", "title": "New", "pagedList": {"totalNumberOfItems": 50, "dataApiPath": "pages/data/abc-1",
                "items": [{"id": 5, "title": "A"}]}}]},
            {"modules": [{"type": "PAGE_LINKS_CLOUD", "title": "Genres", "pagedList": {"items": [
                {"title": "Jazz", "apiPath": "pages/genre_jazz"}, {"title": "Bad", "apiPath": "https://elsewhere"}]}}]},
            {"modules": [{"type": "TEXT_BLOCK", "title": "Hi", "text": "x"}]}
        ]});
        let shelves = parse_v1(&page);
        assert_eq!(shelves.len(), 2);
        assert_eq!(shelves[0].more.as_deref(), Some("pages/data/abc-1"));
        assert!(shelves[1].links);
        assert_eq!(link(&shelves[1].items[0]), Some(("Jazz".into(), "pages/genre_jazz".into())));
        assert_eq!(link(&shelves[1].items[1]), None);
    }
}
