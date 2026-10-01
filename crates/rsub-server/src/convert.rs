//! Catalog rows → Subsonic wire types. Side data (credits, tags, the user's
//! stars, ratings and plays) is loaded in batches per response to keep query
//! counts constant; user state comes from the backend, live.

use std::collections::HashMap;
use std::time::Duration;

use rsub_api::ApiError;
use rsub_api::model::{
    AlbumId3, Artist, ArtistId3, ArtistRef, Child, Contributor, ItemDate, ItemGenre, RecordLabel,
    ReplayGain,
};
use rsub_core::Kind;
use rsub_core::backend::{BackendError, RemoteState};
use rsub_core::text::{iso8601, thumb_version};
use rsub_store::{AlbumRow, ArtistRow, CreditRow, Db, TagLink, TrackRow};

use crate::handlers::remote;
use crate::{SourceRuntime, db_error};

/// Whose view of the catalog a response renders.
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub db: &'a Db,
    pub user: i64,
    pub sources: &'a HashMap<i64, SourceRuntime>,
}

/// How long a response waits for user state before rendering without it. The
/// backend's own timeout is a minute, meant for sync.
const STATE_TIMEOUT: Duration = Duration::from_secs(3);

/// A user's state for one item, by row id.
type States = HashMap<i64, RemoteState>;

/// The user's state for these rows (`(id, library, backend key)`), read live
/// from each row's backend; virtual artists (no key) use the local ratings. A
/// backend that can't be reached, or doesn't answer within [`STATE_TIMEOUT`],
/// leaves its items without state: browsing still works when Plex is down.
async fn states(
    v: View<'_>,
    kind: Kind,
    rows: &[(i64, i64, Option<&str>)],
) -> Result<States, ApiError> {
    let mut by_lib: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    let mut local = Vec::new();
    for (id, lib, key) in rows {
        match key {
            Some(k) => by_lib.entry(*lib).or_default().push((*id, (*k).to_owned())),
            None => local.push(*id),
        }
    }
    let mut out = States::new();
    if !local.is_empty() {
        for r in v.db.local_ratings(v.user, &local).await.map_err(db_error)? {
            out.insert(
                r.artist_id,
                RemoteState {
                    rating: Some(r.rating.clamp(1, 5) as u8),
                    rated_at: Some(r.rated_at),
                    ..Default::default()
                },
            );
        }
    }
    if by_lib.is_empty() {
        return Ok(out);
    }
    let libs = v.db.libraries().await.map_err(db_error)?;
    for (lib_id, items) in by_lib {
        let Some(lib) = libs.iter().find(|l| l.id == lib_id) else {
            continue;
        };
        let Some(rt) = v.sources.get(&lib.source_id) else {
            continue;
        };
        let keys: Vec<String> = items.iter().map(|(_, k)| k.clone()).collect();
        let ids: HashMap<String, i64> = items.into_iter().map(|(id, k)| (k, id)).collect();
        let cx = remote(v.user);
        let read = rt.backend.state.states(&cx, &lib.remote_key, kind, &keys);
        match tokio::time::timeout(STATE_TIMEOUT, read).await {
            Err(_) => tracing::warn!(library = %lib.name, "user state timed out"),
            Ok(Ok(found)) => {
                for s in found {
                    if let Some(id) = ids.get(&s.key) {
                        out.insert(*id, s);
                    }
                }
            }
            Ok(Err(BackendError::Unsupported)) => {}
            Ok(Err(e)) => tracing::warn!(library = %lib.name, "user state unavailable: {e}"),
        }
    }
    Ok(out)
}

/// `(starred, userRating, playCount, played)` for an item. A star is a 5-star
/// rating, dated when it was rated.
struct Marks {
    starred: Option<String>,
    rating: Option<i64>,
    play_count: Option<i64>,
    played: Option<String>,
}

fn marks(s: Option<&RemoteState>) -> Marks {
    Marks {
        starred: s
            .filter(|s| s.starred())
            .map(|s| iso8601(s.rated_at.unwrap_or(0))),
        rating: s.and_then(|s| s.rating).map(i64::from),
        play_count: s.and_then(|s| s.play_count).map(i64::from),
        played: s.and_then(|s| s.last_played_at).map(iso8601),
    }
}

