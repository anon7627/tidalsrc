//! Protocol `ref`s and conversion of TIDAL API objects into protocol items.
//!
//! Refs: `track/<id>`, `album/<id>`, `artist/<id>`, `playlist/<uuid>`,
//! `mix/<id>` for catalogue entries; `fav`, `fav/tracks`, `fav/albums`,
//! `fav/artists`, `fav/playlists`, `my/playlists`, `mixes`, `home`,
//! `explore` for sections; `page/<api path>` for an editorial page (genres,
//! moods…), `shelf/<n>/<page>` for the n-th shelf of a page (`home`, or a
//! page's api path); `radio/<track|album|artist>/<id>` and
//! `similar/<album id>` for related content.

use std::collections::HashSet;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ref {
    Track(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Mix(String),
    Favorites,
    FavTracks,
    FavAlbums,
    FavArtists,
    FavPlaylists,
    MyPlaylists,
    Mixes,
    Home,
    Explore,
    /// An editorial page of the v1 API (`pages/genre_jazz`…).
    Page(String),
    /// Shelf `n` of a page: `home`, or a page's api path.
    Shelf(String, usize),
    /// Tracks close to a track, album or artist.
    Radio(RadioSeed, String),
    /// Albums similar to an album.
    Similar(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioSeed {
    Track,
    Album,
    Artist,
}

impl RadioSeed {
    pub fn as_str(self) -> &'static str {
        match self {
            RadioSeed::Track => "track",
            RadioSeed::Album => "album",
            RadioSeed::Artist => "artist",
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn numeric(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.chars().all(|c| c.is_ascii_digit())
}

/// An api path of the v1 pages (`pages/genre_jazz`).
pub fn valid_page(path: &str) -> bool {
    path.starts_with("pages/")
        && path.len() <= 200
        && !path.contains("..")
        && path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/'))
}

impl Ref {
    pub fn parse(s: &str) -> Option<Self> {
        let r = match s {
            "fav" => Ref::Favorites,
            "fav/tracks" => Ref::FavTracks,
            "fav/albums" => Ref::FavAlbums,
            "fav/artists" => Ref::FavArtists,
            "fav/playlists" => Ref::FavPlaylists,
            "my/playlists" => Ref::MyPlaylists,
            "mixes" => Ref::Mixes,
            "home" => Ref::Home,
            "explore" => Ref::Explore,
            _ => {
                let (kind, id) = s.split_once('/')?;
                match kind {
                    "page" if valid_page(id) => Ref::Page(id.to_string()),
                    "shelf" => {
                        let (n, page) = id.split_once('/')?;
                        let n: usize = n.parse().ok().filter(|n| *n < 1000)?;
                        if page != "home" && !valid_page(page) {
                            return None;
                        }
                        Ref::Shelf(page.to_string(), n)
                    }
                    "radio" => {
                        let (seed, id) = id.split_once('/')?;
                        let seed = match seed {
                            "track" if numeric(id) => RadioSeed::Track,
                            "album" if numeric(id) => RadioSeed::Album,
                            "artist" if numeric(id) => RadioSeed::Artist,
                            _ => return None,
                        };
                        Ref::Radio(seed, id.to_string())
                    }
                    "similar" if numeric(id) => Ref::Similar(id.to_string()),
                    "track" if numeric(id) => Ref::Track(id.to_string()),
                    "album" if numeric(id) => Ref::Album(id.to_string()),
                    "artist" if numeric(id) => Ref::Artist(id.to_string()),
                    "playlist" if valid_id(id) => Ref::Playlist(id.to_string()),
                    "mix" if valid_id(id) => Ref::Mix(id.to_string()),
                    _ => return None,
                }
            }
        };
        Some(r)
    }
}

impl std::fmt::Display for Ref {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ref::Track(id) => write!(f, "track/{id}"),
            Ref::Album(id) => write!(f, "album/{id}"),
            Ref::Artist(id) => write!(f, "artist/{id}"),
            Ref::Playlist(id) => write!(f, "playlist/{id}"),
            Ref::Mix(id) => write!(f, "mix/{id}"),
            Ref::Favorites => f.write_str("fav"),
            Ref::FavTracks => f.write_str("fav/tracks"),
            Ref::FavAlbums => f.write_str("fav/albums"),
            Ref::FavArtists => f.write_str("fav/artists"),
            Ref::FavPlaylists => f.write_str("fav/playlists"),
            Ref::MyPlaylists => f.write_str("my/playlists"),
            Ref::Mixes => f.write_str("mixes"),
            Ref::Home => f.write_str("home"),
            Ref::Explore => f.write_str("explore"),
            Ref::Page(path) => write!(f, "page/{path}"),
            Ref::Shelf(page, n) => write!(f, "shelf/{n}/{page}"),
            Ref::Radio(seed, id) => write!(f, "radio/{}/{id}", seed.as_str()),
            Ref::Similar(id) => write!(f, "similar/{id}"),
        }
    }
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub struct Format {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits: Option<u32>,
    pub codec: &'static str,
}

#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Item {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_no: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Albums, playlists: number of tracks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Format>,
    pub playable: bool,
    pub browsable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_ref: Option<String>,
    /// In the account's favourites; absent when not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Tracks of a playlist: their entry in it, `<index>-<track id>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Playlists: the account owns it, so it may edit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub editable: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// An entry of `Item.actions`: play or open a related ref.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Action {
    pub id: &'static str,
    pub label: String,
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: &'static str,
}

impl Item {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("item serialises")
    }
}

/// Ids of the account's favourites (`users/<id>/favorites/ids`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FavoriteIds {
    pub tracks: HashSet<String>,
    pub albums: HashSet<String>,
    pub artists: HashSet<String>,
    pub playlists: HashSet<String>,
}

impl FavoriteIds {
    /// `{"TRACK": [...], "ALBUM": [...], "ARTIST": [...], "PLAYLIST": [...]}`.
    pub fn from_json(v: &Value) -> Self {
        let ids = |key: &str| -> HashSet<String> {
            v.get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|id| match id {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .collect()
        };
        FavoriteIds { tracks: ids("TRACK"), albums: ids("ALBUM"), artists: ids("ARTIST"), playlists: ids("PLAYLIST") }
    }

    fn set(&mut self, kind: &str) -> Option<&mut HashSet<String>> {
        match kind {
            "track" => Some(&mut self.tracks),
            "album" => Some(&mut self.albums),
            "artist" => Some(&mut self.artists),
            "playlist" => Some(&mut self.playlists),
            _ => None,
        }
    }
}

/// The signed-in account, as far as item conversion needs it. One account
/// per plugin process, hence a global.
struct Account {
    user_id: Option<u64>,
    favorites: Option<FavoriteIds>,
    /// The host's language, for labels.
    french: bool,
}

static ACCOUNT: Mutex<Account> = Mutex::new(Account { user_id: None, favorites: None, french: false });

fn account() -> std::sync::MutexGuard<'static, Account> {
    ACCOUNT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Language of the labels the plugin writes itself (`fr`, else English).
pub fn set_lang(lang: &str) {
    account().french = lang == "fr";
}

/// `en` or `fr` text, per the host's language.
pub fn tr(en: &str, fr: &str) -> String {
    if account().french { fr } else { en }.to_string()
}

pub fn user_id() -> Option<u64> {
    account().user_id
}

/// TIDAL user id of the signed-in account (`None`: signed out).
pub fn set_user(user_id: Option<u64>) {
    let mut a = account();
    if a.user_id != user_id {
        a.favorites = None;
    }
    a.user_id = user_id;
}

pub fn set_favorites(favorites: Option<FavoriteIds>) {
    account().favorites = favorites;
}

/// After `favorites.set`: `kind` is `track`, `album`, `artist` or `playlist`.
pub fn note_favorite(kind: &str, id: &str, on: bool) {
    if let Some(set) = account().favorites.as_mut().and_then(|f| f.set(kind)) {
        if on {
            set.insert(id.to_string());
        } else {
            set.remove(id);
        }
    }
}

fn is_favorite(kind: &str, id: &str) -> Option<bool> {
    let mut a = account();
    let set = a.favorites.as_mut()?.set(kind)?;
    Some(set.contains(id))
}

fn action(id: &'static str, label: String, r: Ref, kind: &'static str) -> Action {
    Action { id, label, reference: r.to_string(), kind }
}

fn str_at(v: &Value, path: &[&str]) -> Option<String> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn id_of(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|id| valid_id(id))
}

/// Picture of `resources.tidal.com` from its id (a UUID): `w`×`h` pixels.
pub fn image(uuid: &str, w: u32, h: u32) -> Option<String> {
    let ok = uuid.len() <= 64 && !uuid.is_empty() && uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    ok.then(|| format!("https://resources.tidal.com/images/{}/{w}x{h}.jpg", uuid.replace('-', "/")))
}

fn cover_of(album: &Value) -> Option<String> {
    image(&str_at(album, &["cover"])?, 640, 640)
}

fn title_with_version(v: &Value) -> String {
    let title = str_at(v, &["title"]).unwrap_or_else(|| "?".into());
    match str_at(v, &["version"]) {
        Some(version) if !title.contains(&version) => format!("{title} ({version})"),
        _ => title,
    }
}

fn year_of(album: &Value) -> Option<u32> {
    str_at(album, &["releaseDate"]).and_then(|d| d.get(..4)?.parse().ok())
}

/// Main artists: those of type `MAIN` in `artists`, else `artist`.
fn artists_of(v: &Value) -> (Option<String>, Option<String>) {
    let list = v.get("artists").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let main: Vec<&Value> = list.iter().filter(|a| a.get("type").and_then(Value::as_str).is_none_or(|t| t == "MAIN")).collect();
    let names: Vec<String> = main.iter().filter_map(|a| str_at(a, &["name"])).collect();
    let first_id = main.first().and_then(|a| id_of(a, "id"));
    let single = v.get("artist");
    let name = if names.is_empty() { single.and_then(|a| str_at(a, &["name"])) } else { Some(names.join(", ")) };
    let id = single.and_then(|a| id_of(a, "id")).or(first_id);
    (name, id.map(|id| Ref::Artist(id).to_string()))
}

/// Best format on offer: `audioQuality` and `mediaMetadata.tags`. Hi-res
/// files come in several rates; only the bit depth is known here.
pub fn format_of(v: &Value) -> Option<Format> {
    let tags: Vec<&str> =
        v.pointer("/mediaMetadata/tags").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    let quality = v.get("audioQuality").and_then(Value::as_str).unwrap_or_default();
    if tags.contains(&"HIRES_LOSSLESS") || quality == "HI_RES_LOSSLESS" {
        Some(Format { sample_rate: None, bits: Some(24), codec: "flac" })
    } else if tags.contains(&"LOSSLESS") || matches!(quality, "LOSSLESS" | "HI_RES") {
        Some(Format { sample_rate: Some(44_100), bits: Some(16), codec: "flac" })
    } else if matches!(quality, "HIGH" | "LOW") {
        Some(Format { sample_rate: Some(44_100), bits: None, codec: "aac" })
    } else {
        None
    }
}

fn duration_ms(v: &Value) -> Option<u64> {
    v.get("duration").and_then(Value::as_u64).map(|s| s * 1000)
}

fn count(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}

fn playable(v: &Value) -> bool {
    v.get("allowStreaming").and_then(Value::as_bool).unwrap_or(true) && v.get("streamReady").and_then(Value::as_bool).unwrap_or(true)
}

fn join(parts: &[Option<&str>]) -> Option<String> {
    let parts: Vec<&str> = parts.iter().flatten().copied().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A track; `album` supplies what tracks listed inside an album omit.
pub fn track(v: &Value, album: Option<&Value>) -> Option<Item> {
    let id = id_of(v, "id").filter(|id| numeric(id))?;
    let own_album = v.get("album").filter(|a| a.is_object());
    let album_v = own_album.or(album);
    let album_title = album_v.and_then(|a| str_at(a, &["title"]));
    let (artist, artist_ref) = artists_of(v);
    let (album_artist, album_artist_ref) = album.map(artists_of).unwrap_or_default();
    let artist = artist.or_else(|| album_artist.clone());
    Some(Item {
        reference: Ref::Track(id.clone()).to_string(),
        kind: "track",
        title: title_with_version(v),
        subtitle: join(&[artist.as_deref(), album_title.as_deref()]),
        artist,
        album: album_title,
        album_artist,
        track_no: v.get("trackNumber").and_then(Value::as_u64),
        disc_no: v.get("volumeNumber").and_then(Value::as_u64),
        year: album_v.and_then(year_of).or_else(|| album.and_then(year_of)),
        duration_ms: duration_ms(v),
        art: album_v.and_then(cover_of).or_else(|| album.and_then(cover_of)),
        format: format_of(v).or_else(|| album.and_then(format_of)),
        playable: playable(v),
        album_ref: album_v.and_then(|a| id_of(a, "id")).map(|a| Ref::Album(a).to_string()),
        artist_ref: artist_ref.or(album_artist_ref),
        favorite: is_favorite("track", &id),
        actions: vec![action("radio", tr("Track radio", "Radio de ce titre"), Ref::Radio(RadioSeed::Track, id), "play")],
        ..Default::default()
    })
}

pub fn album(v: &Value) -> Option<Item> {
    let id = id_of(v, "id").filter(|id| numeric(id))?;
    let (artist, artist_ref) = artists_of(v);
    let year = year_of(v);
    let year_s = year.map(|y| y.to_string());
    Some(Item {
        kind: "album",
        title: title_with_version(v),
        subtitle: join(&[artist.as_deref(), year_s.as_deref()]),
        album_artist: artist.clone(),
        artist,
        year,
        duration_ms: duration_ms(v),
        track_count: count(v, "numberOfTracks"),
        art: cover_of(v),
        format: format_of(v),
        playable: playable(v),
        browsable: true,
        artist_ref,
        favorite: is_favorite("album", &id),
        actions: vec![
            action("radio", tr("Album radio", "Radio de cet album"), Ref::Radio(RadioSeed::Album, id.clone()), "play"),
            action("similar", tr("Similar albums", "Albums similaires"), Ref::Similar(id.clone()), "browse"),
        ],
        reference: Ref::Album(id).to_string(),
        ..Default::default()
    })
}

pub fn artist(v: &Value) -> Option<Item> {
    let id = id_of(v, "id").filter(|id| numeric(id))?;
    Some(Item {
        kind: "artist",
        title: str_at(v, &["name"]).unwrap_or_else(|| "?".into()),
        art: str_at(v, &["picture"]).and_then(|p| image(&p, 750, 750)),
        browsable: true,
        favorite: is_favorite("artist", &id),
        actions: vec![action("radio", tr("Artist radio", "Radio de cet artiste"), Ref::Radio(RadioSeed::Artist, id.clone()), "play")],
        reference: Ref::Artist(id).to_string(),
        ..Default::default()
    })
}

pub fn playlist(v: &Value) -> Option<Item> {
    let id = id_of(v, "uuid")?;
    let creator = v.pointer("/creator/id").and_then(Value::as_u64);
    let owner = match v.get("type").and_then(Value::as_str) {
        Some("EDITORIAL") | Some("ARTIST") => Some("TIDAL".to_string()),
        _ => str_at(v, &["creator", "name"]),
    };
    let user = user_id();
    Some(Item {
        kind: "playlist",
        title: str_at(v, &["title"]).unwrap_or_else(|| "?".into()),
        subtitle: owner,
        duration_ms: duration_ms(v),
        track_count: count(v, "numberOfTracks"),
        art: str_at(v, &["squareImage"])
            .and_then(|i| image(&i, 480, 480))
            .or_else(|| str_at(v, &["image"]).and_then(|i| image(&i, 480, 320))),
        browsable: true,
        editable: user.map(|u| creator == Some(u)),
        favorite: is_favorite("playlist", &id),
        reference: Ref::Playlist(id).to_string(),
        ..Default::default()
    })
}

/// A mix (v1 `{title, subTitle, images}` or v2 `{titleTextInfo,
/// mixImages}`), shown as a playlist of its tracks.
pub fn mix(v: &Value) -> Option<Item> {
    let id = id_of(v, "id")?;
    let title = str_at(v, &["title"]).or_else(|| str_at(v, &["titleTextInfo", "text"]));
    let subtitle = str_at(v, &["subTitle"])
        .or_else(|| str_at(v, &["subTitleTextInfo", "text"]))
        .or_else(|| str_at(v, &["subtitleTextInfo", "text"]))
        .or_else(|| str_at(v, &["subtitle"]));
    let v2_art = || {
        let images = v.get("mixImages")?.as_array()?;
        let best = images.iter().max_by_key(|i| i.get("width").and_then(Value::as_u64).unwrap_or(0))?;
        str_at(best, &["url"])
    };
    let art = ["LARGE", "MEDIUM", "SMALL"].iter().find_map(|s| str_at(v, &["images", s, "url"])).or_else(v2_art);
    Some(Item {
        reference: Ref::Mix(id).to_string(),
        kind: "playlist",
        title: title.unwrap_or_else(|| "Mix".into()),
        subtitle,
        art: art.filter(|a| a.starts_with("https://")),
        browsable: true,
        ..Default::default()
    })
}

/// A radio, shown as a playlist of its tracks.
pub fn radio(seed: RadioSeed, id: &str, name: Option<&str>) -> Item {
    Item {
        reference: Ref::Radio(seed, id.to_string()).to_string(),
        kind: "playlist",
        title: match name {
            Some(name) => format!("{} {name}", tr("Radio:", "Radio :")),
            None => tr("Radio", "Radio"),
        },
        browsable: true,
        ..Default::default()
    }
}

/// Anything a page or a mixed list holds: `{type, item}` / `{type, data}`
/// wrappers, or a bare object recognised by its fields.
pub fn any(v: &Value) -> Option<Item> {
    if let (Some(kind), Some(inner)) = (v.get("type").and_then(Value::as_str), v.get("item").or_else(|| v.get("data"))) {
        if inner.is_object() {
            return match kind.to_ascii_uppercase().as_str() {
                "TRACK" => track(inner, None),
                "ALBUM" => album(inner),
                "ARTIST" => artist(inner),
                "PLAYLIST" | "USER_PLAYLIST" => playlist(inner),
                "MIX" => mix(inner),
                _ => None,
            };
        }
    }
    if v.get("mixType").is_some() || v.get("mixImages").is_some() {
        mix(v)
    } else if v.get("uuid").is_some() {
        playlist(v)
    } else if v.get("trackNumber").is_some() {
        track(v, None)
    } else if v.get("numberOfTracks").is_some() || v.get("cover").is_some() {
        album(v)
    } else if v.get("picture").is_some() || v.get("name").is_some() {
        artist(v)
    } else {
        None
    }
}

/// An entry of a list whose kind is known from the list (`ALBUM_LIST`…):
/// `kind` is `TRACK`, `ALBUM`… Without one, as [`any`].
pub fn typed(kind: Option<&str>, v: &Value) -> Option<Item> {
    let v = unwrap(v);
    match kind {
        Some("TRACK") => track(v, None),
        Some("ALBUM") => album(v),
        Some("ARTIST") => artist(v),
        Some("PLAYLIST") => playlist(v),
        Some("MIX") => mix(v),
        _ => any(v),
    }
}

/// A section: a folder the host lists under the plugin's name.
pub fn folder(r: &Ref, title: &str) -> Item {
    Item { reference: r.to_string(), kind: "folder", title: title.to_string(), browsable: true, ..Default::default() }
}

/// The object of a list entry: favourites and playlist items wrap it in
/// `{item}`.
pub fn unwrap(v: &Value) -> &Value {
    v.get("item").filter(|i| i.is_object()).unwrap_or(v)
}

/// `{items, total, has_more}` from a TIDAL list (`{items,
/// totalNumberOfItems}`, read from `offset`).
pub fn page(list: &Value, offset: u32, map: impl Fn(&Value) -> Option<Item>) -> Value {
    let raw = list.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let items: Vec<Value> = raw.iter().filter_map(|v| map(unwrap(v)).map(|i| i.to_json())).collect();
    let total = list.get("totalNumberOfItems").and_then(Value::as_u64);
    let seen = u64::from(offset) + raw.len() as u64;
    let has_more = total.is_some_and(|t| seen < t) && !raw.is_empty();
    let mut out = json!({"items": items, "has_more": has_more});
    if let Some(total) = total {
        out["total"] = total.into();
    }
    out
}

/// Cut a page out of a whole list (radios, similar albums: not paged).
pub fn slice(all: &[Value], offset: u32, limit: u32, map: impl Fn(&Value) -> Option<Item>) -> Value {
    let from = (offset as usize).min(all.len());
    let to = (from + limit as usize).min(all.len());
    let items: Vec<Value> = all[from..to].iter().filter_map(|v| map(unwrap(v)).map(|i| i.to_json())).collect();
    json!({"items": items, "total": all.len(), "has_more": to < all.len()})
}

/// A page of a list of items already built.
pub fn slice_items(all: Vec<Item>, offset: u32, limit: u32) -> Value {
    let total = all.len();
    let items: Vec<Value> = all.into_iter().skip(offset as usize).take(limit as usize).map(|i| i.to_json()).collect();
    let has_more = (offset as usize).saturating_add(items.len()) < total;
    json!({"items": items, "total": total, "has_more": has_more})
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn track_json() -> Value {
        // As `tracks/<id>` answers, trimmed.
        json!({
            "id": 77646958, "title": "Hey Jude", "duration": 425, "replayGain": -9.6, "peak": 0.98,
            "allowStreaming": true, "streamReady": true, "trackNumber": 21, "volumeNumber": 1,
            "version": "Remastered 2015", "audioQuality": "LOSSLESS",
            "mediaMetadata": {"tags": ["LOSSLESS", "HIRES_LOSSLESS"]},
            "artist": {"id": 3634161, "name": "The Beatles", "type": "MAIN"},
            "artists": [{"id": 3634161, "name": "The Beatles", "type": "MAIN"}, {"id": 1, "name": "Guest", "type": "FEATURED"}],
            "album": {"id": 77646936, "title": "1 (Remastered)", "cover": "b7a5b6ab-4a8b-4bb1-a4c8-11a0a2da3f6d"},
            "mixes": {"TRACK_MIX": "0012d2b6ac73b11c8a2d6ad8bd2a49"}
        })
    }

    #[test]
    fn refs_round_trip_and_reject_junk() {
        for s in [
            "track/123", "album/77646936", "artist/9", "playlist/36ea71a8-445e-41a4-82ab-6628c581535d", "mix/0012d2b6ac73b11c8a2d6ad8bd2a49",
            "fav", "fav/tracks", "fav/albums", "fav/artists", "fav/playlists", "my/playlists", "mixes", "home", "explore",
            "page/pages/genre_jazz", "shelf/3/home", "shelf/0/pages/explore", "radio/track/1", "radio/album/2", "radio/artist/3", "similar/4",
        ] {
            assert_eq!(Ref::parse(s).unwrap().to_string(), s);
        }
        for s in [
            "", "track/", "track/abc", "track/1/2", "playlist/a b", "page/explore", "page/pages/../x", "shelf/x/home", "shelf/1/pages",
            "shelf/1/elsewhere", "radio/label/1", "radio/track/x", "fav/stuff", "similar/x",
        ] {
            assert_eq!(Ref::parse(s), None, "{s}");
        }
    }

    #[test]
    fn tracks_from_recorded_json() {
        let item = track(&track_json(), None).unwrap();
        assert_eq!(item.reference, "track/77646958");
        assert_eq!(item.title, "Hey Jude (Remastered 2015)");
        assert_eq!(item.artist.as_deref(), Some("The Beatles"), "featured artists left out");
        assert_eq!(item.subtitle.as_deref(), Some("The Beatles · 1 (Remastered)"));
        assert_eq!((item.track_no, item.disc_no, item.duration_ms), (Some(21), Some(1), Some(425_000)));
        assert_eq!(item.art.as_deref(), Some("https://resources.tidal.com/images/b7a5b6ab/4a8b/4bb1/a4c8/11a0a2da3f6d/640x640.jpg"));
        assert_eq!(item.format, Some(Format { sample_rate: None, bits: Some(24), codec: "flac" }));
        assert_eq!((item.album_ref.as_deref(), item.artist_ref.as_deref()), (Some("album/77646936"), Some("artist/3634161")));
        assert!(item.playable && !item.browsable);
        assert_eq!(item.actions[0].reference, "radio/track/77646958");

        let mut gone = track_json();
        gone["streamReady"] = false.into();
        assert!(!track(&gone, None).unwrap().playable);
        // Inside an album listing: the album passed along fills the gaps.
        let album_json = json!({"id": 5, "title": "A", "releaseDate": "1968-08-26", "cover": "ab-cd", "artist": {"id": 8, "name": "B"}});
        let t = track(&json!({"id": 1, "title": "T", "audioQuality": "HIGH"}), Some(&album_json)).unwrap();
        assert_eq!((t.year, t.artist.as_deref(), t.album_ref.as_deref()), (Some(1968), Some("B"), Some("album/5")));
        assert_eq!(t.format, Some(Format { sample_rate: Some(44_100), bits: None, codec: "aac" }));
    }

    #[test]
    fn albums_artists_playlists_mixes() {
        let a = album(&json!({"id": 77646936, "title": "1", "version": "Remastered", "numberOfTracks": 27, "duration": 4757,
                              "releaseDate": "2000-11-13", "cover": "b7a5b6ab-4a8b", "audioQuality": "LOSSLESS",
                              "artists": [{"id": 3634161, "name": "The Beatles", "type": "MAIN"}]})).unwrap();
        assert_eq!((a.title.as_str(), a.subtitle.as_deref(), a.track_count), ("1 (Remastered)", Some("The Beatles · 2000"), Some(27)));
        assert_eq!(a.format, Some(Format { sample_rate: Some(44_100), bits: Some(16), codec: "flac" }));
        assert_eq!(a.actions.len(), 2);

        let ar = artist(&json!({"id": 3634161, "name": "The Beatles", "picture": "1b5a6d0f-2a73"})).unwrap();
        assert_eq!(ar.art.as_deref(), Some("https://resources.tidal.com/images/1b5a6d0f/2a73/750x750.jpg"));
        assert_eq!(artist(&json!({"id": 1, "name": "x", "picture": "../../etc"})).unwrap().art, None, "only an image id makes a URL");

        let p = playlist(&json!({"uuid": "36ea71a8-445e", "title": "Soir", "numberOfTracks": 12, "type": "EDITORIAL",
                                 "creator": {"id": 0}, "squareImage": "aa-bb"})).unwrap();
        assert_eq!((p.reference.as_str(), p.subtitle.as_deref(), p.track_count), ("playlist/36ea71a8-445e", Some("TIDAL"), Some(12)));
        assert_eq!(p.art.as_deref(), Some("https://resources.tidal.com/images/aa/bb/480x480.jpg"));

        let m = mix(&json!({"id": "0012d2b6", "title": "My Mix 1", "subTitle": "Daft Punk, Justice", "mixType": "DISCOVERY_MIX",
                            "images": {"LARGE": {"url": "https://resources.tidal.com/images/x/1500x1500.jpg"}}})).unwrap();
        assert_eq!((m.reference.as_str(), m.kind, m.subtitle.as_deref()), ("mix/0012d2b6", "playlist", Some("Daft Punk, Justice")));
        let m2 = mix(&json!({"id": "abc", "titleTextInfo": {"text": "Daily Discovery"},
                             "mixImages": [{"url": "https://i/s.jpg", "width": 320}, {"url": "https://i/l.jpg", "width": 1500}]})).unwrap();
        assert_eq!((m2.title.as_str(), m2.art.as_deref()), ("Daily Discovery", Some("https://i/l.jpg")));
    }

    #[test]
    fn mixed_lists_and_pages() {
        let wrapped = json!({"type": "ALBUM", "item": {"id": 5, "title": "A"}});
        assert_eq!(any(&wrapped).unwrap().reference, "album/5");
        assert_eq!(any(&json!({"type": "PLAYLIST", "data": {"uuid": "u-1", "title": "P"}})).unwrap().reference, "playlist/u-1");
        assert_eq!(any(&json!({"type": "VIDEO", "item": {"id": 5}})), None, "videos are left out");
        assert_eq!(any(&track_json()).unwrap().kind, "track");
        assert_eq!(any(&json!({"id": 9, "name": "Artist", "picture": null})).unwrap().kind, "artist");

        let fav = json!({"limit": 2, "offset": 0, "totalNumberOfItems": 5, "items": [
            {"created": "2024-01-01T00:00:00.000+0000", "item": {"id": 5, "title": "A"}},
            {"created": "2024-01-01T00:00:00.000+0000", "item": {"title": "no id"}}
        ]});
        let p = page(&fav, 0, album);
        assert_eq!((p["items"].as_array().unwrap().len(), p["total"].as_u64(), p["has_more"].as_bool()), (1, Some(5), Some(true)));
        let p = page(&json!({"items": [], "totalNumberOfItems": 5}), 4, album);
        assert_eq!(p["has_more"], false, "nothing more came back: stop");

        let all: Vec<Value> = (1..=5).map(|i| json!({"id": i, "title": "t", "trackNumber": i})).collect();
        let s = slice(&all, 3, 10, |t| track(t, None));
        assert_eq!((s["items"].as_array().unwrap().len(), s["total"].as_u64(), s["has_more"].as_bool()), (2, Some(5), Some(false)));
    }

    /// Account-dependent fields; one test, since the account is global.
    #[test]
    fn favourites_and_ownership_follow_the_account() {
        let mine = json!({"uuid": "p1", "title": "Soir", "creator": {"id": 42}, "type": "USER"});
        let theirs = json!({"uuid": "p2", "title": "Top", "creator": {"id": 7}, "type": "USER"});
        set_user(None);
        assert_eq!(playlist(&mine).unwrap().editable, None, "unknown when signed out");
        set_user(Some(42));
        assert_eq!(playlist(&mine).unwrap().editable, Some(true));
        assert_eq!(playlist(&theirs).unwrap().editable, Some(false));

        assert_eq!(track(&track_json(), None).unwrap().favorite, None, "ids not loaded yet");
        set_favorites(Some(FavoriteIds::from_json(&json!({"TRACK": ["77646958"], "ALBUM": [77646936], "ARTIST": [], "PLAYLIST": ["p1"]}))));
        assert_eq!(track(&track_json(), None).unwrap().favorite, Some(true));
        assert_eq!(album(&json!({"id": 77646936})).unwrap().favorite, Some(true));
        assert_eq!(playlist(&mine).unwrap().favorite, Some(true));
        assert_eq!(artist(&json!({"id": 5, "name": "x"})).unwrap().favorite, Some(false));
        note_favorite("track", "77646958", false);
        assert_eq!(track(&track_json(), None).unwrap().favorite, Some(false));
        set_user(Some(43));
        assert_eq!(track(&track_json(), None).unwrap().favorite, None, "another account: ids dropped");
        set_user(None);
    }

    #[test]
    fn item_json_skips_missing_fields() {
        let v = folder(&Ref::MyPlaylists, "Playlists").to_json();
        assert_eq!(v, json!({"ref": "my/playlists", "kind": "folder", "title": "Playlists", "playable": false, "browsable": true}));
    }
}
