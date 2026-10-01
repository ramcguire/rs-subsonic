//! Catalog endpoints, served from the local index. User fields and lists
//! ordered by user state come from the backend.

use rsub_api::model::{
    AlbumInfo, AlbumList, AlbumList2, ArtistInfo, ArtistsId3, Directory, Empty, Genre, Genres,
    Index, IndexId3, Indexes, MusicFolder, MusicFolders, ScanStatus, SearchResult2, SearchResult3,
    Songs, Starred, Starred2,
};
use rsub_api::{ApiError, Envelope, Payload};
use rsub_core::Kind;
use rsub_core::backend::StateOrder;
use rsub_core::text::{index_letter, search_terms};
use rsub_store::{AlbumOrder, AlbumRow, ArtistRow, Page, TrackRow};

use crate::convert::{self, AlbumExtras, TrackExtras};
use crate::db_error;
use crate::handlers::Ctx;
use crate::ids::{lookup, lookup_any};
use crate::state::{self, rows_in_order};

/// Largest page for list endpoints (the Subsonic limit for album lists).
const MAX_LIST: u64 = 500;
/// Largest page for search; clients use `search3` with an empty query to sync.
const MAX_SEARCH: u64 = 10_000;
/// Most starred items of each kind returned.
const MAX_STARRED: u64 = 10_000;

/// The row behind the required `id` of the given kind; malformed, wrong-kind or
/// unknown ids are "not found".
async fn entity(ctx: &Ctx<'_>, kind: Kind, what: &str) -> Result<i64, ApiError> {
    let raw = ctx.params.required("id")?;
    lookup(ctx, raw, kind)
        .await?
        .ok_or_else(|| ApiError::not_found(what))
}

fn page(ctx: &Ctx<'_>, size: &str, offset: &str, default: u64, max: u64) -> Result<Page, ApiError> {
    Ok(Page {
        limit: ctx.params.parse_or(size, default)?.min(max),
        offset: ctx.params.parse_or(offset, 0)?,
        library: ctx.folder()?,
    })
}

pub async fn music_folders(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let libs = ctx.state.db.libraries().await.map_err(db_error)?;
    Ok(Envelope::with(MusicFolders {
        music_folder: libs
            .into_iter()
            .map(|l| MusicFolder {
                id: l.id,
                name: l.name,
            })
            .collect(),
    }))
}

/// Artists bucketed by index letter, in sort order.
fn buckets(artists: Vec<ArtistRow>) -> Vec<(char, Vec<ArtistRow>)> {
    let mut out: Vec<(char, Vec<ArtistRow>)> = Vec::new();
    for a in artists {
        let letter = index_letter(&a.sort_key);
        match out.iter_mut().find(|(l, _)| *l == letter) {
            Some((_, v)) => v.push(a),
            None => out.push((letter, vec![a])),
        }
    }
    // `#` sorts last, as most clients expect.
    out.sort_by_key(|(l, _)| (*l == '#', *l));
    out
}

pub async fn indexes(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let db = &ctx.state.db;
    let last_modified = db.catalog_modified_at().await.map_err(db_error)?;
    let since: i64 = ctx.params.parse_or("ifModifiedSince", 0)?;
    let index = if since > 0 && last_modified <= since {
        Vec::new()
    } else {
        let rows = db.index_artists(ctx.folder()?).await.map_err(db_error)?;
        let st = convert::artist_states(ctx.view(), &rows).await?;
        buckets(rows)
            .into_iter()
            .map(|(l, v)| Index {
                name: l.to_string(),
                artist: v.iter().map(|a| convert::artist_folder(a, &st)).collect(),
            })
            .collect()
    };
    Ok(Envelope::with(Indexes {
        last_modified,
        ignored_articles: ctx.state.articles.list.clone(),
        index,
    }))
}

pub async fn artists(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let rows = ctx
        .state
        .db
        .index_artists(ctx.folder()?)
        .await
        .map_err(db_error)?;
    let st = convert::artist_states(ctx.view(), &rows).await?;
    Ok(Envelope::with(ArtistsId3 {
        ignored_articles: ctx.state.articles.list.clone(),
        index: buckets(rows)
            .into_iter()
            .map(|(l, v)| IndexId3 {
                name: l.to_string(),
                artist: v.iter().map(|a| convert::artist_id3(a, &st)).collect(),
            })
            .collect(),
    }))
}

