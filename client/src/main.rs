use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use wikidesk_shared::{
    FileEntry, ListWikisResponse, ResearchRequest, ResearchResponse, SyncPlan, SyncRequest,
    SyncResponse, SyncSummary, WIKI_LIST_PATH, WikiInfo, derived_wiki_path,
    ensure_local_mirror_safe, is_valid_wiki_name, snapshot_local_mirror, validate_local_path,
    wiki_base_path,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredWiki {
    name: String,
    local_path: String,
}

fn wiki_spec(name: &str, local_path: &str) -> String {
    if local_path == derived_wiki_path(name) {
        name.to_string()
    } else {
        format!("{name}:{local_path}")
    }
}

struct ClientConfig {
    server_url: String,
    wikis: Vec<ConfiguredWiki>,
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

    fn require_wiki(&self, wiki: Option<String>) -> anyhow::Result<ConfiguredWiki> {
        let Some(wiki) = wiki else {
            anyhow::bail!(
                "--wiki/-w is required (configured wikis: {})",
                self.configured_wikis()
            );
        };
        self.validate_wiki(wiki)
    }

    fn selected_wikis(&self, wiki: Option<String>) -> anyhow::Result<Vec<ConfiguredWiki>> {
        match wiki {
            Some(wiki) => Ok(vec![self.validate_wiki(wiki)?]),
            None => Ok(self.wikis.clone()),
        }
    }

    fn validate_wiki(&self, wiki: String) -> anyhow::Result<ConfiguredWiki> {
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

fn parse_wikis(raw: &str) -> anyhow::Result<Vec<ConfiguredWiki>> {
    let wikis = parse_wiki_specs([raw])?;
    if wikis.is_empty() {
        anyhow::bail!("WIKIDESK_WIKIS must name at least one wiki");
    }
    Ok(wikis)
}

fn parse_wiki_specs<I, S>(specs: I) -> anyhow::Result<Vec<ConfiguredWiki>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    let mut wikis = Vec::new();
    for raw in specs {
        for spec in raw
            .as_ref()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let wiki = parse_wiki_spec(spec)?;
            if !seen_names.insert(wiki.name.clone()) {
                anyhow::bail!("duplicate wiki name '{}'", wiki.name);
            }
            if !seen_paths.insert(wiki.local_path.clone()) {
                anyhow::bail!("duplicate local_path '{}'", wiki.local_path);
            }
            wikis.push(wiki);
        }
    }
    Ok(wikis)
}

fn parse_wiki_spec(spec: &str) -> anyhow::Result<ConfiguredWiki> {
    let (name, local_path) = match spec.split_once(':') {
        Some((name, local_path)) => (name.trim(), Some(local_path.trim())),
        None => (spec.trim(), None),
    };
    if !is_valid_wiki_name(name) {
        anyhow::bail!(
            "invalid wiki name '{name}' (use lowercase letters, digits, and hyphens; start and end with a letter or digit)"
        );
    }
    let local_path = match local_path {
        Some(path) => {
            validate_local_path(path)
                .map_err(|e| anyhow::anyhow!("invalid local_path for wiki '{name}': {e}"))?;
            path.to_string()
        }
        None => derived_wiki_path(name),
    };
    Ok(ConfiguredWiki {
        name: name.to_string(),
        local_path,
    })
}

struct ClientApp<T> {
    transport: T,
    workspace: PathBuf,
}

impl<T: WikideskTransport> ClientApp<T> {
    async fn research(&self, wiki: ConfiguredWiki, question: String) -> anyhow::Result<String> {
        let result = self
            .transport
            .research(&wiki.name, &wiki.local_path, question)
            .await?;
        self.sync_one(&wiki).await?;
        Ok(result.answer)
    }

    async fn sync_one(&self, wiki: &ConfiguredWiki) -> anyhow::Result<SyncSummary> {
        let wiki_path = self.wiki_path(wiki);
        ensure_local_mirror_safe(&wiki_path)?;
        let local_files = snapshot_local_mirror(&wiki_path)?;
        let sync = self.transport.sync(&wiki.name, local_files).await?;
        let plan = SyncPlan::new(sync);
        let summary = plan.summary();
        plan.apply(&wiki_path)?;
        Ok(summary)
    }

    async fn sync_all(
        &self,
        wikis: &[ConfiguredWiki],
    ) -> anyhow::Result<Vec<(String, SyncSummary)>> {
        let mut summaries = Vec::with_capacity(wikis.len());
        for wiki in wikis {
            summaries.push((wiki.name.clone(), self.sync_one(wiki).await?));
        }
        Ok(summaries)
    }

    fn wiki_path(&self, wiki: &ConfiguredWiki) -> PathBuf {
        self.workspace.join(&wiki.local_path)
    }
}

#[async_trait]
trait WikideskTransport {
    async fn research(
        &self,
        wiki: &str,
        local_path: &str,
        question: String,
    ) -> anyhow::Result<ResearchResponse>;
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
            .get(format!("{}{}", self.server_url, WIKI_LIST_PATH))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

#[async_trait]
impl WikideskTransport for HttpTransport {
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
    let wikis = select_wikis(available, requested_wikis)?;
    Ok(render_agent_setup_prompt(&transport.server_url, &wikis))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedWiki {
    name: String,
    description: String,
    local_path: String,
}

fn select_wikis(
    available: Vec<WikiInfo>,
    requested: Vec<String>,
) -> anyhow::Result<Vec<SelectedWiki>> {
    let requested = parse_wiki_specs(requested.iter().map(String::as_str))?;
    let mut by_name = HashMap::new();
    let mut ordered = Vec::new();
    for wiki in available {
        if wiki.description.trim().is_empty() {
            anyhow::bail!("server returned empty description for wiki '{}'", wiki.name);
        }
        by_name.insert(wiki.name.clone(), wiki.clone());
        ordered.push(SelectedWiki {
            local_path: derived_wiki_path(&wiki.name),
            name: wiki.name,
            description: wiki.description,
        });
    }

    if requested.is_empty() {
        return Ok(ordered);
    }

    let mut selected = Vec::with_capacity(requested.len());
    let mut missing = Vec::new();
    for configured in requested {
        match by_name.get(&configured.name) {
            Some(wiki) => selected.push(SelectedWiki {
                name: wiki.name.clone(),
                description: wiki.description.clone(),
                local_path: configured.local_path,
            }),
            None => missing.push(configured.name),
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

fn render_agent_setup_prompt(server_url: &str, wikis: &[SelectedWiki]) -> String {
    let wiki_names = wikis
        .iter()
        .map(|wiki| wiki_spec(&wiki.name, &wiki.local_path))
        .collect::<Vec<_>>()
        .join(",");
    let wiki_list = wikis
        .iter()
        .map(|wiki| {
            format!(
                "- `{}` (`{}/`): {}",
                wiki.name,
                wiki.local_path,
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

Wikidesk keeps local wiki mirrors in configured relative paths. A local wiki mirror is managed by `wikidesk sync`; do not edit files inside it directly.

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
2. Run `wikidesk sync` once as a connection test. It should create or update the listed local mirror paths and place a `.gitignore` file in each one.
3. If this agent environment supports lifecycle hooks, configure them to run `wikidesk sync` before and after agent sessions or tool use. If hooks cannot be configured, mention in AGENTS.md/CLAUDE.md that agents should run `wikidesk sync` at session start and end.
4. Update AGENTS.md, and CLAUDE.md if this repository uses it, with brief wiki-use rules equivalent to:

```md
## Wikidesk

Local wiki mirrors are read-only directories managed by wikidesk:
{wiki_list}

Use these local wiki mirrors before answering questions related to their topics. If a local mirror may not cover the full picture, including adjacent knowledge that may not have been researched yet, run the relevant research command:
{research_commands}

Never edit local wiki mirror files directly.
```

5. If this agent environment supports file permissions, deny writes under the listed local mirror paths. If it does not, rely on the AGENTS.md/CLAUDE.md rule above.
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
        research_calls: Arc<Mutex<Vec<(String, String, String)>>>,
        sync_calls: Arc<Mutex<Vec<String>>>,
        sync_snapshots: Arc<Mutex<Vec<Vec<String>>>>,
        research_response: Option<ResearchResponse>,
        sync_response: Option<SyncResponse>,
    }

    fn configured(name: &str, local_path: &str) -> ConfiguredWiki {
        ConfiguredWiki {
            name: name.into(),
            local_path: local_path.into(),
        }
    }

    #[async_trait]
    impl WikideskTransport for FakeTransport {
        async fn research(
            &self,
            wiki: &str,
            local_path: &str,
            question: String,
        ) -> anyhow::Result<ResearchResponse> {
            self.research_calls.lock().unwrap().push((
                wiki.to_string(),
                local_path.to_string(),
                question,
            ));
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
            vec!["knowledge:wiki".into()],
        )
        .unwrap();

        assert_eq!(selected[0].name, "knowledge");
        assert_eq!(selected[0].local_path, "wiki");
    }

    #[test]
    fn setup_prompt_contains_agent_instructions() {
        let prompt = render_agent_setup_prompt(
            "http://example.test",
            &[
                SelectedWiki {
                    name: "knowledge".into(),
                    description: "Retrieval and epistemology.".into(),
                    local_path: "wiki".into(),
                },
                SelectedWiki {
                    name: "ml".into(),
                    description: "Machine learning.".into(),
                    local_path: "wiki-ml".into(),
                },
            ],
        );

        assert!(prompt.contains("export WIKIDESK_SERVER_URL=http://example.test"));
        assert!(prompt.contains("export WIKIDESK_WIKIS=knowledge:wiki,ml"));
        assert!(prompt.contains("`knowledge` (`wiki/`): Retrieval and epistemology."));
        assert!(prompt.contains("wikidesk research -w knowledge \"<question>\""));
        assert!(prompt.contains("Never edit local wiki mirror files directly."));
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
            .research(configured("rlhf", "wiki-rlhf"), "question?".into())
            .await
            .unwrap();

        assert_eq!(answer, "answer");
        assert_eq!(
            research_calls.lock().unwrap().as_slice(),
            &[("rlhf".into(), "wiki-rlhf".into(), "question?".into())]
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
        std::fs::write(wiki.join(".gitignore"), "*\n").unwrap();
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

        let summary = app
            .sync_one(&configured("rlhf", "wiki-rlhf"))
            .await
            .unwrap();

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

        app.sync_one(&configured("rlhf", "wiki-rlhf"))
            .await
            .unwrap();

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

        let summaries = app
            .sync_all(&[
                configured("rlhf", "wiki-rlhf"),
                configured("rust", "wiki-rust"),
            ])
            .await
            .unwrap();

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
