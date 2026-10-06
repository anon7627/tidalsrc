//! `browse.root`, `browse.list`, `library.*`, `search`, `item.get`,
//! `favorites.set`, `radio.next`.

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};

use super::items::{self, tr, RadioSeed, Ref};
use super::pages;
use super::rpc::{RpcError, RpcResult, METHOD_NOT_FOUND};
use crate::api::{status_of, Api, Req};

/// Protocol maximum page size.
const MAX_PAGE: u32 = 200;
const DEFAULT_PAGE: u32 = 50;
/// Tracks asked of a radio.
const RADIO_TRACKS: u32 = 100;
/// The account's mixes page.
const MY_MIXES: &str = "pages/my_collection_my_mixes";

fn clamp(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

fn parse_ref(s: &str) -> Result<Ref, RpcError> {
    Ref::parse(s).ok_or_else(|| RpcError::not_found(format!("unknown ref {s:?}")))
}

pub fn section_title(r: &Ref) -> String {
    match r {
        Ref::Home => tr("Home", "Accueil"),
        Ref::Explore => tr("Explore", "Explorer"),
        Ref::Mixes => tr("My mixes", "Mes mix"),
        Ref::Favorites => tr("Favourites", "Favoris"),
        Ref::FavTracks => tr("Tracks", "Titres"),
        Ref::FavAlbums => tr("Albums", "Albums"),
        Ref::FavArtists => tr("Artists", "Artistes"),
        Ref::FavPlaylists => tr("Playlists", "Playlists"),
        Ref::MyPlaylists => tr("My playlists", "Mes playlists"),
        Ref::Page(_) => tr("Explore", "Explorer"),
        _ => "TIDAL".into(),
    }
}

fn folder(r: Ref) -> Value {
    items::folder(&r, &section_title(&r)).to_json()
}

async fn uid(api: &Api) -> Result<u64> {
    Ok(api.session().await?.user_id)
}

fn is_status(e: &anyhow::Error, status: u16) -> bool {
    status_of(e) == Some(status)
}

/// `sections` for hosts that show the plugin in their sidebar; `home`, for
/// hosts that merge the `library` lists instead: the account's mixes, then
/// the shelves of TIDAL's Home page.
pub async fn root(api: &Api) -> RpcResult {
    let sections = [Ref::Home, Ref::Explore, Ref::Mixes, Ref::Favorites, Ref::MyPlaylists].map(folder);
    let mut home = vec![folder(Ref::Mixes)];
    if api.signed_in().await {
        match pages::shelves(api, "home").await {
            Ok(shelves) => home.extend(pages::folders("home", &shelves)),
            Err(e) => tracing::info!("home shelves unreadable: {e:#}"),
        }
    }
    Ok(json!({"sections": sections, "home": home}))
}

#[derive(Deserialize)]
pub struct PageParams {
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

fn favorites_req(user: u64, kind: &str) -> Req {
    Req::get(format!("users/{user}/favorites/{kind}")).q("order", "DATE").q("orderDirection", "DESC")
}

/// `library.albums`, `.artists`, `.tracks`, `.playlists`: the account's
/// favourites, and its playlists with those it follows.
pub async fn library(api: &Api, method: &str, p: PageParams) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let user = uid(api).await?;
    let page = match method {
        "library.albums" => items::page(&api.page(favorites_req(user, "albums"), offset, limit).await?, offset, items::album),
        "library.artists" => items::page(&api.page(favorites_req(user, "artists"), offset, limit).await?, offset, items::artist),
        "library.tracks" => {
            items::page(&api.page(favorites_req(user, "tracks"), offset, limit).await?, offset, |t| items::track(t, None))
        }
        "library.playlists" => {
            let v = api.page(Req::get(format!("users/{user}/playlistsAndFavoritePlaylists")), offset, limit).await?;
            items::page(&v, offset, |e| items::playlist(e.get("playlist").unwrap_or(e)))
        }
        _ => return Err(RpcError::new(METHOD_NOT_FOUND, format!("unknown method {method}"))),
    };
    Ok(page)
}

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// A playlist's tracks, each with its entry (`<index>-<track id>`).
pub fn playlist_page(v: &Value, offset: u32) -> Value {
    let raw = v.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let items: Vec<Value> = raw
        .iter()
        .enumerate()
        .filter(|(_, e)| e.get("type").and_then(Value::as_str).is_none_or(|t| t.eq_ignore_ascii_case("track")))
        .filter_map(|(i, e)| {
            let mut t = items::track(items::unwrap(e), None)?;
            let id = t.reference.strip_prefix("track/")?.to_string();
            t.entry_id = Some(format!("{}-{id}", offset as usize + i));
            Some(t.to_json())
        })
        .collect();
    let total = v.get("totalNumberOfItems").and_then(Value::as_u64);
    let has_more = !raw.is_empty() && total.is_some_and(|t| u64::from(offset) + (raw.len() as u64) < t);
    let mut out = json!({"items": items, "has_more": has_more});
    if let Some(t) = total {
        out["total"] = t.into();
    }
    out
}

/// An artist's albums, then their EPs and singles, as one list.
async fn artist_albums(api: &Api, id: &str, offset: u32, limit: u32) -> Result<Value> {
    let albums = api.page(Req::get(format!("artists/{id}/albums")), offset, limit).await?;
    let a_total = albums.get("totalNumberOfItems").and_then(Value::as_u64).unwrap_or(0);
    let mut raw: Vec<Value> = albums.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut e_total = None;
    if (raw.len() as u32) < limit {
        let from = (u64::from(offset)).saturating_sub(a_total) as u32;
        let eps = api.page(Req::get(format!("artists/{id}/albums")).q("filter", "EPSANDSINGLES"), from, limit - raw.len() as u32).await?;
        e_total = eps.get("totalNumberOfItems").and_then(Value::as_u64);
        raw.extend(eps.get("items").and_then(Value::as_array).cloned().unwrap_or_default());
    }
    let items: Vec<Value> = raw.iter().filter_map(items::album).map(|i| i.to_json()).collect();
    let seen = u64::from(offset) + raw.len() as u64;
    Ok(match e_total {
        Some(e) => json!({"items": items, "total": a_total + e, "has_more": !raw.is_empty() && seen < a_total + e}),
        None => json!({"items": items, "has_more": true}),
    })
}

/// Tracks of a radio. TIDAL's `…/radio` lists may give way to mixes: on a
/// 404, the item's own mix (`TRACK_MIX`, `ARTIST_MIX`) is read instead.
pub async fn radio_tracks(api: &Api, seed: RadioSeed, id: &str) -> Result<Vec<Value>> {
    let (path, mix_key) = match seed {
        RadioSeed::Track => (format!("tracks/{id}"), "TRACK_MIX"),
        RadioSeed::Artist => (format!("artists/{id}"), "ARTIST_MIX"),
        RadioSeed::Album => {
            let first = api.call(Req::get(format!("albums/{id}/tracks")).q("limit", 1)).await?;
            let track = first.pointer("/items/0/id").and_then(Value::as_u64).ok_or_else(|| anyhow::anyhow!("album {id} has no tracks"))?;
            return Box::pin(radio_tracks(api, RadioSeed::Track, &track.to_string())).await;
        }
    };
    match api.call(Req::get(format!("{path}/radio")).q("limit", RADIO_TRACKS)).await {
        Ok(v) => Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default()),
        Err(e) if is_status(&e, 404) => {
            let item = api.get(path).await?;
            let mix = item.pointer(&format!("/mixes/{mix_key}")).and_then(Value::as_str).ok_or(e)?.to_string();
            let v = api.page(Req::get(format!("mixes/{mix}/items")), 0, RADIO_TRACKS).await?;
            Ok(v.get("items").and_then(Value::as_array).cloned().unwrap_or_default())
        }
        Err(e) => Err(e),
    }
}

