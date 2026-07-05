use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_with::{hex::Hex, serde_as};
use sha2::{Digest, Sha256};

const LOCAL_MIRROR_GITIGNORE_PATH: &str = ".gitignore";
const LOCAL_MIRROR_GITIGNORE_CONTENT: &str = "*\n";

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

impl SyncResponse {
    pub fn summary(&self) -> SyncSummary {
        let upserted: HashSet<&str> = self.upserts.iter().map(|f| f.path.as_str()).collect();
        SyncSummary {
            updated: self
                .upserts
                .iter()
                .filter(|f| !is_local_mirror_control_path(&f.path))
                .count(),
            deleted: self
                .deletes
                .iter()
                .filter(|p| {
                    !upserted.contains(p.as_str()) && !is_local_mirror_control_path(p.as_str())
                })
                .count(),
        }
    }

    pub fn apply(&self, wiki_path: &Path) -> Result<(), WikiSyncError> {
        apply_sync_response(wiki_path, self)
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

pub fn walk_markdown_files(dir: &Path) -> Result<Vec<WikiFile>, WikiSyncError> {
    Ok(walk_dir(dir)?
        .into_iter()
        .filter(|file| file.absolute_path.extension().is_some_and(|e| e == "md"))
        .collect())
}

pub fn snapshot_local_mirror(dir: &Path) -> Result<Vec<FileEntry>, WikiSyncError> {
    Ok(snapshot_dir(dir)?
        .into_iter()
        .filter(|file| !is_local_mirror_control_path(&file.path))
        .collect())
}

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

    Ok(SyncResponse { upserts, deletes })
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

fn walk_dir(dir: &Path) -> Result<Vec<WikiFile>, WikiSyncError> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    visit(dir, dir, &mut files)?;
    Ok(files)
}

fn snapshot_dir(dir: &Path) -> Result<Vec<FileEntry>, WikiSyncError> {
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

fn apply_sync_response(wiki_path: &Path, sync: &SyncResponse) -> Result<(), WikiSyncError> {
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
        assert_eq!(resp.upserts.len(), 2);
        assert!(resp.upserts.iter().any(|f| f.path == "topics.md"));
    }

    #[test]
    fn apply_sync_writes_upserts_and_removes_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join(".gitignore"), "*\n").unwrap();
        fs::write(wiki.join("old.md"), "old").unwrap();

        SyncResponse {
            upserts: vec![FileContent {
                path: "concepts/RLHF.md".into(),
                content: "# RLHF".into(),
            }],
            deletes: vec!["old.md".into()],
        }
        .apply(&wiki)
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

        let sync = SyncResponse {
            upserts: vec![FileContent {
                path: ".gitignore".into(),
                content: "server\n".into(),
            }],
            deletes: vec![".gitignore".into()],
        };

        assert_eq!(
            sync.summary(),
            SyncSummary {
                updated: 0,
                deleted: 0,
            }
        );
        sync.apply(&wiki).unwrap();

        assert_eq!(fs::read_to_string(wiki.join(".gitignore")).unwrap(), "*\n");
    }

    #[test]
    fn apply_sync_rejects_parent_dir_paths() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();
        fs::write(wiki.join(".gitignore"), "*\n").unwrap();
        let err = SyncResponse {
            upserts: vec![FileContent {
                path: "../escape.md".into(),
                content: "nope".into(),
            }],
            deletes: vec![],
        }
        .apply(&wiki)
        .unwrap_err();

        assert!(matches!(err, WikiSyncError::ParentDir(_)));
    }

    #[test]
    fn apply_sync_refuses_existing_unmarked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        fs::create_dir_all(&wiki).unwrap();

        let err = SyncResponse {
            upserts: vec![],
            deletes: vec![],
        }
        .apply(&wiki)
        .unwrap_err();

        assert!(matches!(err, WikiSyncError::UnsafeLocalMirrorPath { .. }));
    }
}
