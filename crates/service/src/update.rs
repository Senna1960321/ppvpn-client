//! Silent desktop updates (docs/host-integration.md section 16, #548).
//!
//! The client sends `InstallUpdate` when it quits or sits idle. The service
//! copies the package into a directory only it can write, checks the copy
//! (the update feed's EdDSA signature, the platform's signature, the
//! version), answers `accepted`, and waits for the client process to exit.
//! Then a process of its own installs the package: the runner, a copy of
//! this binary started with [`RUNNER_FLAG`], which outlives the service
//! while the package replaces it. The runner records the result where
//! `GetVersion` reads it, and reopens the app when the client asked for it.
//!
//! The staging directory (root or SYSTEM only):
//!   lock              shared by Connect, cleanup and the detached runner;
//!                     never removed with a job
//!   current/          the update in progress: job.json, the package, the runner and,
//!                     on macOS, new/ (root-only copies of the new service
//!                     helpers)
//!   last-update.json  the last result
// Linux has no silent updates: most of this is unused there.
#![cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]

use crate::protocol::{InstallUpdatePayload, UpdateProgress, UpdateResult};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const UNSUPPORTED: &str = "UPDATE_UNSUPPORTED";
pub const IN_PROGRESS: &str = "UPDATE_IN_PROGRESS";
pub const BUSY: &str = "UPDATE_BUSY";
pub const NOT_NEWER: &str = "UPDATE_NOT_NEWER";
pub const FILE_INVALID: &str = "UPDATE_FILE_INVALID";
pub const SIGNATURE_INVALID: &str = "UPDATE_SIGNATURE_INVALID";
pub const PUBLISHER_MISMATCH: &str = "UPDATE_PUBLISHER_MISMATCH";
pub const VERSION_MISMATCH: &str = "UPDATE_VERSION_MISMATCH";
pub const CLIENT_STILL_RUNNING: &str = "UPDATE_CLIENT_STILL_RUNNING";
pub const INSTALL_FAILED: &str = "UPDATE_INSTALL_FAILED";

/// Started with this argument, the service binary is the runner.
pub const RUNNER_FLAG: &str = "--install-update";

/// The runner's log, next to the service's.
const RUNNER_LOG: &str = "ppvpn-update.log";

const MAX_PACKAGE_BYTES: u64 = 1 << 30;
/// How long the client may take to exit after `accepted`.
const CLIENT_EXIT_LIMIT: Duration = Duration::from_secs(60);
/// How long the install may take.
const INSTALL_LIMIT: Duration = Duration::from_secs(10 * 60);

#[cfg(target_os = "macos")]
const PACKAGE: &str = "PPVPN.pkg";
#[cfg(not(target_os = "macos"))]
const PACKAGE: &str = "setup.exe";

#[cfg(windows)]
const RUNNER: &str = "ppvpn-service-update.exe";
#[cfg(not(windows))]
const RUNNER: &str = "ppvpn-service-update";

