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
