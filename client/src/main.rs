use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use wikidesk_shared::{
    FileEntry, ResearchRequest, ResearchResponse, SyncPlan, SyncRequest, SyncResponse, SyncSummary,
    is_valid_wiki_name, snapshot_dir,
};

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
}

struct ClientConfig {
    server_url: String,
    wikis: Vec<String>,
}

impl ClientConfig {
    fn from_env() -> anyhow::Result<Self> {
        let server_url = std::env::var("WIKIDESK_SERVER_URL")
            .map_err(|_| anyhow::anyhow!("WIKIDESK_SERVER_URL not set"))?
            .trim_end_matches('/')
            .to_string();
        let wikis = parse_wikis(&std::env::var("WIKIDESK_WIKIS").map_err(|_| {
            anyhow::anyhow!("WIKIDESK_WIKIS not set (example: WIKIDESK_WIKIS=rlhf,rust-notes)")
        })?)?;
        Ok(Self { server_url, wikis })
    }

    fn require_wiki(&self, wiki: Option<String>) -> anyhow::Result<String> {
        let Some(wiki) = wiki else {
            anyhow::bail!(
                "--wiki/-w is required (configured wikis: {})",
                self.configured_wikis()
            );
        };
        self.validate_wiki(wiki)
    }

    fn selected_wikis(&self, wiki: Option<String>) -> anyhow::Result<Vec<String>> {
        match wiki {
            Some(wiki) => Ok(vec![self.validate_wiki(wiki)?]),
            None => Ok(self.wikis.clone()),
        }
    }

    fn validate_wiki(&self, wiki: String) -> anyhow::Result<String> {
        if self.wikis.contains(&wiki) {
            Ok(wiki)
        } else {
            anyhow::bail!(
                "unknown wiki '{wiki}' (configured wikis: {})",
                self.configured_wikis()
            );
        }
    }

    fn configured_wikis(&self) -> String {
        self.wikis.join(", ")
    }
}

fn parse_wikis(raw: &str) -> anyhow::Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut wikis = Vec::new();
    for wiki in raw
        .split(',')
        .map(str::trim)
        .filter(|wiki| !wiki.is_empty())
    {
        if !is_valid_wiki_name(wiki) {
            anyhow::bail!(
                "invalid wiki name '{wiki}' in WIKIDESK_WIKIS (use lowercase letters, digits, and hyphens; start and end with a letter or digit)"
            );
        }
        if !seen.insert(wiki.to_string()) {
            anyhow::bail!("duplicate wiki name '{wiki}' in WIKIDESK_WIKIS");
        }
        wikis.push(wiki.to_string());
    }
    if wikis.is_empty() {
        anyhow::bail!("WIKIDESK_WIKIS must name at least one wiki");
    }
    Ok(wikis)
}

struct ClientApp<T> {
    transport: T,
    workspace: PathBuf,
}

impl<T: WikideskTransport> ClientApp<T> {
    async fn research(&self, wiki: String, question: String) -> anyhow::Result<String> {
        let result = self.transport.research(&wiki, question).await?;
        self.sync_one(&wiki).await?;
        Ok(result.answer)
    }

    async fn sync_one(&self, wiki: &str) -> anyhow::Result<SyncSummary> {
        let wiki_path = self.wiki_path(wiki);
        let local_files = snapshot_dir(&wiki_path)?;
        let sync = self.transport.sync(wiki, local_files).await?;
        let plan = SyncPlan::new(sync);
        let summary = plan.summary();
        plan.apply(&wiki_path)?;
        Ok(summary)
    }

    async fn sync_all(&self, wikis: &[String]) -> anyhow::Result<Vec<(String, SyncSummary)>> {
        let mut summaries = Vec::with_capacity(wikis.len());
        for wiki in wikis {
            summaries.push((wiki.clone(), self.sync_one(wiki).await?));
        }
        Ok(summaries)
    }

    fn wiki_path(&self, wiki: &str) -> PathBuf {
        self.workspace.join(format!("wiki-{wiki}"))
    }
}

#[async_trait]
trait WikideskTransport {
    async fn research(&self, wiki: &str, question: String) -> anyhow::Result<ResearchResponse>;
    async fn sync(&self, wiki: &str, files: Vec<FileEntry>) -> anyhow::Result<SyncResponse>;
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
        Ok(Self { server_url, client })
    }
}

