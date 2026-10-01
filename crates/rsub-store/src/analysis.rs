//! Stored audio analysis (`file_analysis`): a vector per library file and
//! analyzer, keyed like the tag cache so it survives catalog rewrites, and
//! stamped with the file's size and mtime from the tag pass. Vectors are
//! searched in the database: with sqlite-vec's distance functions over f32
//! BLOBs, or pgvector's operators.

use rsub_core::now_ms;
use sea_query::{Expr, ExprTrait, JoinType, OnConflict, Order, Query};

use crate::{CHUNK, Db, IdRow, Result, StoreError};

/// How vectors of an analyzer are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// Unit vectors; similarity is their cosine.
    Cosine,
    /// Distance is Euclidean (the analyzer pre-weights the features);
    /// similarity is `exp(-2d)`.
    Euclidean,
}

impl Metric {
    /// A distance, smaller is closer: what the database computes.
    pub fn distance(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            Metric::Cosine => {
                let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
                let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
                1.0 - dot / (norm(a) * norm(b))
            }
            Metric::Euclidean => a
                .iter()
                .zip(b)
                .map(|(x, y)| (x - y) * (x - y))
                .sum::<f32>()
                .sqrt(),
        }
    }

    /// The OpenSubsonic `similarity` for a distance: 1 for the same sound,
    /// towards 0 for the most different.
    pub fn similarity(self, distance: f32) -> f32 {
        match self {
            Metric::Cosine => (1.0 - distance).clamp(0.0, 1.0),
            Metric::Euclidean => (-2.0 * distance).exp(),
        }
    }
}

/// A stored vector. Read from a BLOB of little-endian `f32`s on SQLite, and
/// from pgvector's text form on Postgres (see [`Db::vector_out`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Vector(pub Vec<f32>);

impl Vector {
    /// SQLite's (and sqlite-vec's) form.
    pub fn to_le_bytes(v: &[f32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    pub fn from_le_bytes(b: &[u8]) -> Option<Vector> {
        let (chunks, rest) = b.as_chunks::<4>();
        rest.is_empty()
            .then(|| Vector(chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()))
    }

    /// pgvector's text form, `[1,2.5,3]`. `Display` of an `f32` round-trips.
    #[cfg(feature = "postgres")]
    fn to_text(v: &[f32]) -> String {
        let items: Vec<String> = v.iter().map(f32::to_string).collect();
        format!("[{}]", items.join(","))
    }

    #[cfg(feature = "postgres")]
    fn from_text(s: &str) -> Option<Vector> {
        let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?;
        if inner.trim().is_empty() {
            return Some(Vector(Vec::new()));
        }
        inner
            .split(',')
            .map(|x| x.trim().parse().ok())
            .collect::<Option<_>>()
            .map(Vector)
    }
}

#[cfg(feature = "sqlite")]
impl sqlx::Type<sqlx::Sqlite> for Vector {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <Vec<u8> as sqlx::Type<sqlx::Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <Vec<u8> as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

#[cfg(feature = "sqlite")]
impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for Vector {
    fn decode(v: sqlx::sqlite::SqliteValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let b = <&[u8] as sqlx::Decode<sqlx::Sqlite>>::decode(v)?;
        Ok(Vector::from_le_bytes(b).ok_or("vector length isn't a multiple of 4")?)
    }
}

#[cfg(feature = "postgres")]
impl sqlx::Type<sqlx::Postgres> for Vector {
    fn type_info() -> sqlx::postgres::PgTypeInfo {
        <String as sqlx::Type<sqlx::Postgres>>::type_info()
    }

    fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
        <String as sqlx::Type<sqlx::Postgres>>::compatible(ty)
    }
}

#[cfg(feature = "postgres")]
impl<'r> sqlx::Decode<'r, sqlx::Postgres> for Vector {
    fn decode(v: sqlx::postgres::PgValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let s = <&str as sqlx::Decode<sqlx::Postgres>>::decode(v)?;
        Ok(Vector::from_text(s).ok_or("not a pgvector vector")?)
    }
}

/// A file whose analysis is missing or stale, with its current stamp.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FileToAnalyze {
    pub remote_path: String,
    pub size: i64,
    pub mtime: i64,
}

/// The result of analysing one file: its vector, or `None` when it couldn't
/// be analysed (not retried until the file changes).
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisWrite {
    pub file: FileToAnalyze,
    pub vector: Option<Vec<f32>>,
}

/// A track near a query vector.
#[derive(Debug, Clone, Copy, PartialEq, sqlx::FromRow)]
pub struct Neighbour {
    pub track_id: i64,
    pub distance: f64,
}

/// A track's distances to two vectors.
#[derive(Debug, Clone, Copy, PartialEq, sqlx::FromRow)]
pub struct PairDistances {
    pub track_id: i64,
    pub to_a: f64,
    pub to_b: f64,
}

#[derive(sqlx::FromRow)]
struct VectorRow {
    vector: Vector,
}

#[derive(sqlx::FromRow)]
struct DimsRow {
    dims: Option<i64>,
}

/// A stored result, for export.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct StoredAnalysis {
    pub remote_path: String,
    pub size: i64,
    pub vector: Option<Vector>,
}