/// The [`CoverArtId`](rsub_core::CoverArtId) of an item with artwork.
pub(crate) fn cover(public_id: &str, thumb: Option<&str>) -> Option<String> {
    thumb.map(|t| format!("{public_id}-{:08x}", thumb_version(t)))
}

pub(crate) fn secs(ms: i64) -> i64 {
    (ms + 500) / 1000
}

fn group<T>(rows: Vec<T>, key: impl Fn(&T) -> i64) -> HashMap<i64, Vec<T>> {
    let mut map: HashMap<i64, Vec<T>> = HashMap::new();
    for r in rows {
        map.entry(key(&r)).or_default().push(r);
    }
    map
}

fn artist_ref(c: &CreditRow) -> ArtistRef {
    ArtistRef {
        id: c.artist_public_id.clone(),
        name: c.name.clone(),
    }
}

fn names_of(tags: Option<&Vec<TagLink>>, kind: &str) -> Vec<String> {
    tags.map(|v| {
        v.iter()
            .filter(|t| t.kind == kind)
            .map(|t| t.name.clone())
            .collect()
    })
    .unwrap_or_default()
}

pub async fn artist_states(v: View<'_>, artists: &[ArtistRow]) -> Result<States, ApiError> {
    let rows: Vec<_> = artists
        .iter()
        .map(|a| (a.id, a.library_id, a.remote_key.as_deref()))
        .collect();
    states(v, Kind::Artist, &rows).await
}

pub fn artist_folder(a: &ArtistRow, st: &States) -> Artist {
    let m = marks(st.get(&a.id));
    Artist {
        id: a.public_id.clone(),
        name: a.name.clone(),
        cover_art: cover(&a.public_id, a.thumb_ref.as_deref()),
        starred: m.starred,
        user_rating: m.rating,
        album_count: Some(a.album_count),
    }
}

pub fn artist_id3(a: &ArtistRow, st: &States) -> ArtistId3 {
    ArtistId3 {
        id: a.public_id.clone(),
        name: a.name.clone(),
        cover_art: cover(&a.public_id, a.thumb_ref.as_deref()),
        album_count: a.album_count,
        starred: marks(st.get(&a.id)).starred,
        music_brainz_id: a.mbid.clone(),
        sort_name: a.sort_key.clone(),
        roles: if a.album_count > 0 {
            vec!["albumartist".into()]
        } else {
            vec!["artist".into()]
        },
        album: None,
    }
}

/// Per-response side data for albums.
pub struct AlbumExtras {
    credits: HashMap<i64, Vec<CreditRow>>,
    tags: HashMap<i64, Vec<TagLink>>,
    states: States,
}

impl AlbumExtras {
    pub async fn load(v: View<'_>, albums: &[AlbumRow]) -> Result<Self, ApiError> {
        let db = v.db;
        let ids: Vec<i64> = albums.iter().map(|a| a.id).collect();
        let rows: Vec<_> = albums
            .iter()
            .map(|a| (a.id, a.library_id, a.remote_key.as_deref()))
            .collect();
        // The backend's answer takes longest: read the rest meanwhile.
        let (credits, tags, states) = tokio::try_join!(
            async { db.album_credits(&ids).await.map_err(db_error) },
            async { db.album_tags(&ids).await.map_err(db_error) },
            states(v, Kind::Album, &rows),
        )?;
        Ok(AlbumExtras {
            credits: group(credits, |c| c.owner_id),
            tags: group(tags, |t| t.owner_id),
            states,
        })
    }
}

