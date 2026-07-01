use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use wikidesk_shared::{
    FileEntry, ListWikisResponse, ResearchRequest, ResearchResponse, SyncPlan, SyncRequest,
    SyncResponse, SyncSummary, WikiInfo, is_valid_wiki_name, snapshot_local_mirror,
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
    wikis: Vec<String>,
}

impl ClientConfig {
    fn from_env() -> anyhow::Result<Self> {
        let server_url = trim_server_url(
            &std::env::var("WIKIDESK_SERVER_URL")
                .map_err(|_| anyhow::anyhow!("WIKIDESK_SERVER_URL not set"))?,
        );
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

fn trim_server_url(raw: &str) -> String {
    raw.trim_end_matches('/').to_string()
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
        let local_files = snapshot_local_mirror(&wiki_path)?;
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
        Ok(Self {
            server_url: trim_server_url(&server_url),
            client,
        })
    }

    async fn list_wikis(&self) -> anyhow::Result<ListWikisResponse> {
        Ok(self
            .client
            .get(format!("{}/api/wikis", self.server_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
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

async fn render_agent_setup(
    server_url: String,
    requested_wikis: Vec<String>,
) -> anyhow::Result<String> {
    let transport = HttpTransport::new(server_url)?;
    let available = transport.list_wikis().await?.wikis;
    let wikis = select_wikis(available, requested_wikis)?;
    Ok(render_agent_setup_prompt(&transport.server_url, &wikis))
}

fn select_wikis(available: Vec<WikiInfo>, requested: Vec<String>) -> anyhow::Result<Vec<WikiInfo>> {
    let mut seen = HashSet::new();
    for wiki in &requested {
        if !is_valid_wiki_name(wiki) {
            anyhow::bail!(
                "invalid wiki name '{wiki}' (use lowercase letters, digits, and hyphens; start and end with a letter or digit)"
            );
        }
        if !seen.insert(wiki) {
            anyhow::bail!("duplicate wiki name '{wiki}'");
        }
    }

    let mut by_name = HashMap::new();
    let mut ordered = Vec::new();
    for wiki in available {
        if wiki.description.trim().is_empty() {
            anyhow::bail!("server returned empty description for wiki '{}'", wiki.name);
        }
        by_name.insert(wiki.name.clone(), wiki.clone());
        ordered.push(wiki);
    }

    if requested.is_empty() {
        return Ok(ordered);
    }

    let mut selected = Vec::with_capacity(requested.len());
    let mut missing = Vec::new();
    for name in requested {
        match by_name.get(&name) {
            Some(wiki) => selected.push(wiki.clone()),
            None => missing.push(name),
        }
    }
    if !missing.is_empty() {
        let mut available = by_name.into_keys().collect::<Vec<_>>();
        available.sort();
        anyhow::bail!(
            "server does not advertise wiki(s): {} (available: {})",
            missing.join(", "),
            available.join(", ")
        );
    }
    Ok(selected)
}

fn render_agent_setup_prompt(server_url: &str, wikis: &[WikiInfo]) -> String {
    let wiki_names = wikis
        .iter()
        .map(|wiki| wiki.name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let wiki_list = wikis
        .iter()
        .map(|wiki| {
            format!(
                "- `{}` (`wiki-{}/`): {}",
                wiki.name,
                wiki.name,
                compact_whitespace(&wiki.description)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let research_commands = wikis
        .iter()
        .map(|wiki| format!("- `wikidesk research -w {} \"<question>\"`", wiki.name))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"Configure this repository to use wikidesk.

Wikidesk keeps local wiki mirrors in directories named `wiki-{{name}}/`. A local wiki mirror is managed by `wikidesk sync`; do not edit files inside it directly.

Use this server and wiki set:

```sh
export WIKIDESK_SERVER_URL={server_url}
export WIKIDESK_WIKIS={wiki_names}
wikidesk sync
```

Wikis to configure:
{wiki_list}

Do these steps:

1. Persist `WIKIDESK_SERVER_URL` and `WIKIDESK_WIKIS` in this repository's local environment mechanism, if it has one.
2. Run `wikidesk sync` once as a connection test. It should create or update the `wiki-{{name}}/` directories and place a `.gitignore` file in each one.
3. If this agent environment supports lifecycle hooks, configure them to run `wikidesk sync` before and after agent sessions or tool use. If hooks cannot be configured, mention in AGENTS.md/CLAUDE.md that agents should run `wikidesk sync` at session start and end.
4. Update AGENTS.md, and CLAUDE.md if this repository uses it, with brief wiki-use rules equivalent to:

```md
## Wikidesk

Local wiki mirrors are read-only directories managed by wikidesk:
{wiki_list}

Use these local wiki mirrors before answering questions related to their topics. If a local mirror may not cover the full picture, including adjacent knowledge that may not have been researched yet, run the relevant research command:
{research_commands}

Never edit `wiki-*` files directly.
```

5. If this agent environment supports file permissions, deny writes under `wiki-*`. If it does not, rely on the AGENTS.md/CLAUDE.md rule above.
"#
    )
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
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
            let app = ClientApp {
                transport: HttpTransport::new(config.server_url.clone())?,
                workspace: std::env::current_dir()?,
            };
            print!(
                "{}",
                app.research(config.require_wiki(wiki)?, question).await?
            );
        }
        Command::Sync { wiki } => {
            let config = ClientConfig::from_env()?;
            let app = ClientApp {
                transport: HttpTransport::new(config.server_url.clone())?,
                workspace: std::env::current_dir()?,
            };
            let wikis = config.selected_wikis(wiki)?;
            for (wiki, summary) in app.sync_all(&wikis).await? {
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
    use std::sync::{Arc, Mutex};
    use wikidesk_shared::FileContent;

    #[derive(Default)]
    struct FakeTransport {
        research_calls: Arc<Mutex<Vec<(String, String)>>>,
        sync_calls: Arc<Mutex<Vec<String>>>,
        sync_snapshots: Arc<Mutex<Vec<Vec<String>>>>,
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

        async fn sync(&self, wiki: &str, files: Vec<FileEntry>) -> anyhow::Result<SyncResponse> {
            self.sync_calls.lock().unwrap().push(wiki.to_string());
            self.sync_snapshots
                .lock()
                .unwrap()
                .push(files.into_iter().map(|file| file.path).collect::<Vec<_>>());
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

    #[test]
    fn selects_requested_wikis_from_server_list() {
        let selected = select_wikis(
            vec![
                WikiInfo {
                    name: "ml".into(),
                    description: "Machine learning.".into(),
                },
                WikiInfo {
                    name: "knowledge".into(),
                    description: "Retrieval.".into(),
                },
            ],
            vec!["knowledge".into()],
        )
        .unwrap();

        assert_eq!(selected[0].name, "knowledge");
    }

    #[test]
    fn setup_prompt_contains_agent_instructions() {
        let prompt = render_agent_setup_prompt(
            "http://example.test",
            &[
                WikiInfo {
                    name: "knowledge".into(),
                    description: "Retrieval and epistemology.".into(),
                },
                WikiInfo {
                    name: "ml".into(),
                    description: "Machine learning.".into(),
                },
            ],
        );

        assert!(prompt.contains("export WIKIDESK_SERVER_URL=http://example.test"));
        assert!(prompt.contains("export WIKIDESK_WIKIS=knowledge,ml"));
        assert!(prompt.contains("`knowledge` (`wiki-knowledge/`): Retrieval and epistemology."));
        assert!(prompt.contains("wikidesk research -w knowledge \"<question>\""));
        assert!(prompt.contains("Never edit `wiki-*` files directly."));
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
        assert_eq!(
            std::fs::read_to_string(dir.path().join("wiki-rlhf/.gitignore")).unwrap(),
            "*\n"
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
        assert_eq!(
            std::fs::read_to_string(wiki.join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    #[tokio::test]
    async fn sync_ignores_local_gitignore_in_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki-rlhf");
        std::fs::create_dir_all(&wiki).unwrap();
        std::fs::write(wiki.join(".gitignore"), "*\n").unwrap();
        std::fs::write(wiki.join("notes.md"), "old").unwrap();
        let transport = FakeTransport::default();
        let sync_snapshots = transport.sync_snapshots.clone();
        let app = ClientApp {
            transport,
            workspace: dir.path().to_path_buf(),
        };

        app.sync_one("rlhf").await.unwrap();

        let snapshots = sync_snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert!(snapshots[0].contains(&"notes.md".to_string()));
        assert!(!snapshots[0].contains(&".gitignore".to_string()));
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
