# rs-subsonic

A lightweight Rust server that exposes an **OpenSubsonic-compatible API** on top of a
**Plex Media Server** music library. Point any Subsonic client (Symfonium, Feishin,
Supersonic, DSub, …) at your Plex music.

The backend layer is swappable. Storage is SQLite or Postgres. Song, album and artist ids
are **stable**: they survive Plex re-adding or re-matching items, files moving, and
rs-subsonic's own database being rebuilt, so playlists, stars and anything else that stores
ids keep working.

## Quick start

Each release publishes a multi-arch image (linux/amd64, linux/arm64),
`ghcr.io/<owner>/rs-subsonic`; see [Docker](#docker) below. Or build it:

```sh
cargo build --release                      # SQLite (default)
cargo build --release --features postgres  # SQLite + Postgres

cp rs-subsonic.example.toml rs-subsonic.toml   # set [[backend]] url and path_map
./target/release/rs-subsonic user add admin --admin   # password read from stdin
./target/release/rs-subsonic user api-key admin       # optional: OpenSubsonic API key
PLEX_TOKEN=<your Plex token> ./target/release/rs-subsonic   # serve on 0.0.0.0:4533

curl "http://localhost:4533/rest/ping?u=admin&p=<password>&f=json"
```

The first sync starts immediately; `getScanStatus` shows progress. Point your client at
`http://<host>:4533` with the user you created.

Postgres needs the [pgvector](https://github.com/pgvector/pgvector) extension, which the
`pgvector/pgvector` images and managed Postgres services include. rs-subsonic creates it on
first start when its database user may; otherwise run `CREATE EXTENSION vector;` in the
database once as a superuser.

### Docker

With Docker (SQLite and Postgres support; the config in `/config`, data in `/data`, and the
music mounted wherever `path_map` says):

```sh
docker build -t rs-subsonic .    # or pull the released image
docker run -d --name rs-subsonic -p 4533:4533 -e PLEX_TOKEN=<token>   -v ./rs-subsonic.toml:/config/rs-subsonic.toml:ro -v ./data:/data   -v /path/to/music:/mnt/music:ro rs-subsonic
docker exec -it rs-subsonic rs-subsonic user add admin --admin
```

The image is distroless (no shell) and runs as uid 65532 (`nonroot`), so `./data` must be
writable by it: `sudo chown 65532:65532 ./data`.

Configuration: `rs-subsonic.toml` (see the example), with `RSUB_*` environment overrides
for the `server`, `database`, `auth`, `cache` and `analysis` tables (`RSUB_<TABLE>_<KEY>`,
for example `RSUB_AUTH_ALLOW_PLAINTEXT=false`).

## Mounting the music library (`path_map`)

rs-subsonic works with Plex alone, but it expects to see the music files too: mount Plex's
music folders on the machine running rs-subsonic (read-only is enough) and map Plex's paths
to local ones:

```toml
[[backend.path_map]]
remote = "/data/music"   # the folder as Plex sees it (the library's folder settings)
local = "/mnt/music"     # the same folder here
```

Add one `path_map` per Plex library folder; a Plex server on Windows (`D:\Music`) works too.
With the folders mapped:

- **Ids are keyed on MusicBrainz ids** from the files' tags (as written by Lidarr or
  MusicBrainz Picard). Without them, ids rest on Plex's own matches, so re-matching an album
  in Plex can change its ids. rs-subsonic logs a warning at startup when no `path_map` is
  set, and each sync logs how many tracks lack a MusicBrainz id (`debug` lists them).
- **Files are streamed directly** instead of through Plex. That's faster and keeps working
  when Plex is down. Each play checks the file's size against the catalog and falls back to
  Plex if it doesn't match.

Tags are cached by size and modification time, so after the first sync only changed files
are read again.

## Stable ids and backups

Ids are opaque (`tr…`, `al…`, `ar…`), derived from each item's strongest key (its
MusicBrainz id, else Plex's match, else its tags) and recorded in an **identity ledger**.
The ledger never forgets a key, so an item that disappears and comes back, or loses one key
and gains another, keeps its id.

After each sync the ledger is written to **`data/identity.jsonl`** (in `data_dir`). If the
database is lost, a fresh one restores the ledger from this file at startup, before the first
sync, so every id comes back. If the file exists but can't be read, rs-subsonic refuses to
start rather than mint new ids; move it aside to start without it.

Back up these files along with the database, which holds users, API keys and the ratings of
artists Plex doesn't have (stars, play counts and playlists are your Plex account's):

