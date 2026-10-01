use std::borrow::Cow;
use std::fmt;

use serde::Serialize;

/// Subsonic / OpenSubsonic error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Generic = 0,
    MissingParameter = 10,
    ClientTooOld = 20,
    ServerTooOld = 30,
    WrongCredentials = 40,
    TokenAuthNotSupported = 41,
    AuthMechanismNotSupported = 42,
    ConflictingAuth = 43,
    InvalidApiKey = 44,
    NotAuthorized = 50,
    TrialExpired = 60,
    NotFound = 70,
}

impl ErrorCode {
    pub fn default_message(self) -> &'static str {
        match self {
            ErrorCode::Generic => "A generic error.",
            ErrorCode::MissingParameter => "Required parameter is missing.",
            ErrorCode::ClientTooOld => {
                "Incompatible Subsonic REST protocol version. Client must upgrade."
            }
            ErrorCode::ServerTooOld => {
                "Incompatible Subsonic REST protocol version. Server must upgrade."
            }
            ErrorCode::WrongCredentials => "Wrong username or password.",
            ErrorCode::TokenAuthNotSupported => "Token authentication not supported.",
            ErrorCode::AuthMechanismNotSupported => {
                "Provided authentication mechanism not supported."
            }
            ErrorCode::ConflictingAuth => {
                "Multiple conflicting authentication mechanisms provided."
            }
            ErrorCode::InvalidApiKey => "Invalid API key.",
            ErrorCode::NotAuthorized => "User is not authorized for the given operation.",
            ErrorCode::TrialExpired => "The trial period for the Subsonic server is over.",
            ErrorCode::NotFound => "The requested data was not found.",
        }
    }
}

/// An error returned to the client inside a `status="failed"` envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: Cow<'static, str>,
}

impl ApiError {
    pub fn new(code: ErrorCode) -> Self {
        ApiError {
            code,
            message: Cow::Borrowed(code.default_message()),
        }
    }

    pub fn with_message(code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        ApiError {
            code,
            message: message.into(),
        }
    }

    pub fn generic(message: impl Into<Cow<'static, str>>) -> Self {
        Self::with_message(ErrorCode::Generic, message)
    }

    pub fn missing(param: &str) -> Self {
        Self::with_message(
            ErrorCode::MissingParameter,
            format!("Required parameter '{param}' is missing."),
        )
    }

    pub fn not_found(what: &str) -> Self {
        Self::with_message(ErrorCode::NotFound, format!("{what} not found."))
    }

    pub fn not_authorized() -> Self {
        Self::new(ErrorCode::NotAuthorized)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "error {}: {}", self.code as u16, self.message)
    }
}

impl std::error::Error for ApiError {}

impl Serialize for ApiError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("error", 2)?;
        st.serialize_field("code", &(self.code as u16))?;
        st.serialize_field("message", &self.message)?;
        st.end()
    }
}
