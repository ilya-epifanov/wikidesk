use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use wikidesk_shared::is_valid_wiki_name;

use crate::runner::{self, Runner, RunnerType};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerConfig {
    #[serde(default = "default_bind_address")]
    bind_address: String,
    #[serde(default)]
    wikis: Vec<RawWikiConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWikiConfig {
    name: String,
    #[serde(default)]
    runner: RunnerType,
    agent_command: Vec<String>,
    prompt_template: PathBuf,
    instructions: Option<String>,
    research_tool_description: Option<String>,
    #[serde(default = "default_completed_task_ttl_secs")]
    completed_task_ttl_secs: u64,
    #[serde(default = "default_agent_timeout_secs")]
    agent_timeout_secs: u64,
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

pub const QUESTION_PLACEHOLDER: &str = "{question}";
pub const PROMPT_PLACEHOLDER: &str = "$PROMPT";

#[derive(Debug)]
pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub wikis: Vec<AppConfig>,
}

#[derive(Debug)]
pub struct AppConfig {
    pub name: String,
    pub wiki_repo: PathBuf,
    pub runner: RunnerType,
    pub agent_command: Vec<String>,
    pub prompt_template_content: String,
    pub instructions: Option<String>,
    pub research_tool_description: Option<String>,
    pub completed_task_ttl: Duration,
    pub agent_timeout: Duration,
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
}

fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn wiki_repo_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join(format!("wiki-{name}"))
}

fn validate_wiki_name(name: &str) -> Result<(), ConfigError> {
    if is_valid_wiki_name(name) {
        Ok(())
    } else {
        Err(ConfigError::InvalidWikiName(name.to_string()))
    }
}

fn validate_agent_command(
    wiki_name: &str,
    runner: RunnerType,
    agent_command: &[String],
) -> Result<(), ConfigError> {
    if agent_command.is_empty() {
        return Err(ConfigError::AgentCommandEmpty(wiki_name.to_string()));
    }
    let prompt_count = agent_command
        .iter()
        .filter(|a| a.as_str() == PROMPT_PLACEHOLDER)
        .count();
    if runner.requires_prompt_placeholder() {
        if prompt_count != 1 {
            return Err(ConfigError::AgentCommandMissingPrompt(
                wiki_name.to_string(),
            ));
        }
    } else if prompt_count != 0 {
        return Err(ConfigError::AgentCommandUnexpectedPrompt(
            wiki_name.to_string(),
        ));
    }
    Ok(())
}

fn load_prompt_template(prompt_template: &Path) -> Result<String, ConfigError> {
    let prompt_template_content = std::fs::read_to_string(prompt_template).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ConfigError::PromptTemplateMissing(prompt_template.to_path_buf())
        } else {
            ConfigError::Io(prompt_template.to_path_buf(), e)
        }
    })?;
    if !prompt_template_content.contains(QUESTION_PLACEHOLDER) {
        return Err(ConfigError::PromptTemplateMissingPlaceholder(
            prompt_template.to_path_buf(),
        ));
    }
    Ok(prompt_template_content)
}

impl AppConfig {
    pub fn wiki_dir(&self) -> PathBuf {
        self.wiki_repo.join("wiki")
    }

    pub fn base_path(&self) -> String {
        format!("/{}", self.name)
    }

    pub fn client_link_prefix(&self) -> String {
        format!("wiki-{}", self.name)
    }

    pub fn build_research_prompt(&self, question: &str) -> String {
        self.prompt_template_content
            .replace(QUESTION_PLACEHOLDER, question)
    }

    pub fn create_runner_adapter(&self) -> Arc<dyn Runner> {
        runner::create_runner(self.runner)
    }

    pub fn mcp_instructions(&self) -> &str {
        self.instructions.as_deref().unwrap_or(
            "Research server: use 'research' to submit questions, 'get_result' to poll results.",
        )
    }

    pub fn research_tool_description(&self) -> Option<&str> {
        self.research_tool_description.as_deref()
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
            validate_wiki_name(&raw_wiki.name)?;
            if !seen.insert(raw_wiki.name.clone()) {
                return Err(ConfigError::DuplicateWikiName(raw_wiki.name));
            }
            validate_agent_command(&raw_wiki.name, raw_wiki.runner, &raw_wiki.agent_command)?;

            let wiki_repo = wiki_repo_path(&config_dir, &raw_wiki.name);
            if !wiki_repo.is_dir() {
                return Err(ConfigError::WikiRepoMissing {
                    name: raw_wiki.name,
                    path: wiki_repo,
                });
            }
            if !wiki_repo.join("wiki").is_dir() {
                return Err(ConfigError::WikiDirMissing {
                    name: raw_wiki.name,
                    path: wiki_repo.join("wiki"),
                });
            }
            let prompt_template_content =
                load_prompt_template(&resolve(&config_dir, raw_wiki.prompt_template))?;

            wikis.push(AppConfig {
                name: raw_wiki.name,
                wiki_repo,
                runner: raw_wiki.runner,
                agent_command: raw_wiki.agent_command,
                prompt_template_content,
                instructions: raw_wiki.instructions,
                research_tool_description: raw_wiki.research_tool_description,
                completed_task_ttl: Duration::from_secs(raw_wiki.completed_task_ttl_secs),
                agent_timeout: Duration::from_secs(raw_wiki.agent_timeout_secs),
            });
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
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#;

    fn setup_wiki(dir: &Path, name: &str) {
        fs::create_dir_all(dir.join(format!("wiki-{name}/wiki"))).unwrap();
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
        assert_eq!(wiki.wiki_dir(), wiki.wiki_repo.join("wiki"));
        assert_eq!(wiki.base_path(), "/rlhf");
        assert_eq!(wiki.client_link_prefix(), "wiki-rlhf");
        assert_eq!(wiki.prompt_template_content, "Research: {question}");
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
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"

[[wikis]]
name = "rust-notes"
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
    fn rejects_invalid_wiki_name() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path(), "Wiki");
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            r#"
[[wikis]]
name = "Wiki"
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
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"

[[wikis]]
name = "rlhf"
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
runner = "acp"
agent_command = ["claude-agent-acp", "$PROMPT"]
prompt_template = "prompt.md"
"#,
        );

        let err = ServerConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandUnexpectedPrompt(name) if name == "rlhf"));
    }
}
