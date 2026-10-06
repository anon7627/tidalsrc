# tidalsrc: how the plugin works

tidalsrc is a source plugin (protocol v1): a process started by the host
player, speaking JSON-RPC 2.0 over stdin/stdout, one JSON message per line.
Logs go to stderr. The plugin never touches the signal: it hands the host
stream URLs that the host plays itself, or, for hi-res, serves the original
FLAC file from a local relay.

```
host ──(JSON-RPC over stdin/stdout)──► tidalsrc plugin
                                         ├─ auth.*        → auth.tidal.com (device code, PKCE)
                                         ├─ browse.*      → favourites, playlists, mixes, Home, Explore
                                         ├─ search        → v1/search
                                         ├─ item.*        → v1 tracks/albums/artists/playlists
                                         └─ track.resolve → v1/tracks/<id>/playbackinfo
host ──(HTTP)──► TIDAL CDN                    (BTS: the file itself)
host ──(HTTP)──► 127.0.0.1 relay ──► TIDAL CDN (DASH: segments → one FLAC file)
```

Advertised capabilities: `auth`, `browse`, `search`, `resolve`,
`favorites`, `library`, `lyrics`, `radio`, `details`, `playlist_edit`.
`reporting` and `remote_control` are `false`: TIDAL's play-report endpoint
is undocumented and changes with its apps, and there is no open
implementation of TIDAL Connect.

The plugin never writes the account's e-mail address, user id or tokens to
its log (stderr), nor the e-mail address to the account it reports to the
host: hosts keep both in their logs and diagnostic reports. API paths are
logged with the user id masked (`users/…/favorites`), and CDN URLs (signed)
not at all.

## Start-up

`initialize` provides:
- `data_dir`: `account.json`;
- `output`: what the output plays natively (see Resolution);
- `locale`: the host's interface language, for every label the plugin
  writes (French, else English) and for TIDAL's pages (`locale=fr_FR`). On
  `locale.changed {locale}` the plugin switches, forgets the pages it read
  and declares its settings again (`settings.declared`);
- `settings`: the values stored for the plugin's settings (below).

`output.changed` updates the output in use; the object is accepted on its
own or wrapped in `{output: …}`.

## Settings

Declared in the `initialize` result (`settings`), labelled in the host's
language. Both apply without restarting the plugin (`settings.changed`).

| Key | Type | Default | Effect |
|---|---|---|---|
| `quality` | choice `hires` / `lossless` / `high` | `hires` | Best quality asked for (below) |
| `sign_in` | choice `device` / `browser` | `device` | How `auth.begin` signs in; takes effect at the next sign-in |

Values of unknown keys, or of the wrong type, are ignored; missing keys
take their default.

## Sign-in (`auth.*`)

Two flows, both on TIDAL's own pages:

**Device code** (`sign_in = device`):
1. `POST auth.tidal.com/v1/oauth2/device_authorization` (`client_id`,
   `scope=r_usr w_usr w_sub`) answers `deviceCode`, `userCode`,
   `verificationUriComplete` (`link.tidal.com/XXXXX`), `expiresIn`,
   `interval`.
2. `auth.begin` answers `{url: "https://link.tidal.com/XXXXX", instructions
   (with the code), expects_input: false}`.
3. The plugin polls `POST /v1/oauth2/token` (`grant_type =
   urn:ietf:params:oauth:grant-type:device_code`, `client_id`,
   `client_secret`, `device_code`) every `interval` seconds:
   `authorization_pending` means wait; once confirmed, it saves the tokens
   and sends `auth.changed {state: "signed_in", account}`. It stops when the
   code expires, on `auth.begin`, `auth.sign_out` and `shutdown`.
4. `auth.complete` (the paste field, always shown by hosts) polls once: it
   signs in if the code is confirmed, else answers `-32602`.

TIDAL issues tokens of this flow with CD quality at most (since late 2025,
whatever the subscription).

**Browser with PKCE** (`sign_in = browser`), for hi-res:
1. `auth.begin` answers `{url: login.tidal.com/authorize?response_type=code
   &redirect_uri=https://tidal.com/android/login/auth&client_id=…
   &code_challenge=<S256 of a random verifier>&code_challenge_method=S256
   &client_unique_key=…&appMode=android&lang=EN&restrict_signup=true,
   expects_input: true}`.
