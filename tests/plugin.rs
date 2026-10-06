//! Fake plugin host: runs `tidalsrc plugin` against a mock TIDAL (API, OAuth
//! server and CDN in one) and checks the JSON-RPC exchanges.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use base64::Engine;
use serde_json::{json, Value};
use sha2::Digest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const USER: u64 = 1;
/// Home feed, under the v2 API.
const HOME_FEED: &str = "home/feed/static";
const EMAIL: &str = "ada@example.org";

/// What the mock saw of one request.
#[derive(Debug, Clone)]
struct Call {
    method: String,
    path: String,
    query: HashMap<String, String>,
    form: HashMap<String, String>,
    if_none_match: Option<String>,
}

#[derive(Default)]
struct Mock {
    calls: Vec<Call>,
    device_polls: u32,
    /// Access tokens the API refuses (expired).
    stale: HashSet<String>,
    /// The refresh token is refused (revoked).
    revoked: bool,
    /// Version of `p-mine`, its ETag.
    version: u32,
    /// Answer the next playlist edit with 412.
    conflict_once: bool,
    /// Tracks of `p-mine`.
    mine: Vec<u64>,
}

type Shared = Arc<Mutex<Mock>>;

/// FLAC in fragmented MP4, as TIDAL serves hi-res.
mod dash_files {
    pub const FRAMES: [&[&[u8]]; 2] = [&[b"\xff\xf8frame one", b"\xff\xf8frame two!"], &[b"\xff\xf8last frame of the track"]];
    pub const SAMPLES_PER_SEGMENT: u64 = 4096;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    fn streaminfo(total: u64) -> Vec<u8> {
        let mut si = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0x10, 0, 0x20, 0];
        let x: u64 = (96_000u64 << 44) | (1 << 41) | (23u64 << 36) | total;
        si.extend_from_slice(&x.to_be_bytes());
        si.extend_from_slice(&[0x5a; 16]);
        si
    }

    const COMMENT: &[u8] = b"\x05\0\0\0mock!\0\0\0\0";

    fn blocks(total: u64) -> Vec<u8> {
        let mut b = vec![0u8, 0, 0, 34];
        b.extend(streaminfo(total));
        b.push(0x84);
        b.extend_from_slice(&(COMMENT.len() as u32).to_be_bytes()[1..]);
        b.extend_from_slice(COMMENT);
        b
    }

    /// Init segment: STREAMINFO without a length, as TIDAL sends it.
    pub fn init() -> Vec<u8> {
        let mut dfla = vec![0u8; 4];
        dfla.extend(blocks(0));
        let mut entry = vec![0u8; 28];
        entry.extend(boxed(b"dfLa", &dfla));
        let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsd.extend(boxed(b"fLaC", &entry));
        let nest = |kind: &[u8; 4], inner: Vec<u8>| boxed(kind, &inner);
        let moov = nest(b"moov", nest(b"trak", nest(b"mdia", nest(b"minf", nest(b"stbl", boxed(b"stsd", &stsd))))));
        let mut f = boxed(b"ftyp", b"iso6dash");
        f.extend(moov);
        f
    }

    pub fn segment(frames: &[&[u8]]) -> Vec<u8> {
        let tfhd = boxed(b"tfhd", &[0, 2, 0, 0, 0, 0, 0, 1]);
        let trun_len = 20 + frames.len() * 4;
        let moof_len = 8 + 16 + 8 + tfhd.len() + trun_len;
        let mut trun = vec![0, 0, 2, 1];
        trun.extend_from_slice(&(frames.len() as u32).to_be_bytes());
        trun.extend_from_slice(&((moof_len + 8) as u32).to_be_bytes());
        for f in frames {
            trun.extend_from_slice(&(f.len() as u32).to_be_bytes());
        }
        let mut traf = tfhd;
        traf.extend(boxed(b"trun", &trun));
        let mut moof = boxed(b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1]);
        moof.extend(boxed(b"traf", &traf));
        let mut f = boxed(b"styp", b"msdh");
        f.extend(boxed(b"moof", &moof));
        f.extend(boxed(b"mdat", &frames.concat()));
        f
    }

    /// The FLAC file the relay must serve: the length filled in from the
    /// timeline, the frames unchanged.
    pub fn flac() -> Vec<u8> {
        let mut f = b"fLaC".to_vec();
        f.extend(blocks(SAMPLES_PER_SEGMENT * FRAMES.len() as u64));
        for seg in FRAMES {
            f.extend(seg.concat());
        }
        f
    }

    pub fn mpd(base: &str) -> String {
        format!(
            r#"<?xml version='1.0' encoding='UTF-8'?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT0.085S">
 <Period id="0"><AdaptationSet id="0" contentType="audio" mimeType="audio/mp4">
  <Representation id="FLAC,96000,24" codecs="flac" bandwidth="2620823" audioSamplingRate="96000">
   <SegmentTemplate timescale="96000" initialization="{base}/dash/79/0.mp4?token=t&amp;x=1" media="{base}/dash/79/$Number$.mp4?token=t&amp;x=1" startNumber="1">
    <SegmentTimeline><S d="{SAMPLES_PER_SEGMENT}" r="1"/></SegmentTimeline>
   </SegmentTemplate>
  </Representation></AdaptationSet></Period>
</MPD>"#
        )
    }
}

fn b64(s: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn album_json() -> Value {
    json!({"id": 5, "title": "Abbey Road", "numberOfTracks": 2, "numberOfVolumes": 1, "releaseDate": "1969-09-26",
           "cover": "aa11-bb22", "audioQuality": "LOSSLESS", "mediaMetadata": {"tags": ["LOSSLESS", "HIRES_LOSSLESS"]},
           "copyright": "© 1969 Apple Corps", "artist": {"id": 8, "name": "The Beatles"},
           "artists": [{"id": 8, "name": "The Beatles", "type": "MAIN"}]})
}

fn track_json(id: u64, title: &str) -> Value {
    json!({"id": id, "title": title, "duration": 259, "trackNumber": 1, "volumeNumber": 1, "allowStreaming": true,
           "streamReady": true, "audioQuality": "LOSSLESS", "artist": {"id": 8, "name": "The Beatles"},
           "artists": [{"id": 8, "name": "The Beatles", "type": "MAIN"}],
           "album": {"id": 5, "title": "Abbey Road", "cover": "aa11-bb22"}})
}

fn list(items: Vec<Value>) -> Value {
    json!({"limit": 100, "offset": 0, "totalNumberOfItems": items.len(), "items": items})
}

fn playlist_json(uuid: &str, creator: u64, tracks: usize) -> Value {
    json!({"uuid": uuid, "title": if creator == USER { "Soir" } else { "Top 50" }, "numberOfTracks": tracks, "numberOfVideos": 0,
           "type": if creator == USER { "USER" } else { "EDITORIAL" }, "creator": {"id": creator}, "description": "calm",
           "squareImage": "cc33-dd44"})
}

fn form_of(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            let dec = |s: &str| urlencoding::decode(&s.replace('+', " ")).map(|c| c.into_owned()).unwrap_or_default();
            Some((dec(k), dec(v)))
        })
        .collect()
}

