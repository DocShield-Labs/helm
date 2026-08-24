use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};

use helm_proto::{PathCompletion, PathEntryKind};

#[derive(Debug, Eq, PartialEq)]
struct RankedCompletion {
    exact_case: bool,
    hidden: bool,
    folded: String,
    completion: PathCompletion,
}

impl Ord for RankedCompletion {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .exact_case
            .cmp(&self.exact_case)
            .then_with(|| self.hidden.cmp(&other.hidden))
            .then_with(|| match (self.completion.kind, other.completion.kind) {
                (PathEntryKind::Directory, PathEntryKind::File) => Ordering::Less,
                (PathEntryKind::File, PathEntryKind::Directory) => Ordering::Greater,
                _ => Ordering::Equal,
            })
            .then_with(|| self.folded.cmp(&other.folded))
            .then_with(|| self.completion.value.cmp(&other.completion.value))
    }
}

impl PartialOrd for RankedCompletion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub fn complete_path(
    cwd: &Path,
    home: Option<&Path>,
    path: &str,
    directories_only: bool,
    max_results: u32,
) -> Result<(Vec<PathCompletion>, bool), String> {
    if path == "~" && home.is_some() {
        return Ok((vec![PathCompletion {
            value: "~/".into(),
            kind: PathEntryKind::Directory,
        }], false));
    }
    let (typed_parent, leaf) = path.rsplit_once('/').map_or(("", path), |(parent, leaf)| {
        let end = parent.len() + 1;
        (&path[..end], leaf)
    });
    if leaf == "." || leaf == ".." {
        return Ok((vec![PathCompletion {
            value: format!("{typed_parent}{leaf}/"),
            kind: PathEntryKind::Directory,
        }], false));
    }
    let directory = resolve_directory(cwd, home, typed_parent)?;
    let folded_leaf = leaf.to_lowercase();
    let prefer_hidden = leaf.starts_with('.');
    let limit = usize::try_from(max_results.clamp(1, 500)).unwrap_or(500);
    let mut matches = BinaryHeap::with_capacity(limit + 1);
    let mut matched = 0usize;

    let entries = std::fs::read_dir(&directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let folded = name.to_lowercase();
        if !folded.starts_with(&folded_leaf) {
            continue;
        }
        let is_directory = entry.file_type().map_or_else(
            |_| entry.metadata().map(|metadata| metadata.is_dir()).unwrap_or(false),
            |file_type| {
                if file_type.is_symlink() {
                    std::fs::metadata(entry.path())
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false)
                } else {
                    file_type.is_dir()
                }
            },
        );
        if directories_only && !is_directory {
            continue;
        }
        let kind = if is_directory { PathEntryKind::Directory } else { PathEntryKind::File };
        let suffix = if is_directory { "/" } else { "" };
        matched += 1;
        matches.push(RankedCompletion {
            exact_case: name.starts_with(leaf),
            hidden: !prefer_hidden && name.starts_with('.'),
            folded,
            completion: PathCompletion { value: format!("{typed_parent}{name}{suffix}"), kind },
        });
        if matches.len() > limit {
            matches.pop();
        }
    }

    let mut matches = matches.into_vec();
    matches.sort();
    Ok((matches.into_iter().map(|candidate| candidate.completion).collect(), matched > limit))
}

fn resolve_directory(cwd: &Path, home: Option<&Path>, typed_parent: &str) -> Result<PathBuf, String> {
    if typed_parent == "~/" || typed_parent.starts_with("~/") {
        let home = home.ok_or("home directory is unavailable")?;
        return Ok(home.join(typed_parent.strip_prefix("~/").unwrap_or(typed_parent)));
    }
    let parent = Path::new(typed_parent);
    if parent.is_absolute() {
        Ok(parent.to_owned())
    } else {
        Ok(cwd.join(parent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    fn fixture() -> PathBuf {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir()
            .join(format!("helm-completion-{}-{sequence}", std::process::id()));
        std::fs::create_dir_all(root.join("Code")).unwrap();
        std::fs::create_dir_all(root.join(".config")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        std::fs::write(root.join("notes file.txt"), "").unwrap();
        root
    }

    #[test]
    fn completes_case_insensitively_with_canonical_names() {
        let root = fixture();
        let (matches, truncated) = complete_path(&root, Some(&root), "co", false, 20).unwrap();
        assert!(!truncated);
        assert_eq!(matches, vec![PathCompletion {
            value: "Code/".into(),
            kind: PathEntryKind::Directory,
        }]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn includes_hidden_entries_and_filters_files_for_cd() {
        let root = fixture();
        let (all, _) = complete_path(&root, Some(&root), "", false, 20).unwrap();
        assert!(all.iter().any(|candidate| candidate.value == ".config/"));
        let (directories, _) = complete_path(&root, Some(&root), "", true, 20).unwrap();
        assert_eq!(directories.iter().map(|candidate| candidate.value.as_str()).collect::<Vec<_>>(), vec!["Code/", ".config/"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preserves_typed_parent_and_reports_truncation() {
        let root = fixture();
        let (matches, truncated) = complete_path(&root, Some(&root), "~/", false, 1).unwrap();
        assert!(truncated);
        assert_eq!(matches[0].value, "~/Code/");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn follows_directory_symlinks_for_cd_completion() {
        use std::os::unix::fs::symlink;

        let root = fixture();
        symlink(root.join("Code"), root.join("linked-code")).unwrap();
        let (matches, _) = complete_path(&root, Some(&root), "linked", true, 20).unwrap();
        assert_eq!(matches, vec![PathCompletion {
            value: "linked-code/".into(),
            kind: PathEntryKind::Directory,
        }]);
        std::fs::remove_dir_all(root).unwrap();
    }
}
