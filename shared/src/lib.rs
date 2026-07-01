use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_with::{hex::Hex, serde_as};
use sha2::{Digest, Sha256};

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

const LOCAL_MIRROR_GITIGNORE_PATH: &str = ".gitignore";
const LOCAL_MIRROR_GITIGNORE_CONTENT: &str = "*\n";

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

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncRequest {
    pub files: Vec<FileEntry>,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    #[serde_as(as = "Hex")]
    pub checksum: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResponse {
    pub upserts: Vec<FileContent>,
    pub deletes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
}

pub struct SyncPlan {
    response: SyncResponse,
}

impl SyncPlan {
    pub fn new(response: SyncResponse) -> Self {
        Self { response }
    }

    pub fn response(&self) -> &SyncResponse {
        &self.response
    }

    pub fn into_response(self) -> SyncResponse {
        self.response
    }

    pub fn summary(&self) -> SyncSummary {
        let upserted: HashSet<&str> = self
            .response
            .upserts
            .iter()
            .map(|f| f.path.as_str())
            .collect();
        SyncSummary {
            updated: self
                .response
                .upserts
                .iter()
                .filter(|f| !is_local_mirror_control_path(&f.path))
                .count(),
            deleted: self
                .response
                .deletes
                .iter()
                .filter(|p| {
                    !upserted.contains(p.as_str()) && !is_local_mirror_control_path(p.as_str())
                })
                .count(),
        }
    }

    pub fn apply(&self, wiki_path: &Path) -> Result<(), WikiSyncError> {
        apply_sync_plan(wiki_path, self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSummary {
    pub updated: usize,
    pub deleted: usize,
}

impl SyncSummary {
    pub fn total(self) -> usize {
        self.updated + self.deleted
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiFile {
    pub path: String,
    pub absolute_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WikiSyncError {
    #[error("failed to read '{path}'")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("server sent path with '..': '{0}'")]
    ParentDir(String),
    #[error("server sent absolute path: '{0}'")]
    AbsolutePath(String),
    #[error("resolved path escapes wiki directory: '{0}'")]
    EscapedPath(String),
    #[error("refusing to sync into existing non-wikidesk directory '{}'", path.display())]
    UnsafeLocalMirrorPath { path: PathBuf },
}

impl WikiSyncError {
    fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Recursively walks `dir` and returns relative and absolute paths for all files.
/// Returns an empty vec if `dir` does not exist.
pub fn walk_dir(dir: &Path) -> Result<Vec<WikiFile>, WikiSyncError> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    visit(dir, dir, &mut files)?;
    Ok(files)
}

pub fn walk_markdown_files(dir: &Path) -> Result<Vec<WikiFile>, WikiSyncError> {
    Ok(walk_dir(dir)?
        .into_iter()
        .filter(|file| file.absolute_path.extension().is_some_and(|e| e == "md"))
        .collect())
}

/// Recursively walks `dir` and returns a snapshot of all files with SHA-256 checksums.
/// Returns an empty vec if `dir` does not exist.
pub fn snapshot_dir(dir: &Path) -> Result<Vec<FileEntry>, WikiSyncError> {
    walk_dir(dir)?
        .into_iter()
        .map(|file| {
            let bytes = std::fs::read(&file.absolute_path)
                .map_err(|source| WikiSyncError::io(file.path.clone(), source))?;
            Ok(FileEntry {
                path: file.path,
                checksum: Sha256::digest(&bytes).into(),
            })
        })
        .collect()
}

pub fn snapshot_local_mirror(dir: &Path) -> Result<Vec<FileEntry>, WikiSyncError> {
    Ok(snapshot_dir(dir)?
        .into_iter()
        .filter(|file| !is_local_mirror_control_path(&file.path))
        .collect())
}

fn visit(base: &Path, dir: &Path, files: &mut Vec<WikiFile>) -> Result<(), WikiSyncError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|source| WikiSyncError::io(dir.display().to_string(), source))?;
    for entry in entries {
        let entry = entry.map_err(|source| WikiSyncError::io(dir.display().to_string(), source))?;
        let path = entry.path();
        let is_dir = entry
            .file_type()
            .map_err(|source| WikiSyncError::io(path.display().to_string(), source))?
            .is_dir();
        if is_dir {
            visit(base, &path, files)?;
        } else {
            let rel = path
                .strip_prefix(base)
                .expect("visited path must be beneath wiki base")
                .to_string_lossy()
                .into_owned();
            files.push(WikiFile {
                path: rel,
                absolute_path: path,
            });
        }
    }
    Ok(())
}

/// Computes the server-to-client wiki sync delta for a client snapshot.
pub fn compute_sync(
    server_wiki_dir: &Path,
    client_files: &[FileEntry],
) -> Result<SyncResponse, WikiSyncError> {
    let server_files = snapshot_dir(server_wiki_dir)?;

    let client_map: HashMap<&str, &[u8; 32]> = client_files
        .iter()
        .map(|f| (f.path.as_str(), &f.checksum))
        .collect();

    let mut upserts = Vec::new();
    for entry in &server_files {
        let unchanged = client_map
            .get(entry.path.as_str())
            .is_some_and(|c| **c == entry.checksum);
        if !unchanged {
            let content = std::fs::read_to_string(server_wiki_dir.join(&entry.path))
                .map_err(|source| WikiSyncError::io(entry.path.clone(), source))?;
            upserts.push(FileContent {
                path: entry.path.clone(),
                content,
            });
        }
    }

    let server_paths: HashSet<&str> = server_files.iter().map(|f| f.path.as_str()).collect();
    let deletes = client_files
        .iter()
        .filter(|f| !server_paths.contains(f.path.as_str()))
        .map(|f| f.path.clone())
        .collect();

    Ok(SyncPlan::new(SyncResponse { upserts, deletes }).into_response())
}

/// Applies a server-to-client wiki sync delta to the local wiki directory.
pub fn apply_sync(wiki_path: &Path, sync: &SyncResponse) -> Result<(), WikiSyncError> {
    SyncPlan::new(sync.clone()).apply(wiki_path)
}

fn apply_sync_plan(wiki_path: &Path, plan: &SyncPlan) -> Result<(), WikiSyncError> {
    let sync = plan.response();
    ensure_local_mirror_safe(wiki_path)?;
    std::fs::create_dir_all(wiki_path)
        .map_err(|source| WikiSyncError::io(wiki_path.display().to_string(), source))?;
    let wiki_canonical = wiki_path
        .canonicalize()
        .map_err(|source| WikiSyncError::io(wiki_path.display().to_string(), source))?;
    for file in &sync.upserts {
        if is_local_mirror_control_path(&file.path) {
            continue;
        }
        let target = resolve_and_validate(&wiki_canonical, &file.path)?;
        std::fs::write(&target, &file.content)
            .map_err(|source| WikiSyncError::io(target.display().to_string(), source))?;
    }

    let upserted: HashSet<&str> = sync.upserts.iter().map(|f| f.path.as_str()).collect();
    for path in &sync.deletes {
        if upserted.contains(path.as_str()) || is_local_mirror_control_path(path) {
            continue;
        }
        validate_relative_path(path)?;
        let target = wiki_canonical.join(path);
        let canonical = match target.canonicalize() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => return Err(WikiSyncError::io(target.display().to_string(), source)),
        };
        if !canonical.starts_with(&wiki_canonical) {
            return Err(WikiSyncError::EscapedPath(path.clone()));
        }
        std::fs::remove_file(&target)
            .map_err(|source| WikiSyncError::io(target.display().to_string(), source))?;
    }

    write_local_mirror_gitignore(&wiki_canonical)?;
    Ok(())
}

fn is_local_mirror_control_path(path: &str) -> bool {
    path == LOCAL_MIRROR_GITIGNORE_PATH
}

pub fn ensure_local_mirror_safe(wiki_path: &Path) -> Result<(), WikiSyncError> {
    if !wiki_path.exists() {
        return Ok(());
    }
    let marker = wiki_path.join(LOCAL_MIRROR_GITIGNORE_PATH);
    if marker.exists()
        && std::fs::read_to_string(&marker)
            .is_ok_and(|content| content == LOCAL_MIRROR_GITIGNORE_CONTENT)
    {
        Ok(())
    } else {
        Err(WikiSyncError::UnsafeLocalMirrorPath {
            path: wiki_path.to_path_buf(),
        })
    }
}

fn write_local_mirror_gitignore(wiki_path: &Path) -> Result<(), WikiSyncError> {
    let target = wiki_path.join(LOCAL_MIRROR_GITIGNORE_PATH);
    std::fs::write(&target, LOCAL_MIRROR_GITIGNORE_CONTENT)
        .map_err(|source| WikiSyncError::io(target.display().to_string(), source))
}

fn validate_relative_path(relative: &str) -> Result<(), WikiSyncError> {
    for component in Path::new(relative).components() {
        match component {
            Component::ParentDir => return Err(WikiSyncError::ParentDir(relative.to_string())),
            Component::RootDir | Component::Prefix(_) => {
                return Err(WikiSyncError::AbsolutePath(relative.to_string()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn resolve_and_validate(wiki_canonical: &Path, relative: &str) -> Result<PathBuf, WikiSyncError> {
    validate_relative_path(relative)?;
    // Since relative is validated to have no ".." or absolute components,
    // joining it to the canonical base cannot escape.
    let target = wiki_canonical.join(relative);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|source| WikiSyncError::io(parent.display().to_string(), source))?;
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_wiki(dir: &std::path::Path) {
        let wiki = dir.join("wiki");
        fs::create_dir_all(wiki.join("concepts")).unwrap();
        fs::write(wiki.join("concepts/RLHF.md"), "# RLHF").unwrap();
        fs::write(wiki.join("topics.md"), "# Topics").unwrap();
    }

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

    #[test]
    fn walk_markdown_files_filters_to_markdown_paths() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(wiki.join("concepts")).unwrap();
        fs::write(wiki.join("concepts/RLHF.md"), "# RLHF").unwrap();
        fs::write(wiki.join("image.png"), "not markdown").unwrap();

        let paths: Vec<_> = walk_markdown_files(&wiki)
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect();

        assert_eq!(paths, vec!["concepts/RLHF.md"]);
    }

    #[test]
    fn snapshot_local_mirror_ignores_control_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join(".gitignore"), "*\n").unwrap();
        fs::write(wiki.join("notes.md"), "notes").unwrap();

        let paths: Vec<_> = snapshot_local_mirror(&wiki)
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect();

        assert_eq!(paths, vec!["notes.md"]);
    }

    #[test]
    fn sync_new_client_gets_all_files() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let resp = compute_sync(&dir.path().join("wiki"), &[]).unwrap();
        assert_eq!(resp.deletes.len(), 0);
        assert_eq!(resp.upserts.len(), 2);
    }

    #[test]
    fn sync_up_to_date_client_gets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let client_files = snapshot_dir(&dir.path().join("wiki")).unwrap();
        let resp = compute_sync(&dir.path().join("wiki"), &client_files).unwrap();
        assert!(resp.upserts.is_empty());
        assert!(resp.deletes.is_empty());
    }

    #[test]
    fn sync_detects_deleted_server_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let client_files = vec![FileEntry {
            path: "gone.md".into(),
            checksum: [0xab; 32],
        }];

        let resp = compute_sync(&dir.path().join("wiki"), &client_files).unwrap();
        assert!(resp.deletes.contains(&"gone.md".to_string()));
    }

    #[test]
    fn sync_detects_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        setup_wiki(dir.path());

        let client_files = vec![FileEntry {
            path: "topics.md".into(),
            checksum: [0; 32],
        }];

        let resp = compute_sync(&dir.path().join("wiki"), &client_files).unwrap();
        assert_eq!(resp.upserts.len(), 2); // topics.md (changed) + concepts/RLHF.md (new)
        assert!(resp.upserts.iter().any(|f| f.path == "topics.md"));
    }

    #[test]
    fn apply_sync_writes_upserts_and_removes_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join(".gitignore"), "*\n").unwrap();
        fs::write(wiki.join("old.md"), "old").unwrap();

        apply_sync(
            &wiki,
            &SyncResponse {
                upserts: vec![FileContent {
                    path: "concepts/RLHF.md".into(),
                    content: "# RLHF".into(),
                }],
                deletes: vec!["old.md".into()],
            },
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(wiki.join("concepts/RLHF.md")).unwrap(),
            "# RLHF"
        );
        assert!(!wiki.join("old.md").exists());
        assert_eq!(fs::read_to_string(wiki.join(".gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn apply_sync_owns_local_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join(".gitignore"), "*\n").unwrap();

        let plan = SyncPlan::new(SyncResponse {
            upserts: vec![FileContent {
                path: ".gitignore".into(),
                content: "server\n".into(),
            }],
            deletes: vec![".gitignore".into()],
        });

        assert_eq!(
            plan.summary(),
            SyncSummary {
                updated: 0,
                deleted: 0,
            }
        );
        plan.apply(&wiki).unwrap();

        assert_eq!(fs::read_to_string(wiki.join(".gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn apply_sync_rejects_parent_dir_paths() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join(".gitignore"), "*\n").unwrap();
        let err = apply_sync(
            &wiki,
            &SyncResponse {
                upserts: vec![FileContent {
                    path: "../escape.md".into(),
                    content: "nope".into(),
                }],
                deletes: vec![],
            },
        )
        .unwrap_err();

        assert!(matches!(err, WikiSyncError::ParentDir(_)));
    }

    #[test]
    fn apply_sync_refuses_existing_unmarked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();

        let err = apply_sync(
            &wiki,
            &SyncResponse {
                upserts: vec![],
                deletes: vec![],
            },
        )
        .unwrap_err();

        assert!(matches!(err, WikiSyncError::UnsafeLocalMirrorPath { .. }));
    }
}
