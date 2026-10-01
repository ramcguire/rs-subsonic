//! Wire-format snapshots: every payload rendered as XML and JSON.

use rsub_api::model::{
    AlbumId3, Artist, ArtistRef, Child, Empty, Genre, Genres, Index, Indexes, ItemDate, ItemGenre,
    LyricLine, Lyrics, LyricsList, NowPlaying, Playlist, Playlists, RecordLabel, ReplayGain, Songs,
    StructuredLyrics,
};
use rsub_api::response::{License, OpenSubsonicExtension, SERVER_VERSION, TokenInfo, User, Users};
use rsub_api::{ApiError, Envelope, ErrorCode, Format, Payload, render};

/// Snapshots of both formats, with `serverVersion` redacted so a version bump
/// doesn't rewrite them.
fn both(name: &str, env: &Envelope) {
    let xml = String::from_utf8(render(&Format::Xml, env))
        .unwrap()
        .replace(
            &format!("serverVersion=\"{SERVER_VERSION}\""),
            "serverVersion=\"[version]\"",
        );
    let mut json: serde_json::Value = serde_json::from_slice(&render(&Format::Json, env)).unwrap();
    json["subsonic-response"]["serverVersion"] = "[version]".into();
    insta::assert_snapshot!(format!("{name}_xml"), xml);
    insta::assert_json_snapshot!(format!("{name}_json"), json);
}

fn user() -> User {
    User {
        username: "alice".into(),
        email: Some("alice@example.com".into()),
        scrobbling_enabled: true,
        admin_role: true,
        stream_role: true,
        playlist_role: true,
        folder: vec![1, 2],
        ..Default::default()
    }
}

#[test]
fn ping() {
    both("ping", &Envelope::ok());
}

#[test]
fn error() {
    both(
        "error",
        &Envelope::error(ApiError::new(ErrorCode::WrongCredentials)),
    );
}

#[test]
fn license() {
    both(
        "license",
        &Envelope::with(License {
            valid: true,
            email: None,
            license_expires: None,
            trial_expires: None,
        }),
    );
}

#[test]
fn extensions() {
    both(
        "extensions",
        &Envelope::with(vec![
            OpenSubsonicExtension {
                name: "formPost",
                versions: vec![1],
            },
            OpenSubsonicExtension {
                name: "apiKeyAuthentication",
                versions: vec![1],
            },
        ]),
    );
}

#[test]
fn user_and_users() {
    both("user", &Envelope::with(user()));
    both("users", &Envelope::with(Users { user: vec![user()] }));
    let info = TokenInfo {
        username: "alice".into(),
    };
    both("token_info", &Envelope::with(info));
}

#[test]
fn jsonp_wraps_json() {
    let body = String::from_utf8(render(&Format::Jsonp("cb".into()), &Envelope::ok())).unwrap();
    assert!(
        body.starts_with("cb({\"subsonic-response\":{\"status\":\"ok\""),
        "{body}"
    );
    assert!(body.ends_with("});"));
}

fn song() -> Child {
    Child {
        id: "tr56".into(),
        parent: Some("al34".into()),
        title: "Come Together".into(),
        album: Some("Abbey Road".into()),
        artist: Some("The Beatles".into()),
        track: Some(1),
        year: Some(1969),
        genre: Some("Rock".into()),
        cover_art: Some("al34-0badf00d".into()),
        size: Some(32985721),
        content_type: Some("audio/flac".into()),
        suffix: Some("flac".into()),
        duration: Some(259),
        bit_rate: Some(1015),
        path: Some("The Beatles/Abbey Road/01 - Come Together.flac".into()),
        is_video: Some(false),
        disc_number: Some(1),
        created: Some("2020-09-13T12:26:40.000Z".into()),
        album_id: Some("al34".into()),
        artist_id: Some("ar12".into()),
        kind: Some("music"),
        media_type: Some("song"),
        sort_name: Some("come together".into()),
        genres: Some(vec![ItemGenre {
            name: "Rock".into(),
        }]),
        artists: Some(vec![ArtistRef {
            id: "ar12".into(),
            name: "The Beatles".into(),
        }]),
        display_artist: Some("The Beatles".into()),
        album_artists: Some(vec![ArtistRef {
            id: "ar12".into(),
            name: "The Beatles".into(),
        }]),
        display_album_artist: Some("The Beatles".into()),
        contributors: Some(vec![]),
        moods: Some(vec!["Groovy".into()]),
        replay_gain: Some(ReplayGain {
            track_gain: Some(-9.36),
            album_gain: Some(-8.9),
            ..Default::default()
        }),
        bit_depth: Some(24),
        sampling_rate: Some(96000),
        channel_count: Some(2),
        ..Default::default()
    }
}

