use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

static WIKILINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]]+)\]\]").unwrap());

enum PageEntry {
    Unique(PathBuf),
    Ambiguous,
}

fn build_page_map(wiki_dir: &Path) -> std::io::Result<HashMap<String, PageEntry>> {
    let mut map = HashMap::new();
    visit_dir(wiki_dir, wiki_dir, &mut map)?;
    Ok(map)
}

fn visit_dir(
    base: &Path,
    dir: &Path,
    map: &mut HashMap<String, PageEntry>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            visit_dir(base, &path, map)?;
        } else if path.extension().is_some_and(|e| e == "md")
            && let Some(stem) = path.file_stem()
        {
            let key = stem.to_string_lossy().to_lowercase();
            let rel = path.strip_prefix(base).unwrap().to_path_buf();
            map.entry(key)
                .and_modify(|entry| {
                    if let PageEntry::Unique(existing) = entry {
                        tracing::warn!(
                            existing = %existing.display(),
                            duplicate = %rel.display(),
                            "duplicate page stem, leaving wikilink unresolved",
                        );
                        *entry = PageEntry::Ambiguous;
                    }
                })
                .or_insert(PageEntry::Unique(rel));
        }
    }
    Ok(())
}

pub fn rewrite_wikilinks(text: &str, wiki_repo: &Path) -> String {
    let wiki_dir = wiki_repo.join("wiki");
    let page_map = match build_page_map(&wiki_dir) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("failed to build page map: {e}");
            return text.to_string();
        }
    };

    WIKILINK_RE
        .replace_all(text, |caps: &regex::Captures| {
            let inner = &caps[1];
            let (page, display) = inner
                .split_once('|')
                .unwrap_or((inner, inner));
            match page_map.get(&page.to_lowercase()) {
                Some(PageEntry::Unique(rel_path)) => {
                    format!("[{display}](wiki/{})", rel_path.display())
                }
                Some(PageEntry::Ambiguous) | None => format!("[{display}]()"),
            }
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_wiki(dir: &Path) {
        let wiki = dir.join("wiki");
        fs::create_dir_all(wiki.join("concepts")).unwrap();
        fs::create_dir_all(wiki.join("topics")).unwrap();
        fs::write(wiki.join("concepts/RLHF.md"), "# RLHF").unwrap();
        fs::write(wiki.join("concepts/DPO.md"), "# DPO").unwrap();
        fs::write(wiki.join("topics/alignment.md"), "# Alignment").unwrap();
    }

    #[test]
    fn rewrites_known_wikilinks() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "See [[RLHF]] and [[DPO]] for details.";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(
            result,
            "See [RLHF](wiki/concepts/RLHF.md) and [DPO](wiki/concepts/DPO.md) for details."
        );
    }

    #[test]
    fn rewrites_unknown_wikilinks_to_empty_href() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "See [[NonExistent]] for more.";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(result, "See [NonExistent]() for more.");
    }

    #[test]
    fn preserves_text_without_wikilinks() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "No links here.";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(result, "No links here.");
    }

    #[test]
    fn rewrites_nested_directory_links() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "Read [[alignment]] for background.";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(
            result,
            "Read [alignment](wiki/topics/alignment.md) for background."
        );
    }

    #[test]
    fn rewrites_pipe_wikilink_with_display_text() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "See [[RLHF|reinforcement learning from human feedback]].";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(
            result,
            "See [reinforcement learning from human feedback](wiki/concepts/RLHF.md)."
        );
    }

    #[test]
    fn pipe_wikilink_unknown_page_produces_dead_link() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "See [[missing|some description]].";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(result, "See [some description]().");
    }

    #[test]
    fn duplicate_stems_leave_wikilink_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(wiki.join("a")).unwrap();
        fs::create_dir_all(wiki.join("b")).unwrap();
        fs::write(wiki.join("a/overview.md"), "# Overview A").unwrap();
        fs::write(wiki.join("b/overview.md"), "# Overview B").unwrap();

        let input = "See [[overview]].";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(result, "See [overview]().");
    }

    #[test]
    fn case_insensitive_matching() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let input = "See [[rlhf]] and [[dpo]] and [[Alignment]].";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(
            result,
            "See [rlhf](wiki/concepts/RLHF.md) and [dpo](wiki/concepts/DPO.md) and [Alignment](wiki/topics/alignment.md)."
        );
    }

    #[test]
    fn handles_missing_wiki_dir_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        // No wiki/ directory created
        let input = "See [[RLHF]].";
        let result = rewrite_wikilinks(input, dir.path());
        assert_eq!(result, "See [[RLHF]].");
    }
}
