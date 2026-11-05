//! ne-mem: ppvpn-core and sail in the iOS Network Extension's shape, for
//! measuring memory (#388). One process holds the Engine the way the
//! extension will: a TUN instance on a mobile platform (the host builds the
//! routes; the core touches none), its addresses IPv4 and IPv6 as
//! `tun_settings` gives them (#511), no local proxy, no system proxy, the
//! log to a file. Nothing else runs in it: the loads come from other
//! processes.
//!
//!     ne-mem --state-dir <dir> [--platform ios|android] [--workers N] [--log-file <path>]
//!            [--dhat-file <path>]
//!
//! Commands, one JSON object per line on stdin, one answer per line on
//! stdout (`{"ok":true,...}` or `{"ok":false,"error":"..."}`):
//!
//!     {"cmd":"apply","request":<ApplyRequest>}
//!     {"cmd":"start"} | {"cmd":"stop"} | {"cmd":"select","node":"<id>"}
//!     {"cmd":"status"}
//!     {"cmd":"mark","label":"<scenario>"}   peaks since the last mark, then reset
//!     {"cmd":"quit"}
//!
//! A mark answers with `footprint_kib` (phys_footprint on macOS, the figure
//! jetsam judges; RssAnon on Linux, the nearest to it) now and its peak
//! since the last mark, the heap bytes live now and at their peak (a
//! counting allocator over the system's), and the thread count; on Linux
//! also RSS and VmHWM, on macOS the resident size. The peak is the kernel's
//! where it keeps one per interval (macOS's interval maximum when the call
//! to reset it exists), else the highest of samples taken every
//! millisecond; `peak_source` says which.
//!
//! Built with the `dhat` feature, the allocator is dhat's instead (the heap
//! figures then read 0) and `--dhat-file` gets its heap profile at quit:
//! the allocation sites live at the heap's peak (t-gmax) and in all. Its
//! bookkeeping costs memory and time, so its footprints are not the
//! extension's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ppvpn_core::{
    ApplyRequest, Engine, EngineConfig, LogConfig, LogLevel, LogSink, Platform, Role, TunConfig,
};
use serde_json::{json, Value};

#[path = "ne-mem/trust.rs"]
mod trust;

/// The system allocator, counting the bytes live and their peak.
#[cfg_attr(feature = "dhat", allow(dead_code))]
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

// SAFETY: every call goes to System with the same arguments; only counters
// are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            grew(new_size);
        }
        p
    }
}

#[cfg(not(feature = "dhat"))]
#[global_allocator]
static ALLOC: Counting = Counting;

#[cfg(feature = "dhat")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[cfg(target_os = "macos")]
mod mem {
    //! phys_footprint from proc_pid_rusage (RUSAGE_INFO_V4), the interval
    //! maximum reset through proc_reset_footprint_interval when libSystem
    //! exports it (looked up at run time: it is not in the public headers).
    use std::ffi::{c_int, c_void};
    use std::sync::OnceLock;

    const RUSAGE_INFO_V4: c_int = 4;
    // u64 fields after the 16-byte uuid of rusage_info_v4 (sys/resource.h).
    const RESIDENT_SIZE: usize = 6;
    const PHYS_FOOTPRINT: usize = 7;
    const INTERVAL_MAX_PHYS_FOOTPRINT: usize = 33;

    #[repr(C)]
    struct RusageInfo {
        _uuid: [u8; 16],
        fields: [u64; 48],
    }

    extern "C" {
        fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
    }

    fn rusage() -> Option<RusageInfo> {
        let mut info = RusageInfo {
            _uuid: [0; 16],
            fields: [0; 48],
        };
        // SAFETY: the buffer is larger than rusage_info_v4.
        let rc = unsafe {
            proc_pid_rusage(
                std::process::id() as c_int,
                RUSAGE_INFO_V4,
                (&mut info as *mut RusageInfo).cast(),
            )
        };
        (rc == 0).then_some(info)
    }

    pub fn now_bytes() -> u64 {
        rusage().map_or(0, |r| r.fields[PHYS_FOOTPRINT])
    }

    type Reset = unsafe extern "C" fn(c_int) -> c_int;

