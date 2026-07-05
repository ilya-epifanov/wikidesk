use std::path::PathBuf;

use clap::{Parser, Subcommand};
use wikidesk_shared::sync::{
    FileEntry, SyncRequest, SyncResponse, SyncSummary, ensure_local_mirror_safe,
    snapshot_local_mirror,
};
use wikidesk_shared::{
    ListWikisResponse, ResearchRequest, ResearchResponse, WIKI_LIST_PATH, wiki_base_path,
};

use client_mirror::{ClientMirror, parse_wikis};

mod agent_setup;
mod client_mirror;

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
        /// Wiki name from WIKIDESK_WIKIS
        #[arg(short = 'w', long = "wiki")]
        wiki: Option<String>,
        /// The question to research
        question: String,
    },
    /// Sync local wiki directory with the server
    Sync {
        /// Wiki name from WIKIDESK_WIKIS. Omit to sync all wikis.
        #[arg(short = 'w', long = "wiki")]
        wiki: Option<String>,
    },
    /// Generate prompts for agent configuration
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Print a prompt that configures a repository to use wikidesk
    Setup {
        /// Wikidesk server URL
        server_url: String,
        /// Wiki names to configure. Omit to configure all server wikis.
        wikis: Vec<String>,
    },
}

struct ClientConfig {
    server_url: String,
    wikis: Vec<ClientMirror>,
}

impl ClientConfig {
    fn from_env() -> anyhow::Result<Self> {
        let server_url = trim_server_url(
            &std::env::var("WIKIDESK_SERVER_URL")
                .map_err(|_| anyhow::anyhow!("WIKIDESK_SERVER_URL not set"))?,
        );
        let wikis = parse_wikis(&std::env::var("WIKIDESK_WIKIS").map_err(|_| {
            anyhow::anyhow!("WIKIDESK_WIKIS not set (example: WIKIDESK_WIKIS=default,ml:wiki-ml)")
        })?)?;
        Ok(Self { server_url, wikis })
    }

    fn require_wiki(&self, wiki: Option<String>) -> anyhow::Result<ClientMirror> {
        let Some(wiki) = wiki else {
            anyhow::bail!(
                "--wiki/-w is required (configured wikis: {})",
                self.configured_wikis()
            );
        };
        self.validate_wiki(wiki)
    }

    fn selected_wikis(&self, wiki: Option<String>) -> anyhow::Result<Vec<ClientMirror>> {
        match wiki {
            Some(wiki) => Ok(vec![self.validate_wiki(wiki)?]),
            None => Ok(self.wikis.clone()),
        }
    }

