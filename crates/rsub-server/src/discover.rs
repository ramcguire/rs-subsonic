//! Discovery endpoints: top songs, similar songs and artists, and lyrics.
//!
//! Top songs rank the catalog by the backend's popularity count (Plex's
//! `ratingCount`, the global listener count behind its "Popular Tracks"), so
//! they need no backend call. Similar artists and lyrics are live. Plex only
//! names similar artists that are in the library, which for many artists is
//! none, so similar artists fall back on the catalog: collaborators, then
//! artists with the same styles.

use std::collections::HashSet;

use futures_util::future::try_join_all;

use rsub_api::model::{ArtistId3, LyricLine, Lyrics, LyricsList, Songs, StructuredLyrics};
use rsub_api::{ApiError, Envelope, Payload};
use rsub_core::Kind;
use rsub_core::backend::{BackendError, LyricsDoc, RemoteRef};
use rsub_core::text::{normalize, search_terms};
use rsub_store::{ArtistRow, Page, TrackRow};

use crate::browse::item;
use crate::convert;
use crate::db_error;
use crate::handlers::{Ctx, backend_error};
use crate::ids::track;
use crate::media::track_remote;

/// Largest `count` for song lists (the Subsonic limit for album lists).
const MAX_SONGS: usize = 500;
/// Similar artists asked of the backend per seed artist.
const SIMILAR_ARTISTS: usize = 20;
/// Related artists wanted before the catalog fallbacks stop adding more.
const MIN_RELATED: usize = 8;
/// Tracks searched to find a song by artist and title for `getLyrics`.
const LYRICS_CANDIDATES: u64 = 50;

fn count(ctx: &Ctx<'_>, default: usize) -> Result<usize, ApiError> {
    Ok(ctx.params.parse_or("count", default)?.min(MAX_SONGS))
}

/// `getTopSongs`: the artist's most popular songs, one copy of each.
///
/// Subsonic and Navidrome take these from Last.fm's top tracks for the artist
/// name. Plex's `ratingCount` is the same kind of signal: global popularity,
/// not the user's plays (the per-user "most played" is `getAlbumList2
/// ?type=frequent`). Every artist of that name counts, virtual ones included.
pub async fn top_songs(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let name = ctx.params.required("artist")?;
    let limit = count(ctx, 50)?;
    let lib = ctx.folder()?;
    let db = &ctx.state.db;
    let ids: Vec<i64> = db
        .artists_named(name, lib)
        .await
        .map_err(db_error)?
        .into_iter()
        .map(|a| a.id)
        .collect();
    let rows = db
        .popular_tracks(&ids, lib, limit)
        .await
        .map_err(db_error)?;
    Ok(Envelope::with(Payload::TopSongs(Songs {
        song: convert::songs(ctx.view(), &rows).await?,
    })))
}

/// `getSimilarSongs` and `getSimilarSongs2`: popular songs by the seed's
/// artists and the artists the backend calls similar, taking turns so no
/// artist dominates. Both accept an artist, album or song id (subwave sends
/// song ids to `getSimilarSongs2`, as Navidrome allows). A seed song is left
/// out of its own list.
pub async fn similar_songs(ctx: &Ctx<'_>, v2: bool) -> Result<Envelope, ApiError> {
    let (kind, id) = item(ctx, ctx.params.required("id")?).await?;
    let limit = count(ctx, 50)?;
    let db = &ctx.state.db;
    let seeds = seed_artists(ctx, kind, id).await?;
    let mut artists = seeds.clone();
    let seed_track = (kind == Kind::Track).then_some(id);
    artists.extend(related_artists(ctx, &seeds, seed_track, true).await?);
    // Enough of each artist's songs to fill the list, and a few spare for the
    // seed song and artists with short lists.
    let per_artist = limit.div_ceil(artists.len().max(1)) + 2;
    let lists =
        try_join_all(artists.iter().map(|a| {
            db.popular_tracks(std::slice::from_ref(&a.id), Some(a.library_id), per_artist)
        }))
        .await
        .map_err(db_error)?;
    let lists = lists.into_iter().map(Vec::into_iter).collect();
    let skip = (kind == Kind::Track).then_some(id);
    let rows = interleave(lists, skip, limit);
    let songs = Songs {
        song: convert::songs(ctx.view(), &rows).await?,
    };
    Ok(Envelope::with(if v2 {
        Payload::SimilarSongs2(songs)
    } else {
        Payload::SimilarSongs(songs)
    }))
}

