//! One query source, two databases: queries are built once with sea-query and
//! rendered for the active dialect at call time.

#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("rsub-store needs at least one of the `sqlite` or `postgres` features");

use sea_query::{QueryStatementWriter, Value, Values};

use crate::{Result, StoreError};

#[cfg(feature = "postgres")]
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
#[cfg(feature = "sqlite")]
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};

/// A row type decodable from every enabled database.
#[cfg(all(feature = "sqlite", feature = "postgres"))]
pub trait DbRow:
    for<'r> sqlx::FromRow<'r, SqliteRow> + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin
{
}
#[cfg(all(feature = "sqlite", feature = "postgres"))]
impl<T> DbRow for T where
    T: for<'r> sqlx::FromRow<'r, SqliteRow> + for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin
{
}

#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub trait DbRow: for<'r> sqlx::FromRow<'r, SqliteRow> + Send + Unpin {}
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
impl<T> DbRow for T where T: for<'r> sqlx::FromRow<'r, SqliteRow> + Send + Unpin {}

#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
pub trait DbRow: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin {}
#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
impl<T> DbRow for T where T: for<'r> sqlx::FromRow<'r, PgRow> + Send + Unpin {}

#[derive(Clone, Debug)]
pub enum Db {
    /// SQLite with a single-connection writer (avoids SQLITE_BUSY) and a reader pool.
    #[cfg(feature = "sqlite")]
    Sqlite { read: SqlitePool, write: SqlitePool },
    #[cfg(feature = "postgres")]
    Postgres(PgPool),
}

/// Postgres text can't hold NUL, which stray file tags carry: drop it in every
/// dialect so both store the same.
fn without_nul(s: String) -> String {
    if s.contains('\0') {
        s.replace('\0', "")
    } else {
        s
    }
}

/// Bind sea-query values onto an sqlx query. A macro rather than a generic fn so
/// each dialect gets concrete `Encode` impls. Types are widened where a dialect
/// lacks them (Postgres has no `i8`/unsigned; SQLite has no `u64`).
macro_rules! bind_values {
    ($query:expr, $values:expr) => {{
        let mut q = $query;
        for v in $values.0 {
            q = match v {
                Value::Bool(v) => q.bind(v),
                Value::TinyInt(v) => q.bind(v.map(i16::from)),
                Value::SmallInt(v) => q.bind(v),
                Value::Int(v) => q.bind(v),
                Value::BigInt(v) => q.bind(v),
                Value::TinyUnsigned(v) => q.bind(v.map(i16::from)),
                Value::SmallUnsigned(v) => q.bind(v.map(i32::from)),
                Value::Unsigned(v) => q.bind(v.map(i64::from)),
                Value::BigUnsigned(v) => q.bind(v.map(|x| i64::try_from(x).unwrap_or(i64::MAX))),
                Value::Float(v) => q.bind(v),
                Value::Double(v) => q.bind(v),
                Value::String(v) => q.bind(v.map(without_nul)),
                Value::Char(v) => q.bind(v.map(String::from)),
                Value::Enum(e) => q.bind(match e {
                    sea_query::OptionEnum::Some(e) => Some(e.value.into_owned()),
                    sea_query::OptionEnum::None(_) => None,
                }),
                Value::Bytes(v) => q.bind(v),
                #[allow(unreachable_patterns)]
                other => panic!("rsub-store: unsupported sea-query value {other:?}"),
            };
        }
        q
    }};
}

/// Load sqlite-vec into every SQLite connection opened from now on.
#[cfg(feature = "sqlite")]
fn register_sqlite_vec() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        type Init = unsafe extern "C" fn(
            *mut libsqlite3_sys::sqlite3,
            *mut *mut std::ffi::c_char,
            *const libsqlite3_sys::sqlite3_api_routines,
        ) -> std::ffi::c_int;
        // SAFETY: sqlite-vec declares its entry point without parameters; it is
        // the standard extension entry point, which this type describes.
        // sqlite-vec is compiled against the SQLite libsqlite3-sys links.
        unsafe {
            let init: Init = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
            libsqlite3_sys::sqlite3_auto_extension(Some(init));
        }
    });
}

