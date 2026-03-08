use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_FALLBACK_AFTER_MS: u64 = 1_500;
const DEFAULT_LANE_SLOTS: usize = 4;
const STALE_LEASE_AFTER_SECS: u64 = 12 * 60 * 60;
const LEASE_REFRESH_INTERVAL_SECS: u64 = 60;

pub fn main_entry() -> i32 {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cargo-sidestep: {err}");
            1
        }
    }
}

fn run() -> Result<i32, String> {
    let args = normalize_cli_args(env::args_os().skip(1).collect());
    if args.is_empty() || is_help(&args) {
        print_help();
        return Ok(0);
    }
    if is_version(&args) {
        println!("cargo-sidestep {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }

    let settings = Settings::from_env()?;
    let workspace = WorkspaceIdentity::discover(&env::current_dir().map_err(err_string)?, &args)?;
    let paths = WorkspacePaths::new(&settings, &workspace)?;
    let supports_build_dir = detect_build_dir_support(&settings.cargo_bin);

    let primary = ExecPlan::shared(&settings, &paths, supports_build_dir);
    match run_plan(&settings, &args, &primary)? {
        RunOutcome::Completed(code, _) => Ok(code),
        RunOutcome::Lock(kind) => {
            let lane = acquire_lane(&paths, settings.lane_slots)?;
            eprintln!(
                "cargo-sidestep: observed Cargo lock on {kind}; retrying in lane `{}`",
                lane.name
            );

            match kind {
                LockKind::BuildDirectory => {
                    rerun_from_build_lock(&settings, &args, &paths, &lane, supports_build_dir)
                }
                _ => rerun_from_home_lock(&settings, &args, &paths, &lane, supports_build_dir),
            }
        }
    }
}

fn normalize_cli_args(mut args: Vec<OsString>) -> Vec<OsString> {
    if matches!(args.first().and_then(|arg| arg.to_str()), Some("sidestep")) {
        args.remove(0);
    }
    args
}

fn rerun_from_build_lock(
    settings: &Settings,
    args: &[OsString],
    paths: &WorkspacePaths,
    lane: &LaneLease,
    supports_build_dir: bool,
) -> Result<i32, String> {
    let build_lane = ExecPlan::build_lane(settings, paths, lane, supports_build_dir);
    match run_plan(settings, args, &build_lane)? {
        RunOutcome::Completed(code, _) => Ok(code),
        RunOutcome::Lock(kind) if kind.is_home_lock() => {
            rerun_from_home_lock(settings, args, paths, lane, supports_build_dir)
        }
        RunOutcome::Lock(kind) => Err(format!(
            "lock persisted on {kind} even after moving to build lane `{}`",
            lane.name
        )),
    }
}

fn rerun_from_home_lock(
    settings: &Settings,
    args: &[OsString],
    paths: &WorkspacePaths,
    lane: &LaneLease,
    supports_build_dir: bool,
) -> Result<i32, String> {
    let offline_home = prepare_overlay_home(
        &settings.base_cargo_home,
        &lane.readonly_home_dir,
        OverlayMode::ReadonlyOffline,
    )?;
    let offline_plan = ExecPlan::readonly_overlay(paths, lane, &offline_home, supports_build_dir);
    match run_plan(settings, args, &offline_plan)? {
        RunOutcome::Completed(code, stderr) => {
            if code != 0 && stderr_looks_like_offline_miss(&stderr) {
                eprintln!(
                    "cargo-sidestep: readonly overlay missed the warm cache; retrying with an isolated online Cargo home"
                );
                rerun_online_overlay(settings, args, lane, supports_build_dir)
            } else {
                Ok(code)
            }
        }
        RunOutcome::Lock(kind) => {
            eprintln!(
                "cargo-sidestep: readonly overlay still saw {kind}; escalating to fully isolated Cargo home"
            );
            rerun_online_overlay(settings, args, lane, supports_build_dir)
        }
    }
}

fn rerun_online_overlay(
    settings: &Settings,
    args: &[OsString],
    lane: &LaneLease,
    supports_build_dir: bool,
) -> Result<i32, String> {
    let online_home = prepare_overlay_home(
        &settings.base_cargo_home,
        &lane.online_home_dir,
        OverlayMode::IsolatedOnline,
    )?;
    let plan = ExecPlan::online_overlay(lane, &online_home, supports_build_dir);
    match run_plan(settings, args, &plan)? {
        RunOutcome::Completed(code, _) => Ok(code),
        RunOutcome::Lock(kind) => Err(format!(
            "fully isolated retry still encountered a lock on {kind}"
        )),
    }
}

fn is_help(args: &[OsString]) -> bool {
    matches!(
        args.first().and_then(|arg| arg.to_str()),
        Some("-h" | "--help")
    )
}

fn is_version(args: &[OsString]) -> bool {
    matches!(
        args.first().and_then(|arg| arg.to_str()),
        Some("-V" | "--version" | "version")
    )
}

fn print_help() {
    println!(
        "\
cargo-sidestep {version}

USAGE:
    cargo-sidestep <cargo-subcommand> [args...]
    cargo sidestep <cargo-subcommand> [args...]

DESCRIPTION:
    Runs Cargo with a managed per-workspace cache root. If Cargo prints
    \"Blocking waiting for file lock ...\", the command is re-run in an isolated lane
    instead of waiting behind another Cargo process.

ENVIRONMENT:
    CARGO_SIDESTEP_STATE_DIR         Override the cache root used by cargo-sidestep
    CARGO_SIDESTEP_FALLBACK_AFTER_MS Milliseconds to wait after a lock message before rerouting
    CARGO_SIDESTEP_LANES             Number of reusable fallback lanes per workspace
    CARGO_SIDESTEP_CARGO_BIN         Override the underlying cargo executable (useful for tests)

NOTES:
    Cargo 1.91+ uses CARGO_BUILD_BUILD_DIR for lock-sensitive intermediates.
    Older Cargo versions fall back to isolated CARGO_TARGET_DIR lanes.
",
        version = env!("CARGO_PKG_VERSION")
    );
}

#[derive(Clone)]
struct Settings {
    cargo_bin: OsString,
    base_cargo_home: PathBuf,
    state_root: PathBuf,
    fallback_after: Duration,
    lane_slots: usize,
}

impl Settings {
    fn from_env() -> Result<Self, String> {
        let cargo_bin = env::var_os("CARGO_SIDESTEP_CARGO_BIN")
            .or_else(|| env::var_os("CARGO"))
            .unwrap_or_else(|| OsString::from("cargo"));
        let base_cargo_home = env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(default_cargo_home);
        let state_root = env::var_os("CARGO_SIDESTEP_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_state_root);
        let fallback_after = env::var("CARGO_SIDESTEP_FALLBACK_AFTER_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(DEFAULT_FALLBACK_AFTER_MS));
        let lane_slots = env::var("CARGO_SIDESTEP_LANES")
            .ok()
            .and_then(|raw| raw.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_LANE_SLOTS);

        Ok(Self {
            cargo_bin,
            base_cargo_home,
            state_root,
            fallback_after,
            lane_slots,
        })
    }
}

#[derive(Clone)]
struct WorkspaceIdentity {
    root: PathBuf,
    key: String,
}

impl WorkspaceIdentity {
    fn discover(cwd: &Path, args: &[OsString]) -> Result<Self, String> {
        let root = discover_workspace_root(cwd, args)?;
        let root = root.canonicalize().unwrap_or(root);
        let key = stable_hash(&root.to_string_lossy());
        Ok(Self { root, key })
    }
}

struct WorkspacePaths {
    shared_target_dir: PathBuf,
    shared_build_dir: PathBuf,
    lanes_dir: PathBuf,
}

impl WorkspacePaths {
    fn new(settings: &Settings, workspace: &WorkspaceIdentity) -> Result<Self, String> {
        let managed_root = settings.state_root.join("workspaces").join(&workspace.key);
        let shared_target_dir = managed_root.join("target");
        let shared_build_dir = managed_root.join("build").join("shared");
        let lanes_dir = managed_root.join("lanes");
        fs::create_dir_all(&shared_target_dir).map_err(err_string)?;
        fs::create_dir_all(&shared_build_dir).map_err(err_string)?;
        fs::create_dir_all(&lanes_dir).map_err(err_string)?;
        fs::write(
            managed_root.join("workspace-path.txt"),
            workspace.root.display().to_string(),
        )
        .map_err(err_string)?;
        Ok(Self {
            shared_target_dir,
            shared_build_dir,
            lanes_dir,
        })
    }
}

#[derive(Clone)]
struct ExecPlan {
    label: &'static str,
    cargo_home: PathBuf,
    target_dir: PathBuf,
    build_dir: Option<PathBuf>,
    offline: bool,
}

impl ExecPlan {
    fn shared(settings: &Settings, paths: &WorkspacePaths, supports_build_dir: bool) -> Self {
        Self {
            label: "shared",
            cargo_home: settings.base_cargo_home.clone(),
            target_dir: paths.shared_target_dir.clone(),
            build_dir: supports_build_dir.then(|| paths.shared_build_dir.clone()),
            offline: false,
        }
    }

    fn build_lane(
        settings: &Settings,
        paths: &WorkspacePaths,
        lane: &LaneLease,
        supports_build_dir: bool,
    ) -> Self {
        Self {
            label: "build-lane",
            cargo_home: settings.base_cargo_home.clone(),
            target_dir: shared_or_lane_target_dir(paths, lane, supports_build_dir),
            build_dir: lane_build_dir(lane, supports_build_dir),
            offline: false,
        }
    }

    fn readonly_overlay(
        paths: &WorkspacePaths,
        lane: &LaneLease,
        cargo_home: &Path,
        supports_build_dir: bool,
    ) -> Self {
        Self {
            label: "readonly-overlay",
            cargo_home: cargo_home.to_path_buf(),
            target_dir: shared_or_lane_target_dir(paths, lane, supports_build_dir),
            build_dir: lane_build_dir(lane, supports_build_dir),
            offline: true,
        }
    }

    fn online_overlay(lane: &LaneLease, cargo_home: &Path, supports_build_dir: bool) -> Self {
        Self {
            label: "online-overlay",
            cargo_home: cargo_home.to_path_buf(),
            target_dir: lane.target_dir.clone(),
            build_dir: lane_build_dir(lane, supports_build_dir),
            offline: false,
        }
    }
}

fn shared_or_lane_target_dir(
    paths: &WorkspacePaths,
    lane: &LaneLease,
    supports_build_dir: bool,
) -> PathBuf {
    if supports_build_dir {
        paths.shared_target_dir.clone()
    } else {
        lane.target_dir.clone()
    }
}

fn lane_build_dir(lane: &LaneLease, supports_build_dir: bool) -> Option<PathBuf> {
    supports_build_dir.then(|| lane.build_dir.clone())
}

enum RunOutcome {
    Completed(i32, String),
    Lock(LockKind),
}

fn run_plan(settings: &Settings, args: &[OsString], plan: &ExecPlan) -> Result<RunOutcome, String> {
    fs::create_dir_all(&plan.target_dir).map_err(err_string)?;
    if let Some(build_dir) = &plan.build_dir {
        fs::create_dir_all(build_dir).map_err(err_string)?;
    }
    fs::create_dir_all(&plan.cargo_home).map_err(err_string)?;

    let mut command = Command::new(&settings.cargo_bin);
    command.args(args);
    command.stdin(Stdio::inherit());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::piped());
    command.env("CARGO_HOME", &plan.cargo_home);
    command.env("CARGO_TARGET_DIR", &plan.target_dir);
    command.env("CARGO_SIDESTEP_ACTIVE", "1");
    command.env("CARGO_SIDESTEP_PLAN", plan.label);
    if let Some(build_dir) = &plan.build_dir {
        command.env("CARGO_BUILD_BUILD_DIR", build_dir);
    } else {
        command.env_remove("CARGO_BUILD_BUILD_DIR");
    }
    if plan.offline {
        command.env("CARGO_NET_OFFLINE", "true");
    } else {
        command.env_remove("CARGO_NET_OFFLINE");
    }

    let mut child = command.spawn().map_err(err_string)?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture cargo stderr".to_string())?;
    let (events_tx, events_rx) = mpsc::channel();
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_clone = Arc::clone(&captured);
    let reader_handle = thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let _ = io::stderr().write_all(line.as_bytes());
                    let _ = io::stderr().flush();
                    if let Ok(mut captured) = captured_clone.lock() {
                        if captured.len() < 128 * 1024 {
                            captured.push_str(&line);
                        }
                    }
                    if let Some(kind) = LockKind::parse(&line) {
                        let _ = events_tx.send(kind);
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut first_lock: Option<(LockKind, Instant)> = None;
    loop {
        if let Some(status) = child.try_wait().map_err(err_string)? {
            let _ = reader_handle.join();
            let captured = captured
                .lock()
                .map(|value| value.clone())
                .unwrap_or_default();
            return Ok(RunOutcome::Completed(exit_code(status), captured));
        }

        match events_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(kind) => {
                if first_lock.is_none() {
                    first_lock = Some((kind, Instant::now()));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }

        if let Some((kind, since)) = first_lock {
            if since.elapsed() >= settings.fallback_after {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader_handle.join();
                return Ok(RunOutcome::Lock(kind));
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LockKind {
    BuildDirectory,
    PackageCache,
    RegistryIndex,
    GitDb,
    Other,
}

impl LockKind {
    fn parse(line: &str) -> Option<Self> {
        let lower = line.to_ascii_lowercase();
        if !lower.contains("blocking waiting for file lock") {
            return None;
        }
        if lower.contains("build directory") {
            Some(Self::BuildDirectory)
        } else if lower.contains("package cache") {
            Some(Self::PackageCache)
        } else if lower.contains("registry index") {
            Some(Self::RegistryIndex)
        } else if lower.contains("git db") || lower.contains("git database") {
            Some(Self::GitDb)
        } else {
            Some(Self::Other)
        }
    }

    fn is_home_lock(self) -> bool {
        !matches!(self, Self::BuildDirectory)
    }
}

impl std::fmt::Display for LockKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BuildDirectory => write!(f, "the build directory"),
            Self::PackageCache => write!(f, "the package cache"),
            Self::RegistryIndex => write!(f, "the registry index"),
            Self::GitDb => write!(f, "the git database"),
            Self::Other => write!(f, "a shared Cargo path"),
        }
    }
}

struct LaneLease {
    name: String,
    lease_file: PathBuf,
    build_dir: PathBuf,
    target_dir: PathBuf,
    readonly_home_dir: PathBuf,
    online_home_dir: PathBuf,
    heartbeat: Option<LeaseHeartbeat>,
}

impl Drop for LaneLease {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.stop();
        }
        let _ = fs::remove_file(&self.lease_file);
    }
}

struct LeaseHeartbeat {
    stop_tx: mpsc::Sender<()>,
    join_handle: thread::JoinHandle<()>,
}

impl LeaseHeartbeat {
    fn stop(self) {
        let _ = self.stop_tx.send(());
        let _ = self.join_handle.join();
    }
}

fn acquire_lane(paths: &WorkspacePaths, slots: usize) -> Result<LaneLease, String> {
    for index in 0..slots {
        let name = format!("lane-{index}");
        if let Some(lease) = try_acquire_lane(paths, &name)? {
            return Ok(lease);
        }
    }

    let name = format!("overflow-{}-{}", std::process::id(), unix_timestamp());
    try_acquire_lane(paths, &name)?.ok_or_else(|| "failed to allocate overflow lane".to_string())
}

fn try_acquire_lane(paths: &WorkspacePaths, name: &str) -> Result<Option<LaneLease>, String> {
    let root = paths.lanes_dir.join(name);
    fs::create_dir_all(&root).map_err(err_string)?;
    let lease_file = root.join("lease");

    if lease_file.exists() {
        if lease_is_stale(&lease_file) {
            let _ = fs::remove_file(&lease_file);
        } else {
            return Ok(None);
        }
    }

    match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lease_file)
    {
        Ok(mut file) => {
            let _ = writeln!(file, "pid={}", std::process::id());
            let _ = writeln!(file, "timestamp={}", unix_timestamp());
            let build_dir = root.join("build");
            let target_dir = root.join("target");
            let readonly_home_dir = root.join("cargo-home-readonly");
            let online_home_dir = root.join("cargo-home-online");
            let heartbeat = start_lease_heartbeat(&lease_file);
            Ok(Some(LaneLease {
                name: name.to_string(),
                lease_file,
                build_dir,
                target_dir,
                readonly_home_dir,
                online_home_dir,
                heartbeat: Some(heartbeat),
            }))
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(None),
        Err(err) => Err(err_string(err)),
    }
}

fn start_lease_heartbeat(lease_file: &Path) -> LeaseHeartbeat {
    let lease_file = lease_file.to_path_buf();
    let pid = std::process::id();
    let (stop_tx, stop_rx) = mpsc::channel();
    let join_handle = thread::spawn(move || {
        let interval = Duration::from_secs(LEASE_REFRESH_INTERVAL_SECS);
        loop {
            match stop_rx.recv_timeout(interval) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = refresh_lease_file(&lease_file, pid);
                }
            }
        }
    });
    LeaseHeartbeat {
        stop_tx,
        join_handle,
    }
}

fn refresh_lease_file(path: &Path, pid: u32) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(path)?;
    writeln!(file, "pid={pid}")?;
    writeln!(file, "timestamp={}", unix_timestamp())?;
    file.flush()
}

fn lease_is_stale(path: &Path) -> bool {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .map(|age| age > Duration::from_secs(STALE_LEASE_AFTER_SECS))
        .unwrap_or(false)
}

enum OverlayMode {
    ReadonlyOffline,
    IsolatedOnline,
}

fn prepare_overlay_home(base: &Path, overlay: &Path, mode: OverlayMode) -> Result<PathBuf, String> {
    fs::create_dir_all(overlay).map_err(err_string)?;
    fs::create_dir_all(overlay.join("registry")).map_err(err_string)?;
    fs::create_dir_all(overlay.join("registry").join("src")).map_err(err_string)?;
    fs::create_dir_all(overlay.join("registry").join("index")).map_err(err_string)?;
    fs::create_dir_all(overlay.join("git").join("db")).map_err(err_string)?;
    fs::create_dir_all(overlay.join("git").join("checkouts")).map_err(err_string)?;

    for file in [
        ".crates.toml",
        ".crates2.json",
        "env",
        "config.toml",
        "credentials.toml",
        "credentials",
    ] {
        copy_file_if_exists(&base.join(file), &overlay.join(file))?;
    }

    if base.join("bin").exists() {
        ensure_symlink(&base.join("bin"), &overlay.join("bin"))?;
    }

    if matches!(mode, OverlayMode::ReadonlyOffline) && base.join("registry").join("cache").exists()
    {
        ensure_symlink(
            &base.join("registry").join("cache"),
            &overlay.join("registry").join("cache"),
        )?;
    } else {
        fs::create_dir_all(overlay.join("registry").join("cache")).map_err(err_string)?;
    }

    Ok(overlay.to_path_buf())
}

fn copy_file_if_exists(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(err_string)?;
    }
    fs::copy(src, dst).map_err(err_string)?;
    Ok(())
}

