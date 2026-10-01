//! Tag reading from the mounted library with lofty.

use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};
use rsub_core::tags::{FileStamp, FileTags, TagReader, extract_mbids};

use crate::PathMapper;

/// Reads tags of backend paths through a [`PathMapper`].
pub struct LocalTagReader {
    paths: PathMapper,
}

impl LocalTagReader {
    pub fn new(paths: PathMapper) -> Self {
        LocalTagReader { paths }
    }

    fn local(&self, remote_path: &str) -> io::Result<std::path::PathBuf> {
        self.paths
            .map(remote_path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no path_map matches"))
    }
}

#[async_trait]
impl TagReader for LocalTagReader {
    async fn stat(&self, remote_path: &str) -> io::Result<FileStamp> {
        let meta = tokio::fs::metadata(self.local(remote_path)?).await?;
        if !meta.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
        }
        let mtime_ms = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        Ok(FileStamp {
            size: meta.len(),
            mtime_ms,
        })
    }

    async fn read(&self, remote_path: &str) -> io::Result<FileTags> {
        let path = self.local(remote_path)?;
        tokio::task::spawn_blocking(move || read_file_tags(&path))
            .await
            .map_err(io::Error::other)?
    }
}

/// Read the identity-relevant tags of one file (blocking). Fields are taken
/// from the primary tag first, then from any other tag in the file.
///
/// A file that can't be parsed fails with [`io::ErrorKind::InvalidData`];
/// any other error is an I/O failure that may be transient.
pub fn read_file_tags(path: &Path) -> io::Result<FileTags> {
    let opts = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let file = Probe::open(path)
        .map_err(parse_error)?
        .options(opts)
        .guess_file_type()?
        .read()
        .map_err(parse_error)?;
    let primary = file.primary_tag();
    let tags: Vec<&Tag> = primary
        .into_iter()
        .chain(
            file.tags()
                .iter()
                .filter(|t| Some(t.tag_type()) != primary.map(Tag::tag_type)),
        )
        .collect();
    Ok(from_tags(&tags))
}

/// Surface an I/O failure behind a lofty error as itself; everything else
/// (unknown format, corrupt or truncated file) is invalid data.
fn parse_error(e: lofty::error::FileParseError) -> io::Error {
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<io::Error>()
            && !matches!(
                io.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            )
        {
            return io::Error::new(io.kind(), io.to_string());
        }
        source = s.source();
    }
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn from_tags(tags: &[&Tag]) -> FileTags {
    let mbids = |key: ItemKey| {
        tags.iter()
            .map(|t| {
                t.get_strings(key)
                    .flat_map(extract_mbids)
                    .collect::<Vec<_>>()
            })
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let mbid = |key: ItemKey| mbids(key).into_iter().next();
    let names = |multi: ItemKey, single: ItemKey| {
        [multi, single]
            .into_iter()
            .flat_map(|key| tags.iter().map(move |t| names(t, key)))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    let text = |f: fn(&Tag) -> Option<std::borrow::Cow<'_, str>>| {
        tags.iter()
            .find_map(|t| f(t).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()))
    };
    FileTags {
        release_track_mbid: mbid(ItemKey::MusicBrainzTrackId),
        recording_mbid: mbid(ItemKey::MusicBrainzRecordingId),
        release_mbid: mbid(ItemKey::MusicBrainzReleaseId),
        artist_mbids: mbids(ItemKey::MusicBrainzArtistId),
        album_artist_mbids: mbids(ItemKey::MusicBrainzReleaseArtistId),
        artists: names(ItemKey::TrackArtists, ItemKey::TrackArtist),
        album_artists: names(ItemKey::AlbumArtists, ItemKey::AlbumArtist),
        title: text(|t| t.title()),
        album: text(|t| t.album()),
        disc_no: tags.iter().find_map(|t| t.disk()),
        track_no: tags.iter().find_map(|t| t.track()),
    }
}

