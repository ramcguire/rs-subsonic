//! Persistence for rs-subsonic on SQLite and/or Postgres.

mod analysis;
mod browse;
mod catalog;
mod db;
mod identity;
mod ids;
mod related;
mod settings;
mod state;
mod tags;
pub mod testing;
mod users;

pub use analysis::{
    AnalysisWrite, AnalyzerCounts, FileStampRow, FileToAnalyze, Metric, Neighbour, PairDistances,
    StoredAnalysis, Vector,
};
pub use browse::{
    AlbumOrder, AlbumRow, ArtSource, ArtistRow, CreditRow, GenreRow, Page, TagLink, TrackRow,
};
pub use catalog::{Library, SweepStats, SyncCtx};
pub use db::{Db, DbRow, Tx};
pub use identity::LedgerEntry;
pub use related::RelatedArtist;
pub use state::{LocalRating, RemoteItem};
pub use tags::{CachedFile, FileTagsWrite};
pub use users::{
    AccountRestore, ApiKeyInfo, BackupApiKey, BackupRating, BackupUser, NewUser, UserUpdate,
    username_key,
};

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Values per `IN (…)` list, well under both engines' bind-parameter limits.
const CHUNK: usize = 500;

/// The table holding a catalog kind; other kinds have no rows of their own.
fn catalog_table(kind: rsub_core::Kind) -> Option<&'static str> {
    use rsub_core::Kind;
    match kind {
        Kind::Artist => Some("artists"),
        Kind::Album => Some("albums"),
        Kind::Track => Some("tracks"),
        Kind::Playlist | Kind::Share | Kind::PodcastEpisode => None,
    }
}

/// A row's id, from `INSERT … RETURNING id` and the like.
#[derive(sqlx::FromRow)]
struct IdRow {
    id: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database configuration: {0}")]
    Config(String),
    #[error("invalid data: {0}")]
    Invalid(String),
    /// A unique constraint was violated; the database's message names it.
    #[error("already exists ({0})")]
    Conflict(String),
    /// An incremental pass met an id it can only settle knowing every item
    /// the backend lists; a full sync can.
    #[error("needs a full sync: {0}")]
    NeedsFullSync(String),
    /// The change would leave no admin.
    #[error("the last admin can't be removed or demoted")]
    LastAdmin,
    #[error(transparent)]
    Sqlx(sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(transparent)]
    Crypto(#[from] rsub_core::crypto::CryptoError),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        match e.as_database_error() {
            Some(db) if db.is_unique_violation() => StoreError::Conflict(db.message().to_owned()),
            _ => StoreError::Sqlx(e),
        }
    }
}
