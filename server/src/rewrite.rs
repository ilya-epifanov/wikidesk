use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use wikidesk_shared::sync::walk_markdown_files;

static WIKILINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]]+)\]\]").unwrap());

enum PageEntry {
    Unique(PathBuf),
    Ambiguous,
}

pub struct WikiLinkIndex {
    page_map: HashMap<String, PageEntry>,
}

impl WikiLinkIndex {
    fn from_wiki_dir(wiki_dir: &Path) -> std::io::Result<Self> {
        if !wiki_dir.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("wiki directory '{}' not found", wiki_dir.display()),
            ));
        }
        let mut page_map = HashMap::new();
        for file in
            walk_markdown_files(wiki_dir).map_err(|e| std::io::Error::other(e.to_string()))?
        {
            if let Some(stem) = file.absolute_path.file_stem() {
                let key = stem.to_string_lossy().to_lowercase();
                let rel = PathBuf::from(file.path);
                page_map
                    .entry(key)
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
        Ok(Self { page_map })
    }

    fn resolve(&self, page: &str) -> LinkTarget<'_> {
        match self.page_map.get(&page.to_lowercase()) {
            Some(PageEntry::Unique(path)) => LinkTarget::Resolved(path),
            Some(PageEntry::Ambiguous) => LinkTarget::Ambiguous,
            None => LinkTarget::Missing,
        }
    }
}

enum LinkTarget<'a> {
    Resolved(&'a Path),
    Ambiguous,
    Missing,
}

/// Renders wiki-style links for answers returned by a research agent.
pub struct WikiLinkRenderer {
    index: WikiLinkIndex,
}

impl WikiLinkRenderer {
    pub fn from_wiki_dir(wiki_dir: &Path) -> std::io::Result<Self> {
        Ok(Self {
            index: WikiLinkIndex::from_wiki_dir(wiki_dir)?,
        })
    }

    pub fn render_markdown_links(&self, text: &str, link_prefix: &str) -> String {
        WIKILINK_RE
            .replace_all(text, |caps: &regex::Captures| {
                let link = WikiLink::parse(&caps[1]);
                match self.index.resolve(link.page) {
                    LinkTarget::Resolved(rel_path) => {
                        format!("[{}]({link_prefix}/{})", link.display, rel_path.display())
                    }
                    LinkTarget::Ambiguous | LinkTarget::Missing => format!("[{}]()", link.display),
                }
            })
            .into_owned()
    }
}

struct WikiLink<'a> {
    page: &'a str,
    display: &'a str,
}

impl<'a> WikiLink<'a> {
    fn parse(inner: &'a str) -> Self {
        let (page, display) = inner.split_once('|').unwrap_or((inner, inner));
        Self { page, display }
    }
}

pub fn rewrite_wikilinks(text: &str, wiki_dir: &Path, link_prefix: &str) -> String {
    let renderer = match WikiLinkRenderer::from_wiki_dir(wiki_dir) {
        Ok(renderer) => renderer,
        Err(e) => {
            tracing::warn!("failed to build page map: {e}");
            return text.to_string();
        }
    };
    renderer.render_markdown_links(text, link_prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_wiki(dir: &Path) -> PathBuf {
        let wiki = dir.join("wiki");
        fs::create_dir_all(wiki.join("concepts")).unwrap();
        fs::create_dir_all(wiki.join("topics")).unwrap();
        fs::write(wiki.join("concepts/RLHF.md"), "# RLHF").unwrap();
        fs::write(wiki.join("concepts/DPO.md"), "# DPO").unwrap();
        fs::write(wiki.join("topics/alignment.md"), "# Alignment").unwrap();
        wiki
    }

    #[test]
    fn rewrites_known_wikilinks() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "See [[RLHF]] and [[DPO]] for details.";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(
            result,
            "See [RLHF](wiki/concepts/RLHF.md) and [DPO](wiki/concepts/DPO.md) for details."
        );
    }

    #[test]
    fn rewrites_unknown_wikilinks_to_empty_href() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "See [[NonExistent]] for more.";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(result, "See [NonExistent]() for more.");
    }

    #[test]
    fn preserves_text_without_wikilinks() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "No links here.";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(result, "No links here.");
    }

    #[test]
    fn rewrites_nested_directory_links() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "Read [[alignment]] for background.";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(
            result,
            "Read [alignment](wiki/topics/alignment.md) for background."
        );
    }

    #[test]
    fn rewrites_pipe_wikilink_with_display_text() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "See [[RLHF|reinforcement learning from human feedback]].";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(
            result,
            "See [reinforcement learning from human feedback](wiki/concepts/RLHF.md)."
        );
    }

    #[test]
    fn pipe_wikilink_unknown_page_produces_dead_link() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "See [[missing|some description]].";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
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
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(result, "See [overview]().");
    }

    #[test]
    fn case_insensitive_matching() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "See [[rlhf]] and [[dpo]] and [[Alignment]].";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(
            result,
            "See [rlhf](wiki/concepts/RLHF.md) and [dpo](wiki/concepts/DPO.md) and [Alignment](wiki/topics/alignment.md)."
        );
    }

    #[test]
    fn handles_missing_wiki_dir_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        let input = "See [[RLHF]].";
        let result = rewrite_wikilinks(input, &wiki, "wiki");
        assert_eq!(result, "See [[RLHF]].");
    }

    #[test]
    fn custom_link_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());

        let input = "See [[RLHF]] and [[alignment]].";
        let result = rewrite_wikilinks(input, &wiki, "/home/user/notes");
        assert_eq!(
            result,
            "See [RLHF](/home/user/notes/concepts/RLHF.md) and [alignment](/home/user/notes/topics/alignment.md)."
        );
    }

    #[test]
    fn renderer_reuses_page_map_across_answers() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = setup_wiki(dir.path());
        let renderer = WikiLinkRenderer::from_wiki_dir(&wiki).unwrap();

        assert_eq!(
            renderer.render_markdown_links("[[RLHF]]", "wiki"),
            "[RLHF](wiki/concepts/RLHF.md)"
        );
        assert_eq!(
            renderer.render_markdown_links("[[DPO]]", "wiki"),
            "[DPO](wiki/concepts/DPO.md)"
        );
    }
}