fn baked(value: Option<&'static str>) -> Option<&'static str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// This release's build number (`PPVPN_BUILD_NUMBER`); 0 in a build
/// without one.
pub fn build_number() -> u64 {
    baked(option_env!("PPVPN_BUILD_NUMBER"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

/// The update feeds' EdDSA key (`PPVPN_SPARKLE_PUBLIC_KEY`), the one
/// Sparkle and WinSparkle check.
fn public_key() -> Option<[u8; 32]> {
    decode_public_key(baked(option_env!("PPVPN_SPARKLE_PUBLIC_KEY"))?)
}

fn decode_public_key(text: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()?
        .try_into()
        .ok()
}

/// Ed25519 over the whole file, as Sparkle's `sign_update` and
/// `winsparkle-tool sign` make it.
fn signature_valid(key: &[u8; 32], package: &[u8], signature: &str) -> bool {
    use base64::Engine;
    let Ok(signature) = base64::engine::general_purpose::STANDARD.decode(signature.trim()) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(package, &signature)
        .is_ok()
}

/// `x.y.z` of the feed, as numbers.
fn version_parts(version: &str) -> Option<Vec<u64>> {
    let parts = version
        .split('.')
        .map(|part| {
            (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| part.parse().ok())
                .flatten()
        })
        .collect::<Option<Vec<u64>>>()?;
    (parts.len() == 3).then_some(parts)
}

/// The version the package itself must carry: the build number in the
/// macOS pkg's PackageInfo, `<version>.<build>` in the Windows installer's
/// FileVersion.
fn expected_version(payload: &InstallUpdatePayload) -> Option<Vec<u64>> {
    let mut parts = version_parts(&payload.version)?;
    if cfg!(target_os = "macos") {
        return Some(vec![payload.build]);
    }
    parts.push(payload.build);
    Some(parts)
}

fn version_matches(found: &str, expected: &[u64]) -> bool {
    let found: Option<Vec<u64>> = found.split('.').map(|part| part.parse().ok()).collect();
    found.as_deref() == Some(expected)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    /// Copying and checking the package; not reported.
    Checking,
    Waiting,
    Installing,
    /// The runner recorded the result.
    Finished,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Job {
    /// Binds a delayed runner to this job, never a later staged package.
    #[serde(default)]
    id: String,
    version: String,
    build: u64,
    relaunch: bool,
    phase: Phase,
    /// The client's user: its uid on macOS, its session on Windows.
    user: Option<u32>,
    /// The client's app (macOS: its bundle, Windows: its exe), reopened
    /// after a failed install.
    client_app: Option<PathBuf>,
}

impl Job {
    fn progress(&self) -> Option<UpdateProgress> {
        let phase = match self.phase {
            Phase::Waiting => "waiting",
            Phase::Installing => "installing",
            Phase::Checking | Phase::Finished => return None,
        };
        Some(UpdateProgress {
            version: self.version.clone(),
            build: self.build,
            phase: phase.to_string(),
        })
    }
}

/// The update this process handles, from the checks until its runner
/// exits. Taken before `CORE` where both are.
static ACTIVE: Lazy<Mutex<Option<Job>>> = Lazy::new(|| Mutex::new(None));

#[derive(Debug, Clone)]
struct Staging {
    root: PathBuf,
    /// The system one: its directory and its parent must belong to root
    /// (SYSTEM or Administrators) and be writable by nobody else.
    system: bool,
}

impl Staging {
    fn system() -> Option<Self> {
        platform::staging_root().map(|root| Staging { root, system: true })
    }

    fn current(&self) -> PathBuf {
        self.root.join("current")
    }

    fn job_path(&self) -> PathBuf {
        self.current().join("job.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.root.join("lock")
    }

    fn result_path(&self) -> PathBuf {
        self.root.join("last-update.json")
    }

    fn prepare_root(&self) -> Result<(), String> {
        if self.system {
            let parent = self
                .root
                .parent()
                .ok_or_else(|| "staging directory has no parent".to_string())?;
            platform::trusted_dir(parent)?;
            match platform::create_private_dir(&self.root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    platform::make_private(&self.root)?;
                }
                Err(error) => return Err(format!("create {}: {error}", self.root.display())),
            }
        } else {
            std::fs::create_dir_all(&self.root).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// An empty current/, created by this process in a directory only it
    /// can write. The root reservation survives replacement of this job.
    fn prepare(&self) -> Result<PathBuf, String> {
        self.prepare_root()?;
        let current = self.current();
        if std::fs::symlink_metadata(&current).is_ok() {
            std::fs::remove_dir_all(&current)
                .map_err(|error| format!("remove {}: {error}", current.display()))?;
        }
        // Fails when something else made it in between: never use a
        // directory this process did not create.
        let created = if self.system {
            platform::create_private_dir(&current)
        } else {
            std::fs::create_dir(&current)
        };
        created.map_err(|error| format!("create {}: {error}", current.display()))?;
        Ok(current)
    }
}

fn read_job(staging: &Staging) -> Option<Job> {
    serde_json::from_slice(&std::fs::read(staging.job_path()).ok()?).ok()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

fn write_job(staging: &Staging, job: &Job) -> std::io::Result<()> {
    write_atomic(&staging.job_path(), &serde_json::to_vec(job)?)
}

fn read_result(staging: &Staging) -> Option<UpdateResult> {
    serde_json::from_slice(&std::fs::read(staging.result_path()).ok()?).ok()
}

/// Records `job`'s result: `Err` carries the error code.
fn record(staging: &Staging, job: &Job, outcome: Result<(), String>) {
    match &outcome {
        Ok(()) => log::info!("update to {} ({}) installed", job.version, job.build),
        Err(error) => log::warn!("update to {} ({}) failed: {error}", job.version, job.build),
    }
    let result = UpdateResult {
        version: job.version.clone(),
        build: job.build,
        outcome: if outcome.is_ok() {
            "installed"
        } else {
            "failed"
        }
        .to_string(),
        error: outcome.err(),
        finished_at_ms: now_ms(),
    };
    let written = serde_json::to_vec(&result)
        .map_err(std::io::Error::from)
        .and_then(|bytes| write_atomic(&staging.result_path(), &bytes));
    if let Err(error) = written {
        log::error!("cannot record the update's result: {error}");
    }
}

/// Hold the same, stable lock through cleanup, core startup or installation.
/// A missing root is idle; all other open/lock errors fail closed. Only the
/// service creates the private root, and the lock is never unlinked.
fn reservation(staging: &Staging) -> std::io::Result<Option<std::fs::File>> {
    match std::fs::symlink_metadata(&staging.root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    if staging.system {
        platform::trusted_dir(&staging.root).map_err(std::io::Error::other)?;
    }
    let lock = match platform::create_new_private_file(&staging.lock_path()) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(staging.lock_path())?
        }
        Err(error) => return Err(error),
    };
    let metadata = lock.metadata()?;
    if !metadata.is_file() || platform::is_reparse_point(&metadata) {
        return Err(std::io::Error::other("update lock is not a regular file"));
    }
    match lock.try_lock() {
        Ok(()) => Ok(Some(lock)),
        Err(std::fs::TryLockError::WouldBlock) => Err(std::io::ErrorKind::WouldBlock.into()),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Both paths take ACTIVE before the disk reservation before CORE. A service
/// restart may race an old worker before it locks: cancel that job under the
/// reservation before a new core starts. The worker also checks its job id.
pub fn guard_connect<T>(connect: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    guard_connect_in(Staging::system().as_ref(), connect)
}

fn guard_connect_in<T>(
    staging: Option<&Staging>,
    connect: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let active = ACTIVE.lock();
    if active.is_some() {
        anyhow::bail!(IN_PROGRESS);
    }
    let _reservation = if let Some(staging) = staging {
        let lock = reservation(staging).map_err(|_| anyhow::anyhow!(IN_PROGRESS))?;
        settle_locked(staging).map_err(|_| anyhow::anyhow!(IN_PROGRESS))?;
        lock
    } else {
        None
    };
    connect()
}

/// ACTIVE must be held and empty. Keep the reservation until cleanup ends,
/// rather than merely probing it and racing a worker that has not locked yet.
fn settle(staging: &Staging) -> std::io::Result<()> {
    let _reservation = reservation(staging)?;
    settle_locked(staging)
}

fn settle_locked(staging: &Staging) -> std::io::Result<()> {
    if let Some(job) = read_job(staging) {
        if job.phase == Phase::Installing {
            record(staging, &job, Err(format!("{INSTALL_FAILED}:interrupted")));
        }
    }
    // Invalidate first, even if Windows cannot remove an old runner's exe.
    // A worker delayed before taking the lock must never install this job.
    match std::fs::remove_file(staging.job_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Err(error) = std::fs::remove_dir_all(staging.current()) {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("cannot remove {}: {error}", staging.current().display());
        }
    }
    Ok(())
}

/// A stale runner cannot consume a new job, even if its directory was reused.
fn claim_runner(staging: &Staging, id: &str) -> Result<(std::fs::File, Job), String> {
    if id.is_empty() {
        return Err("missing update job id".into());
    }
    let lock = reservation(staging)
        .map_err(|error| format!("update reservation: {error}"))?
        .ok_or_else(|| "no update to install".to_string())?;
    let job = read_job(staging)
        .filter(|job| job.id == id && job.phase == Phase::Installing)
        .ok_or_else(|| "no matching update to install".to_string())?;
    Ok((lock, job))
}

/// `GetVersion`'s `update` and `last_update`.
pub fn status() -> (Option<UpdateProgress>, Option<UpdateResult>) {
    match Staging::system() {
        Some(staging) => status_in(&staging),
        None => (None, None),
    }
}

fn status_in(staging: &Staging) -> (Option<UpdateProgress>, Option<UpdateResult>) {
    let progress = {
        let active = ACTIVE.lock();
        match active.as_ref() {
            Some(job) => job.progress(),
            None => match settle(staging) {
                Ok(()) => None,
                Err(_) => read_job(staging).and_then(|job| job.progress()),
            },
        }
    };
    (progress, read_result(staging))
}

/// Cleans up after an earlier service process (at start).
pub fn settle_at_start() {
    if let Some(staging) = Staging::system() {
        let _active = ACTIVE.lock();
        if let Err(error) = settle(&staging) {
            log::info!("update reservation unavailable at service start: {error}");
        }
    }
}

/// The client process at the other end of an `InstallUpdate` connection.
pub struct Client {
    pub pid: u32,
    /// Its uid (macOS) or session (Windows).
    pub user: Option<u32>,
    /// Its app: the bundle (macOS) or the exe (Windows).
    pub app: Option<PathBuf>,
    /// A wait on the process itself, opened while it held the connection.
    waiter: platform::Waiter,
}

/// The package and the platform's checks of it (steps 5 and 6).
trait PackageChecks {
    fn publisher(&self, package: &Path) -> Result<(), String>;
    /// The package's own version, read with `scratch` to work in.
    fn version(&self, package: &Path, scratch: &Path) -> Result<String, String>;
}

/// A package that passed the checks; discarded when dropped before
/// [`Staged::start`].
pub struct Staged {
    staging: Staging,
    job: Job,
    started: bool,
}

/// Steps 1 to 6 of section 16.3 (the stopping state is the IPC server's).
/// `busy`: whether the service runs a core.
pub fn check(
    payload: InstallUpdatePayload,
    busy: &dyn Fn() -> bool,
) -> Result<Staged, &'static str> {
    let (Some(staging), Some(key)) = (Staging::system(), public_key()) else {
        return Err(UNSUPPORTED);
    };
    let own_build = build_number();
    if own_build == 0 {
        return Err(UNSUPPORTED);
    }
    check_in(staging, &key, own_build, payload, busy, &platform::Checks)
}

fn check_in(
    staging: Staging,
    key: &[u8; 32],
    own_build: u64,
    payload: InstallUpdatePayload,
    busy: &dyn Fn() -> bool,
    checks: &dyn PackageChecks,
) -> Result<Staged, &'static str> {
    let job = Job {
        id: uuid::Uuid::new_v4().to_string(),
        version: payload.version.clone(),
        build: payload.build,
        relaunch: payload.relaunch,
        phase: Phase::Checking,
        user: None,
        client_app: None,
    };
    {
        let mut active = ACTIVE.lock();
        if active.is_some() {
            return Err(IN_PROGRESS);
        }
        staging.prepare_root().map_err(|_| UNSUPPORTED)?;
        let _reservation = reservation(&staging).map_err(|_| IN_PROGRESS)?;
        settle_locked(&staging).map_err(|_| IN_PROGRESS)?;
        if busy() {
            return Err(BUSY);
        }
        *active = Some(job.clone());
    }
    let staged = Staged {
        staging,
        job,
        started: false,
    };
    // Dropping `staged` on a refusal removes the copy.
    match staged.stage(key, own_build, &payload, checks) {
        Ok(()) => Ok(staged),
        Err((code, detail)) => {
            log::warn!(
                "update to {} ({}) refused: {code}: {detail}",
                payload.version,
                payload.build
            );
            Err(code)
        }
    }
}

/// Opens `path` without following a final symbolic link (or reparse
/// point), checks the open file and copies it to `target`; returns its
/// bytes. Everything after this looks only at the copy.
fn copy_package(path: &Path, length: u64, target: &Path) -> Result<Vec<u8>, String> {
    if !path.is_absolute() {
        return Err("not an absolute path".into());
    }
    if length == 0 || length > MAX_PACKAGE_BYTES {
        return Err(format!("length {length} out of range"));
    }
    let file = platform::open_no_follow(path).map_err(|error| format!("open: {error}"))?;
    let metadata = file.metadata().map_err(|error| format!("stat: {error}"))?;
    if !metadata.is_file() || platform::is_reparse_point(&metadata) {
        return Err("not a regular file".into());
    }
    if metadata.len() != length {
        return Err(format!("{} bytes, not {length}", metadata.len()));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    (&file)
        .take(length + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read: {error}"))?;
    if bytes.len() as u64 != length {
        return Err("changed while read".into());
    }
    let mut copy = platform::create_new_private_file(target)
        .map_err(|error| format!("create {}: {error}", target.display()))?;
    copy.write_all(&bytes)
        .and_then(|()| copy.sync_all())
        .map_err(|error| format!("write {}: {error}", target.display()))?;
    Ok(bytes)
}

impl Staged {
    fn stage(
        &self,
        key: &[u8; 32],
        own_build: u64,
        payload: &InstallUpdatePayload,
        checks: &dyn PackageChecks,
    ) -> Result<(), (&'static str, String)> {
        if payload.build <= own_build {
            return Err((
                NOT_NEWER,
                format!("build {} is not newer than {own_build}", payload.build),
            ));
        }
        let current = self
            .staging
            .prepare()
            .map_err(|error| (UNSUPPORTED, error))?;
        let package = current.join(PACKAGE);
        let bytes = copy_package(Path::new(&payload.path), payload.length, &package)
            .map_err(|error| (FILE_INVALID, error))?;
        if !signature_valid(key, &bytes, &payload.ed_signature) {
            return Err((SIGNATURE_INVALID, "EdDSA signature does not verify".into()));
        }
        drop(bytes);
        checks
            .publisher(&package)
            .map_err(|error| (PUBLISHER_MISMATCH, error))?;
        let expected = expected_version(payload)
            .ok_or_else(|| (VERSION_MISMATCH, format!("version {:?}", payload.version)))?;
        let found = checks
            .version(&package, &current)
            .map_err(|error| (VERSION_MISMATCH, error))?;
        if !version_matches(&found, &expected) {
            return Err((
                VERSION_MISMATCH,
                format!("the package is {found}, not {expected:?}"),
            ));
        }
        Ok(())
    }

    /// Accepted: waits for `client` to exit, then hands the install to the
    /// runner (on a thread of its own).
    pub fn start(mut self, client: Client, busy: fn() -> bool) {
        self.started = true;
        let mut job = self.job.clone();
        job.phase = Phase::Waiting;
        job.user = client.user;
        job.client_app = client.app.clone();
        let staging = self.staging.clone();
        if let Err(error) = write_job(&staging, &job) {
            finish(
                &staging,
                &job,
                Err(format!("{INSTALL_FAILED}:job: {error}")),
            );
            return;
        }
        *ACTIVE.lock() = Some(job.clone());
        log::info!(
            "update to {} ({}) accepted; waiting for pid {} to exit",
            job.version,
            job.build,
            client.pid
        );
        let thread = {
            let (staging, job) = (staging.clone(), job.clone());
            std::thread::Builder::new()
                .name("update".into())
                .spawn(move || supervise(staging, job, client, busy))
        };
        if let Err(error) = thread {
            finish(
                &staging,
                &job,
                Err(format!("{INSTALL_FAILED}:thread: {error}")),
            );
        }
    }

    /// Not accepted after all (the client went away): no result.
    pub fn discard(self, reason: &str) {
        log::info!(
            "update to {} ({}) dropped: {reason}",
            self.job.version,
            self.job.build
        );
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.started {
            let _ = std::fs::remove_dir_all(self.staging.current());
            *ACTIVE.lock() = None;
        }
    }
}

/// Records the result of an update that never reached the runner and
/// clears it.
fn finish(staging: &Staging, job: &Job, outcome: Result<(), String>) {
    let mut active = ACTIVE.lock();
    record(staging, job, outcome);
    let _ = std::fs::remove_dir_all(staging.current());
    *active = None;
}

fn supervise(staging: Staging, mut job: Job, client: Client, busy: fn() -> bool) {
    if !client.waiter.wait(CLIENT_EXIT_LIMIT) {
        finish(&staging, &job, Err(CLIENT_STILL_RUNNING.into()));
        return;
    }
    if busy() {
        finish(&staging, &job, Err(BUSY.into()));
        return;
    }
    job.phase = Phase::Installing;
    if let Err(error) = write_job(&staging, &job) {
        finish(
            &staging,
            &job,
            Err(format!("{INSTALL_FAILED}:job: {error}")),
        );
        return;
    }
    *ACTIVE.lock() = Some(job.clone());
    let mut runner = match spawn_runner(&staging, &job) {
        Ok(runner) => runner,
        Err(error) => {
            finish(
                &staging,
                &job,
                Err(format!("{INSTALL_FAILED}:runner: {error}")),
            );
            return;
        }
    };
    log::info!(
        "update to {} ({}): installing (runner pid {})",
        job.version,
        job.build,
        runner.id()
    );
    // Usually the install replaces this process first.
    let status = runner.wait();
    let mut active = ACTIVE.lock();
    *active = None;
    log::info!("update runner exited: {status:?}");
    let _ = settle(&staging);
}

/// Starts a copy of this binary as the runner, outside this process's
/// group (macOS) or console (Windows), so stopping the service leaves it.
fn spawn_runner(staging: &Staging, job: &Job) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let runner = staging.current().join(RUNNER);
    std::fs::copy(std::env::current_exe()?, &runner)?;
    let mut command = Command::new(&runner);
    command
        .arg(RUNNER_FLAG)
        .arg(&job.id)
        .current_dir(staging.current())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    platform::detach(&mut command);
    command.spawn()
}

/// Awake time: it stands still while the machine sleeps (section 16.4).
fn awake() -> Duration {
    platform::awake()
}

/// Runs `command` to the end, killing it when `deadline` (awake time)
/// passes.
fn run_until(
    command: &mut std::process::Command,
    deadline: Duration,
) -> Result<std::process::ExitStatus, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("spawn {program}: {error}"))?;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => return Err(format!("wait for {program}: {error}")),
        }
        if awake() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{program} timed out"));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn exit_code(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| status.to_string(), |code| code.to_string())
}

/// The runner's `main` (the service binary started with [`RUNNER_FLAG`]):
/// installs the staged update, records the result and reopens the app.
pub fn run_runner() -> i32 {
    let Some(staging) = Staging::system() else {
        return 2;
    };
    if let Err(error) = crate::logfile::init_logger(RUNNER_LOG) {
        eprintln!("cannot open the update log: {error}");
    }
    let id = std::env::args().nth(2).unwrap_or_default();
    let (_reservation, mut job) = match claim_runner(&staging, &id) {
        Ok(claim) => claim,
        Err(error) => {
            log::error!("cannot claim update: {error}");
            return 2;
        }
    };
    log::info!("installing {} ({})", job.version, job.build);
    let current = staging.current();
    let outcome = platform::install(&current, awake() + INSTALL_LIMIT);
    let installed = outcome.is_ok();
    record(
        &staging,
        &job,
        outcome.map_err(|detail| format!("{INSTALL_FAILED}:{detail}")),
    );
    job.phase = Phase::Finished;
    if let Err(error) = write_job(&staging, &job) {
        log::warn!("cannot mark the update finished: {error}");
    }
    let _ = std::fs::remove_file(current.join(PACKAGE));
    let _ = std::fs::remove_dir_all(current.join("new"));
    if job.relaunch {
        platform::relaunch(&job, installed);
    }
    log::logger().flush();
    0
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{exit_code, run_until, Client, Job, PackageChecks, PACKAGE};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Where the pkg installs the app (package-app.sh).
    const INSTALLED_APP: &str = "/Applications/PPVPN.app";

    pub(super) fn staging_root() -> Option<PathBuf> {
        Some(PathBuf::from("/Library/Application Support/PPVPN/updates"))
    }

    /// A directory, not a link, owned by root and writable by nobody else.
    pub(super) fn trusted_dir(path: &Path) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(format!("{} is not a root-only directory", path.display()));
        }
        Ok(())
    }

    pub(super) fn create_private_dir(path: &Path) -> std::io::Result<()> {
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }

    pub(super) fn make_private(path: &Path) -> Result<(), String> {
        trusted_dir(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("chmod {}: {error}", path.display()))
    }

    pub(super) fn create_new_private_file(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
    }

    /// O_NONBLOCK: a FIFO opens at once (and is then refused).
    pub(super) fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
    }

    pub(super) fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
        false
    }

    /// `Instant` stands still while the Mac sleeps (CLOCK_UPTIME_RAW).
    pub(super) fn awake() -> Duration {
        static START: once_cell::sync::Lazy<Instant> = once_cell::sync::Lazy::new(Instant::now);
        START.elapsed()
    }

    /// Its own session: `launchctl bootout` of the service (by the new
    /// ppvpn-service-install) ends the service's process group, not this.
    pub(super) fn detach(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// A kqueue watching the process for its exit.
    pub(super) struct Waiter(OwnedFd);

    impl Waiter {
        fn open(pid: libc::pid_t) -> Option<Self> {
            // SAFETY: plain system calls on a descriptor owned here.
            unsafe {
                let queue = libc::kqueue();
                if queue < 0 {
                    return None;
                }
                let queue = OwnedFd::from_raw_fd(queue);
                let mut change: libc::kevent = std::mem::zeroed();
                change.ident = pid as libc::uintptr_t;
                change.filter = libc::EVFILT_PROC;
                change.flags = libc::EV_ADD | libc::EV_ENABLE;
                change.fflags = libc::NOTE_EXIT;
                let added = libc::kevent(
                    queue.as_raw_fd(),
                    &change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                );
                (added == 0).then_some(Waiter(queue))
            }
        }

        /// Whether the process exits within `limit` of awake time.
        pub(super) fn wait(&self, limit: Duration) -> bool {
            let started = awake();
            loop {
                let timeout = libc::timespec {
                    tv_sec: 1,
                    tv_nsec: 0,
                };
                // SAFETY: one event into a local, on the owned queue.
                let ready = unsafe {
                    let mut event: libc::kevent = std::mem::zeroed();
                    libc::kevent(
                        self.0.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        &mut event,
                        1,
                        &timeout,
                    )
                };
                if ready > 0 {
                    return true;
                }
                if ready < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    log::warn!(
                        "waiting for the client: {}",
                        std::io::Error::last_os_error()
                    );
                    return false;
                }
                if awake().saturating_sub(started) >= limit {
                    return false;
                }
            }
        }
    }

    /// The peer of `stream`, from its audit token, with a wait on it.
    pub(crate) fn client(stream: &std::os::unix::net::UnixStream) -> Option<Client> {
        let token = crate::ipc::peer_audit_token(stream)?;
        let pid = token[5] as libc::pid_t;
        let waiter = Waiter::open(pid)?;
        let app = crate::ipc::macos_process_path(pid)
            .ok()
            .and_then(|path| bundle_of(Path::new(&path)));
        Some(Client {
            pid: pid as u32,
            user: Some(token[1]),
            app,
            waiter,
        })
    }

    /// `<bundle>.app` of `<bundle>.app/Contents/MacOS/<executable>`.
    pub(super) fn bundle_of(executable: &Path) -> Option<PathBuf> {
        let bundle = executable.parent()?.parent()?.parent()?;
        (bundle.extension()? == "app").then(|| bundle.to_path_buf())
    }

    pub(super) struct Checks;

    impl PackageChecks for Checks {
        /// Gatekeeper accepts it as notarized, signed by our team's
        /// Developer ID Installer.
        fn publisher(&self, package: &Path) -> Result<(), String> {
            let output = Command::new("/usr/sbin/spctl")
                .args(["--assess", "--type", "install", "-vv"])
                .arg(package)
                .output()
                .map_err(|error| format!("spctl: {error}"))?;
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if output.status.success() && spctl_accepts(&text, crate::macsign::team_id()) {
                Ok(())
            } else {
                Err(format!("spctl: {}", text.trim()))
            }
        }

        /// PackageInfo's version, which package-app.sh sets to the build.
        fn version(&self, package: &Path, scratch: &Path) -> Result<String, String> {
            let directory = scratch.join("package-info");
            std::fs::create_dir(&directory).map_err(|error| error.to_string())?;
            let status = Command::new("/usr/bin/xar")
                .arg("-xf")
                .arg(package)
                .arg("-C")
                .arg(&directory)
                .arg("PackageInfo")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|error| format!("xar: {error}"))?;
            let info = std::fs::read_to_string(directory.join("PackageInfo"));
            let _ = std::fs::remove_dir_all(&directory);
            if !status.success() {
                return Err(format!("xar exit {}", exit_code(status)));
            }
            let info = info.map_err(|error| format!("PackageInfo: {error}"))?;
            pkg_info_version(&info).ok_or_else(|| "PackageInfo has no version".into())
        }
    }

    /// `spctl -vv`'s verdict on an install package: accepted, notarized,
    /// and the origin is a Developer ID Installer of `team`.
    pub(super) fn spctl_accepts(output: &str, team: &str) -> bool {
        let lines: Vec<&str> = output.lines().map(str::trim).collect();
        let suffix = format!("({team})");
        lines.iter().any(|line| line.ends_with(": accepted"))
            && lines.contains(&"source=Notarized Developer ID")
            && lines.iter().any(|line| {
                line.strip_prefix("origin=Developer ID Installer: ")
                    .is_some_and(|name| name.ends_with(&suffix))
            })
    }

    /// The `version` attribute of PackageInfo's `<pkg-info>`.
    pub(super) fn pkg_info_version(xml: &str) -> Option<String> {
        let start = xml.find("<pkg-info")? + "<pkg-info".len();
        let tag = &xml[start..];
        let mut rest = &tag[..tag.find('>')?];
        loop {
            rest = rest.trim_start();
            let equals = rest.find('=')?;
            let name = rest[..equals].trim();
            rest = rest[equals + 1..].trim_start();
            let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
            let end = rest[1..].find(quote)? + 1;
            if name == "version" {
                return Some(rest[1..end].to_string());
            }
            rest = &rest[end + 1..];
        }
    }

    /// Keep the new helpers bound to the verified package. postinstall gives
    /// /Applications/PPVPN.app to its user, so copying from that bundle after
    /// installation would allow substitution of older, still-signed helpers.
    pub(super) fn prepare_helpers(current: &Path, deadline: Duration) -> Result<PathBuf, String> {
        let payload = current.join("payload");
        let status = run_until(
            Command::new("/usr/sbin/pkgutil")
                .arg("--expand-full")
                .arg(current.join(PACKAGE))
                .arg(&payload),
            deadline,
        )?;
        if !status.success() {
            return Err(format!("pkgutil exit {}", exit_code(status)));
        }
        let new = current.join("new");
        std::fs::create_dir(&new).map_err(|error| format!("create {}: {error}", new.display()))?;
        let helpers = payload.join("Payload/PPVPN.app/Contents/MacOS");
        for directory in helpers
            .ancestors()
            .take_while(|path| path.starts_with(&payload))
        {
            if !std::fs::symlink_metadata(directory)
                .map_err(|error| format!("stat package directory: {error}"))?
                .is_dir()
            {
                return Err("helper directory is not inside the package payload".into());
            }
        }
        for (name, identifier) in [
            ("ppvpn-service-install", crate::macsign::INSTALL_IDENTIFIER),
            ("ppvpn-service", crate::macsign::SERVICE_IDENTIFIER),
        ] {
            let source = helpers.join(name);
            if !std::fs::symlink_metadata(&source)
                .map_err(|error| format!("stat {name}: {error}"))?
                .is_file()
            {
                return Err(format!("{name}: not a regular package file"));
            }
            let target = new.join(name);
            std::fs::copy(&source, &target).map_err(|error| format!("copy {name}: {error}"))?;
            let requirement = crate::macsign::requirement(identifier, crate::macsign::team_id())
                .map_err(|error| error.to_string())?;
            crate::macsign::check_file(&target, &requirement)
                .map_err(|error| format!("{name}: {error}"))?;
        }
        Ok(new)
    }

    pub(super) fn install(current: &Path, deadline: Duration) -> Result<(), String> {
        let new = prepare_helpers(current, deadline)?;
        let status = run_until(
            Command::new("/usr/sbin/installer")
                .arg("-pkg")
                .arg(current.join(PACKAGE))
                .args(["-target", "/"]),
            deadline,
        )?;
        if !status.success() {
            return Err(format!("installer exit {}", exit_code(status)));
        }
        let helper = new.join("ppvpn-service-install");
        let status = run_until(&mut Command::new(&helper), deadline)?;
        if !status.success() {
            return Err(format!("ppvpn-service-install exit {}", exit_code(status)));
        }
        Ok(())
    }

    /// Opens the app as the client's user, in that user's session; not
    /// when the user has logged out.
    pub(super) fn relaunch(job: &Job, installed: bool) {
        let Some(uid) = job.user else {
            return;
        };
        let app = if installed {
            PathBuf::from(INSTALLED_APP)
        } else {
            match &job.client_app {
                Some(app) => app.clone(),
                None => return,
            }
        };
        let session = Command::new("/bin/launchctl")
            .args(["print", &format!("gui/{uid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !session.is_ok_and(|status| status.success()) {
            log::info!("not reopening the app: user {uid} has no session");
            return;
        }
        let opened = Command::new("/bin/launchctl")
            .args(["asuser", &uid.to_string()])
            .args(["/usr/bin/sudo", "-u", &format!("#{uid}"), "/usr/bin/open"])
            .arg(&app)
            .status();
        match opened {
            Ok(status) if status.success() => log::info!("reopened {}", app.display()),
            Ok(status) => log::warn!("open {} exit {}", app.display(), exit_code(status)),
            Err(error) => log::warn!("open {}: {error}", app.display()),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{exit_code, run_until, Client, Job, PackageChecks, PACKAGE};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::ptr;
    use std::time::Duration;
    use winapi::shared::ntdef::HANDLE;

    fn wide(text: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        text.as_ref().encode_wide().chain(Some(0)).collect()
    }

    pub(super) fn staging_root() -> Option<PathBuf> {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        Some(base.join("PPVPN").join("updates"))
    }

    /// A directory, not a reparse point, owned by SYSTEM or the
    /// Administrators: users may add to %ProgramData%, so one they made
    /// is not used.
    pub(super) fn trusted_dir(path: &Path) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if !metadata.is_dir() || is_reparse_point(&metadata) {
            return Err(format!("{} is not a plain directory", path.display()));
        }
        if !owned_by_system(path)? {
            return Err(format!(
                "{} is not owned by SYSTEM or Administrators",
                path.display()
            ));
        }
        Ok(())
    }

    fn owned_by_system(path: &Path) -> Result<bool, String> {
        use winapi::shared::winerror::ERROR_SUCCESS;
        use winapi::um::accctrl::SE_FILE_OBJECT;
        use winapi::um::aclapi::GetNamedSecurityInfoW;
        use winapi::um::securitybaseapi::IsWellKnownSid;
        use winapi::um::winbase::LocalFree;
        use winapi::um::winnt::{
            WinBuiltinAdministratorsSid, WinLocalSystemSid, OWNER_SECURITY_INFORMATION, PSID,
        };
        let name = wide(path);
        let mut owner: PSID = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        // SAFETY: out pointers to locals; the descriptor is freed below.
        unsafe {
            let status = GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            );
            if status != ERROR_SUCCESS {
                return Err(format!("owner of {}: error {status}", path.display()));
            }
            let trusted = IsWellKnownSid(owner, WinLocalSystemSid) != 0
                || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0;
            LocalFree(descriptor);
            Ok(trusted)
        }
    }

    /// Created with a protected DACL: SYSTEM and Administrators only.
    pub(super) fn create_private_dir(path: &Path) -> std::io::Result<()> {
        use winapi::shared::sddl::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use winapi::um::fileapi::CreateDirectoryW;
        use winapi::um::minwinbase::SECURITY_ATTRIBUTES;
        use winapi::um::winbase::LocalFree;
        let sddl = wide("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
        let name = wide(path);
        let mut descriptor = ptr::null_mut();
        // SAFETY: NUL-terminated strings; the descriptor is freed below.
        unsafe {
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let created = CreateDirectoryW(name.as_ptr(), &mut attributes);
            let error = std::io::Error::last_os_error();
            LocalFree(descriptor);
            if created == 0 {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn make_private(path: &Path) -> Result<(), String> {
        trusted_dir(path)?;
        crate::core::harden_permissions(path).map_err(|error| format!("{error:#}"))
    }

    /// Inherits current/'s DACL.
    pub(super) fn create_new_private_file(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }

    /// The reparse point itself, not its target; writers shut out while
    /// it is read.
    pub(super) fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        use winapi::um::winbase::FILE_FLAG_OPEN_REPARSE_POINT;
        use winapi::um::winnt::FILE_SHARE_READ;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
    }

    pub(super) fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
        use std::os::windows::fs::MetadataExt;
        use winapi::um::winnt::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    /// Interrupt time without the time spent asleep.
    pub(super) fn awake() -> Duration {
        let mut ticks = 0u64;
        // SAFETY: writes one u64.
        unsafe { winapi::um::realtimeapiset::QueryUnbiasedInterruptTime(&mut ticks) };
        Duration::from_nanos(ticks.saturating_mul(100))
    }

    /// No console, its own process group: the installer stops the service
    /// without touching it.
    pub(super) fn detach(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        use winapi::um::winbase::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    /// A handle on the process, waited on for its exit.
    pub(super) struct Waiter(HANDLE);

    // SAFETY: a process handle may be used from any thread.
    unsafe impl Send for Waiter {}

    impl Drop for Waiter {
        fn drop(&mut self) {
            // SAFETY: owned handle.
            unsafe { winapi::um::handleapi::CloseHandle(self.0) };
        }
    }

    impl Waiter {
        /// Whether the process exits within `limit` of awake time (the
        /// wait's own timeout leaves out time asleep too).
        pub(super) fn wait(&self, limit: Duration) -> bool {
            use winapi::shared::winerror::WAIT_TIMEOUT;
            use winapi::um::synchapi::WaitForSingleObject;
            use winapi::um::winbase::WAIT_OBJECT_0;
            let started = awake();
            loop {
                // SAFETY: owned handle.
                match unsafe { WaitForSingleObject(self.0, 1000) } {
                    WAIT_OBJECT_0 => return true,
                    WAIT_TIMEOUT => {}
                    other => {
                        log::warn!("waiting for the client: {other:#x}");
                        return false;
                    }
                }
                if awake().saturating_sub(started) >= limit {
                    return false;
                }
            }
        }
    }

    /// The pipe's client process, with a wait on it.
    pub(crate) fn client(pipe: HANDLE) -> Option<Client> {
        use winapi::um::processthreadsapi::{OpenProcess, ProcessIdToSessionId};
        use winapi::um::winbase::{GetNamedPipeClientProcessId, QueryFullProcessImageNameW};
        use winapi::um::winnt::{PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE};
        let mut pid = 0u32;
        // SAFETY: out pointers to locals; the process handle is owned by
        // the Waiter.
        unsafe {
            if GetNamedPipeClientProcessId(pipe, &mut pid) == 0 || pid == 0 {
                return None;
            }
            let process = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return None;
            }
            let waiter = Waiter(process);
            let mut session = 0u32;
            let user = (ProcessIdToSessionId(pid, &mut session) != 0).then_some(session);
            let mut buffer = vec![0u16; 32_768];
            let mut length = buffer.len() as u32;
            let app = (QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length)
                != 0)
                .then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])));
            Some(Client {
                pid,
                user,
                app,
                waiter,
            })
        }
    }

    pub(super) struct Checks;

    impl PackageChecks for Checks {
        /// As the service checks its client (#489).
        fn publisher(&self, package: &Path) -> Result<(), String> {
            crate::ipc::verify_authenticode(package).map_err(|error| format!("{error:#}"))
        }

        /// The installer's fixed FileVersion.
        fn version(&self, package: &Path, _scratch: &Path) -> Result<String, String> {
            file_version(package).ok_or_else(|| "no FileVersion".into())
        }
    }

    /// VS_FIXEDFILEINFO (verrsrc.h; winapi 0.3 does not have it).
    #[allow(dead_code)]
    #[repr(C)]
    struct FixedFileInfo {
        signature: u32,
        struct_version: u32,
        file_version_ms: u32,
        file_version_ls: u32,
        rest: [u32; 9],
    }

    fn file_version(path: &Path) -> Option<String> {
        use winapi::um::winver::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
        let name = wide(path);
        // SAFETY: the buffer is as large as Windows asked for; the fixed
        // info points into it and is checked for size.
        unsafe {
            let mut ignored = 0u32;
            let size = GetFileVersionInfoSizeW(name.as_ptr(), &mut ignored);
            if size == 0 {
                return None;
            }
            let mut data = vec![0u8; size as usize];
            if GetFileVersionInfoW(name.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
                return None;
            }
            let root = wide("\\");
            let mut info = ptr::null_mut();
            let mut length = 0u32;
            if VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut info, &mut length) == 0
                || (length as usize) < std::mem::size_of::<FixedFileInfo>()
            {
                return None;
            }
            let fixed = std::ptr::read_unaligned(info as *const FixedFileInfo);
            if fixed.signature != 0xfeef_04bd {
                return None;
            }
            Some(format!(
                "{}.{}.{}.{}",
                fixed.file_version_ms >> 16,
                fixed.file_version_ms & 0xffff,
                fixed.file_version_ls >> 16,
                fixed.file_version_ls & 0xffff
            ))
        }
    }

    /// The installer, silent; it stops, replaces and restarts the service
    /// itself.
    pub(super) fn install(current: &Path, deadline: Duration) -> Result<(), String> {
        let status = run_until(
            Command::new(current.join(PACKAGE))
                .args(["/S", "/SERVICEUPDATE"])
                .current_dir(current),
            deadline,
        )?;
        if !status.success() {
            return Err(format!("setup exit {}", exit_code(status)));
        }
        Ok(())
    }

    /// Starts the app in the client's session with that user's token, as
    /// the user (not SYSTEM); not when the session is gone.
    pub(super) fn relaunch(job: &Job, _installed: bool) {
        use winapi::um::handleapi::CloseHandle;
        use winapi::um::processthreadsapi::{
            CreateProcessAsUserW, PROCESS_INFORMATION, STARTUPINFOW,
        };
        use winapi::um::userenv::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
        use winapi::um::winbase::CREATE_UNICODE_ENVIRONMENT;
        use winapi::um::wtsapi32::WTSQueryUserToken;
        let (Some(session), Some(app)) = (job.user, job.client_app.as_ref()) else {
            return;
        };
        if !app.is_file() {
            return;
        }
        // SAFETY: out pointers to locals; every handle and the
        // environment block are released below.
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if WTSQueryUserToken(session, &mut token) == 0 {
                log::info!("not reopening the app: session {session} has no user");
                return;
            }
            let mut environment = ptr::null_mut();
            let have_environment = CreateEnvironmentBlock(&mut environment, token, 0) != 0;
            let mut desktop = wide("winsta0\\default");
            let mut startup: STARTUPINFOW = std::mem::zeroed();
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            startup.lpDesktop = desktop.as_mut_ptr();
            let application = wide(app);
            let mut command_line = wide(format!("\"{}\"", app.display()));
            let directory = app.parent().map(wide);
            let mut info: PROCESS_INFORMATION = std::mem::zeroed();
            let created = CreateProcessAsUserW(
                token,
                application.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
                CREATE_UNICODE_ENVIRONMENT,
                if have_environment {
                    environment
                } else {
                    ptr::null_mut()
                },
                directory.as_ref().map_or(ptr::null(), |dir| dir.as_ptr()),
                &mut startup,
                &mut info,
            );
            if created != 0 {
                log::info!("reopened {}", app.display());
                CloseHandle(info.hProcess);
                CloseHandle(info.hThread);
            } else {
                log::warn!(
                    "reopen {}: {}",
                    app.display(),
                    std::io::Error::last_os_error()
                );
            }
            if have_environment {
                DestroyEnvironmentBlock(environment);
            }
            CloseHandle(token);
        }
    }
}

