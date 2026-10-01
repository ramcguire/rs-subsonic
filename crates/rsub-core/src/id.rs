//! Public ids: a type prefix plus an opaque token (`arq3v7k2m9x4b8c1d`).
//! Backend keys and local surrogate ids are never exposed.

use std::fmt;
use std::str::FromStr;

use crate::crypto::{CryptoError, random_bytes, sha256};

/// Token length in characters: 80 bits in base32.
pub const TOKEN_LEN: usize = 16;
const TOKEN_BYTES: usize = 10;
/// RFC 4648 base32, lowercase.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Artist,
    Album,
    Track,
    Playlist,
    Share,
    PodcastEpisode,
}

impl Kind {
    pub const fn prefix(self) -> &'static str {
        match self {
            Kind::Artist => "ar",
            Kind::Album => "al",
            Kind::Track => "tr",
            Kind::Playlist => "pl",
            Kind::Share => "sh",
            Kind::PodcastEpisode => "pe",
        }
    }

    fn from_prefix(p: &str) -> Option<Kind> {
        Some(match p {
            "ar" => Kind::Artist,
            "al" => Kind::Album,
            "tr" => Kind::Track,
            "pl" => Kind::Playlist,
            "sh" => Kind::Share,
            "pe" => Kind::PodcastEpisode,
            _ => return None,
        })
    }
}

/// A public id. Clients must treat the token as opaque.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicId {
    kind: Kind,
    token: [u8; TOKEN_LEN],
}

impl PublicId {
    /// The id minted from an identity key: the first 80 bits of
    /// `SHA-256("rsub-id:v1:{prefix}:{key}")`. The same key always mints the
    /// same id, so a rebuilt database reproduces it.
    pub fn mint(kind: Kind, key: &str) -> Self {
        let digest = sha256(format!("rsub-id:v1:{}:{key}", kind.prefix()).as_bytes());
        Self::from_bytes(
            kind,
            digest[..TOKEN_BYTES]
                .try_into()
                .expect("digest is long enough"),
        )
    }

    /// A random id, for local-only records such as shares.
    pub fn random(kind: Kind) -> Result<Self, CryptoError> {
        Ok(Self::from_bytes(kind, random_bytes::<TOKEN_BYTES>()?))
    }

    fn from_bytes(kind: Kind, bytes: [u8; TOKEN_BYTES]) -> Self {
        let bits = bytes.iter().fold(0u128, |acc, &b| acc << 8 | u128::from(b));
        let mut token = [0u8; TOKEN_LEN];
        for (i, c) in token.iter_mut().enumerate() {
            *c = ALPHABET[(bits >> (5 * (TOKEN_LEN - 1 - i)) & 31) as usize];
        }
        PublicId { kind, token }
    }

    pub const fn kind(&self) -> Kind {
        self.kind
    }
}

impl fmt::Display for PublicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.prefix())?;
        // The token is always ASCII from `ALPHABET`.
        f.write_str(std::str::from_utf8(&self.token).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for PublicId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicId({self})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid id")]
pub struct InvalidId;

impl FromStr for PublicId {
    type Err = InvalidId;

    fn from_str(s: &str) -> Result<Self, InvalidId> {
        let (prefix, token) = s.split_at_checked(2).ok_or(InvalidId)?;
        let kind = Kind::from_prefix(prefix).ok_or(InvalidId)?;
        let token: [u8; TOKEN_LEN] = token.as_bytes().try_into().map_err(|_| InvalidId)?;
        if !token.iter().all(|b| ALPHABET.contains(b)) {
            return Err(InvalidId);
        }
        Ok(PublicId { kind, token })
    }
}

/// Cover art id: `<public id>-<8 hex version>`. The version busts client caches when
/// art changes and is ignored when parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CoverArtId {
    pub entity: PublicId,
    pub version: u32,
}

impl fmt::Display for CoverArtId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{:08x}", self.entity, self.version)
    }
}

impl FromStr for CoverArtId {
    type Err = InvalidId;

    fn from_str(s: &str) -> Result<Self, InvalidId> {
        let (entity, version) = match s.split_once('-') {
            Some((e, v)) => (e, u32::from_str_radix(v, 16).map_err(|_| InvalidId)?),
            None => (s, 0),
        };
        Ok(CoverArtId {
            entity: entity.parse()?,
            version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [Kind; 6] = [
        Kind::Artist,
        Kind::Album,
        Kind::Track,
        Kind::Playlist,
        Kind::Share,
        Kind::PodcastEpisode,
    ];

    #[test]
    fn roundtrip() {
        for kind in KINDS {
            let id = PublicId::mint(kind, "plex://track/5d07");
            let s = id.to_string();
            assert_eq!(s.len(), 2 + TOKEN_LEN);
            assert!(s.starts_with(kind.prefix()));
            assert_eq!(s.parse::<PublicId>().unwrap(), id);
            let r = PublicId::random(kind).unwrap();
            assert_eq!(r.to_string().parse::<PublicId>().unwrap(), r);
        }
        let art = CoverArtId {
            entity: PublicId::mint(Kind::Album, "x"),
            version: 0x9f3a01bc,
        };
        let s = art.to_string();
        assert!(s.ends_with("-9f3a01bc"), "{s}");
        assert_eq!(s.parse::<CoverArtId>().unwrap(), art);
        assert_eq!(
            art.entity.to_string().parse::<CoverArtId>().unwrap().entity,
            art.entity
        );
    }

    #[test]
    fn minting_is_deterministic_and_pinned() {
        assert_eq!(
            PublicId::mint(Kind::Track, "k"),
            PublicId::mint(Kind::Track, "k")
        );
        assert_ne!(
            PublicId::mint(Kind::Track, "k"),
            PublicId::mint(Kind::Track, "l")
        );
        // The kind is part of the hash, not only the prefix.
        assert_ne!(
            PublicId::mint(Kind::Track, "k").token,
            PublicId::mint(Kind::Album, "k").token
        );
        // Changing the scheme changes every id clients hold: these must not move.
        // (Python: base64.b32encode(sha256(b"rsub-id:v1:tr:k").digest()[:10]).lower())
        assert_eq!(
            PublicId::mint(Kind::Track, "k").to_string(),
            "trcnkkcbslrgo2ll24"
        );
        assert_eq!(
            PublicId::mint(Kind::Artist, "name:the beatles").to_string(),
            "aru3obbni2gjhznok7"
        );
    }

    #[test]
    fn base32_matches_rfc4648() {
        // RFC 4648 test vector: base32("foobar") = "MZXW6YTBOI======"; ten
        // zero-padded bytes encode to exactly 16 characters.
        let mut bytes = [0u8; TOKEN_BYTES];
        bytes[..6].copy_from_slice(b"foobar");
        let id = PublicId::from_bytes(Kind::Track, bytes);
        assert_eq!(id.to_string(), "trmzxw6ytboiaaaaaa");
    }

    #[test]
    fn rejects_garbage() {
        let good = PublicId::mint(Kind::Album, "x").to_string();
        let token = &good[2..];
        for s in [
            "".to_owned(),
            "al".to_owned(),
            "al1".to_owned(),
            "al34".to_owned(),
            format!("xx{token}"),
            format!("al{}", &token[1..]),
            format!("al{token}a"),
            format!("al{}", token.to_uppercase()),
            format!("al{}1", &token[1..]),
            format!("al{}é", &token[2..]),
        ] {
            assert!(s.parse::<PublicId>().is_err(), "{s}");
        }
    }
}
