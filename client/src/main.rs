use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use clap::{Parser, Subcommand};
use wikidesk_shared::{ResearchRequest, ResearchResponse, SyncRequest, SyncResponse, snapshot_dir};

#[derive(Parser)]
#[command(name = "wikidesk", about = "CLI client for wikidesk server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Submit a research question, sync wiki, and print the answer
    Research {
        /// The question to research
        question: String,
    },
    /// Sync local wiki directory with the server
    Sync,
}

struct ClientConfig {
    server_url: String,
    wiki_path: PathBuf,
}

impl ClientConfig {
    fn from_env() -> anyhow::Result<Self> {
        let server_url = std::env::var("WIKIDESK_SERVER_URL")
            .map_err(|_| anyhow::anyhow!("WIKIDESK_SERVER_URL not set"))?
            .trim_end_matches('/')
            .to_string();
        let wiki_path = std::env::var("WIKIDESK_WIKI_PATH")
            .map(PathBuf::from)
            .map_err(|_| anyhow::anyhow!("WIKIDESK_WIKI_PATH not set"))?;
        Ok(Self {
            server_url,
            wiki_path,
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Process env > .env.local > .env. dotenvy skips vars that are already set,
    // so loading .env.local first lets it shadow .env.
    load_dotenv(".env.local", dotenvy::from_filename(".env.local"));
    load_dotenv(".env", dotenvy::dotenv());

    let cli = Cli::parse();
    let config = ClientConfig::from_env()?;
    // Research can run for up to 30 minutes; pad to avoid client-side timeout.
    const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35 * 60);
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?;

    match cli.command {
        Command::Research { question } => {
            let wiki_path_str = config.wiki_path.to_string_lossy().into_owned();
            let result: ResearchResponse = client
                .post(format!("{}/api/research", config.server_url))
                .json(&ResearchRequest {
                    question,
                    wiki_path: wiki_path_str,
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            run_sync(&client, &config).await?;
            print!("{}", result.answer);
        }
        Command::Sync => {
            run_sync(&client, &config).await?;
        }
    }

    Ok(())
}

fn load_dotenv(name: &str, result: dotenvy::Result<PathBuf>) {
    if let Err(e) = result
        && !e.not_found()
    {
        eprintln!("warning: failed to load {name}: {e}");
    }
}

async fn run_sync(client: &reqwest::Client, config: &ClientConfig) -> anyhow::Result<()> {
    let local_files = snapshot_dir(&config.wiki_path)?;
    let sync: SyncResponse = client
        .post(format!("{}/api/sync", config.server_url))
        .json(&SyncRequest { files: local_files })
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    apply_sync(&config.wiki_path, &sync)?;

    let total = sync.upserts.len() + sync.deletes.len();
    if total > 0 {
        eprintln!(
            "sync: {} updated, {} deleted",
            sync.upserts.len(),
            sync.deletes.len()
        );
    }

    Ok(())
}

fn validate_relative_path(relative: &str) -> anyhow::Result<()> {
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir => {
                anyhow::bail!("server sent path with '..': '{relative}'")
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("server sent absolute path: '{relative}'")
            }
            _ => {}
        }
    }
    Ok(())
}

fn resolve_and_validate(wiki_canonical: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    validate_relative_path(relative)?;
    // Since relative is validated to have no ".." or absolute components,
    // joining it to the canonical base cannot escape.
    let target = wiki_canonical.join(relative);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(target)
}

fn apply_sync(wiki_path: &Path, sync: &SyncResponse) -> anyhow::Result<()> {
    std::fs::create_dir_all(wiki_path)?;
    let wiki_canonical = wiki_path.canonicalize()?;
    for file in &sync.upserts {
        let target = resolve_and_validate(&wiki_canonical, &file.path)?;
        std::fs::write(&target, &file.content)?;
    }

    let upserted: HashSet<&str> = sync.upserts.iter().map(|f| f.path.as_str()).collect();
    for path in &sync.deletes {
        if upserted.contains(path.as_str()) {
            continue;
        }
        validate_relative_path(path)?;
        let target = wiki_canonical.join(path);
        let canonical = match target.canonicalize() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if !canonical.starts_with(&wiki_canonical) {
            anyhow::bail!("resolved path escapes wiki directory: '{path}'");
        }
        std::fs::remove_file(&target)?;
    }

    Ok(())
}
