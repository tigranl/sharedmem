//! Every git call lives here (D11). Shells out so it matches the user's git exactly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

pub fn toplevel(cwd: &Path) -> Option<PathBuf> {
    git(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

/// `None` when HEAD is detached.
pub fn branch(top: &Path) -> Option<String> {
    git(top, &["symbolic-ref", "--short", "-q", "HEAD"])
}

/// `None` in a repo without commits.
pub fn head(top: &Path) -> Option<String> {
    git(top, &["rev-parse", "-q", "--verify", "HEAD"])
}

/// Normalized `origin` URL, else the root commit (D8). `None` for a repo
/// with neither, which then gets personal memories only.
pub fn repo_id(top: &Path) -> Option<String> {
    git(top, &["remote", "get-url", "origin"])
        .and_then(|url| sm_core::repo_id_from_url(&url))
        .or_else(|| {
            let roots = git(top, &["rev-list", "--max-parents=0", "HEAD"])?;
            // several roots after merging unrelated histories: pick one every clone agrees on
            roots.lines().min().map(sm_core::repo_id_from_root)
        })
}
