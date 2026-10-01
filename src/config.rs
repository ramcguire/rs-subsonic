//! TOML configuration with `RSUB_<SECTION>_<KEY>` environment overrides.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_PATH: &str = "rs-subsonic.toml";

/// Top-level tables that environment variables may override.
const ENV_SECTIONS: &[&str] = &["server", "database", "auth", "cache", "analysis"];

// Every table denies unknown fields, so a misspelt key (say
// `allow_plain_text`) stops startup instead of silently keeping the default.
// `Serialize` gives `apply_env` the type of each field.

#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub cache: CacheConfig,
    pub analysis: AnalysisConfig,
    #[serde(rename = "backend")]
    pub backends: Vec<BackendConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: String,
    pub data_dir: PathBuf,
    /// Tokio worker threads; 0 = one per CPU.
    pub worker_threads: usize,
    /// `tracing` filter, overridden by `RUST_LOG`.
    pub log: String,
    /// Space-separated articles ignored when sorting and indexing.
    pub ignored_articles: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            listen: "0.0.0.0:4533".into(),
            data_dir: "./data".into(),
            worker_threads: 0,
            log: "info".into(),
            ignored_articles: "The El La Los Las Le Les".into(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Cover-art disk cache size, e.g. `"500MB"`; `"0"` disables it.
    pub covers_max: String,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            covers_max: "500MB".into(),
        }
    }
}

/// Local audio analysis for sonic similarity (`getSonicSimilarTracks`,
/// `findSonicPath`), which needs a build with the `bliss` or `clap` feature.
/// Reads the mounted library through each backend's `path_map`, and needs
/// `read_tags`: the tag pass stamps the files. `rsub_analysis::Settings`
/// checks the analyzer settings.
///
/// Changing the analyzer, `clap_crops` or `bliss_seconds` analyses the
/// library again; vectors from other settings are kept, so going back is free.
#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalysisConfig {
    pub enabled: bool,
    /// `clap` or `bliss`; defaults to the analyzer built in, `clap` when
    /// both are.
    pub analyzer: Option<String>,
    /// Files analysed at once, each on one core; 0 = half the CPUs.
    pub threads: usize,
    /// How often to look for files to analyse besides after each sync.
    pub interval: String,
    /// CLAP: the audio model exported to ONNX (`tools/export_clap.py`).
    pub clap_model: Option<PathBuf>,
    /// CLAP: 10 s crops embedded per track, 1 to 4. The time per track is
    /// about linear in them.
    pub clap_crops: usize,
    /// bliss: analyse only this many seconds from the middle of each track
    /// (at least 10); 0 analyses all of it.
    pub bliss_seconds: u32,
}

impl Default for AnalysisConfig {
    fn default() -> Self {
        AnalysisConfig {
            enabled: false,
            analyzer: None,
            threads: 0,
            interval: "1h".into(),
            clap_model: None,
            clap_crops: 4,
            bliss_seconds: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Plex,
}

impl BackendKind {
    /// The kind as the config and the `sources` table spell it.
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Plex => "plex",
        }
    }
}

/// One `[[backend]]` table.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// Stable name; the catalog is keyed on it, so don't rename it casually.
    pub id: String,
    pub kind: BackendKind,
    pub url: String,
    /// Name of the environment variable holding the token (default `PLEX_TOKEN`).
    #[serde(default)]
    pub token_env: Option<String>,
    /// The token itself. Prefer `token_env` to keep secrets out of files.
    #[serde(default)]
    pub token: Option<String>,
    /// Library names (or keys) to mirror; empty means all music libraries.
    #[serde(default)]
    pub libraries: Vec<String>,
    /// Items per catalog page.
    #[serde(default = "default_page_size")]
    pub page_size: u64,
    #[serde(default)]
    pub sync: SyncConfig,
    #[serde(default)]
    pub path_map: Vec<PathMapConfig>,
    #[serde(default)]
    pub local: LocalConfig,
}

fn default_page_size() -> u64 {
    500
}