    fn reset_fn() -> Option<Reset> {
        static RESET: OnceLock<Option<Reset>> = OnceLock::new();
        *RESET.get_or_init(|| {
            let name = c"proc_reset_footprint_interval";
            // SAFETY: a symbol lookup; the signature is libproc's.
            let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            (!sym.is_null()).then(|| unsafe { std::mem::transmute::<*mut c_void, Reset>(sym) })
        })
    }

    /// The kernel's peak since the last reset, when it keeps one.
    pub fn kernel_peak_bytes() -> Option<u64> {
        reset_fn()?;
        rusage().map(|r| r.fields[INTERVAL_MAX_PHYS_FOOTPRINT])
    }

    pub fn reset_kernel_peak() {
        if let Some(reset) = reset_fn() {
            // SAFETY: our own pid.
            unsafe { reset(std::process::id() as c_int) };
        }
    }

    pub const KERNEL_PEAK: &str = "interval_max_phys_footprint";

    /// Other figures for the mark, in bytes: the resident size, file pages
    /// included.
    pub fn extra() -> Vec<(&'static str, u64)> {
        vec![("resident", rusage().map_or(0, |r| r.fields[RESIDENT_SIZE]))]
    }

    pub fn threads() -> u64 {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as c_int;
        // SAFETY: the buffer is a proc_taskinfo of the size given.
        let n = unsafe {
            libc::proc_pidinfo(
                std::process::id() as c_int,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast(),
                size,
            )
        };
        if n == size {
            info.pti_threadnum as u64
        } else {
            0
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod mem {
    //! The footprint is RssAnon from /proc/self/status: the anonymous
    //! pages, as phys_footprint counts dirty memory and not the binary's
    //! clean file pages (RSS counts those: sail's text alone is megabytes).
    //! Its peak is sampled; RSS and VmHWM (reset through clear_refs) go
    //! along as `rss_kib` and `rss_peak_kib`.
    fn status_kib(field: &str) -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        status
            .lines()
            .find_map(|l| l.strip_prefix(field)?.strip_prefix(':'))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
    }

    pub fn now_bytes() -> u64 {
        status_kib("RssAnon").unwrap_or(0) * 1024
    }

    pub fn kernel_peak_bytes() -> Option<u64> {
        None
    }

    pub fn extra() -> Vec<(&'static str, u64)> {
        vec![
            ("rss", status_kib("VmRSS").unwrap_or(0) * 1024),
            ("rss_peak", status_kib("VmHWM").unwrap_or(0) * 1024),
        ]
    }

    pub fn reset_kernel_peak() {
        let _ = std::fs::write("/proc/self/clear_refs", "5");
    }

    pub const KERNEL_PEAK: &str = "";

    pub fn threads() -> u64 {
        std::fs::read_dir("/proc/self/task").map_or(0, |d| d.count() as u64)
    }
}

/// The highest footprint sampled since the last mark.
static SAMPLED_PEAK: AtomicUsize = AtomicUsize::new(0);

fn sample_forever(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        SAMPLED_PEAK.fetch_max(mem::now_bytes() as usize, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn kib(bytes: u64) -> u64 {
    bytes.div_ceil(1024)
}

fn mark(label: &str) -> Value {
    let now = mem::now_bytes();
    let sampled = (SAMPLED_PEAK.load(Ordering::Relaxed) as u64).max(now);
    let (peak, source) = match mem::kernel_peak_bytes() {
        Some(k) => (k.max(sampled), mem::KERNEL_PEAK),
        None => (sampled, "sampled_1ms"),
    };
    let live = LIVE.load(Ordering::Relaxed) as u64;
    let heap_peak = PEAK.load(Ordering::Relaxed) as u64;
    let mut out = json!({
        "label": label,
        "footprint_kib": kib(now),
        "peak_kib": kib(peak),
        "sampled_peak_kib": kib(sampled),
        "peak_source": source,
        "heap_live_kib": kib(live),
        "heap_peak_kib": kib(heap_peak),
        "threads": mem::threads(),
    });
    for (name, bytes) in mem::extra() {
        out[format!("{name}_kib")] = kib(bytes).into();
    }
    mem::reset_kernel_peak();
    SAMPLED_PEAK.store(mem::now_bytes() as usize, Ordering::Relaxed);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    out
}

struct Args {
    state_dir: String,
    platform: Platform,
    workers: usize,
    log_file: Option<String>,
    #[cfg_attr(not(feature = "dhat"), allow(dead_code))]
    dhat_file: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        state_dir: String::new(),
        platform: Platform::Ios,
        workers: 0,
        log_file: None,
        dhat_file: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--state-dir" => args.state_dir = value()?,
            "--platform" => {
                args.platform = match value()?.as_str() {
                    "ios" => Platform::Ios,
                    "android" => Platform::Android,
                    other => return Err(format!("unknown platform {other:?}")),
                }
            }
            "--workers" => {
                args.workers = value()?.parse().map_err(|e| format!("--workers: {e}"))?
            }
            "--log-file" => args.log_file = Some(value()?),
            "--dhat-file" if cfg!(feature = "dhat") => args.dhat_file = Some(value()?),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    if args.state_dir.is_empty() {
        return Err("usage: ne-mem --state-dir <dir> [--platform ios|android] [--workers N] [--log-file <path>]".into());
    }
    Ok(args)
}

fn main() {
    // Export in a separate, short process: loading the platform trust store
    // must not warm the allocator or contribute to measured engine memory.
    if std::env::args().nth(1).as_deref() == Some("--export-trust-bundle") {
        let command: Vec<String> = std::env::args().skip(1).collect();
        if let Err(error) = trust::export_command(&command) {
            eprintln!("ne-mem trust bundle: {error}");
            std::process::exit(2);
        }
        return;
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    #[cfg(feature = "dhat")]
    let profiler = args.dhat_file.as_ref().map(|path| {
        dhat::Profiler::builder()
            .file_name(path)
            .trim_backtraces(Some(24))
            .build()
    });
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("ne-mem-sampler".into())
            .stack_size(64 << 10)
            .spawn(move || sample_forever(stop))
            .expect("sampler thread")
    };
    // 0 workers: tokio's default, one per CPU. An extension would pick a
    // small number; the driver measures both.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    if args.workers > 0 {
        builder.worker_threads(args.workers);
    }
    let runtime = builder.enable_all().build().expect("runtime");
    let code = runtime.block_on(run(args));
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    // exit() runs no destructors: the profile is written here.
    #[cfg(feature = "dhat")]
    drop(profiler);
    std::process::exit(code);
}

fn answer(value: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn failed(error: impl std::fmt::Display) -> Value {
    json!({ "ok": false, "error": error.to_string() })
}

async fn run(args: Args) -> i32 {
    let sink = match &args.log_file {
        Some(path) => LogSink::File { path: path.into() },
        None => LogSink::None,
    };
    let config = EngineConfig::new(Role::Tun, args.platform, &args.state_dir)
        .with_system_proxy(false)
        .with_tun(TunConfig::new())
        .with_log(LogConfig::new(LogLevel::Info, sink));
    let engine = match Engine::new(config).await {
        Ok(engine) => engine,
        Err(e) => {
            answer(failed(e));
            return 1;
        }
    };
    answer(json!({ "ok": true, "ready": true }));

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(16);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    while let Some(line) = rx.recv().await {
        let command: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                answer(failed(format!("bad command: {e}")));
                continue;
            }
        };
        let reply = match command["cmd"].as_str().unwrap_or("") {
            "apply" => match serde_json::from_value::<ApplyRequest>(command["request"].clone()) {
                Ok(request) => match engine.apply(request).await {
                    Ok(_) => json!({ "ok": true }),
                    Err(e) => failed(e),
                },
                Err(e) => failed(format!("bad request: {e}")),
            },
            "start" => engine
                .start()
                .await
                .map_or_else(failed, |()| json!({ "ok": true })),
            "stop" => engine
                .stop()
                .await
                .map_or_else(failed, |()| json!({ "ok": true })),
            "select" => engine
                .select_node(command["node"].as_str().unwrap_or(""))
                .await
                .map_or_else(failed, |()| json!({ "ok": true })),
            "status" => json!({ "ok": true, "status": engine.status() }),
            "mark" => {
                let mut v = mark(command["label"].as_str().unwrap_or(""));
                v["ok"] = true.into();
                v
            }
            "quit" => break,
            other => failed(format!("unknown command {other:?}")),
        };
        answer(reply);
    }
    let _ = tokio::time::timeout(Duration::from_secs(10), engine.shutdown()).await;
    answer(json!({ "ok": true, "bye": true }));
    0
}