/// A tag-pass stamp: the file a result would be stored for.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct FileStampRow {
    pub library_id: i64,
    pub remote_path: String,
    pub size: i64,
    pub mtime: i64,
}

/// Stored results of one analyzer (as the admin API's status reports them).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct AnalyzerCounts {
    #[serde(rename = "id")]
    pub analyzer: String,
    pub analysed: i64,
    pub failed: i64,
}

/// The file of the row aliased `table` belongs to a live track.
fn live_file(table: &str) -> Expr {
    Expr::cust(format!(
        "EXISTS (SELECT 1 FROM tracks t WHERE t.library_id = {table}.library_id \
         AND t.remote_path = {table}.remote_path AND t.deleted_at IS NULL)"
    ))
}

impl Db {
    /// Files of live tracks in `library` without a current `analyzer` result,
    /// by path. Stamps come from the tag cache, so only files the tag pass has
    /// seen are listed.
    pub async fn files_to_analyze(
        &self,
        library: i64,
        analyzer: &str,
        limit: u64,
    ) -> Result<Vec<FileToAnalyze>> {
        self.fetch_all(
            &Query::select()
                .columns([("f", "remote_path"), ("f", "size"), ("f", "mtime")])
                .from_as("file_tags", "f")
                .and_where(Expr::col(("f", "library_id")).eq(library))
                .and_where(live_file("f"))
                .and_where(
                    Expr::exists(
                        Query::select()
                            .expr(Expr::val(1))
                            .from_as("file_analysis", "a")
                            .and_where(Expr::col(("a", "library_id")).equals(("f", "library_id")))
                            .and_where(Expr::col(("a", "remote_path")).equals(("f", "remote_path")))
                            .and_where(Expr::col(("a", "analyzer")).eq(analyzer))
                            .and_where(Expr::col(("a", "size")).equals(("f", "size")))
                            .and_where(Expr::col(("a", "mtime")).equals(("f", "mtime")))
                            .to_owned(),
                    )
                    .not(),
                )
                .order_by(("f", "remote_path"), Order::Asc)
                .limit(limit)
                .to_owned(),
        )
        .await
    }

    /// Store results of `analyzer`, registering it on its first. Every vector
    /// of an analyzer has the same length, and its elements are finite.
    pub async fn write_analysis(
        &self,
        library: i64,
        analyzer: &str,
        writes: &[AnalysisWrite],
    ) -> Result<()> {
        if writes.is_empty() {
            return Ok(());
        }
        let mut dims = None;
        for w in writes {
            let Some(v) = &w.vector else { continue };
            if v.is_empty() || v.iter().any(|x| !x.is_finite()) {
                return Err(StoreError::Invalid(format!(
                    "{}: an empty vector, or one that isn't finite",
                    w.file.remote_path
                )));
            }
            if *dims.get_or_insert(v.len()) != v.len() {
                return Err(StoreError::Invalid(format!(
                    "{}: vector lengths differ",
                    w.file.remote_path
                )));
            }
        }
        let now = now_ms();
        let mut tx = self.begin().await?;
        tx.execute(
            &Query::insert()
                .into_table("analyzers")
                .columns(["id", "dims", "created_at"])
                .values_panic([analyzer.into(), dims.map(|d| d as i64).into(), now.into()])
                .on_conflict(OnConflict::column("id").do_nothing().to_owned())
                .to_owned(),
        )
        .await?;
        if let Some(d) = dims {
            tx.execute(
                &Query::update()
                    .table("analyzers")
                    .value("dims", d as i64)
                    .and_where(Expr::col("id").eq(analyzer))
                    .and_where(Expr::col("dims").is_null())
                    .to_owned(),
            )
            .await?;
            let row: DimsRow = tx
                .fetch_one(
                    &Query::select()
                        .column("dims")
                        .from("analyzers")
                        .and_where(Expr::col("id").eq(analyzer))
                        .to_owned(),
                )
                .await?;
            if row.dims != Some(d as i64) {
                return Err(StoreError::Invalid(format!(
                    "{analyzer} vectors have {} dimensions, not {d}",
                    row.dims.unwrap_or_default()
                )));
            }
        }
        let mut q = Query::insert()
            .into_table("file_analysis")
            .columns([
                "library_id",
                "remote_path",
                "analyzer",
                "size",
                "mtime",
                "vector",
                "analyzed_at",
            ])
            .on_conflict(
                OnConflict::columns(["library_id", "remote_path", "analyzer"])
                    .update_columns(["size", "mtime", "vector", "analyzed_at"])
                    .to_owned(),
            )
            .to_owned();
        for w in writes {
            q.values_panic([
                library.into(),
                w.file.remote_path.as_str().into(),
                analyzer.into(),
                w.file.size.into(),
                w.file.mtime.into(),
                w.vector
                    .as_deref()
                    .map_or_else(|| Expr::cust("NULL"), |v| self.vector_in(v)),
                now.into(),
            ]);
        }
        tx.execute(&q).await?;
        tx.commit().await
    }

