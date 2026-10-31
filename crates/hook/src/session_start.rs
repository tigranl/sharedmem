//! SessionStart: fetch the repo's memories, check their anchors against the
//! files on this machine, and inject the context block (section 9).

use std::io::ErrorKind;
use std::path::Path;

use serde_json::{Value, json};
use sm_core::{Current, clean_anchor_path, content_hash, staleness};
use sm_proto::{Anchor, Memory, Staleness};

use crate::{Result, api::Api, git};

/// About 1,500 tokens.
const MAX_CHARS: usize = 6_000;
/// One long memory must not crowd out the rest.
const MAX_MEMORY_CHARS: usize = 600;

/// Returns the hook output JSON carrying the context block.
pub fn run(input: &Value, api: &Api) -> Result<String> {
    let cwd = input["cwd"].as_str().ok_or("no cwd in hook input")?;
    let top = git::toplevel(Path::new(cwd));
    let repo = top.as_deref().and_then(git::repo_id);
    let branch = top.as_deref().and_then(git::branch);
    let context = api.context(repo.as_deref())?;

    let checked: Vec<(&Memory, Vec<Staleness>)> = context
        .memories
        .iter()
        .map(|m| {
            // anchors are paths in the memory's repo; only a checkout of that repo can vouch for them
            let here = top
                .as_deref()
                .filter(|_| m.repo.is_some() && m.repo == repo);
            (m, m.anchors.iter().map(|a| check(here, a)).collect())
        })
        .collect();
    let block = render(repo.as_deref(), branch.as_deref(), &checked);
    Ok(json!({
        "hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": block}
    })
    .to_string())
}

fn check(top: Option<&Path>, anchor: &Anchor) -> Staleness {
    let Some(file) = top
        .zip(clean_anchor_path(&anchor.path))
        .map(|(t, p)| t.join(p))
    else {
        return staleness(anchor, Current::Unknown);
    };
    match std::fs::read(file) {
        Ok(bytes) => staleness(anchor, Current::Blob(&content_hash(&bytes))),
        Err(e) if e.kind() == ErrorKind::NotFound => staleness(anchor, Current::Missing),
        Err(_) => staleness(anchor, Current::Unknown),
    }
}

fn label(s: Staleness) -> &'static str {
    match s {
        Staleness::Fresh => "fresh",
        Staleness::Changed => "CHANGED since saved, verify",
        Staleness::Deleted => "DELETED since saved, verify",
        Staleness::Unknown => "unverified",
    }
}

fn line(m: &Memory, flags: &[Staleness]) -> String {
    let text = m.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = match text.char_indices().nth(MAX_MEMORY_CHARS) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    };
    let mut out = format!("- [{}] {text}", m.scope.as_str());
    if !m.anchors.is_empty() {
        let anchors: Vec<String> = m
            .anchors
            .iter()
            .zip(flags)
            .map(|(a, s)| format!("{}: {}", a.path, label(*s)))
            .collect();
        out.push_str(&format!(" ({})", anchors.join("; ")));
    }
    out
}

fn render(
    repo: Option<&str>,
    branch: Option<&str>,
    memories: &[(&Memory, Vec<Staleness>)],
) -> String {
    let header = match (repo, branch) {
        (Some(r), Some(b)) => format!("## sharedmem · {r} @ {b}"),
        (Some(r), None) => format!("## sharedmem · {r}"),
        (None, _) => "## sharedmem · personal (not in a git repo)".to_string(),
    };
    let footer = match repo {
        Some(r) => format!(
            "Save durable facts with sharedmem `remember` (repo=\"{r}\",\nanchors=[files the fact is about]); pass a `key` to replace an older fact."
        ),
        None => "Save durable facts with sharedmem `remember`; outside a repo they are personal. Pass a `key` to replace an older fact.".to_string(),
    };
    // room for the footer and a "more" line
    let budget = MAX_CHARS.saturating_sub(footer.len() + 80);
    let mut out = header;
    for (i, (m, flags)) in memories.iter().enumerate() {
        let line = line(m, flags);
        if out.len() + 1 + line.len() > budget {
            out.push_str(&format!(
                "\n- … {} more; search them with `recall`.",
                memories.len() - i
            ));
            break;
        }
        out.push('\n');
        out.push_str(&line);
    }
    out.push('\n');
    out.push_str(&footer);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_proto::Scope;

    fn memory(scope: Scope, text: &str, anchors: &[&str]) -> Memory {
        Memory {
            id: "0".into(),
            scope,
            repo: None,
            key: None,
            text: text.into(),
            created_at: 0,
            anchors: anchors
                .iter()
                .map(|p| Anchor {
                    path: p.to_string(),
                    blob_at_save: None,
                    commit_at_save: None,
                })
                .collect(),
        }
    }

    #[test]
    fn renders_section_9_block() {
        let a = memory(
            Scope::Repo,
            "Fix companies.yaml first.",
            &["companies.yaml"],
        );
        let b = memory(Scope::Repo, "Token lifts limits.", &["fetch/github.py"]);
        let c = memory(Scope::Personal, "Prefers building.", &[]);
        let block = render(
            Some("github.com/tigran/osshunt"),
            Some("main"),
            &[
                (&a, vec![Staleness::Fresh]),
                (&b, vec![Staleness::Changed]),
                (&c, vec![]),
            ],
        );
        assert_eq!(
            block,
            "## sharedmem · github.com/tigran/osshunt @ main\n\
             - [repo] Fix companies.yaml first. (companies.yaml: fresh)\n\
             - [repo] Token lifts limits. (fetch/github.py: CHANGED since saved, verify)\n\
             - [personal] Prefers building.\n\
             Save durable facts with sharedmem `remember` (repo=\"github.com/tigran/osshunt\",\n\
             anchors=[files the fact is about]); pass a `key` to replace an older fact."
        );
    }

    #[test]
    fn truncates_to_budget() {
        let long = "word ".repeat(400);
        let ms: Vec<Memory> = (0..40)
            .map(|_| memory(Scope::Personal, &long, &[]))
            .collect();
        let checked: Vec<_> = ms.iter().map(|m| (m, vec![])).collect();
        let block = render(None, None, &checked);
        assert!(
            block.chars().count() <= MAX_CHARS,
            "{} chars over budget",
            block.chars().count()
        );
        assert!(
            block.contains("more; search them with `recall`."),
            "no \"more\" line: {block}"
        );
        let first = block
            .lines()
            .nth(1)
            .expect("a memory line after the header");
        assert!(first.ends_with('…'), "long memory not cut: {first}");
        assert!(
            block.ends_with("replace an older fact."),
            "footer missing: {block}"
        );
    }
}