/// Create pgvector when the database lacks it (the migration only makes sure),
/// explaining what to do when that fails.
#[cfg(feature = "postgres")]
async fn create_pgvector(pool: &PgPool) -> Result<()> {
    let installed: Option<(String,)> =
        sqlx::query_as("SELECT extname::text FROM pg_extension WHERE extname = 'vector'")
            .fetch_optional(pool)
            .await?;
    if installed.is_some() {
        return Ok(());
    }
    sqlx::query("CREATE EXTENSION IF NOT EXISTS vector")
        .execute(pool)
        .await
        .map_err(|e| {
            StoreError::Config(format!(
                "Postgres needs the pgvector extension ({e}). Install it on the server                  (the pgvector/pgvector images have it), then run                  `CREATE EXTENSION vector;` in this database as a superuser"
            ))
        })?;
    Ok(())
}

impl Db {
    /// Connect by URL scheme: `sqlite:` or `postgres:`/`postgresql:`.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Db> {
        let max_connections = max_connections.max(1);
        if url.starts_with("sqlite:") {
            #[cfg(feature = "sqlite")]
            return Self::connect_sqlite(url, max_connections).await;
            #[cfg(not(feature = "sqlite"))]
            return Err(StoreError::Config(
                "built without the `sqlite` feature".into(),
            ));
        }
        if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            #[cfg(feature = "postgres")]
            {
                let pool = PgPoolOptions::new()
                    .max_connections(max_connections)
                    .connect(url)
                    .await?;
                return Ok(Db::Postgres(pool));
            }
            #[cfg(not(feature = "postgres"))]
            return Err(StoreError::Config(
                "built without the `postgres` feature".into(),
            ));
        }
        Err(StoreError::Config(format!(
            "unsupported database URL scheme: {url}"
        )))
    }

    #[cfg(feature = "sqlite")]
    async fn connect_sqlite(url: &str, max_connections: u32) -> Result<Db> {
        use std::str::FromStr;
        use std::time::Duration;

        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

        register_sqlite_vec();

        let in_memory = url.contains(":memory:") || url.contains("mode=memory");
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .synchronous(SqliteSynchronous::Normal)
            .pragma("cache_size", "-8000")
            // Vector searches scan every stored vector; reading them through a
            // shared memory map rather than each connection's page cache makes
            // a scan several times faster.
            .pragma("mmap_size", "1073741824");

        if in_memory {
            // Each connection would get its own in-memory database: use exactly one, forever.
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(opts)
                .await?;
            return Ok(Db::Sqlite {
                read: pool.clone(),
                write: pool,
            });
        }

        // The writer connects first so the file exists and is in WAL mode before readers open it.
        let opts = opts.journal_mode(SqliteJournalMode::Wal);
        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts.clone())
            .await?;
        let read = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(opts)
            .await?;
        Ok(Db::Sqlite { read, write })
    }

    pub async fn migrate(&self) -> Result<()> {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { write, .. } => sqlx::migrate!("./migrations/sqlite").run(write).await?,
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => {
                create_pgvector(pool).await?;
                sqlx::migrate!("./migrations/postgres").run(pool).await?
            }
        }
        Ok(())
    }

    pub fn dialect(&self) -> &'static str {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { .. } => "sqlite",
            #[cfg(feature = "postgres")]
            Db::Postgres(_) => "postgres",
        }
    }

    pub async fn close(&self) {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { read, write } => {
                read.close().await;
                write.close().await;
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => pool.close().await,
        }
    }

    fn build(&self, q: &impl QueryStatementWriter) -> (String, Values) {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { .. } => q.build(sea_query::SqliteQueryBuilder),
            #[cfg(feature = "postgres")]
            Db::Postgres(_) => q.build(sea_query::PostgresQueryBuilder),
        }
    }

    pub async fn fetch_all<T: DbRow>(&self, q: &impl QueryStatementWriter) -> Result<Vec<T>> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { read, .. } => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_all(read)
                    .await?
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_all(pool)
                    .await?
            }
        })
    }

    pub async fn fetch_optional<T: DbRow>(
        &self,
        q: &impl QueryStatementWriter,
    ) -> Result<Option<T>> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { read, .. } => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_optional(read)
                    .await?
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_optional(pool)
                    .await?
            }
        })
    }

    pub async fn fetch_one<T: DbRow>(&self, q: &impl QueryStatementWriter) -> Result<T> {
        self.fetch_optional(q)
            .await?
            .ok_or(StoreError::Sqlx(sqlx::Error::RowNotFound))
    }

    /// Run a write returning rows (e.g. `INSERT … RETURNING id`) on the writer.
    pub async fn write_fetch_one<T: DbRow>(&self, q: &impl QueryStatementWriter) -> Result<T> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { write, .. } => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_one(write)
                    .await?
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_one(pool)
                    .await?
            }
        })
    }

    /// Run a write; returns the number of affected rows.
    pub async fn execute(&self, q: &impl QueryStatementWriter) -> Result<u64> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { write, .. } => bind_values!(sqlx::query(sql), values)
                .execute(write)
                .await?
                .rows_affected(),
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => bind_values!(sqlx::query(sql), values)
                .execute(pool)
                .await?
                .rows_affected(),
        })
    }

    /// Start a transaction on the writer.
    pub async fn begin(&self) -> Result<Tx> {
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { write, .. } => Tx::Sqlite(write.begin().await?),
            #[cfg(feature = "postgres")]
            Db::Postgres(pool) => Tx::Postgres(pool.begin().await?),
        })
    }
}

