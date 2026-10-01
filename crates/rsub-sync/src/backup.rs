//! Backups: what only this database holds, as JSON lines, portable between
//! SQLite and Postgres. That's users (passwords still encrypted), their API
//! keys (by hash) and local artist ratings, the settings worth keeping, and
//! the identity ledger. The catalog is left out: a sync rebuilds it, with the
//! same ids once the ledger is back. So are analysis vectors, which move with
//! `/api/v1/analysis/vectors`.
//!
//! The first line is a header, `{"rsub_backup":1,…}`; each further line is
//! one record, `{"user":{…}}`, `{"api_key":{…}}`, `{"artist_rating":{…}}`,
//! `{"setting":{…}}` or `{"identity":{…}}`. Passwords only decrypt with the
//! server key they were stored under, so a restore refuses a backup whose
//! passwords the current key can't decrypt.

use std::collections::HashSet;

use rsub_core::crypto::{Purpose, SecretBox};
use rsub_core::{Roles, now_ms};
use rsub_store::{
    AccountRestore, BackupApiKey, BackupRating, BackupUser, Db, LedgerEntry, StoreError,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufWriter};

use crate::snapshot::Imported;

const VERSION: u32 = 1;
const PAGE: u64 = 5000;
/// Settings a backup carries. The rest describe this database's catalog.
const SETTINGS: &[&str] = &["plex_client_id"];

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("line {line}: {msg}")]
    Format { line: usize, msg: String },
    #[error(
        "the password of '{0}' can't be decrypted: the backup was made under another server \
         key (set RSUB_SECRET_KEY or auth.secret_key_file to that key)"
    )]
    WrongKey(String),
}

#[derive(Serialize, Deserialize)]
struct Header {
    rsub_backup: u32,
    created_at: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Line {
    User(UserLine),
    ApiKey(KeyLine),
    ArtistRating(RatingLine),
    Setting(SettingLine),
    Identity(LedgerEntry),
}

#[derive(Serialize, Deserialize)]
struct UserLine {
    username: String,
    /// Encrypted with the server key, hex.
    password: String,
    email: Option<String>,
    /// [`Roles::NAMES`].
    roles: Vec<String>,
    max_bitrate: u32,
    created_at: i64,
    updated_at: i64,
}

#[derive(Serialize, Deserialize)]
struct KeyLine {
    username: String,
    name: String,
    /// SHA-256 of the key, hex.
    key_hash: String,
    created_at: i64,
}

#[derive(Serialize, Deserialize)]
struct RatingLine {
    username: String,
    /// The artist's public id.
    artist: String,
    rating: i64,
    rated_at: i64,
}

#[derive(Serialize, Deserialize)]
struct SettingLine {
    key: String,
    value: String,
}

/// What a backup holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub users: u64,
    pub api_keys: u64,
    pub ratings: u64,
    pub settings: u64,
    pub identity_keys: u64,
}

/// What a restore did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restored {
    /// What the backup held.
    pub read: Counts,
    pub accounts: AccountRestore,
    /// Settings this database didn't have.
    pub settings_added: u64,
    pub identity: Imported,
}

/// Write a backup of `db` to `w`.
pub async fn export<W: AsyncWrite + Unpin>(db: &Db, w: W) -> Result<Counts, BackupError> {
    let mut w = BufWriter::new(w);
    let mut buf = Vec::new();
    let mut n = Counts::default();
    let header = Header {
        rsub_backup: VERSION,
        created_at: now_ms(),
    };
    write_line(&mut w, &mut buf, &header).await?;
    for u in db.backup_users().await? {
        let roles = Roles(u32::try_from(u.roles).unwrap_or(0)).names();
        let line = Line::User(UserLine {
            username: u.username,
            password: hex::encode(u.password_enc),
            email: u.email,
            roles: roles.into_iter().map(str::to_owned).collect(),
            max_bitrate: u32::try_from(u.max_bitrate).unwrap_or(0),
            created_at: u.created_at,
            updated_at: u.updated_at,
        });
        write_line(&mut w, &mut buf, &line).await?;
        n.users += 1;
    }
    for k in db.backup_api_keys().await? {
        let line = Line::ApiKey(KeyLine {
            username: k.username,
            name: k.name,
            key_hash: hex::encode(k.key_hash),
            created_at: k.created_at,
        });
        write_line(&mut w, &mut buf, &line).await?;
        n.api_keys += 1;
    }
    for r in db.backup_ratings().await? {
        let line = Line::ArtistRating(RatingLine {
            username: r.username,
            artist: r.artist,
            rating: r.rating,
            rated_at: r.rated_at,
        });
        write_line(&mut w, &mut buf, &line).await?;
        n.ratings += 1;
    }
    for key in SETTINGS {
        if let Some(value) = db.get_setting(key).await? {
            let line = Line::Setting(SettingLine {
                key: (*key).to_owned(),
                value,
            });
            write_line(&mut w, &mut buf, &line).await?;
            n.settings += 1;
        }
    }
    let mut after: Option<(String, String)> = None;
    loop {
        let page = db
            .ledger_page(after.as_ref().map(|(k, v)| (k.as_str(), v.as_str())), PAGE)
            .await?;
        let Some(last) = page.last() else { break };
        after = Some((last.kind.clone(), last.key.clone()));
        let full = page.len() as u64 == PAGE;
        for e in page {
            write_line(&mut w, &mut buf, &Line::Identity(e)).await?;
            n.identity_keys += 1;
        }
        if !full {
            break;
        }
    }
    w.flush().await?;
    Ok(n)
}