pub async fn artist(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let id = entity(ctx, Kind::Artist, "Artist").await?;
    let db = &ctx.state.db;
    let row = db
        .artist(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("Artist"))?;
    let albums = db.albums_by_artist(id).await.map_err(db_error)?;
    let (st, albums) = tokio::try_join!(
        convert::artist_states(ctx.view(), std::slice::from_ref(&row)),
        convert::albums_id3(ctx.view(), &albums),
    )?;
    let mut a = convert::artist_id3(&row, &st);
    a.album = Some(albums);
    Ok(Envelope::with(a))
}

pub async fn album(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let id = entity(ctx, Kind::Album, "Album").await?;
    let db = &ctx.state.db;
    let row = db
        .album(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("Album"))?;
    let tracks = db.tracks_by_album(id).await.map_err(db_error)?;
    let (x, songs) = tokio::try_join!(
        AlbumExtras::load(ctx.view(), std::slice::from_ref(&row)),
        convert::songs(ctx.view(), &tracks),
    )?;
    let mut a = convert::album_id3(&row, &x);
    a.song = Some(songs);
    Ok(Envelope::with(a))
}

pub async fn song(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let id = entity(ctx, Kind::Track, "Song").await?;
    let db = &ctx.state.db;
    let t = db
        .track(id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("Song"))?;
    let x = TrackExtras::load(ctx.view(), std::slice::from_ref(&t)).await?;
    Ok(Envelope::with(convert::song(&t, &x)))
}

/// Folder-style browsing: artists contain albums, albums contain songs.
pub async fn music_directory(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let raw = ctx.params.required("id")?;
    let not_found = || ApiError::not_found("Directory");
    let (kind, id) = lookup_any(ctx, raw).await?.ok_or_else(not_found)?;
    let db = &ctx.state.db;
    match kind {
        Kind::Artist => {
            let a = db
                .artist(id)
                .await
                .map_err(db_error)?
                .ok_or_else(not_found)?;
            let albums = db.albums_by_artist(id).await.map_err(db_error)?;
            let (st, child) = tokio::try_join!(
                convert::artist_states(ctx.view(), std::slice::from_ref(&a)),
                convert::album_children(ctx.view(), &albums),
            )?;
            let marks = convert::artist_folder(&a, &st);
            Ok(Envelope::with(Directory {
                id: raw.to_owned(),
                parent: None,
                name: a.name,
                starred: marks.starred,
                user_rating: marks.user_rating,
                play_count: None,
                child,
            }))
        }
        Kind::Album => {
            let a = db
                .album(id)
                .await
                .map_err(db_error)?
                .ok_or_else(not_found)?;
            let tracks = db.tracks_by_album(id).await.map_err(db_error)?;
            let (x, child) = tokio::try_join!(
                AlbumExtras::load(ctx.view(), std::slice::from_ref(&a)),
                convert::songs(ctx.view(), &tracks),
            )?;
            let marks = convert::album_child(&a, &x);
            Ok(Envelope::with(Directory {
                id: raw.to_owned(),
                parent: a.artist_public_id.clone(),
                name: a.title,
                starred: marks.starred,
                user_rating: marks.user_rating,
                play_count: marks.play_count,
                child,
            }))
        }
        _ => Err(not_found()),
    }
}

/// A `getAlbumList(2)` type: served from the index, or from the user's state
/// in the backend.
enum ListType {
    Catalog(AlbumOrder),
    User(StateOrder),
}

fn album_order(ctx: &Ctx<'_>) -> Result<ListType, ApiError> {
    let p = ctx.params;
    Ok(ListType::Catalog(match p.required("type")? {
        "random" => AlbumOrder::Random,
        "newest" => AlbumOrder::Newest,
        "alphabeticalByName" => AlbumOrder::ByName,
        "alphabeticalByArtist" => AlbumOrder::ByArtist,
        "byYear" => AlbumOrder::ByYear {
            from: p.parse_required("fromYear")?,
            to: p.parse_required("toYear")?,
        },
        "byGenre" => AlbumOrder::ByGenre(p.required("genre")?.to_owned()),
        "frequent" => return Ok(ListType::User(StateOrder::Frequent)),
        "recent" => return Ok(ListType::User(StateOrder::Recent)),
        "starred" => return Ok(ListType::User(StateOrder::Starred)),
        "highest" => return Ok(ListType::User(StateOrder::Highest)),
        other => {
            return Err(ApiError::generic(format!(
                "Unknown album list type '{other}'."
            )));
        }
    }))
}