impl BackendConfig {
    pub fn resolve_token(&self) -> Result<String, String> {
        let var = self.token_env.as_deref().unwrap_or("PLEX_TOKEN");
        match (std::env::var(var).ok(), &self.token) {
            (Some(t), _) if !t.trim().is_empty() => Ok(t.trim().to_owned()),
            (_, Some(t)) if !t.trim().is_empty() => Ok(t.trim().to_owned()),
            _ => Err(format!(
                "backend '{}': no token (set the {var} environment variable)",
                self.id
            )),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// How often to run a full sync, e.g. `"24h"`, `"90m"`.
    pub full_interval: String,
    /// How often to ask the backend whether a library changed, and sync what
    /// changed if so; `"0"` turns it off.
    pub check_interval: String,
    /// Fetch per-track detail (replay gain, bit depth, moods, lyrics flag).
    pub enrich: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig {
            full_interval: "24h".into(),
            check_interval: "5m".into(),
            enrich: true,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PathMapConfig {
    /// Path prefix as the backend reports it.
    pub remote: String,
    /// Where that prefix is mounted here.
    pub local: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// Serve mapped files directly instead of streaming through the backend.
    pub serve_files: bool,
    /// Read tags from mapped files: the source of MBIDs for stable ids. Needs
    /// the `local-tags` feature.
    pub read_tags: bool,
}

impl Default for LocalConfig {
    fn default() -> Self {
        LocalConfig {
            serve_files: true,
            read_tags: true,
        }
    }
}

/// `"30s"`, `"10m"`, `"24h"`, `"7d"` or plain seconds.
pub fn parse_duration(s: &str) -> Result<std::time::Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("invalid duration '{s}'"))?;
    let mult = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("invalid duration unit in '{s}' (use s, m, h or d)")),
    };
    Ok(std::time::Duration::from_secs(n.saturating_mul(mult)))
}

/// `"500MB"`, `"2GB"`, `"800KB"` or plain bytes.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().map_err(|_| format!("invalid size '{s}'"))?;
    let mult = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" | "K" => 1 << 10,
        "MB" | "M" => 1 << 20,
        "GB" | "G" => 1 << 30,
        _ => return Err(format!("invalid size unit in '{s}' (use KB, MB or GB)")),
    };
    Ok(n.saturating_mul(mult))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Defaults to `sqlite://<data_dir>/rsub.db`.
    pub url: Option<String>,
    pub max_connections: u32,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        DatabaseConfig {
            url: None,
            max_connections: 4,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub allow_plaintext: bool,
    pub allow_token_auth: bool,
    /// Defaults to `<data_dir>/secret.key`. `RSUB_SECRET_KEY` (hex) takes precedence.
    pub secret_key_file: Option<PathBuf>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig {
            allow_plaintext: true,
            allow_token_auth: true,
            secret_key_file: None,
        }
    }
}

impl Config {
    /// Load `path` (or the default path if it exists), then apply env overrides.
    pub fn load(path: Option<&Path>) -> Result<Config, String> {
        let mut table = match path {
            Some(p) => read_table(p)?,
            None if Path::new(DEFAULT_PATH).exists() => read_table(Path::new(DEFAULT_PATH))?,
            None => toml::Table::new(),
        };
        apply_env(&mut table, std::env::vars());
        table
            .try_into()
            .map_err(|e| format!("invalid configuration: {e}"))
    }

    pub fn database_url(&self) -> String {
        self.database.url.clone().unwrap_or_else(|| {
            let path = self.server.data_dir.join("rsub.db");
            format!("sqlite://{}", path.to_string_lossy().replace('\\', "/"))
        })
    }

    pub fn secret_key_file(&self) -> PathBuf {
        self.auth
            .secret_key_file
            .clone()
            .unwrap_or_else(|| self.server.data_dir.join("secret.key"))
    }
}

fn read_table(path: &Path) -> Result<toml::Table, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.parse().map_err(|e| format!("{}: {e}", path.display()))
}