fn tokens(access: &str, refresh: &str) -> Value {
    json!({"access_token": access, "refresh_token": refresh, "token_type": "Bearer", "expires_in": 3600,
           "user": {"userId": USER, "countryCode": "FR", "firstName": "Ada", "username": EMAIL, "email": EMAIL}})
}

fn ranged(headers: &HeaderMap, body: Vec<u8>) -> Response {
    let range = headers.get(header::RANGE).and_then(|r| r.to_str().ok()).and_then(|r| r.strip_prefix("bytes="));
    let Some((a, b)) = range.and_then(|r| r.split_once('-')) else { return body.into_response() };
    let a: usize = a.parse().unwrap();
    let b: usize = b.parse::<usize>().unwrap().min(body.len() - 1);
    let len = body.len();
    (StatusCode::PARTIAL_CONTENT, [(header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}"))], body[a..=b].to_vec()).into_response()
}

async fn mock(
    State(mock): State<Shared>,
    UrlPath(path): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
    method: Method,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form = form_of(&body);
    let if_none_match = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(str::to_string);
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or_default().to_string();
    let base = format!("http://{host}");
    let mut m = mock.lock().unwrap();
    m.calls.push(Call { method: method.to_string(), path: path.clone(), query: q.clone(), form: form.clone(), if_none_match: if_none_match.clone() });

    // OAuth server.
    match path.as_str() {
        "v1/oauth2/device_authorization" => {
            assert_eq!(form.get("scope").map(String::as_str), Some("r_usr w_usr w_sub"));
            return Json(json!({"deviceCode": "dev-1", "userCode": "ABCDE", "verificationUri": "link.tidal.com",
                               "verificationUriComplete": "link.tidal.com/ABCDE", "expiresIn": 300, "interval": 1}))
            .into_response();
        }
        "v1/oauth2/token" => {
            return match form.get("grant_type").map(String::as_str) {
                Some("urn:ietf:params:oauth:grant-type:device_code") => {
                    assert_eq!(form.get("device_code").map(String::as_str), Some("dev-1"));
                    m.device_polls += 1;
                    if m.device_polls < 2 {
                        (StatusCode::BAD_REQUEST, r#"{"status":400,"error":"authorization_pending","sub_status":1002,"error_description":"Device Authorization code is not authorized yet"}"#).into_response()
                    } else {
                        Json(tokens("acc-1", "ref-1")).into_response()
                    }
                }
                Some("authorization_code") if form.get("code").map(String::as_str) == Some("c0de") => {
                    Json(tokens("acc-pkce", "ref-pkce")).into_response()
                }
                Some("refresh_token") if !m.revoked => {
                    Json(json!({"access_token": "acc-2", "token_type": "Bearer", "expires_in": 3600})).into_response()
                }
                _ => (StatusCode::BAD_REQUEST, r#"{"status":400,"error":"invalid_grant","sub_status":11101,"error_description":"Token could not be verified"}"#).into_response(),
            };
        }
        _ => {}
    }

    // CDN: no token.
    if path == "cdn/77.flac" {
        return ranged(&headers, b"fLaC plain file".to_vec());
    }
    if let Some(n) = path.strip_prefix("dash/79/").and_then(|f| f.strip_suffix(".mp4")) {
        assert_eq!(q.get("token").map(String::as_str), Some("t"), "query kept from the manifest");
        let file = match n {
            "0" => dash_files::init(),
            "1" => dash_files::segment(dash_files::FRAMES[0]),
            "2" => dash_files::segment(dash_files::FRAMES[1]),
            _ => return (StatusCode::NOT_FOUND, "no such segment").into_response(),
        };
        return ranged(&headers, file);
    }

    // API.
    let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    if !matches!(token, "acc-1" | "acc-2" | "acc-pkce") || m.stale.contains(token) {
        return (StatusCode::UNAUTHORIZED, r#"{"status":401,"subStatus":11003,"userMessage":"The token has expired. (Expired on time)"}"#).into_response();
    }
    if path.starts_with("v1/") && path != "v1/sessions" {
        assert_eq!(q.get("countryCode").map(String::as_str), Some("FR"), "{path}: country code");
    }
    let etag = format!("\"{}\"", m.version);
    let body = match (method.as_str(), path.as_str()) {
        ("GET", "v1/sessions") => json!({"sessionId": "s", "userId": USER, "countryCode": "FR"}),
        ("GET", "v1/users/1/subscription") => json!({"subscription": {"type": "HIFI_PLUS"}, "highestSoundQuality": "HI_RES_LOSSLESS"}),
        ("GET", "v1/users/1/favorites/ids") => json!({"TRACK": ["77"], "ALBUM": ["5"], "ARTIST": [], "PLAYLIST": [], "VIDEO": []}),
        ("GET", "v1/users/1/favorites/tracks") => list(vec![json!({"created": "2024", "item": track_json(77, "Come Together")})]),
        ("GET", "v1/users/1/favorites/albums") => list(vec![json!({"created": "2024", "item": album_json()})]),
        ("GET", "v1/users/1/favorites/artists") => {
            list(vec![json!({"created": "2024", "item": {"id": 8, "name": "The Beatles", "picture": "ee55-ff66"}})])
        }
        ("GET", "v1/users/1/favorites/playlists") => list(vec![json!({"created": "2024", "item": playlist_json("p-theirs", 9, 50)})]),
        ("POST", "v1/users/1/favorites/albums") | ("DELETE", "v1/users/1/favorites/albums/5") => Value::Null,
        ("GET", "v1/users/1/playlists") => list(vec![playlist_json("p-mine", USER, 2)]),
        ("GET", "v1/users/1/playlistsAndFavoritePlaylists") => list(vec![
            json!({"type": "USER_CREATED", "playlist": playlist_json("p-mine", USER, 2)}),
            json!({"type": "USER_FAVORITE", "playlist": playlist_json("p-theirs", 9, 50)}),
        ]),
        ("GET", "v1/search") if q.get("query").map(String::as_str) == Some("slow") => {
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "7")], r#"{"status":429,"userMessage":"Too many requests"}"#).into_response();
        }
        ("GET", "v1/search") => {
            let types = q.get("types").cloned().unwrap_or_default();
            let mut v = json!({});
            if types.contains("TRACKS") {
                v["tracks"] = list(vec![track_json(77, "Come Together")]);
            }
            if types.contains("ALBUMS") {
                v["albums"] = list(vec![album_json()]);
            }
            if types.contains("ARTISTS") {
                v["artists"] = list(vec![]);
            }
            if types.contains("PLAYLISTS") {
                v["playlists"] = list(vec![playlist_json("p-theirs", 9, 50)]);
            }
            v
        }
        ("GET", "v1/albums/5") => album_json(),
        ("GET", "v1/albums/5/tracks") => {
            let all = vec![json!({"id": 77, "title": "Come Together", "duration": 259, "trackNumber": 1}), json!({"id": 78, "title": "Something", "trackNumber": 2})];
            let offset: usize = q.get("offset").and_then(|o| o.parse().ok()).unwrap_or(0);
            let limit: usize = q.get("limit").and_then(|o| o.parse().ok()).unwrap_or(10);
            json!({"totalNumberOfItems": 2, "items": all.into_iter().skip(offset).take(limit).collect::<Vec<_>>()})
        }
        ("GET", "v1/albums/5/review") => json!({"source": "TiVo", "text": "A [wimpLink artistId=\"8\"]Beatles[/wimpLink] classic.<br/>Recorded in 1969."}),
        ("GET", "v1/albums/5/items/credits") => json!({"items": [
            {"item": {"id": 77}, "credits": [{"type": "Producer", "contributors": [{"name": "George Martin"}]}]},
            {"item": {"id": 78}, "credits": [{"type": "Producer", "contributors": [{"name": "George Martin"}]},
                                             {"type": "Composer", "contributors": [{"name": "George Harrison"}]}]}
        ]}),
        ("GET", "v1/artists/8") => json!({"id": 8, "name": "The Beatles", "picture": "ee55-ff66"}),
        ("GET", "v1/artists/8/albums") => match q.get("filter").map(String::as_str) {
            Some("EPSANDSINGLES") => list(vec![json!({"id": 6, "title": "Let It Be (single)", "type": "SINGLE"})]),
            _ => list(vec![album_json()]),
        },
        ("GET", "v1/artists/8/bio") => json!({"source": "TiVo", "text": "Formed in Liverpool."}),
        ("GET", "v1/artists/8/toptracks") => list(vec![track_json(77, "Come Together")]),
        ("GET", "v1/tracks/77") => track_json(77, "Come Together"),
        ("GET", "v1/tracks/79") => track_json(79, "Hi-res"),
        ("GET", "v1/tracks/77/radio") | ("GET", "v1/artists/8/radio") => list(vec![
            track_json(77, "Come Together"),
            track_json(90, "Here Comes the Sun"),
            json!({"id": 91, "title": "Gone", "streamReady": false}),
        ]),
        ("GET", "v1/tracks/77/credits") => json!([{"type": "Composer", "contributors": [{"name": "John Lennon"}, {"name": "Paul McCartney"}]}]),
        ("GET", "v1/tracks/77/lyrics") => json!({"trackId": 77, "lyrics": "Here come old flat top", "subtitles": "[00:03.10] Here come old flat top"}),
        ("GET", p) if p.starts_with("v1/tracks/") && p.ends_with("/playbackinfo") => {
            let id = p.trim_start_matches("v1/tracks/").trim_end_matches("/playbackinfo");
            let asked = q.get("audioquality").cloned().unwrap_or_default();
            let bts = |codecs: &str, mime: &str, enc: &str| {
                b64(json!({"mimeType": mime, "codecs": codecs, "encryptionType": enc, "urls": [format!("{base}/cdn/{id}.flac?token=exp=4000000000~hmac=x")]}).to_string().as_bytes())
            };
            match (id, asked.as_str()) {
                ("79", "HI_RES_LOSSLESS") => json!({"trackId": 79, "assetPresentation": "FULL", "audioQuality": "HI_RES_LOSSLESS",
                    "manifestMimeType": "application/dash+xml", "manifest": b64(dash_files::mpd(&base).as_bytes()),
                    "bitDepth": 24, "sampleRate": 96000, "trackReplayGain": -8.1, "trackPeakAmplitude": 0.99}),
                ("80", _) => json!({"trackId": 80, "assetPresentation": "FULL", "audioQuality": "LOSSLESS",
                    "manifestMimeType": "application/vnd.tidal.bts", "manifest": bts("flac", "audio/flac", "OLD_AES")}),
                ("81", _) => {
                    return (StatusCode::UNAUTHORIZED, r#"{"status":401,"subStatus":4005,"userMessage":"Asset is not ready for playback"}"#).into_response();
                }
                ("82", _) => json!({"trackId": 82, "assetPresentation": "PREVIEW", "audioQuality": "LOW",
                    "manifestMimeType": "application/vnd.tidal.bts", "manifest": bts("mp4a.40.2", "audio/mp4", "NONE")}),
                (_, "HIGH") => json!({"trackId": 77, "assetPresentation": "FULL", "audioQuality": "HIGH",
                    "manifestMimeType": "application/vnd.tidal.bts", "manifest": bts("mp4a.40.2", "audio/mp4", "NONE"), "sampleRate": 44100}),
                _ => json!({"trackId": 77, "assetPresentation": "FULL", "audioQuality": "LOSSLESS",
                    "manifestMimeType": "application/vnd.tidal.bts", "manifest": bts("flac", "audio/flac", "NONE"),
                    "bitDepth": 16, "sampleRate": 44100, "trackReplayGain": -7.2, "trackPeakAmplitude": 0.98}),
            }
        }
        ("GET", p) if p.strip_prefix("v2/") == Some(HOME_FEED) => json!({"items": [
            {"type": "HORIZONTAL_LIST", "moduleId": "m1", "title": "Suggested new albums",
             "items": [{"type": "ALBUM", "data": album_json()}]},
            {"type": "SHORTCUT_LIST", "title": "Your mixes",
             "items": [{"type": "MIX", "data": {"id": "m1", "titleTextInfo": {"text": "My Daily Discovery"}}}]}
        ]}),
        ("GET", "v1/pages/explore") => json!({"rows": [
            {"modules": [{"type": "PAGE_LINKS_CLOUD", "title": "Genres", "pagedList": {"items": [{"title": "Jazz", "apiPath": "pages/genre_jazz"}]}}]},
            {"modules": [{"type": "ALBUM_LIST", "title": "New", "pagedList": {"totalNumberOfItems": 3, "dataApiPath": "pages/data/new-albums", "items": [album_json()]}}]}
        ]}),
        ("GET", "v1/pages/data/new-albums") => list(vec![album_json(), json!({"id": 6, "title": "Let It Be"}), json!({"id": 7, "title": "Help!"})]),
        ("GET", "v1/pages/genre_jazz") => json!({"rows": [{"modules": [{"type": "PLAYLIST_LIST", "title": "Jazz classics",
            "pagedList": {"items": [playlist_json("p-theirs", 9, 50)]}}]}]}),
        ("GET", "v1/pages/my_collection_my_mixes") => json!({"rows": [{"modules": [{"type": "MIX_LIST", "title": "My mixes", "pagedList": {"items": [
            {"id": "m1", "title": "My Mix 1", "subTitle": "The Beatles, The Kinks", "mixType": "DISCOVERY_MIX",
             "images": {"LARGE": {"url": "https://resources.tidal.com/images/m1/1500x1500.jpg"}}}]}}]}]}),
        ("GET", "v1/mixes/m1/items") => list(vec![json!({"type": "track", "item": track_json(90, "Here Comes the Sun")})]),
        // Playlists.
        ("GET", "v1/playlists/p-theirs") => playlist_json("p-theirs", 9, 50),
        ("GET", "v1/playlists/p-mine") => {
            let n = m.mine.len();
            return ([(header::ETAG, etag)], Json(playlist_json("p-mine", USER, n))).into_response();
        }
        ("GET", "v1/playlists/p-mine/items") => {
            let items: Vec<Value> = m.mine.iter().map(|id| json!({"type": "track", "cut": null, "item": track_json(*id, "T")})).collect();
            list(items)
        }
        ("PUT", "v2/my-collection/playlists/folders/create-playlist") => {
            assert_eq!(q.get("folderId").map(String::as_str), Some("root"));
            json!({"trn": "trn:playlist:p-new", "data": {"uuid": "p-new", "title": q["name"], "numberOfTracks": 0, "creator": {"id": USER}, "type": "USER"}})
        }
        ("PUT", "v2/playlists/p-new/set-public") => Value::Null,
        (_, p) if p.starts_with("v1/playlists/p-mine") => {
            if if_none_match.as_deref() != Some(etag.as_str()) {
                return (StatusCode::PRECONDITION_FAILED, r#"{"status":412,"userMessage":"Precondition failed"}"#).into_response();
            }
            if m.conflict_once {
                m.conflict_once = false;
                m.version += 1;
                return (StatusCode::PRECONDITION_FAILED, r#"{"status":412,"userMessage":"Precondition failed"}"#).into_response();
            }
            m.version += 1;
            let rest = p.trim_start_matches("v1/playlists/p-mine");
            match (method.as_str(), rest) {
                ("POST", "") | ("DELETE", "") => {}
                ("POST", "/items") => {
                    let to: usize = form["toIndex"].parse().unwrap();
                    assert_eq!(to, m.mine.len(), "appended");
                    let ids: Vec<u64> = form["trackIds"].split(',').map(|t| t.parse().unwrap()).collect();
                    m.mine.extend(ids);
                }
                ("DELETE", r) => {
                    let mut idx: Vec<usize> = r.trim_start_matches("/items/").split(',').map(|i| i.parse().unwrap()).collect();
                    idx.sort_unstable_by(|a, b| b.cmp(a));
                    for i in idx {
                        m.mine.remove(i);
                    }
                }
                ("POST", r) => {
                    let from: usize = r.trim_start_matches("/items/").parse().unwrap();
                    let to: usize = form["toIndex"].parse().unwrap();
                    let t = m.mine.remove(from);
                    m.mine.insert(to, t);
                }
                _ => return (StatusCode::NOT_FOUND, "no such endpoint").into_response(),
            }
            Value::Null
        }
        _ => return (StatusCode::NOT_FOUND, r#"{"status":404,"subStatus":2001,"userMessage":"The requested resource could not be found"}"#).into_response(),
    };
    if body.is_null() {
        return StatusCode::OK.into_response();
    }
    Json(body).into_response()
}

async fn start_mock() -> (String, Shared) {
    let state = Shared::default();
    state.lock().unwrap().mine = vec![77, 78];
    let app = Router::new().route("/{*path}", any(mock)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, state)
}

struct Host {
    child: Child,
    /// The plugin's stderr (its log), kept to check what it writes there.
    log: PathBuf,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    notifications: Vec<Value>,
}

impl Host {
    fn spawn(base: &str) -> Self {
        let log = std::env::temp_dir().join(format!("tidalsrc-plugin-log-{}", uuid::Uuid::new_v4()));
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_tidalsrc"))
            .args(["plugin", "--api-base", base, "--auth-base", base, "--login-base", base])
            .args(["--client-id", "device-test", "--pkce-client-id", "pkce-test"])
            .env("RUST_LOG", "tidalsrc=debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(&log).unwrap())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Host { child, log, stdin, lines, next_id: 0, notifications: vec![] }
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("plugin answered in time")
            .unwrap()
            .expect("plugin still running");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    /// Whole response (`result` or `error`) to one request.
    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        loop {
            let msg = self.read().await;
            if msg.get("id") == Some(&json!(id)) {
                assert_eq!(msg["jsonrpc"], "2.0");
                return msg;
            }
            assert!(msg.get("method").is_some(), "unexpected message {msg}");
            self.notifications.push(msg);
        }
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg.get("error").is_none(), "{method} failed: {msg}");
        msg["result"].clone()
    }

    async fn err(&mut self, method: &str, params: Value) -> Value {
        let msg = self.call(method, params).await;
        assert!(msg["error"].is_object(), "{method} should fail: {msg}");
        msg["error"].clone()
    }

    async fn err_code(&mut self, method: &str, params: Value) -> i64 {
        self.err(method, params).await["code"].as_i64().unwrap()
    }

    async fn notification(&mut self, method: &str) -> Value {
        if let Some(i) = self.notifications.iter().position(|n| n["method"] == method) {
            return self.notifications.remove(i);
        }
        loop {
            let msg = self.read().await;
            if msg["method"] == method {
                return msg;
            }
            self.notifications.push(msg);
        }
    }

    async fn initialize(&mut self, data_dir: &Path, output: Value, settings: Value) -> Value {
        self.ok(
            "initialize",
            json!({"protocol": 1, "host": {"name": "fake", "version": "0"}, "data_dir": data_dir,
                   "cache_dir": data_dir.join("cache"), "locale": "fr-FR", "output": output, "settings": settings}),
        )
        .await
    }

    async fn shutdown(mut self) {
        assert_eq!(self.ok("shutdown", Value::Null).await, Value::Null);
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.unwrap().unwrap();
        assert!(status.success());
    }
}

fn data_dir(signed_in: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tidalsrc-plugin-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    if signed_in {
        let path = dir.join("account.json");
        let creds = json!({"kind": "device", "user_id": USER, "country_code": "FR", "display_name": "Ada",
                           "access_token": "acc-1", "refresh_token": "ref-1", "expires_at": 4_000_000_000u64});
        std::fs::write(&path, creds.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    dir
}

fn dac(max_rate: u32, max_bits: u32, rates: &[u32]) -> Value {
    json!({"device": "hw:1,0", "bit_perfect": true, "max_rate": max_rate, "max_bits": max_bits, "rates": rates})
}

fn calls(mock: &Shared, path: &str) -> Vec<Call> {
    mock.lock().unwrap().calls.iter().filter(|c| c.path == path).cloned().collect()
}

/// Nothing that identifies the account or opens it in the log.
fn assert_clean_log(log: &str) {
    for secret in ["acc-1", "acc-2", "acc-pkce", "ref-1", "ref-pkce", EMAIL, "users/1/", "token=t", "hmac=x"] {
        assert!(!log.contains(secret), "{secret:?} in the log:\n{log}");
    }
}

#[tokio::test]
async fn device_code_sign_in() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(false);
    let mut host = Host::spawn(&base);
    let init = host.initialize(&dir, dac(192_000, 24, &[]), json!({})).await;
    assert_eq!(init["plugin"], json!({"id": "tidal", "name": "TIDAL", "version": env!("CARGO_PKG_VERSION")}));
    let caps = &init["capabilities"];
    for cap in ["auth", "browse", "search", "resolve", "favorites", "library", "lyrics", "radio", "details", "playlist_edit"] {
        assert_eq!(caps[cap], true, "{cap}");
    }
    assert_eq!((caps["reporting"].as_bool(), caps["remote_control"].as_bool()), (Some(false), Some(false)));
    assert_eq!(init["settings"][0]["key"], "quality");
    assert_eq!(init["settings"][1]["label"], "Méthode de connexion", "labels in the host's language");

    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert_eq!(host.err_code("browse.list", json!({"ref": "fav/tracks"})).await, -32001);
    assert_eq!(host.err_code("auth.complete", json!({"input": "x"})).await, -32602, "nothing begun");

    let begin = host.ok("auth.begin", json!({})).await;
    assert_eq!(begin["url"], "https://link.tidal.com/ABCDE");
    assert_eq!(begin["expects_input"], false);
    assert!(begin["instructions"].as_str().unwrap().contains("ABCDE"));
    assert_eq!(calls(&mock, "v1/oauth2/device_authorization")[0].form["client_id"], "device-test", "client id from the command line");

    // The plugin polls (pending, then confirmed) and says so by itself.
    let changed = host.notification("auth.changed").await;
    assert_eq!(changed["params"]["state"], "signed_in");
    assert_eq!(changed["params"]["account"], json!({"display_name": "Ada", "detail": "HiFi Plus · qualité CD au mieux (connexion par code)"}));
    let polls = calls(&mock, "v1/oauth2/token");
    assert_eq!(polls.len(), 2);
    assert_eq!(polls[1].form["client_secret"], tidal_device_secret());

    let saved = dir.join("account.json");
    assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&saved).unwrap()).unwrap();
    assert_eq!((stored["kind"].as_str(), stored["refresh_token"].as_str(), stored["country_code"].as_str()), (Some("device"), Some("ref-1"), Some("FR")));
    assert!(!stored.to_string().contains(EMAIL), "the e-mail address is not kept");

    let status = host.ok("auth.status", json!({})).await;
    assert_eq!(status["state"], "signed_in");
    let fav = host.ok("browse.list", json!({"ref": "fav/tracks"})).await;
    assert_eq!(fav["items"][0]["ref"], "track/77");

    assert_eq!(host.ok("auth.sign_out", json!({})).await, json!({"state": "signed_out"}));
    assert!(!saved.exists());
    assert_eq!(host.ok("auth.status", json!({})).await, json!({"state": "signed_out"}));
    assert_clean_log(&host.logs());
    host.shutdown().await;
}

fn tidal_device_secret() -> &'static str {
    // Not overridden on the command line: the built-in one.
    "1Nn9AfDAjxrgJFJbKNWLeAyKGVGmINuXPPLHVXAvxAg="
}

#[tokio::test]
async fn browser_sign_in() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(false);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, dac(192_000, 24, &[]), json!({"sign_in": "browser"})).await;
    let begin = host.ok("auth.begin", json!({})).await;
    assert_eq!(begin["expects_input"], true);
    let url = begin["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{base}/authorize?response_type=code&redirect_uri=https%3A%2F%2Ftidal.com%2Fandroid%2Flogin%2Fauth")));
    assert!(url.contains("client_id=pkce-test"));
    let challenge = url.split("code_challenge=").nth(1).unwrap().split('&').next().unwrap().to_string();

    assert_eq!(host.err_code("auth.complete", json!({"input": "https://tidal.com/android/login/auth?error=x"})).await, -32602);
    let status = host.ok("auth.complete", json!({"input": "https://tidal.com/android/login/auth?code=c0de&state=na"})).await;
    assert_eq!(status["state"], "signed_in");
    assert_eq!(status["account"], json!({"display_name": "Ada", "detail": "HiFi Plus"}), "hi-res sign-in: no cap noted");
    let exchange = calls(&mock, "v1/oauth2/token").pop().unwrap();
    let verifier = &exchange.form["code_verifier"];
    let hashed = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()));
    assert_eq!(hashed, challenge, "the verifier matches the challenge of the sign-in page");
    assert_eq!(exchange.form["client_id"], "pkce-test");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("account.json")).unwrap()).unwrap();
    assert_eq!(stored["kind"], "pkce");
    assert_clean_log(&host.logs());
    host.shutdown().await;
}

