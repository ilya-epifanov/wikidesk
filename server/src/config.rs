use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use wikidesk_shared::{WikiInfo, derived_wiki_path, wiki_base_path};

use crate::runner::RunnerType;

use normalize::normalize_wiki_config;
use raw::RawServerConfig;

mod normalize;
mod raw;

pub const QUESTION_PLACEHOLDER: &str = "{question}";
pub const PROMPT_PLACEHOLDER: &str = "$PROMPT";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VcsWorkflow {
    #[default]
    None,
    Jj,
}

#[derive(Debug)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub wikis: Vec<AppConfig>,
}

#[derive(Debug)]
pub struct AppConfig {
    pub name: String,
    pub wiki_repo: PathBuf,
    pub description: String,
    pub runner: RunnerType,
    pub agent_command: Vec<String>,
    pub prompt_template_content: String,
    pub vcs_workflow: VcsWorkflow,
    pub research_concurrency: NonZeroUsize,
    pub git_sync: Option<GitSyncConfig>,
    pub mcp_instructions: String,
    pub research_tool_description: String,
    pub completed_task_ttl: Duration,
    pub agent_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSyncConfig {
    pub remote: String,
    pub interval: Duration,
    pub retry_max_elapsed: Duration,
    pub retry_initial_delay: Duration,
    pub retry_max_delay: Duration,
    pub ssh_command: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error(transparent)]
    Load(#[from] config::ConfigError),
    #[error("failed to read '{}'", .0.display())]
    Io(PathBuf, #[source] std::io::Error),
    #[error("at least one [[wikis]] entry is required")]
    NoWikis,
    #[error(
        "invalid wiki name '{0}' (use lowercase letters, digits, and hyphens; start and end with a letter or digit)"
    )]
    InvalidWikiName(String),
    #[error("duplicate wiki name '{0}'")]
    DuplicateWikiName(String),
    #[error("description for wiki '{0}' must not be empty")]
    WikiDescriptionEmpty(String),
    #[error("wiki repo for '{name}' does not exist at '{}'", path.display())]
    WikiRepoMissing { name: String, path: PathBuf },
    #[error("wiki repo for '{name}' has no wiki/ subdirectory at '{}'", path.display())]
    WikiDirMissing { name: String, path: PathBuf },
    #[error("prompt_template '{}' does not exist", .0.display())]
    PromptTemplateMissing(PathBuf),
    #[error("prompt_template '{}' does not contain {{question}} placeholder", .0.display())]
    PromptTemplateMissingPlaceholder(PathBuf),
    #[error("failed to resolve bind_address '{0}'")]
    InvalidBindAddress(String, #[source] std::io::Error),
    #[error("agent_command for wiki '{0}' must not be empty")]
    AgentCommandEmpty(String),
    #[error("agent_command for wiki '{0}' must contain exactly one {PROMPT_PLACEHOLDER} element")]
    AgentCommandMissingPrompt(String),
    #[error(
        "agent_command for acp runner in wiki '{0}' must not contain {PROMPT_PLACEHOLDER} (ACP sends prompt via RPC)"
    )]
    AgentCommandUnexpectedPrompt(String),
    #[error("git_sync for wiki '{0}' requires vcs_workflow = \"jj\"")]
    GitSyncRequiresJj(String),
    #[error("research_concurrency for wiki '{0}' must be greater than zero")]
    ResearchConcurrencyZero(String),
    #[error("research_concurrency for wiki '{0}' requires vcs_workflow != \"none\"")]
    ResearchConcurrencyRequiresVcs(String),
    #[error("git_sync remote for wiki '{0}' must not be empty")]
    GitSyncRemoteEmpty(String),
    #[error("git_sync interval_secs for wiki '{0}' must be greater than zero")]
    GitSyncIntervalZero(String),
    #[error("git_sync retry_max_elapsed_secs for wiki '{0}' must be greater than zero")]
    GitSyncRetryMaxElapsedZero(String),
    #[error("git_sync retry_initial_delay_secs for wiki '{0}' must be greater than zero")]
    GitSyncRetryInitialDelayZero(String),
    #[error("git_sync retry_max_delay_secs for wiki '{0}' must be greater than zero")]
    GitSyncRetryMaxDelayZero(String),
}

impl AppConfig {
    pub fn base_path(&self) -> String {
        wiki_base_path(&self.name)
    }

    pub fn derived_wiki_path(&self) -> String {
        derived_wiki_path(&self.name)
    }

    pub fn info(&self) -> WikiInfo {
        WikiInfo {
            name: self.name.clone(),
            description: self.description.clone(),
        }
    }

    pub fn mcp_instructions(&self) -> &str {
        &self.mcp_instructions
    }

    pub fn research_tool_description(&self) -> &str {
        &self.research_tool_description
    }
}

