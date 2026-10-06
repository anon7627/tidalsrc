# tidalsrc

Unofficial **TIDAL** source plugin for players that implement the source
plugin protocol v1 (separate process, JSON-RPC over stdin/stdout). The
player starts it and hands it everything TIDAL-related:

- **account sign-in**, through TIDAL's own pages (your password never goes
  through tidalsrc): confirm a code on `link.tidal.com`, or sign in on
  TIDAL's page for hi-res;
- **catalogue**: search, Home and Explore shelves (new albums, mixes,
  genres, moods…), your mixes, favourites (with their state on every item),
  playlists; track, album and artist radio, similar albums; artist
  biographies, album reviews and credits, track credits; lyrics, synced
  when TIDAL has them;
- **your playlists**: create, rename, delete, add, remove and move tracks
  (only playlists you own);
- **playback**: each track becomes a stream in the best quality your DAC
  plays natively. CD-quality and AAC files are read by the player straight
  from TIDAL's servers. Hi-res files, which TIDAL serves in segments, are
  put back together by tidalsrc on this computer and handed over as the
  original FLAC file, unchanged (no decoding, no re-encoding).

There is no TIDAL Connect (no open implementation exists), and plays are not
reported to TIDAL (no reliable endpoint).

Implementation details: [docs/plugin.md](docs/plugin.md).

> Unofficial TIDAL client. It uses TIDAL's API outside its terms of use. A
> TIDAL subscription is required. TIDAL may break it or restrict the
> accounts that use it. Use at your own risk.

## Installation

```sh
cargo install --path .         # → ~/.cargo/bin/tidalsrc
```

Releases are built by GitHub Actions (`.github/workflows/release.yml`), never
on a developer's machine. Pushing a `vX.Y.Z` tag (annotated: its message
becomes the release notes) runs the tests, builds static Linux binaries for
x86_64 and aarch64, checks that no build path is left in them, and publishes
the release with their SHA-256. The tag must match the version in
`Cargo.toml`.

Then declare the plugin in your player's configuration, with the absolute
path of the binary and the `plugin` argument:

```toml
[[plugins]]
id = "tidal"
command = "/path/to/.cargo/bin/tidalsrc"
args = ["plugin"]
enabled = true
```

## Usage

1. In the player's plugin settings: TIDAL → **Sign in**.
   - **With a code** (default): `link.tidal.com` opens with a code; sign in
     to TIDAL there and confirm it. Sign-in completes by itself within a
     few seconds, and works from any device (the player also shows the link
     as a QR code). TIDAL limits this kind of sign-in to CD quality.
   - **With the TIDAL page** (setting *Sign-in method*, for hi-res): sign in
     on TIDAL's page. You land on a TIDAL address containing `code=`, which
     may show "page not found": copy that full address and paste it into the
     player.
2. The TIDAL catalogue shows up in the player: your albums, artists, tracks
   and playlists next to your own music, Home shelves, browsing and search.

Changing the sign-in method takes effect at the next sign-in: sign out, then
in again.

## Quality and your DAC

The player tells tidalsrc what the current output plays natively, and
tidalsrc picks the stream accordingly:

| Output | Asked of TIDAL |
|---|---|
| 24 bits or more (or unknown) | hi-res (FLAC up to 24-bit/192 kHz) |
| 16 bits | CD quality (FLAC 16-bit/44.1 kHz) |

