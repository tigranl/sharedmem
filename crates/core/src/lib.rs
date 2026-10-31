//! Pure logic shared by the hook and the server. No IO, no VCS (D11).

use sha1::{Digest, Sha1};
use sm_proto::{Anchor, Scope, Staleness};

/// Normalized repo id from an `origin` URL (section 3, D8): lowercase, no
/// scheme, credentials, port, trailing `/` or `.git`.
/// `git@github.com:Org/Name.git` → `github.com/org/name`.
pub fn repo_id_from_url(url: &str) -> Option<String> {
    let url = url.trim().to_lowercase();
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').unwrap_or((rest, "")),
        // scp-like `user@host:path`, only when the colon comes before any slash
        None => match url.split_once(':') {
            Some((host, path)) if !host.contains('/') => (host, path),
            _ => ("", url.as_str()),
        },
    };
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split_once(':').map_or(host, |(h, _)| h);
    let path = path.trim_matches('/');
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    let id = match (host.is_empty(), path.is_empty()) {
        (true, true) => return None,
        (true, false) => path.to_string(),
        (false, true) => host.to_string(),
        (false, false) => format!("{host}/{path}"),
    };
    Some(id)
}

/// Repo id for a repo without `origin`: its root commit, the same in every clone.
pub fn repo_id_from_root(root_commit: &str) -> String {
    format!("root:{}", root_commit.trim().to_lowercase())
}

/// Content hash in git's blob format, `sha1("blob <len>\0" + bytes)` (D11).
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()));
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// What the caller knows about an anchored file right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Current<'a> {
    /// The file exists and hashes to this blob.
    Blob(&'a str),
    Missing,
    /// The file could not be checked, e.g. no checkout of its repo here.
    Unknown,
}

/// Compares an anchor's baseline with what the file is now.
pub fn staleness(anchor: &Anchor, current: Current) -> Staleness {
    let Some(saved) = anchor.blob_at_save.as_deref() else {
        return Staleness::Unknown; // no baseline yet
    };
    match current {
        Current::Unknown => Staleness::Unknown,
        Current::Missing => Staleness::Deleted,
        Current::Blob(now) if now == saved => Staleness::Fresh,
        Current::Blob(_) => Staleness::Changed,
    }
}

/// `repo` when a repo is given, else `personal`. An explicit `repo` scope needs a repo.
pub fn resolve_scope(scope: Option<Scope>, repo: Option<&str>) -> Result<Scope, &'static str> {
    match (scope, repo) {
        (Some(Scope::Repo), None) => Err("scope \"repo\" needs a repo"),
        (Some(scope), _) => Ok(scope),
        (None, Some(_)) => Ok(Scope::Repo),
        (None, None) => Ok(Scope::Personal),
    }
}

/// Anchors are repo-relative paths. Both the server (when saving) and the hook
/// (when hashing) clean them the same way, so the strings match.
pub fn clean_anchor_path(path: &str) -> Option<String> {
    let path = path.trim().trim_start_matches("./");
    let ok = !path.is_empty() && !path.starts_with('/') && !path.split('/').any(|c| c == "..");
    ok.then(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ids() {
        let want = Some("github.com/org/name".to_string());
        for url in [
            "git@github.com:Org/Name.git",
            "ssh://git@github.com:22/org/name.git",
            "ssh://git@GitHub.com/Org/Name",
            "https://user:s3cret@github.com/Org/Name.git",
            "https://github.com/org/name/",
            "https://github.com/org/name.git/",
            "HTTPS://GITHUB.COM/ORG/NAME",
            "git://github.com/org/name.git",
            "  https://oauth2:tok@github.com:443/org/name.git\n",
        ] {
            assert_eq!(repo_id_from_url(url), want, "{url}");
        }
        assert_eq!(
            repo_id_from_url("/srv/git/Proj.git").as_deref(),
            Some("srv/git/proj")
        );
        assert_eq!(repo_id_from_url(""), None);
        assert_eq!(repo_id_from_root("ABC123\n"), "root:abc123");
    }

    #[test]
    fn content_hash_matches_git() {
        assert_eq!(
            content_hash(b""),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            content_hash(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }

    #[test]
    fn staleness_cases() {
        let anchor = |blob: Option<&str>| Anchor {
            path: "a.rs".into(),
            blob_at_save: blob.map(String::from),
            commit_at_save: None,
        };
        let a = anchor(Some("abc"));
        assert_eq!(staleness(&a, Current::Blob("abc")), Staleness::Fresh);
        assert_eq!(staleness(&a, Current::Blob("def")), Staleness::Changed);
        assert_eq!(staleness(&a, Current::Missing), Staleness::Deleted);
        assert_eq!(staleness(&a, Current::Unknown), Staleness::Unknown);
        let no_baseline = anchor(None);
        assert_eq!(
            staleness(&no_baseline, Current::Blob("abc")),
            Staleness::Unknown
        );
    }

    #[test]
    fn scope_default() {
        assert_eq!(resolve_scope(None, Some("r")), Ok(Scope::Repo));
        assert_eq!(resolve_scope(None, None), Ok(Scope::Personal));
        assert_eq!(
            resolve_scope(Some(Scope::Personal), Some("r")),
            Ok(Scope::Personal)
        );
        assert!(resolve_scope(Some(Scope::Repo), None).is_err());
    }

    #[test]
    fn anchor_paths() {
        assert_eq!(clean_anchor_path("./src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(clean_anchor_path("src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(clean_anchor_path("/etc/passwd"), None);
        assert_eq!(clean_anchor_path("../x"), None);
        assert_eq!(clean_anchor_path("a/../../x"), None);
        assert_eq!(clean_anchor_path("  "), None);
    }
}