/// Every mix of the account's mixes page.
async fn my_mixes(api: &Api) -> Result<Vec<Value>> {
    let shelves = pages::shelves(api, MY_MIXES).await?;
    let mixes = shelves.into_iter().filter(|s| !s.links && matches!(s.kind, None | Some("MIX")));
    Ok(mixes.flat_map(|s| s.items).collect())
}

pub async fn list(api: &Api, p: ListParams) -> RpcResult {
    let (offset, limit) = (p.offset, clamp(p.limit));
    let page = match parse_ref(&p.reference)? {
        Ref::Favorites => {
            let children = [Ref::FavTracks, Ref::FavAlbums, Ref::FavArtists, Ref::FavPlaylists];
            let items: Vec<Value> = children.into_iter().map(folder).collect();
            json!({"items": items, "total": 4, "has_more": false})
        }
        Ref::FavTracks => {
            let v = api.page(favorites_req(uid(api).await?, "tracks"), offset, limit).await?;
            items::page(&v, offset, |t| items::track(t, None))
        }
        Ref::FavAlbums => items::page(&api.page(favorites_req(uid(api).await?, "albums"), offset, limit).await?, offset, items::album),
        Ref::FavArtists => items::page(&api.page(favorites_req(uid(api).await?, "artists"), offset, limit).await?, offset, items::artist),
        Ref::FavPlaylists => {
            items::page(&api.page(favorites_req(uid(api).await?, "playlists"), offset, limit).await?, offset, items::playlist)
        }
        Ref::MyPlaylists => {
            let v = api.page(Req::get(format!("users/{}/playlists", uid(api).await?)), offset, limit).await?;
            items::page(&v, offset, items::playlist)
        }
        Ref::Mixes => items::slice(&my_mixes(api).await?, offset, limit, items::mix),
        Ref::Mix(id) => items::page(&api.page(Req::get(format!("mixes/{id}/items")), offset, limit).await?, offset, items::any),
        Ref::Home => {
            let shelves = pages::shelves(api, "home").await?;
            let f = pages::folders("home", &shelves);
            json!({"total": f.len(), "items": f, "has_more": false})
        }
        Ref::Explore => {
            let shelves = pages::shelves(api, "pages/explore").await?;
            let f = pages::folders("pages/explore", &shelves);
            json!({"total": f.len(), "items": f, "has_more": false})
        }
        Ref::Page(path) => {
            let shelves = pages::shelves(api, &path).await?;
            let f = pages::folders(&path, &shelves);
            json!({"total": f.len(), "items": f, "has_more": false})
        }
        Ref::Shelf(page, n) => pages::shelf(api, &page, n, offset, limit).await?,
        Ref::Album(id) => {
            let (album, tracks) = tokio::join!(api.get(format!("albums/{id}")), api.page(Req::get(format!("albums/{id}/tracks")), offset, limit));
            let (album, tracks) = (album?, tracks?);
            items::page(&tracks, offset, |t| items::track(t, Some(&album)))
        }
        Ref::Artist(id) => artist_albums(api, &id, offset, limit).await?,
        Ref::Playlist(id) => playlist_page(&api.page(Req::get(format!("playlists/{id}/items")), offset, limit).await?, offset),
        Ref::Radio(seed, id) => items::slice(&radio_tracks(api, seed, &id).await?, offset, limit, |t| items::track(t, None)),
        Ref::Similar(id) => {
            let v = match api.call(Req::get(format!("albums/{id}/similar")).q("limit", 50)).await {
                Ok(v) => v,
                // No similar albums: an empty list, not an error.
                Err(e) if is_status(&e, 404) => json!({"items": []}),
                Err(e) => return Err(e.into()),
            };
            let all = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            items::slice(&all, offset, limit, items::album)
        }
        Ref::Track(_) => return Err(RpcError::invalid_params("a track is not browsable")),
    };
    Ok(page)
}