pub fn album_id3(a: &AlbumRow, x: &AlbumExtras) -> AlbumId3 {
    let tags = x.tags.get(&a.id);
    let genres = names_of(tags, "genre");
    let m = marks(x.states.get(&a.id));
    AlbumId3 {
        id: a.public_id.clone(),
        name: a.title.clone(),
        artist: Some(a.display_artist.clone()),
        artist_id: a.artist_public_id.clone(),
        cover_art: cover(&a.public_id, a.thumb_ref.as_deref()),
        song_count: a.song_count,
        duration: secs(a.duration_ms),
        play_count: m.play_count,
        created: iso8601(a.added_at),
        starred: m.starred,
        year: a.year,
        genre: genres.first().cloned(),
        played: m.played,
        user_rating: m.rating,
        record_labels: a
            .label
            .iter()
            .map(|l| RecordLabel { name: l.clone() })
            .collect(),
        music_brainz_id: a.mbid.clone(),
        genres: genres.into_iter().map(|name| ItemGenre { name }).collect(),
        artists: x
            .credits
            .get(&a.id)
            .map(|v| v.iter().map(artist_ref).collect())
            .unwrap_or_default(),
        display_artist: a.display_artist.clone(),
        release_types: serde_json::from_str(&a.release_types).unwrap_or_default(),
        moods: names_of(tags, "mood"),
        sort_name: a.sort_key.clone(),
        original_release_date: a.orig_release_date.as_deref().and_then(ItemDate::parse),
        release_date: a
            .release_date
            .as_deref()
            .and_then(ItemDate::parse)
            .or_else(|| {
                a.year.map(|y| ItemDate {
                    year: Some(y as i32),
                    month: None,
                    day: None,
                })
            }),
        is_compilation: a.is_compilation,
        song: None,
    }
}

/// Folder-style album entry (`getAlbumList`, `search2`, artist directories).
pub fn album_child(a: &AlbumRow, x: &AlbumExtras) -> Child {
    let m = marks(x.states.get(&a.id));
    Child {
        id: a.public_id.clone(),
        parent: a.artist_public_id.clone(),
        is_dir: true,
        title: a.title.clone(),
        album: Some(a.title.clone()),
        artist: Some(a.display_artist.clone()),
        year: a.year,
        genre: names_of(x.tags.get(&a.id), "genre").into_iter().next(),
        cover_art: cover(&a.public_id, a.thumb_ref.as_deref()),
        duration: Some(secs(a.duration_ms)),
        created: Some(iso8601(a.added_at)),
        album_id: Some(a.public_id.clone()),
        artist_id: a.artist_public_id.clone(),
        starred: m.starred,
        user_rating: m.rating,
        play_count: m.play_count,
        played: m.played,
        ..Default::default()
    }
}

/// Per-response side data for songs.
pub struct TrackExtras {
    credits: HashMap<i64, Vec<CreditRow>>,
    track_tags: HashMap<i64, Vec<TagLink>>,
    album_tags: HashMap<i64, Vec<TagLink>>,
    states: States,
}

impl TrackExtras {
    pub async fn load(v: View<'_>, tracks: &[TrackRow]) -> Result<Self, ApiError> {
        let db = v.db;
        let ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
        let rows: Vec<_> = tracks
            .iter()
            .map(|t| (t.id, t.library_id, Some(t.remote_key.as_str())))
            .collect();
        let mut album_ids: Vec<i64> = tracks.iter().map(|t| t.album_id).collect();
        album_ids.sort_unstable();
        album_ids.dedup();
        let (credits, track_tags, album_tags, states) = tokio::try_join!(
            async { db.track_credits(&ids).await.map_err(db_error) },
            async { db.track_tags(&ids).await.map_err(db_error) },
            async { db.album_tags(&album_ids).await.map_err(db_error) },
            states(v, Kind::Track, &rows),
        )?;
        Ok(TrackExtras {
            credits: group(credits, |c| c.owner_id),
            track_tags: group(track_tags, |t| t.owner_id),
            album_tags: group(album_tags, |t| t.owner_id),
            states,
        })
    }
}

/// Replace path separators so names can't create fake directories.
fn path_part(s: &str) -> String {
    s.replace(['/', '\\'], "_")
}

