//! `bench`'s in-memory vector index: the files it analysed, searched by brute
//! force. The server searches in its database instead.

use std::collections::HashMap;

use rsub_analysis::{Match, Metric};

pub struct SonicIndex {
    metric: Metric,
    dim: usize,
    ids: Vec<i64>,
    pos: HashMap<i64, usize>,
    /// `ids.len() × dim` floats.
    data: Vec<f32>,
}

impl SonicIndex {
    /// Vectors whose length differs from the first's are skipped.
    pub fn new(metric: Metric, rows: impl IntoIterator<Item = (i64, Vec<f32>)>) -> Self {
        let mut index = SonicIndex {
            metric,
            dim: 0,
            ids: Vec::new(),
            pos: HashMap::new(),
            data: Vec::new(),
        };
        for (id, v) in rows {
            if index.dim == 0 {
                index.dim = v.len();
            }
            if v.len() != index.dim || v.is_empty() || index.pos.contains_key(&id) {
                continue;
            }
            index.pos.insert(id, index.ids.len());
            index.ids.push(id);
            index.data.extend(v);
        }
        index
    }

    fn row(&self, i: usize) -> &[f32] {
        &self.data[i * self.dim..(i + 1) * self.dim]
    }

    /// The kept tracks nearest `track`, closest first, never `track` itself.
    /// `None` when `track` has no vector.
    pub fn similar(
        &self,
        track: i64,
        limit: usize,
        keep: impl Fn(i64) -> bool,
    ) -> Option<Vec<Match>> {
        let &i = self.pos.get(&track)?;
        let q = self.row(i);
        let mut d: Vec<(usize, f32)> = (0..self.ids.len())
            .filter(|&j| j != i && keep(self.ids[j]))
            .map(|j| (j, self.metric.distance(q, self.row(j))))
            .collect();
        if d.len() > limit {
            d.select_nth_unstable_by(limit, |a, b| a.1.total_cmp(&b.1));
            d.truncate(limit);
        }
        d.sort_by(|a, b| a.1.total_cmp(&b.1).then(self.ids[a.0].cmp(&self.ids[b.0])));
        Some(
            d.into_iter()
                .map(|(j, dist)| Match {
                    track_id: self.ids[j],
                    similarity: self.metric.similarity(dist),
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_first() {
        // Tracks 1..=5 on a line, 10 next to 1; 7's vector is too short.
        let rows = [(1, 0.0), (2, 1.0), (3, 2.0), (4, 3.0), (5, 4.0), (10, 0.1)]
            .map(|(id, x)| (id, vec![x, 0.0]));
        let idx = SonicIndex::new(Metric::Euclidean, rows.into_iter().chain([(7, vec![1.0])]));
        let m = idx.similar(1, 3, |_| true).unwrap();
        let ids: Vec<i64> = m.iter().map(|m| m.track_id).collect();
        assert_eq!(ids, [10, 2, 3]);
        assert!(m[0].similarity > m[1].similarity);
        let m = idx.similar(1, 2, |id| id != 10).unwrap();
        assert_eq!(m[0].track_id, 2);
        assert!(idx.similar(99, 3, |_| true).is_none());
        assert!(idx.similar(7, 3, |_| true).is_none());
    }
}