#[derive(Deserialize)]
pub struct RadioNextParams {
    seed: String,
    #[serde(default)]
    exclude: Vec<String>,
    limit: Option<u32>,
}

/// `radio.next {seed, exclude, limit}`: playable tracks close to the seed
/// (a track, album or artist ref), without those in `exclude`.
pub async fn radio_next(api: &Api, p: RadioNextParams) -> RpcResult {
    let (seed, id) = match parse_ref(&p.seed)? {
        Ref::Track(id) => (RadioSeed::Track, id),
        Ref::Album(id) => (RadioSeed::Album, id),
        Ref::Artist(id) => (RadioSeed::Artist, id),
        Ref::Radio(seed, id) => (seed, id),
        _ => return Err(RpcError::invalid_params(format!("{} cannot seed a radio", p.seed))),
    };
    let limit = p.limit.unwrap_or(20).clamp(1, 50) as usize;
    let tracks = radio_tracks(api, seed, &id).await?;
    let exclude: std::collections::HashSet<&str> = p.exclude.iter().map(String::as_str).collect();
    let seed_ref = (seed == RadioSeed::Track).then(|| Ref::Track(id.clone()).to_string());
    let items: Vec<Value> = tracks
        .iter()
        .filter_map(|t| items::track(items::unwrap(t), None))
        .filter(|t| t.playable && !exclude.contains(t.reference.as_str()) && Some(&t.reference) != seed_ref.as_ref())
        .take(limit)
        .map(|t| t.to_json())
        .collect();
    Ok(json!({"items": items}))
}

