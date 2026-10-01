//! Sorting, searching and formatting helpers. Computed in Rust so ordering and
//! search behave the same on every database.

/// Articles ignored when sorting and indexing (`[server] ignored_articles`).
#[derive(Debug, Clone)]
pub struct IgnoredArticles {
    /// Space-separated, as reported in `getIndexes`.
    pub list: String,
    /// Lowercased, each with a trailing space.
    prefixes: Vec<String>,
}

impl IgnoredArticles {
    pub fn new(list: &str) -> Self {
        let prefixes = list
            .split_whitespace()
            .map(|a| format!("{} ", a.to_lowercase()))
            .collect();
        IgnoredArticles {
            list: list.split_whitespace().collect::<Vec<_>>().join(" "),
            prefixes,
        }
    }

    /// Sort key: transliterated, lowercased, leading article removed.
    /// `explicit` (e.g. Plex `titleSort`) wins over `name` when present.
    pub fn sort_key(&self, name: &str, explicit: Option<&str>) -> String {
        let base = normalize(explicit.filter(|s| !s.trim().is_empty()).unwrap_or(name));
        for p in &self.prefixes {
            if let Some(rest) = base.strip_prefix(p.as_str())
                && !rest.is_empty()
            {
                return rest.to_owned();
            }
        }
        base
    }
}

impl Default for IgnoredArticles {
    fn default() -> Self {
        Self::new("The El La Los Las Le Les")
    }
}

/// Transliterate to ASCII, lowercase and collapse whitespace.
pub fn normalize(s: &str) -> String {
    let ascii = deunicode::deunicode(s);
    let mut out = String::with_capacity(ascii.len());
    for word in ascii.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().map(|c| c.to_ascii_lowercase()));
    }
    out
}

/// Text matched by search: all given fields normalized and joined.
pub fn search_norm<'a>(fields: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for f in fields {
        let n = normalize(f);
        if n.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&n);
    }
    out
}

/// Search terms from a client query: normalized words with Subsonic's quoting
/// (`""` for "everything") and wildcard `*` removed. Empty means match all.
pub fn search_terms(query: &str) -> Vec<String> {
    let cleaned: String = query
        .chars()
        .map(|c| if c == '"' || c == '*' { ' ' } else { c })
        .collect();
    normalize(&cleaned)
        .split(' ')
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Index bucket for `getIndexes`/`getArtists`: an uppercase letter or `#`.
pub fn index_letter(sort_key: &str) -> char {
    match sort_key.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c.to_ascii_uppercase(),
        _ => '#',
    }
}

/// Cover-art version from the backend's thumb reference (FNV-1a).
pub fn thumb_version(thumb: &str) -> u32 {
    thumb.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// Epoch milliseconds as ISO 8601 UTC (`2024-01-02T03:04:05.000Z`).
pub fn iso8601(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        tod % 3600 / 60,
        tod % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// MIME type for an audio file suffix.
pub fn content_type(suffix: &str) -> &'static str {
    match suffix.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "m4b" | "mp4" | "aac" | "alac" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        "ape" => "audio/x-ape",
        "wv" => "audio/x-wavpack",
        "dsf" | "dff" => "audio/x-dsd",
        "mka" => "audio/x-matroska",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_keys() {
        let a = IgnoredArticles::default();
        assert_eq!(a.sort_key("The Beatles", None), "beatles");
        assert_eq!(a.sort_key("The", None), "the");
        assert_eq!(a.sort_key("Théâtre", None), "theatre");
        assert_eq!(a.sort_key("Beatles, The", Some("Beatles")), "beatles");
        assert_eq!(a.sort_key("  Los   Lobos ", None), "lobos");
        assert_eq!(a.list, "The El La Los Las Le Les");
    }

    #[test]
    fn search() {
        assert_eq!(search_norm(["Björk", "", "Homogenic"]), "bjork homogenic");
        assert_eq!(search_terms("\"\""), Vec::<String>::new());
        assert_eq!(search_terms("Björk  hom*"), ["bjork", "hom"]);
        assert_eq!(index_letter("beatles"), 'B');
        assert_eq!(index_letter("2pac"), '#');
    }

    #[test]
    fn iso() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(1_709_251_199_123), "2024-02-29T23:59:59.123Z");
        assert_eq!(iso8601(-1), "1969-12-31T23:59:59.999Z");
    }
}
