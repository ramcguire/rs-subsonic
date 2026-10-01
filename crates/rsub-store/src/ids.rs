//! Public ids to internal row ids, at the API edge.

use std::collections::HashMap;

use rsub_core::identity::alias_key;
use rsub_core::{Kind, PublicId};
use sea_query::{Expr, ExprTrait, Query};

use crate::identity::kind_str;
use crate::{CHUNK, Db, Result, catalog_table};

/// Aliases followed from one id before giving up (a long chain, or a cycle).
const MAX_HOPS: usize = 8;

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    public_id: String,
    deleted_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct AliasRow {
    public_id: String,
}

impl Db {
    /// The internal id behind a public id. The id of a gone item leads to the
    /// live item it continued as, through the ledger's aliases; failing that
    /// to its soft-deleted row.
    pub async fn internal_id(&self, id: PublicId) -> Result<Option<i64>> {
        Ok(self.internal_ids(&[id]).await?[0])
    }

    /// [`Db::internal_id`] for many ids, in order.
    pub async fn internal_ids(&self, ids: &[PublicId]) -> Result<Vec<Option<i64>>> {
        let mut by_kind: HashMap<Kind, Vec<String>> = HashMap::new();
        for id in ids {
            by_kind.entry(id.kind()).or_default().push(id.to_string());
        }
        let mut found: HashMap<String, i64> = HashMap::new();
        for (kind, wanted) in by_kind {
            // Playlist ids are minted from the backend playlist.
            let Some(table) = catalog_table(kind) else {
                continue;
            };
            let mut rows: HashMap<String, Row> = HashMap::new();
            for chunk in wanted.chunks(CHUNK) {
                rows.extend(
                    self.rows(table, chunk.iter().map(String::as_str))
                        .await?
                        .into_iter()
                        .map(|r| (r.public_id.clone(), r)),
                );
            }
            for pid in &wanted {
                let row = rows.get(pid);
                let id = match row {
                    Some(r) if r.deleted_at.is_none() => Some(r.id),
                    // Rare: only ids of gone items look for an alias.
                    _ => self
                        .follow_aliases(kind, table, pid)
                        .await?
                        .or(row.map(|r| r.id)),
                };
                if let Some(id) = id {
                    found.insert(pid.clone(), id);
                }
            }
        }
        Ok(ids
            .iter()
            .map(|id| found.get(&id.to_string()).copied())
            .collect())
    }

    async fn rows<'a>(
        &self,
        table: &'static str,
        public_ids: impl IntoIterator<Item = &'a str>,
    ) -> Result<Vec<Row>> {
        self.fetch_all(
            &Query::select()
                .columns(["id", "public_id", "deleted_at"])
                .from(table)
                .and_where(Expr::col("public_id").is_in(public_ids))
                .to_owned(),
        )
        .await
    }

    /// The live row a gone item's id leads to through its aliases, if any.
    async fn follow_aliases(
        &self,
        kind: Kind,
        table: &'static str,
        public_id: &str,
    ) -> Result<Option<i64>> {
        let mut current = public_id.to_owned();
        for _ in 0..MAX_HOPS {
            let Some(next) = self
                .fetch_optional::<AliasRow>(
                    &Query::select()
                        .column("public_id")
                        .from("identity_keys")
                        .and_where(Expr::col("kind").eq(kind_str(kind)))
                        .and_where(Expr::col("key").eq(alias_key(&current)))
                        .to_owned(),
                )
                .await?
            else {
                return Ok(None);
            };
            if next.public_id == public_id {
                return Ok(None);
            }
            match self.rows(table, [next.public_id.as_str()]).await?.pop() {
                Some(r) if r.deleted_at.is_none() => return Ok(Some(r.id)),
                _ => current = next.public_id,
            }
        }
        Ok(None)
    }
}