/// A write transaction. Rolled back on drop unless [`Tx::commit`] is called.
pub enum Tx {
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
}

impl Tx {
    fn build(&self, q: &impl QueryStatementWriter) -> (String, Values) {
        match self {
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(_) => q.build(sea_query::SqliteQueryBuilder),
            #[cfg(feature = "postgres")]
            Tx::Postgres(_) => q.build(sea_query::PostgresQueryBuilder),
        }
    }

    pub async fn fetch_all<T: DbRow>(&mut self, q: &impl QueryStatementWriter) -> Result<Vec<T>> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(t) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_all(&mut **t)
                    .await?
            }
            #[cfg(feature = "postgres")]
            Tx::Postgres(t) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_all(&mut **t)
                    .await?
            }
        })
    }

    pub async fn fetch_one<T: DbRow>(&mut self, q: &impl QueryStatementWriter) -> Result<T> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(t) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_one(&mut **t)
                    .await?
            }
            #[cfg(feature = "postgres")]
            Tx::Postgres(t) => {
                bind_values!(sqlx::query_as::<_, T>(sql), values)
                    .fetch_one(&mut **t)
                    .await?
            }
        })
    }

    pub async fn execute(&mut self, q: &impl QueryStatementWriter) -> Result<u64> {
        let (sql, values) = self.build(q);
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(t) => bind_values!(sqlx::query(sql), values)
                .execute(&mut **t)
                .await?
                .rows_affected(),
            #[cfg(feature = "postgres")]
            Tx::Postgres(t) => bind_values!(sqlx::query(sql), values)
                .execute(&mut **t)
                .await?
                .rows_affected(),
        })
    }

    /// Run SQL that is valid in every dialect and has no bind parameters.
    /// Callers only interpolate integers they own.
    pub async fn execute_portable(&mut self, sql: String) -> Result<u64> {
        let sql = sqlx::AssertSqlSafe(sql);
        Ok(match self {
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(t) => sqlx::query(sql).execute(&mut **t).await?.rows_affected(),
            #[cfg(feature = "postgres")]
            Tx::Postgres(t) => sqlx::query(sql).execute(&mut **t).await?.rows_affected(),
        })
    }

    pub async fn commit(self) -> Result<()> {
        match self {
            #[cfg(feature = "sqlite")]
            Tx::Sqlite(t) => t.commit().await?,
            #[cfg(feature = "postgres")]
            Tx::Postgres(t) => t.commit().await?,
        }
        Ok(())
    }
}
