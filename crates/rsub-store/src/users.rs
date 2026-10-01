use rsub_core::{Roles, User, now_ms};
use std::collections::{HashMap, HashSet};

use sea_query::{Expr, ExprTrait, OnConflict, Order, Query, SelectStatement};

use crate::{Db, IdRow, Result, StoreError, Tx};

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    username: String,
    password_enc: Vec<u8>,
    email: Option<String>,
    roles: i64,
    max_bitrate: i64,
    created_at: i64,
    updated_at: i64,
}

impl From<UserRow> for User {
    fn from(r: UserRow) -> Self {
        User {
            id: r.id,
            username: r.username,
            email: r.email,
            roles: Roles(r.roles as u32),
            max_bitrate: r.max_bitrate.clamp(0, u32::MAX as i64) as u32,
            password_enc: r.password_enc,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct CountRow {
    n: i64,
}

pub struct NewUser<'a> {
    pub username: &'a str,
    /// Already encrypted with `Purpose::Password` and the username as context.
    pub password_enc: Vec<u8>,
    pub email: Option<&'a str>,
    pub roles: Roles,
    pub max_bitrate: u32,
}

/// A username as stored in `users.username_key`: usernames are unique and
/// matched regardless of case.
pub fn username_key(username: &str) -> String {
    username.to_lowercase()
}

fn select_users() -> SelectStatement {
    Query::select()
        .columns(
            [
                "id",
                "username",
                "password_enc",
                "email",
                "roles",
                "max_bitrate",
                "created_at",
                "updated_at",
            ]
            .map(|c| ("users", c)),
        )
        .from("users")
        .to_owned()
}

impl Db {
    /// Returns the new user id; `StoreError::Conflict` if the username exists.
    pub async fn create_user(&self, u: NewUser<'_>) -> Result<i64> {
        let now = now_ms();
        let q = Query::insert()
            .into_table("users")
            .columns([
                "username",
                "username_key",
                "password_enc",
                "email",
                "roles",
                "max_bitrate",
                "created_at",
                "updated_at",
            ])
            .values_panic([
                u.username.into(),
                username_key(u.username).into(),
                u.password_enc.into(),
                u.email.map(str::to_owned).into(),
                i64::from(u.roles.0).into(),
                i64::from(u.max_bitrate).into(),
                now.into(),
                now.into(),
            ])
            .returning_col("id")
            .to_owned();
        Ok(self.write_fetch_one::<IdRow>(&q).await?.id)
    }

    /// The user named `username`, in any case.
    pub async fn user_by_username(&self, username: &str) -> Result<Option<User>> {
        let q = select_users()
            .and_where(Expr::col("username_key").eq(username_key(username)))
            .to_owned();
        Ok(self.fetch_optional::<UserRow>(&q).await?.map(User::from))
    }

    pub async fn list_users(&self) -> Result<Vec<User>> {
        let q = select_users()
            .order_by(("users", "username"), Order::Asc)
            .to_owned();
        Ok(self
            .fetch_all::<UserRow>(&q)
            .await?
            .into_iter()
            .map(User::from)
            .collect())
    }

    pub async fn count_users(&self) -> Result<i64> {
        let q = Query::select()
            .expr_as(Expr::col("id").count(), "n")
            .from("users")
            .to_owned();
        Ok(self
            .fetch_optional::<CountRow>(&q)
            .await?
            .map_or(0, |r| r.n))
    }

    /// Store an API key by its SHA-256 hash; returns the key id.
    pub async fn create_api_key(&self, user_id: i64, name: &str, key_hash: &[u8]) -> Result<i64> {
        let q = Query::insert()
            .into_table("api_keys")
            .columns(["user_id", "name", "key_hash", "created_at"])
            .values_panic([
                user_id.into(),
                name.into(),
                key_hash.to_vec().into(),
                now_ms().into(),
            ])
            .returning_col("id")
            .to_owned();
        Ok(self.write_fetch_one::<IdRow>(&q).await?.id)
    }

    pub async fn user_by_api_key_hash(&self, key_hash: &[u8]) -> Result<Option<User>> {
        let q = select_users()
            .inner_join(
                "api_keys",
                Expr::col(("api_keys", "user_id")).equals(("users", "id")),
            )
            .and_where(Expr::col(("api_keys", "key_hash")).eq(key_hash.to_vec()))
            .to_owned();
        Ok(self.fetch_optional::<UserRow>(&q).await?.map(User::from))
    }
}

/// Changes to a user; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct UserUpdate {
    /// Already encrypted with `Purpose::Password` and the username as context.
    pub password_enc: Option<Vec<u8>>,
    /// `Some(None)` clears the email.
    pub email: Option<Option<String>>,
    pub roles: Option<Roles>,
    pub max_bitrate: Option<u32>,
}

/// An API key as listed: the key itself is never stored.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ApiKeyInfo {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
}

