//! PostToolUse on `remember`: hash the anchored files on this machine and post
//! them, with HEAD, as the memory's baseline (section 7).

use std::path::Path;

use serde_json::Value;
use sm_core::{clean_anchor_path, content_hash};
use sm_proto::{AnchorBaseline, PathBlob};

use crate::{Result, api::Api, git};

pub fn run(input: &Value, api: &Api) -> Result<()> {
    let anchors: Vec<&str> = input["tool_input"]["anchors"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if anchors.is_empty() {
        return Ok(());
    }
    let id = memory_id(&input["tool_response"]).ok_or("no memory id in tool_response")?;
    let cwd = input["cwd"].as_str().ok_or("no cwd in hook input")?;
    let top = git::toplevel(Path::new(cwd)).ok_or("cwd is not in a git repo")?;
    // hashing another checkout's files would record a wrong baseline
    let saved_for = input["tool_input"]["repo"].as_str().map(str::trim);
    let here = git::repo_id(&top);
    if saved_for != here.as_deref() {
        return Err(format!("memory {id} is for repo {saved_for:?}, cwd is {here:?}").into());
    }
    let blobs = anchors
        .iter()
        .filter_map(|a| {
            let path = clean_anchor_path(a)?;
            let bytes = std::fs::read(top.join(&path)).ok()?;
            Some(PathBlob {
                path,
                blob: content_hash(&bytes),
            })
        })
        .collect();
    let baseline = AnchorBaseline {
        commit: git::head(&top),
        blobs,
    };
    api.post_baselines(&id, &baseline)
}

/// The memory id in the remember tool's result. Claude Code does not document
/// the shape of `tool_response` for MCP tools, so accept any nesting of: the
/// MCP result (`{content, structuredContent}`), a content array
/// (`[{"type":"text","text":"{\"id\":…}"}]`), a JSON string, or the payload itself.
fn memory_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => memory_id(&serde_json::from_str(s).ok()?),
        Value::Array(items) => items.iter().find_map(memory_id),
        Value::Object(o) => match o.get("id").and_then(Value::as_str) {
            // the id goes into a URL path: accept only a UUID's characters
            Some(id)
                if id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') =>
            {
                Some(id.to_string())
            }
            _ => ["structuredContent", "content", "text", "result"]
                .iter()
                .find_map(|k| o.get(*k).and_then(memory_id)),
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::memory_id;
    use serde_json::json;

    const ID: &str = "019a1b2c-3d4e-7f00-8000-0123456789ab";

    #[test]
    fn finds_id_in_every_plausible_shape() {
        let payload = json!({"id": ID, "scope": "repo", "anchors": [{"path": "a.rs"}]});
        let text = payload.to_string();
        let shapes = [
            json!({"content": [{"type": "text", "text": text}], "structuredContent": payload, "isError": false}),
            json!({"content": [{"type": "text", "text": text}]}),
            json!([{"type": "text", "text": text}]),
            json!(text),
            payload.clone(),
            json!({"result": {"structuredContent": payload}}),
        ];
        for shape in shapes {
            assert_eq!(memory_id(&shape).as_deref(), Some(ID), "{shape}");
        }
    }

    #[test]
    fn rejects_non_ids() {
        for shape in [
            json!({"id": "../../v1/x"}),
            json!([{"type": "text", "text": "storage error"}]),
            json!(null),
        ] {
            assert_eq!(memory_id(&shape), None, "{shape}");
        }
    }
}
