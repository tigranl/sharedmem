//! Wire types shared by `sm-hook` and `sm-server`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Personal,
    Repo,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Personal => "personal",
            Scope::Repo => "repo",
        }
    }

    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "personal" => Some(Scope::Personal),
            "repo" => Some(Scope::Repo),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Staleness {
    Fresh,
    Changed,
    Deleted,
    Unknown,
}

/// A file a memory is about. The baseline stays `None` until the hook posts it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Repo-relative path.
    pub path: String,
    pub blob_at_save: Option<String>,
    pub commit_at_save: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub scope: Scope,
    pub repo: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathBlob {
    pub path: String,
    pub blob: String,
}
