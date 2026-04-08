use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_with::{hex::Hex, serde_as};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
pub struct ResearchRequest {
    pub question: String,
    pub wiki_path: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResearchResponse {
    pub answer: String,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncResponse {
    pub upserts: Vec<FileContent>,
    pub deletes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub content: String,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WalkError {
    #[error("failed to read '{path}'")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Recursively walks `dir` and returns a snapshot of all files with SHA-256 checksums.
/// Returns an empty vec if `dir` does not exist.
pub fn snapshot_dir(dir: &Path) -> Result<Vec<FileEntry>, WalkError> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    visit(dir, dir, &mut files)?;
    Ok(files)
}

fn visit(base: &Path, dir: &Path, files: &mut Vec<FileEntry>) -> Result<(), WalkError> {
    let entries = std::fs::read_dir(dir).map_err(|source| WalkError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| WalkError::Io {
            path: dir.display().to_string(),
            source,
        })?;
        let path = entry.path();
        let is_dir = entry.file_type().map_err(|source| WalkError::Io {
            path: path.display().to_string(),
            source,
        })?.is_dir();
        if is_dir {
            visit(base, &path, files)?;
        } else {
            let rel = path.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).map_err(|source| WalkError::Io {
                path: rel.clone(),
                source,
            })?;
            files.push(FileEntry {
                path: rel,
                checksum: Sha256::digest(&bytes).into(),
            });
        }
    }
    Ok(())
}
