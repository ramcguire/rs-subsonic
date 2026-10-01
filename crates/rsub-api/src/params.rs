use std::str::FromStr;

use crate::error::ApiError;

/// Request parameters from the query string and (for the `formPost` extension) a
/// form-encoded body. Keys may repeat (`id=1&id=2`).
#[derive(Debug, Default, Clone)]
pub struct Params {
    pairs: Vec<(String, String)>,
}

impl Params {
    pub fn parse(query: Option<&str>, form_body: Option<&[u8]>) -> Self {
        let mut pairs = Vec::new();
        if let Some(q) = query {
            pairs.extend(form_urlencoded::parse(q.as_bytes()).into_owned());
        }
        if let Some(b) = form_body {
            pairs.extend(form_urlencoded::parse(b).into_owned());
        }
        Params { pairs }
    }

    /// First value for `key`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// All values for `key`, in request order.
    pub fn get_all<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter(move |(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Error 10 if missing (or empty).
    pub fn required(&self, key: &str) -> Result<&str, ApiError> {
        match self.get(key) {
            Some(v) if !v.is_empty() => Ok(v),
            _ => Err(ApiError::missing(key)),
        }
    }

    /// Parse an optional value; error 0 if present but malformed.
    pub fn parse_opt<T: FromStr>(&self, key: &str) -> Result<Option<T>, ApiError> {
        match self.get(key) {
            None | Some("") => Ok(None),
            Some(v) => v
                .parse()
                .map(Some)
                .map_err(|_| ApiError::generic(format!("Invalid value for parameter '{key}'."))),
        }
    }

    pub fn parse_or<T: FromStr>(&self, key: &str, default: T) -> Result<T, ApiError> {
        Ok(self.parse_opt(key)?.unwrap_or(default))
    }

    pub fn parse_required<T: FromStr>(&self, key: &str) -> Result<T, ApiError> {
        self.parse_opt(key)?.ok_or_else(|| ApiError::missing(key))
    }
}

/// Decode a Subsonic `p` value: plaintext or `enc:`-prefixed hex.
pub fn decode_password(p: &str) -> Option<String> {
    match p.strip_prefix("enc:") {
        Some(h) => String::from_utf8(hex::decode(h).ok()?).ok(),
        None => Some(p.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_query_and_body_with_repeats() {
        let p = Params::parse(Some("u=bob&id=1&id=2"), Some(b"id=3&name=a+b%21"));
        assert_eq!(p.get("u"), Some("bob"));
        assert_eq!(p.get_all("id").collect::<Vec<_>>(), ["1", "2", "3"]);
        assert_eq!(p.get("name"), Some("a b!"));
        assert_eq!(p.parse_opt::<u32>("id").unwrap(), Some(1));
        assert!(p.parse_opt::<u32>("name").is_err());
        assert_eq!(
            p.required("missing").unwrap_err().code,
            crate::ErrorCode::MissingParameter
        );
    }

    #[test]
    fn decodes_enc_passwords() {
        assert_eq!(
            decode_password("enc:736573616d65").as_deref(),
            Some("sesame")
        );
        assert_eq!(decode_password("sesame").as_deref(), Some("sesame"));
        assert_eq!(decode_password("enc:zz"), None);
    }
}