async fn album_rows(ctx: &Ctx<'_>) -> Result<Vec<AlbumRow>, ApiError> {
    let p = page(ctx, "size", "offset", 10, MAX_LIST)?;
    let db = &ctx.state.db;
    match album_order(ctx)? {
        ListType::Catalog(order) => db.album_list(&order, p).await.map_err(db_error),
        ListType::User(order) => {
            let r = state::ranked(ctx, Kind::Album, order, p.library, p.offset, p.limit).await?;
            rows_in_order::<AlbumRow>(db, &r).await
        }
    }
}

pub async fn album_list(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let rows = album_rows(ctx).await?;
    Ok(Envelope::with(AlbumList {
        album: convert::album_children(ctx.view(), &rows).await?,
    }))
}

pub async fn album_list2(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let rows = album_rows(ctx).await?;
    Ok(Envelope::with(AlbumList2 {
        album: convert::albums_id3(ctx.view(), &rows).await?,
    }))
}

pub async fn random_songs(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let p = ctx.params;
    let rows = ctx
        .state
        .db
        .random_tracks(
            p.get("genre").filter(|g| !g.is_empty()),
            (p.parse_opt("fromYear")?, p.parse_opt("toYear")?),
            Page::first(p.parse_or("size", 10u64)?.min(MAX_LIST)).in_library(ctx.folder()?),
        )
        .await
        .map_err(db_error)?;
    Ok(Envelope::with(Payload::RandomSongs(Songs {
        song: convert::songs(ctx.view(), &rows).await?,
    })))
}

pub async fn songs_by_genre(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let genre = ctx.params.required("genre")?;
    let rows = ctx
        .state
        .db
        .tracks_by_genre(genre, page(ctx, "count", "offset", 10, MAX_LIST)?)
        .await
        .map_err(db_error)?;
    Ok(Envelope::with(Payload::SongsByGenre(Songs {
        song: convert::songs(ctx.view(), &rows).await?,
    })))
}

pub async fn genres(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let rows = ctx.state.db.genres().await.map_err(db_error)?;
    Ok(Envelope::with(Genres {
        genre: rows
            .into_iter()
            .map(|g| Genre {
                song_count: g.song_count,
                album_count: g.album_count,
                value: g.name,
            })
            .collect(),
    }))
}

/// `search2` and `search3`: the rows matching `query`, each kind paged.
async fn search_rows(
    ctx: &Ctx<'_>,
) -> Result<(Vec<ArtistRow>, Vec<AlbumRow>, Vec<TrackRow>), ApiError> {
    let terms = search_terms(ctx.params.get("query").unwrap_or(""));
    let artists = page(ctx, "artistCount", "artistOffset", 20, MAX_SEARCH)?;
    let albums = page(ctx, "albumCount", "albumOffset", 20, MAX_SEARCH)?;
    let songs = page(ctx, "songCount", "songOffset", 20, MAX_SEARCH)?;
    let db = &ctx.state.db;
    tokio::try_join!(
        db.search_artists(&terms, artists),
        db.search_albums(&terms, albums),
        db.search_tracks(&terms, songs),
    )
    .map_err(db_error)
}

pub async fn search2(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let (artists, albums, songs) = search_rows(ctx).await?;
    let v = ctx.view();
    let (artist, album, song) = tokio::try_join!(
        convert::artists_folder(v, &artists),
        convert::album_children(v, &albums),
        convert::songs(v, &songs),
    )?;
    Ok(Envelope::with(SearchResult2 {
        artist,
        album,
        song,
    }))
}

pub async fn search3(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let (artists, albums, songs) = search_rows(ctx).await?;
    let v = ctx.view();
    let (artist, album, song) = tokio::try_join!(
        convert::artists_id3(v, &artists),
        convert::albums_id3(v, &albums),
        convert::songs(v, &songs),
    )?;
    Ok(Envelope::with(SearchResult3 {
        artist,
        album,
        song,
    }))
}

pub async fn artist_info(ctx: &Ctx<'_>, v2: bool) -> Result<Envelope, ApiError> {
    // Folder-style clients may pass album or song ids; only artists have a bio.
    let id = ctx.params.required("id")?;
    let row = match lookup(ctx, id, Kind::Artist).await? {
        Some(id) => ctx.state.db.artist(id).await.map_err(db_error)?,
        None => None,
    };
    let similar_artist = match &row {
        Some(a) => {
            let n = ctx.params.parse_or("count", 20usize)?;
            crate::discover::similar_artists_id3(ctx, a, n).await?
        }
        None => Vec::new(),
    };
    let info = ArtistInfo {
        biography: row.as_ref().and_then(|a| a.summary.clone()),
        music_brainz_id: row.and_then(|a| a.mbid),
        similar_artist,
    };
    Ok(Envelope::with(if v2 {
        Payload::ArtistInfo2(info)
    } else {
        Payload::ArtistInfo(info)
    }))
}

