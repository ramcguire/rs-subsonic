//! The identity ledger: which public id each identity key resolves to, and
//! resolution of incoming catalog items to public ids through it.

use std::collections::{HashMap, HashSet};

use rsub_core::identity::{alias_key, duplicate_key, is_mintable, rating_key};
use rsub_core::{Kind, PublicId, now_ms};
use sea_query::{Cond, Expr, ExprTrait, OnConflict, Order, Query};
use serde::{Deserialize, Serialize};

use crate::catalog::SyncCtx;
use crate::tags::{FileKey, any_listed, listed};
use crate::{CHUNK, Db, Result, StoreError, Tx, catalog_table};

/// `rk:` keys unseen for this long are pruned: ratingKeys are never minted
/// from, so a stale one can only mislead.
const RATING_KEY_TTL_MS: i64 = 30 * 24 * 3600 * 1000;

/// Duplicate suffixes (`#2` …) recognised when an item takes back its
/// canonical id; far more copies of one item than any library has.
const MAX_DUPLICATE: u32 = 64;

pub(crate) fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::Artist => "artist",
        Kind::Album => "album",
        Kind::Track => "track",
        _ => unreachable!("only catalog items have identity keys"),
    }
}

fn parse_kind(s: &str) -> Option<Kind> {
    Some(match s {
        "artist" => Kind::Artist,
        "album" => Kind::Album,
        "track" => Kind::Track,
        _ => return None,
    })
}

fn file_key(kind: Kind) -> FileKey {
    match kind {
        Kind::Artist => FileKey::Artist,
        Kind::Album => FileKey::Album,
        _ => FileKey::Track,
    }
}

/// An incoming item to resolve.
pub(crate) struct Claimant {
    /// The backend key; `None` for virtual artists.
    pub remote_key: Option<String>,
    /// Identity keys, strongest first, including the `rk:` key if any.
    pub keys: Vec<String>,
    /// Breaks ties between items with the same strongest key, so rebuilds
    /// settle them the same way: the remote path for tracks, else the name.
    pub tiebreak: String,
}

impl Claimant {
    fn rating_key(&self) -> Option<&str> {
        self.keys
            .iter()
            .map(String::as_str)
            .find(|k| !is_mintable(k))
    }

    fn strongest(&self) -> &str {
        self.keys
            .iter()
            .map(String::as_str)
            .find(|k| is_mintable(k))
            .unwrap_or("")
    }
}

/// The catalog row that holds a public id.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct Holder {
    pub id: i64,
    pub public_id: String,
    pub library_id: i64,
    pub remote_key: Option<String>,
    pub remote_updated_at: i64,
    pub generation: i64,
    pub deleted_at: Option<i64>,
}

#[derive(sqlx::FromRow)]
struct LedgerRow {
    key: String,
    public_id: String,
}

/// Resolves one batch of items of one kind within a library pass.
pub(crate) struct Resolver<'c> {
    cx: SyncCtx<'c>,
    kind: Kind,
    /// Ledger entries for the batch's keys, kept current as keys are written.
    ledger: HashMap<String, String>,
    /// Rows by public id, loaded on demand (`None`: no row holds the id).
    holders: HashMap<String, Option<Holder>>,
    /// Ids chosen in this batch.
    chosen: HashSet<String>,
    /// Backend keys the tag pass listed, by key, loaded on demand.
    listed: HashMap<String, bool>,
    /// Keys already pointing at their item's id: only `last_seen_at` changes.
    seen: Vec<String>,
    /// The keys each id chosen in this batch was recorded with.
    carried: HashMap<String, HashSet<String>>,
    /// Keys `(key, from)` that couldn't move off an id another item holds,
    /// by the id they would move to. Settled in [`Resolver::finish`].
    deferred: Vec<(String, String, String)>,
    /// Whether the tag pass listed the library, loaded on demand.
    has_listing: Option<bool>,
    now: i64,
}

