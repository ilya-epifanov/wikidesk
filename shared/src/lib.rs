pub mod sync;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ResearchRequest {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchResponse {
    pub answer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListWikisResponse {
    pub wikis: Vec<WikiInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiInfo {
    pub name: String,
    pub description: String,
}

pub const WIKI_LIST_PATH: &str = "/wiki";

pub fn is_valid_wiki_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    let Some(last) = name.chars().next_back() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && (last.is_ascii_lowercase() || last.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn derived_wiki_path(wiki: &str) -> String {
    if wiki == "default" {
        "wiki".to_string()
    } else {
        format!("wiki-{wiki}")
    }
}

pub fn wiki_base_path(wiki: &str) -> String {
    format!("{WIKI_LIST_PATH}/{wiki}")
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LocalPathError {
    #[error("local_path must not be empty")]
    Empty,
    #[error("local_path must be relative: '{0}'")]
    Absolute(String),
    #[error("local_path must use '/' separators: '{0}'")]
    Backslash(String),
    #[error("local_path must not contain ':': '{0}'")]
    Colon(String),
    #[error("local_path must not contain empty components: '{0}'")]
    EmptyComponent(String),
    #[error("local_path must not contain '.' or '..' components: '{0}'")]
    DotComponent(String),
}

pub fn validate_local_path(path: &str) -> Result<(), LocalPathError> {
    if path.is_empty() {
        return Err(LocalPathError::Empty);
    }
    if path.starts_with('/') {
        return Err(LocalPathError::Absolute(path.to_string()));
    }
    if path.contains('\\') {
        return Err(LocalPathError::Backslash(path.to_string()));
    }
    if path.contains(':') {
        return Err(LocalPathError::Colon(path.to_string()));
    }
    for component in path.split('/') {
        if component.is_empty() {
            return Err(LocalPathError::EmptyComponent(path.to_string()));
        }
        if component == "." || component == ".." {
            return Err(LocalPathError::DotComponent(path.to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_names_are_url_and_dir_safe_slugs() {
        for valid in ["rlhf", "rust-notes", "a1", "1a", "default"] {
            assert!(is_valid_wiki_name(valid), "{valid}");
        }
        for invalid in [
            "",
            "Wiki",
            "wiki_name",
            "-wiki",
            "wiki-",
            "../wiki",
            "wiki/name",
        ] {
            assert!(!is_valid_wiki_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn derived_wiki_paths_follow_wiki_names() {
        assert_eq!(derived_wiki_path("default"), "wiki");
        assert_eq!(derived_wiki_path("rlhf"), "wiki-rlhf");
        assert_eq!(wiki_base_path("rlhf"), "/wiki/rlhf");
    }

    #[test]
    fn validates_portable_relative_local_paths() {
        for valid in ["wiki", "wiki-ml", "mirrors/ml"] {
            assert!(validate_local_path(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "/wiki",
            "wiki/",
            "mirrors//ml",
            "./wiki",
            "../wiki",
            "a\\b",
            "C:/wiki",
        ] {
            assert!(validate_local_path(invalid).is_err(), "{invalid}");
        }
    }
}