#[derive(Deserialize)]
pub struct SearchParams {
    query: String,
    kinds: Option<Vec<String>>,
    #[serde(default)]
    offset: u32,
    limit: Option<u32>,
}

/// Protocol kind → TIDAL search type and answer key.
fn search_group(kind: &str) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        "track" => ("TRACKS", "tracks"),
        "album" => ("ALBUMS", "albums"),
        "artist" => ("ARTISTS", "artists"),
        "playlist" => ("PLAYLISTS", "playlists"),
        _ => return None,
    })
}

pub async fn search(api: &Api, p: SearchParams) -> RpcResult {
    let query = p.query.trim();
    if query.is_empty() {
        return Err(RpcError::invalid_params("empty query"));
    }
    let kinds: Vec<&str> = match &p.kinds {
        Some(kinds) => kinds.iter().map(String::as_str).filter(|k| search_group(k).is_some()).collect(),
        None => vec!["track", "album", "artist", "playlist"],
    };
    if kinds.is_empty() {
        return Ok(json!({"groups": []}));
    }
    let limit = clamp(p.limit).min(100);
    let types: Vec<&str> = kinds.iter().filter_map(|k| search_group(k)).map(|g| g.0).collect();
    let req = Req::get("search").q("query", query).q("types", types.join(",")).q("offset", p.offset).q("limit", limit);
    let v = api.call(req).await?;
    let groups: Vec<Value> = kinds
        .iter()
        .map(|&kind| {
            let list = v.get(search_group(kind).expect("filtered above").1).cloned().unwrap_or_default();
            let mut group = match kind {
                "track" => items::page(&list, p.offset, |t| items::track(t, None)),
                "album" => items::page(&list, p.offset, items::album),
                "artist" => items::page(&list, p.offset, items::artist),
                _ => items::page(&list, p.offset, items::playlist),
            };
            group["kind"] = kind.into();
            group
        })
        .collect();
    Ok(json!({"groups": groups}))
}

pub async fn get(api: &Api, reference: &str) -> RpcResult {
    let r = parse_ref(reference)?;
    let item = match &r {
        Ref::Track(id) => items::track(&api.get(format!("tracks/{id}")).await?, None),
        Ref::Album(id) => items::album(&api.get(format!("albums/{id}")).await?),
        Ref::Artist(id) => items::artist(&api.get(format!("artists/{id}")).await?),
        Ref::Playlist(id) => items::playlist(&api.get(format!("playlists/{id}")).await?),
        Ref::Mix(_) => {
            let mixes = my_mixes(api).await.unwrap_or_default();
            let found = mixes.iter().filter_map(items::mix).find(|m| m.reference == reference);
            Some(found.unwrap_or_else(|| items::Item { kind: "playlist", ..items::folder(&r, "Mix") }))
        }
        Ref::Radio(seed, id) => {
            let path = match seed {
                RadioSeed::Track => format!("tracks/{id}"),
                RadioSeed::Album => format!("albums/{id}"),
                RadioSeed::Artist => format!("artists/{id}"),
            };
            let v = api.get(path).await?;
            let name = v.get("title").or_else(|| v.get("name")).and_then(Value::as_str);
            Some(items::radio(*seed, id, name))
        }
        _ => return Ok(folder(r)),
    };
    item.map(|i| i.to_json()).ok_or_else(|| RpcError::not_found(format!("{reference}: unreadable answer")))
}