/// A user as a backup carries them, password still encrypted.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackupUser {
    pub username: String,
    pub password_enc: Vec<u8>,
    pub email: Option<String>,
    pub roles: i64,
    pub max_bitrate: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// An API key as a backup carries it: by its hash.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackupApiKey {
    pub username: String,
    pub name: String,
    pub key_hash: Vec<u8>,
    pub created_at: i64,
}

/// A local artist rating, by the artist's public id.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct BackupRating {
    pub username: String,
    pub artist: String,
    pub rating: i64,
    pub rated_at: i64,
}

/// What [`Db::restore_accounts`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountRestore {
    pub users_added: u64,
    /// Users whose name this database already has: left as they are.
    pub users_kept: u64,
    pub api_keys_added: u64,
    pub ratings_added: u64,
    /// Ratings of users neither the backup nor this database has.
    pub ratings_skipped: u64,
}

#[derive(sqlx::FromRow)]
struct NamedId {
    id: i64,
    name: String,
}

impl Db {
    /// Apply `u` to the user `id`. Returns false if there is no such user;
    /// `StoreError::LastAdmin` if it would leave no admin.
    pub async fn update_user(&self, id: i64, u: UserUpdate) -> Result<bool> {
        let demotes = u.roles.is_some_and(|r| !r.contains(Roles::ADMIN));
        let mut q = Query::update();
        q.table("users")
            .value("updated_at", now_ms())
            .and_where(Expr::col("id").eq(id));
        if let Some(p) = u.password_enc {
            q.value("password_enc", p);
        }
        if let Some(e) = u.email {
            q.value("email", e);
        }
        if let Some(r) = u.roles {
            q.value("roles", i64::from(r.0));
        }
        if let Some(b) = u.max_bitrate {
            q.value("max_bitrate", i64::from(b));
        }
        let n = if demotes {
            keeping_an_admin(self, &q).await?
        } else {
            self.execute(&q).await?
        };
        Ok(n > 0)
    }

    /// Delete the user `id` with their API keys and local ratings. Returns
    /// false if there is no such user; `StoreError::LastAdmin` if they are
    /// the last admin.
    pub async fn delete_user(&self, id: i64) -> Result<bool> {
        let q = Query::delete()
            .from_table("users")
            .and_where(Expr::col("id").eq(id))
            .to_owned();
        Ok(keeping_an_admin(self, &q).await? > 0)
    }