#[tokio::test]
async fn catalogue_library_and_favourites() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(true);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, dac(192_000, 24, &[]), json!({})).await;

    let root = host.ok("browse.root", json!({})).await;
    let refs = |v: &Value| -> Vec<String> { v.as_array().unwrap().iter().map(|i| i["ref"].as_str().unwrap().to_string()).collect() };
    assert_eq!(refs(&root["sections"]), ["home", "explore", "mixes", "fav", "my/playlists"]);
    assert_eq!(root["sections"][0]["title"], "Accueil");
    assert_eq!(refs(&root["home"]), ["mixes", "shelf/0/home", "shelf/1/home"], "the account's mixes, then Home's shelves");
    assert_eq!(root["home"][1]["title"], "Suggested new albums");
    let shelf = host.ok("browse.list", json!({"ref": "shelf/0/home"})).await;
    assert_eq!(shelf["items"][0]["ref"], "album/5");
    let shelf = host.ok("browse.list", json!({"ref": "shelf/1/home"})).await;
    assert_eq!((shelf["items"][0]["ref"].as_str(), shelf["items"][0]["title"].as_str()), (Some("mix/m1"), Some("My Daily Discovery")));
    let home_call = calls(&mock, &format!("v2/{HOME_FEED}")).pop().unwrap();
    assert_eq!(home_call.query["locale"], "fr_FR");

    // Explore: a shelf of links to pages, and a shelf read whole from its data path.
    let explore = host.ok("browse.list", json!({"ref": "explore"})).await;
    assert_eq!(refs(&explore["items"]), ["shelf/0/pages/explore", "shelf/1/pages/explore"]);
    let genres = host.ok("browse.list", json!({"ref": "shelf/0/pages/explore"})).await;
    assert_eq!(genres["items"][0], json!({"ref": "page/pages/genre_jazz", "kind": "folder", "title": "Jazz", "playable": false, "browsable": true}));
    let jazz = host.ok("browse.list", json!({"ref": "page/pages/genre_jazz"})).await;
    assert_eq!(jazz["items"][0]["title"], "Jazz classics");
    let classics = host.ok("browse.list", json!({"ref": jazz["items"][0]["ref"]})).await;
    assert_eq!(classics["items"][0]["ref"], "playlist/p-theirs");
    let new = host.ok("browse.list", json!({"ref": "shelf/1/pages/explore", "offset": 0, "limit": 2})).await;
    assert_eq!((refs(&new["items"]), new["total"].as_u64(), new["has_more"].as_bool()), (vec!["album/5".into(), "album/6".into()], Some(3), Some(true)));

    // Mixes.
    let mixes = host.ok("browse.list", json!({"ref": "mixes"})).await;
    assert_eq!(mixes["items"][0], json!({"ref": "mix/m1", "kind": "playlist", "title": "My Mix 1", "subtitle": "The Beatles, The Kinks",
        "art": "https://resources.tidal.com/images/m1/1500x1500.jpg", "playable": false, "browsable": true}));
    assert_eq!(host.ok("browse.list", json!({"ref": "mix/m1"})).await["items"][0]["ref"], "track/90");
    assert_eq!(host.ok("item.get", json!({"ref": "mix/m1"})).await["title"], "My Mix 1");

    // Library lists.
    let albums = host.ok("library.albums", json!({"offset": 0, "limit": 200})).await;
    let a = &albums["items"][0];
    assert_eq!((a["ref"].as_str(), a["artist"].as_str(), a["year"].as_u64(), a["track_count"].as_u64()), (Some("album/5"), Some("The Beatles"), Some(1969), Some(2)));
    assert_eq!(a["art"], "https://resources.tidal.com/images/aa11/bb22/640x640.jpg");
    assert_eq!(a["format"], json!({"bits": 24, "codec": "flac"}));
    assert_eq!(albums["has_more"], false);
    let fav_call = calls(&mock, "v1/users/1/favorites/albums").pop().unwrap();
    assert_eq!((fav_call.query["limit"].as_str(), fav_call.query["order"].as_str()), ("100", "DATE"), "pages of 100, newest first");
    let artists = host.ok("library.artists", json!({})).await;
    assert_eq!(artists["items"][0]["art"], "https://resources.tidal.com/images/ee55/ff66/750x750.jpg");
    assert_eq!(host.ok("library.tracks", json!({})).await["items"][0]["ref"], "track/77");
    let playlists = host.ok("library.playlists", json!({})).await;
    assert_eq!(refs(&playlists["items"]), ["playlist/p-mine", "playlist/p-theirs"]);
    assert_eq!((playlists["items"][0]["editable"].as_bool(), playlists["items"][1]["editable"].as_bool()), (Some(true), Some(false)));
    assert_eq!(playlists["items"][1]["subtitle"], "TIDAL");
    assert_eq!(host.err_code("library.genres", json!({})).await, -32601);
    let fav = host.ok("browse.list", json!({"ref": "fav"})).await;
    assert_eq!(refs(&fav["items"]), ["fav/tracks", "fav/albums", "fav/artists", "fav/playlists"]);
    assert_eq!(host.ok("browse.list", json!({"ref": "my/playlists"})).await["items"][0]["title"], "Soir");

    // Search.
    let all = host.ok("search", json!({"query": "beatles"})).await;
    let kinds: Vec<&str> = all["groups"].as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["track", "album", "artist", "playlist"]);
    let found = host.ok("search", json!({"query": "beatles", "kinds": ["album"], "offset": 0, "limit": 10})).await;
    assert_eq!(found["groups"][0]["items"][0]["subtitle"], "The Beatles · 1969");
    assert_eq!(calls(&mock, "v1/search").pop().unwrap().query["types"], "ALBUMS");
    assert_eq!(host.err_code("search", json!({"query": "  "})).await, -32602);

    // Albums, artists.
    let tracks = host.ok("browse.list", json!({"ref": "album/5", "offset": 0, "limit": 50})).await;
    assert_eq!(tracks["total"], 2);
    assert_eq!((tracks["items"][1]["album"].as_str(), tracks["items"][1]["artist"].as_str()), (Some("Abbey Road"), Some("The Beatles")));
    assert_eq!((tracks["items"][1]["album_ref"].as_str(), tracks["items"][1]["artist_ref"].as_str()), (Some("album/5"), Some("artist/8")));
    assert_eq!(tracks["items"][0]["actions"][0], json!({"id": "radio", "label": "Radio de ce titre", "ref": "radio/track/77", "kind": "play"}));
    let discography = host.ok("browse.list", json!({"ref": "artist/8"})).await;
    assert_eq!((refs(&discography["items"]), discography["total"].as_u64()), (vec!["album/5".into(), "album/6".into()], Some(2)), "albums, then singles");

    // Favourite state: ids read in the background at start-up.
    let mut fav = Value::Null;
    for _ in 0..100 {
        fav = host.ok("item.get", json!({"ref": "track/77"})).await["favorite"].clone();
        if !fav.is_null() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(fav, true);
    assert_eq!(host.ok("item.get", json!({"ref": "album/5"})).await["favorite"], true);
    assert_eq!(host.ok("favorites.set", json!({"ref": "album/5", "on": false})).await, Value::Null);
    assert_eq!(calls(&mock, "v1/users/1/favorites/albums/5")[0].method, "DELETE");
    assert_eq!(host.ok("item.get", json!({"ref": "album/5"})).await["favorite"], false, "updated at once");
    host.ok("favorites.set", json!({"ref": "album/5", "on": true})).await;
    let added = calls(&mock, "v1/users/1/favorites/albums").into_iter().find(|c| c.method == "POST").unwrap();
    assert_eq!(added.form["albumId"], "5");
    assert_eq!(host.err_code("favorites.set", json!({"ref": "mix/m1", "on": true})).await, -32602);

    // Radio.
    let radio = host.ok("browse.list", json!({"ref": "radio/track/77"})).await;
    assert_eq!(radio["total"], 3);
    assert_eq!(host.ok("item.get", json!({"ref": "radio/track/77"})).await["title"], "Radio : Come Together");
    let next = host.ok("radio.next", json!({"seed": "track/77", "exclude": [], "limit": 10})).await;
    assert_eq!(refs(&next["items"]), ["track/90"], "not the seed, not an unplayable track");
    let next = host.ok("radio.next", json!({"seed": "artist/8", "exclude": ["track/90"]})).await;
    assert_eq!(refs(&next["items"]), ["track/77"]);
    let next = host.ok("radio.next", json!({"seed": "album/5", "exclude": []})).await;
    assert_eq!(refs(&next["items"]), ["track/77", "track/90"], "an album's radio is its first track's");
    assert_eq!(host.err_code("radio.next", json!({"seed": "playlist/p-mine"})).await, -32602);

    // Details.
    let d = host.ok("item.details", json!({"ref": "artist/8"})).await;
    assert_eq!(d["biography"], json!({"text": "Formed in Liverpool.", "source": "TiVo"}));
    let shelves: Vec<&str> = d["related"].as_array().unwrap().iter().map(|s| s["title"].as_str().unwrap()).collect();
    assert_eq!(shelves, ["Titres phares"], "no similar artists (404): left out");
    let d = host.ok("item.details", json!({"ref": "album/5"})).await;
    assert_eq!(d["biography"]["text"], "A Beatles classic.\nRecorded in 1969.");
    assert_eq!(d["facts"], json!([
        {"label": "Sortie", "value": "26/09/1969"},
        {"label": "Qualité", "value": "Haute résolution (FLAC 24 bits)"},
        {"label": "©", "value": "© 1969 Apple Corps"},
        {"label": "Producer", "value": "George Martin"},
        {"label": "Composer", "value": "George Harrison"}
    ]));
    let d = host.ok("item.details", json!({"ref": "track/77"})).await;
    assert_eq!(d["facts"], json!([{"label": "Composer", "value": "John Lennon, Paul McCartney"}]));
    assert_eq!(host.ok("item.details", json!({"ref": "home"})).await, json!({}));

    // Lyrics.
    let lyrics = host.ok("lyrics.get", json!({"ref": "track/77"})).await;
    assert_eq!(lyrics, json!({"synced": [{"time_ms": 3100, "text": "Here come old flat top"}], "plain": "Here come old flat top"}));
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "track/78"})).await, -32002);
    assert_eq!(host.err_code("lyrics.get", json!({"ref": "track/78"})).await, -32002);
    assert_eq!(calls(&mock, "v1/tracks/78/lyrics").len(), 1, "a missing one is remembered");

    // Settings and language follow the host.
    host.send(json!({"jsonrpc": "2.0", "method": "locale.changed", "params": {"locale": "en-GB"}})).await;
    let declared = host.notification("settings.declared").await;
    assert_eq!(declared["params"]["settings"][0]["label"], "Best quality");
    assert_eq!(host.ok("browse.root", json!({})).await["sections"][0]["title"], "Home");
    assert_eq!(calls(&mock, &format!("v2/{HOME_FEED}")).pop().unwrap().query["locale"], "en_GB", "pages read again in the new language");

    assert_eq!(host.err_code("browse.list", json!({"ref": "nonsense"})).await, -32002);
    assert_eq!(host.err_code("browse.list", json!({"ref": "album/404"})).await, -32002);
    assert_eq!(host.err_code("frobnicate", json!({})).await, -32601);
    assert_clean_log(&host.logs());
    host.shutdown().await;
}