fn ensure_symlink(src: &Path, dst: &Path) -> Result<(), String> {
    if dst.exists() {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(err_string)?;
    }
    symlink_path(src, dst).map_err(err_string)
}

#[cfg(unix)]
fn symlink_path(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(windows)]
fn symlink_path(src: &Path, dst: &Path) -> io::Result<()> {
    if src.is_dir() {
        std::os::windows::fs::symlink_dir(src, dst)
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    }
}

fn discover_workspace_root(cwd: &Path, args: &[OsString]) -> Result<PathBuf, String> {
    let search_root = manifest_path_from_args(args)
        .map(|manifest| {
            manifest
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| "manifest path has no parent directory".to_string())
        })
        .transpose()?
        .unwrap_or_else(|| cwd.to_path_buf());
    let mut nearest_manifest = None;
    for ancestor in search_root.ancestors() {
        let manifest = ancestor.join("Cargo.toml");
        if !manifest.exists() {
            continue;
        }
        if nearest_manifest.is_none() {
            nearest_manifest = Some(ancestor.to_path_buf());
        }
        let contents = fs::read_to_string(&manifest).unwrap_or_default();
        if contents.contains("[workspace]") {
            return Ok(ancestor.to_path_buf());
        }
    }
    Ok(nearest_manifest.unwrap_or_else(|| cwd.to_path_buf()))
}

