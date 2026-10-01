//! Sonic-similarity queries over one analyzer's stored vectors. The database
//! does the vector work (`rsub_store::Db::nearest_tracks`); a path is worked
//! out here from one scan of distances to its two ends.

use std::collections::HashSet;

use rsub_store::{Db, Metric, PairDistances, StoreError};

/// A track and its similarity to the query, 1 for the same sound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Match {
    pub track_id: i64,
    pub similarity: f32,
}

#[derive(Clone)]
pub struct SonicSearch {
    db: Db,
    analyzer: String,
    metric: Metric,
}

impl SonicSearch {
    pub fn new(db: Db, analyzer: impl Into<String>, metric: Metric) -> Self {
        SonicSearch {
            db,
            analyzer: analyzer.into(),
            metric,
        }
    }

    pub fn analyzer(&self) -> &str {
        &self.analyzer
    }

    /// Whether any live track has a vector.
    pub async fn is_ready(&self) -> Result<bool, StoreError> {
        self.db.has_vectors(&self.analyzer).await
    }

    /// Whether `track` has a vector.
    pub async fn contains(&self, track: i64) -> Result<bool, StoreError> {
        Ok(self.db.track_vector(&self.analyzer, track).await?.is_some())
    }

    /// The tracks nearest `track`, closest first, at most `limit`, never
    /// `track` itself or `exclude`. `None` when `track` has no vector.
    pub async fn similar(
        &self,
        track: i64,
        limit: usize,
        exclude: &[i64],
    ) -> Result<Option<Vec<Match>>, StoreError> {
        let Some(q) = self.db.track_vector(&self.analyzer, track).await? else {
            return Ok(None);
        };
        let exclude: Vec<i64> = exclude.iter().copied().chain([track]).collect();
        let near = self
            .db
            .nearest_tracks(&self.analyzer, self.metric, &q, limit as u64, &exclude)
            .await?;
        Ok(Some(
            near.into_iter()
                .map(|n| Match {
                    track_id: n.track_id,
                    similarity: self.metric.similarity(n.distance as f32),
                })
                .collect(),
        ))
    }

    /// `count` tracks (two at least) leading from `start` to `end`: `start`
    /// first and `end` last, and between them, for evenly spaced points on the
    /// line from one to the other, the nearest track not already on the path
    /// or in `exclude`. Similarities are to `start`. `None` when either has no
    /// vector.
    ///
    /// A track's distance to a point on the line follows from its distances to
    /// the two ends, so one scan serves every point.
    pub async fn path(
        &self,
        start: i64,
        end: i64,
        count: usize,
        exclude: &[i64],
    ) -> Result<Option<Vec<Match>>, StoreError> {
        let db = &self.db;
        let (Some(a), Some(b)) = (
            db.track_vector(&self.analyzer, start).await?,
            db.track_vector(&self.analyzer, end).await?,
        ) else {
            return Ok(None);
        };
        let exclude: Vec<i64> = exclude.iter().copied().chain([start, end]).collect();
        let rows = db
            .distances_to_pair(&self.analyzer, self.metric, &a, &b, &exclude)
            .await?;
        let ab = f64::from(self.metric.distance(&a, &b));
        let mut used: HashSet<i64> = HashSet::new();
        let mut out = vec![Match {
            track_id: start,
            similarity: 1.0,
        }];
        let count = count.max(2);
        for k in 1..count - 1 {
            let t = k as f64 / (count - 1) as f64;
            let Some(r) = rows
                .iter()
                .filter(|r| !used.contains(&r.track_id))
                .map(|r| (to_point(self.metric, r, t, ab), r))
                .min_by(|x, y| x.0.total_cmp(&y.0).then(x.1.track_id.cmp(&y.1.track_id)))
                .map(|(_, r)| r)
            else {
                break;
            };
            used.insert(r.track_id);
            out.push(Match {
                track_id: r.track_id,
                similarity: self.metric.similarity(r.to_a as f32),
            });
        }
        out.push(Match {
            track_id: end,
            similarity: self.metric.similarity(ab as f32),
        });
        Ok(Some(out))
    }
}

/// A track's distance to the point `t` of the way from `a` to `b`, from its
/// distances to both and theirs to each other (`ab`). For cosine the line
/// runs between the unit vectors of the ends.
fn to_point(metric: Metric, r: &PairDistances, t: f64, ab: f64) -> f64 {
    let s = 1.0 - t;
    match metric {
        // |x - (sA + tB)|² = s|x - A|² + t|x - B|² - st|A - B|²
        Metric::Euclidean => (s * r.to_a.powi(2) + t * r.to_b.powi(2) - s * t * ab.powi(2))
            .max(0.0)
            .sqrt(),
        // cos(x, sA + tB) = (s cos(x, A) + t cos(x, B)) / |sA + tB|
        Metric::Cosine => {
            let norm = (s * s + t * t + 2.0 * s * t * (1.0 - ab)).sqrt();
            if norm == 0.0 {
                return 1.0;
            }
            1.0 - (s * (1.0 - r.to_a) + t * (1.0 - r.to_b)) / norm
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shortcut agrees with measuring to the point itself.
    #[test]
    fn distance_to_a_point_from_the_ends() {
        let (a, b, x) = ([0.6f32, 0.8, 0.0], [0.0f32, 0.6, 0.8], [0.3f32, 0.1, 0.5]);
        for metric in [Metric::Euclidean, Metric::Cosine] {
            let r = PairDistances {
                track_id: 1,
                to_a: metric.distance(&x, &a).into(),
                to_b: metric.distance(&x, &b).into(),
            };
            let ab = metric.distance(&a, &b).into();
            for t in [0.0f32, 0.25, 0.5, 0.9] {
                let p: Vec<f32> = a.iter().zip(&b).map(|(u, v)| u + (v - u) * t).collect();
                let direct = f64::from(metric.distance(&x, &p));
                let shortcut = to_point(metric, &r, f64::from(t), ab);
                assert!((direct - shortcut).abs() < 1e-5, "{metric:?} {t}");
            }
        }
    }
}