2. The user signs in and lands on
   `https://tidal.com/android/login/auth?code=…`, which TIDAL may show as
   not found; they paste that address.
3. `auth.complete {input}` takes `code` from it and trades it at
   `/v1/oauth2/token` (`grant_type=authorization_code`, `code_verifier`,
   `client_unique_key`, `redirect_uri`).

**Clients.** The built-in client ids and secrets are those of public apps,
as open-source TIDAL clients (python `tidalapi`, which `tidal-dl-ng` and
others build on) use them in 2026: `fX2JxdmntZWK0ixT` for the device code,
`6BDSRdpK9hqEBTgU` (TIDAL's Android app) for PKCE, the only one of the two
whose tokens carry the hi-res entitlement. Hidden options replace them:
`--client-id`, `--client-secret`, `--pkce-client-id`,
`--pkce-client-secret` (or `TIDALSRC_CLIENT_ID`…, for runs by hand: hosts
clear the environment).

**The account.** The token answer holds `user` (`userId`, `countryCode`,
names). `account.json` keeps the tokens, the user id, the country, the
flow, and a display name: `firstName lastName`, else `fullName` or
`nickname`, never `username` or `email` (often the e-mail address), never
anything containing `@`; `TIDAL account` otherwise. Without a country in
the answer, `GET v1/sessions` gives it.

| Method | Implementation |
|---|---|
| `auth.status` | `GET v1/sessions` with the stored tokens: `signed_in` (account: display name, subscription from `users/<id>/subscription`, plus "up to CD quality" after a device-code sign-in), `expired` if TIDAL refuses them, `signed_out` without `account.json` |
| `auth.begin` | Above |
| `auth.complete` | Above |
| `auth.sign_out` | Deletes `account.json`, stops a pending sign-in |

**Tokens.** The access token is refreshed (`grant_type=refresh_token`,
with the client of the flow that issued it) 2 minutes before it expires,
and once more when the API answers 401 about the token (`subStatus` 11xxx,
or none); the request is then sent again. New tokens are saved at once.
When TIDAL refuses the refresh token (revoked, password changed), requests
answer `auth_required` (-32001) and the plugin sends `auth.changed {state:
"expired"}` once.

## Catalogue

Every request carries `countryCode` (from the account) and the access
token. Lists are read in pages of 100 (the API's limit), several per host
page when the host asks for up to 200.

| Need | Endpoint |
|---|---|
| Search | `search?query=&types=TRACKS,ALBUMS,ARTISTS,PLAYLISTS&offset=&limit=` |
| Album, its tracks | `albums/<id>`, `albums/<id>/tracks` |
| Artist, their albums | `artists/<id>`, `artists/<id>/albums`, then `?filter=EPSANDSINGLES` |
| Playlist, its tracks | `playlists/<uuid>`, `playlists/<uuid>/items` |
| My playlists | `users/<id>/playlists` |
| My playlists and those I follow | `users/<id>/playlistsAndFavoritePlaylists` |
| Favourites | `users/<id>/favorites/{tracks,albums,artists,playlists}?order=DATE&orderDirection=DESC` |
| Favourite ids | `users/<id>/favorites/ids` |
| Add a favourite | `POST users/<id>/favorites/<kind>` (`trackId`, `albumId`, `artistId` or `uuids`) |
| Remove a favourite | `DELETE users/<id>/favorites/<kind>/<id>` |
| My mixes | `pages/my_collection_my_mixes` (v1 page), then `mixes/<id>/items` |
| Home | `home/feed/static` (API v2), else `pages/home` (v1) |
| Explore and its pages | `pages/explore`, `pages/<name>` |
| Track or artist radio | `tracks/<id>/radio`, `artists/<id>/radio` (100 tracks); on 404 the item's mix (`mixes.TRACK_MIX`, `ARTIST_MIX`) |
| Album radio | the radio of its first track |
| Similar albums | `albums/<id>/similar` (404: none) |

Protocol refs:
- catalogue entries: `track/<id>`, `album/<id>`, `artist/<id>`,
  `playlist/<uuid>`, `mix/<id>`;
- sections: `home`, `explore`, `mixes`, `fav` (a folder holding the next
  four), `fav/tracks`, `fav/albums`, `fav/artists`, `fav/playlists`,
  `my/playlists`;
- pages: `page/<api path>` (`page/pages/genre_jazz`), a folder of its
  shelves; `shelf/<n>/<page>`, the n-th shelf of `home` or of a page;
- related content: `radio/track/<id>`, `radio/album/<id>`,
  `radio/artist/<id>` (playlists of close tracks), `similar/<album id>`.

`browse.root` returns:
- `sections`, for hosts that show the plugin in their sidebar: Home ·
  Explore · My mixes · Favourites · My playlists;
- `home`, for hosts that merge the library lists: My mixes, then the
  shelves of TIDAL's Home page (read when signed in; none if it cannot be
  read). Favourites and playlists are left out, since they reach the host
  through `library.*`.

**Pages.** TIDAL's Home (v2 feed: `{items: [{title, items: [{type,
data}]}]}`) and Explore (v1 pages: `{rows: [{modules: [{type, title,
pagedList}]}]}`) are cut into shelves, one folder each, titled by TIDAL in
the host's language. Shelves holding nothing the plugin can list (videos,
text) are dropped. A v1 shelf with more items than it shows is read further
from its `dataApiPath`; a shelf of links (`PAGE_LINKS`) lists the linked
pages (genres, moods…) as `page/…` folders. Pages are kept 10 minutes, and
forgotten on sign-in, sign-out and language change. TIDAL reshapes these
pages often: one that cannot be read gives no shelves rather than an error.

**Item fields:**
- `art`: `resources.tidal.com/images/<id with / for ->/<w>x<h>.jpg`,
  640×640 for covers, 750×750 for artists, 480×480 for playlists;
- `format`: from `audioQuality` and `mediaMetadata.tags`. `HIRES_LOSSLESS`
  gives `{bits: 24, codec: "flac"}` (the rate is only known at
  resolution), `LOSSLESS` `{44100, 16, flac}`, `HIGH` `{44100, aac}`;
- `playable`: `allowStreaming` and `streamReady`;
- `album_ref`, `artist_ref` (tracks, albums): the album, the first main
  artist; `artist` joins the main artists (`type: MAIN`), featured ones
  are left out;
- `favorite`: from the favourite ids, read in the background at start-up
  and sign-in, again after 10 minutes, and updated at once by
  `favorites.set`. Absent until they are read;
- `entry_id` (tracks of a playlist): `<index>-<track id>` (see Playlist
  edits);
- `editable` (playlists): `creator.id` is the account's;
- `actions`, labelled in the host's language: on a track, its radio; on an
  album, its radio and similar albums; on an artist, their radio.

**`radio.next {seed, exclude, limit}`.** The seed is a track, album,
artist or radio ref. Answers `{items}`: playable tracks of that radio,
without the seed track and those in `exclude`, at most `limit` (20 by
default, 50 at most). `-32602` for any other seed.

## Library (`library.*`)

| Method | Source |
|---|---|
| `library.albums` | favourite albums |
| `library.artists` | favourite artists |
| `library.tracks` | favourite tracks |
| `library.playlists` | `playlistsAndFavoritePlaylists`: owned (`editable`) and followed |

All take `{offset, limit}` and answer `{items, total, has_more}`, 200 items
per page at most.

## Playlist edits (`playlists.*`)

| Method | Request |
|---|---|
| `playlists.create {name, description?, public?}` | `PUT v2/my-collection/playlists/folders/create-playlist?name=&description=&folderId=root` (`v1 POST users/<id>/playlists` if v2 answers 404/405); `public`: then `PUT v2/playlists/<uuid>/set-public`. Answers the new playlist item |
| `playlists.rename {ref, name}` | `POST playlists/<uuid>` `title`, `description` (kept) |
| `playlists.delete {ref}` | `DELETE playlists/<uuid>` |
| `playlists.add {ref, items}` | `POST playlists/<uuid>/items` `trackIds` (comma-separated), `onDupes=ADD`, `onArtifactNotFound=SKIP`, `toIndex` = the playlist's length |
| `playlists.remove {ref, entries}` | `DELETE playlists/<uuid>/items/<i,j,…>` |
| `playlists.move {ref, entry, to}` | `POST playlists/<uuid>/items/<i>` `toIndex=<to>` |

- **Versions.** Every edit sends the playlist's `ETag` (from `GET
  playlists/<uuid>`) as `If-None-Match`, as TIDAL requires. A `412` (the
  playlist changed meanwhile) reads the ETag again and retries once.
- **Ownership.** Before editing an existing playlist, the plugin reads it
  and refuses (`-32602`) unless `creator.id` is the account's.
- **Entries.** TIDAL removes and moves by position, and gives items no id
  of their own. `entry_id` is therefore `<index>-<track id>`. Before a
  removal or a move, the plugin reads the playlist (up to 10 000 items) and
  checks that each entry's track is still at its index; if not, `not_found`
  (-32002), and the host reads the playlist again.
- **Moves.** `to` is the entry's final position, sent as is (clamped to the
  last position). That `toIndex` means the final position is what the
  python client's tests check; it is not verified against TIDAL here.
- `items` must be track refs, 1 to 500; names are trimmed (200 characters at
  most) and required.

## Details (`item.details`)

`{biography?: {text, source}, related?: [{title, items}], facts?: [{label,
value}]}`, titles and labels in the host's language, empty shelves left
out, 20 items per shelf. Missing parts (404) are left out.

| Ref | Source | Content |
|---|---|---|
| `artist/<id>` | `artists/<id>/bio`, `toptracks`, `similar` | biography; top tracks, similar artists |
| `album/<id>` | `albums/<id>`, `review`, `items/credits`, `similar` | review; release date, discs, quality, copyright, credits gathered over the tracks (one fact per role); similar albums |
| `track/<id>` | `tracks/<id>/credits` | one fact per role (`Composer: A, B`) |

Other refs answer `{}`. TIDAL's texts hold `[wimpLink …]…[/wimpLink]` and
HTML: both are stripped, line breaks kept and entities decoded.

## Lyrics (`lyrics.get`)

`tracks/<id>/lyrics` answers `lyrics` (plain text) and `subtitles` (LRC
lines, `[mm:ss.xx] text`):
- `subtitles` becomes `synced: [{time_ms, text}]` (lines without a time are
  skipped; a line with several times appears once per time);
- `lyrics` becomes `plain`.

A track without lyrics (404) or with empty ones answers `not_found`
(-32002) and is remembered for 6 hours, so the API is not asked again
meanwhile.

## Resolution (`track.resolve`)

`tracks/<id>/playbackinfo?audioquality=<Q>&playbackmode=STREAM&assetpresentation=FULL`
answers `audioQuality` (what TIDAL picked, at most `Q`), `sampleRate`,
`bitDepth`, `trackReplayGain`, `trackPeakAmplitude`, `manifestMimeType` and
`manifest` (base64). In parallel, `tracks/<id>` gives the duration.

**Quality matched to the output:**
- first asked: `HI_RES_LOSSLESS`, or `LOSSLESS` when `output.max_bits` is
  below 24; never above the `quality` setting;
- the format TIDAL hands out is checked against the output (rate among
  `rates`, else at most `max_rate`; bits at most `max_bits`); if it does not
  fit, the next lower quality is asked: `HI_RES_LOSSLESS` → `LOSSLESS` →
  `HIGH` (AAC). Nothing fits: `unavailable` (-32003);
- a preview (`assetPresentation` other than `FULL`) gives `unavailable`.

**Manifests:**
- `application/vnd.tidal.bts`: JSON `{mimeType, codecs, encryptionType,
  urls}`. With `encryptionType: NONE`, `urls[0]` goes to the host as is
  (`delivery: "direct"`); `expires_at` is the URL's `exp=` minus 60 s when
  it carries one, absent otherwise (the host resolves again on an HTTP
  error). Anything else (`OLD_AES`…) gives `unavailable`, with the scheme
  in the message: tidalsrc does not decrypt.
- `application/dash+xml`: an MPD. With `ContentProtection` (DRM):
  `unavailable`. With `codecs="flac"`: rebuilt by the relay (below). Other
  codecs: the next lower quality.

**DASH → FLAC.** TIDAL serves hi-res as FLAC in fragmented MP4: one
`SegmentTemplate` with `initialization` and `media` (`$Number$`) URLs, and
a `SegmentTimeline` (`<S d r>`) giving the number of segments.
1. The init segment holds `moov/…/stsd/fLaC/dfLa`: the FLAC metadata blocks.
   `fLaC` + those blocks (the last one flagged) is the file's header.
   TIDAL leaves STREAMINFO's total samples at 0; when the timeline's
   timescale is the sample rate, the timeline's sum is written there (the
   only change; the audio bytes are untouched). The rate and depth come
   from STREAMINFO, and are checked against the output again.
2. Each segment is `moof` (`traf`: `tfhd` + `trun` with the sizes of its
   samples and where they start) + `mdat`. Each sample is one FLAC frame.
   At resolution, the plugin reads the first 16 KiB of every segment (16 at
   once; the whole segment if the CDN ignores `Range` or the `moof` is
   longer) and adds up the sample sizes.
3. The relay serves `http://127.0.0.1:<port>/<token>/<id>.flac`:
   `Content-Length` = header + every segment's frames, known before the
   first byte; `Accept-Ranges: bytes`, and a range (`206`) fetches only the
   segments it covers (the last 3 are kept). The answer to `track.resolve`
   carries `"delivery": "proxied"`.