#[cfg(test)]
pub(crate) fn test_app_config(wiki_repo: PathBuf, agent_command: Vec<String>) -> AppConfig {
    AppConfig {
        name: "test".into(),
        wiki_repo,
        description: "Test wiki.".into(),
        runner: RunnerType::Generic,
        agent_command,
        prompt_template_content: "{question}".into(),
        vcs_workflow: VcsWorkflow::None,
        research_concurrency: NonZeroUsize::MIN,
        git_sync: None,
        mcp_instructions: "Test instructions.".into(),
        research_tool_description: "Test research tool.".into(),
        completed_task_ttl: Duration::from_secs(900),
        agent_timeout: Duration::from_secs(5),
    }
}

impl ServerConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let config_dir = path
            .canonicalize()
            .map_err(|e| ConfigError::Io(path.to_path_buf(), e))?
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let raw = config::Config::builder()
            .add_source(config::File::from(path))
            .build()?
            .try_deserialize::<RawServerConfig>()?;

        if raw.wikis.is_empty() {
            return Err(ConfigError::NoWikis);
        }

        let bind_addr = raw
            .bind_address
            .to_socket_addrs()
            .map_err(|e| ConfigError::InvalidBindAddress(raw.bind_address.clone(), e))?
            .next()
            .ok_or_else(|| {
                ConfigError::InvalidBindAddress(
                    raw.bind_address.clone(),
                    std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "no addresses found"),
                )
            })?;

        let mut seen = HashSet::new();
        let mut wikis = Vec::with_capacity(raw.wikis.len());
        for raw_wiki in raw.wikis {
            wikis.push(normalize_wiki_config(&config_dir, raw_wiki, &mut seen)?);
        }

        Ok(Self { bind_addr, wikis })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const BASE_CONFIG: &str = r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#;

    fn setup_wiki(dir: &Path, name: &str) {
        fs::create_dir_all(dir.join(derived_wiki_path(name)).join("wiki")).unwrap();
    }

    fn setup_dir(dir: &Path) {
        setup_wiki(dir, "rlhf");
        fs::write(dir.join("prompt.md"), "Research: {question}").unwrap();
    }

    fn write_config(dir: &Path, content: &str) -> PathBuf {
        let config_path = dir.join("config.toml");
        fs::write(&config_path, content).unwrap();
        config_path
    }

    fn setup_valid_config(dir: &Path) -> PathBuf {
        setup_dir(dir);
        write_config(dir, BASE_CONFIG)
    }

    fn load_one(path: &Path) -> AppConfig {
        let mut cfg = ServerConfig::load(path).unwrap();
        cfg.wikis.pop().unwrap()
    }

    #[test]
    fn loads_named_wiki_config_with_derived_paths() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = setup_valid_config(dir.path());
        let cfg = ServerConfig::load(&config_path).unwrap();

        assert_eq!(
            cfg.bind_addr,
            "127.0.0.1:1238".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(cfg.wikis.len(), 1);
        let wiki = &cfg.wikis[0];
        assert_eq!(wiki.name, "rlhf");
        assert_eq!(wiki.agent_command, ["echo", "$PROMPT"]);
        assert_eq!(
            wiki.wiki_repo,
            dir.path().canonicalize().unwrap().join("wiki-rlhf")
        );
        assert!(wiki.wiki_repo.join("wiki").is_dir());
        assert_eq!(wiki.base_path(), "/wiki/rlhf");
        assert_eq!(wiki.derived_wiki_path(), "wiki-rlhf");
        assert_eq!(wiki.description, "Test wiki.");
        assert_eq!(
            wiki.mcp_instructions,
            "Use this wiki for: Test wiki.\n\nUse research when the existing wiki may not cover the full picture, including adjacent knowledge that may not have been researched yet."
        );
        assert_eq!(
            wiki.research_tool_description,
            "Submit a research question for this wiki. Covers: Test wiki."
        );
        assert_eq!(wiki.info().description, "Test wiki.");
        assert_eq!(wiki.prompt_template_content, "Research: {question}");
        assert_eq!(wiki.vcs_workflow, VcsWorkflow::None);
        assert_eq!(wiki.research_concurrency, NonZeroUsize::MIN);
        assert_eq!(wiki.git_sync, None);
    }

    #[test]
    fn loads_jj_vcs_workflow() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
vcs_workflow = "jj"
"#,
        );

        let wiki = load_one(&config_path);

        assert_eq!(wiki.vcs_workflow, VcsWorkflow::Jj);
    }

    #[test]
    fn loads_research_concurrency_for_jj_workflow() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
vcs_workflow = "jj"
research_concurrency = 3
"#,
        );

        let wiki = load_one(&config_path);

        assert_eq!(wiki.research_concurrency.get(), 3);
    }

    #[test]
    fn rejects_research_concurrency_without_vcs_workflow() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
research_concurrency = 2
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();

        assert!(matches!(
            err,
            ConfigError::ResearchConcurrencyRequiresVcs(name) if name == "rlhf"
        ));
    }

    #[test]
    fn rejects_zero_research_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
