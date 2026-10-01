//! Local media: mapping backend paths to local files, serving them with HTTP
//! Range support, tag reading (`local-tags`), and the cover-art disk cache.

pub mod cover;
pub mod local;
pub mod paths;
pub mod range;
#[cfg(feature = "local-tags")]
pub mod tags;

pub use cover::CoverCache;
pub use local::{LocalFile, open_local};
pub use paths::PathMapper;
pub use range::{RangeError, parse_range, resolve_range};
#[cfg(feature = "local-tags")]
pub use tags::LocalTagReader;
