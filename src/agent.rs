use crate::config::AppConfig;
use crate::rewrite;

pub async fn run_agent(config: &AppConfig, question: &str) -> anyhow::Result<String> {
    let prompt = config
        .prompt_template_content
        .replace(crate::config::QUESTION_PLACEHOLDER, question);

    let args: Vec<_> = config
        .agent_command
        .iter()
        .map(|a| {
            if a == crate::config::PROMPT_PLACEHOLDER {
                prompt.as_str()
            } else {
                a.as_str()
            }
        })
        .collect();

    let child = tokio::process::Command::new(args[0])
        .args(&args[1..])
        .current_dir(&config.wiki_repo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    let output = match tokio::time::timeout(config.agent_timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => {
            anyhow::bail!(
                "agent timed out after {}s",
                config.agent_timeout.as_secs()
            );
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("agent exited with {}: {}", output.status, stderr.trim());
    }

    Ok(rewrite::rewrite_wikilinks(
        &String::from_utf8_lossy(&output.stdout),
        &config.wiki_repo,
    ))
}
