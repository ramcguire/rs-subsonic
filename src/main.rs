mod config;

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use argh::FromArgs;
use rsub_core::Roles;
use rsub_core::crypto::{self, Purpose, SecretBox};
use rsub_core::tags::TagReader;
use rsub_core::text::IgnoredArticles;
use rsub_media::{CoverCache, PathMapper};
use rsub_plex::{PlexBackend, PlexConfig};
use rsub_server::{AppState, AuthOptions, SourceRuntime};
use rsub_store::{Db, NewUser, StoreError, UserUpdate};
use rsub_sync::backup;
use rsub_sync::snapshot::{self, SNAPSHOT_FILE};
use rsub_sync::{SyncEngine, SyncSource};

use config::{BackendConfig, BackendKind, Config};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type Error = Box<dyn std::error::Error + Send + Sync>;

const KEY_CHECK_SETTING: &str = "key_check";
const KEY_CHECK_PLAINTEXT: &[u8] = b"rs-subsonic";

/// OpenSubsonic-compatible music server backed by Plex.
#[derive(FromArgs)]
struct Cli {
    /// path to the TOML config file (default: ./rs-subsonic.toml if present)
    #[argh(option, short = 'c')]
    config: Option<PathBuf>,
    /// print the version and exit
    #[argh(switch, short = 'V')]
    version: bool,
    #[argh(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum Cmd {
    Serve(ServeCmd),
    User(UserCmd),
    Ids(IdsCmd),
    Backup(BackupCmd),
}

/// Run the server (the default).
#[derive(FromArgs)]
#[argh(subcommand, name = "serve")]
struct ServeCmd {}

