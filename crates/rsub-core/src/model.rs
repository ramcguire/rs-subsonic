//! Domain types shared by the store, the server and backends.

use std::ops::BitOr;

/// Subsonic user roles, stored as a bitmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Roles(pub u32);

impl Roles {
    pub const ADMIN: Roles = Roles(1 << 0);
    pub const SETTINGS: Roles = Roles(1 << 1);
    pub const STREAM: Roles = Roles(1 << 2);
    pub const DOWNLOAD: Roles = Roles(1 << 3);
    pub const UPLOAD: Roles = Roles(1 << 4);
    pub const PLAYLIST: Roles = Roles(1 << 5);
    pub const COVER_ART: Roles = Roles(1 << 6);
    pub const COMMENT: Roles = Roles(1 << 7);
    pub const PODCAST: Roles = Roles(1 << 8);
    pub const SHARE: Roles = Roles(1 << 9);
    pub const JUKEBOX: Roles = Roles(1 << 10);
    pub const VIDEO_CONVERSION: Roles = Roles(1 << 11);
    pub const SCROBBLING: Roles = Roles(1 << 12);
    /// Star and rate items in the backend. Not a Subsonic role: Subsonic lets
    /// every user rate, but here ratings are the shared Plex account's.
    pub const RATING: Roles = Roles(1 << 13);

    /// Roles granted to a regular user by default. Until users link their own
    /// Plex account (M5), playlists, plays and ratings are the configured
    /// account's, so writing them is left to an admin to grant.
    pub const USER_DEFAULT: Roles = Roles(
        Self::SETTINGS.0
            | Self::STREAM.0
            | Self::DOWNLOAD.0
            | Self::COVER_ART.0
            | Self::COMMENT.0
            | Self::SHARE.0,
    );
    pub const ALL: Roles = Roles((1 << 14) - 1);

    pub const fn contains(self, other: Roles) -> bool {
        self.0 & other.0 == other.0
    }

    /// Each role by the name the Subsonic API gives it (`getUser`'s
    /// `…Role` attributes without the suffix), then `rating`.
    pub const NAMES: [(&'static str, Roles); 14] = [
        ("admin", Roles::ADMIN),
        ("settings", Roles::SETTINGS),
        ("stream", Roles::STREAM),
        ("download", Roles::DOWNLOAD),
        ("upload", Roles::UPLOAD),
        ("playlist", Roles::PLAYLIST),
        ("coverArt", Roles::COVER_ART),
        ("comment", Roles::COMMENT),
        ("podcast", Roles::PODCAST),
        ("share", Roles::SHARE),
        ("jukebox", Roles::JUKEBOX),
        ("videoConversion", Roles::VIDEO_CONVERSION),
        ("scrobbling", Roles::SCROBBLING),
        ("rating", Roles::RATING),
    ];

    /// The names of the roles held, in [`Roles::NAMES`] order.
    pub fn names(self) -> Vec<&'static str> {
        Self::NAMES
            .iter()
            .filter(|(_, r)| self.contains(*r))
            .map(|(n, _)| *n)
            .collect()
    }

    /// The roles with these names (case-insensitive); `Err` names the first
    /// unknown one.
    pub fn from_names<S: AsRef<str>>(names: &[S]) -> Result<Roles, String> {
        names.iter().try_fold(Roles(0), |acc, n| {
            let n = n.as_ref().trim();
            Self::NAMES
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(n))
                .map(|(_, r)| acc | *r)
                .ok_or_else(|| {
                    let all: Vec<&str> = Self::NAMES.iter().map(|(n, _)| *n).collect();
                    format!("unknown role '{n}' (one of: {})", all.join(", "))
                })
        })
    }

    /// These roles with `admin` granted or taken away.
    pub const fn with_admin(self, admin: bool) -> Roles {
        if admin {
            Roles(self.0 | Self::ADMIN.0)
        } else {
            Roles(self.0 & !Self::ADMIN.0)
        }
    }
}

impl BitOr for Roles {
    type Output = Roles;
    fn bitor(self, rhs: Roles) -> Roles {
        Roles(self.0 | rhs.0)
    }
}

/// A local user account. Secrets stay encrypted until needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub roles: Roles,
    /// 0 = unlimited.
    pub max_bitrate: u32,
    pub password_enc: Vec<u8>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.roles.contains(Roles::ADMIN)
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_by_name() {
        let r = Roles::from_names(&["stream", "CoverArt"]).unwrap();
        assert_eq!(r, Roles::STREAM | Roles::COVER_ART);
        assert_eq!(r.names(), ["stream", "coverArt"]);
        assert_eq!(Roles::ALL.names().len(), Roles::NAMES.len());
        assert!(Roles::from_names(&["root"]).unwrap_err().contains("root"));
        assert!(r.with_admin(true).contains(Roles::ADMIN));
        assert_eq!(r.with_admin(true).with_admin(false), r);
    }
}