#[test]
fn album_with_songs() {
    both(
        "album",
        &Envelope::with(AlbumId3 {
            id: "al34".into(),
            name: "Abbey Road".into(),
            artist: Some("The Beatles".into()),
            artist_id: Some("ar12".into()),
            cover_art: Some("al34-0badf00d".into()),
            song_count: 1,
            duration: 259,
            created: "2020-09-13T12:26:40.000Z".into(),
            year: Some(1969),
            genre: Some("Rock".into()),
            record_labels: vec![RecordLabel {
                name: "Apple".into(),
            }],
            genres: vec![
                ItemGenre {
                    name: "Rock".into(),
                },
                ItemGenre { name: "Pop".into() },
            ],
            artists: vec![ArtistRef {
                id: "ar12".into(),
                name: "The Beatles".into(),
            }],
            display_artist: "The Beatles".into(),
            release_types: vec!["album".into()],
            sort_name: "abbey road".into(),
            release_date: ItemDate::parse("1969-09-26"),
            song: Some(vec![song()]),
            ..Default::default()
        }),
    );
}

#[test]
fn indexes() {
    both(
        "indexes",
        &Envelope::with(Indexes {
            last_modified: 1700000000000,
            ignored_articles: "The El La".into(),
            index: vec![Index {
                name: "B".into(),
                artist: vec![Artist {
                    id: "ar12".into(),
                    name: "The Beatles".into(),
                    cover_art: Some("ar12-00c0ffee".into()),
                    album_count: Some(2),
                    ..Default::default()
                }],
            }],
        }),
    );
}

#[test]
fn genres() {
    both(
        "genres",
        &Envelope::with(Genres {
            genre: vec![
                Genre {
                    song_count: 3,
                    album_count: 2,
                    value: "Rock & Roll".into(),
                },
                Genre {
                    song_count: 1,
                    album_count: 1,
                    value: "Pop".into(),
                },
            ],
        }),
    );
}

#[test]
fn song_lists_and_empty_lists() {
    both(
        "random_songs",
        &Envelope::with(Payload::RandomSongs(Songs { song: vec![song()] })),
    );
    both("playlists_empty", &Envelope::with(Playlists::default()));
    both(
        "bookmarks_empty",
        &Envelope::with(Payload::Bookmarks(Empty {})),
    );
}

fn playlist() -> Playlist {
    Playlist {
        id: "pl7".into(),
        name: "Road trip".into(),
        comment: Some("summer".into()),
        owner: "alice".into(),
        public: false,
        song_count: 1,
        duration: 259,
        created: "2024-06-01T10:00:00.000Z".into(),
        changed: "2024-06-02T10:00:00.000Z".into(),
        cover_art: Some("al34-0badf00d".into()),
        readonly: false,
        entry: None,
    }
}

#[test]
fn playlists_and_user_state() {
    both(
        "playlists",
        &Envelope::with(Playlists {
            playlist: vec![playlist()],
        }),
    );
    let starred = Child {
        user_rating: Some(4),
        play_count: Some(12),
        starred: Some("2024-06-01T10:00:00.000Z".into()),
        played: Some("2024-06-02T10:00:00.000Z".into()),
        ..song()
    };
    both(
        "playlist",
        &Envelope::with(Playlist {
            entry: Some(vec![starred]),
            ..playlist()
        }),
    );
    let playing = Child {
        username: Some("alice".into()),
        minutes_ago: Some(2),
        player_id: Some(1),
        player_name: Some("Feishin".into()),
        ..song()
    };
    both(
        "now_playing",
        &Envelope::with(NowPlaying {
            entry: vec![playing],
        }),
    );
}

#[test]
fn lyrics() {
    let line = |start: Option<i64>, value: &str| LyricLine {
        start,
        value: value.into(),
    };
    both(
        "lyrics_list",
        &Envelope::with(LyricsList {
            structured_lyrics: vec![
                StructuredLyrics {
                    lang: "und".into(),
                    synced: true,
                    line: vec![line(Some(1500), "Oh, I know"), line(Some(4520), "")],
                    ..Default::default()
                },
                StructuredLyrics {
                    display_artist: Some("A & B".into()),
                    display_title: Some("Song".into()),
                    lang: "eng".into(),
                    offset: Some(-100),
                    synced: false,
                    line: vec![line(None, "Plain <line>")],
                },
            ],
        }),
    );
    both("lyrics_list_empty", &Envelope::with(LyricsList::default()));
    both(
        "lyrics",
        &Envelope::with(Lyrics {
            artist: Some("A".into()),
            title: Some("Song".into()),
            value: Some(
                "One
Two"
                .into(),
            ),
        }),
    );
}