fn manifest_path_from_args(args: &[OsString]) -> Option<PathBuf> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--manifest-path" {
            return iter.next().map(PathBuf::from);
        }
        if let Some(raw) = arg.to_str() {
            if let Some(value) = raw.strip_prefix("--manifest-path=") {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

fn detect_build_dir_support(cargo_bin: &OsStr) -> bool {
    let output = Command::new(cargo_bin).arg("-V").output();
    let Ok(output) = output else {
        return false;
    };
    let version = String::from_utf8_lossy(&output.stdout);
    parse_cargo_version(&version)
        .map(|(major, minor)| major > 1 || (major == 1 && minor >= 91))
        .unwrap_or(false)
}

fn parse_cargo_version(raw: &str) -> Option<(u64, u64)> {
    let mut parts = raw.split_whitespace();
    let _cargo = parts.next()?;
    let version = parts.next()?;
    let mut segments = version.split('.');
    let major = segments.next()?.parse::<u64>().ok()?;
    let minor = segments.next()?.parse::<u64>().ok()?;
    Some((major, minor))
}

fn stderr_looks_like_offline_miss(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("offline was specified")
        || lower.contains("can't be accessed in offline mode")
        || lower.contains("unable to update registry")
        || lower.contains("no matching package named")
        || lower.contains("failed to fetch")
        || lower.contains("attempting to make an http request")
}

fn default_cargo_home() -> PathBuf {
    home_dir().join(".cargo")
}

fn default_state_root() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("cargo-sidestep");
    }
    #[cfg(target_os = "macos")]
    {
        home_dir()
            .join("Library")
            .join("Caches")
            .join("cargo-sidestep")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home_dir().join(".cache").join("cargo-sidestep")
    }
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn stable_hash(input: &str) -> String {
    format!("{:016x}", stable_hash_u64(input))
}

fn stable_hash_u64(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

fn err_string(err: impl std::fmt::Display) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        discover_workspace_root, normalize_cli_args, parse_cargo_version, stable_hash_u64,
        stderr_looks_like_offline_miss, LockKind,
    };
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!("cargo-sidestep-lib-{name}-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parses_lock_lines() {
        assert_eq!(
            LockKind::parse("Blocking waiting for file lock on package cache"),
            Some(LockKind::PackageCache)
        );
        assert_eq!(
            LockKind::parse("Blocking waiting for file lock on build directory"),
            Some(LockKind::BuildDirectory)
        );
        assert_eq!(LockKind::parse("Finished dev profile"), None);
    }

    #[test]
    fn detects_offline_cache_miss() {
        assert!(stderr_looks_like_offline_miss(
            "error: failed to download from registry because offline was specified"
        ));
        assert!(!stderr_looks_like_offline_miss("Compiling foo v0.1.0"));
    }

    #[test]
    fn parses_cargo_version_output() {
        assert_eq!(
            parse_cargo_version("cargo 1.91.0 (abcd 2025-10-10)"),
            Some((1, 91))
        );
        assert_eq!(parse_cargo_version("cargo"), None);
    }

    #[test]
    fn stable_hash_is_stable() {
        assert_eq!(stable_hash_u64("abc"), stable_hash_u64("abc"));
        assert_ne!(stable_hash_u64("abc"), stable_hash_u64("abd"));
    }

    #[test]
    fn strips_cargo_plugin_prefix_from_args() {
        let args = normalize_cli_args(vec![
            OsString::from("sidestep"),
            OsString::from("check"),
            OsString::from("--workspace"),
        ]);
        assert_eq!(
            args,
            vec![OsString::from("check"), OsString::from("--workspace")]
        );
    }

    #[test]
    fn manifest_path_uses_enclosing_workspace_root() {
        let root = temp_dir("workspace-root");
        let member_dir = root.join("crates").join("member");
        fs::create_dir_all(&member_dir).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/member\"]\n",
        )
        .unwrap();
        fs::write(
            member_dir.join("Cargo.toml"),
            "[package]\nname = \"member\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();

        let resolved = discover_workspace_root(
            &root,
            &[OsString::from(format!(
                "--manifest-path={}",
                member_dir.join("Cargo.toml").display()
            ))],
        )
        .unwrap();

        assert_eq!(resolved, root);
    }
}
