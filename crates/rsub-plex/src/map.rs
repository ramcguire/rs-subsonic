//! Plex metadata -> backend records.

use rsub_core::backend::{
    AlbumRecord, ArtistRecord, CreditRecord, CreditRole, LyricLine, LyricsDoc, PlaylistEntry,
    RemotePlaylist, RemoteRef, RemoteState, ReplayGain, TagKind, TrackEnrichment, TrackFile,
    TrackRecord,
};

use crate::model::{GuidTag, Lyrics, Metadata, STREAM_AUDIO, STREAM_LYRICS, Tag};

/// Plex times are epoch seconds.
fn ms(secs: Option<i64>) -> i64 {
    secs.unwrap_or(0).saturating_mul(1000)
}

fn mbid(guids: &[GuidTag]) -> Option<String> {
    guids
        .iter()
        .find_map(|g| g.id.strip_prefix("mbid://").map(str::to_owned))
}

fn tags(m: &Metadata) -> Vec<(TagKind, String)> {
    let each = |kind: TagKind, list: &[Tag]| {
        list.iter()
            .filter(|t| !t.tag.is_empty())
            .map(move |t| (kind, t.tag.clone()))
            .collect::<Vec<_>>()
    };
    let mut out = each(TagKind::Genre, &m.genre);
    out.extend(each(TagKind::Style, &m.style));
    out.extend(each(TagKind::Mood, &m.mood));
    out
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

pub fn artist(m: Metadata) -> ArtistRecord {
    ArtistRecord {
        tags: tags(&m),
        mbid: mbid(&m.guids),
        updated_at: ms(m.updated_at.or(m.added_at)),
        remote: RemoteRef {
            key: m.rating_key,
            guid: m.guid,
        },
        name: m.title,
        sort_name: nonempty(m.title_sort),
        summary: nonempty(m.summary),
        thumb: nonempty(m.thumb),
    }
}

pub fn album(m: Metadata) -> AlbumRecord {
    let release_types: Vec<String> = m
        .format
        .iter()
        .chain(&m.subformat)
        .map(|t| t.tag.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let display_artist = m.parent_title.clone().unwrap_or_default();
    AlbumRecord {
        tags: tags(&m),
        mbid: mbid(&m.guids),
        is_compilation: release_types.iter().any(|t| t == "compilation")
            || display_artist.eq_ignore_ascii_case("Various Artists"),
        release_types,
        remote: RemoteRef {
            key: m.rating_key,
            guid: m.guid,
        },
        artist: m.parent_rating_key.map(|key| RemoteRef {
            key,
            guid: m.parent_guid,
        }),
        display_artist,
        title: m.title,
        sort_title: nonempty(m.title_sort),
        year: m.year,
        release_date: nonempty(m.originally_available_at),
        original_release_date: None,
        label: nonempty(m.studio),
        thumb: nonempty(m.thumb),
        added_at: ms(m.added_at),
        updated_at: ms(m.updated_at.or(m.added_at)),
    }
}

/// `None` for tracks without a playable part.
pub fn track(m: Metadata) -> Option<TrackRecord> {
    let media = m.media.first()?;
    let part = media.part.first()?;
    let part_key = part.key.clone()?;

    let album_artist = m.grandparent_rating_key.clone().map(|key| RemoteRef {
        key,
        guid: m.grandparent_guid.clone(),
    });
    let album_artist_name = m.grandparent_title.clone().unwrap_or_default();
    let track_artist = nonempty(m.original_title.clone());
    let mut credits = Vec::with_capacity(2);
    match &track_artist {
        Some(name) => credits.push(CreditRecord {
            remote: None,
            name: name.clone(),
            role: CreditRole::Artist,
        }),
        None => credits.push(CreditRecord {
            remote: album_artist.clone(),
            name: album_artist_name.clone(),
            role: CreditRole::Artist,
        }),
    }
    credits.push(CreditRecord {
        remote: album_artist,
        name: album_artist_name.clone(),
        role: CreditRole::AlbumArtist,
    });

    Some(TrackRecord {
        album: RemoteRef {
            key: m.parent_rating_key.clone()?,
            guid: m.parent_guid.clone(),
        },
        display_artist: track_artist.unwrap_or(album_artist_name),
        credits,
        track_no: m.index,
        disc_no: m.parent_index,
        year: m.parent_year.or(m.year),
        duration_ms: m.duration.unwrap_or(0),
        part_key,
        remote_path: part.file.clone(),
        size: part.size,
        codec: media.audio_codec.clone(),
        container: part.container.clone().or_else(|| media.container.clone()),
        bitrate_kbps: media.bitrate,
        sample_rate: None,
        bit_depth: None,
        channels: media.audio_channels,
        popularity: m.rating_count,
        mbid: mbid(&m.guids),
        added_at: ms(m.added_at),
        updated_at: ms(m.updated_at.or(m.added_at)),
        remote: RemoteRef {
            key: m.rating_key,
            guid: m.guid,
        },
        title: m.title,
        sort_title: nonempty(m.title_sort),
    })
}

/// The first part's file, as [`track`] uses it; `None` without one.
pub fn track_file(m: Metadata) -> Option<TrackFile> {
    let remote_path = m.media.first()?.part.first()?.file.clone()?;
    Some(TrackFile {
        key: m.rating_key,
        album_key: m.parent_rating_key?,
        artist_key: m.grandparent_rating_key,
        remote_path,
    })
}

pub fn enrichment(m: Metadata) -> TrackEnrichment {
    let streams = m
        .media
        .iter()
        .flat_map(|md| &md.part)
        .flat_map(|p| &p.stream);
    let mut e = TrackEnrichment {
        tags: tags(&m),
        key: m.rating_key.clone(),
        ..Default::default()
    };
    for s in streams {
        match s.stream_type {
            Some(STREAM_AUDIO) if e.sample_rate.is_none() => {
                e.bit_depth = s.bit_depth;
                e.sample_rate = s.sampling_rate;
                e.replay_gain = ReplayGain {
                    track_gain: s.gain,
                    track_peak: s.peak,
                    album_gain: s.album_gain,
                    album_peak: s.album_peak,
                };
            }
            Some(STREAM_LYRICS) => e.has_lyrics = true,
            _ => {}
        }
    }
    e
}

/// Plex rates 0–10; half stars round down, so only 10 reads as 5 (a star).
/// Anything below one star is unrated.
pub fn state(m: Metadata) -> RemoteState {
    let rating = m
        .user_rating
        .map(|r| (r / 2.0).floor().clamp(0.0, 5.0) as u8)
        .filter(|r| *r > 0);
    RemoteState {
        key: m.rating_key,
        rated_at: rating.and(m.last_rated_at).map(|s| ms(Some(s))),
        rating,
        play_count: m.view_count.filter(|n| *n > 0),
        last_played_at: m.last_viewed_at.map(|s| ms(Some(s))),
    }
}

pub fn playlist(m: Metadata) -> RemotePlaylist {
    RemotePlaylist {
        id: m.rating_key,
        name: m.title,
        comment: nonempty(m.summary),
        smart: m.smart.unwrap_or(false),
        song_count: m.leaf_count.unwrap_or(0),
        duration_ms: m.duration.unwrap_or(0),
        created_at: ms(m.added_at),
        updated_at: ms(m.updated_at.or(m.added_at)),
        thumb: nonempty(m.composite),
    }
}

/// Items of smart playlists have no `playlistItemID`; they can't be edited,
/// so the track key stands in for it.
/// Plex doesn't say which language lyrics are in; that is left to the caller.
pub fn lyrics(l: Lyrics) -> LyricsDoc {
    LyricsDoc {
        lang: String::new(),
        synced: l.timed,
        display_artist: None,
        display_title: None,
        offset_ms: 0,
        lines: l
            .lines
            .into_iter()
            .map(|line| LyricLine {
                start_ms: line.start_offset.filter(|_| l.timed),
                text: line.spans.into_iter().map(|s| s.text).collect(),
            })
            .collect(),
    }
}

pub fn playlist_entry(m: Metadata) -> PlaylistEntry {
    PlaylistEntry {
        id: m.playlist_item_id.unwrap_or_else(|| m.rating_key.clone()),
        key: m.rating_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_playlist_items_have_no_entry_id() {
        let m: Metadata =
            serde_json::from_str(r#"{"ratingKey":"23785","type":"track","title":"x"}"#).unwrap();
        assert_eq!(playlist_entry(m).id, "23785");
    }
}
