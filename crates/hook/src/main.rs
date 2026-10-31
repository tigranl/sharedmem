//! `sm-hook <event>`: run by Claude Code hooks. Reads the hook JSON on stdin,
//! always exits 0, writes only hook JSON to stdout and logs errors to
//! `~/.local/state/sharedmem/hook.log`. Fails open: on any error it prints nothing.

mod api;
mod git;
mod remembered;
mod session_start;

use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::api::Api;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() {
    let event = std::env::args().nth(1).unwrap_or_default();
    // SessionStart is on the session's clock (section 6); async hooks are not.
    let (network, total) = match event.as_str() {
        "session-start" => (Duration::from_millis(300), Duration::from_millis(500)),
        _ => (Duration::from_secs(2), Duration::from_secs(3)),
    };
    let watchdog_event = event.clone();
    std::thread::spawn(move || {
        std::thread::sleep(total);
        log(&format!("{watchdog_event}: gave up after {total:?}"));
        std::process::exit(0);
    });
    match run(&event, network) {
        Ok(Some(out)) => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout
                .write_all(out.as_bytes())
                .and_then(|_| stdout.flush());
        }
        Ok(None) => {}
        Err(e) => log(&format!("{event}: {e}")),
    }
}

fn run(event: &str, network: Duration) -> Result<Option<String>> {
    let mut stdin = String::new();
    std::io::stdin().read_to_string(&mut stdin)?;
    let input: Value = serde_json::from_str(&stdin)?;
    let api = Api::from_env(network)?;
    match event {
        "session-start" => session_start::run(&input, &api).map(Some),
        "remembered" => remembered::run(&input, &api).map(|()| None),
        _ => Err(format!("unknown event {event:?}").into()),
    }
}

fn log(msg: &str) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = Path::new(&home).join(".local/state/sharedmem");
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
