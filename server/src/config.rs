use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::runner::RunnerType;

#[derive(Deserialize)]
struct RawConfig {
    #[serde(default = "default_wiki_repo")]
    wiki_repo: PathBuf,
    #[serde(default = "default_bind_address")]
    bind_address: String,
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

fn default_wiki_repo() -> PathBuf {
    ".".into()
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
pub struct AppConfig {
    pub wiki_repo: PathBuf,
    pub bind_addr: SocketAddr,
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
    #[error("wiki_repo '{}' does not exist", .0.display())]
    WikiRepoMissing(PathBuf),
    #[error("wiki_repo '{}' has no wiki/ subdirectory", .0.display())]
    WikiDirMissing(PathBuf),
    #[error("prompt_template '{}' does not exist", .0.display())]
    PromptTemplateMissing(PathBuf),
    #[error("prompt_template '{}' does not contain {{question}} placeholder", .0.display())]
    PromptTemplateMissingPlaceholder(PathBuf),
    #[error("failed to resolve bind_address '{0}'")]
    InvalidBindAddress(String, #[source] std::io::Error),
    #[error("agent_command must not be empty")]
    AgentCommandEmpty,
    #[error("agent_command must contain exactly one {PROMPT_PLACEHOLDER} element")]
    AgentCommandMissingPrompt,
    #[error(
        "agent_command for acp runner must not contain {PROMPT_PLACEHOLDER} (ACP sends prompt via RPC)"
    )]
    AgentCommandUnexpectedPrompt,
}

fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

impl AppConfig {
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
            .try_deserialize::<RawConfig>()?;

        let wiki_repo = resolve(&config_dir, raw.wiki_repo);
        let prompt_template = resolve(&config_dir, raw.prompt_template);
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

        if raw.agent_command.is_empty() {
            return Err(ConfigError::AgentCommandEmpty);
        }
        let prompt_count = raw
            .agent_command
            .iter()
            .filter(|a| a.as_str() == PROMPT_PLACEHOLDER)
            .count();
        if raw.runner.requires_prompt_placeholder() {
            if prompt_count != 1 {
                return Err(ConfigError::AgentCommandMissingPrompt);
            }
        } else if prompt_count != 0 {
            return Err(ConfigError::AgentCommandUnexpectedPrompt);
        }

        if !wiki_repo.exists() {
            return Err(ConfigError::WikiRepoMissing(wiki_repo));
        }
        if !wiki_repo.join("wiki").exists() {
            return Err(ConfigError::WikiDirMissing(wiki_repo));
        }
        let prompt_template_content = std::fs::read_to_string(&prompt_template).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ConfigError::PromptTemplateMissing(prompt_template.clone())
            } else {
                ConfigError::Io(prompt_template.clone(), e)
            }
        })?;
        if !prompt_template_content.contains(QUESTION_PLACEHOLDER) {
            return Err(ConfigError::PromptTemplateMissingPlaceholder(
                prompt_template,
            ));
        }

        Ok(AppConfig {
            wiki_repo,
            bind_addr,
            runner: raw.runner,
            agent_command: raw.agent_command,
            prompt_template_content,
            instructions: raw.instructions,
            research_tool_description: raw.research_tool_description,
            completed_task_ttl: Duration::from_secs(raw.completed_task_ttl_secs),
            agent_timeout: Duration::from_secs(raw.agent_timeout_secs),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const BASE_CONFIG: &str = r#"
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#;

    fn setup_dir(dir: &Path) {
        fs::create_dir_all(dir.join("wiki")).unwrap();
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

    #[test]
    fn loads_valid_config_with_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = setup_valid_config(dir.path());
        let cfg = AppConfig::load(&config_path).unwrap();

        assert_eq!(
            cfg.bind_addr,
            "127.0.0.1:1238".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(cfg.agent_command, ["echo", "$PROMPT"]);
        assert_eq!(cfg.wiki_repo, dir.path().canonicalize().unwrap());
        assert_eq!(cfg.prompt_template_content, "Research: {question}");
    }

    #[test]
    fn absolute_paths_are_not_rebased() {
        let dir = tempfile::tempdir().unwrap();
        let wiki_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(wiki_dir.path().join("wiki")).unwrap();
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                r#"
wiki_repo = "{}"
agent_command = ["echo", "$PROMPT"]
prompt_template = "prompt.md"
"#,
                wiki_dir.path().display(),
            ),
        )
        .unwrap();

        let cfg = AppConfig::load(&config_path).unwrap();
        assert_eq!(cfg.wiki_repo, wiki_dir.path().to_path_buf());
    }

    #[test]
    fn rejects_missing_wiki_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("repo")).unwrap();
        fs::write(dir.path().join("prompt.md"), "Research: {question}").unwrap();
        let config_path = write_config(
            dir.path(),
            "wiki_repo = \"repo\"\nagent_command = [\"echo\", \"$PROMPT\"]\nprompt_template = \"prompt.md\"\n",
        );

        let err = AppConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::WikiDirMissing(_)));
    }

    #[test]
    fn rejects_prompt_without_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("wiki")).unwrap();
        fs::write(dir.path().join("prompt.md"), "No placeholder here").unwrap();
        let config_path = write_config(dir.path(), BASE_CONFIG);

        let err = AppConfig::load(&config_path).unwrap_err();
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
            &format!("{BASE_CONFIG}bind_address = \"localhost:1238\"\n"),
        );

        let cfg = AppConfig::load(&config_path).unwrap();
        assert_eq!(cfg.bind_addr.port(), 1238);
    }

    #[test]
    fn rejects_invalid_bind_address() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            &format!("{BASE_CONFIG}bind_address = \"not-an-address\"\n"),
        );

        let err = AppConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidBindAddress(..)));
    }

    #[test]
    fn rejects_empty_agent_command() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            "agent_command = []\nprompt_template = \"prompt.md\"\n",
        );

        let err = AppConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandEmpty));
    }

    #[test]
    fn rejects_agent_command_without_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            "agent_command = [\"echo\", \"hello\"]\nprompt_template = \"prompt.md\"\n",
        );

        let err = AppConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandMissingPrompt));
    }

    #[test]
    fn acp_runner_accepts_command_without_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            "runner = \"acp\"\nagent_command = [\"claude-agent-acp\"]\nprompt_template = \"prompt.md\"\n",
        );

        let cfg = AppConfig::load(&config_path).unwrap();
        assert_eq!(cfg.runner, RunnerType::Acp);
    }

    #[test]
    fn acp_runner_rejects_command_with_prompt_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        setup_dir(dir.path());
        let config_path = write_config(
            dir.path(),
            "runner = \"acp\"\nagent_command = [\"claude-agent-acp\", \"$PROMPT\"]\nprompt_template = \"prompt.md\"\n",
        );

        let err = AppConfig::load(&config_path).unwrap_err();
        assert!(matches!(err, ConfigError::AgentCommandUnexpectedPrompt));
    }
}