4. Tokens are random, forgotten when the segment URLs expire (`exp=` in
   their URL, else after 3 hours; 32 streams at most); an unknown token
   answers `410`, and the host resolves the track again.

**Response:**
- `format`: `sample_rate`, `bits` (FLAC only), `channels`, `codec`
  (`flac`, `aac`);
- `duration_ms` from `tracks/<id>`; `replaygain` from `trackReplayGain` and
  `trackPeakAmplitude`;
- `expires_at` when known.

## Errors

| API response | Protocol code |
|---|---|
| HTTP 401 about the token, refresh refused, not signed in | `auth_required` (-32001) |
| HTTP 401 about the item (`subStatus` 4005, 4010, 4032, 4033, 4035), HTTP 403 | `unavailable` (-32003) |
| Encrypted or DRM stream, preview only, no format fits | `unavailable` (-32003), the reason in `message` |
| HTTP 404, unknown `ref` | `not_found` (-32002) |
| HTTP 429 | `rate_limited` (-32004), `data.retry_after` from `Retry-After` (30 s otherwise, 3600 at most) |
| HTTP 5xx, connection error or timeout | `network` (-32005) |

## Tests

`tests/plugin.rs` runs `tidalsrc plugin --api-base <mock> --auth-base
<mock> --login-base <mock>` (hidden options) against a mock TIDAL: API,
OAuth server and CDN in one axum server.

