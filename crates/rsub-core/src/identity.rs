//! Identity keys: the values an artist, album or track is recognised by across
//! Plex re-keys and database rebuilds, and that its public id is minted from.

use crate::text::normalize;

/// `mb:rt:{mbid}`: a track on one release (`MUSICBRAINZ_RELEASETRACKID`).
pub fn release_track_key(mbid: &str) -> String {
    format!("mb:rt:{}", mbid.to_ascii_lowercase())
}

/// `mb:rel:{mbid}`: a release (`MUSICBRAINZ_ALBUMID`).
pub fn release_key(mbid: &str) -> String {
    format!("mb:rel:{}", mbid.to_ascii_lowercase())
}

/// `mb:ar:{mbid}`: an artist (`MUSICBRAINZ_ARTISTID`, `MUSICBRAINZ_ALBUMARTISTID`).
pub fn artist_key(mbid: &str) -> String {
    format!("mb:ar:{}", mbid.to_ascii_lowercase())
}

/// `rk:{source}:{ratingKey}`: the item's current backend key. Recognises an
/// item whose other keys just changed, but is never minted from, because
/// backends hand out new keys on re-add.
pub fn rating_key(source: &str, key: &str) -> String {
    format!("rk:{source}:{key}")
}

/// `pl:{source}:{playlistId}`: a backend playlist. Playlists aren't indexed;
/// their public id is minted from this key on every read. Backends keep a
/// playlist's id for its lifetime, so this key needs no ledger.
pub fn playlist_key(source: &str, id: &str) -> String {
    format!("pl:{source}:{id}")
}

/// `id:{public id}`: an id whose item is gone, pointing at the id its item
/// continued as, so ids clients hold keep resolving. Never minted from and
/// never part of an item's own keys.
pub fn alias_key(public_id: &str) -> String {
    format!("id:{public_id}")
}

/// Whether a public id may be minted from `key` (every key but ratingKeys
/// and aliases).
pub fn is_mintable(key: &str) -> bool {
    !key.starts_with("rk:") && !key.starts_with("id:")
}

/// `plex:{guid}` for guids in the `plex://` scheme. Other schemes (unmatched
/// items' `local://` and agent guids) change on re-add, so they are not keys.
pub fn plex_key(guid: Option<&str>) -> Option<String> {
    guid.filter(|g| g.starts_with("plex://"))
        .map(|g| format!("plex:{g}"))
}

/// `tag:{field}|{field}|…`, each field normalised like `search_norm`.
pub fn tag_key<'a>(fields: impl IntoIterator<Item = &'a str>) -> String {
    let fields: Vec<String> = fields.into_iter().map(normalize).collect();
    format!("tag:{}", fields.join("|"))
}

/// `tag:{album artist}|{album}|{disc}|{track}|{title}`.
pub fn track_tag_key(
    album_artist: &str,
    album: &str,
    disc: Option<u32>,
    track: Option<u32>,
    title: &str,
) -> String {
    let n = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_default();
    tag_key([album_artist, album, &n(disc), &n(track), title])
}

/// `tag:{album artist}|{album}|{year}`.
pub fn album_tag_key(album_artist: &str, album: &str, year: Option<i32>) -> String {
    let year = year.map(|y| y.to_string()).unwrap_or_default();
    tag_key([album_artist, album, &year])
}

/// `name:{name}`, normalised.
pub fn name_key(name: &str) -> String {
    format!("name:{}", normalize(name))
}

/// The key a new id is minted from when `key` is already taken: the `n`th
/// duplicate (`n >= 2`) gets `{key}#{n}`.
pub fn duplicate_key(key: &str, n: u32) -> String {
    format!("{key}#{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(
            plex_key(Some("plex://track/5d07")).as_deref(),
            Some("plex:plex://track/5d07")
        );
        assert_eq!(plex_key(Some("local://1234")), None);
        assert_eq!(plex_key(None), None);
        assert_eq!(
            track_tag_key("The Beatles", "Abbey Road", Some(1), Some(2), "Something"),
            "tag:the beatles|abbey road|1|2|something"
        );
        assert_eq!(
            track_tag_key("Björk", "Homogenic ", None, None, "Jóga"),
            "tag:bjork|homogenic|||joga"
        );
        assert_eq!(
            album_tag_key("Björk", "Homogenic", Some(1997)),
            "tag:bjork|homogenic|1997"
        );
        assert_eq!(name_key("  Sigur  Rós"), "name:sigur ros");
        assert_eq!(duplicate_key("name:x", 2), "name:x#2");
        assert_eq!(
            release_track_key("0A4F9C2E-1111-2222-3333-444455556666"),
            "mb:rt:0a4f9c2e-1111-2222-3333-444455556666"
        );
        assert_eq!(release_key("r"), "mb:rel:r");
        assert_eq!(artist_key("a"), "mb:ar:a");
        assert_eq!(rating_key("home", "123"), "rk:home:123");
        assert!(!is_mintable(&rating_key("home", "123")));
        assert_eq!(alias_key("trabc"), "id:trabc");
        assert!(!is_mintable(&alias_key("trabc")));
        assert!(is_mintable(&release_key("r")));
    }
}