If TIDAL hands out a format the output would have to convert (its sample
rate is not among the output's rates, or above its maximum), tidalsrc asks
for the next lower quality: hi-res, then CD quality, then AAC 320 kbit/s.
When nothing fits, the track is reported unavailable. The *Best quality*
setting caps all this (hi-res, CD quality or AAC).

What you get also depends on your subscription and on the sign-in method:
TIDAL serves hi-res only to accounts signed in with the TIDAL page.

## Data

In the data directory the player gives the plugin (`data_dir`):

| File | Contents |
|---|---|
| `data_dir/account.json` | TIDAL OAuth tokens, user id, country, a display name (never your e-mail address or password), mode `600` |

`account.json` gives access to your account, like a cookie. The access token
is refreshed by itself; signing out deletes the file. Nothing is written to
the cache directory.

## Troubleshooting

The plugin logs to stderr, which the player copies into its own log. More
detail: `RUST_LOG=tidalsrc=debug` in the player's environment. Tokens, your
user id and signed stream addresses never appear there.

- **Sign-in fails with `invalid_client` or `Client not found`**: TIDAL has
  revoked the client id tidalsrc presents (they are those of public apps,
  as other open-source TIDAL clients use them). Pass working ones on the
  command line, in the plugin's `args` (the player clears the environment):

  ```toml
  args = ["plugin", "--client-id", "…", "--client-secret", "…",
          "--pkce-client-id", "…", "--pkce-client-secret", "…"]
  ```

  (`TIDALSRC_CLIENT_ID`, `TIDALSRC_CLIENT_SECRET`, `TIDALSRC_PKCE_CLIENT_ID`
  and `TIDALSRC_PKCE_CLIENT_SECRET` work too when running it by hand.) Then
  sign in again: tokens are tied to the client that issued them.
- **Everything is CD quality on a hi-res DAC**: you signed in with a code.
  Pick *With the TIDAL page* in the plugin's settings, sign out and sign in
  again. Check the account line: it says "up to CD quality" after a code
  sign-in.
- **`TIDAL sent track … encrypted`**: TIDAL served an encrypted stream
  (older clients get `OLD_AES`; some catalogue gets DRM). tidalsrc does not
  decrypt; the track is skipped.
- **The player keeps asking to sign in**: TIDAL no longer accepts the stored
  refresh token (password changed, sessions revoked). Sign in again.
- **Home or Explore is empty**: TIDAL changes these pages often; tidalsrc
  shows no shelves rather than an error when it cannot read them. Search,
  favourites and playlists are not affected.

## Tests

```sh
cargo test                                   # unit tests + fake host against a mock TIDAL
TIDALSRC_DATA=/tmp/tidal cargo test live_device_sign_in -- --ignored --nocapture
TIDALSRC_DATA=/tmp/tidal cargo test live_catalogue -- --ignored --nocapture
TIDALSRC_DATA=/tmp/tidal TIDALSRC_TRACK=<id> TIDALSRC_OUT=/tmp/t.flac \
  cargo test live_resolve -- --ignored --nocapture && flac -t /tmp/t.flac
```

The `live_*` tests talk to the real TIDAL: the first signs in (it prints the
link to open) and writes `account.json` in `TIDALSRC_DATA`, which the others
use.

## Architecture

| Module | Role |
|---|---|
| `plugin/mod.rs` | JSON-RPC loop, `initialize`/`shutdown`, `auth.*`, notifications |
| `plugin/rpc.rs` | Line-based JSON-RPC 2.0, error codes |
| `plugin/catalog.rs`, `plugin/items.rs` | `browse.*`, `library.*`, `search`, `item.get`, `favorites.set`, `radio.next`; API objects → items |
| `plugin/pages.rs` | Home and Explore pages cut into shelves |
| `plugin/details.rs`, `plugin/lyrics.rs` | `item.details`, `lyrics.get` |
| `plugin/playlists.rs` | `playlists.*`, with TIDAL's ETags |
| `plugin/resolve.rs` | `track.resolve`: quality matched to the output, manifests |
| `plugin/relay.rs` | Local HTTP server serving DASH streams as one FLAC file (`Range` too) |
| `plugin/settings.rs` | Declared settings and their values |
| `dash.rs`, `fmp4.rs` | BTS and DASH manifests; FLAC header and frames from fragmented MP4 |
| `api.rs` | TIDAL REST API, token refresh, errors |
| `oauth.rs`, `auth.rs` | Device-code and PKCE sign-in, token storage |