/// Linux and the rest: no silent updates (`UPDATE_UNSUPPORTED`).
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::{Job, PackageChecks};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    pub(super) fn staging_root() -> Option<PathBuf> {
        None
    }

    pub(super) fn trusted_dir(_path: &Path) -> Result<(), String> {
        Err("unsupported".into())
    }

    pub(super) fn create_private_dir(path: &Path) -> std::io::Result<()> {
        std::fs::create_dir(path)
    }

    pub(super) fn make_private(_path: &Path) -> Result<(), String> {
        Err("unsupported".into())
    }

    pub(super) fn create_new_private_file(path: &Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
    }

    pub(super) fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(path)
        }
    }

    pub(super) fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
        false
    }

    pub(super) fn awake() -> Duration {
        static START: once_cell::sync::Lazy<std::time::Instant> =
            once_cell::sync::Lazy::new(std::time::Instant::now);
        START.elapsed()
    }

    pub(super) fn detach(_command: &mut std::process::Command) {}

    pub(super) struct Waiter;

    impl Waiter {
        pub(super) fn wait(&self, _limit: Duration) -> bool {
            false
        }
    }

    pub(super) struct Checks;

    impl PackageChecks for Checks {
        fn publisher(&self, _package: &Path) -> Result<(), String> {
            Err("unsupported".into())
        }

        fn version(&self, _package: &Path, _scratch: &Path) -> Result<String, String> {
            Err("unsupported".into())
        }
    }

    pub(super) fn install(_current: &Path, _deadline: Duration) -> Result<(), String> {
        Err("unsupported".into())
    }

    pub(super) fn relaunch(_job: &Job, _installed: bool) {}
}

