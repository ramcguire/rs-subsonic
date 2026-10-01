//! Domain model, identifiers, backend traits and crypto for rs-subsonic.

pub mod backend;
pub mod crypto;
pub mod id;
pub mod identity;
pub mod model;
pub mod tags;
pub mod text;

pub use id::{CoverArtId, Kind, PublicId};
pub use model::{Roles, User, now_ms};
