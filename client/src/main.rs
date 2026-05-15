use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use wikidesk_shared::{
    FileEntry, ResearchRequest, ResearchResponse, SyncRequest, SyncResponse, apply_sync,
    snapshot_dir,
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

struct ClientApp<T> {
    transport: T,
    wiki_path: PathBuf,
}

impl<T: WikideskTransport> ClientApp<T> {
    async fn research(&self, question: String) -> anyhow::Result<String> {
        let result = self
            .transport
            .research(question, self.wiki_path.to_string_lossy().into_owned())
            .await?;
        self.sync().await?;
        Ok(result.answer)
    }

    async fn sync(&self) -> anyhow::Result<SyncSummary> {
        let local_files = snapshot_dir(&self.wiki_path)?;
        let sync = self.transport.sync(local_files).await?;
        let summary = SyncSummary::from_response(&sync);
        apply_sync(&self.wiki_path, &sync)?;
        Ok(summary)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SyncSummary {
    updated: usize,
    deleted: usize,
}

impl SyncSummary {
    fn from_response(sync: &SyncResponse) -> Self {
        Self {
            updated: sync.upserts.len(),
            deleted: sync.deletes.len(),
        }
    }

    fn total(self) -> usize {
        self.updated + self.deleted
    }
}

#[async_trait]
trait WikideskTransport {
    async fn research(
        &self,
        question: String,
        wiki_path: String,
    ) -> anyhow::Result<ResearchResponse>;
    async fn sync(&self, files: Vec<FileEntry>) -> anyhow::Result<SyncResponse>;
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
    async fn research(
        &self,
        question: String,
        wiki_path: String,
    ) -> anyhow::Result<ResearchResponse> {
        Ok(self
            .client
            .post(format!("{}/api/research", self.server_url))
            .json(&ResearchRequest {
                question,
                wiki_path,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn sync(&self, files: Vec<FileEntry>) -> anyhow::Result<SyncResponse> {
        Ok(self
            .client
            .post(format!("{}/api/sync", self.server_url))
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
        transport: HttpTransport::new(config.server_url)?,
        wiki_path: config.wiki_path,
    };

    match cli.command {
        Command::Research { question } => {
            print!("{}", app.research(question).await?);
        }
        Command::Sync => {
            print_sync_summary(app.sync().await?);
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

fn print_sync_summary(summary: SyncSummary) {
    if summary.total() > 0 {
        eprintln!(
            "sync: {} updated, {} deleted",
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
        sync_calls: Arc<Mutex<usize>>,
        research_response: Option<ResearchResponse>,
        sync_response: Option<SyncResponse>,
    }

    #[async_trait]
    impl WikideskTransport for FakeTransport {
        async fn research(
            &self,
            question: String,
            wiki_path: String,
        ) -> anyhow::Result<ResearchResponse> {
            self.research_calls
                .lock()
                .unwrap()
                .push((question, wiki_path));
            Ok(self.research_response.clone().unwrap())
        }

        async fn sync(&self, _files: Vec<FileEntry>) -> anyhow::Result<SyncResponse> {
            *self.sync_calls.lock().unwrap() += 1;
            Ok(self.sync_response.clone().unwrap_or(SyncResponse {
                upserts: vec![],
                deletes: vec![],
            }))
        }
    }

    #[tokio::test]
    async fn research_submits_question_then_syncs_wiki() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
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
            wiki_path: wiki.clone(),
        };

        let answer = app.research("question?".into()).await.unwrap();

        assert_eq!(answer, "answer");
        assert_eq!(
            research_calls.lock().unwrap().as_slice(),
            &[("question?".into(), wiki.to_string_lossy().into_owned())]
        );
        assert_eq!(*sync_calls.lock().unwrap(), 1);
        assert_eq!(
            std::fs::read_to_string(wiki.join("notes.md")).unwrap(),
            "# Notes"
        );
    }

    #[tokio::test]
    async fn sync_returns_summary_and_applies_changes() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
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
            wiki_path: wiki.clone(),
        };

        let summary = app.sync().await.unwrap();

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

    #[test]
    fn sync_summary_counts_delta() {
        let summary = SyncSummary::from_response(&SyncResponse {
            upserts: vec![FileContent {
                path: "a.md".into(),
                content: "a".into(),
            }],
            deletes: vec!["b.md".into(), "c.md".into()],
        });

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
