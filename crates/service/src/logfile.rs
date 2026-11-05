//! Log files of the service (`ppvpn-service.log`) and of the TUN instance it
//! runs (`ppvpn-core.log`, the engine's lines).
//!
//! They live in a directory that neither uninstalling nor reinstalling the
//! service touches, so the evidence of a failed session survives a repair:
//!
//! - macOS: `/Library/Logs/PPVPN` (root, 0755; files 0644)
//! - Linux: `/var/log/ppvpn` (systemd `LogsDirectory=`)
//! - Windows: `%ProgramData%\PPVPN\logs`
//!
//! Each file is capped at [`MAX_BYTES`]; when it is full it becomes
//! `<name>.1.log` (the previous `.1` becomes `.2`, and so on), keeping
//! [`KEEP_FILES`] files per log in all.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Size cap of one log file.
pub const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Files kept per log: the current one and `KEEP_FILES - 1` rolled ones.
pub const KEEP_FILES: usize = 3;

pub const SERVICE_LOG: &str = "ppvpn-service.log";
pub const CORE_LOG: &str = "ppvpn-core.log";

/// Directory of the service's and the core's logs.
#[cfg(target_os = "macos")]
pub fn log_dir() -> PathBuf {
    PathBuf::from("/Library/Logs/PPVPN")
}

#[cfg(target_os = "linux")]
pub fn log_dir() -> PathBuf {
    PathBuf::from(crate::protocol::LINUX_LOG_DIR)
}

#[cfg(windows)]
pub fn log_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("PPVPN")
        .join("logs")
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn log_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Creates the log directory (readable by everyone on macOS, so testers can
/// collect the logs without root) and returns it.
pub fn prepare_log_dir() -> std::io::Result<PathBuf> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(dir)
}

/// `<dir>/<stem>.<index>.<ext>` for `<dir>/<stem>.<ext>`.
fn rolled_path(path: &Path, index: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = match path.extension() {
        Some(ext) => format!("{stem}.{index}.{}", ext.to_string_lossy()),
        None => format!("{stem}.{index}"),
    };
    path.with_file_name(name)
}

/// Opens `path` for appending; new files are readable by everyone (the
/// service runs with umask 077).
fn open_append(path: &Path) -> std::io::Result<File> {
    let file = File::options().create(true).append(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(file)
}

/// An append-only log file with a size cap (see the module docs).
#[derive(Debug)]
pub struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: Option<File>,
    size: u64,
}

impl RotatingLog {
    pub fn open(path: impl Into<PathBuf>, max_bytes: u64, keep: usize) -> std::io::Result<Self> {
        let path = path.into();
        let file = open_append(&path)?;
        let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path,
            max_bytes,
            keep: keep.max(1),
            file: Some(file),
            size,
        })
    }

    /// Appends `bytes`, rolling the file over first when they would not fit.
    pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        if self.size > 0 && self.size.saturating_add(bytes.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        if self.file.is_none() {
            self.file = Some(open_append(&self.path)?);
            self.size = 0;
        }
        if let Some(file) = self.file.as_mut() {
            file.write_all(bytes)?;
            self.size = self.size.saturating_add(bytes.len() as u64);
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.file = None;
        if self.keep > 1 {
            let _ = std::fs::remove_file(rolled_path(&self.path, self.keep - 1));
            for index in (1..self.keep - 1).rev() {
                let from = rolled_path(&self.path, index);
                if from.exists() {
                    std::fs::rename(&from, rolled_path(&self.path, index + 1))?;
                }
            }
            std::fs::rename(&self.path, rolled_path(&self.path, 1))?;
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
        self.file = Some(open_append(&self.path)?);
        self.size = 0;
        Ok(())
    }
}

/// log4rs appender writing through a [`RotatingLog`].
#[derive(Debug)]
struct RotatingAppender {
    log: parking_lot::Mutex<RotatingLog>,
    encoder: Box<dyn log4rs::encode::Encode>,
}

impl log4rs::append::Append for RotatingAppender {
    fn append(&self, record: &log::Record) -> anyhow::Result<()> {
        let mut line = Vec::new();
        self.encoder.encode(
            &mut log4rs::encode::writer::simple::SimpleWriter(&mut line),
            record,
        )?;
        self.log.lock().write(&line)?;
        Ok(())
    }

    fn flush(&self) {}
}

/// Installs the service's logger (info and above) writing to
/// `<log_dir>/ppvpn-service.log`.
pub fn init_service_logger() -> Result<PathBuf, Box<dyn std::error::Error>> {
    init_logger(SERVICE_LOG)
}

/// The same logger, writing to `<log_dir>/<name>` (the update runner's).
pub fn init_logger(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use log4rs::config::{Appender, Config, Root};
    use log4rs::encode::pattern::PatternEncoder;

    let path = prepare_log_dir()?.join(name);
    let appender = RotatingAppender {
        log: parking_lot::Mutex::new(RotatingLog::open(&path, MAX_BYTES, KEEP_FILES)?),
        encoder: Box::new(PatternEncoder::new(
            "[{d(%Y-%m-%d %H:%M:%S%.3f)}][{l}] {m}\n",
        )),
    };
    let config = Config::builder()
        .appender(Appender::builder().build("file", Box::new(appender)))
        .build(
            Root::builder()
                .appender("file")
                .build(log::LevelFilter::Info),
        )?;
    log4rs::init_config(config)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ppvpn-service-log-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn rolled_names_keep_the_extension() {
        let path = Path::new("/var/log/ppvpn/ppvpn-core.log");
        assert_eq!(
            rolled_path(path, 2),
            PathBuf::from("/var/log/ppvpn/ppvpn-core.2.log")
        );
    }

    #[test]
    fn a_full_log_rolls_over_and_keeps_the_newest_files() {
        let dir = temp_dir();
        let path = dir.join("ppvpn-core.log");
        let mut log = RotatingLog::open(&path, 12, 3).unwrap();
        for line in ["aaaaaaaa\n", "bbbbbbbb\n", "cccccccc\n", "dddddddd\n"] {
            log.write(line.as_bytes()).unwrap();
        }
        assert_eq!(read(&path), "dddddddd\n");
        assert_eq!(read(&dir.join("ppvpn-core.1.log")), "cccccccc\n");
        assert_eq!(read(&dir.join("ppvpn-core.2.log")), "bbbbbbbb\n");
        assert!(!dir.join("ppvpn-core.3.log").exists(), "only 3 files kept");

        // A reopened log continues where it was, with its size counted.
        drop(log);
        let mut log = RotatingLog::open(&path, 12, 3).unwrap();
        log.write(b"e\n").unwrap();
        assert_eq!(read(&path), "dddddddd\ne\n");
        log.write(b"f\n").unwrap();
        assert_eq!(read(&path), "f\n");
        assert_eq!(read(&dir.join("ppvpn-core.1.log")), "dddddddd\ne\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn log_files_are_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let path = dir.join("ppvpn-service.log");
        let mut log = RotatingLog::open(&path, 4, 2).unwrap();
        log.write(b"12345").unwrap();
        log.write(b"6").unwrap();
        for file in [path.clone(), dir.join("ppvpn-service.1.log")] {
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644, "{}", file.display());
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
