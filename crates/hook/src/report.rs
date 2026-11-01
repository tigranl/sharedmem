//! Reports of the hook's own runs (D13). Each run appends one line to a spool
//! file and spawns a detached `sm-hook flush`, so no run waits on the network
//! for its report. Runs that failed to reach the server are sent by a later
//! flush.

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use sm_proto::{HookOutcome, HookRun, HookRuns};

use crate::Result;
use crate::api::Api;

const SPOOL: &str = "runs.jsonl";
/// shortcut: past this size new runs are dropped, not the oldest; keep the newest if a long outage loses ones that matter.
const MAX_SPOOL_BYTES: u64 = 256 * 1024;
/// Runs per POST; the server takes up to 1,000.
const MAX_BATCH: usize = 500;

/// Spools `run` in `dir` and starts a detached flush. Errors are the caller's to log.
pub fn record(dir: &Path, run: &HookRun) -> Result<()> {
    let mut line = serde_json::to_string(run)?;
    line.push('\n');
    append(dir, &line)?;
    // null stdio: Claude Code waits for the hook's stdout to close, and must not wait for the flush
    Command::new(std::env::current_exe()?)
        .arg("flush")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Sends every spooled run. Runs the server refused (4xx) are dropped, since
/// resending cannot help; runs that failed to arrive go back to the spool.
pub fn flush(dir: &Path, api: &Api) -> Result<()> {
    let taken = take(dir)?;
    // a line cut short by a crash mid-write is skipped, not fatal
    let runs: Vec<HookRun> = taken
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    for (i, chunk) in runs.chunks(MAX_BATCH).enumerate() {
        let body = HookRuns {
            runs: chunk.to_vec(),
        };
        if let Err(e) = api.post_hook_runs(&body) {
            if retryable(&*e) {
                let mut rest = String::new();
                for run in &runs[i * MAX_BATCH..] {
                    rest.push_str(&serde_json::to_string(run)?);
                    rest.push('\n');
                }
                append(dir, &rest)?;
            }
            return Err(e);
        }
    }
    Ok(())
}

/// The outcome a run that failed with `e` reports.
pub fn outcome(e: &(dyn Error + 'static)) -> HookOutcome {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(_)) => HookOutcome::ServerError,
        Some(
            ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed,
        ) => HookOutcome::Unreachable,
        _ => HookOutcome::Error,
    }
}

/// Worth sending again: the server was unreachable or failed (5xx).
fn retryable(e: &(dyn Error + 'static)) -> bool {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(code)) => *code >= 500,
        _ => outcome(e) == HookOutcome::Unreachable,
    }
}

/// Appends `lines` under the spool's lock: hooks of parallel sessions append
/// while a flush takes.
fn append(dir: &Path, lines: &str) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(SPOOL))?;
    f.lock()?;
    if f.metadata()?.len() < MAX_SPOOL_BYTES {
        f.write_all(lines.as_bytes())?;
    }
    Ok(())
}

/// Takes every spooled line, leaving the spool empty. The lock is released on
/// return, so it is never held across the network.
fn take(dir: &Path) -> io::Result<String> {
    let mut f = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(SPOOL))
    {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e),
    };
    f.lock()?;
    let mut taken = String::new();
    f.read_to_string(&mut taken)?;
    f.set_len(0)?;
    Ok(taken)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_proto::{AnchorChecks, HookEvent};

    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(name: &str) -> TempDir {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos();
        TempDir(std::env::temp_dir().join(format!("sm-hook-{name}-{nanos}")))
    }

    fn run(outcome: HookOutcome) -> HookRun {
        HookRun {
            event: HookEvent::SessionStart,
            outcome,
            duration_ms: 12,
            injected: Some(3),
            anchors: AnchorChecks {
                fresh: 2,
                ..AnchorChecks::default()
            },
        }
    }

    #[test]
    fn take_returns_appended_lines_and_empties_the_spool() {
        let dir = temp_dir("take");
        assert_eq!(take(&dir.0).unwrap(), "", "a missing spool is empty");
        append(&dir.0, "a\n").unwrap();
        append(&dir.0, "b\n").unwrap();
        assert_eq!(take(&dir.0).unwrap(), "a\nb\n");
        assert_eq!(take(&dir.0).unwrap(), "", "take left lines behind");
    }

    #[test]
    fn a_full_spool_drops_new_runs() {
        let dir = temp_dir("full");
        let big = "x".repeat(MAX_SPOOL_BYTES as usize);
        append(&dir.0, &big).unwrap();
        append(&dir.0, "dropped\n").unwrap();
        assert_eq!(take(&dir.0).unwrap(), big);
    }

    #[test]
    fn runs_round_trip_through_the_spool() {
        let dir = temp_dir("round-trip");
        let runs = [run(HookOutcome::Ok), run(HookOutcome::Timeout)];
        for r in &runs {
            append(&dir.0, &format!("{}\n", serde_json::to_string(r).unwrap())).unwrap();
        }
        let back: Vec<HookRun> = take(&dir.0)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(back, runs);
    }

    #[test]
    fn failed_flush_keeps_runs_only_when_resending_can_help() {
        let dir = temp_dir("flush");
        let line = format!(
            "{}\n",
            serde_json::to_string(&run(HookOutcome::Ok)).unwrap()
        );
        // nothing listens on port 9 (discard) on loopback
        let down = Api::new(
            "http://127.0.0.1:9",
            "t",
            std::time::Duration::from_millis(200),
        );
        append(&dir.0, &line).unwrap();
        assert!(
            flush(&dir.0, &down).is_err(),
            "posting to a closed port succeeded"
        );
        assert_eq!(
            take(&dir.0).unwrap(),
            line,
            "unreachable: the run must stay spooled"
        );
    }

    #[test]
    fn outcomes_from_errors() {
        let status: Box<dyn Error> = Box::new(ureq::Error::StatusCode(503));
        let refused: Box<dyn Error> = Box::new(ureq::Error::ConnectionFailed);
        let other: Box<dyn Error> = "no cwd in hook input".into();
        assert_eq!(outcome(&*status), HookOutcome::ServerError);
        assert_eq!(outcome(&*refused), HookOutcome::Unreachable);
        assert_eq!(outcome(&*other), HookOutcome::Error);
        assert!(retryable(&*status), "5xx is worth resending");
        let bad: Box<dyn Error> = Box::new(ureq::Error::StatusCode(400));
        assert!(!retryable(&*bad), "4xx would be refused again");
        assert!(
            !retryable(&*other),
            "a local error never reached the server"
        );
    }
}