Covered:
- handshake, capabilities and settings;
- device-code sign-in end to end (pending, then confirmed; `auth.changed`;
  `account.json` mode 600 without the e-mail address), sign-out;
- browser sign-in: the verifier sent matches the page's challenge;
- Home, Explore, pages and shelves, mixes, library lists, search, albums,
  artists (albums then singles), favourites and their state, radio and
  `radio.next`, details, lyrics, language change;
- playlist edits: create, rename with a 412 retry, add, move, remove,
  stale entries, refusal of a playlist the account does not own;
- resolution: hi-res DASH through the relay (whole file compared byte for
  byte, a range across segments, `HEAD`, an unknown token), CD quality
  direct, quality stepping down for 16-bit and 48/96 kHz-only outputs, the
  quality setting, encrypted, unplayable and preview tracks;
- an expired access token refreshed and the request retried, rate
  limiting, a revoked refresh token (`auth.changed {expired}` once);
- no token, user id, e-mail address or signed URL in the log.

Unit tests cover the manifests, the fMP4 → FLAC rebuild on synthetic
segments, the quality choice, items from recorded API answers, entries,
pages, lyrics, credits and errors. The `live_*` tests (`#[ignore]`, see the
README) run against the real TIDAL.

## Risks

- **Client ids**: those of public apps. TIDAL may revoke them or restrict
  what their tokens get (it did so for the device code in late 2025). The
  hidden options replace them without a new build.
- **Endpoints**: the v1 API, the v2 Home feed and playlist creation are
  undocumented; Home, Explore and radios degrade to empty lists where they
  can.
- **Not checked against TIDAL**: `toIndex` semantics of moves, the lifetime
  of segment URLs, the shape of v2 mixes and Home items beyond what the
  python client parses.
- **Terms of use**: tidalsrc uses TIDAL's API outside its terms of use;
  TIDAL may block it or restrict the accounts that use it.