impl<'c> Resolver<'c> {
    /// Load the ledger entries and rows reachable from `claimants`' keys.
    pub async fn load(
        tx: &mut Tx,
        cx: SyncCtx<'c>,
        kind: Kind,
        claimants: &[Claimant],
    ) -> Result<Resolver<'c>> {
        let mut r = Resolver {
            cx,
            kind,
            ledger: HashMap::new(),
            holders: HashMap::new(),
            chosen: HashSet::new(),
            listed: HashMap::new(),
            seen: Vec::new(),
            carried: HashMap::new(),
            deferred: Vec::new(),
            has_listing: None,
            now: now_ms(),
        };
        let keys: Vec<&str> = claimants
            .iter()
            .flat_map(|c| c.keys.iter().map(String::as_str))
            .collect();
        for chunk in keys.chunks(CHUNK) {
            let rows: Vec<LedgerRow> = tx
                .fetch_all(
                    &Query::select()
                        .columns(["key", "public_id"])
                        .from("identity_keys")
                        .and_where(Expr::col("kind").eq(kind_str(kind)))
                        .and_where(Expr::col("key").is_in(chunk.iter().copied()))
                        .to_owned(),
                )
                .await?;
            r.ledger
                .extend(rows.into_iter().map(|l| (l.key, l.public_id)));
        }
        let ids: Vec<String> = r
            .ledger
            .values()
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        r.load_holders(tx, &ids).await?;
        Ok(r)
    }

    async fn load_holders(&mut self, tx: &mut Tx, ids: &[String]) -> Result<()> {
        let wanted: Vec<&str> = ids
            .iter()
            .filter(|p| !self.holders.contains_key(*p))
            .map(String::as_str)
            .collect();
        for chunk in wanted.chunks(CHUNK) {
            let rows: Vec<Holder> = tx
                .fetch_all(
                    &Query::select()
                        .columns([
                            "id",
                            "public_id",
                            "library_id",
                            "remote_key",
                            "remote_updated_at",
                            "generation",
                            "deleted_at",
                        ])
                        .from(
                            catalog_table(self.kind)
                                .expect("only catalog items have identity keys"),
                        )
                        .and_where(Expr::col("public_id").is_in(chunk.iter().copied()))
                        .to_owned(),
                )
                .await?;
            for p in chunk {
                self.holders.insert((*p).to_owned(), None);
            }
            for h in rows {
                self.holders.insert(h.public_id.clone(), Some(h));
            }
        }
        Ok(())
    }

    /// Note a row the caller inserted or revived for an id, so later
    /// claimants in the batch that reach the id find it.
    pub fn hold(&mut self, h: Holder) {
        self.holders.insert(h.public_id.clone(), Some(h));
    }

    async fn holder(&mut self, tx: &mut Tx, public_id: &str) -> Result<Option<Holder>> {
        if !self.holders.contains_key(public_id) {
            self.load_holders(tx, &[public_id.to_owned()]).await?;
        }
        Ok(self.holders[public_id].clone())
    }

    /// The order to resolve `claimants` in: items recognised by their
    /// ratingKey first (they keep their ids), then by strongest key and
    /// tiebreak, so a rebuild settles contested ids the same way.
    pub fn order(&self, claimants: &[Claimant]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..claimants.len()).collect();
        order.sort_by_cached_key(|&i| {
            let c = &claimants[i];
            let known = c.rating_key().is_some_and(|k| self.ledger.contains_key(k));
            (!known, c.strongest().to_owned(), c.tiebreak.clone())
        });
        order
    }

    /// Choose the public id of a real (backend) item and record its keys.
    /// Returns the id and the row that holds it, if any.
    pub async fn resolve(&mut self, tx: &mut Tx, c: &Claimant) -> Result<(String, Option<Holder>)> {
        let mut chosen = None;
        for p in self.candidates(c) {
            if !self.claimed(tx, &p, c).await? {
                chosen = Some(p);
                break;
            }
        }
        let chosen = match chosen {
            Some(p) => self.take_back(tx, c, p).await?,
            None => self.mint(tx, c).await?,
        };
        self.record(tx, c, &chosen).await?;
        let holder = self.holder(tx, &chosen).await?;
        Ok((chosen, holder))
    }

    /// The id `c` is minted from its strongest key (its canonical id) in
    /// place of `p`, when `p` only stood in for it: `c` was minted from a
    /// weaker key or as a duplicate because another copy held the canonical
    /// id. Once that copy is gone, the survivor takes the canonical id back,
    /// so the id clients have known longest is the one that lives on, and
    /// `p` becomes an alias of it. Only an id issued before is taken back,
    /// never one a newly added MBID would mint, and only by a full pass that
    /// knows the copy is gone: its row is deleted, or the tag pass listed the
    /// library without it. Without that listing (no mounted library), an
    /// unlisted copy may still exist, and the ratingKey keeps each its id.
    async fn take_back(&mut self, tx: &mut Tx, c: &Claimant, p: String) -> Result<String> {
        let strongest = c.strongest();
        if self.cx.incremental || strongest.is_empty() {
            return Ok(p);
        }
        let canonical = PublicId::mint(self.kind, strongest).to_string();
        if p == canonical || !self.stands_in(c, &p) {
            return Ok(p);
        }
        // Issued to this item's line: its strongest key still leads there
        // (not merely another key hashing to the same id).
        if self.ledger.get(strongest) != Some(&canonical) {
            return Ok(p);
        }
        let gone = match self.holder(tx, &canonical).await? {
            None => false,
            Some(h) if h.deleted_at.is_some() => true,
            Some(_) => self.has_listing(tx).await? && !self.claimed(tx, &canonical, c).await?,
        };
        if !gone {
            return Ok(p);
        }
        tracing::info!(
            kind = kind_str(self.kind),
            key = strongest,
            from = %p,
            to = %canonical,
            "the copy holding an id is gone; its duplicate takes the id back"
        );
        self.alias(tx, &p, &canonical).await?;
        Ok(canonical)
    }

    /// Whether `p` is an id [`Resolver::mint`] gives `c` when its canonical
    /// id is taken: from a weaker stable key, or a duplicate of the strongest.
    fn stands_in(&self, c: &Claimant, p: &str) -> bool {
        let mut stable = c.keys.iter().map(String::as_str).filter(|k| is_mintable(k));
        let Some(strongest) = stable.next() else {
            return false;
        };
        let minted = |k: &str| PublicId::mint(self.kind, k).to_string() == p;
        stable.any(minted) || (2..=MAX_DUPLICATE).any(|n| minted(&duplicate_key(strongest, n)))
    }

    /// Whether this pass's tag pass listed the library, so an item missing
    /// from the listing is known to be gone.
    async fn has_listing(&mut self, tx: &mut Tx) -> Result<bool> {
        if let Some(v) = self.has_listing {
            return Ok(v);
        }
        let v = any_listed(tx, self.cx.library.id, self.cx.generation).await?;
        self.has_listing = Some(v);
        Ok(v)
    }

    /// Record that `from`, an id whose item is gone, continues as `to`.
    async fn alias(&mut self, tx: &mut Tx, from: &str, to: &str) -> Result<()> {
        let key = alias_key(from);
        if from == to || self.ledger.get(&key).is_some_and(|p| p == to) {
            return Ok(());
        }
        self.write(tx, &[key.as_str()], to).await
    }

    /// Choose the public id of a virtual artist. Unlike a real item it doesn't
    /// claim the id: any row of this library it resolves to is the artist it
    /// names. The caller records its keys with [`Resolver::record`] when the
    /// row is the virtual artist's own.
    pub async fn resolve_link(
        &mut self,
        tx: &mut Tx,
        c: &Claimant,
    ) -> Result<(String, Option<Holder>)> {
        let lib = self.cx.library.id;
        let mut chosen = None;
        for p in self.candidates(c) {
            match self.holder(tx, &p).await? {
                Some(h) if h.library_id != lib => {}
                _ => {
                    chosen = Some(p);
                    break;
                }
            }
        }
        let chosen = match chosen {
            Some(p) => p,
            None => self.mint(tx, c).await?,
        };
        let holder = self.holder(tx, &chosen).await?;
        Ok((chosen, holder))
    }

    /// Ids reached through `c`'s keys: through its ratingKey first (the same
    /// backend item as before), then in key strength order.
    fn candidates(&self, c: &Claimant) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let keys = c
            .rating_key()
            .into_iter()
            .chain(c.keys.iter().map(String::as_str));
        for k in keys {
            if let Some(p) = self.ledger.get(k)
                && !out.contains(p)
            {
                out.push(p.clone());
            }
        }
        out
    }

    /// Whether another live item holds `public_id`, so `c` can't have it.
    async fn claimed(&mut self, tx: &mut Tx, public_id: &str, c: &Claimant) -> Result<bool> {
        if self.chosen.contains(public_id) {
            return Ok(true);
        }
        let Some(h) = self.holder(tx, public_id).await? else {
            return self.anchored(tx, public_id, c).await;
        };
        if h.deleted_at.is_some() {
            return Ok(false);
        }
        // Rows of other libraries can't be checked from this pass.
        if h.library_id != self.cx.library.id {
            return Ok(true);
        }
        if c.remote_key.is_some() && h.remote_key == c.remote_key {
            return Ok(false);
        }
        if h.generation >= self.cx.generation {
            return Ok(true);
        }
        // Not seen yet in this pass: taken over (a re-added item) unless its
        // item is still in the backend's listing and so comes later. Virtual
        // artists are taken over by a real one.
        match h.remote_key {
            None => Ok(false),
            Some(k) => self.still_listed(tx, &k).await,
        }
    }

    /// Whether an id no row holds yet (a database rebuilt from a snapshot)
    /// belongs to another item of this source that is still in the backend's
    /// listing, found through the ratingKeys the ledger records for the id.
    async fn anchored(&mut self, tx: &mut Tx, public_id: &str, c: &Claimant) -> Result<bool> {
        let prefix = rating_key(self.cx.source, "");
        let rows: Vec<LedgerRow> = tx
            .fetch_all(
                &Query::select()
                    .columns(["key", "public_id"])
                    .from("identity_keys")
                    .and_where(Expr::col("kind").eq(kind_str(self.kind)))
                    .and_where(Expr::col("public_id").eq(public_id))
                    .to_owned(),
            )
            .await?;
        for row in rows {
            let Some(key) = row.key.strip_prefix(&prefix) else {
                continue;
            };
            if Some(row.key.as_str()) != c.rating_key() && self.still_listed(tx, key).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether the item with backend `key` is in this pass's listing. An
    /// incremental pass lists only changed items, so it can't tell.
    async fn still_listed(&mut self, tx: &mut Tx, key: &str) -> Result<bool> {
        if self.cx.incremental {
            return Err(StoreError::NeedsFullSync(format!(
                "a {} contests the id of item {key}, which this pass didn't list",
                kind_str(self.kind)
            )));
        }
        if let Some(&v) = self.listed.get(key) {
            return Ok(v);
        }
        let v = listed(
            tx,
            self.cx.library.id,
            self.cx.generation,
            file_key(self.kind),
            key,
        )
        .await?;
        self.listed.insert(key.to_owned(), v);
        Ok(v)
    }

    /// Mint an id for `c`: from its strongest stable key, else its next one,
    /// else its strongest with `#2`, `#3`, … appended.
    async fn mint(&mut self, tx: &mut Tx, c: &Claimant) -> Result<String> {
        let stable: Vec<&str> = c
            .keys
            .iter()
            .map(String::as_str)
            .filter(|k| is_mintable(k))
            .collect();
        for k in &stable {
            let p = PublicId::mint(self.kind, k).to_string();
            if !self.taken(tx, &p).await? {
                return Ok(p);
            }
        }
        let base = stable.first().copied().unwrap_or_default();
        for n in 2.. {
            let p = PublicId::mint(self.kind, &duplicate_key(base, n)).to_string();
            if !self.taken(tx, &p).await? {
                tracing::info!(
                    kind = kind_str(self.kind),
                    key = base,
                    n,
                    "identity key already has an id; minted a duplicate"
                );
                return Ok(p);
            }
        }
        unreachable!()
    }

    /// Whether `public_id` belongs to anything: a row, or a ledger entry.
    async fn taken(&mut self, tx: &mut Tx, public_id: &str) -> Result<bool> {
        if self.chosen.contains(public_id) || self.ledger.values().any(|p| p == public_id) {
            return Ok(true);
        }
        if self.holder(tx, public_id).await?.is_some() {
            return Ok(true);
        }
        let rows: Vec<LedgerRow> = tx
            .fetch_all(
                &Query::select()
                    .columns(["key", "public_id"])
                    .from("identity_keys")
                    .and_where(Expr::col("public_id").eq(public_id))
                    .limit(1)
                    .to_owned(),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// Record `c`'s keys against `chosen`. A key pointing at another id is
    /// moved to `chosen`, unless that id is held by another live item (a
    /// duplicate copy shares its keys); the ratingKey always moves. Such a
    /// key still moves at the end of the batch if the item holding it was
    /// resolved without it: it was recorded there by mistake (say, an MBID
    /// Plex's filing gave the wrong artist).
    pub async fn record(&mut self, tx: &mut Tx, c: &Claimant, chosen: &str) -> Result<()> {
        self.chosen.insert(chosen.to_owned());
        self.carried
            .entry(chosen.to_owned())
            .or_default()
            .extend(c.keys.iter().cloned());
        let mut writes = Vec::new();
        // Ids whose item is gone and whose keys move to `chosen`.
        let mut gone = Vec::new();
        for k in &c.keys {
            match self.ledger.get(k).cloned() {
                None => writes.push(k.as_str()),
                Some(p) if p == chosen => self.seen.push(k.clone()),
                Some(p) => {
                    if is_mintable(k) {
                        if self.claimed(tx, &p, c).await? {
                            self.deferred.push((k.clone(), p, chosen.to_owned()));
                            continue;
                        }
                        if !gone.contains(&p) {
                            gone.push(p.clone());
                        }
                    }
                    tracing::info!(
                        kind = kind_str(self.kind),
                        key = %k,
                        from = %p,
                        to = chosen,
                        "identity key moved"
                    );
                    writes.push(k.as_str());
                }
            }
        }
        self.write(tx, &writes, chosen).await?;
        // Clients may still hold those ids: they now lead here.
        for p in gone {
            self.alias(tx, &p, chosen).await?;
        }
        Ok(())
    }

    /// Point `keys` at `public_id`.
    async fn write(&mut self, tx: &mut Tx, keys: &[&str], public_id: &str) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut q = Query::insert()
            .into_table("identity_keys")
            .columns(["kind", "key", "public_id", "first_seen_at", "last_seen_at"])
            .on_conflict(
                OnConflict::columns(["kind", "key"])
                    .update_columns(["public_id", "first_seen_at", "last_seen_at"])
                    .to_owned(),
            )
            .to_owned();
        for k in keys {
            q.values_panic([
                kind_str(self.kind).into(),
                (*k).into(),
                public_id.into(),
                self.now.into(),
                self.now.into(),
            ]);
        }
        tx.execute(&q).await?;
        for k in keys {
            self.ledger.insert((*k).to_owned(), public_id.to_owned());
        }
        Ok(())
    }

    /// Move the deferred keys whose holder was resolved in this batch without
    /// them, and stamp `last_seen_at` on the keys that were already current.
    pub async fn finish(mut self, tx: &mut Tx) -> Result<()> {
        for (k, from, to) in std::mem::take(&mut self.deferred) {
            let dropped = self
                .carried
                .get(&from)
                .is_some_and(|keys| !keys.contains(&k));
            if !dropped || self.ledger.get(&k) != Some(&from) {
                continue;
            }
            tracing::info!(
                kind = kind_str(self.kind),
                key = %k,
                from = %from,
                to = %to,
                "identity key moved off an item that no longer has it"
            );
            self.write(tx, &[k.as_str()], &to).await?;
        }
        for chunk in self.seen.chunks(CHUNK) {
            tx.execute(
                &Query::update()
                    .table("identity_keys")
                    .value("last_seen_at", self.now)
                    .and_where(Expr::col("kind").eq(kind_str(self.kind)))
                    .and_where(Expr::col("key").is_in(chunk.iter().map(String::as_str)))
                    .to_owned(),
            )
            .await?;
        }
        Ok(())
    }
}

/// Drop `rk:` keys unseen for [`RATING_KEY_TTL_MS`].
pub(crate) async fn prune_rating_keys(tx: &mut Tx) -> Result<u64> {
    tx.execute(
        &Query::delete()
            .from_table("identity_keys")
            .and_where(Expr::col("key").like("rk:%"))
            .and_where(Expr::col("last_seen_at").lt(now_ms() - RATING_KEY_TTL_MS))
            .to_owned(),
    )
    .await
}

impl Db {
    /// The identity keys recorded for a public id, sorted.
    pub async fn identity_keys(&self, public_id: &str) -> Result<Vec<String>> {
        let rows: Vec<LedgerRow> = self
            .fetch_all(
                &Query::select()
                    .columns(["key", "public_id"])
                    .from("identity_keys")
                    .and_where(Expr::col("public_id").eq(public_id))
                    .order_by("key", Order::Asc)
                    .to_owned(),
            )
            .await?;
        Ok(rows.into_iter().map(|r| r.key).collect())
    }
}

/// One identity ledger entry, as snapshotted, exported and imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct LedgerEntry {
    /// `artist`, `album` or `track`.
    pub kind: String,
    pub key: String,
    pub public_id: String,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
}

impl LedgerEntry {
    /// Refuse entries that could never have come from a ledger: an unknown
    /// kind, an empty key, or an id that isn't a public id of that kind.
    fn check(&self) -> Result<()> {
        let valid = parse_kind(&self.kind).is_some_and(|kind| {
            !self.key.is_empty()
                && self
                    .public_id
                    .parse::<PublicId>()
                    .is_ok_and(|p| p.kind() == kind)
        });
        if valid {
            Ok(())
        } else {
            Err(StoreError::Invalid(format!(
                "identity key {:?} ({}) -> {:?}",
                self.key, self.kind, self.public_id
            )))
        }
    }
}

impl Db {
    /// Whether the identity ledger is empty: a new or rebuilt database.
    pub async fn ledger_is_empty(&self) -> Result<bool> {
        let rows: Vec<LedgerRow> = self
            .fetch_all(
                &Query::select()
                    .columns(["key", "public_id"])
                    .from("identity_keys")
                    .limit(1)
                    .to_owned(),
            )
            .await?;
        Ok(rows.is_empty())
    }

    /// Up to `limit` ledger entries after `after` (a kind and key), in kind
    /// and key order: one page of an export.
    pub async fn ledger_page(
        &self,
        after: Option<(&str, &str)>,
        limit: u64,
    ) -> Result<Vec<LedgerEntry>> {
        let mut q = Query::select()
            .columns(["kind", "key", "public_id", "first_seen_at", "last_seen_at"])
            .from("identity_keys")
            .order_by("kind", Order::Asc)
            .order_by("key", Order::Asc)
            .limit(limit)
            .to_owned();
        if let Some((kind, key)) = after {
            q.cond_where(
                Cond::any()
                    .add(Expr::col("kind").gt(kind))
                    .add(Expr::col("kind").eq(kind).and(Expr::col("key").gt(key))),
            );
        }
        self.fetch_all(&q).await
    }

    /// Add `entries` to the ledger in one transaction. A key the ledger
    /// already has keeps its id, so importing is safe to repeat. Returns how
    /// many entries were added.
    pub async fn import_ledger(&self, entries: &[LedgerEntry]) -> Result<u64> {
        for e in entries {
            e.check()?;
        }
        let mut tx = self.begin().await?;
        let mut added = 0;
        for chunk in entries.chunks(CHUNK) {
            let mut q = Query::insert()
                .into_table("identity_keys")
                .columns(["kind", "key", "public_id", "first_seen_at", "last_seen_at"])
                .on_conflict(OnConflict::columns(["kind", "key"]).do_nothing().to_owned())
                .to_owned();
            for e in chunk {
                q.values_panic([
                    e.kind.as_str().into(),
                    e.key.as_str().into(),
                    e.public_id.as_str().into(),
                    e.first_seen_at.into(),
                    e.last_seen_at.into(),
                ]);
            }
            added += tx.execute(&q).await?;
        }
        tx.commit().await?;
        Ok(added)
    }
}