/// Values of a name tag. Only `\0` splits a value: `/` and `;` appear in
/// real names (AC/DC).
fn names(tag: &Tag, key: ItemKey) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in tag.get_strings(key).flat_map(|v| v.split('\0')) {
        let v = v.trim();
        if !v.is_empty() && !out.iter().any(|o| o == v) {
            out.push(v.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use lofty::tag::{ItemValue, TagItem, TagType};

    use super::*;

    const REC: &str = "5b11f4ce-a62d-471e-81fc-a69a8278c7da";
    const TRK: &str = "b7ffd2af-418f-4be2-bdd1-22f8b48613da";
    const REL: &str = "0e3b3c85-61b6-4ba6-9f1b-8a4b8b8e7d8f";
    const AR1: &str = "e0140a67-e4d1-4f13-8a01-364355bee46e";
    const AR2: &str = "1f9df192-a621-4f54-8850-2c5373b7eac9";

    fn item(tag: &mut Tag, key: ItemKey, v: &str) {
        tag.push(TagItem::new(key, ItemValue::Text(v.into())));
    }

    #[test]
    fn maps_musicbrainz_and_names() {
        let mut t = Tag::new(TagType::VorbisComments);
        item(&mut t, ItemKey::MusicBrainzTrackId, TRK);
        item(&mut t, ItemKey::MusicBrainzRecordingId, REC);
        item(&mut t, ItemKey::MusicBrainzReleaseId, &REL.to_uppercase());
        item(&mut t, ItemKey::MusicBrainzArtistId, AR1);
        item(&mut t, ItemKey::MusicBrainzArtistId, AR2);
        item(
            &mut t,
            ItemKey::MusicBrainzReleaseArtistId,
            &format!("{AR1}/{AR2}"),
        );
        item(&mut t, ItemKey::TrackArtist, "A & B");
        item(&mut t, ItemKey::TrackArtists, "AC/DC\0B");
        item(&mut t, ItemKey::AlbumArtist, "A & B");
        t.set_title("  Song ".into());
        t.set_album("Album".into());
        t.set_disk(1);
        t.set_track(4);

        let f = from_tags(&[&t]);
        assert_eq!(f.release_track_mbid.as_deref(), Some(TRK));
        assert_eq!(f.recording_mbid.as_deref(), Some(REC));
        assert_eq!(f.release_mbid.as_deref(), Some(REL));
        assert_eq!(f.artist_mbids, [AR1, AR2]);
        assert_eq!(f.album_artist_mbids, [AR1, AR2]);
        assert_eq!(f.artists, ["AC/DC", "B"]);
        assert_eq!(f.album_artists, ["A & B"]);
        assert_eq!(f.title.as_deref(), Some("Song"));
        assert_eq!(f.album.as_deref(), Some("Album"));
        assert_eq!((f.disc_no, f.track_no), (Some(1), Some(4)));
    }

    #[test]
    fn falls_back_across_tags() {
        let mut id3 = Tag::new(TagType::Id3v2);
        id3.set_title("Title".into());
        let mut ape = Tag::new(TagType::Ape);
        item(&mut ape, ItemKey::MusicBrainzTrackId, TRK);
        ape.set_title("Other".into());

        let f = from_tags(&[&id3, &ape]);
        assert_eq!(f.release_track_mbid.as_deref(), Some(TRK));
        assert_eq!(f.title.as_deref(), Some("Title"));
        assert!(from_tags(&[]).release_track_mbid.is_none());
    }

    /// A minimal file of each format: tags written by lofty in the format's
    /// native frames (ID3v2 TXXX/UFID, Vorbis comments) read back through
    /// [`read_file_tags`].
    #[test]
    fn round_trips_real_formats() {
        use lofty::config::WriteOptions;
        use lofty::file::AudioFile;

        // Silent MPEG-1 Layer III frames (128 kbps, 44.1 kHz).
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
        frame.resize(417, 0);
        let mp3 = frame.repeat(4);
        // fLaC + a last-block STREAMINFO (44.1 kHz, stereo, 16 bit).
        let mut flac = b"fLaC\x80\x00\x00\x22".to_vec();
        flac.extend([0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]);
        flac.extend([0x0A, 0xC4, 0x42, 0xF0, 0, 0, 0, 0]);
        flac.extend([0u8; 16]);

        let dir = std::env::temp_dir().join(format!("rsub-fmt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes, tag_type) in [
            ("a.mp3", mp3, TagType::Id3v2),
            ("a.flac", flac, TagType::VorbisComments),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            let mut tag = Tag::new(tag_type);
            item(&mut tag, ItemKey::MusicBrainzTrackId, TRK);
            item(&mut tag, ItemKey::MusicBrainzRecordingId, REC);
            item(&mut tag, ItemKey::MusicBrainzReleaseId, REL);
            item(&mut tag, ItemKey::MusicBrainzArtistId, AR1);
            item(&mut tag, ItemKey::MusicBrainzArtistId, AR2);
            item(&mut tag, ItemKey::MusicBrainzReleaseArtistId, AR1);
            item(&mut tag, ItemKey::TrackArtist, "A");
            tag.set_title("Song".into());
            tag.set_track(3);
            let mut file = lofty::read_from_path(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            file.insert_tag(tag);
            file.save_to_path(&path, WriteOptions::default()).unwrap();

            let f = read_file_tags(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(f.release_track_mbid.as_deref(), Some(TRK), "{name}");
            assert_eq!(f.recording_mbid.as_deref(), Some(REC), "{name}");
            assert_eq!(f.release_mbid.as_deref(), Some(REL), "{name}");
            assert_eq!(f.artist_mbids, [AR1, AR2], "{name}");
            assert_eq!(f.album_artist_mbids, [AR1], "{name}");
            assert_eq!(f.artists, ["A"], "{name}");
            assert_eq!(f.title.as_deref(), Some("Song"), "{name}");
            assert_eq!(f.track_no, Some(3), "{name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stat_and_read_real_file() {
        let dir = std::env::temp_dir().join(format!("rsub-tags-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("A")).unwrap();
        // Not an audio file: stat works, reading fails cleanly.
        std::fs::write(dir.join("A/x.flac"), b"not audio").unwrap();
        let r = LocalTagReader::new(PathMapper::new([("/music".to_string(), dir.clone())]));

        let s = r.stat("/music/A/x.flac").await.unwrap();
        assert_eq!(s.size, 9);
        assert!(s.mtime_ms > 0);
        assert!(r.stat("/music/A/missing.flac").await.is_err());
        let unmapped = r.stat("/elsewhere/x.flac").await.unwrap_err();
        assert!(unmapped.to_string().contains("path_map"), "{unmapped}");
        let err = r.read("/music/A/x.flac").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        let err = r.read("/music/A/missing.flac").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(dir);
    }
}
