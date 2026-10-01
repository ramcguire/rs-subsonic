//! OpenSubsonic `sonicSimilarity`: `getSonicSimilarTracks` and `findSonicPath`
//! over the stored analysis (`rsub-analysis`), searched in the database.
//! Answered when an analyzer is configured, and advertised once some tracks
//! have vectors.

use std::collections::{HashMap, HashSet};

use rsub_analysis::{Match, SonicSearch};
use rsub_api::model::SonicMatch;
use rsub_api::{ApiError, Envelope, Payload};
use rsub_core::text::{normalize, search_terms};
use rsub_store::{ArtistRow, Page, TrackRow};

use crate::AppState;
use crate::convert;
use crate::db_error;
use crate::handlers::Ctx;
use crate::ids::track;

/// Largest `count`.
const MAX_COUNT: usize = 500;
/// Neighbours read per match wanted, leaving room for copies of a song.
const OVERSAMPLE: usize = 3;
/// Tracks searched by title to find copies of a path's ends.
const COPY_CANDIDATES: u64 = 50;
/// An artist's most popular tracks tried as the seed for neighbour artists.
const SONIC_SEEDS: usize = 5;
/// Sonic neighbours whose artists may fill the related artists.
const SONIC_NEIGHBOURS: usize = 60;

/// Whether sonic similarity is on and some tracks have vectors.
pub async fn ready(state: &AppState) -> bool {
    let Some(search) = &state.sonic else {
        return false;
    };
    search.is_ready().await.unwrap_or_else(|e| {
        tracing::error!("database error: {e}");
        false
    })
}

/// The search, when sonic similarity is on. Before any track is analysed
/// every seed has no neighbours, so this doesn't ask the database.
fn search<'a>(ctx: &Ctx<'a>) -> Result<&'a SonicSearch, ApiError> {
    ctx.state
        .sonic
        .as_ref()
        .ok_or_else(|| ApiError::generic("Sonic similarity is not enabled on this server."))
}

/// Same song, by the same artist: another release or copy of it. The lead
/// artist is the first credited one, so "A & B ft. C" and "A, B & C" agree.
fn song_key(t: &TrackRow) -> (String, String) {
    let artist = t
        .artist_id
        .or(t.album_artist_id)
        .map_or_else(|| normalize(&t.display_artist), |id| id.to_string());
    (normalize(&t.title), artist)
}

/// `getSonicSimilarTracks`: the songs that sound most like `id`, most similar
/// first. Other copies of the song are left out, and each song appears once.
/// A song not analysed yet has none.
pub async fn similar_tracks(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let search = search(ctx)?;
    let seed = track(ctx, "id").await?;
    let count = ctx.params.parse_or("count", 10usize)?.min(MAX_COUNT);
    let matches = search
        .similar(seed.id, count * OVERSAMPLE + 5, &[])
        .await
        .map_err(db_error)?
        .unwrap_or_default();
    let mut seen: HashSet<(String, String)> = [song_key(&seed)].into();
    let mut rows = rows_for(ctx, &matches).await?;
    let kept: Vec<(TrackRow, f32)> = matches
        .iter()
        .filter_map(|m| Some((rows.remove(&m.track_id)?, m.similarity)))
        .filter(|(t, _)| seen.insert(song_key(t)))
        .take(count)
        .collect();
    respond(ctx, kept).await
}

/// `findSonicPath`: `count` songs leading from `startSongId` to `endSongId`
/// through sound, both ends included; similarities are to the start.
pub async fn path(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let search = search(ctx)?;
    let start = track(ctx, "startSongId").await?;
    let end = track(ctx, "endSongId").await?;
    let count = ctx.params.parse_or("count", 25usize)?.clamp(2, MAX_COUNT);
    // Copies of either end would stand still on the path.
    let mut copies: HashSet<i64> = HashSet::new();
    for t in [&start, &end] {
        let page = Page::first(COPY_CANDIDATES);
        let rows = ctx
            .state
            .db
            .search_tracks(&search_terms(&t.title), page)
            .await
            .map_err(db_error)?;
        copies.extend(
            rows.iter()
                .filter(|r| song_key(r) == song_key(t))
                .map(|r| r.id),
        );
    }
    let copies: Vec<i64> = copies.into_iter().collect();
    let Some(matches) = search
        .path(start.id, end.id, count, &copies)
        .await
        .map_err(db_error)?
    else {
        return Err(ApiError::generic(
            "The start or end song has not been analysed yet.",
        ));
    };
    let mut rows = rows_for(ctx, &matches).await?;
    let kept = matches
        .iter()
        .filter_map(|m| Some((rows.remove(&m.track_id)?, m.similarity)))
        .collect();
    respond(ctx, kept).await
}

async fn rows_for(ctx: &Ctx<'_>, matches: &[Match]) -> Result<HashMap<i64, TrackRow>, ApiError> {
    let ids: Vec<i64> = matches.iter().map(|m| m.track_id).collect();
    Ok(ctx
        .state
        .db
        .tracks_by_ids(&ids)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|t| (t.id, t))
        .collect())
}

async fn respond(ctx: &Ctx<'_>, kept: Vec<(TrackRow, f32)>) -> Result<Envelope, ApiError> {
    let (rows, sims): (Vec<TrackRow>, Vec<f32>) = kept.into_iter().unzip();
    let songs = convert::songs(ctx.view(), &rows).await?;
    Ok(Envelope::with(Payload::SonicMatches(
        songs
            .into_iter()
            .zip(sims)
            .map(|(entry, similarity)| SonicMatch {
                entry,
                similarity: (similarity * 1000.0).round() / 1000.0,
            })
            .collect(),
    )))
}

/// The artists of the tracks that sound most like `seed_track`, or like the
/// seeds' most popular analysed track, in order of their nearest track: the
/// last tier of similar artists (`discover::related_artists`). Empty when
/// sonic similarity is off.
pub async fn neighbour_artists(
    ctx: &Ctx<'_>,
    seeds: &[ArtistRow],
    library: i64,
    seed_track: Option<i64>,
) -> Result<Vec<ArtistRow>, ApiError> {
    let Some(search) = &ctx.state.sonic else {
        return Ok(Vec::new());
    };
    let db = &ctx.state.db;
    // Neighbours of the seed track, or else of the first analysed of the
    // seeds' most popular tracks.
    let mut near = match seed_track {
        Some(t) => search
            .similar(t, SONIC_NEIGHBOURS, &[])
            .await
            .map_err(db_error)?,
        None => None,
    };
    if near.is_none() {
        let ids: Vec<i64> = seeds.iter().map(|a| a.id).collect();
        let popular = db
            .popular_tracks(&ids, Some(library), SONIC_SEEDS)
            .await
            .map_err(db_error)?;
        for t in popular {
            near = search
                .similar(t.id, SONIC_NEIGHBOURS, &[])
                .await
                .map_err(db_error)?;
            if near.is_some() {
                break;
            }
        }
    }
    let near = near.unwrap_or_default();
    let tracks = rows_for(ctx, &near).await?;
    let mut artist_ids: Vec<i64> = Vec::new();
    for m in &near {
        if let Some(a) = tracks
            .get(&m.track_id)
            .and_then(|t| t.artist_id.or(t.album_artist_id))
            && !artist_ids.contains(&a)
        {
            artist_ids.push(a);
        }
    }
    let mut rows: HashMap<i64, ArtistRow> = db
        .artists_by_ids(&artist_ids)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    Ok(artist_ids
        .into_iter()
        .filter_map(|id| rows.remove(&id))
        .collect())
}
