//! `sm-hook <event>`: run by Claude Code hooks. Reads the hook JSON on stdin,
//! always exits 0, writes only hook JSON to stdout and logs errors to
//! `~/.local/state/sharedmem/hook.log`. Fails open: on any error it prints nothing.
//!
//! Every run of an event is reported for the server's metrics (D13): see
//! `report`. `sm-hook flush` sends the spooled reports; runs spawn it.

mod api;
mod git;
mod remembered;
mod report;
mod session_start;

use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sm_proto::{AnchorChecks, HookEvent, HookOutcome, HookRun};

use crate::api::Api;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// What a successful run produced: the hook output, and what its report carries.
#[derive(Default)]
pub struct Ran {
    pub output: Option<String>,
    /// Memories in the injected block. Session start only.
    pub injected: Option<u32>,
    pub anchors: AnchorChecks,
}

/// Set by whichever reports first, the run or its watchdog, so a run is reported once.
static REPORTED: AtomicBool = AtomicBool::new(false);

fn main() {
    let start = Instant::now();
    let event = std::env::args().nth(1).unwrap_or_default();
    // SessionStart is on the session's clock (section 6); async hooks are not.
    let (network, total) = match event.as_str() {
        "session-start" => (Duration::from_millis(300), Duration::from_millis(500)),
        _ => (Duration::from_secs(2), Duration::from_secs(3)),
    };
    let reported = match event.as_str() {
        "session-start" => Some(HookEvent::SessionStart),
        "remembered" => Some(HookEvent::Remembered),
        _ => None,
    };
    let watchdog_event = event.clone();
    std::thread::spawn(move || {
        std::thread::sleep(total);
        log(&format!("{watchdog_event}: gave up after {total:?}"));
        if let Some(e) = reported {
            report(e, HookOutcome::Timeout, total, Ran::default());
        }
        std::process::exit(0);
    });
    if event == "flush" {
        if let Err(e) = flush(network) {
            log(&format!("flush: {e}"));
        }
        return;
    }
    let (outcome, ran) = match run(&event, network) {
        Ok(ran) => {
            if let Some(out) = &ran.output {
                let mut stdout = std::io::stdout().lock();
                let _ = stdout
                    .write_all(out.as_bytes())
                    .and_then(|_| stdout.flush());
            }
            (HookOutcome::Ok, ran)
        }
        Err(e) => {
            log(&format!("{event}: {e}"));
            (report::outcome(&*e), Ran::default())
        }
    };
    if let Some(e) = reported {
        report(e, outcome, start.elapsed(), ran);
    }
}

fn run(event: &str, network: Duration) -> Result<Ran> {
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    let input: Value = serde_json::from_str(&stdin)?;
    let api = Api::from_env(network)?;
    match event {
        "session-start" => session_start::run(&input, &api),
        "remembered" => remembered::run(&input, &api).map(|()| Ran::default()),
        _ => Err(format!("unknown event {event:?}").into()),
    }
}

/// Spools one run's report, unless the run or its watchdog already did.
fn report(event: HookEvent, outcome: HookOutcome, took: Duration, ran: Ran) {
    if REPORTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(dir) = state_dir() else {
        return;
    };
    let run = HookRun {
        event,
        outcome,
        duration_ms: u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
        injected: ran.injected,
        anchors: ran.anchors,
    };
    if let Err(e) = report::record(&dir, &run) {
        log(&format!("report: {e}"));
    }
}

fn flush(network: Duration) -> Result<()> {
    let Some(dir) = state_dir() else {
        return Ok(());
    };
    // unconfigured: the run that spawned this flush has already logged it
    let Ok(api) = Api::from_env(network) else {
        return Ok(());
    };
    report::flush(&dir, &api)
}

/// `~/.local/state/sharedmem`, for the log and the report spool.
fn state_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/sharedmem"))
}

fn log(msg: &str) {
    let Some(dir) = state_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hook.log"))
    {
        let _ = writeln!(f, "{secs} {msg}");
    }
}
