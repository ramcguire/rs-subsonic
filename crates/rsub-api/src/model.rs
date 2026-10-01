//! Subsonic / OpenSubsonic response objects for the catalog endpoints.
//!
//! Field order follows the Subsonic XSD; OpenSubsonic additions come last.
//! `Option` fields are omitted when `None`; OpenSubsonic arrays are always
//! present in JSON (empty arrays render as nothing in XML).

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MusicFolders {
    pub music_folder: Vec<MusicFolder>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct MusicFolder {
    pub id: i64,
    pub name: String,
}

/// `getIndexes`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Indexes {
    pub last_modified: i64,
    pub ignored_articles: String,
    pub index: Vec<Index>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Index {
    pub name: String,
    pub artist: Vec<Artist>,
}

/// Folder-style artist (`getIndexes`, `search2`, `getStarred`).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_count: Option<i64>,
}

/// `getArtists`.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistsId3 {
    pub ignored_articles: String,
    pub index: Vec<IndexId3>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IndexId3 {
    pub name: String,
    pub artist: Vec<ArtistId3>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistId3 {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    pub album_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    // OpenSubsonic
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_brainz_id: Option<String>,
    pub sort_name: String,
    pub roles: Vec<String>,
    /// Present for `getArtist` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<Vec<AlbumId3>>,
}

/// Minimal artist reference used in OpenSubsonic `artists`/`albumArtists`.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ArtistRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Contributor {
    pub role: String,
    pub artist: ArtistRef,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ItemGenre {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecordLabel {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ItemDate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub month: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<u32>,
}

impl ItemDate {
    /// Parse `YYYY`, `YYYY-MM` or `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<ItemDate> {
        let mut it = s.trim().splitn(3, '-');
        let year = it.next()?.parse().ok()?;
        let month = it
            .next()
            .and_then(|m| m.parse().ok())
            .filter(|m| (1..=12).contains(m));
        let day = it
            .next()
            .and_then(|d| d.get(..2).unwrap_or(d).parse().ok())
            .filter(|d| (1..=31).contains(d));
        Some(ItemDate {
            year: Some(year),
            month,
            day: month.and(day),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReplayGain {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_peak: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_peak: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumId3 {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    pub song_count: i64,
    pub duration: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_count: Option<i64>,
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    // OpenSubsonic
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<i64>,
    pub record_labels: Vec<RecordLabel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_brainz_id: Option<String>,
    pub genres: Vec<ItemGenre>,
    pub artists: Vec<ArtistRef>,
    pub display_artist: String,
    pub release_types: Vec<String>,
    pub moods: Vec<String>,
    pub sort_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_release_date: Option<ItemDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_date: Option<ItemDate>,
    pub is_compilation: bool,
    /// Present for `getAlbum` only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song: Option<Vec<Child>>,
}

/// A song, or a folder-style directory entry (`isDir`).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Child {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub is_dir: bool,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_video: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    // OpenSubsonic (songs only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bpm: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_brainz_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<ItemGenre>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artists: Option<Vec<ArtistRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artists: Option<Vec<ArtistRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contributors: Option<Vec<Contributor>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moods: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_gain: Option<ReplayGain>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_rate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_count: Option<i64>,
    // `getNowPlaying` entries only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minutes_ago: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Directory {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starred: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_rating: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_count: Option<i64>,
    pub child: Vec<Child>,
}

/// `getAlbumList` (folder-style albums).
#[derive(Debug, Clone, Default, Serialize)]
pub struct AlbumList {
    pub album: Vec<Child>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AlbumList2 {
    pub album: Vec<AlbumId3>,
}

/// `randomSongs`, `songsByGenre`, `similarSongs(2)`, `topSongs`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Songs {
    pub song: Vec<Child>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Genres {
    pub genre: Vec<Genre>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Genre {
    pub song_count: i64,
    pub album_count: i64,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchResult2 {
    pub artist: Vec<Artist>,
    pub album: Vec<Child>,
    pub song: Vec<Child>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchResult3 {
    pub artist: Vec<ArtistId3>,
    pub album: Vec<AlbumId3>,
    pub song: Vec<Child>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Starred {
    pub artist: Vec<Artist>,
    pub album: Vec<Child>,
    pub song: Vec<Child>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Starred2 {
    pub artist: Vec<ArtistId3>,
    pub album: Vec<AlbumId3>,
    pub song: Vec<Child>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatus {
    pub scanning: bool,
    pub count: i64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub biography: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_brainz_id: Option<String>,
    pub similar_artist: Vec<ArtistId3>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub music_brainz_id: Option<String>,
}

/// `getLyrics`: the plain text of a song's lyrics, or nothing without a match.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Lyrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// OpenSubsonic `sonicSimilarity` extension (`getSonicSimilarTracks`,
/// `findSonicPath`): a song and its similarity to the query or start song, 1
/// for the same sound and towards 0 for the most different.
#[derive(Debug, Clone, Serialize)]
pub struct SonicMatch {
    pub entry: Child,
    pub similarity: f32,
}

/// OpenSubsonic `getLyricsBySongId` (`songLyrics` extension): every lyrics
/// document of a song, empty when it has none.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LyricsList {
    pub structured_lyrics: Vec<StructuredLyrics>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredLyrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    /// ISO 639 language, or `und` when unknown.
    pub lang: String,
    /// Milliseconds to shift every line's start by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    pub synced: bool,
    pub line: Vec<LyricLine>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LyricLine {
    /// Milliseconds from the start of the song; synced lyrics only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Playlists {
    pub playlist: Vec<Playlist>,
}

/// A playlist; `entry` is present for `getPlaylist`/`createPlaylist` only.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub owner: String,
    pub public: bool,
    pub song_count: i64,
    pub duration: i64,
    pub created: String,
    pub changed: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_art: Option<String>,
    // OpenSubsonic
    pub readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<Vec<Child>>,
}

/// `getNowPlaying`: songs with the `username`, `minutesAgo`, `playerId` and
/// `playerName` fields set.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NowPlaying {
    pub entry: Vec<Child>,
}

/// An empty list element (`bookmarks`, `podcasts`, …) for features that
/// arrive in later milestones.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Empty {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_dates() {
        let d = |y, m, dd| {
            Some(ItemDate {
                year: Some(y),
                month: m,
                day: dd,
            })
        };
        assert_eq!(ItemDate::parse("1969-09-26"), d(1969, Some(9), Some(26)));
        assert_eq!(
            ItemDate::parse("1969-09-26T00:00:00Z"),
            d(1969, Some(9), Some(26))
        );
        assert_eq!(ItemDate::parse("1997"), d(1997, None, None));
        assert_eq!(ItemDate::parse("1997-13"), d(1997, None, None));
        assert_eq!(ItemDate::parse("x"), None);
    }
}
