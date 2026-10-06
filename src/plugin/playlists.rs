//! `playlists.*` (capability `playlist_edit`): edits of the account's own
//! playlists.
//!
//! | Method | Request |
//! |---|---|
//! | `playlists.create {name, description?, public?}` | `PUT v2/my-collection/playlists/folders/create-playlist?name=&description=&folderId=root` (else `POST v1/users/<id>/playlists`), then `PUT v2/playlists/<uuid>/set-public` |
//! | `playlists.rename {ref, name}` | `POST playlists/<uuid>` `title`, `description` |
//! | `playlists.delete {ref}` | `DELETE playlists/<uuid>` |
//! | `playlists.add {ref, items}` | `POST playlists/<uuid>/items` `trackIds`, `onDupes=ADD`, `toIndex` = the end |
//! | `playlists.remove {ref, entries}` | `DELETE playlists/<uuid>/items/<i,j,…>` |
//! | `playlists.move {ref, entry, to}` | `POST playlists/<uuid>/items/<i>` `toIndex` |
//!
//! TIDAL edits a playlist only with its current version: every edit sends
//! the `ETag` of `GET playlists/<uuid>` as `If-None-Match`. A `412`
//! (changed meanwhile) reads it again and retries once.
//!
//! TIDAL removes and moves items by position. Entries are therefore
//! `<index>-<track id>`; before an edit, the plugin checks that each entry's
//! track is still at its index, and answers `not_found` otherwise (the host
//! then reads the playlist again).
//!
//! Every edit of an existing playlist first checks that the account owns
//! it (`creator.id`): a followed playlist is never touched.

use anyhow::Result;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;

use super::items::{self, Ref};
use super::rpc::{RpcError, RpcResult, INTERNAL, METHOD_NOT_FOUND};
use crate::api::{status_of, Api, Req};

const MAX_NAME: usize = 200;
const MAX_DESCRIPTION: usize = 2000;
/// Tracks or entries per request.
const MAX_TRACKS: usize = 500;
/// Longest playlist read when checking entries.
const MAX_SCAN: u32 = 10_000;

#[derive(Deserialize)]
pub struct Params {
    #[serde(rename = "ref")]
    reference: Option<String>,
    name: Option<String>,
    description: Option<String>,
    public: Option<bool>,
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    entries: Vec<String>,
    entry: Option<String>,
    to: Option<u32>,
}

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::invalid_params(msg)
}

fn name(p: &Params) -> Result<String, RpcError> {
    let name = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| invalid("a name is needed"))?;
    Ok(name.chars().take(MAX_NAME).collect())
}

/// `<index>-<track id>` → both.
pub fn parse_entry(s: &str) -> Result<(usize, String), RpcError> {
    let parsed = s.split_once('-').and_then(|(i, t)| {
        let ok = !t.is_empty() && t.len() <= 20 && t.chars().all(|c| c.is_ascii_digit());
        let i = i.parse::<usize>().ok()?;
        ok.then(|| (i, t.to_string()))
    });
    parsed.ok_or_else(|| invalid(format!("{s:?} is not a playlist entry")))
}

/// A playlist of the account: its uuid, its JSON and its ETag.
struct Owned {
    uuid: String,
    playlist: Value,
    etag: Option<String>,
}

async fn owned(api: &Api, p: &Params) -> Result<Owned, RpcError> {
    let reference = p.reference.as_deref().ok_or_else(|| invalid("ref is needed"))?;
    let Some(Ref::Playlist(uuid)) = Ref::parse(reference) else {
        return Err(invalid(format!("{reference} is not a playlist")));
    };
    let (playlist, etag) = api.get_etag(format!("playlists/{uuid}")).await?;
    let creator = playlist.pointer("/creator/id").and_then(Value::as_u64);
    let user = api.session().await?.user_id;
    if creator != Some(user) {
        return Err(invalid(format!("{reference} belongs to another account")));
    }
    Ok(Owned { uuid, playlist, etag })
}

/// Send an edit with the playlist's ETag; on `412`, read it again and
/// retry once.
async fn with_etag(api: &Api, o: &Owned, req: impl Fn(Option<String>) -> Req) -> Result<Value> {
    match api.call(req(o.etag.clone())).await {
        Err(e) if status_of(&e) == Some(412) => {
            tracing::info!("playlist changed meanwhile, retrying with its new version");
            let (_, etag) = api.get_etag(format!("playlists/{}", o.uuid)).await?;
            api.call(req(etag)).await
        }
        other => other,
    }
}

/// Track ids of the playlist, in order (videos included, as TIDAL counts
/// them).
async fn item_ids(api: &Api, uuid: &str) -> Result<Vec<String>> {
    let v = api.page(Req::get(format!("playlists/{uuid}/items")), 0, MAX_SCAN).await?;
    Ok(v.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| items::unwrap(e).get("id").map(|id| id.to_string().trim_matches('"').to_string()).unwrap_or_default())
        .collect())
}

/// Indices of `entries`, once checked against the playlist as it is now.
fn check_entries(ids: &[String], entries: &[(usize, String)]) -> Result<Vec<usize>, RpcError> {
    entries
        .iter()
        .map(|(i, track)| match ids.get(*i) {
            Some(id) if id == track => Ok(*i),
            _ => Err(RpcError::not_found(format!("entry {i}-{track} is no longer there; the playlist changed"))),
        })
        .collect()
}

