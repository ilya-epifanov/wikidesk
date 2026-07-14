use std::path::PathBuf;

use serde::Deserialize;

use crate::runner::RunnerType;

use super::VcsWorkflow;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawServerConfig {
    #[serde(default = "default_bind_address")]
    pub(super) bind_address: String,
    #[serde(default)]
    pub(super) wikis: Vec<RawWikiConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawWikiConfig {
    pub(super) name: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) runner: RunnerType,
    pub(super) agent_command: Vec<String>,
    pub(super) prompt_template: PathBuf,
    #[serde(default)]
    pub(super) vcs_workflow: VcsWorkflow,
    pub(super) research_concurrency: Option<usize>,
    #[serde(default)]
    pub(super) mcp: RawMcpConfig,
    pub(super) git_sync: Option<RawGitSyncConfig>,
    #[serde(default = "default_completed_task_ttl_secs")]
    pub(super) completed_task_ttl_secs: u64,
    #[serde(default = "default_agent_timeout_secs")]
    pub(super) agent_timeout_secs: u64,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawMcpConfig {
    pub(super) instructions: Option<String>,
    pub(super) research_tool_description: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawGitSyncConfig {
    #[serde(default = "default_git_sync_remote")]
    pub(super) remote: String,
    #[serde(default = "default_git_sync_interval_secs")]
    pub(super) interval_secs: u64,
    #[serde(default = "default_git_sync_retry_max_elapsed_secs")]
    pub(super) retry_max_elapsed_secs: u64,
    #[serde(default = "default_git_sync_retry_initial_delay_secs")]
    pub(super) retry_initial_delay_secs: u64,
    #[serde(default = "default_git_sync_retry_max_delay_secs")]
    pub(super) retry_max_delay_secs: u64,
    pub(super) ssh_command: Option<String>,
}

fn default_bind_address() -> String {
    "127.0.0.1:1238".to_string()
}

fn default_completed_task_ttl_secs() -> u64 {
    7200
}

fn default_agent_timeout_secs() -> u64 {
    1800
}

fn default_git_sync_remote() -> String {
    "origin".to_string()
}

fn default_git_sync_interval_secs() -> u64 {
    3600
}

fn default_git_sync_retry_max_elapsed_secs() -> u64 {
    900
}

fn default_git_sync_retry_initial_delay_secs() -> u64 {
    5
}

fn default_git_sync_retry_max_delay_secs() -> u64 {
    60
}
