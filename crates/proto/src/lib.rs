//! Wire types shared by `sm-hook` and `sm-server`.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Who a memory is for: one repo, or the user everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Personal,
    Repo,
}

impl Scope {
    /// The lowercase name used on the wire and in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Personal => "personal",
            Scope::Repo => "repo",
        }
    }
}

impl FromStr for Scope {
    /// A message for the caller.
    type Err = String;

    fn from_str(s: &str) -> Result<Scope, String> {
        match s {
            "personal" => Ok(Scope::Personal),
            "repo" => Ok(Scope::Repo),
            _ => Err("scope must be \"repo\" or \"personal\"".into()),
        }
    }
}

/// Whether an anchored file still matches its baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Staleness {
    Fresh,
    Changed,
    Deleted,
    /// No baseline, or the file could not be checked.
    Unknown,
}

/// A file a memory is about. The baseline stays `None` until the hook posts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Repo-relative path.
    pub path: String,
    /// Git blob hash of the file when the memory was saved.
    pub blob_at_save: Option<String>,
    /// HEAD when the memory was saved.
    pub commit_at_save: Option<String>,
}

/// One saved fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub scope: Scope,
    pub repo: Option<String>,
    /// Saving again with the same key replaces this memory.
    pub key: Option<String>,
    pub text: String,
    /// Unix milliseconds.
    pub created_at: i64,
    pub anchors: Vec<Anchor>,
}

/// `GET /v1/context` response: the repo's memories, then personal ones, newest first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub memories: Vec<Memory>,
}

/// `POST /v1/memories/{id}/anchors` body: what the anchored files hashed to when saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorBaseline {
    /// HEAD when the memory was saved; `None` in a repo without commits.
    pub commit: Option<String>,
    pub blobs: Vec<PathBlob>,
}

/// A repo-relative path and its git blob hash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathBlob {
    pub path: String,
    pub blob: String,
}

/// A hook event whose runs `sm-hook` reports (D13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    Remembered,
    /// A name this server doesn't know, from a newer hook. Counted, never a new label.
    #[serde(other)]
    Other,
}

impl HookEvent {
    /// The snake_case name used on the wire and as a metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session_start",
            HookEvent::Remembered => "remembered",
            HookEvent::Other => "other",
        }
    }
}

/// How a hook run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcome {
    Ok,
    /// The server could not be reached, or did not answer in time.
    Unreachable,
    /// The server answered with an error status.
    ServerError,
    /// The run hit its watchdog; a session start injected nothing.
    Timeout,
    /// Anything else, such as bad hook input or a memory saved for another repo.
    Error,
    /// A name this server doesn't know, from a newer hook.
    #[serde(other)]
    Other,
}

impl HookOutcome {
    /// The snake_case name used on the wire and as a metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            HookOutcome::Ok => "ok",
            HookOutcome::Unreachable => "unreachable",
            HookOutcome::ServerError => "server_error",
            HookOutcome::Timeout => "timeout",
            HookOutcome::Error => "error",
            HookOutcome::Other => "other",
        }
    }
}

/// Anchors a session start checked against the files on disk, by result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorChecks {
    pub fresh: u32,
    pub changed: u32,
    pub deleted: u32,
    pub unknown: u32,
}

impl AnchorChecks {
    /// Counts one check.
    pub fn add(&mut self, s: Staleness) {
        let n = match s {
            Staleness::Fresh => &mut self.fresh,
            Staleness::Changed => &mut self.changed,
            Staleness::Deleted => &mut self.deleted,
            Staleness::Unknown => &mut self.unknown,
        };
        *n += 1;
    }
}

/// One `sm-hook` run, reported after the fact so the server can count it (D13).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookRun {
    pub event: HookEvent,
    pub outcome: HookOutcome,
    pub duration_ms: u64,
    /// Memories in the injected block, after trimming to the budget. Session start only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injected: Option<u32>,
    /// Session start only; zero otherwise.
    #[serde(default)]
    pub anchors: AnchorChecks,
}

/// Body of `POST /v1/hook-runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookRuns {
    pub runs: Vec<HookRun>,
}
