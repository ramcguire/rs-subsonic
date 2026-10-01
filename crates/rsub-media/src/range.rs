//! HTTP `Range` handling (single ranges only, per RFC 9110 §14).

use rsub_core::backend::ByteRange;

/// Parse a `Range` header. Returns `None` for anything we serve as a full
/// response instead: missing, malformed, non-`bytes` units and multi-range.
pub fn parse_range(header: &str) -> Option<ByteRange> {
    let spec = header.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = (start.trim(), end.trim());
    if start.is_empty() {
        let n: u64 = end.parse().ok()?;
        return Some(ByteRange::Suffix(n));
    }
    let start: u64 = start.parse().ok()?;
    let end = if end.is_empty() {
        None
    } else {
        let e: u64 = end.parse().ok()?;
        if e < start {
            return None;
        }
        Some(e)
    };
    Some(ByteRange::From { start, end })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeError;

/// Inclusive `(start, end)` of `range` within a resource of `len` bytes.
pub fn resolve_range(range: ByteRange, len: u64) -> Result<(u64, u64), RangeError> {
    if len == 0 {
        return Err(RangeError);
    }
    match range {
        ByteRange::From { start, end } => {
            if start >= len {
                return Err(RangeError);
            }
            Ok((start, end.unwrap_or(len - 1).min(len - 1)))
        }
        ByteRange::Suffix(0) => Err(RangeError),
        ByteRange::Suffix(n) => Ok((len.saturating_sub(n), len - 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let from = |start, end| Some(ByteRange::From { start, end });
        assert_eq!(parse_range("bytes=0-"), from(0, None));
        assert_eq!(parse_range("bytes=10-19"), from(10, Some(19)));
        assert_eq!(parse_range("bytes=-500"), Some(ByteRange::Suffix(500)));
        assert_eq!(parse_range("bytes=0-1,5-6"), None);
        assert_eq!(parse_range("bytes=9-3"), None);
        assert_eq!(parse_range("items=0-1"), None);
        assert_eq!(parse_range("bytes=x-"), None);
    }

    #[test]
    fn resolves() {
        let r = |s, e| ByteRange::From { start: s, end: e };
        assert_eq!(resolve_range(r(0, None), 100), Ok((0, 99)));
        assert_eq!(resolve_range(r(90, Some(200)), 100), Ok((90, 99)));
        assert_eq!(resolve_range(ByteRange::Suffix(500), 100), Ok((0, 99)));
        assert_eq!(resolve_range(ByteRange::Suffix(10), 100), Ok((90, 99)));
        assert_eq!(resolve_range(r(100, None), 100), Err(RangeError));
        assert_eq!(resolve_range(ByteRange::Suffix(0), 100), Err(RangeError));
    }
}