pub fn song(t: &TrackRow, x: &TrackExtras) -> Child {
    let credits = x.credits.get(&t.id).map(Vec::as_slice).unwrap_or_default();
    let by_role = |role: &str| -> Vec<ArtistRef> {
        credits
            .iter()
            .filter(|c| c.role == role)
            .map(artist_ref)
            .collect()
    };
    let mut genres = names_of(x.track_tags.get(&t.id), "genre");
    if genres.is_empty() {
        genres = names_of(x.album_tags.get(&t.album_id), "genre");
    }
    let suffix = t.suffix.clone();
    let path = format!(
        "{}/{}/{}{:02} - {}{}",
        path_part(&t.album_display_artist),
        path_part(&t.album_title),
        t.disc_no
            .filter(|d| *d > 1)
            .map(|d| format!("{d}-"))
            .unwrap_or_default(),
        t.track_no.unwrap_or(0),
        path_part(&t.title),
        suffix
            .as_deref()
            .map(|s| format!(".{s}"))
            .unwrap_or_default(),
    );
    let m = marks(x.states.get(&t.id));
    let rg = ReplayGain {
        track_gain: t.rg_track_gain,
        album_gain: t.rg_album_gain,
        track_peak: t.rg_track_peak,
        album_peak: t.rg_album_peak,
    };
    Child {
        id: t.public_id.clone(),
        parent: Some(t.album_public_id.clone()),
        is_dir: false,
        title: t.title.clone(),
        album: Some(t.album_title.clone()),
        artist: Some(t.display_artist.clone()),
        track: t.track_no,
        year: t.year,
        genre: genres.first().cloned(),
        cover_art: cover(&t.album_public_id, t.album_thumb_ref.as_deref()),
        size: t.size,
        content_type: t.content_type.clone(),
        suffix,
        duration: Some(secs(t.duration_ms)),
        bit_rate: t.bitrate,
        path: Some(path),
        is_video: Some(false),
        user_rating: m.rating,
        play_count: m.play_count,
        starred: m.starred,
        played: m.played,
        disc_number: t.disc_no,
        created: Some(iso8601(t.added_at)),
        album_id: Some(t.album_public_id.clone()),
        artist_id: t
            .artist_public_id
            .clone()
            .or_else(|| t.album_artist_public_id.clone()),
        kind: Some("music"),
        media_type: Some("song"),
        bpm: t.bpm,
        comment: t.comment.clone(),
        sort_name: Some(t.sort_key.clone()),
        music_brainz_id: t.mbid.clone(),
        genres: Some(genres.into_iter().map(|name| ItemGenre { name }).collect()),
        artists: Some(by_role("artist")),
        display_artist: Some(t.display_artist.clone()),
        album_artists: Some(by_role("albumartist")),
        display_album_artist: Some(t.album_display_artist.clone()),
        contributors: Some(
            credits
                .iter()
                .filter(|c| c.role == "composer")
                .map(|c| Contributor {
                    role: c.role.clone(),
                    artist: artist_ref(c),
                })
                .collect(),
        ),
        moods: Some(names_of(x.track_tags.get(&t.id), "mood")),
        replay_gain: (rg != ReplayGain::default()).then_some(rg),
        bit_depth: t.bit_depth,
        sampling_rate: t.sample_rate,
        channel_count: t.channels,
        ..Default::default()
    }
}

pub async fn songs(v: View<'_>, tracks: &[TrackRow]) -> Result<Vec<Child>, ApiError> {
    let x = TrackExtras::load(v, tracks).await?;
    Ok(tracks.iter().map(|t| song(t, &x)).collect())
}

pub async fn albums_id3(v: View<'_>, albums: &[AlbumRow]) -> Result<Vec<AlbumId3>, ApiError> {
    let x = AlbumExtras::load(v, albums).await?;
    Ok(albums.iter().map(|a| album_id3(a, &x)).collect())
}

pub async fn album_children(v: View<'_>, albums: &[AlbumRow]) -> Result<Vec<Child>, ApiError> {
    let x = AlbumExtras::load(v, albums).await?;
    Ok(albums.iter().map(|a| album_child(a, &x)).collect())
}

pub async fn artists_folder(v: View<'_>, artists: &[ArtistRow]) -> Result<Vec<Artist>, ApiError> {
    let st = artist_states(v, artists).await?;
    Ok(artists.iter().map(|a| artist_folder(a, &st)).collect())
}

pub async fn artists_id3(v: View<'_>, artists: &[ArtistRow]) -> Result<Vec<ArtistId3>, ApiError> {
    let st = artist_states(v, artists).await?;
    Ok(artists.iter().map(|a| artist_id3(a, &st)).collect())
}