| File | Why |
|---|---|
| `data/identity.jsonl` | Keeps ids stable if the database is lost |
| `data/secret.key` | Stored passwords are encrypted with it (or set `RSUB_SECRET_KEY`); without it they can't be decrypted |

### Moving from SQLite to Postgres

A new database with the same `data_dir` restores ids from `identity.jsonl` by itself. To
move them explicitly, or between machines:

```sh
rs-subsonic ids export -o ids.jsonl          # with the old database configured; stop the server first
RSUB_DATABASE_URL=postgres://rsub:secret@localhost/rsub rs-subsonic ids import ids.jsonl
```

Import before the new database's first sync. Importing into a database that already has ids
only adds the keys it's missing; keys it has keep their ids. To move users too, use a backup
(below).

### Backups

A backup holds what only the database has: users (passwords still encrypted), their API keys,
local ratings, and the identity ledger. The catalog isn't in it; the first sync rebuilds it,
with the same ids.

```sh
rs-subsonic backup export -o backup.jsonl       # on the server's machine
rs-subsonic backup restore backup.jsonl         # into a new database, before its first sync

rsub-cli backup --server http://music.local:4533 --key <admin API key> export -o backup.jsonl
rsub-cli backup restore backup.jsonl            # with RSUB_SERVER and RSUB_API_KEY set
```

Restoring merges: users the database already has stay as they are, and ids it already has are
kept, so a restore is safe to repeat. Passwords only decrypt with the server key
they were stored under, so keep `secret.key` with the backup; a restore under another key is
refused.

## Administration

Users are managed on the server's machine with `rs-subsonic user …`, or remotely through the
admin API (`/api/v1`) with `rsub-cli user …` and an admin's API key:

```sh
rs-subsonic user list
rs-subsonic user add carol --roles stream,download,playlist,coverArt --max-bitrate 320
rs-subsonic user update carol --admin true      # or --email, --roles, --max-bitrate
rs-subsonic user passwd carol                   # new password from stdin
rs-subsonic user api-key carol --name phone     # printed once
rs-subsonic user api-keys carol                 # ids and names
rs-subsonic user revoke-key carol 3
rs-subsonic user delete carol

export RSUB_SERVER=http://music.local:4533 RSUB_API_KEY=<admin API key>
rsub-cli user list                              # the same commands, plus `user show`
```

The last admin can't be deleted or demoted.

Regular users get `settings`, `stream`, `download`, `coverArt`, `comment` and `share`. Until
users link their own Plex account (M5), everyone writes to the configured one, so the roles
that change it are left to you to grant:

| Role | Allows |
|---|---|
| `rating` | star, unstar and rate songs, albums and artists (not a Subsonic role) |
| `scrobbling` | record plays and now-playing in Plex; without it, scrobbles are accepted and dropped |
| `playlist` | create, change and delete the account's playlists |

Artists that exist only in rs-subsonic (credited but not in Plex) are rated per user, and need
no role.

## Cargo features

| Feature | Default | Description |
|---|---|---|
| `sqlite` | ✓ | SQLite storage |
| `postgres` | | Postgres storage |
| `mimalloc` | | Use the mimalloc allocator |
| `local-tags` | ✓ | Read tags (MusicBrainz ids) from a mounted library |
| `clap` | | Sonic similarity with CLAP embeddings |
| `bliss` | | Sonic similarity with bliss-rs (makes the binary GPL-3.0) |

The released image is built with `postgres` (so SQLite and Postgres) and without an
analyzer; build from source for sonic similarity, and for `rsub-cli`.

## Development

```sh
cargo test --workspace
# Postgres parity tests (skipped unless set):
docker run -d --rm --name rsub-pg -e POSTGRES_PASSWORD=pw -p 127.0.0.1:5432:5432 pgvector/pgvector:pg17
RSUB_TEST_PG_URL=postgres://postgres:pw@127.0.0.1/postgres cargo test --workspace --features postgres
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. A binary built with the `bliss` feature includes
bliss-audio and is therefore distributed under GPL-3.0.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as
above, without any additional terms or conditions.