/// Manage users.
#[derive(FromArgs)]
#[argh(subcommand, name = "user")]
struct UserCmd {
    #[argh(subcommand)]
    cmd: UserSub,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum UserSub {
    Add(UserAdd),
    List(UserList),
    Update(UserUpdateCmd),
    Passwd(UserPasswd),
    Delete(UserDelete),
    ApiKey(UserApiKey),
    ApiKeys(UserApiKeys),
    RevokeKey(UserRevokeKey),
}

/// Back up or restore what only the database holds: users, API keys, local
/// ratings and the identity ledger (a sync rebuilds the catalog).
#[derive(FromArgs)]
#[argh(subcommand, name = "backup")]
struct BackupCmd {
    #[argh(subcommand)]
    cmd: BackupSub,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum BackupSub {
    Export(BackupExport),
    Restore(BackupRestore),
}

/// Write a backup as JSON lines, to stdout unless --output is given.
/// Passwords stay encrypted: keep the server key (secret.key) too.
#[derive(FromArgs)]
#[argh(subcommand, name = "export")]
struct BackupExport {
    /// file to write
    #[argh(option, short = 'o')]
    output: Option<PathBuf>,
}

/// Merge a backup into this database. Users it already has are left as
/// they are; ledger keys it has keep their ids. Safe to repeat: after the
/// first sync, again to add ratings of artists that weren't synced yet.
#[derive(FromArgs)]
#[argh(subcommand, name = "restore")]
struct BackupRestore {
    /// file to read (default: stdin)
    #[argh(positional)]
    input: Option<PathBuf>,
}

/// Export or import the identity ledger, which keeps public ids stable
/// (for example to move from SQLite to Postgres).
#[derive(FromArgs)]
#[argh(subcommand, name = "ids")]
struct IdsCmd {
    #[argh(subcommand)]
    cmd: IdsSub,
}

#[derive(FromArgs)]
#[argh(subcommand)]
enum IdsSub {
    Export(IdsExport),
    Import(IdsImport),
}

/// Write the identity ledger as JSON lines, to stdout unless --output is given.
#[derive(FromArgs)]
#[argh(subcommand, name = "export")]
struct IdsExport {
    /// file to write
    #[argh(option, short = 'o')]
    output: Option<PathBuf>,
}

/// Add an exported identity ledger to this database. Keys it already has
/// keep their ids.
#[derive(FromArgs)]
#[argh(subcommand, name = "import")]
struct IdsImport {
    /// file to read (default: stdin)
    #[argh(positional)]
    input: Option<PathBuf>,
}

/// Create a user. The password is read from stdin unless --password is given.
#[derive(FromArgs)]
#[argh(subcommand, name = "add")]
struct UserAdd {
    /// username
    #[argh(positional)]
    username: String,
    /// password (prefer stdin to keep it out of shell history)
    #[argh(option)]
    password: Option<String>,
    /// email address
    #[argh(option)]
    email: Option<String>,
    /// grant all roles including admin
    #[argh(switch)]
    admin: bool,
    /// comma-separated roles instead of a regular user's (see `user list`)
    #[argh(option)]
    roles: Option<String>,
    /// maximum streaming bitrate in kbps (0 = unlimited)
    #[argh(option, default = "0")]
    max_bitrate: u32,
}

/// List users.
#[derive(FromArgs)]
#[argh(subcommand, name = "list")]
struct UserList {}

/// Change a user's email, roles or bitrate limit.
#[derive(FromArgs)]
#[argh(subcommand, name = "update")]
struct UserUpdateCmd {
    /// username
    #[argh(positional)]
    username: String,
    /// email address ("" clears it)
    #[argh(option)]
    email: Option<String>,
    /// make the user an admin (true) or not (false)
    #[argh(option)]
    admin: Option<bool>,
    /// comma-separated roles, replacing theirs
    #[argh(option)]
    roles: Option<String>,
    /// maximum streaming bitrate in kbps (0 = unlimited)
    #[argh(option)]
    max_bitrate: Option<u32>,
}

/// Set a user's password, read from stdin unless --password is given.
#[derive(FromArgs)]
#[argh(subcommand, name = "passwd")]
struct UserPasswd {
    /// username
    #[argh(positional)]
    username: String,
    /// the new password (prefer stdin to keep it out of shell history)
    #[argh(option)]
    password: Option<String>,
}

/// Delete a user with their API keys and local ratings.
#[derive(FromArgs)]
#[argh(subcommand, name = "delete")]
struct UserDelete {
    /// username
    #[argh(positional)]
    username: String,
}

/// List a user's API keys: ids and names (the keys aren't stored).
#[derive(FromArgs)]
#[argh(subcommand, name = "api-keys")]
struct UserApiKeys {
    /// username
    #[argh(positional)]
    username: String,
}

/// Revoke one of a user's API keys.
#[derive(FromArgs)]
#[argh(subcommand, name = "revoke-key")]
struct UserRevokeKey {
    /// username
    #[argh(positional)]
    username: String,
    /// the key's id (see `user api-keys`)
    #[argh(positional)]
    id: i64,
}

/// Create an OpenSubsonic API key for a user and print it once.
#[derive(FromArgs)]
#[argh(subcommand, name = "api-key")]
struct UserApiKey {
    /// username
    #[argh(positional)]
    username: String,
    /// label for the key
    #[argh(option, default = "String::from(\"default\")")]
    name: String,
}

fn main() -> ExitCode {
    let cli: Cli = argh::from_env();
    if cli.version {
        println!("rs-subsonic {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let config = match Config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    init_tracing(&config.server.log);

    let mut rt = tokio::runtime::Builder::new_multi_thread();
    if config.server.worker_threads > 0 {
        rt.worker_threads(config.server.worker_threads);
    }
    let rt = match rt.enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rt.block_on(run(cli.cmd.unwrap_or(Cmd::Serve(ServeCmd {})), config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(default_filter: &str) {
    use tracing_subscriber::EnvFilter;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn run(cmd: Cmd, config: Config) -> Result<(), Error> {
    std::fs::create_dir_all(&config.server.data_dir).map_err(|e| {
        format!(
            "cannot create data dir {}: {e}",
            config.server.data_dir.display()
        )
    })?;
    let db = Db::connect(&config.database_url(), config.database.max_connections).await?;
    db.migrate().await?;
    let secrets = Arc::new(load_secrets(&config, &db).await?);

    let result = match cmd {
        Cmd::Serve(_) => serve(config, db.clone(), secrets).await,
        Cmd::User(u) => user_cmd(u.cmd, &db, &secrets).await,
        Cmd::Ids(i) => ids_cmd(i.cmd, &db).await,
        Cmd::Backup(b) => backup_cmd(b.cmd, &db, &secrets).await,
    };
    db.close().await;
    result
}

/// Load the server key and make sure it is the key this database was set up with.
async fn load_secrets(config: &Config, db: &Db) -> Result<SecretBox, Error> {
    let env_key = std::env::var("RSUB_SECRET_KEY").ok();
    let secrets = SecretBox::load_or_create(env_key.as_deref(), &config.secret_key_file())?;
    match db.get_setting(KEY_CHECK_SETTING).await? {
        Some(check) => {
            let ok = hex::decode(check)
                .ok()
                .and_then(|ct| secrets.decrypt(Purpose::KeyCheck, "", &ct).ok())
                .is_some_and(|pt| pt == KEY_CHECK_PLAINTEXT);
            if !ok {
                return Err("the server secret key does not match this database \
                            (check RSUB_SECRET_KEY / auth.secret_key_file)"
                    .into());
            }
        }
        None => {
            let ct = secrets.encrypt(Purpose::KeyCheck, "", KEY_CHECK_PLAINTEXT)?;
            db.set_setting(KEY_CHECK_SETTING, &hex::encode(ct)).await?;
        }
    }
    Ok(secrets)
}

/// Stable identifier this bridge presents to Plex (`X-Plex-Client-Identifier`).
async fn plex_client_id(db: &Db) -> Result<String, Error> {
    const KEY: &str = "plex_client_id";
    if let Some(id) = db.get_setting(KEY).await? {
        return Ok(id);
    }
    let id = format!("rs-subsonic-{}", hex::encode(crypto::random_bytes::<12>()?));
    db.set_setting(KEY, &id).await?;
    Ok(id)
}

/// Warn about `path_map` folders that aren't reachable, which otherwise shows
/// only as every file being unavailable.
fn check_roots(backend: &str, paths: &PathMapper) {
    for root in paths.roots() {
        match std::fs::metadata(root) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                tracing::warn!(backend, root = %root.display(), "path_map local path is not a folder")
            }
            Err(e) => {
                tracing::warn!(backend, root = %root.display(), "path_map local folder unreachable: {e}")
            }
        }
    }
}

/// The tag reader for a backend's mounted library, if tags are to be read.
fn tag_reader(b: &BackendConfig, paths: &PathMapper) -> Option<Arc<dyn TagReader>> {
    if !b.local.read_tags {
        return None;
    }
    if paths.is_empty() {
        tracing::warn!(
            backend = %b.id,
            "no path_map: file tags (MBIDs) can't be read, so ids rest on Plex album guids and names"
        );
        return None;
    }
    #[cfg(feature = "local-tags")]
    return Some(Arc::new(rsub_media::LocalTagReader::new(paths.clone())));
    #[cfg(not(feature = "local-tags"))]
    {
        tracing::warn!(backend = %b.id, "built without `local-tags`: file tags are not read");
        None
    }
}

/// Start the analysis engine when analysis is on.
#[cfg(any(feature = "bliss", feature = "clap"))]
fn start_analysis(
    config: &Config,
    db: &Db,
    b: &Backends,
) -> Result<Option<Arc<rsub_analysis::AnalysisEngine>>, Error> {
    let cfg = &config.analysis;
    if !cfg.enabled {
        return Ok(None);
    }
    let settings = || {
        rsub_analysis::Settings {
            analyzer: cfg.analyzer.as_deref().map(str::parse).transpose()?,
            clap_model: cfg.clap_model.clone(),
            clap_crops: cfg.clap_crops,
            bliss_seconds: cfg.bliss_seconds,
        }
        .build()
    };
    let analyzer = settings().map_err(|e| format!("[analysis]: {e}"))?;
    let paths: HashMap<i64, PathMapper> = b
        .runtimes
        .iter()
        .filter(|(_, r)| !r.paths.is_empty())
        .map(|(id, r)| (*id, r.paths.clone()))
        .collect();
    if paths.is_empty() {
        tracing::warn!("analysis is on, but no backend has a path_map: nothing can be analysed");
    }
    if config.backends.iter().any(|b| !b.local.read_tags) {
        tracing::warn!("analysis needs read_tags: files of backends without it aren't analysed");
    }
    let threads = match cfg.threads {
        0 => std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)),
        n => n,
    };
    let interval = config::parse_duration(&cfg.interval)?.max(Duration::from_secs(60));
    tracing::info!(analyzer = analyzer.id(), threads, "audio analysis on");
    let engine = rsub_analysis::AnalysisEngine::new(db.clone(), analyzer, paths, threads);
    if let Some(sync) = &b.sync {
        let e = engine.clone();
        sync.on_synced(move || e.trigger());
    }
    tokio::spawn(engine.clone().run(interval));
    Ok(Some(engine))
}

/// The configured backends, with the sync engine that feeds the catalog.
struct Backends {
    runtimes: HashMap<i64, SourceRuntime>,
    sync: Option<Arc<SyncEngine>>,
    /// Shortest `full_interval`.
    interval: Duration,
    /// Shortest `check_interval` that isn't off.
    check: Option<Duration>,
}

async fn backends(config: &Config, db: &Db, articles: &IgnoredArticles) -> Result<Backends, Error> {
    let mut runtimes = HashMap::new();
    let mut sync_sources = Vec::new();
    let mut interval = Duration::MAX;
    let mut check: Option<Duration> = None;
    let mut seen = std::collections::HashSet::new();
    for b in &config.backends {
        if !seen.insert(b.id.as_str()) {
            return Err(format!("duplicate backend id '{}'", b.id).into());
        }
        interval = interval
            .min(config::parse_duration(&b.sync.full_interval)?.max(Duration::from_secs(60)));
        let c = config::parse_duration(&b.sync.check_interval)?;
        if !c.is_zero() {
            let c = c.max(rsub_sync::SETTLE);
            check = Some(check.map_or(c, |prev| prev.min(c)));
        }
        let source_id = db.ensure_source(&b.id, b.kind.name()).await?;
        let backend = match b.kind {
            BackendKind::Plex => {
                let plex = PlexBackend::new(
                    &b.id,
                    &PlexConfig {
                        url: b.url.clone(),
                        token: b.resolve_token()?,
                        client_id: plex_client_id(db).await?,
                        page_size: b.page_size,
                    },
                )?;
                // Reachability check only: sync retries on its own if Plex is down.
                match plex.identity().await {
                    Ok((machine, version)) => {
                        tracing::info!(backend = %b.id, %machine, %version, "connected to Plex")
                    }
                    Err(e) => tracing::warn!(backend = %b.id, "Plex not reachable yet: {e}"),
                }
                plex
            }
        };
        let handle = backend.handle();
        let paths = PathMapper::new(
            b.path_map
                .iter()
                .map(|m| (m.remote.clone(), m.local.clone())),
        );
        check_roots(&b.id, &paths);
        sync_sources.push(SyncSource {
            id: source_id,
            name: b.id.clone(),
            catalog: handle.catalog.clone(),
            libraries: b.libraries.clone(),
            enrich: b.sync.enrich,
            tags: tag_reader(b, &paths),
        });
        runtimes.insert(
            source_id,
            SourceRuntime {
                backend: handle,
                paths,
                serve_local: b.local.serve_files && !b.path_map.is_empty(),
            },
        );
    }
    let configured: Vec<i64> = runtimes.keys().copied().collect();
    let retired = db.retire_sources(&configured).await?;
    if retired > 0 {
        tracing::info!(
            libraries = retired,
            "hid the libraries of backends no longer configured"
        );
    }
    let snapshot = config.server.data_dir.join(SNAPSHOT_FILE);
    let sync = (!sync_sources.is_empty())
        .then(|| SyncEngine::new(db.clone(), sync_sources, articles.clone(), Some(snapshot)));
    Ok(Backends {
        runtimes,
        sync,
        interval,
        check,
    })
}

async fn serve(config: Config, db: Db, secrets: Arc<SecretBox>) -> Result<(), Error> {
    if db.count_users().await? == 0 {
        tracing::warn!("no users yet; create one with `rs-subsonic user add <name> --admin`");
    }
    restore_ids(&config, &db).await?;
    let articles = IgnoredArticles::new(&config.server.ignored_articles);
    let b = backends(&config, &db, &articles).await?;
    #[cfg(any(feature = "bliss", feature = "clap"))]
    let analysis = start_analysis(&config, &db, &b)?;
    #[cfg(not(any(feature = "bliss", feature = "clap")))]
    if config.analysis.enabled {
        return Err("analysis.enabled needs a build with the `bliss` or `clap` feature".into());
    }
    match &b.sync {
        Some(engine) => {
            tokio::spawn(engine.clone().run(b.interval, b.check));
        }
        None => tracing::warn!("no [[backend]] configured; the catalog stays empty"),
    }
    let covers_max = config::parse_size(&config.cache.covers_max)?;
    let covers = if covers_max > 0 {
        let dir = config.server.data_dir.join("cache").join("covers");
        Some(Arc::new(CoverCache::open(dir, covers_max).await?))
    } else {
        None
    };

    let mut state = AppState::new(
        db,
        secrets,
        AuthOptions {
            allow_plaintext: config.auth.allow_plaintext,
            allow_token_auth: config.auth.allow_token_auth,
        },
    );
    state.sources = Arc::new(b.runtimes);
    state.sync = b.sync;
    state.covers = covers;
    state.articles = Arc::new(articles);
    #[cfg(any(feature = "bliss", feature = "clap"))]
    {
        state.sonic = analysis.as_ref().map(|e| e.search());
        state.analysis = analysis;
    }
    let listener = tokio::net::TcpListener::bind(&config.server.listen)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.server.listen))?;
    tracing::info!(addr = %listener.local_addr()?, db = state.db.dialect(), "rs-subsonic listening");
    let stop = Arc::new(tokio::sync::Notify::new());
    let app =
        rsub_server::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = axum::serve(listener, app).with_graceful_shutdown({
        let stop = stop.clone();
        async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            stop.notify_one();
        }
    });
    // Open streams can run for minutes: give requests a while to finish, then
    // exit anyway.
    tokio::select! {
        r = server => r?,
        () = async {
            stop.notified().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!(
            "requests still open after {}s; exiting",
            SHUTDOWN_GRACE.as_secs()
        ),
    }
    Ok(())
}

/// How long shutdown waits for in-flight requests.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Ctrl-C, or SIGTERM from Docker or systemd.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!("cannot listen for SIGTERM: {e}"),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Restore the identity snapshot into a new or rebuilt database before the
/// first sync, so the sync finds the ids it had. A snapshot that can't be read
/// stops startup: syncing without it would mint new ids and then overwrite it.
async fn restore_ids(config: &Config, db: &Db) -> Result<(), Error> {
    let path = config.server.data_dir.join(SNAPSHOT_FILE);
    match snapshot::restore_snapshot(db, &path).await {
        Ok(Some(i)) => tracing::info!(
            entries = i.read,
            path = %path.display(),
            "restored the identity ledger from its snapshot"
        ),
        Ok(None) => {}
        Err(e) => {
            return Err(format!(
                "cannot restore the identity snapshot {}: {e} \
                 (move it aside to start without it; ids may then change)",
                path.display()
            )
            .into());
        }
    }
    Ok(())
}

async fn ids_cmd(cmd: IdsSub, db: &Db) -> Result<(), Error> {
    match cmd {
        IdsSub::Export(e) => {
            let n = match &e.output {
                Some(path) => {
                    let mut file = tokio::fs::File::create(path).await?;
                    let n = snapshot::export(db, &mut file).await?;
                    file.sync_all().await?;
                    n
                }
                None => snapshot::export(db, tokio::io::stdout()).await?,
            };
            eprintln!("exported {n} identity keys");
        }
        IdsSub::Import(i) => {
            let imported = match &i.input {
                Some(path) => {
                    let file = tokio::fs::File::open(path)
                        .await
                        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
                    snapshot::import(db, tokio::io::BufReader::new(file)).await?
                }
                None => snapshot::import(db, tokio::io::BufReader::new(tokio::io::stdin())).await?,
            };
            eprintln!(
                "imported {} of {} identity keys ({} already known)",
                imported.added,
                imported.read,
                imported.read - imported.added
            );
        }
    }
    Ok(())
}

async fn backup_cmd(cmd: BackupSub, db: &Db, secrets: &SecretBox) -> Result<(), Error> {
    match cmd {
        BackupSub::Export(e) => {
            let n = match &e.output {
                Some(path) => {
                    // It holds encrypted passwords and API-key hashes.
                    let file = crypto::create_private(path)
                        .map_err(|e| format!("{}: {e}", path.display()))?;
                    let mut file = tokio::fs::File::from_std(file);
                    let n = backup::export(db, &mut file).await?;
                    file.sync_all().await?;
                    n
                }
                None => backup::export(db, tokio::io::stdout()).await?,
            };
            eprintln!(
                "backed up {} users, {} API keys, {} ratings, {} settings and {} identity keys",
                n.users, n.api_keys, n.ratings, n.settings, n.identity_keys
            );
        }
        BackupSub::Restore(r) => {
            let restored = match &r.input {
                Some(path) => {
                    let file = tokio::fs::File::open(path)
                        .await
                        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
                    backup::restore(db, secrets, tokio::io::BufReader::new(file)).await?
                }
                None => {
                    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
                    backup::restore(db, secrets, stdin).await?
                }
            };
            let a = restored.accounts;
            eprintln!(
                "users: {} added, {} already here (kept as they are); API keys: {} added",
                a.users_added, a.users_kept, a.api_keys_added
            );
            eprintln!(
                "ratings: {} added, {} skipped; settings: {} added; identity keys: {} of {} added",
                a.ratings_added,
                a.ratings_skipped,
                restored.settings_added,
                restored.identity.added,
                restored.identity.read
            );
        }
    }
    Ok(())
}

/// Roles from a comma-separated list of names.
fn parse_roles(list: &str) -> Result<Roles, Error> {
    let names: Vec<&str> = list.split(',').filter(|n| !n.trim().is_empty()).collect();
    Ok(Roles::from_names(&names)?)
}

async fn find_user(db: &Db, username: &str) -> Result<rsub_core::User, Error> {
    Ok(db
        .user_by_username(username)
        .await?
        .ok_or_else(|| format!("no user '{username}'"))?)
}

/// `StoreError::LastAdmin` as advice.
fn last_admin(e: StoreError) -> Error {
    match e {
        StoreError::LastAdmin => "that would leave no admin: make another user admin first".into(),
        e => e.into(),
    }
}

async fn user_cmd(cmd: UserSub, db: &Db, secrets: &SecretBox) -> Result<(), Error> {
    match cmd {
        UserSub::Add(a) => {
            if a.username.trim().is_empty() || a.username != a.username.trim() {
                return Err("a username must be non-empty, without surrounding spaces".into());
            }
            let password = match a.password {
                Some(p) => p,
                None => read_password()?,
            };
            if password.is_empty() {
                return Err("password must not be empty".into());
            }
            let password_enc =
                secrets.encrypt(Purpose::Password, &a.username, password.as_bytes())?;
            let roles = match (&a.roles, a.admin) {
                (Some(list), admin) => {
                    let r = parse_roles(list)?;
                    if admin { r.with_admin(true) } else { r }
                }
                (None, true) => Roles::ALL,
                (None, false) => Roles::USER_DEFAULT,
            };
            let new = NewUser {
                username: &a.username,
                password_enc,
                email: a.email.as_deref().filter(|e| !e.trim().is_empty()),
                roles,
                max_bitrate: a.max_bitrate,
            };
            match db.create_user(new).await {
                Ok(id) => println!(
                    "created user '{}' (id {id}{})",
                    a.username,
                    if roles.contains(Roles::ADMIN) {
                        ", admin"
                    } else {
                        ""
                    }
                ),
                Err(StoreError::Conflict(_)) => {
                    return Err(format!("user '{}' already exists", a.username).into());
                }
                Err(e) => return Err(e.into()),
            }
        }
        UserSub::List(_) => {
            let users = db.list_users().await?;
            let width = users
                .iter()
                .map(|u| u.username.chars().count())
                .max()
                .unwrap_or(0)
                .max(8);
            println!(
                "{:>4}  {:<width$}  {:<5}  {:>7}  EMAIL  ROLES",
                "ID", "USERNAME", "ADMIN", "KBPS"
            );
            for u in users {
                let kbps = match u.max_bitrate {
                    0 => "-".to_owned(),
                    n => n.to_string(),
                };
                println!(
                    "{:>4}  {:<width$}  {:<5}  {:>7}  {}  {}",
                    u.id,
                    u.username,
                    if u.is_admin() { "yes" } else { "no" },
                    kbps,
                    u.email.as_deref().unwrap_or("-"),
                    u.roles.names().join(",")
                );
            }
        }
        UserSub::Update(c) => {
            let user = find_user(db, &c.username).await?;
            let mut roles = c.roles.as_deref().map(parse_roles).transpose()?;
            if let Some(admin) = c.admin {
                roles = Some(roles.unwrap_or(user.roles).with_admin(admin));
            }
            let change = UserUpdate {
                password_enc: None,
                email: c
                    .email
                    .map(|e| Some(e.trim().to_owned()).filter(|e| !e.is_empty())),
                roles,
                max_bitrate: c.max_bitrate,
            };
            db.update_user(user.id, change).await.map_err(last_admin)?;
            println!("updated user '{}'", user.username);
        }
        UserSub::Passwd(p) => {
            let user = find_user(db, &p.username).await?;
            let password = match p.password {
                Some(p) => p,
                None => read_password()?,
            };
            if password.is_empty() {
                return Err("password must not be empty".into());
            }
            let change = UserUpdate {
                password_enc: Some(secrets.encrypt(
                    Purpose::Password,
                    &user.username,
                    password.as_bytes(),
                )?),
                ..UserUpdate::default()
            };
            db.update_user(user.id, change).await?;
            println!("password of '{}' changed", user.username);
        }
        UserSub::Delete(d) => {
            let user = find_user(db, &d.username).await?;
            db.delete_user(user.id).await.map_err(last_admin)?;
            println!("deleted user '{}'", user.username);
        }
        UserSub::ApiKey(k) => {
            let user = find_user(db, &k.username).await?;
            let (key, hash) = crypto::new_api_key()?;
            db.create_api_key(user.id, &k.name, &hash).await?;
            println!("{key}");
            eprintln!(
                "API key '{}' created for '{}'. It is shown only once.",
                k.name, user.username
            );
        }
        UserSub::ApiKeys(k) => {
            let user = find_user(db, &k.username).await?;
            println!("{:>4}  {:<16}  CREATED (UTC)", "ID", "NAME");
            for key in db.api_keys(user.id).await? {
                println!(
                    "{:>4}  {:<16}  {}",
                    key.id,
                    key.name,
                    format_ms(key.created_at)
                );
            }
        }
        UserSub::RevokeKey(k) => {
            let user = find_user(db, &k.username).await?;
            if !db.delete_api_key(user.id, k.id).await? {
                return Err(format!("'{}' has no API key {}", user.username, k.id).into());
            }
            println!("revoked API key {} of '{}'", k.id, user.username);
        }
    }
    Ok(())
}

/// A millisecond timestamp as UTC `YYYY-MM-DD HH:MM`.
fn format_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

/// A password typed at the terminal without echo, or a line piped to stdin.
fn read_password() -> Result<String, Error> {
    if std::io::stdin().is_terminal() {
        return Ok(rpassword::prompt_password("password: ")?);
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}