    /// A vector bound as the column's type.
    fn vector_in(&self, v: &[f32]) -> Expr {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { .. } => Expr::val(Vector::to_le_bytes(v)),
            #[cfg(feature = "postgres")]
            Db::Postgres(_) => Expr::val(Vector::to_text(v)).cast_as("vector"),
        }
    }

    /// A vector column as [`Vector`] decodes it.
    fn vector_out(&self, col: (&'static str, &'static str)) -> Expr {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { .. } => Expr::col(col),
            #[cfg(feature = "postgres")]
            Db::Postgres(_) => Expr::col(col).cast_as("text"),
        }
    }

    /// The distance from a vector column to `q`: sqlite-vec's functions, or
    /// pgvector's operators.
    fn distance(&self, metric: Metric, col: (&'static str, &'static str), q: &[f32]) -> Expr {
        match self {
            #[cfg(feature = "sqlite")]
            Db::Sqlite { .. } => {
                let f = match metric {
                    Metric::Cosine => "vec_distance_cosine",
                    Metric::Euclidean => "vec_distance_l2",
                };
                sea_query::Func::cust(f)
                    .arg(Expr::col(col))
                    .arg(self.vector_in(q))
                    .into()
            }
            #[cfg(feature = "postgres")]
            Db::Postgres(_) => {
                let op = match metric {
                    Metric::Cosine => "<=>",
                    Metric::Euclidean => "<->",
                };
                Expr::col(col).binary(sea_query::BinOper::Custom(op), self.vector_in(q))
            }
        }
    }

    /// `file_analysis` rows (`a`) with an `analyzer` vector, joined to their
    /// live tracks (`t`).
    fn live_vectors(analyzer: &str) -> sea_query::SelectStatement {
        Query::select()
            .from_as("file_analysis", "a")
            .join_as(
                JoinType::InnerJoin,
                "tracks",
                "t",
                Expr::col(("t", "library_id"))
                    .equals(("a", "library_id"))
                    .and(Expr::col(("t", "remote_path")).equals(("a", "remote_path"))),
            )
            .and_where(Expr::col(("a", "analyzer")).eq(analyzer))
            .and_where(Expr::col(("a", "vector")).is_not_null())
            .and_where(Expr::col(("t", "deleted_at")).is_null())
            .to_owned()
    }

    /// Whether any live track has an `analyzer` vector.
    pub async fn has_vectors(&self, analyzer: &str) -> Result<bool> {
        let row: Option<IdRow> = self
            .fetch_optional(
                &Self::live_vectors(analyzer)
                    .expr_as(Expr::col(("t", "id")), "id")
                    .limit(1)
                    .to_owned(),
            )
            .await?;
        Ok(row.is_some())
    }

    /// A live track's `analyzer` vector, if it has one.
    pub async fn track_vector(&self, analyzer: &str, track: i64) -> Result<Option<Vec<f32>>> {
        let row: Option<VectorRow> = self
            .fetch_optional(
                &Self::live_vectors(analyzer)
                    .expr_as(self.vector_out(("a", "vector")), "vector")
                    .and_where(Expr::col(("t", "id")).eq(track))
                    .limit(1)
                    .to_owned(),
            )
            .await?;
        Ok(row.map(|r| r.vector.0))
    }

    /// The live tracks with an `analyzer` vector nearest `q`, closest first
    /// (ties by id), at most `limit`, leaving out `exclude`. An exact scan.
    pub async fn nearest_tracks(
        &self,
        analyzer: &str,
        metric: Metric,
        q: &[f32],
        limit: u64,
        exclude: &[i64],
    ) -> Result<Vec<Neighbour>> {
        let mut query = Self::live_vectors(analyzer)
            .expr_as(Expr::col(("t", "id")), "track_id")
            .expr_as(self.distance(metric, ("a", "vector"), q), "distance")
            .order_by("distance", Order::Asc)
            .order_by(("t", "id"), Order::Asc)
            .limit(limit)
            .to_owned();
        if !exclude.is_empty() {
            query.and_where(Expr::col(("t", "id")).is_not_in(exclude.iter().copied()));
        }
        self.fetch_all(&query).await
    }

    /// Every live track with an `analyzer` vector, but `exclude`, with its
    /// distances to `a` and `b`: one scan from which a path between them is
    /// worked out (see `rsub_analysis::SonicSearch::path`).
    pub async fn distances_to_pair(
        &self,
        analyzer: &str,
        metric: Metric,
        a: &[f32],
        b: &[f32],
        exclude: &[i64],
    ) -> Result<Vec<PairDistances>> {
        let mut query = Self::live_vectors(analyzer)
            .expr_as(Expr::col(("t", "id")), "track_id")
            .expr_as(self.distance(metric, ("a", "vector"), a), "to_a")
            .expr_as(self.distance(metric, ("a", "vector"), b), "to_b")
            .to_owned();
        if !exclude.is_empty() {
            query.and_where(Expr::col(("t", "id")).is_not_in(exclude.iter().copied()));
        }
        self.fetch_all(&query).await
    }

    /// The tag-pass stamps of these files, in any library.
    pub async fn file_stamps(&self, paths: &[String]) -> Result<Vec<FileStampRow>> {
        let mut out = Vec::new();
        for chunk in paths.chunks(CHUNK) {
            out.extend(
                self.fetch_all::<FileStampRow>(
                    &Query::select()
                        .columns(["library_id", "remote_path", "size", "mtime"])
                        .from("file_tags")
                        .and_where(Expr::col("remote_path").is_in(chunk.iter().map(String::as_str)))
                        .to_owned(),
                )
                .await?,
            );
        }
        Ok(out)
    }

    /// Every stored `analyzer` result for files of live tracks, by path (what
    /// [`Db::analyzer_counts`] counts).
    pub async fn stored_analysis(&self, analyzer: &str) -> Result<Vec<StoredAnalysis>> {
        self.fetch_all(
            &Query::select()
                .columns([("a", "remote_path"), ("a", "size")])
                .expr_as(self.vector_out(("a", "vector")), "vector")
                .from_as("file_analysis", "a")
                .and_where(Expr::col(("a", "analyzer")).eq(analyzer))
                .and_where(live_file("a"))
                .order_by(("a", "remote_path"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Stored results by analyzer, for files of live tracks.
    pub async fn analyzer_counts(&self) -> Result<Vec<AnalyzerCounts>> {
        self.fetch_all(
            &Query::select()
                .column(("a", "analyzer"))
                .expr_as(
                    Expr::cust("COALESCE(SUM(CASE WHEN a.vector IS NULL THEN 0 ELSE 1 END), 0)"),
                    "analysed",
                )
                .expr_as(
                    Expr::cust("COALESCE(SUM(CASE WHEN a.vector IS NULL THEN 1 ELSE 0 END), 0)"),
                    "failed",
                )
                .from_as("file_analysis", "a")
                .and_where(live_file("a"))
                .group_by_col(("a", "analyzer"))
                .order_by(("a", "analyzer"), Order::Asc)
                .to_owned(),
        )
        .await
    }

    /// Drop results for files the tag cache no longer has.
    pub async fn prune_analysis(&self, library: i64) -> Result<u64> {
        self.execute(
            &Query::delete()
                .from_table("file_analysis")
                .and_where(Expr::col("library_id").eq(library))
                .and_where(Expr::cust(
                    "NOT EXISTS (SELECT 1 FROM file_tags f WHERE f.library_id = file_analysis.library_id \
                     AND f.remote_path = file_analysis.remote_path)",
                ))
                .to_owned(),
        )
        .await
    }
}
