//! Helpers for tests in this and dependent crates: fresh databases per test.

use crate::Db;

/// A migrated in-memory SQLite database.
#[cfg(feature = "sqlite")]
pub async fn sqlite() -> Db {
    let db = Db::connect("sqlite::memory:", 1).await.expect("sqlite");
    db.migrate().await.expect("migrate");
    db
}

/// A migrated Postgres database in a fresh schema, if `RSUB_TEST_PG_URL` is set.
/// Each call gets its own schema so tests can run in parallel on one server.
#[cfg(feature = "postgres")]
pub async fn postgres() -> Option<Db> {
    let url = std::env::var("RSUB_TEST_PG_URL").ok()?;
    let schema = format!(
        "t{}",
        hex::encode(rsub_core::crypto::random_bytes::<6>().expect("random"))
    );
    let admin = Db::connect(&url, 1).await.expect("postgres");
    #[allow(irrefutable_let_patterns)] // when built without sqlite
    if let Db::Postgres(p) = &admin {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(p)
            .await
            .expect("create schema");
        // pgvector goes in `public`, shared by every test schema. Tests racing
        // to create it may fail; one of them will have.
        let _ = sqlx::query("CREATE EXTENSION IF NOT EXISTS vector SCHEMA public")
            .execute(p)
            .await;
    }
    admin.close().await;
    let sep = if url.contains('?') { '&' } else { '?' };
    let db = Db::connect(
        &format!("{url}{sep}options=-c%20search_path%3D{schema}%2Cpublic"),
        2,
    )
    .await
    .expect("postgres");
    db.migrate().await.expect("migrate");
    Some(db)
}

/// Expands each body into one `#[tokio::test]` per enabled database. The calling
/// crate needs `tokio` (with `macros`) and `sqlite`/`postgres` features that
/// forward to `rsub-store`.
#[macro_export]
macro_rules! store_tests {
    ($($name:ident($db:ident) $body:block)*) => {
        mod sqlite {
            #[allow(unused_imports)]
            use super::*;
            $(#[tokio::test] async fn $name() {
                let $db = $crate::testing::sqlite().await;
                $body
            })*
        }
        #[cfg(feature = "postgres")]
        mod postgres {
            #[allow(unused_imports)]
            use super::*;
            $(#[tokio::test] async fn $name() {
                let Some($db) = $crate::testing::postgres().await else { return };
                $body
            })*
        }
    };
}
