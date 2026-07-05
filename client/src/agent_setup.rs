use crate::client_mirror::ClientMirror;

pub(super) fn render_agent_setup_prompt(
    server_url: &str,
    wikis: &[ClientMirror],
) -> anyhow::Result<String> {
    let wiki_names = wikis
        .iter()
        .map(ClientMirror::spec)
        .collect::<Vec<_>>()
        .join(",");
    let wiki_list = wikis
        .iter()
        .map(|wiki| {
            Ok(format!(
                "- `{}` (`{}/`): {}",
                wiki.name,
                wiki.local_path,
                compact_whitespace(wiki.description()?)
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .join("\n");
    let research_commands = wikis
        .iter()
        .map(|wiki| format!("- `wikidesk research -w {} \"<question>\"`", wiki.name))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(format!(
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
    ))
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_mirror::select_wikis;
    use wikidesk_shared::WikiInfo;

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
                ClientMirror::new("knowledge".into(), "wiki".into())
                    .with_description("Retrieval and epistemology.".into()),
                ClientMirror::new("ml".into(), "wiki-ml".into())
                    .with_description("Machine learning.".into()),
            ],
        )
        .unwrap();

        assert!(prompt.contains("export WIKIDESK_SERVER_URL=http://example.test"));
        assert!(prompt.contains("export WIKIDESK_WIKIS=knowledge:wiki,ml"));
        assert!(prompt.contains("`knowledge` (`wiki/`): Retrieval and epistemology."));
        assert!(prompt.contains("wikidesk research -w knowledge \"<question>\""));
        assert!(prompt.contains("Never edit local wiki mirror files directly."));
    }
}