vcs_workflow = "jj"
research_concurrency = 0
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();

        assert!(matches!(
            err,
            ConfigError::ResearchConcurrencyZero(name) if name == "rlhf"
        ));
    }

    #[test]
    fn loads_git_sync_for_jj_wiki() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
vcs_workflow = "jj"

[wikis.git_sync]
remote = "origin"
interval_secs = 60
retry_max_elapsed_secs = 120
retry_initial_delay_secs = 2
retry_max_delay_secs = 10
ssh_command = "ssh -i /run/secrets/wikidesk/rlhf -o IdentitiesOnly=yes"
"#,
        );

        let wiki = load_one(&config_path);

        assert_eq!(
            wiki.git_sync,
            Some(GitSyncConfig {
                remote: "origin".into(),
                interval: Duration::from_secs(60),
                retry_max_elapsed: Duration::from_secs(120),
                retry_initial_delay: Duration::from_secs(2),
                retry_max_delay: Duration::from_secs(10),
                ssh_command: Some("ssh -i /run/secrets/wikidesk/rlhf -o IdentitiesOnly=yes".into()),
            })
        );
    }

    #[test]
    fn rejects_git_sync_without_jj_workflow() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"

[wikis.git_sync]
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::GitSyncRequiresJj(name) if name == "rlhf"));
    }

    #[test]
    fn default_wiki_uses_plain_wiki_repo() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "default");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "default"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let wiki = load_one(&config_path);

        assert_eq!(
            wiki.wiki_repo,
            dir.path().canonicalize().unwrap().join("wiki")
        );
        assert_eq!(wiki.base_path(), "/wiki/default");
        assert_eq!(wiki.derived_wiki_path(), "wiki");
    }

    #[test]
    fn loads_multiple_wikis() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        setup_wiki(dir.path(), "rust-notes");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"

[[wikis]]
name = "rust-notes"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let cfg = ServerConfig::load(&config_path).unwrap();

        assert_eq!(
            cfg.wikis
                .iter()
                .map(|w| w.name.as_str())
                .collect::<Vec<_>>(),
            ["rlhf", "rust-notes"]
        );
    }

    #[test]
    fn rejects_missing_wikis() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = write_config(dir.path(), "bind_address = \"127.0.0.1:1238\"\n");

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::NoWikis));
    }

    #[test]
    fn loads_mcp_overrides() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"

[wikis.mcp]
instructions = "custom instructions"
research_tool_description = "custom tool"
"#,
        );

        let cfg = load_one(&config_path);

        assert_eq!(cfg.mcp_instructions, "custom instructions");
        assert_eq!(cfg.research_tool_description, "custom tool");
    }

    #[test]
    fn rejects_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = " "
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::WikiDescriptionEmpty(name) if name == "rlhf"));
    }

    #[test]
    fn rejects_invalid_wiki_name() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "Wiki");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "Wiki"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidWikiName(name) if name == "Wiki"));
    }

    #[test]
    fn rejects_duplicate_wiki_name() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"

[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateWikiName(name) if name == "rlhf"));
    }

    #[test]
    fn rejects_missing_derived_wiki_repo() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(dir.path(), BASE_CONFIG);

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::WikiRepoMissing { name, .. } if name == "rlhf"));
    }

    #[test]
    fn rejects_missing_wiki_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wiki-rlhf")).unwrap();
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(dir.path(), BASE_CONFIG);

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::WikiDirMissing { name, .. } if name == "rlhf"));
    }

    #[test]
    fn rejects_prompt_without_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "No placeholder here").unwrap();
        let config_path = write_config(dir.path(), BASE_CONFIG);

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::PromptTemplateMissingPlaceholder(_)
        ));
    }

    #[test]
    fn accepts_hostname_bind_address() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            &format!("bind_address = \"localhost:1238\"\n{BASE_CONFIG}"),
        );

        let cfg = ServerConfig::load(&config_path).unwrap();
        assert_eq!(cfg.bind_addr.port(), 1238);
    }

    #[test]
    fn rejects_invalid_bind_address() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            &format!("bind_address = \"not-an-address\"\n{BASE_CONFIG}"),
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidBindAddress(..)));
    }

    #[test]
    fn rejects_empty_agent_command() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = []
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandEmpty(name) if name == "rlhf"));
    }

    #[test]
    fn rejects_agent_command_without_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
agent_command = ["echo", "hello"]
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandMissingPrompt(name) if name == "rlhf"));
    }

    #[test]
    fn acp_runner_accepts_command_without_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
runner = "acp"
agent_command = ["claude-agent-acp"]
prompt_template = "prompt.md"
"#,
        );

        let cfg = load_one(&config_path);
        assert_eq!(cfg.runner, RunnerType::Acp);
    }

    #[test]
    fn acp_runner_rejects_command_with_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "rlhf");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "rlhf"
description = "Test wiki."
runner = "acp"
agent_command = ["claude-agent-acp", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandUnexpectedPrompt(name) if name == "rlhf"));
    }
}