async fn create(api: &Api, p: &Params) -> RpcResult {
    let title = name(p)?;
    let description: String =
        p.description.as_deref().map(str::trim).unwrap_or_default().chars().take(MAX_DESCRIPTION).collect();
    let v2 = Req::new(Method::PUT, "my-collection/playlists/folders/create-playlist")
        .v2()
        .q("name", &title)
        .q("description", &description)
        .q("folderId", "root");
    let playlist = match api.call(v2).await {
        Ok(v) => v.get("data").cloned().unwrap_or(v),
        Err(e) if matches!(status_of(&e), Some(404 | 405)) => {
            tracing::info!("v2 playlist creation unavailable ({e:#}), using v1");
            let user = api.session().await?.user_id;
            let req = Req::new(Method::POST, format!("users/{user}/playlists")).form(&[("title", title), ("description", description)]);
            api.call(req).await?
        }
        Err(e) => return Err(e.into()),
    };
    let mut item = items::playlist(&playlist).ok_or_else(|| RpcError::new(INTERNAL, "playlist creation: unreadable answer"))?;
    if p.public == Some(true) {
        let uuid = item.reference.trim_start_matches("playlist/").to_string();
        if let Err(e) = api.call(Req::new(Method::PUT, format!("playlists/{uuid}/set-public")).v2()).await {
            tracing::warn!("playlist created, but not made public: {e:#}");
        }
    }
    // Just created by this account.
    item.editable = Some(true);
    Ok(item.to_json())
}

pub async fn edit(api: &Api, method: &str, p: Params) -> RpcResult {
    match method {
        "playlists.create" => create(api, &p).await,
        "playlists.rename" => {
            let title = name(&p)?;
            let o = owned(api, &p).await?;
            let description = o.playlist.get("description").and_then(Value::as_str).unwrap_or_default().to_string();
            with_etag(api, &o, |etag| {
                Req::new(Method::POST, format!("playlists/{}", o.uuid))
                    .form(&[("title", title.clone()), ("description", description.clone())])
                    .etag(etag)
            })
            .await?;
            Ok(Value::Null)
        }
        "playlists.delete" => {
            let o = owned(api, &p).await?;
            with_etag(api, &o, |etag| Req::new(Method::DELETE, format!("playlists/{}", o.uuid)).etag(etag)).await?;
            Ok(Value::Null)
        }
        "playlists.add" => {
            let tracks: Vec<String> = p
                .items
                .iter()
                .map(|r| match Ref::parse(r) {
                    Some(Ref::Track(id)) => Ok(id),
                    _ => Err(invalid(format!("{r} is not a track"))),
                })
                .collect::<Result<_, _>>()?;
            if tracks.is_empty() || tracks.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} tracks at a time")));
            }
            let o = owned(api, &p).await?;
            let end = o.playlist.get("numberOfTracks").and_then(Value::as_u64).unwrap_or(0)
                + o.playlist.get("numberOfVideos").and_then(Value::as_u64).unwrap_or(0);
            let fields = [
                ("trackIds", tracks.join(",")),
                ("onDupes", "ADD".to_string()),
                ("onArtifactNotFound", "SKIP".to_string()),
                ("toIndex", end.to_string()),
            ];
            with_etag(api, &o, |etag| Req::new(Method::POST, format!("playlists/{}/items", o.uuid)).form(&fields).etag(etag)).await?;
            Ok(Value::Null)
        }
        "playlists.remove" => {
            let entries: Vec<(usize, String)> = p.entries.iter().map(|e| parse_entry(e)).collect::<Result<_, _>>()?;
            if entries.is_empty() || entries.len() > MAX_TRACKS {
                return Err(invalid(format!("1 to {MAX_TRACKS} entries at a time")));
            }
            let o = owned(api, &p).await?;
            let mut indices = check_entries(&item_ids(api, &o.uuid).await?, &entries)?;
            indices.sort_unstable();
            indices.dedup();
            let list = indices.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
            with_etag(api, &o, |etag| Req::new(Method::DELETE, format!("playlists/{}/items/{list}", o.uuid)).etag(etag)).await?;
            Ok(Value::Null)
        }
        "playlists.move" => {
            let entry = parse_entry(p.entry.as_deref().ok_or_else(|| invalid("entry is needed"))?)?;
            let to = p.to.ok_or_else(|| invalid("to is needed"))? as usize;
            let o = owned(api, &p).await?;
            let ids = item_ids(api, &o.uuid).await?;
            let from = check_entries(&ids, std::slice::from_ref(&entry))?[0];
            // Past the end: last.
            let to = to.min(ids.len().saturating_sub(1));
            if from == to {
                return Ok(Value::Null);
            }
            with_etag(api, &o, |etag| {
                Req::new(Method::POST, format!("playlists/{}/items/{from}", o.uuid)).form(&[("toIndex", to.to_string())]).etag(etag)
            })
            .await?;
            Ok(Value::Null)
        }
        _ => Err(RpcError::new(METHOD_NOT_FOUND, format!("unknown method {method}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_name_a_position_and_a_track() {
        assert_eq!(parse_entry("12-77646958").unwrap(), (12, "77646958".into()));
        for bad in ["", "12", "-1", "a-1", "1-", "1-x", "1-2-3"] {
            assert!(parse_entry(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn entries_are_checked_against_the_playlist() {
        let ids: Vec<String> = ["10", "20", "10"].map(String::from).to_vec();
        assert_eq!(check_entries(&ids, &[(2, "10".into()), (1, "20".into())]).unwrap(), [2, 1]);
        assert!(check_entries(&ids, &[(1, "10".into())]).is_err(), "another track there now");
        assert!(check_entries(&ids, &[(5, "10".into())]).is_err(), "past the end");
    }

    #[test]
    fn names_are_trimmed_and_required() {
        let p = |n: &str| Params {
            reference: None,
            name: Some(n.into()),
            description: None,
            public: None,
            items: vec![],
            entries: vec![],
            entry: None,
            to: None,
        };
        assert_eq!(name(&p("  Soir  ")).unwrap(), "Soir");
        assert!(name(&p("   ")).is_err());
    }
}