/// One song from each list in turn, skipping `skip` and songs already taken
/// (an artist's song can also appear under a co-credited artist).
fn interleave(
    mut lists: Vec<std::vec::IntoIter<TrackRow>>,
    skip: Option<i64>,
    limit: usize,
) -> Vec<TrackRow> {
    let mut seen: HashSet<i64> = skip.into_iter().collect();
    let mut out = Vec::new();
    while out.len() < limit {
        let mut any = false;
        for list in &mut lists {
            if out.len() == limit {
                break;
            }
            if let Some(t) = list.find(|t| !seen.contains(&t.id)) {
                seen.insert(t.id);
                out.push(t);
                any = true;
            }
        }
        if !any {
            break;
        }
    }
    out
}

/// The artists a seed stands for: an artist itself, an album's album artists,
/// or a song's first track artist and first album artist.
async fn seed_artists(ctx: &Ctx<'_>, kind: Kind, id: i64) -> Result<Vec<ArtistRow>, ApiError> {
    let db = &ctx.state.db;
    let ids: Vec<i64> = match kind {
        Kind::Artist => vec![id],
        Kind::Album => db
            .album_credits(&[id])
            .await
            .map_err(db_error)?
            .into_iter()
            .map(|c| c.artist_id)
            .collect(),
        _ => match db.track(id).await.map_err(db_error)? {
            Some(t) => t.artist_id.into_iter().chain(t.album_artist_id).collect(),
            None => Vec::new(),
        },
    };
    db.artists_by_ids(&ids).await.map_err(db_error)
}

/// Artists related to the seeds, best first: those the backend calls similar,
/// then, while fewer than [`MIN_RELATED`], collaborators, artists with the
/// same styles, and (with sonic similarity on) the artists of the tracks that
/// sound most like `seed_track`, or like the seeds' most popular analysed
/// track. Seeds and their namesakes are left out. With `strict`, a backend
/// error fails the call; otherwise it's logged and the catalog fallbacks
/// stand in.
#[cfg_attr(not(feature = "sonic"), allow(unused_variables))]
async fn related_artists(
    ctx: &Ctx<'_>,
    seeds: &[ArtistRow],
    seed_track: Option<i64>,
    strict: bool,
) -> Result<Vec<ArtistRow>, ApiError> {
    let Some(library) = seeds.first().map(|a| a.library_id) else {
        return Ok(Vec::new());
    };
    let mut names: HashSet<String> = seeds.iter().map(|a| normalize(&a.name)).collect();
    let mut out = Vec::new();
    let mut add = |out: &mut Vec<ArtistRow>, a: ArtistRow| {
        if names.insert(normalize(&a.name)) {
            out.push(a);
        }
    };
    for a in seeds {
        match similar_artists(ctx, a).await {
            Ok(rows) => rows.into_iter().for_each(|s| add(&mut out, s)),
            Err(e) if strict => return Err(e),
            Err(e) => tracing::warn!("similar artists unavailable: {}", e.message),
        }
    }
    let seeds: Vec<ArtistRow> = seeds
        .iter()
        .filter(|a| a.library_id == library)
        .cloned()
        .collect();
    let db = &ctx.state.db;
    if out.len() < MIN_RELATED {
        let more = db
            .collaborators(&seeds, library, MIN_RELATED - out.len())
            .await
            .map_err(db_error)?;
        more.into_iter().for_each(|r| add(&mut out, r.artist));
    }
    if out.len() < MIN_RELATED {
        // Extra for artists already added as collaborators.
        let more = db
            .style_neighbours(&seeds, library, MIN_RELATED)
            .await
            .map_err(db_error)?;
        for r in more {
            if out.len() == MIN_RELATED {
                break;
            }
            add(&mut out, r.artist);
        }
    }
    #[cfg(feature = "sonic")]
    if out.len() < MIN_RELATED {
        for a in crate::sonic::neighbour_artists(ctx, &seeds, library, seed_track).await? {
            if out.len() == MIN_RELATED {
                break;
            }
            add(&mut out, a);
        }
    }
    Ok(out)
}