    fn validate_wiki(&self, wiki: String) -> anyhow::Result<ClientMirror> {
        self.wikis
            .iter()
            .find(|configured| configured.name == wiki)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown wiki '{wiki}' (configured wikis: {})",
                    self.configured_wikis()
                )
            })
    }

    fn configured_wikis(&self) -> String {
        self.wikis
            .iter()
            .map(|wiki| wiki.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn trim_server_url(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
}

async fn sync_one(
    transport: &HttpTransport,
    workspace: &std::path::Path,
    wiki: &ClientMirror,
) -> anyhow::Result<SyncSummary> {
    let wiki_path = workspace.join(&wiki.local_path);
    ensure_local_mirror_safe(&wiki_path)?;
    let local_files = snapshot_local_mirror(&wiki_path)?;
    let sync = transport.sync(&wiki.name, local_files).await?;
    let summary = sync.summary();
    sync.apply(&wiki_path)?;
    Ok(summary)
}

struct HttpTransport {
    server_url: String,
    client: reqwest::Client,
}

impl HttpTransport {
    fn new(server_url: String) -> anyhow::Result<Self> {
        // Research can run for up to 30 minutes; pad to avoid client-side timeout.
        const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35 * 60);
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            server_url: trim_server_url(&server_url),
            client,
        })
    }

    async fn list_wikis(&self) -> anyhow::Result<ListWikisResponse> {
        Ok(self
            .client
            .get(format!("{}{}", self.server_url, WIKI_LIST_PATH))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn research(
        &self,
        wiki: &str,
        local_path: &str,
        question: String,
    ) -> anyhow::Result<ResearchResponse> {
        Ok(self
            .client
            .post(format!(
                "{}{}/api/research",
                self.server_url,
                wiki_base_path(wiki)
            ))
            .json(&ResearchRequest {
                question,
                local_path: Some(local_path.to_string()),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn sync(&self, wiki: &str, files: Vec<FileEntry>) -> anyhow::Result<SyncResponse> {
        Ok(self
            .client
            .post(format!(
                "{}{}/api/sync",
                self.server_url,
                wiki_base_path(wiki)
            ))
            .json(&SyncRequest { files })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

async fn render_agent_setup(
    server_url: String,
    requested_wikis: Vec<String>,
) -> anyhow::Result<String> {
    let transport = HttpTransport::new(server_url)?;
    let available = transport.list_wikis().await?.wikis;
    let wikis = client_mirror::select_wikis(available, requested_wikis)?;
    agent_setup::render_agent_setup_prompt(&transport.server_url, &wikis)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Process env > .env.local > .env. dotenvy skips vars that are already set,
    // so loading .env.local first lets it shadow .env.
    load_dotenv(".env.local", dotenvy::from_filename(".env.local"));
    load_dotenv(".env", dotenvy::dotenv());

    match Cli::parse().command {
        Command::Research { wiki, question } => {
            let config = ClientConfig::from_env()?;
            let wiki = config.require_wiki(wiki)?;
            let transport = HttpTransport::new(config.server_url)?;
            let workspace = std::env::current_dir()?;
            let result = transport
                .research(&wiki.name, &wiki.local_path, question)
                .await?;
            sync_one(&transport, &workspace, &wiki).await?;
            print!("{}", result.answer);
        }
        Command::Sync { wiki } => {
            let config = ClientConfig::from_env()?;
            let wikis = config.selected_wikis(wiki)?;
            let transport = HttpTransport::new(config.server_url)?;
            let workspace = std::env::current_dir()?;
            let mut summaries = Vec::with_capacity(wikis.len());
            for wiki in &wikis {
                summaries.push((
                    wiki.name.clone(),
                    sync_one(&transport, &workspace, wiki).await?,
                ));
            }
            for (wiki, summary) in summaries {
                print_sync_summary(&wiki, summary);
            }
        }
        Command::Agent {
            command: AgentCommand::Setup { server_url, wikis },
        } => print!("{}", render_agent_setup(server_url, wikis).await?),
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

fn print_sync_summary(wiki: &str, summary: SyncSummary) {
    if summary.total() > 0 {
        eprintln!(
            "sync {wiki}: {} updated, {} deleted",
            summary.updated, summary.deleted
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikidesk_shared::sync::FileContent;

    fn configured(name: &str, local_path: &str) -> ClientMirror {
        ClientMirror::new(name.into(), local_path.into())
    }

    #[test]
    fn parses_wiki_list() {
        assert_eq!(
            parse_wikis("default, ml:mirrors/ml").unwrap(),
            [
                configured("default", "wiki"),
                configured("ml", "mirrors/ml")
            ]
        );
        assert!(parse_wikis("Wiki").is_err());
        assert!(parse_wikis("rlhf,rlhf").is_err());
        assert!(parse_wikis("audio:wiki,default:wiki").is_err());
        assert!(parse_wikis("ml:../wiki").is_err());
    }

    #[test]
    fn research_requires_explicit_wiki() {
        let config = ClientConfig {
            server_url: "http://example.test".into(),
            wikis: vec![
                configured("rlhf", "wiki-rlhf"),
                configured("rust", "wiki-rust"),
            ],
        };

        let err = config.require_wiki(None).unwrap_err().to_string();

        assert!(err.contains("--wiki/-w is required"));
        assert!(err.contains("rlhf, rust"));
    }

    #[test]
    fn rejects_unknown_wiki() {
        let config = ClientConfig {
            server_url: "http://example.test".into(),
            wikis: vec![configured("rlhf", "wiki-rlhf")],
        };

        let err = config
            .require_wiki(Some("rust".into()))
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown wiki 'rust'"));
        assert!(err.contains("rlhf"));
    }

    #[test]
    fn sync_summary_counts_delta() {
        let summary = SyncResponse {
            upserts: vec![FileContent {
                path: "a.md".into(),
                content: "a".into(),
            }],
            deletes: vec!["b.md".into(), "c.md".into()],
        }
        .summary();

        assert_eq!(
            summary,
            SyncSummary {
                updated: 1,
                deleted: 2
            }
        );
        assert_eq!(summary.total(), 3);
    }
}
