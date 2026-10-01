use serde::Serialize;

use crate::error::ApiError;
use crate::params::Params;
use crate::response::{Envelope, XMLNS};
use crate::xml;

/// Response format selected by the `f` parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    Xml,
    Json,
    Jsonp(String),
}

impl Format {
    /// Unknown `f` values fall back to XML (the Subsonic default).
    /// An invalid JSONP callback is an error; render it with [`Format::Json`].
    pub fn from_params(p: &Params) -> Result<Format, ApiError> {
        match p.get("f") {
            Some("json") => Ok(Format::Json),
            Some("jsonp") => {
                let cb = p.required("callback")?;
                if cb
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.$".contains(&b))
                {
                    Ok(Format::Jsonp(cb.to_owned()))
                } else {
                    Err(ApiError::generic("Invalid JSONP callback name."))
                }
            }
            _ => Ok(Format::Xml),
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self {
            Format::Xml => "application/xml; charset=utf-8",
            Format::Json => "application/json; charset=utf-8",
            Format::Jsonp(_) => "application/javascript; charset=utf-8",
        }
    }
}

#[derive(Serialize)]
struct JsonRoot<'a> {
    #[serde(rename = "subsonic-response")]
    inner: &'a Envelope,
}

/// Render an envelope. Serialization of our own types cannot fail except through a bug,
/// in which case a generic error envelope is rendered instead.
pub fn render(format: &Format, env: &Envelope) -> Vec<u8> {
    match try_render(format, env) {
        Ok(body) => body,
        Err(msg) => {
            let fallback =
                Envelope::error(ApiError::generic(format!("Serialization failed: {msg}")));
            try_render(format, &fallback).expect("error envelope must serialize")
        }
    }
}

fn try_render(format: &Format, env: &Envelope) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(512);
    match format {
        Format::Xml => {
            out.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            xml::to_writer(&mut out, "subsonic-response", Some(XMLNS), env)
                .map_err(|e| e.to_string())?;
        }
        Format::Json => {
            serde_json::to_writer(&mut out, &JsonRoot { inner: env }).map_err(|e| e.to_string())?;
        }
        Format::Jsonp(cb) => {
            out.extend_from_slice(cb.as_bytes());
            out.push(b'(');
            serde_json::to_writer(&mut out, &JsonRoot { inner: env }).map_err(|e| e.to_string())?;
            out.extend_from_slice(b");");
        }
    }
    Ok(out)
}
