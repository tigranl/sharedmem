//! Blocking HTTP to sm-server.

use std::time::Duration;

use sm_proto::{AnchorBaseline, Context, HookRuns};
use ureq::Agent;

use crate::Result;

/// Client for sm-server, configured from the plugin options or env.
pub struct Api {
    url: String,
    auth: String,
    agent: Agent,
}

fn env(plugin: &str, fallback: &str) -> Option<String> {
    [plugin, fallback]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

impl Api {
    /// Every request gives up after `timeout`.
    pub fn from_env(timeout: Duration) -> Result<Api> {
        let url = env("CLAUDE_PLUGIN_OPTION_URL", "SHAREDMEM_URL").ok_or("no server url set")?;
        let token = env("CLAUDE_PLUGIN_OPTION_TOKEN", "SHAREDMEM_TOKEN").ok_or("no token set")?;
        Ok(Api::new(&url, &token, timeout))
    }

    /// A client for the server at `url`; every request gives up after `timeout`.
    pub fn new(url: &str, token: &str, timeout: Duration) -> Api {
        let agent = Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .new_agent();
        Api {
            url: url.trim().trim_end_matches('/').to_string(),
            auth: format!("Bearer {}", token.trim()),
            agent,
        }
    }

    /// `GET /v1/context`, for `repo` if given.
    pub fn context(&self, repo: Option<&str>) -> Result<Context> {
        let mut req = self
            .agent
            .get(format!("{}/v1/context", self.url))
            .header("Authorization", &self.auth);
        if let Some(repo) = repo {
            req = req.query("repo", repo);
        }
        Ok(req.call()?.body_mut().read_json()?)
    }

    /// `POST /v1/hook-runs`.
    pub fn post_hook_runs(&self, body: &HookRuns) -> Result<()> {
        self.agent
            .post(format!("{}/v1/hook-runs", self.url))
            .header("Authorization", &self.auth)
            .send_json(body)?;
        Ok(())
    }

    /// `POST /v1/memories/{id}/anchors`.
    pub fn post_baselines(&self, id: &str, body: &AnchorBaseline) -> Result<()> {
        self.agent
            .post(format!("{}/v1/memories/{id}/anchors", self.url))
            .header("Authorization", &self.auth)
            .send_json(body)?;
        Ok(())
    }
}
