//! Related artists from the catalog alone, for artists whose backend knows few
//! similar artists in the library: collaborators (shared track credits) and
//! artists with the same styles.

use std::collections::{HashMap, HashSet};

use rsub_core::text::normalize;
use sea_query::{Expr, ExprTrait, JoinType, Order, Query};

use crate::{ArtistRow, Db, Result};

/// Least style information two artists must share, in nats: at least one style
/// held by a quarter of the library's styled artists or fewer, or several
/// broader ones. Sharing only "Electronic" doesn't make two artists alike.
const MIN_STYLE_INFO: f64 = 1.386; // ln 4

/// An artist related to the seeds, with a score that orders one kind of
/// relation (not comparable across kinds).
#[derive(Debug, Clone, PartialEq)]
pub struct RelatedArtist {
    pub artist: ArtistRow,
    pub score: f64,
}

impl Db {
    /// Artists credited on the seeds' tracks (features, co-producers, the
    /// album artist of a guest spot), most shared tracks first. Artists whose
    /// only tracks are the shared ones are left out: they'd add no songs.
    /// Artists named like a seed count as the seed.
    pub async fn collaborators(
        &self,
        seeds: &[ArtistRow],
        library: i64,
        limit: usize,
    ) -> Result<Vec<RelatedArtist>> {
        #[derive(sqlx::FromRow)]
        struct Shared {
            artist_id: i64,
            shared: i64,
            own: i64,
        }
        if seeds.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let names: Vec<String> = seeds.iter().map(|a| normalize(&a.name)).collect();
        let seed_ids = Query::select()
            .column("id")
            .from("artists")
            .and_where(Expr::col("library_id").eq(library))
            .and_where(Expr::col("search_norm").is_in(names.iter().map(String::as_str)))
            .to_owned();
        let candidates = Query::select()
            .column("id")
            .from("artists")
            .and_where(Expr::col("library_id").eq(library))
            .and_where(Expr::col("deleted_at").is_null())
            .and_where(Expr::col("search_norm").is_not_in(names.iter().map(String::as_str)))
            .to_owned();
        let rows: Vec<Shared> = self
            .fetch_all(
                &Query::select()
                    .expr_as(Expr::col(("c", "artist_id")), "artist_id")
                    .expr_as(Expr::cust("COUNT(DISTINCT c.track_id)"), "shared")
                    .expr_as(
                        Expr::cust(
                            "(SELECT COUNT(*) FROM track_credits o \
                             WHERE o.artist_id = c.artist_id AND o.role = 'artist')",
                        ),
                        "own",
                    )
                    .from_as("track_credits", "c")
                    .join_as(
                        JoinType::InnerJoin,
                        "tracks",
                        "t",
                        Expr::col(("t", "id")).equals(("c", "track_id")),
                    )
                    .and_where(Expr::col(("t", "deleted_at")).is_null())
                    .and_where(Expr::col(("c", "role")).eq("artist"))
                    .and_where(
                        Expr::col(("c", "track_id")).in_subquery(
                            Query::select()
                                .column("track_id")
                                .from("track_credits")
                                .and_where(Expr::col("artist_id").in_subquery(seed_ids))
                                .to_owned(),
                        ),
                    )
                    .and_where(Expr::col(("c", "artist_id")).in_subquery(candidates))
                    .group_by_col(("c", "artist_id"))
                    .to_owned(),
            )
            .await?;
        let mut rows: Vec<Shared> = rows.into_iter().filter(|r| r.own > r.shared).collect();
        rows.sort_by(|a, b| {
            b.shared
                .cmp(&a.shared)
                .then(b.own.cmp(&a.own))
                .then(a.artist_id.cmp(&b.artist_id))
        });
        rows.truncate(limit);
        let ids: Vec<i64> = rows.iter().map(|r| r.artist_id).collect();
        let artists: HashMap<i64, ArtistRow> = self
            .artists_by_ids(&ids)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                Some(RelatedArtist {
                    artist: artists.get(&r.artist_id)?.clone(),
                    score: r.shared as f64,
                })
            })
            .collect())
    }

    /// Artists whose backend styles overlap the seeds', by cosine similarity
    /// of the style sets weighted by rarity (a style few artists have says
    /// more). Pairs sharing too little information are left out
    /// ([`MIN_STYLE_INFO`]).
    pub async fn style_neighbours(
        &self,
        seeds: &[ArtistRow],
        library: i64,
        limit: usize,
    ) -> Result<Vec<RelatedArtist>> {
        #[derive(sqlx::FromRow)]
        struct Styled {
            artist_id: i64,
            name: String,
        }
        if seeds.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let rows: Vec<Styled> = self
            .fetch_all(
                &Query::select()
                    .column(("l", "artist_id"))
                    .column(("g", "name"))
                    .from_as("artist_tags", "l")
                    .join_as(
                        JoinType::InnerJoin,
                        "tags",
                        "g",
                        Expr::col(("g", "id")).equals(("l", "tag_id")),
                    )
                    .join_as(
                        JoinType::InnerJoin,
                        "artists",
                        "a",
                        Expr::col(("a", "id")).equals(("l", "artist_id")),
                    )
                    .and_where(Expr::col(("g", "kind")).eq("style"))
                    .and_where(Expr::col(("a", "library_id")).eq(library))
                    .and_where(Expr::col(("a", "deleted_at")).is_null())
                    .order_by(("l", "artist_id"), Order::Asc)
                    .to_owned(),
            )
            .await?;
        let mut styles: HashMap<i64, HashSet<String>> = HashMap::new();
        for r in rows {
            styles.entry(r.artist_id).or_default().insert(r.name);
        }
        let n = styles.len() as f64;
        let mut df: HashMap<&str, f64> = HashMap::new();
        for s in styles.values().flatten() {
            *df.entry(s.as_str()).or_default() += 1.0;
        }
        let idf = |s: &str| (n / df.get(s).copied().unwrap_or(n)).ln();
        let norm = |set: &HashSet<String>| set.iter().map(|s| idf(s).powi(2)).sum::<f64>().sqrt();

        let seed_names: HashSet<String> = seeds.iter().map(|a| normalize(&a.name)).collect();
        let seed_ids: HashSet<i64> = seeds.iter().map(|a| a.id).collect();
        let seed: HashSet<String> = seeds
            .iter()
            .filter_map(|a| styles.get(&a.id))
            .flatten()
            .cloned()
            .collect();
        let seed_norm = norm(&seed);
        if seed_norm == 0.0 {
            return Ok(Vec::new());
        }
        let mut scored: Vec<(i64, f64)> = styles
            .iter()
            .filter(|(id, _)| !seed_ids.contains(id))
            .filter_map(|(id, set)| {
                let shared: Vec<f64> = set.intersection(&seed).map(|s| idf(s)).collect();
                if shared.iter().sum::<f64>() < MIN_STYLE_INFO {
                    return None;
                }
                let dot: f64 = shared.iter().map(|w| w * w).sum();
                Some((*id, dot / (seed_norm * norm(set))))
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        // Room for artists dropped below as a seed's namesake.
        scored.truncate(limit + seeds.len());
        let ids: Vec<i64> = scored.iter().map(|(id, _)| *id).collect();
        let mut artists: HashMap<i64, ArtistRow> = self
            .artists_by_ids(&ids)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
        Ok(scored
            .into_iter()
            .filter_map(|(id, score)| {
                let artist = artists.remove(&id)?;
                (!seed_names.contains(&normalize(&artist.name)))
                    .then_some(RelatedArtist { artist, score })
            })
            .take(limit)
            .collect())
    }
}