#[async_trait]
impl WikideskTransport for HttpTransport {
    async fn research(&self, wiki: &str, question: String) -> anyhow::Result<ResearchResponse> {
        Ok(self
            .client
            .post(format!("{}/{wiki}/api/research", self.server_url))
            .json(&ResearchRequest { question })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn sync(&self, wiki: &str, files: Vec<FileEntry>) -> anyhow::Result<SyncResponse> {
        Ok(self
            .client
            .post(format!("{}/{wiki}/api/sync", self.server_url))
            .json(&SyncRequest { files })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
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
    let app = ClientApp {
        transport: HttpTransport::new(config.server_url.clone())?,
        workspace: std::env::current_dir()?,
    };

    match cli.command {
        Command::Research { wiki, question } => {
            print!(
                "{}",
                app.research(config.require_wiki(wiki)?, question).await?
            );
        }
        Command::Sync { wiki } => {
            let wikis = config.selected_wikis(wiki)?;
            for (wiki, summary) in app.sync_all(&wikis).await? {
                print_sync_summary(&wiki, summary);
            }
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
    use std::sync::{Arc, Mutex};
    use wikidesk_shared::FileContent;

    #[derive(Default)]
    struct FakeTransport {
        research_calls: Arc<Mutex<Vec<(String, String)>>>,
        sync_calls: Arc<Mutex<Vec<String>>>,
        research_response: Option<ResearchResponse>,
        sync_response: Option<SyncResponse>,
    }

    #[async_trait]
    impl WikideskTransport for FakeTransport {
        async fn research(&self, wiki: &str, question: String) -> anyhow::Result<ResearchResponse> {
            self.research_calls
                .lock()
                .unwrap()
                .push((wiki.to_string(), question));
            Ok(self.research_response.clone().unwrap())
        }

        async fn sync(&self, wiki: &str, _files: Vec<FileEntry>) -> anyhow::Result<SyncResponse> {
            self.sync_calls.lock().unwrap().push(wiki.to_string());
            Ok(self.sync_response.clone().unwrap_or(SyncResponse {
                upserts: vec![],
                deletes: vec![],
            }))
        }
    }

    #[test]
    fn parses_wiki_list() {
        assert_eq!(
            parse_wikis("rlhf, rust-notes").unwrap(),
            ["rlhf", "rust-notes"]
        );
        assert!(parse_wikis("Wiki").is_err());
        assert!(parse_wikis("rlhf,rlhf").is_err());
    }

    #[test]
    fn research_requires_explicit_wiki() {
        let config = ClientConfig {
            server_url: "http://example.test".into(),
            wikis: vec!["rlhf".into(), "rust".into()],
        };

        let err = config.require_wiki(None).unwrap_err().to_string();

        assert!(err.contains("--wiki/-w is required"));
        assert!(err.contains("rlhf, rust"));
    }

    #[test]
    fn rejects_unknown_wiki() {
        let config = ClientConfig {
            server_url: "http://example.test".into(),
            wikis: vec!["rlhf".into()],
        };

        let err = config
            .require_wiki(Some("rust".into()))
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown wiki 'rust'"));
        assert!(err.contains("rlhf"));
    }

    #[tokio::test]
    async fn research_submits_question_then_syncs_wiki() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport {
            research_response: Some(ResearchResponse {
                answer: "answer".into(),
            }),
            sync_response: Some(SyncResponse {
                upserts: vec![FileContent {
                    path: "notes.md".into(),
                    content: "# Notes".into(),
                }],
                deletes: vec![],
            }),
            ..Default::default()
        };
        let research_calls = transport.research_calls.clone();
        let sync_calls = transport.sync_calls.clone();
        let app = ClientApp {
            transport,
            workspace: dir.path().to_path_buf(),
        };

        let answer = app
            .research("rlhf".into(), "question?".into())
            .await
            .unwrap();

        assert_eq!(answer, "answer");
        assert_eq!(
            research_calls.lock().unwrap().as_slice(),
            &[("rlhf".into(), "question?".into())]
        );
        assert_eq!(sync_calls.lock().unwrap().as_slice(), &["rlhf"]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("wiki-rlhf/notes.md")).unwrap(),
            "# Notes"
        );
    }

    #[tokio::test]
    async fn sync_returns_summary_and_applies_changes() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki-rlhf");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join("old.md"), "old").unwrap();
        let transport = FakeTransport {
            sync_response: Some(SyncResponse {
                upserts: vec![FileContent {
                    path: "new.md".into(),
                    content: "new".into(),
                }],
                deletes: vec!["old.md".into()],
            }),
            ..Default::default()
        };
        let app = ClientApp {
            transport,
            workspace: dir.path().to_path_buf(),
        };

        let summary = app.sync_one("rlhf").await.unwrap();

        assert_eq!(
            summary,
            SyncSummary {
                updated: 1,
                deleted: 1
            }
        );
        assert_eq!(std::fs::read_to_string(wiki.join("new.md")).unwrap(), "new");
        assert!(!wiki.join("old.md").exists());
    }

    #[tokio::test]
    async fn sync_all_syncs_every_configured_wiki() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::default();
        let sync_calls = transport.sync_calls.clone();
        let app = ClientApp {
            transport,
            workspace: dir.path().to_path_buf(),
        };

        let summaries = app.sync_all(&["rlhf".into(), "rust".into()]).await.unwrap();

        assert_eq!(sync_calls.lock().unwrap().as_slice(), &["rlhf", "rust"]);
        assert_eq!(summaries.len(), 2);
    }

    #[test]
    fn sync_summary_counts_delta() {
        let summary = SyncPlan::new(SyncResponse {
            upserts: vec![FileContent {
                path: "a.md".into(),
                content: "a".into(),
            }],
            deletes: vec!["b.md".into(), "c.md".into()],
        })
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