#[tokio::test]
async fn playlist_edits() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(true);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, dac(192_000, 24, &[]), json!({})).await;

    let created = host.ok("playlists.create", json!({"name": "  Nouvelle  ", "description": "d", "public": true})).await;
    assert_eq!((created["ref"].as_str(), created["title"].as_str(), created["editable"].as_bool()), (Some("playlist/p-new"), Some("Nouvelle"), Some(true)));
    assert_eq!(calls(&mock, "v2/playlists/p-new/set-public").len(), 1);
    assert_eq!(host.err_code("playlists.create", json!({"name": " "})).await, -32602);

    let entries = host.ok("browse.list", json!({"ref": "playlist/p-mine"})).await;
    let ids: Vec<&str> = entries["items"].as_array().unwrap().iter().map(|t| t["entry_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["0-77", "1-78"]);

    // Rename: the playlist changed meanwhile (412), read again and retried.
    mock.lock().unwrap().conflict_once = true;
    assert_eq!(host.ok("playlists.rename", json!({"ref": "playlist/p-mine", "name": "Nuit"})).await, Value::Null);
    let renames: Vec<Call> = calls(&mock, "v1/playlists/p-mine").into_iter().filter(|c| c.method == "POST").collect();
    assert_eq!(renames.len(), 2);
    assert_eq!((renames[0].if_none_match.as_deref(), renames[1].if_none_match.as_deref()), (Some("\"0\""), Some("\"1\"")));
    assert_eq!((renames[1].form["title"].as_str(), renames[1].form["description"].as_str()), ("Nuit", "calm"), "description kept");

    host.ok("playlists.add", json!({"ref": "playlist/p-mine", "items": ["track/90", "track/91"]})).await;
    assert_eq!(mock.lock().unwrap().mine, [77, 78, 90, 91]);
    assert_eq!(host.err_code("playlists.add", json!({"ref": "playlist/p-mine", "items": ["album/5"]})).await, -32602);

    // [77, 78, 90, 91]: 77 to 2 → [78, 90, 77, 91].
    host.ok("playlists.move", json!({"ref": "playlist/p-mine", "entry": "0-77", "to": 2})).await;
    assert_eq!(mock.lock().unwrap().mine, [78, 90, 77, 91]);
    // Past the end: last.
    host.ok("playlists.move", json!({"ref": "playlist/p-mine", "entry": "0-78", "to": 99})).await;
    assert_eq!(mock.lock().unwrap().mine, [90, 77, 91, 78]);
    // A stale entry (the track moved since the host read it).
    assert_eq!(host.err_code("playlists.move", json!({"ref": "playlist/p-mine", "entry": "0-78", "to": 1})).await, -32002);

    host.ok("playlists.remove", json!({"ref": "playlist/p-mine", "entries": ["1-77", "3-78"]})).await;
    assert_eq!(mock.lock().unwrap().mine, [90, 91]);
    let removal = calls(&mock, "v1/playlists/p-mine/items/1,3");
    assert_eq!(removal[0].method, "DELETE");
    assert_eq!(host.err_code("playlists.remove", json!({"ref": "playlist/p-mine", "entries": ["junk"]})).await, -32602);

    // Someone else's playlist is never edited.
    for (method, params) in [
        ("playlists.rename", json!({"ref": "playlist/p-theirs", "name": "x"})),
        ("playlists.delete", json!({"ref": "playlist/p-theirs"})),
        ("playlists.add", json!({"ref": "playlist/p-theirs", "items": ["track/77"]})),
    ] {
        assert_eq!(host.err_code(method, params).await, -32602, "{method}");
    }
    assert!(calls(&mock, "v1/playlists/p-theirs").iter().all(|c| c.method == "GET"));

    host.ok("playlists.delete", json!({"ref": "playlist/p-mine"})).await;
    assert!(calls(&mock, "v1/playlists/p-mine").iter().any(|c| c.method == "DELETE"));
    host.shutdown().await;
}

#[tokio::test]
async fn resolve_direct_and_proxied() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(true);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, dac(192_000, 24, &[44_100, 48_000, 88_200, 96_000, 176_400, 192_000]), json!({})).await;

    // Hi-res: DASH segments rebuilt into one FLAC file by the relay.
    let r = host.ok("track.resolve", json!({"ref": "track/79", "purpose": "play"})).await;
    assert_eq!(r["delivery"], "proxied");
    assert_eq!(r["format"], json!({"sample_rate": 96000, "bits": 24, "channels": 2, "codec": "flac"}));
    assert_eq!(r["replaygain"], json!({"track_gain": -8.1, "track_peak": 0.99}));
    assert_eq!(r["duration_ms"], 259_000);
    assert!(r["expires_at"].as_i64().unwrap() > 0);
    let url = r["url"].as_str().unwrap();
    assert!(url.starts_with("http://127.0.0.1:") && url.ends_with("/79.flac"));
    let probes = calls(&mock, "dash/79/1.mp4").len() + calls(&mock, "dash/79/2.mp4").len();
    assert_eq!(probes, 2, "each segment measured once at resolution");

    let expected = dash_files::flac();
    let http = reqwest::Client::new();
    let resp = http.get(url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"].to_str().unwrap(), expected.len().to_string());
    assert_eq!(resp.headers()["accept-ranges"], "bytes");
    assert_eq!(resp.headers()["content-type"], "audio/flac");
    assert!(resp.bytes().await.unwrap() == expected, "the original frames behind the original header");

    // A range across the header and both segments.
    let resp = http.get(url).header("Range", format!("bytes=20-{}", expected.len() - 3)).send().await.unwrap();
    assert_eq!(resp.status(), 206);
    assert_eq!(resp.headers()["content-range"].to_str().unwrap(), format!("bytes 20-{}/{}", expected.len() - 3, expected.len()));
    assert!(resp.bytes().await.unwrap() == expected[20..expected.len() - 2]);
    let resp = http.head(url).send().await.unwrap();
    assert_eq!(resp.headers()["content-length"].to_str().unwrap(), expected.len().to_string());
    let gone = url.replace(&url[url.len() - 40..url.len() - 8], "0123456789abcdef0123456789abcdef");
    assert_eq!(http.get(&gone).send().await.unwrap().status(), 410, "unknown token");

    // CD quality: the file's own URL.
    let r = host.ok("track.resolve", json!({"ref": "track/77", "purpose": "preload"})).await;
    assert_eq!(r["delivery"], "direct");
    assert_eq!(r["url"], format!("{base}/cdn/77.flac?token=exp=4000000000~hmac=x"));
    assert_eq!(r["format"], json!({"sample_rate": 44100, "bits": 16, "channels": 2, "codec": "flac"}));
    assert_eq!(r["expires_at"], 4_000_000_000i64 - 60);
    assert_eq!(r["replaygain"], json!({"track_gain": -7.2, "track_peak": 0.98}));

    // A 16-bit DAC: hi-res is not asked; the setting caps it the same way.
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": {"output": dac(48_000, 16, &[44_100, 48_000])}})).await;
    let r = host.ok("track.resolve", json!({"ref": "track/79"})).await;
    assert_eq!((r["delivery"].as_str(), r["format"]["bits"].as_u64()), (Some("direct"), Some(16)));
    let asked: Vec<String> = calls(&mock, "v1/tracks/79/playbackinfo").iter().map(|c| c.query["audioquality"].clone()).collect();
    assert_eq!(asked, ["HI_RES_LOSSLESS", "LOSSLESS"]);

    // A 96 kHz DAC with no 44.1 kHz: nothing fits but AAC… whose rate is 44.1 kHz too.
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": dac(96_000, 24, &[48_000, 96_000])})).await;
    let e = host.err("track.resolve", json!({"ref": "track/77"})).await;
    assert_eq!(e["code"], -32003);
    assert!(e["message"].as_str().unwrap().contains("natively"));

    host.send(json!({"jsonrpc": "2.0", "method": "settings.changed", "params": {"settings": {"quality": "high", "sign_in": "device"}}})).await;
    host.send(json!({"jsonrpc": "2.0", "method": "output.changed", "params": dac(192_000, 24, &[])})).await;
    let r = host.ok("track.resolve", json!({"ref": "track/77"})).await;
    assert_eq!(r["format"], json!({"sample_rate": 44100, "channels": 2, "codec": "aac"}), "capped at AAC by the setting");

    // Refusals, with the reason.
    let e = host.err("track.resolve", json!({"ref": "track/80"})).await;
    assert_eq!(e["code"], -32003);
    assert!(e["message"].as_str().unwrap().contains("OLD_AES"));
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/81"})).await, -32003, "401 about the track, not the token");
    assert_eq!(host.err_code("track.resolve", json!({"ref": "track/82"})).await, -32003, "preview only");
    assert_eq!(host.err_code("track.resolve", json!({"ref": "album/5"})).await, -32002);
    assert!(host.notifications.iter().all(|n| n["method"] != "auth.changed"), "no sign-in trouble reported");
    assert_clean_log(&host.logs());
    host.shutdown().await;
}

