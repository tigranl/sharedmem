//! remember / recall / forget: validation and rules on top of the store.

use serde::Serialize;
use sm_core::{Current, clean_anchor_path, resolve_scope, staleness};
use sm_proto::{AnchorBaseline, Memory, Scope, Staleness};

use crate::store::{NewMemory, Principal, Saved, Store};

/// Why a request failed.
#[derive(Debug)]
pub enum Error {
    /// The caller's input is wrong; the message says how.
    Invalid(String),
    /// A storage failure; logged, never shown to the caller.
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Internal(e)
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(msg.into())
}

/// A `remember` request as the caller sent it, before validation.
#[derive(Debug)]
pub struct Remember {
    pub text: String,
    pub repo: Option<String>,
    pub scope: Option<Scope>,
    pub key: Option<String>,
    pub anchors: Vec<String>,
}

const MAX_TEXT: usize = 8_000;
const MAX_ANCHORS: usize = 20;

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Validates and saves a memory.
pub async fn remember(store: &Store, p: &Principal, r: Remember) -> Result<Saved, Error> {
    let text = r.text.trim().to_string();
    if text.is_empty() || text.len() > MAX_TEXT {
        return Err(invalid(format!("text must be 1..{MAX_TEXT} bytes")));
    }
    let repo = non_empty(r.repo);
    let key = non_empty(r.key);
    let scope = resolve_scope(r.scope, repo.as_deref()).map_err(invalid)?;
    if !r.anchors.is_empty() && repo.is_none() {
        return Err(invalid("anchors need a repo: pass the repo id"));
    }
    if r.anchors.len() > MAX_ANCHORS {
        return Err(invalid(format!("at most {MAX_ANCHORS} anchors")));
    }
    let anchors = r
        .anchors
        .iter()
        .map(|a| {
            clean_anchor_path(a)
                .ok_or_else(|| invalid(format!("anchor {a:?} must be a repo-relative path")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let new = NewMemory {
        scope,
        repo,
        key,
        text,
        anchors,
    };
    Ok(store.insert_memory(p, new).await?)
}

/// A recall hit with its anchors' staleness.
#[derive(Debug, Serialize)]
pub struct Recalled {
    #[serde(flatten)]
    pub memory: Memory,
    /// One entry per anchor, in the same order.
    pub staleness: Vec<Staleness>,
}

/// Searches the caller's memories; `limit` defaults to 10 and is clamped to 1..=50.
pub async fn recall(
    store: &Store,
    p: &Principal,
    query: &str,
    repo: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<Recalled>, Error> {
    let limit = limit.unwrap_or(10).clamp(1, 50);
    let memories = store.recall(p, query, repo, limit).await?;
    Ok(memories
        .into_iter()
        .map(|memory| Recalled {
            // shortcut: the server has no git until slice 3 mirrors, so recall is always Unknown
            staleness: memory
                .anchors
                .iter()
                .map(|a| staleness(a, Current::Unknown))
                .collect(),
            memory,
        })
        .collect())
}

/// Soft-deletes one of the caller's memories. False when there is none with that id.
pub async fn forget(store: &Store, p: &Principal, id: &str) -> Result<bool, Error> {
    Ok(store.forget(p, id.trim()).await?)
}

fn is_sha1_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Anchor baselines posted by the hook right after `remember`.
pub async fn set_baselines(
    store: &Store,
    p: &Principal,
    id: &str,
    b: AnchorBaseline,
) -> Result<bool, Error> {
    if b.blobs.iter().any(|pb| !is_sha1_hex(&pb.blob)) {
        return Err(invalid("blobs must be 40-char hex"));
    }
    if b.commit
        .as_deref()
        .is_some_and(|c| !c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(invalid("commit must be hex"));
    }
    Ok(store.set_baselines(p, id, b.commit, b.blobs).await?)
}