pub async fn album_info(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let id = ctx.params.required("id")?;
    let row = match lookup(ctx, id, Kind::Album).await? {
        Some(id) => ctx.state.db.album(id).await.map_err(db_error)?,
        None => None,
    };
    Ok(Envelope::with(AlbumInfo {
        notes: None,
        music_brainz_id: row.and_then(|a| a.mbid),
    }))
}

pub async fn scan_status(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    let (scanning, count) = match &ctx.state.sync {
        Some(s) if s.status().scanning() => (true, s.status().count() as i64),
        _ => (false, ctx.state.db.track_count().await.map_err(db_error)?),
    };
    Ok(Envelope::with(ScanStatus { scanning, count }))
}

pub async fn start_scan(ctx: &Ctx<'_>) -> Result<Envelope, ApiError> {
    if !ctx.user.is_admin() {
        return Err(ApiError::not_authorized());
    }
    if let Some(s) = &ctx.state.sync {
        s.trigger();
    }
    // Report `scanning` even if the sync task has not picked the trigger up yet.
    let count = ctx.state.db.track_count().await.map_err(db_error)?;
    Ok(Envelope::with(ScanStatus {
        scanning: ctx.state.sync.is_some(),
        count,
    }))
}

/// `getStarred` (folder style) and `getStarred2` (ID3): items rated 5, newest
/// rating first, from the backend; starred virtual artists come last.
pub async fn starred(ctx: &Ctx<'_>, v2: bool) -> Result<Envelope, ApiError> {
    let db = &ctx.state.db;
    let lib = ctx.folder()?;
    let starred = |kind| state::ranked(ctx, kind, StateOrder::Starred, lib, 0, MAX_STARRED);
    let (artists, albums, songs, local) = tokio::try_join!(
        starred(Kind::Artist),
        starred(Kind::Album),
        starred(Kind::Track),
        async { db.locally_starred(ctx.user.id, lib).await.map_err(db_error) },
    )?;
    let (mut artists, albums, songs) = tokio::try_join!(
        rows_in_order::<ArtistRow>(db, &artists),
        rows_in_order::<AlbumRow>(db, &albums),
        rows_in_order::<TrackRow>(db, &songs),
    )?;
    artists.extend(local);
    let v = ctx.view();
    Ok(if v2 {
        let (artist, album, song) = tokio::try_join!(
            convert::artists_id3(v, &artists),
            convert::albums_id3(v, &albums),
            convert::songs(v, &songs),
        )?;
        Envelope::with(Starred2 {
            artist,
            album,
            song,
        })
    } else {
        let (artist, album, song) = tokio::try_join!(
            convert::artists_folder(v, &artists),
            convert::album_children(v, &albums),
            convert::songs(v, &songs),
        )?;
        Envelope::with(Starred {
            artist,
            album,
            song,
        })
    })
}

/// The row behind an id that must name an artist, album or song.
pub async fn item(ctx: &Ctx<'_>, raw: &str) -> Result<(Kind, i64), ApiError> {
    lookup_any(ctx, raw)
        .await?
        .filter(|(kind, _)| matches!(kind, Kind::Artist | Kind::Album | Kind::Track))
        .ok_or_else(|| ApiError::not_found("Item"))
}

/// Endpoints whose features arrive in later milestones: valid, empty responses.
pub fn empty(name: &str) -> Envelope {
    match name {
        "getBookmarks" => Envelope::with(Payload::Bookmarks(Empty {})),
        "getInternetRadioStations" => Envelope::with(Payload::InternetRadioStations(Empty {})),
        "getPodcasts" => Envelope::with(Payload::Podcasts(Empty {})),
        "getNewestPodcasts" => Envelope::with(Payload::NewestPodcasts(Empty {})),
        "getShares" => Envelope::with(Payload::Shares(Empty {})),
        // No saved queue: an ok response without a `playQueue` element. Saves
        // and bookmark changes are accepted and dropped.
        _ => Envelope::ok(),
    }
}