#[tokio::test]
async fn expired_tokens_rate_limits_and_revocation() {
    let (base, mock) = start_mock().await;
    let dir = data_dir(true);
    let mut host = Host::spawn(&base);
    host.initialize(&dir, dac(192_000, 24, &[]), json!({})).await;
    assert_eq!(host.ok("auth.status", json!({})).await["state"], "signed_in");

    // The access token expires: refreshed once, the request retried, the new tokens saved.
    mock.lock().unwrap().stale.insert("acc-1".into());
    let fav = host.ok("browse.list", json!({"ref": "fav/albums"})).await;
    assert_eq!(fav["items"][0]["ref"], "album/5");
    let refreshes: Vec<Call> = calls(&mock, "v1/oauth2/token");
    assert_eq!(refreshes.len(), 1);
    assert_eq!((refreshes[0].form["grant_type"].as_str(), refreshes[0].form["refresh_token"].as_str()), ("refresh_token", "ref-1"));
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("account.json")).unwrap()).unwrap();
    assert_eq!((stored["access_token"].as_str(), stored["refresh_token"].as_str()), (Some("acc-2"), Some("ref-1")));

    // Rate limited: the host gets `retry_after`.
    let e = host.err("search", json!({"query": "slow"})).await;
    assert_eq!((e["code"].as_i64(), e["data"]["retry_after"].as_u64()), (Some(-32004), Some(7)));

    // The refresh token revoked: sign-in needed, said once.
    {
        let mut m = mock.lock().unwrap();
        m.stale.insert("acc-2".into());
        m.revoked = true;
    }
    assert_eq!(host.err_code("library.tracks", json!({})).await, -32001);
    let changed = host.notification("auth.changed").await;
    assert_eq!(changed["params"]["state"], "expired");
    assert_eq!(host.err_code("library.albums", json!({})).await, -32001);
    assert_eq!(host.ok("auth.status", json!({})).await["state"], "expired");
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.send(json!({"jsonrpc": "2.0", "id": 999, "method": "item.get", "params": {"ref": "track/77"}})).await;
    let mut expired = 0;
    loop {
        let msg = host.read().await;
        if msg["id"] == 999 {
            break;
        }
        if msg["method"] == "auth.changed" {
            expired += 1;
        }
    }
    assert_eq!(expired, 0, "auth.changed sent once");
    assert_clean_log(&host.logs());
    host.shutdown().await;
}
