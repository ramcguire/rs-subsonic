//! `user` and `backup`: administer a running server through its admin API
//! (`/api/v1/users`, `/api/v1/backup`), authenticated with an admin's API
//! key. They match `rs-subsonic user …` and `rs-subsonic backup …`, which
//! work on the database directly.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use argh::FromArgs;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::Error;
use crate::remote::Api;

/// Administer a server's users.
#[derive(FromArgs)]
#[argh(subcommand, name = "user")]
pub struct User {
    /// the server's base URL (default: the RSUB_SERVER environment variable)
    #[argh(option)]
    server: Option<String>,
    /// an admin's API key (default: the RSUB_API_KEY environment variable)
    #[argh(option)]
    key: Option<String>,
    #[argh(subcommand)]
    cmd: UserSub,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum UserSub {
    List(List),
    Show(Show),
    Add(Add),
    Update(Update),
    Passwd(Passwd),
    Delete(Delete),
    ApiKey(ApiKey),
    ApiKeys(ApiKeys),
    RevokeKey(RevokeKey),
}

/// List users.
#[derive(FromArgs)]
#[argh(subcommand, name = "list")]
struct List {}

/// Show one user.
#[derive(FromArgs)]
#[argh(subcommand, name = "show")]
struct Show {
    /// username
    #[argh(positional)]
    username: String,
}

/// Create a user. The password is read from stdin unless --password is given.
#[derive(FromArgs)]
#[argh(subcommand, name = "add")]
struct Add {
    /// username
    #[argh(positional)]
    username: String,
    /// password (prefer stdin to keep it out of shell history)
    #[argh(option)]
    password: Option<String>,
    /// email address
    #[argh(option)]
    email: Option<String>,
    /// grant all roles including admin
    #[argh(switch)]
    admin: bool,
    /// comma-separated roles instead of a regular user's
    #[argh(option)]
    roles: Option<String>,
    /// maximum streaming bitrate in kbps (0 = unlimited)
    #[argh(option, default = "0")]
    max_bitrate: u32,
}

/// Change a user's email, roles or bitrate limit.
#[derive(FromArgs)]
#[argh(subcommand, name = "update")]
struct Update {
    /// username
    #[argh(positional)]
    username: String,
    /// email address ("" clears it)
    #[argh(option)]
    email: Option<String>,
    /// make the user an admin (true) or not (false)
    #[argh(option)]
    admin: Option<bool>,
    /// comma-separated roles, replacing theirs
    #[argh(option)]
    roles: Option<String>,
    /// maximum streaming bitrate in kbps (0 = unlimited)
    #[argh(option)]
    max_bitrate: Option<u32>,
}

/// Set a user's password, read from stdin unless --password is given.
#[derive(FromArgs)]
#[argh(subcommand, name = "passwd")]
struct Passwd {
    /// username
    #[argh(positional)]
    username: String,
    /// the new password (prefer stdin to keep it out of shell history)
    #[argh(option)]
    password: Option<String>,
}

/// Delete a user with their API keys and local ratings.
#[derive(FromArgs)]
#[argh(subcommand, name = "delete")]
struct Delete {
    /// username
    #[argh(positional)]
    username: String,
}

/// Create an API key for a user and print it once.
#[derive(FromArgs)]
#[argh(subcommand, name = "api-key")]
struct ApiKey {
    /// username
    #[argh(positional)]
    username: String,
    /// label for the key
    #[argh(option, default = "String::from(\"default\")")]
    name: String,
}

/// List a user's API keys: ids and names (the keys aren't stored).
#[derive(FromArgs)]
#[argh(subcommand, name = "api-keys")]
struct ApiKeys {
    /// username
    #[argh(positional)]
    username: String,
}

/// Revoke one of a user's API keys.
#[derive(FromArgs)]
#[argh(subcommand, name = "revoke-key")]
struct RevokeKey {
    /// username
    #[argh(positional)]
    username: String,
    /// the key's id (see `api-keys`)
    #[argh(positional)]
    id: i64,
}

/// Back up a server, or restore a backup into it.
#[derive(FromArgs)]
#[argh(subcommand, name = "backup")]
pub struct Backup {
    /// the server's base URL (default: the RSUB_SERVER environment variable)
    #[argh(option)]
    server: Option<String>,
    /// an admin's API key (default: the RSUB_API_KEY environment variable)
    #[argh(option)]
    key: Option<String>,
    #[argh(subcommand)]
    cmd: BackupSub,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum BackupSub {
    Export(Export),
    Restore(Restore),
}

/// Download a backup: users, API keys, local ratings and the identity
/// ledger. Passwords stay encrypted: keep the server key (secret.key) too.
#[derive(FromArgs)]
#[argh(subcommand, name = "export")]
struct Export {
    /// the file to write
    #[argh(option, short = 'o')]
    out: PathBuf,
}

/// Merge a backup into the server. Users it already has are left as they
/// are; ledger keys it has keep their ids. Safe to repeat.
#[derive(FromArgs)]
#[argh(subcommand, name = "restore")]
struct Restore {
    /// the backup file
    #[argh(positional)]
    file: PathBuf,
}

#[derive(Deserialize)]
struct UserOut {
    id: i64,
    username: String,
    email: Option<String>,
    admin: bool,
    roles: Vec<String>,
    max_bitrate: u32,
}

#[derive(Deserialize)]
struct KeyOut {
    id: i64,
    name: String,
    created_at: i64,
    key: Option<String>,
}

/// `<base>/users[/<name>[/<rest>…]]`, the name escaped.
fn users_url(api: &Api, name: Option<&str>, rest: &[&str]) -> Result<Url, Error> {
    let mut url = Url::parse(&api.url("users"))?;
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| "the server URL can't take a path")?;
        path.extend(name);
        path.extend(rest);
    }
    Ok(url)
}