/// The catalog artists the backend calls similar to `a`, in its order. Virtual
/// artists have none, and neither does a backend without discovery.
async fn similar_artists(ctx: &Ctx<'_>, a: &ArtistRow) -> Result<Vec<ArtistRow>, ApiError> {
    let Some(key) = &a.remote_key else {
        return Ok(Vec::new());
    };
    let lib = ctx
        .state
        .db
        .library(a.library_id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("Library"))?;
    let rt = ctx.runtime(lib.source_id)?;
    let artist = RemoteRef {
        key: key.clone(),
        guid: None,
    };
    let keys = match rt
        .backend
        .discovery
        .similar_artists(&ctx.remote(), &artist, SIMILAR_ARTISTS)
        .await
    {
        Ok(keys) => keys,
        Err(BackendError::Unsupported) => return Ok(Vec::new()),
        Err(e) => return Err(backend_error(e)),
    };
    let mut rows = ctx
        .state
        .db
        .artists_by_remote(lib.source_id, &keys)
        .await
        .map_err(db_error)?;
    rows.retain(|r| r.library_id == a.library_id);
    rows.sort_by_key(|r| keys.iter().position(|k| Some(k) == r.remote_key.as_ref()));
    Ok(rows)
}

/// `similarArtist` for `getArtistInfo(2)`: [`related_artists`]. Like user
/// fields, the backend's part is left out (with a warning) when it fails,
/// rather than failing the page.
pub async fn similar_artists_id3(
    ctx: &Ctx<'_>,
    a: &ArtistRow,
    limit: usize,
) -> Result<Vec<ArtistId3>, ApiError> {
    let mut rows = related_artists(ctx, std::slice::from_ref(a), None, false).await?;
    rows.truncate(limit);
    convert::artists_id3(ctx.view(), &rows).await
}

/// OpenSubsonic `getLyricsBySongId`: every lyrics document of the song.
pub async fn lyrics_by_song_id(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let t = track(ctx, "id").await?;
    let docs = song_lyrics(ctx, &t).await?;
    Ok(Envelope::with(LyricsList {
        structured_lyrics: docs.into_iter().map(structured).collect(),
    }))
}

/// `getLyrics`: the plain text of the first song matching `artist` and
/// `title` that has lyrics. Synced lyrics lose their times.
pub async fn lyrics(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let artist = ctx.params.get("artist").unwrap_or("");
    let title = ctx.params.get("title").unwrap_or("");
    if title.trim().is_empty() {
        return Ok(Envelope::with(Lyrics::default()));
    }
    let terms = search_terms(&format!("{title} {artist}"));
    let rows = ctx
        .state
        .db
        .search_tracks(&terms, Page::first(LYRICS_CANDIDATES))
        .await
        .map_err(db_error)?;
    let (title_n, artist_n) = (normalize(title), normalize(artist));
    let found = rows.into_iter().find(|t| {
        t.has_lyrics
            && normalize(&t.title) == title_n
            && normalize(&t.display_artist).contains(&artist_n)
    });
    let Some(t) = found else {
        return Ok(Envelope::with(Lyrics::default()));
    };
    let docs = song_lyrics(ctx, &t).await?;
    let Some(doc) = docs.into_iter().next() else {
        return Ok(Envelope::with(Lyrics::default()));
    };
    let text: Vec<String> = doc.lines.into_iter().map(|l| l.text).collect();
    Ok(Envelope::with(Lyrics {
        artist: Some(t.display_artist),
        title: Some(t.title),
        value: Some(text.join("\n")),
    }))
}

/// A song's lyrics from its backend. Songs the catalog records as having none
/// cost no backend call.
async fn song_lyrics(ctx: &Ctx<'_>, t: &TrackRow) -> Result<Vec<LyricsDoc>, ApiError> {
    if !t.has_lyrics {
        return Ok(Vec::new());
    }
    let rt = ctx.library_runtime(t.library_id).await?;
    match rt
        .backend
        .media
        .lyrics(&ctx.remote(), &track_remote(t))
        .await
    {
        Ok(docs) => Ok(docs),
        Err(BackendError::Unsupported) => Ok(Vec::new()),
        Err(e) => Err(backend_error(e)),
    }
}

fn structured(d: LyricsDoc) -> StructuredLyrics {
    StructuredLyrics {
        display_artist: d.display_artist,
        display_title: d.display_title,
        lang: if d.lang.is_empty() {
            "und".into()
        } else {
            d.lang
        },
        offset: (d.offset_ms != 0).then_some(d.offset_ms),
        synced: d.synced,
        line: d
            .lines
            .into_iter()
            .map(|l| LyricLine {
                start: l.start_ms.filter(|_| d.synced).map(|s| s as i64),
                value: l.text,
            })
            .collect(),
    }
}