async fn write_line<W: AsyncWrite + Unpin>(
    w: &mut W,
    buf: &mut Vec<u8>,
    value: &impl Serialize,
) -> std::io::Result<()> {
    buf.clear();
    serde_json::to_writer(&mut *buf, value).expect("backup lines serialise");
    buf.push(b'\n');
    w.write_all(buf).await
}

/// Read a backup from `r` into `db`, merging: what the database already has
/// stays as it is (see [`Db::restore_accounts`]; ledger keys it has keep
/// their ids), so restoring is safe to repeat. Nothing is written unless the
/// whole backup parses and was made under `secrets`' key.
pub async fn restore<R: AsyncBufRead + Unpin>(
    db: &Db,
    secrets: &SecretBox,
    r: R,
) -> Result<Restored, BackupError> {
    let mut lines = r.lines();
    let mut n = 0;
    let mut header: Option<Header> = None;
    let mut users = Vec::new();
    let mut keys = Vec::new();
    let mut ratings = Vec::new();
    let mut settings = Vec::new();
    let mut ledger = Vec::new();
    let mut names = HashSet::new();
    while let Some(line) = lines.next_line().await? {
        n += 1;
        if line.trim().is_empty() {
            continue;
        }
        let bad = |msg: String| BackupError::Format { line: n, msg };
        if header.is_none() {
            let h: Header =
                serde_json::from_str(&line).map_err(|_| bad("not an rs-subsonic backup".into()))?;
            if h.rsub_backup != VERSION {
                return Err(bad(format!(
                    "backup version {} is not supported",
                    h.rsub_backup
                )));
            }
            header = Some(h);
            continue;
        }
        match serde_json::from_str::<Line>(&line).map_err(|e| bad(e.to_string()))? {
            Line::User(u) => {
                if u.username.trim().is_empty() || !names.insert(u.username.clone()) {
                    return Err(bad(format!("user '{}' is empty or repeated", u.username)));
                }
                users.push(BackupUser {
                    password_enc: hex::decode(&u.password)
                        .map_err(|_| bad("password isn't hex".into()))?,
                    roles: i64::from(Roles::from_names(&u.roles).map_err(bad)?.0),
                    username: u.username,
                    email: u.email,
                    max_bitrate: i64::from(u.max_bitrate),
                    created_at: u.created_at,
                    updated_at: u.updated_at,
                });
            }
            Line::ApiKey(k) => keys.push(BackupApiKey {
                key_hash: hex::decode(&k.key_hash)
                    .ok()
                    .filter(|h| h.len() == 32)
                    .ok_or_else(|| bad("key_hash isn't a hex SHA-256".into()))?,
                username: k.username,
                name: k.name,
                created_at: k.created_at,
            }),
            Line::ArtistRating(r) => ratings.push(BackupRating {
                username: r.username,
                artist: r.artist,
                rating: r.rating,
                rated_at: r.rated_at,
            }),
            Line::Setting(s) => {
                if SETTINGS.contains(&s.key.as_str()) {
                    settings.push(s);
                }
            }
            Line::Identity(e) => ledger.push(e),
        }
    }
    if header.is_none() {
        return Err(BackupError::Format {
            line: 1,
            msg: "empty backup".into(),
        });
    }
    if let Some(u) = users.iter().find(|u| {
        secrets
            .decrypt(Purpose::Password, &u.username, &u.password_enc)
            .is_err()
    }) {
        return Err(BackupError::WrongKey(u.username.clone()));
    }
    let read = Counts {
        users: users.len() as u64,
        api_keys: keys.len() as u64,
        ratings: ratings.len() as u64,
        settings: settings.len() as u64,
        identity_keys: ledger.len() as u64,
    };
    // The ledger first: it's what keeps ids, and a sync may be about to run.
    let added = db.import_ledger(&ledger).await?;
    let accounts = db.restore_accounts(&users, &keys, &ratings).await?;
    let mut settings_added = 0;
    for s in settings {
        if db.get_setting(&s.key).await?.is_none() {
            db.set_setting(&s.key, &s.value).await?;
            settings_added += 1;
        }
    }
    Ok(Restored {
        read,
        accounts,
        settings_added,
        identity: Imported {
            read: read.identity_keys,
            added,
        },
    })
}
