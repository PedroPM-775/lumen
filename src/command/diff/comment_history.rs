//! Local comment history for a diff.
//!
//! Comments are stored inside the repository (`.git/lumen/comments.json`, or
//! `.jj/lumen/comments.json` when there is no git dir). Each diff reference
//! keeps its own list. Nothing is sent to a forge.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use super::state::{Annotation, AnnotationTarget};
use super::types::DiffPanelFocus;

#[derive(Serialize, Deserialize, Default)]
struct HistoryFile {
    reviews: BTreeMap<String, Vec<StoredAnnotation>>,
}

#[derive(Serialize, Deserialize)]
struct StoredAnnotation {
    filename: String,
    target: StoredTarget,
    content: String,
    created_at_ms: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredTarget {
    File,
    LineRange {
        panel: String,
        start_line: usize,
        end_line: usize,
    },
}

/// Path of the history file for the repository that contains `cwd`.
pub fn discover(cwd: &Path) -> Option<PathBuf> {
    if let Ok(repo) = git2::Repository::discover(cwd) {
        return Some(repo.path().join("lumen").join("comments.json"));
    }
    let mut dir = cwd.to_path_buf();
    loop {
        let jj = dir.join(".jj");
        if jj.is_dir() {
            return Some(jj.join("lumen").join("comments.json"));
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn load(path: &Path, key: &str) -> Vec<Annotation> {
    let file = read_file(path).unwrap_or_default();
    file.reviews
        .get(key)
        .map(|items| {
            items
                .iter()
                .filter_map(StoredAnnotation::to_annotation)
                .enumerate()
                .map(|(index, mut annotation)| {
                    annotation.id = index as u64;
                    annotation
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Write `annotations` for `key`.
///
/// Comments whose file is not in `open_files` are left as they were, so a
/// filtered diff does not erase notes on files that are not on screen.
pub fn save(
    path: &Path,
    key: &str,
    annotations: &[Annotation],
    open_files: &HashSet<String>,
) -> io::Result<()> {
    let mut file = read_file(path).unwrap_or_default();
    let existing = file.reviews.remove(key).unwrap_or_default();
    let mut kept: Vec<StoredAnnotation> = existing
        .into_iter()
        .filter(|annotation| !open_files.contains(&annotation.filename))
        .collect();
    kept.extend(annotations.iter().map(StoredAnnotation::from_annotation));
    if kept.is_empty() {
        file.reviews.remove(key);
    } else {
        file.reviews.insert(key.to_string(), kept);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&file)
        .map_err(|err| io::Error::other(err.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_file(path: &Path) -> Option<HistoryFile> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

impl StoredAnnotation {
    fn from_annotation(annotation: &Annotation) -> Self {
        let created_at_ms = annotation
            .created_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let target = match &annotation.target {
            AnnotationTarget::File => StoredTarget::File,
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => StoredTarget::LineRange {
                panel: match panel {
                    DiffPanelFocus::Old => "old",
                    _ => "new",
                }
                .to_string(),
                start_line: *start_line,
                end_line: *end_line,
            },
        };
        Self {
            filename: annotation.filename.clone(),
            target,
            content: annotation.content.clone(),
            created_at_ms,
        }
    }

    fn to_annotation(&self) -> Option<Annotation> {
        let target = match &self.target {
            StoredTarget::File => AnnotationTarget::File,
            StoredTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => AnnotationTarget::LineRange {
                panel: if panel == "old" {
                    DiffPanelFocus::Old
                } else {
                    DiffPanelFocus::New
                },
                start_line: *start_line,
                end_line: *end_line,
            },
        };
        Some(Annotation {
            id: 0,
            filename: self.filename.clone(),
            target,
            content: self.content.clone(),
            created_at: UNIX_EPOCH + std::time::Duration::from_millis(self.created_at_ms),
        })
    }
}

/// Stable key for the diff currently on screen.
pub fn review_key(diff_reference: Option<&str>, stacked_commit_id: Option<&str>) -> String {
    let base = diff_reference.unwrap_or("working-tree");
    match stacked_commit_id {
        Some(commit_id) => format!("{base}@{commit_id}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn sample(filename: &str, content: &str) -> Annotation {
        Annotation {
            id: 1,
            filename: filename.to_string(),
            target: AnnotationTarget::LineRange {
                panel: DiffPanelFocus::New,
                start_line: 4,
                end_line: 4,
            },
            content: content.to_string(),
            created_at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        }
    }

    #[test]
    fn roundtrip_keeps_comments_for_the_same_diff_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("comments.json");
        let open = HashSet::from(["a.rb".to_string()]);

        save(&path, "working-tree", &[sample("a.rb", "check this")], &open).unwrap();
        save(
            &path,
            "HEAD~1",
            &[sample("a.rb", "other diff")],
            &open,
        )
        .unwrap();

        let loaded = load(&path, "working-tree");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "check this");
        assert_eq!(loaded[0].filename, "a.rb");
        assert!(matches!(
            loaded[0].target,
            AnnotationTarget::LineRange {
                panel: DiffPanelFocus::New,
                start_line: 4,
                end_line: 4,
            }
        ));
        assert_eq!(load(&path, "HEAD~1")[0].content, "other diff");
    }

    #[test]
    fn save_leaves_comments_on_files_that_are_not_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("comments.json");
        let both = HashSet::from(["a.rb".to_string(), "b.rb".to_string()]);
        save(
            &path,
            "working-tree",
            &[sample("a.rb", "a"), sample("b.rb", "b")],
            &both,
        )
        .unwrap();

        let only_a = HashSet::from(["a.rb".to_string()]);
        save(&path, "working-tree", &[], &only_a).unwrap();

        let loaded = load(&path, "working-tree");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].filename, "b.rb");
        assert_eq!(loaded[0].content, "b");
    }

    #[test]
    fn review_key_uses_working_tree_and_stacked_commit() {
        assert_eq!(review_key(None, None), "working-tree");
        assert_eq!(review_key(Some("main..feature"), None), "main..feature");
        assert_eq!(
            review_key(Some("main..feature"), Some("abc123")),
            "main..feature@abc123"
        );
    }
}
