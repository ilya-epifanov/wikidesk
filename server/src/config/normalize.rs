use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use wikidesk_shared::{derived_wiki_path, is_valid_wiki_name};

use crate::runner::RunnerType;

use super::raw::{RawGitSyncConfig, RawWikiConfig};
use super::{
    AppConfig, ConfigError, GitSyncConfig, PROMPT_PLACEHOLDER, QUESTION_PLACEHOLDER, VcsWorkflow,
};

fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn wiki_repo_path(config_dir: &Path, name: &str) -> PathBuf {
    config_dir.join(derived_wiki_path(name))
}

fn validate_wiki_name(name: &str) -> Result<(), ConfigError> {
    if is_valid_wiki_name(name) {
        Ok(())
    } else {
        Err(ConfigError::InvalidWikiName(name.to_string()))
    }
}

fn validate_description(name: &str, description: &str) -> Result<String, ConfigError> {
    let description = description.trim();
    if description.is_empty() {
        Err(ConfigError::WikiDescriptionEmpty(name.to_string()))
    } else {
        Ok(description.to_string())
    }
}

fn default_mcp_instructions(description: &str) -> String {
    format!(
        "Use this wiki for: {description}\n\nUse research when the existing wiki may not cover the full picture, including adjacent knowledge that may not have been researched yet."
    )
}

fn default_research_tool_description(description: &str) -> String {
    format!("Submit a research question for this wiki. Covers: {description}")
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

fn validate_git_sync(
    wiki_name: &str,
    workflow: VcsWorkflow,
    raw: Option<RawGitSyncConfig>,
) -> Result<Option<GitSyncConfig>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if workflow != VcsWorkflow::Jj {
        return Err(ConfigError::GitSyncRequiresJj(wiki_name.to_string()));
    }
    let remote = nonempty_trimmed(&raw.remote, wiki_name, ConfigError::GitSyncRemoteEmpty)?;
    Ok(Some(GitSyncConfig {
        remote: remote.to_string(),
        interval: nonzero_secs(
            raw.interval_secs,
            wiki_name,
            ConfigError::GitSyncIntervalZero,
        )?,
        retry_max_elapsed: nonzero_secs(
            raw.retry_max_elapsed_secs,
            wiki_name,
            ConfigError::GitSyncRetryMaxElapsedZero,
        )?,
        retry_initial_delay: nonzero_secs(
            raw.retry_initial_delay_secs,
            wiki_name,
            ConfigError::GitSyncRetryInitialDelayZero,
        )?,
        retry_max_delay: nonzero_secs(
            raw.retry_max_delay_secs,
            wiki_name,
            ConfigError::GitSyncRetryMaxDelayZero,
        )?,
        ssh_command: raw.ssh_command,
    }))
}

fn validate_research_concurrency(
    wiki_name: &str,
    workflow: VcsWorkflow,
    raw: Option<usize>,
) -> Result<NonZeroUsize, ConfigError> {
    if raw.is_some() && workflow == VcsWorkflow::None {
        return Err(ConfigError::ResearchConcurrencyRequiresVcs(
            wiki_name.to_string(),
        ));
    }
    NonZeroUsize::new(raw.unwrap_or(1))
        .ok_or_else(|| ConfigError::ResearchConcurrencyZero(wiki_name.to_string()))
}

fn nonempty_trimmed<'a>(
    value: &'a str,
    wiki_name: &str,
    error: fn(String) -> ConfigError,
) -> Result<&'a str, ConfigError> {
    let value = value.trim();
    if value.is_empty() {
        Err(error(wiki_name.to_string()))
    } else {
        Ok(value)
    }
}

fn nonzero_secs(
    secs: u64,
    wiki_name: &str,
    error: fn(String) -> ConfigError,
) -> Result<Duration, ConfigError> {
    if secs == 0 {
        Err(error(wiki_name.to_string()))
    } else {
        Ok(Duration::from_secs(secs))
    }
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

pub(super) fn normalize_wiki_config(
    config_dir: &Path,
    raw_wiki: RawWikiConfig,
    seen: &mut HashSet<String>,
) -> Result<AppConfig, ConfigError> {
    let RawWikiConfig {
        name,
        description,
        runner,
        agent_command,
        prompt_template,
        vcs_workflow,
        research_concurrency,
        mcp,
        git_sync,
        completed_task_ttl_secs,
        agent_timeout_secs,
    } = raw_wiki;

    validate_wiki_name(&name)?;
    if !seen.insert(name.clone()) {
        return Err(ConfigError::DuplicateWikiName(name));
    }
    let description = validate_description(&name, &description)?;
    validate_agent_command(&name, runner, &agent_command)?;
    let research_concurrency =
        validate_research_concurrency(&name, vcs_workflow, research_concurrency)?;
    let git_sync = validate_git_sync(&name, vcs_workflow, git_sync)?;

    let wiki_repo = wiki_repo_path(config_dir, &name);
    if !wiki_repo.is_dir() {
        return Err(ConfigError::WikiRepoMissing {
            name,
            path: wiki_repo,
        });
    }
    if !wiki_repo.join("wiki").is_dir() {
        return Err(ConfigError::WikiDirMissing {
            name,
            path: wiki_repo.join("wiki"),
        });
    }
    let prompt_template_content = load_prompt_template(&resolve(config_dir, prompt_template))?;

    let mcp_instructions = mcp
        .instructions
        .unwrap_or_else(|| default_mcp_instructions(&description));
    let research_tool_description = mcp
        .research_tool_description
        .unwrap_or_else(|| default_research_tool_description(&description));

    Ok(AppConfig {
        name,
        wiki_repo,
        description,
        runner,
        agent_command,
        prompt_template_content,
        vcs_workflow,
        research_concurrency,
        git_sync,
        mcp_instructions,
        research_tool_description,
        completed_task_ttl: Duration::from_secs(completed_task_ttl_secs),
        agent_timeout: Duration::from_secs(agent_timeout_secs),
    })
}
