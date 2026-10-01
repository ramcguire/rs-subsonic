//! Subsonic / OpenSubsonic wire layer: response types, error codes, parameter parsing
//! and the XML / JSON / JSONP renderers. No IO and no dependency on the domain model.

pub mod error;
pub mod format;
pub mod model;
pub mod params;
pub mod response;
pub mod xml;

pub use error::{ApiError, ErrorCode};
pub use format::{Format, render};
pub use params::Params;
pub use response::{Envelope, Payload, Status};
