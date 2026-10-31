//! Blocking HTTP to sm-server.

use std::time::Duration;

use sm_proto::{AnchorBaseline, Context};

use crate::Result;

pub struct Api {
    url: String,
    auth: String,
    agent: ureq::Agent,
}

fn env(plugin: &str, fallback: &str) -> Option<String> {
    [plugin, fallback]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
}

impl Api {
    pub fn from_env(timeout: Duration) -> Result<Api> {
        let url = env("CLAUDE_PLUGIN_OPTION_URL", "SHAREDMEM_URL").ok_or("no server url set")?;
        let token = env("CLAUDE_PLUGIN_OPTION_TOKEN", "SHAREDMEM_TOKEN").ok_or("no token set")?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build()
            .new_agent();
        Ok(Api {
            url: url.trim().trim_end_matches('/').to_string(),
            auth: format!("Bearer {}", token.trim()),
            agent,
        })
    }

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

    pub fn post_baselines(&self, id: &str, body: &AnchorBaseline) -> Result<()> {
        self.agent
            .post(format!("{}/v1/memories/{id}/anchors", self.url))
            .header("Authorization", &self.auth)
            .send_json(body)?;
        Ok(())
    }
}