fn roles(list: &str) -> Vec<&str> {
    list.split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect()
}

/// A password typed at the terminal without echo, or a line piped to stdin.
fn read_password() -> Result<String, Error> {
    if std::io::stdin().is_terminal() {
        return Ok(rpassword::prompt_password("password: ")?);
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn print_users(users: &[UserOut]) {
    let width = users
        .iter()
        .map(|u| u.username.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);
    println!(
        "{:>4}  {:<width$}  {:<5}  {:>7}  EMAIL  ROLES",
        "ID", "USERNAME", "ADMIN", "KBPS"
    );
    for u in users {
        let kbps = match u.max_bitrate {
            0 => "-".to_owned(),
            n => n.to_string(),
        };
        println!(
            "{:>4}  {:<width$}  {:<5}  {:>7}  {}  {}",
            u.id,
            u.username,
            if u.admin { "yes" } else { "no" },
            kbps,
            u.email.as_deref().unwrap_or("-"),
            u.roles.join(",")
        );
    }
}

pub fn user(u: User) -> Result<(), Error> {
    let api = Api::connect(u.server, u.key)?;
    match u.cmd {
        UserSub::List(_) => {
            let users: Vec<UserOut> = api
                .send(api.http.get(users_url(&api, None, &[])?))?
                .json()?;
            print_users(&users);
        }
        UserSub::Show(s) => {
            let one: UserOut = api
                .send(api.http.get(users_url(&api, Some(&s.username), &[])?))?
                .json()?;
            print_users(&[one]);
        }
        UserSub::Add(a) => {
            let password = match a.password {
                Some(p) => p,
                None => read_password()?,
            };
            let mut body = json!({
                "username": a.username,
                "password": password,
                "admin": a.admin,
                "max_bitrate": a.max_bitrate,
            });
            if let Some(e) = &a.email {
                body["email"] = json!(e);
            }
            if let Some(r) = &a.roles {
                body["roles"] = json!(roles(r));
            }
            let made: UserOut = api
                .send(api.http.post(users_url(&api, None, &[])?).json(&body))?
                .json()?;
            println!(
                "created user '{}' (id {}{})",
                made.username,
                made.id,
                if made.admin { ", admin" } else { "" }
            );
        }
        UserSub::Update(c) => {
            let mut body = Map::new();
            if let Some(e) = c.email {
                body.insert("email".into(), json!(e));
            }
            if let Some(a) = c.admin {
                body.insert("admin".into(), json!(a));
            }
            if let Some(r) = &c.roles {
                body.insert("roles".into(), json!(roles(r)));
            }
            if let Some(b) = c.max_bitrate {
                body.insert("max_bitrate".into(), json!(b));
            }
            if body.is_empty() {
                return Err(
                    "nothing to change: give --email, --admin, --roles or --max-bitrate".into(),
                );
            }
            let url = users_url(&api, Some(&c.username), &[])?;
            let done: UserOut = api
                .send(api.http.patch(url).json(&Value::Object(body)))?
                .json()?;
            println!("updated user '{}'", done.username);
        }
        UserSub::Passwd(p) => {
            let password = match p.password {
                Some(p) => p,
                None => read_password()?,
            };
            let url = users_url(&api, Some(&p.username), &[])?;
            api.send(api.http.patch(url).json(&json!({ "password": password })))?;
            println!("password of '{}' changed", p.username);
        }
        UserSub::Delete(d) => {
            api.send(api.http.delete(users_url(&api, Some(&d.username), &[])?))?;
            println!("deleted user '{}'", d.username);
        }
        UserSub::ApiKey(k) => {
            let url = users_url(&api, Some(&k.username), &["api-keys"])?;
            let made: KeyOut = api
                .send(api.http.post(url).json(&json!({ "name": k.name })))?
                .json()?;
            println!("{}", made.key.unwrap_or_default());
            eprintln!(
                "API key '{}' (id {}) created for '{}'. It is shown only once.",
                made.name, made.id, k.username
            );
        }
        UserSub::ApiKeys(k) => {
            let url = users_url(&api, Some(&k.username), &["api-keys"])?;
            let keys: Vec<KeyOut> = api.send(api.http.get(url))?.json()?;
            println!("{:>4}  {:<16}  CREATED (UTC)", "ID", "NAME");
            for key in keys {
                println!(
                    "{:>4}  {:<16}  {}",
                    key.id,
                    key.name,
                    format_ms(key.created_at)
                );
            }
        }
        UserSub::RevokeKey(k) => {
            let id = k.id.to_string();
            let url = users_url(&api, Some(&k.username), &["api-keys", &id])?;
            api.send(api.http.delete(url))?;
            println!("revoked API key {} of '{}'", k.id, k.username);
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Restored {
    users_added: u64,
    users_kept: u64,
    api_keys_added: u64,
    ratings_added: u64,
    ratings_skipped: u64,
    settings_added: u64,
    identity_keys_read: u64,
    identity_keys_added: u64,
}

pub fn backup(b: Backup) -> Result<(), Error> {
    let api = Api::connect(b.server, b.key)?;
    match b.cmd {
        BackupSub::Export(e) => {
            let body = api.send(api.http.get(api.url("backup")))?.bytes()?;
            let count = |k: &str| {
                let tag = format!("{{\"{k}\":");
                body.split(|&c| c == b'\n')
                    .filter(|l| l.starts_with(tag.as_bytes()))
                    .count()
            };
            // It holds encrypted passwords and API-key hashes.
            rsub_core::crypto::create_private(&e.out)
                .and_then(|mut f| f.write_all(&body))
                .map_err(|err| format!("{}: {err}", e.out.display()))?;
            println!(
                "{}: {} users, {} API keys, {} ratings, {} identity keys",
                e.out.display(),
                count("user"),
                count("api_key"),
                count("artist_rating"),
                count("identity")
            );
        }
        BackupSub::Restore(r) => {
            let body = std::fs::read(&r.file).map_err(|e| format!("{}: {e}", r.file.display()))?;
            let got: Restored = api
                .send(
                    api.http
                        .post(api.url("backup"))
                        .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                        .body(body),
                )?
                .json()?;
            println!(
                "users: {} added, {} already there (kept as they are); API keys: {} added",
                got.users_added, got.users_kept, got.api_keys_added
            );
            println!(
                "ratings: {} added, {} skipped; settings: {} added; identity keys: {} of {} added",
                got.ratings_added,
                got.ratings_skipped,
                got.settings_added,
                got.identity_keys_added,
                got.identity_keys_read
            );
        }
    }
    Ok(())
}

/// A millisecond timestamp as UTC `YYYY-MM-DD HH:MM`.
fn format_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}