/// `RSUB_SERVER_DATA_DIR=/x` sets `server.data_dir = "/x"`. A value is an
/// integer or bool only where the field is one, so `RSUB_CACHE_COVERS_MAX=0`
/// stays the string `"0"`.
fn apply_env(table: &mut toml::Table, vars: impl Iterator<Item = (String, String)>) {
    // The defaults, for each field's type. Unset options are left out; they
    // are all strings or paths.
    let template = toml::Value::try_from(Config::default()).unwrap_or(toml::Value::Boolean(false));
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix("RSUB_") else {
            continue;
        };
        let rest = rest.to_ascii_lowercase();
        let Some((section, key)) = rest.split_once('_') else {
            continue;
        };
        if !ENV_SECTIONS.contains(&section) {
            continue;
        }
        // A malformed number or bool stays a string, for serde to reject
        // with the field's name.
        let value = match template.get(section).and_then(|s| s.get(key)) {
            Some(toml::Value::Integer(_)) => value
                .trim()
                .parse::<i64>()
                .map_or(toml::Value::String(value), toml::Value::Integer),
            Some(toml::Value::Boolean(_)) => value
                .trim()
                .parse::<bool>()
                .map_or(toml::Value::String(value), toml::Value::Boolean),
            _ => toml::Value::String(value),
        };
        let entry = table
            .entry(section)
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(t) = entry {
            t.insert(key.to_owned(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_overrides_file() {
        let mut table: toml::Table =
            "[server]\nlisten = \"127.0.0.1:1\"\n[database]\nmax_connections = 2\n"
                .parse()
                .unwrap();
        let vars = [
            ("RSUB_SERVER_LISTEN", "0.0.0.0:9"),
            ("RSUB_SERVER_DATA_DIR", "/srv/rsub"),
            ("RSUB_AUTH_ALLOW_PLAINTEXT", "false"),
            ("RSUB_DATABASE_MAX_CONNECTIONS", "8"),
            // Numeric values for string fields stay strings.
            ("RSUB_CACHE_COVERS_MAX", "0"),
            ("RSUB_ANALYSIS_INTERVAL", "3600"),
            ("RSUB_SERVER_LOG", "1"),
            ("RSUB_SECRET_KEY", "ignored"),
            ("HOME", "ignored"),
        ];
        apply_env(
            &mut table,
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())),
        );
        let cfg: Config = table.try_into().unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:9");
        assert_eq!(cfg.server.data_dir, PathBuf::from("/srv/rsub"));
        assert!(!cfg.auth.allow_plaintext);
        assert!(cfg.auth.allow_token_auth);
        assert_eq!(cfg.database.max_connections, 8);
        assert_eq!(cfg.database_url(), "sqlite:///srv/rsub/rsub.db");
        assert_eq!(parse_size(&cfg.cache.covers_max).unwrap(), 0);
        assert_eq!(
            parse_duration(&cfg.analysis.interval).unwrap().as_secs(),
            3600
        );
        assert_eq!(cfg.server.log, "1");

        // A malformed number is reported against its field.
        let mut table = toml::Table::new();
        apply_env(
            &mut table,
            [(
                "RSUB_DATABASE_MAX_CONNECTIONS".to_owned(),
                "lots".to_owned(),
            )]
            .into_iter(),
        );
        let r: Result<Config, _> = table.try_into();
        let err = r.unwrap_err().to_string();
        assert!(err.contains("max_connections"), "{err}");
    }

    #[test]
    fn misspelt_keys_are_errors() {
        for text in [
            "[auth]\nallow_plain_text = false\n",
            "[server]\nlistn = \"0.0.0.0:1\"\n",
            "[database]\nurl2 = \"x\"\n",
            "[cache]\ncover_max = \"1MB\"\n",
            "[analysys]\nenabled = true\n",
        ] {
            let r: Result<Config, _> = text.parse::<toml::Table>().unwrap().try_into();
            assert!(r.is_err(), "{text}");
        }
        for path in ["rs-subsonic.example.toml", "rs-subsonic.toml"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
            if path.exists() {
                let table = read_table(&path).unwrap();
                let r: Result<Config, _> = table.try_into();
                assert!(r.is_ok(), "{}: {:?}", path.display(), r.err());
            }
        }
    }

    #[test]
    fn sync_defaults() {
        let cfg: Config = "[[backend]]\nid = \"x\"\nkind = \"plex\"\nurl = \"u\"\n"
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        let s = &cfg.backends[0].sync;
        assert_eq!(parse_duration(&s.check_interval).unwrap().as_secs(), 300);
        assert_eq!(parse_duration(&s.full_interval).unwrap().as_secs(), 86_400);
    }

    #[test]
    fn backends() {
        let cfg: Config = r#"
            [[backend]]
            id = "home"
            kind = "plex"
            url = "http://plex:32400"
            token = "abc"
            libraries = ["Music"]
              [backend.sync]
              full_interval = "90m"
              check_interval = "0"
              [[backend.path_map]]
              remote = "/data/music"
              local = "/mnt/music"
        "#
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap();
        let b = &cfg.backends[0];
        assert_eq!(b.kind, BackendKind::Plex);
        assert_eq!(b.page_size, 500);
        assert!(b.sync.enrich && b.local.serve_files && b.local.read_tags);
        assert_eq!(b.path_map[0].local, PathBuf::from("/mnt/music"));
        assert_eq!(
            parse_duration(&b.sync.full_interval).unwrap().as_secs(),
            5400
        );
        assert!(parse_duration(&b.sync.check_interval).unwrap().is_zero());
        assert!(parse_duration("5x").is_err());
        assert_eq!(parse_size("500MB").unwrap(), 500 << 20);
        assert_eq!(parse_size("0").unwrap(), 0);
        assert!(parse_size("1TB").is_err());

        assert!(!cfg.analysis.enabled);

        let typo: Result<Config, _> =
            "[[backend]]\nid = \"x\"\nkind = \"plex\"\nurl = \"u\"\ntokn = \"t\"\n"
                .parse::<toml::Table>()
                .unwrap()
                .try_into();
        assert!(typo.is_err());
    }
}
