//! Response envelope and payload types.

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use crate::error::ApiError;
use crate::model as m;

/// Subsonic REST API version we implement.
pub const API_VERSION: &str = "1.16.1";
/// OpenSubsonic `type` field.
pub const SERVER_TYPE: &str = "rs-subsonic";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const XMLNS: &str = "http://subsonic.org/restapi";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Failed,
}

/// The `subsonic-response` element / object.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub status: Status,
    pub error: Option<ApiError>,
    pub payload: Option<Payload>,
}

impl Envelope {
    /// An empty `status="ok"` response (e.g. `ping`).
    pub fn ok() -> Self {
        Envelope {
            status: Status::Ok,
            error: None,
            payload: None,
        }
    }

    pub fn with(payload: impl Into<Payload>) -> Self {
        Envelope {
            status: Status::Ok,
            error: None,
            payload: Some(payload.into()),
        }
    }

    pub fn error(err: ApiError) -> Self {
        Envelope {
            status: Status::Failed,
            error: Some(err),
            payload: None,
        }
    }
}

impl From<Result<Envelope, ApiError>> for Envelope {
    fn from(r: Result<Envelope, ApiError>) -> Self {
        r.unwrap_or_else(Envelope::error)
    }
}

impl Serialize for Envelope {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("subsonic-response", 7)?;
        st.serialize_field("status", &self.status)?;
        st.serialize_field("version", API_VERSION)?;
        st.serialize_field("type", SERVER_TYPE)?;
        st.serialize_field("serverVersion", SERVER_VERSION)?;
        st.serialize_field("openSubsonic", &true)?;
        if let Some(e) = &self.error {
            st.serialize_field("error", e)?;
        }
        if let Some(p) = &self.payload {
            p.serialize_field(&mut st)?;
        }
        st.end()
    }
}

macro_rules! payloads {
    (
        from { $($variant:ident($ty:ty) => $name:literal),* $(,)? }
        plain { $($pvariant:ident($pty:ty) => $pname:literal),* $(,)? }
    ) => {
        /// One variant per Subsonic response element; serialized as a field of the
        /// envelope named after the element. Variants whose type is shared with
        /// another variant have no `From` impl and are constructed explicitly.
        // One payload per response, never stored in bulk: boxing the large
        // variants would only add an allocation.
        #[allow(clippy::large_enum_variant)]
        #[derive(Debug, Clone)]
        pub enum Payload {
            $($variant($ty),)*
            $($pvariant($pty),)*
        }

        impl Payload {
            fn serialize_field<S: SerializeStruct>(&self, st: &mut S) -> Result<(), S::Error> {
                match self {
                    $(Payload::$variant(v) => st.serialize_field($name, v),)*
                    $(Payload::$pvariant(v) => st.serialize_field($pname, v),)*
                }
            }
        }

        $(impl From<$ty> for Payload {
            fn from(v: $ty) -> Self { Payload::$variant(v) }
        })*
    };
}

payloads! {
    from {
    License(License) => "license",
    OpenSubsonicExtensions(Vec<OpenSubsonicExtension>) => "openSubsonicExtensions",
    User(User) => "user",
    Users(Users) => "users",
    TokenInfo(TokenInfo) => "tokenInfo",
    MusicFolders(m::MusicFolders) => "musicFolders",
    Indexes(m::Indexes) => "indexes",
    Artists(m::ArtistsId3) => "artists",
    Artist(m::ArtistId3) => "artist",
    Album(m::AlbumId3) => "album",
    Song(m::Child) => "song",
    Directory(m::Directory) => "directory",
    AlbumList(m::AlbumList) => "albumList",
    AlbumList2(m::AlbumList2) => "albumList2",
    Genres(m::Genres) => "genres",
    SearchResult2(m::SearchResult2) => "searchResult2",
    SearchResult3(m::SearchResult3) => "searchResult3",
    Starred(m::Starred) => "starred",
    Starred2(m::Starred2) => "starred2",
    ScanStatus(m::ScanStatus) => "scanStatus",
    AlbumInfo(m::AlbumInfo) => "albumInfo",
    Lyrics(m::Lyrics) => "lyrics",
    LyricsList(m::LyricsList) => "lyricsList",
    SonicMatches(Vec<m::SonicMatch>) => "sonicMatch",
    Playlists(m::Playlists) => "playlists",
    Playlist(m::Playlist) => "playlist",
    NowPlaying(m::NowPlaying) => "nowPlaying",
    }
    plain {
    ArtistInfo(m::ArtistInfo) => "artistInfo",
    ArtistInfo2(m::ArtistInfo) => "artistInfo2",
    RandomSongs(m::Songs) => "randomSongs",
    SongsByGenre(m::Songs) => "songsByGenre",
    SimilarSongs(m::Songs) => "similarSongs",
    SimilarSongs2(m::Songs) => "similarSongs2",
    TopSongs(m::Songs) => "topSongs",
    Bookmarks(m::Empty) => "bookmarks",
    InternetRadioStations(m::Empty) => "internetRadioStations",
    Podcasts(m::Empty) => "podcasts",
    NewestPodcasts(m::Empty) => "newestPodcasts",
    Shares(m::Empty) => "shares",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct License {
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license_expires: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_expires: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenSubsonicExtension {
    pub name: &'static str,
    pub versions: Vec<u32>,
}

/// Whose API key authenticated the request (OpenSubsonic `tokenInfo`).
#[derive(Debug, Clone, Serialize)]
pub struct TokenInfo {
    pub username: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub scrobbling_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bit_rate: Option<u32>,
    pub admin_role: bool,
    pub settings_role: bool,
    pub download_role: bool,
    pub upload_role: bool,
    pub playlist_role: bool,
    pub cover_art_role: bool,
    pub comment_role: bool,
    pub podcast_role: bool,
    pub stream_role: bool,
    pub jukebox_role: bool,
    pub share_role: bool,
    pub video_conversion_role: bool,
    /// Music folder ids the user may access.
    pub folder: Vec<i64>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Users {
    pub user: Vec<User>,
}
