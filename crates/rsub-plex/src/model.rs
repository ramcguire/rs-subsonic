//! Plex Media Server JSON (`Accept: application/json`) as far as we use it.
//! Everything is optional/defaulted: Plex omits empty fields, and numeric fields
//! are accepted as either numbers or strings since versions differ.

use std::str::FromStr;

use serde::{Deserialize, Deserializer};

#[derive(Debug, Deserialize)]
pub struct Envelope<T> {
    #[serde(rename = "MediaContainer")]
    pub container: T,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Identity {
    pub machine_identifier: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Sections {
    #[serde(rename = "Directory")]
    pub directory: Vec<Section>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Section {
    #[serde(deserialize_with = "string")]
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    #[serde(rename = "Location")]
    pub location: Vec<Location>,
    /// A counter Plex advances when the section's content changes.
    #[serde(deserialize_with = "opt_string")]
    pub content_changed_at: Option<String>,
    /// A scan of the section is running.
    #[serde(deserialize_with = "flag")]
    pub refreshing: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Location {
    pub path: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MetadataPage {
    #[serde(deserialize_with = "lenient")]
    pub total_size: Option<u64>,
    #[serde(deserialize_with = "lenient")]
    pub size: Option<u64>,
    #[serde(rename = "Metadata")]
    pub metadata: Vec<Metadata>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Metadata {
    #[serde(deserialize_with = "string")]
    pub rating_key: String,
    pub guid: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub title_sort: Option<String>,
    pub summary: Option<String>,
    pub thumb: Option<String>,
    #[serde(deserialize_with = "opt_string")]
    pub parent_rating_key: Option<String>,
    pub parent_guid: Option<String>,
    pub parent_title: Option<String>,
    pub parent_thumb: Option<String>,
    #[serde(deserialize_with = "opt_string")]
    pub grandparent_rating_key: Option<String>,
    pub grandparent_guid: Option<String>,
    pub grandparent_title: Option<String>,
    /// Track artist when it differs from the album artist.
    pub original_title: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub index: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub parent_index: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub year: Option<i32>,
    #[serde(deserialize_with = "lenient")]
    pub parent_year: Option<i32>,
    pub originally_available_at: Option<String>,
    pub studio: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub duration: Option<u64>,
    /// Epoch seconds, like every Plex time (`*At`); `map::ms` turns them into
    /// the epoch milliseconds the rest of rs-subsonic uses.
    #[serde(deserialize_with = "lenient")]
    pub added_at: Option<i64>,
    #[serde(deserialize_with = "lenient")]
    pub updated_at: Option<i64>,
    #[serde(deserialize_with = "lenient")]
    pub rating_count: Option<u32>,
    /// The user's rating, 0–10 (half stars are odd numbers).
    #[serde(deserialize_with = "lenient")]
    pub user_rating: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub last_rated_at: Option<i64>,
    #[serde(deserialize_with = "lenient")]
    pub view_count: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub last_viewed_at: Option<i64>,
    /// Playlists: item count.
    #[serde(deserialize_with = "lenient")]
    pub leaf_count: Option<u64>,
    pub smart: Option<bool>,
    /// Playlists: artwork made from the first items' covers.
    pub composite: Option<String>,
    /// Playlist items: the entry's id within the playlist.
    #[serde(rename = "playlistItemID", deserialize_with = "opt_string")]
    pub playlist_item_id: Option<String>,
    #[serde(rename = "Genre")]
    pub genre: Vec<Tag>,
    #[serde(rename = "Mood")]
    pub mood: Vec<Tag>,
    #[serde(rename = "Style")]
    pub style: Vec<Tag>,
    #[serde(rename = "Format")]
    pub format: Vec<Tag>,
    #[serde(rename = "Subformat")]
    pub subformat: Vec<Tag>,
    #[serde(rename = "Guid")]
    pub guids: Vec<GuidTag>,
    #[serde(rename = "Media")]
    pub media: Vec<Media>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Tag {
    pub tag: String,
}

/// `/library/sections/{id}/style?type=8`: the values of one tag field in a
/// library, each with the key that filters by it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TagDirectories {
    #[serde(rename = "Directory")]
    pub directory: Vec<TagDirectory>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TagDirectory {
    #[serde(deserialize_with = "string")]
    pub key: String,
    pub title: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct GuidTag {
    pub id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Media {
    #[serde(deserialize_with = "lenient")]
    pub bitrate: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub audio_channels: Option<u32>,
    pub audio_codec: Option<String>,
    pub container: Option<String>,
    #[serde(rename = "Part")]
    pub part: Vec<Part>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Part {
    pub key: Option<String>,
    pub file: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub size: Option<u64>,
    pub container: Option<String>,
    #[serde(rename = "Stream")]
    pub stream: Vec<Stream>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Stream {
    #[serde(deserialize_with = "lenient")]
    pub stream_type: Option<u32>,
    pub codec: Option<String>,
    pub key: Option<String>,
    #[serde(deserialize_with = "lenient")]
    pub bit_depth: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub sampling_rate: Option<u32>,
    #[serde(deserialize_with = "lenient")]
    pub gain: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub album_gain: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub peak: Option<f32>,
    #[serde(deserialize_with = "lenient")]
    pub album_peak: Option<f32>,
}

pub const STREAM_AUDIO: u32 = 2;
pub const STREAM_LYRICS: u32 = 4;

/// A lyrics stream (`/library/streams/{id}`) as JSON. Plex also serves the
/// same stream as raw LRC or text without `Accept: application/json`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LyricsPage {
    #[serde(rename = "Lyrics")]
    pub lyrics: Vec<Lyrics>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Lyrics {
    /// Lines carry start times.
    #[serde(deserialize_with = "flag")]
    pub timed: bool,
    #[serde(rename = "Line")]
    pub lines: Vec<LyricLine>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LyricLine {
    /// Milliseconds from the start of the track.
    #[serde(deserialize_with = "lenient")]
    pub start_offset: Option<u64>,
    #[serde(rename = "Span")]
    pub spans: Vec<LyricSpan>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LyricSpan {
    pub text: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
}

impl NumOrStr {
    fn into_string(self) -> String {
        match self {
            NumOrStr::Int(i) => i.to_string(),
            NumOrStr::Float(f) => f.to_string(),
            NumOrStr::Str(s) => s,
            NumOrStr::Bool(b) => b.to_string(),
        }
    }
}

/// A number given as a JSON number or string; unparsable values become `None`.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: FromStr,
{
    Ok(Option::<NumOrStr>::deserialize(d)?.and_then(|v| {
        let s = match v {
            // Integral floats (e.g. `1.0` for a u32) parse as integers.
            NumOrStr::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => (f as i64).to_string(),
            other => other.into_string(),
        };
        s.trim().parse().ok()
    }))
}

/// A boolean sent as `true`, `1` or `"1"`.
fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::Bool(b)) => b,
        Some(NumOrStr::Int(i)) => i != 0,
        Some(NumOrStr::Str(s)) => matches!(s.trim(), "1" | "true"),
        _ => false,
    })
}

fn string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    NumOrStr::deserialize(d).map(NumOrStr::into_string)
}

fn opt_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<NumOrStr>::deserialize(d)?.map(NumOrStr::into_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_numbers() {
        let m: Metadata = serde_json::from_str(
            r#"{"ratingKey":123,"parentRatingKey":"45","index":"3","year":1999,
                "duration":1.8e5,"type":"track","title":"x","Media":[{"Part":[{"Stream":[
                {"streamType":2,"gain":"-7.5","peak":0.98}]}]}]}"#,
        )
        .unwrap();
        assert_eq!(m.rating_key, "123");
        assert_eq!(m.parent_rating_key.as_deref(), Some("45"));
        assert_eq!(m.index, Some(3));
        assert_eq!(m.year, Some(1999));
        assert_eq!(m.duration, Some(180_000));
        let s = &m.media[0].part[0].stream[0];
        assert_eq!(s.gain, Some(-7.5));
        assert_eq!(s.peak, Some(0.98));
    }
}