pub async fn set_favorite(api: &Api, reference: &str, on: bool) -> RpcResult {
    let (kind, path, field, id) = match parse_ref(reference)? {
        Ref::Track(id) => ("track", "tracks", "trackId", id),
        Ref::Album(id) => ("album", "albums", "albumId", id),
        Ref::Artist(id) => ("artist", "artists", "artistId", id),
        Ref::Playlist(id) => ("playlist", "playlists", "uuids", id),
        _ => return Err(RpcError::invalid_params(format!("{reference} cannot be a favourite"))),
    };
    let user = uid(api).await?;
    let req = if on {
        Req::new(reqwest::Method::POST, format!("users/{user}/favorites/{path}"))
            .form(&[(field, id.clone()), ("onArtifactNotFound", "FAIL".into())])
    } else {
        Req::new(reqwest::Method::DELETE, format!("users/{user}/favorites/{path}/{id}"))
    };
    api.call(req).await?;
    items::note_favorite(kind, &id, on);
    Ok(Value::Null)
}

/// Ids of the account's favourites, for the `favorite` field of items.
pub async fn favorite_ids(api: &Api) -> Result<items::FavoriteIds> {
    let v = api.get(format!("users/{}/favorites/ids", uid(api).await?)).await?;
    Ok(items::FavoriteIds::from_json(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against TIDAL: `TIDALSRC_DATA=<dir holding account.json> cargo test
    /// live_catalogue -- --ignored --nocapture`. Reads every kind of list
    /// once and prints what came back.
    #[tokio::test]
    #[ignore]
    async fn live_catalogue() {
        let dir = std::path::PathBuf::from(std::env::var("TIDALSRC_DATA").expect("TIDALSRC_DATA"));
        let api = Api::new(crate::api::Config::default());
        api.set_data_dir(Some(dir.clone()));
        let creds = crate::auth::Credentials::load(&dir).unwrap().expect("signed in");
        items::set_user(Some(creds.user_id));
        api.set_credentials(Some(creds)).await;
        let count = |v: &Value| v["items"].as_array().map_or(0, Vec::len);
        let root = root(&api).await.unwrap();
        println!("home shelves: {}", root["home"].as_array().unwrap().len() - 1);
        for r in ["mixes", "explore", "fav/tracks", "fav/albums", "fav/artists", "my/playlists"] {
            let p: ListParams = serde_json::from_value(json!({"ref": r})).unwrap();
            println!("{r}: {} items", count(&list(&api, p).await.unwrap()));
        }
        let found = search(&api, serde_json::from_value(json!({"query": "Beatles"})).unwrap()).await.unwrap();
        let first_track = found["groups"][0]["items"][0]["ref"].as_str().unwrap().to_string();
        println!("search: {first_track}");
        let p: ListParams = serde_json::from_value(json!({"ref": format!("radio/{first_track}")})).unwrap();
        println!("radio: {} tracks", count(&list(&api, p).await.unwrap()));
        println!("favourites: {:?} tracks", favorite_ids(&api).await.unwrap().tracks.len());
    }

    #[test]
    fn playlist_entries_name_their_position() {
        let v = json!({"totalNumberOfItems": 5, "items": [
            {"type": "track", "cut": null, "item": {"id": 77, "title": "A"}},
            {"type": "video", "item": {"id": 9, "title": "Clip"}},
            {"type": "track", "item": {"id": 78, "title": "B"}}
        ]});
        let p = playlist_page(&v, 2);
        let entries: Vec<&str> = p["items"].as_array().unwrap().iter().map(|t| t["entry_id"].as_str().unwrap()).collect();
        assert_eq!(entries, ["2-77", "4-78"], "a video keeps its place in the count");
        assert_eq!((p["total"].as_u64(), p["has_more"].as_bool()), (Some(5), Some(false)));
    }
}
