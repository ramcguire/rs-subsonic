//! What user state needs from the store: backend keys of index rows and back,
//! and the ratings of virtual artists, which only rs-subsonic has.

use rsub_core::Kind;
use sea_query::{Expr, ExprTrait, JoinType, OnConflict, Order, Query, SelectStatement};

use crate::browse::{albums_select, artists_select, tracks_select};
use crate::{AlbumRow, ArtistRow, CHUNK, Db, Result, TrackRow, catalog_table};

/// A virtual artist's rating by one user; 5 is a star.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LocalRating {
    pub artist_id: i64,
    pub rating: i64,
    pub rated_at: i64,
}

/// Where an item lives in its backend.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RemoteItem {
    pub id: i64,
    pub source_id: i64,
    pub library_id: i64,
    /// `None` for virtual artists.
    pub remote_key: Option<String>,
}

/// Restrict a select to rows of `source` whose `remote_key` is in `keys`. A
/// subquery rather than a join, since some selects use unqualified columns.
fn by_remote(
    mut q: SelectStatement,
    alias: &'static str,
    source: i64,
    keys: &[String],
) -> SelectStatement {
    q.and_where(
        Expr::col((alias, "library_id")).in_subquery(
            Query::select()
                .column("id")
                .from("libraries")
                .and_where(Expr::col("source_id").eq(source))
                .to_owned(),
        ),
    )
    .and_where(Expr::col((alias, "remote_key")).is_in(keys.iter().map(String::as_str)));
    q
}

impl Db {
    /// Where live items are in their backends; deleted items are left out.
    pub async fn remote_items(&self, kind: Kind, ids: &[i64]) -> Result<Vec<RemoteItem>> {
        let Some(table) = catalog_table(kind).filter(|_| !ids.is_empty()) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(CHUNK) {
            let rows: Vec<RemoteItem> = self
                .fetch_all(
                    &Query::select()
                        .columns([("x", "id"), ("x", "library_id"), ("x", "remote_key")])
                        .column(("l", "source_id"))
                        .from_as(table, "x")
                        .join_as(
                            JoinType::InnerJoin,
                            "libraries",
                            "l",
                            Expr::col(("l", "id")).equals(("x", "library_id")),
                        )
                        .and_where(Expr::col(("x", "id")).is_in(chunk.iter().copied()))
                        .and_where(Expr::col(("x", "deleted_at")).is_null())
                        .to_owned(),
                )
                .await?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// Live artists of `source` with these backend keys, in no particular order.
    pub async fn artists_by_remote(&self, source: i64, keys: &[String]) -> Result<Vec<ArtistRow>> {
        let mut out = Vec::new();
        for chunk in keys.chunks(CHUNK) {
            let q = by_remote(artists_select(), "artists", source, chunk);
            out.extend(self.fetch_all::<ArtistRow>(&q).await?);
        }
        Ok(out)
    }

    pub async fn albums_by_remote(&self, source: i64, keys: &[String]) -> Result<Vec<AlbumRow>> {
        let mut out = Vec::new();
        for chunk in keys.chunks(CHUNK) {
            let q = by_remote(albums_select(), "a", source, chunk);
            out.extend(self.fetch_all::<AlbumRow>(&q).await?);
        }
        Ok(out)
    }

    pub async fn tracks_by_remote(&self, source: i64, keys: &[String]) -> Result<Vec<TrackRow>> {
        let mut out = Vec::new();
        for chunk in keys.chunks(CHUNK) {
            let q = by_remote(tracks_select(), "t", source, chunk);
            out.extend(self.fetch_all::<TrackRow>(&q).await?);
        }
        Ok(out)
    }

    /// Rate the virtual artist `artist` 1–5; 0 clears the rating.
    pub async fn rate_locally(&self, user: i64, artist: i64, rating: u8) -> Result<()> {
        #[derive(sqlx::FromRow)]
        struct PublicIdRow {
            public_id: String,
        }
        let Some(PublicIdRow { public_id }) = self
            .fetch_optional(
                &Query::select()
                    .column("public_id")
                    .from("artists")
                    .and_where(Expr::col("id").eq(artist))
                    .to_owned(),
            )
            .await?
        else {
            return Ok(());
        };
        if rating == 0 {
            self.execute(
                &Query::delete()
                    .from_table("artist_ratings")
                    .and_where(Expr::col("user_id").eq(user))
                    .and_where(Expr::col("artist_public_id").eq(public_id))
                    .to_owned(),
            )
            .await?;
            return Ok(());
        }
        let q = Query::insert()
            .into_table("artist_ratings")
            .columns(["user_id", "artist_public_id", "rating", "rated_at"])
            .values_panic([
                user.into(),
                public_id.into(),
                i64::from(Ord::min(rating, 5)).into(),
                rsub_core::now_ms().into(),
            ])
            .on_conflict(
                OnConflict::columns(["user_id", "artist_public_id"])
                    .update_columns(["rating", "rated_at"])
                    .to_owned(),
            )
            .to_owned();
        self.execute(&q).await?;
        Ok(())
    }

    /// Local ratings of these artists; unrated artists are absent.
    pub async fn local_ratings(&self, user: i64, artists: &[i64]) -> Result<Vec<LocalRating>> {
        let mut out = Vec::new();
        for chunk in artists.chunks(CHUNK) {
            let rows: Vec<LocalRating> = self
                .fetch_all(
                    &Query::select()
                        .expr_as(Expr::col(("a", "id")), "artist_id")
                        .columns([("r", "rating"), ("r", "rated_at")])
                        .from_as("artist_ratings", "r")
                        .join_as(
                            JoinType::InnerJoin,
                            "artists",
                            "a",
                            Expr::col(("a", "public_id")).equals(("r", "artist_public_id")),
                        )
                        .and_where(Expr::col(("r", "user_id")).eq(user))
                        .and_where(Expr::col(("a", "id")).is_in(chunk.iter().copied()))
                        .to_owned(),
                )
                .await?;
            out.extend(rows);
        }
        Ok(out)
    }

    /// Live virtual artists `user` starred locally (rated 5), newest first.
    pub async fn locally_starred(&self, user: i64, library: Option<i64>) -> Result<Vec<ArtistRow>> {
        let mut q = artists_select();
        q.join_as(
            JoinType::InnerJoin,
            "artist_ratings",
            "r",
            Expr::col(("r", "artist_public_id")).equals(("artists", "public_id")),
        )
        .and_where(Expr::col(("r", "user_id")).eq(user))
        // An artist that has since turned up in the backend is rated there.
        .and_where(Expr::col(("artists", "remote_key")).is_null())
        .and_where(Expr::col(("r", "rating")).eq(5))
        .order_by(("r", "rated_at"), Order::Desc)
        .order_by(("artists", "id"), Order::Asc);
        if let Some(lib) = library {
            q.and_where(Expr::col(("artists", "library_id")).eq(lib));
        }
        self.fetch_all(&q).await
    }
}
