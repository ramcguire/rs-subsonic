//! Public ids at the API edge: parse what clients send and find the row behind
//! it. Malformed, wrong-kind and unknown ids all come back as `None`, which
//! handlers report as "not found".

use rsub_api::ApiError;
use rsub_core::{Kind, PublicId};
use rsub_store::TrackRow;

use crate::db_error;
use crate::handlers::Ctx;

/// The row behind `raw` if it names an item of kind `kind`.
pub async fn lookup(ctx: &Ctx<'_>, raw: &str, kind: Kind) -> Result<Option<i64>, ApiError> {
    match raw.parse::<PublicId>() {
        Ok(id) if id.kind() == kind => ctx.state.db.internal_id(id).await.map_err(db_error),
        _ => Ok(None),
    }
}

/// The track named by the required parameter `param`.
pub async fn track(ctx: &Ctx<'_>, param: &str) -> Result<TrackRow, ApiError> {
    let not_found = || ApiError::not_found("Song");
    let id = lookup(ctx, ctx.params.required(param)?, Kind::Track)
        .await?
        .ok_or_else(not_found)?;
    ctx.state
        .db
        .track(id)
        .await
        .map_err(db_error)?
        .ok_or_else(not_found)
}

/// The kind `raw` names and the row behind it.
pub async fn lookup_any(ctx: &Ctx<'_>, raw: &str) -> Result<Option<(Kind, i64)>, ApiError> {
    let Ok(id) = raw.parse::<PublicId>() else {
        return Ok(None);
    };
    let found = ctx.state.db.internal_id(id).await.map_err(db_error)?;
    Ok(found.map(|i| (id.kind(), i)))
}

/// [`lookup`] for many ids, in order; `None` unless every one is found.
pub async fn lookup_all<'a>(
    ctx: &Ctx<'_>,
    raw: impl IntoIterator<Item = &'a str>,
    kind: Kind,
) -> Result<Option<Vec<i64>>, ApiError> {
    let mut ids = Vec::new();
    for r in raw {
        match r.parse::<PublicId>() {
            Ok(id) if id.kind() == kind => ids.push(id),
            _ => return Ok(None),
        }
    }
    let found = ctx.state.db.internal_ids(&ids).await.map_err(db_error)?;
    Ok(found.into_iter().collect())
}