#[cfg(target_os = "macos")]
pub(crate) use platform::client as client_of_socket;
#[cfg(windows)]
pub(crate) use platform::client as client_of_pipe;

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    /// `ACTIVE` is one per process: tests that use it take turns.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn temp_staging(name: &str) -> Staging {
        let root = std::env::temp_dir().join(format!(
            "ppvpn-update-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        Staging {
            root,
            system: false,
        }
    }

    struct Signer {
        pair: ring::signature::Ed25519KeyPair,
    }

    impl Signer {
        fn new() -> Self {
            let rng = ring::rand::SystemRandom::new();
            let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            Signer {
                pair: ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
            }
        }

        fn key(&self) -> [u8; 32] {
            self.pair.public_key().as_ref().try_into().unwrap()
        }

        fn sign(&self, bytes: &[u8]) -> String {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(self.pair.sign(bytes).as_ref())
        }
    }

    /// Steps 5 and 6 as told.
    struct FakeChecks {
        publisher: Result<(), String>,
        version: Result<String, String>,
    }

    impl PackageChecks for FakeChecks {
        fn publisher(&self, package: &Path) -> Result<(), String> {
            assert!(package.is_file(), "checks the copy");
            self.publisher.clone()
        }

        fn version(&self, _package: &Path, _scratch: &Path) -> Result<String, String> {
            self.version.clone()
        }
    }

    fn package_version(build: u64) -> String {
        if cfg!(target_os = "macos") {
            build.to_string()
        } else {
            format!("0.7.5.{build}")
        }
    }

    fn good_checks() -> FakeChecks {
        FakeChecks {
            publisher: Ok(()),
            version: Ok(package_version(340)),
        }
    }

    /// A downloaded package in `dir` and the payload that describes it.
    fn download(dir: &Path, signer: &Signer, bytes: &[u8]) -> InstallUpdatePayload {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("download.bin");
        std::fs::write(&path, bytes).unwrap();
        InstallUpdatePayload {
            path: path.to_string_lossy().into_owned(),
            version: "0.7.5".into(),
            build: 340,
            length: bytes.len() as u64,
            ed_signature: signer.sign(bytes),
            relaunch: true,
        }
    }

    fn check_with(
        staging: &Staging,
        signer: &Signer,
        payload: InstallUpdatePayload,
        checks: &FakeChecks,
    ) -> Result<Staged, &'static str> {
        check_in(
            staging.clone(),
            &signer.key(),
            332,
            payload,
            &|| false,
            checks,
        )
    }

    #[test]
    fn a_good_package_is_copied_and_accepted() {
        let _serial = serial();
        let staging = temp_staging("good");
        let signer = Signer::new();
        let bytes = b"a signed installer".repeat(1000);
        let payload = download(&staging.root.join("cache"), &signer, &bytes);
        let staged = check_with(&staging, &signer, payload, &good_checks()).unwrap();
        assert_eq!(
            std::fs::read(staging.current().join(PACKAGE)).unwrap(),
            bytes
        );
        assert!(ACTIVE.lock().is_some(), "in progress while checked");
        staged.discard("test");
        assert!(ACTIVE.lock().is_none());
        assert!(!staging.current().exists(), "the copy goes with it");
        let _ = std::fs::remove_dir_all(&staging.root);
    }

    #[test]
    fn refusals_carry_their_codes_and_leave_nothing() {
        let _serial = serial();
        let staging = temp_staging("refusals");
        let signer = Signer::new();
        let bytes = b"installer".repeat(100);
        let cache = staging.root.join("cache");
        let base = download(&cache, &signer, &bytes);
        let refused = |payload: InstallUpdatePayload, checks: &FakeChecks| {
            let code = check_with(&staging, &signer, payload, checks)
                .err()
                .expect("refused");
            assert!(ACTIVE.lock().is_none(), "{code}: not in progress after");
            assert!(!staging.current().exists(), "{code}: no copy left");
            code
        };
        let with = |change: &dyn Fn(&mut InstallUpdatePayload)| {
            let mut payload = base.clone();
            change(&mut payload);
            payload
        };

        assert_eq!(refused(with(&|p| p.build = 332), &good_checks()), NOT_NEWER);
        assert_eq!(refused(with(&|p| p.build = 100), &good_checks()), NOT_NEWER);
        assert_eq!(
            refused(with(&|p| p.length += 1), &good_checks()),
            FILE_INVALID
        );
        assert_eq!(
            refused(with(&|p| p.length = 0), &good_checks()),
            FILE_INVALID
        );
        assert_eq!(
            refused(with(&|p| p.path = "download.bin".into()), &good_checks()),
            FILE_INVALID
        );
        assert_eq!(
            refused(
                with(&|p| p.path = cache.to_string_lossy().into_owned()),
                &good_checks()
            ),
            FILE_INVALID,
            "a directory"
        );
        assert_eq!(
            refused(
                with(&|p| p.path = cache.join("missing").to_string_lossy().into_owned()),
                &good_checks()
            ),
            FILE_INVALID
        );
        assert_eq!(
            refused(
                with(&|p| p.ed_signature = signer.sign(b"something else")),
                &good_checks()
            ),
            SIGNATURE_INVALID
        );
        assert_eq!(
            refused(
                with(&|p| p.ed_signature = "not base64".into()),
                &good_checks()
            ),
            SIGNATURE_INVALID
        );
        let other = Signer::new();
        assert_eq!(
            refused(
                with(&|p| p.ed_signature = other.sign(&bytes)),
                &good_checks()
            ),
            SIGNATURE_INVALID,
            "another key"
        );
        assert_eq!(
            refused(
                base.clone(),
                &FakeChecks {
                    publisher: Err("not ours".into()),
                    version: Ok(package_version(340)),
                }
            ),
            PUBLISHER_MISMATCH
        );
        assert_eq!(
            refused(
                base.clone(),
                &FakeChecks {
                    publisher: Ok(()),
                    version: Ok(package_version(341)),
                }
            ),
            VERSION_MISMATCH
        );
        assert_eq!(
            refused(
                base.clone(),
                &FakeChecks {
                    publisher: Ok(()),
                    version: Err("unreadable".into()),
                }
            ),
            VERSION_MISMATCH
        );
        assert_eq!(
            refused(with(&|p| p.version = "0.7".into()), &good_checks()),
            VERSION_MISMATCH
        );
        let _ = std::fs::remove_dir_all(&staging.root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_not_followed() {
        let _serial = serial();
        let staging = temp_staging("link");
        let signer = Signer::new();
        let bytes = b"installer".repeat(10);
        let mut payload = download(&staging.root.join("cache"), &signer, &bytes);
        let link = staging.root.join("cache/link.bin");
        std::os::unix::fs::symlink(&payload.path, &link).unwrap();
        payload.path = link.to_string_lossy().into_owned();
        assert_eq!(
            check_with(&staging, &signer, payload, &good_checks()).err(),
            Some(FILE_INVALID)
        );
        let _ = std::fs::remove_dir_all(&staging.root);
    }

    #[test]
    fn busy_and_in_progress_come_first() {
        let _serial = serial();
        let staging = temp_staging("busy");
        let signer = Signer::new();
        let payload = download(&staging.root.join("cache"), &signer, b"installer");
        let busy = check_in(
            staging.clone(),
            &signer.key(),
            332,
            payload.clone(),
            &|| true,
            &good_checks(),
        );
        assert_eq!(busy.err(), Some(BUSY));
        assert!(ACTIVE.lock().is_none());

        let first = check_with(&staging, &signer, payload.clone(), &good_checks()).unwrap();
        assert_eq!(
            check_with(&staging, &signer, payload.clone(), &good_checks()).err(),
            Some(IN_PROGRESS)
        );
        drop(first);
        let again = check_with(&staging, &signer, payload, &good_checks()).unwrap();
        drop(again);
        let _ = std::fs::remove_dir_all(&staging.root);
    }

    #[test]
    fn a_running_runner_is_in_progress_and_a_dead_one_is_settled() {
        let _serial = serial();
        let staging = temp_staging("runner");
        std::fs::create_dir_all(staging.current()).unwrap();
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            version: "0.7.5".into(),
            build: 340,
            relaunch: false,
            phase: Phase::Installing,
            user: None,
            client_app: None,
        };
        write_job(&staging, &job).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(staging.lock_path())
            .unwrap();
        lock.lock().unwrap();

        let (progress, last) = status_in(&staging);
        assert_eq!(
            progress,
            Some(UpdateProgress {
                version: "0.7.5".into(),
                build: 340,
                phase: "installing".into(),
            })
        );
        assert_eq!(last, None);

        // The runner died mid-install (the machine restarted).
        drop(lock);
        let (progress, last) = status_in(&staging);
        assert_eq!(progress, None);
        let last = last.expect("recorded");
        assert_eq!(last.outcome, "failed");
        assert_eq!(
            last.error.as_deref(),
            Some("UPDATE_INSTALL_FAILED:interrupted")
        );
        assert!(!staging.current().exists());

        // A runner that finished leaves its result alone.
        std::fs::create_dir_all(staging.current()).unwrap();
        let finished = Job {
            phase: Phase::Finished,
            build: 341,
            ..job
        };
        write_job(&staging, &finished).unwrap();
        record(&staging, &finished, Ok(()));
        let (progress, last) = status_in(&staging);
        assert_eq!(progress, None);
        let last = last.unwrap();
        assert_eq!((last.build, last.outcome.as_str()), (341, "installed"));
        assert_eq!(last.error, None);
        assert!(!staging.current().exists());
        let _ = std::fs::remove_dir_all(&staging.root);
    }

    #[test]
    fn the_result_is_the_contracts_shape() {
        let staging = temp_staging("shape");
        std::fs::create_dir_all(&staging.root).unwrap();
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            version: "0.7.5".into(),
            build: 340,
            relaunch: true,
            phase: Phase::Installing,
            user: Some(501),
            client_app: None,
        };
        record(
            &staging,
            &job,
            Err("UPDATE_INSTALL_FAILED:setup exit 2".into()),
        );
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(staging.result_path()).unwrap()).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["build", "error", "finished_at_ms", "outcome", "version"]
        );
        assert_eq!(value["outcome"], "failed");
        assert!(value["finished_at_ms"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&staging.root);
    }

    #[test]
    fn versions_and_keys() {
        assert_eq!(version_parts("0.7.5"), Some(vec![0, 7, 5]));
        for bad in ["0.7", "0.7.5.1", "0.7.x", "", "0..5", "+1.2.3"] {
            assert_eq!(version_parts(bad), None, "{bad}");
        }
        assert!(version_matches("0.7.5.340", &[0, 7, 5, 340]));
        assert!(version_matches("340", &[340]));
        assert!(!version_matches("0.7.5.341", &[0, 7, 5, 340]));
        assert!(!version_matches("0.7.5", &[0, 7, 5, 340]));

        let signer = Signer::new();
        use base64::Engine;
        let text = base64::engine::general_purpose::STANDARD.encode(signer.key());
        assert_eq!(decode_public_key(&format!(" {text}\n")), Some(signer.key()));
        assert_eq!(decode_public_key("c2hvcnQ="), None);
        assert!(signature_valid(&signer.key(), b"x", &signer.sign(b"x")));
        assert!(!signature_valid(&signer.key(), b"y", &signer.sign(b"x")));
    }

    #[test]
    fn a_build_without_its_number_or_key_is_unsupported() {
        if build_number() == 0 || public_key().is_none() {
            assert_eq!(
                check(InstallUpdatePayload::default(), &|| false).err(),
                Some(UNSUPPORTED)
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn spctl_and_package_info() {
        use platform::{bundle_of, pkg_info_version, spctl_accepts};
        let accepted = "/tmp/PPVPN.pkg: accepted\nsource=Notarized Developer ID\n\
                        origin=Developer ID Installer: PeakPass Labs LLC (HSVMRQBQJ2)\n";
        assert!(spctl_accepts(accepted, "HSVMRQBQJ2"));
        assert!(!spctl_accepts(accepted, "ABCDE12345"));
        assert!(!spctl_accepts(
            &accepted.replace("Notarized Developer ID", "Developer ID"),
            "HSVMRQBQJ2"
        ));
        assert!(!spctl_accepts(
            &accepted.replace("Installer", "Application"),
            "HSVMRQBQJ2"
        ));
        assert!(!spctl_accepts(
            &accepted.replace(": accepted", ": rejected"),
            "HSVMRQBQJ2"
        ));

        let info = r#"<?xml version="1.0" encoding="utf-8"?>
<pkg-info overwrite-permissions="true" relocatable="false" identifier="com.peakpassvpn.ppvpn.desktop.pkg" postinstall-action="none" version="340" format-version="2" generator-version="InstallCmds-860" auth="root">
    <payload numberOfFiles="10" installKBytes="1000"/>
</pkg-info>"#;
        assert_eq!(pkg_info_version(info).as_deref(), Some("340"));
        let reordered = r#"<pkg-info format-version='2' version='341'>"#;
        assert_eq!(pkg_info_version(reordered).as_deref(), Some("341"));
        assert_eq!(pkg_info_version(r#"<pkg-info format-version="2">"#), None);

        assert_eq!(
            bundle_of(Path::new("/Applications/PPVPN.app/Contents/MacOS/PPVPN")),
            Some(PathBuf::from("/Applications/PPVPN.app"))
        );
        assert_eq!(bundle_of(Path::new("/usr/local/bin/ppvpn")), None);
    }

    include!("update_guard_tests.rs");
    #[cfg(target_os = "macos")]
    include!("update_macos_package_tests.rs");
}