    /// The user's API keys, oldest first.
    pub async fn api_keys(&self, user_id: i64) -> Result<Vec<ApiKeyInfo>> {
        self.fetch_all(
            &Query::select()
                .columns(["id", "name", "created_at"])
                .from("api_keys")
                .and_where(Expr::col("user_id").eq(user_id))
                .order_by("id", Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Revoke one of the user's API keys. Returns false if they have no such key.
    pub async fn delete_api_key(&self, user_id: i64, key_id: i64) -> Result<bool> {
        let n = self
            .execute(
                &Query::delete()
                    .from_table("api_keys")
                    .and_where(Expr::col("user_id").eq(user_id))
                    .and_where(Expr::col("id").eq(key_id))
                    .to_owned(),
            )
            .await?;
        Ok(n > 0)
    }

    /// Every user, by name, for a backup.
    pub async fn backup_users(&self) -> Result<Vec<BackupUser>> {
        self.fetch_all(
            &Query::select()
                .columns([
                    "username",
                    "password_enc",
                    "email",
                    "roles",
                    "max_bitrate",
                    "created_at",
                    "updated_at",
                ])
                .from("users")
                .order_by("username", Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Every API key, by user and age, for a backup.
    pub async fn backup_api_keys(&self) -> Result<Vec<BackupApiKey>> {
        self.fetch_all(
            &Query::select()
                .column(("users", "username"))
                .columns([
                    ("api_keys", "name"),
                    ("api_keys", "key_hash"),
                    ("api_keys", "created_at"),
                ])
                .from("api_keys")
                .inner_join(
                    "users",
                    Expr::col(("users", "id")).equals(("api_keys", "user_id")),
                )
                .order_by(("users", "username"), Order::Asc)
                .order_by(("api_keys", "id"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Every local artist rating, for a backup.
    pub async fn backup_ratings(&self) -> Result<Vec<BackupRating>> {
        self.fetch_all(
            &Query::select()
                .column(("users", "username"))
                .expr_as(Expr::col(("artist_ratings", "artist_public_id")), "artist")
                .columns([("artist_ratings", "rating"), ("artist_ratings", "rated_at")])
                .from("artist_ratings")
                .inner_join(
                    "users",
                    Expr::col(("users", "id")).equals(("artist_ratings", "user_id")),
                )
                .order_by(("users", "username"), Order::Asc)
                .order_by(("artist_ratings", "artist_public_id"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Merge a backup's accounts in, in one transaction:
    /// - users this database doesn't have are added with their API keys;
    ///   users it has (by name, in any case) are left as they are, keys
    ///   included, so a revoked key doesn't come back;
    /// - ratings are added where the user hasn't rated the artist here. They
    ///   are kept by public id, so the artist needn't be synced yet.
    pub async fn restore_accounts(
        &self,
        users: &[BackupUser],
        keys: &[BackupApiKey],
        ratings: &[BackupRating],
    ) -> Result<AccountRestore> {
        let mut out = AccountRestore::default();
        let mut tx = self.begin().await?;
        let mut added = HashSet::new();
        for u in users {
            let n = tx
                .execute(
                    &Query::insert()
                        .into_table("users")
                        .columns([
                            "username",
                            "username_key",
                            "password_enc",
                            "email",
                            "roles",
                            "max_bitrate",
                            "created_at",
                            "updated_at",
                        ])
                        .values_panic([
                            u.username.as_str().into(),
                            username_key(&u.username).into(),
                            u.password_enc.clone().into(),
                            u.email.clone().into(),
                            u.roles.into(),
                            u.max_bitrate.into(),
                            u.created_at.into(),
                            u.updated_at.into(),
                        ])
                        .on_conflict(OnConflict::column("username_key").do_nothing().to_owned())
                        .to_owned(),
                )
                .await?;
            if n > 0 {
                out.users_added += 1;
                added.insert(username_key(&u.username));
            } else {
                out.users_kept += 1;
            }
        }
        let users: HashMap<String, i64> = tx
            .fetch_all::<NamedId>(
                &Query::select()
                    .column("id")
                    .expr_as(Expr::col("username_key"), "name")
                    .from("users")
                    .to_owned(),
            )
            .await?
            .into_iter()
            .map(|r| (r.name, r.id))
            .collect();
        for k in keys {
            let name = username_key(&k.username);
            if !added.contains(&name) {
                continue;
            }
            let Some(&user) = users.get(&name) else {
                continue;
            };
            out.api_keys_added += tx
                .execute(
                    &Query::insert()
                        .into_table("api_keys")
                        .columns(["user_id", "name", "key_hash", "created_at"])
                        .values_panic([
                            user.into(),
                            k.name.as_str().into(),
                            k.key_hash.clone().into(),
                            k.created_at.into(),
                        ])
                        .on_conflict(OnConflict::column("key_hash").do_nothing().to_owned())
                        .to_owned(),
                )
                .await?;
        }
        for r in ratings {
            let Some(&user) = users.get(&username_key(&r.username)) else {
                out.ratings_skipped += 1;
                continue;
            };
            out.ratings_added += tx
                .execute(
                    &Query::insert()
                        .into_table("artist_ratings")
                        .columns(["user_id", "artist_public_id", "rating", "rated_at"])
                        .values_panic([
                            user.into(),
                            r.artist.as_str().into(),
                            r.rating.clamp(1, 5).into(),
                            r.rated_at.into(),
                        ])
                        .on_conflict(
                            OnConflict::columns(["user_id", "artist_public_id"])
                                .do_nothing()
                                .to_owned(),
                        )
                        .to_owned(),
                )
                .await?;
        }
        tx.commit().await?;
        Ok(out)
    }
}

async fn count_admins(tx: &mut Tx) -> Result<i64> {
    let row: CountRow = tx
        .fetch_one(
            &Query::select()
                .expr_as(Expr::col("id").count(), "n")
                .from("users")
                .and_where(Expr::cust(format!("(roles & {}) <> 0", Roles::ADMIN.0)))
                .to_owned(),
        )
        .await?;
    Ok(row.n)
}

/// Run `q` (changing or deleting one user) in a transaction, refusing with
/// `StoreError::LastAdmin` if it took the last admin away: a database without
/// one can only be fixed from the command line. Returns the rows changed.
async fn keeping_an_admin(db: &Db, q: &impl sea_query::QueryStatementWriter) -> Result<u64> {
    let mut tx = db.begin().await?;
    let before = count_admins(&mut tx).await?;
    let n = tx.execute(q).await?;
    if n > 0 && before > 0 && count_admins(&mut tx).await? == 0 {
        // Rolled back on drop.
        return Err(StoreError::LastAdmin);
    }
    tx.commit().await?;
    Ok(n)
}
