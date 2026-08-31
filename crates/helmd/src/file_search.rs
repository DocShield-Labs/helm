//! Fuzzy recursive file search under a session's cwd — the `@file`
//! menu's second gear. Segment-by-segment prefix completion
//! (`completion.rs`) answers an empty query; once the user types, this
//! walks the whole tree (`.gitignore` respected, hidden files included,
//! `.git` itself excluded) and ranks every path against the query.
//!
//! Ranking, best first — ties broken by fewer path segments, then
//! shorter path, then lexicographic:
//!   0. basename starts with the query
//!   1. basename contains it
//!   2. the relative path contains it
//!   3. the relative path contains it as a subsequence (`smain` →
//!      `src/main.rs`)
//!   4. one typed character forgiven: some single deletion of the query
//!      is a subsequence (`flegstone` → `flagstone`)
//!
//! The walk is bounded: past `MAX_VISITED` entries the tree is bigger
//! than a completion menu should pretend to know, and the result is
//! marked truncated.

use std::path::Path;

use helm_proto::{PathCompletion, PathEntryKind};

/// Entries examined before the walk gives up (monorepo guard).
const MAX_VISITED: usize = 30_000;

pub fn file_search(root: &Path, query: &str, max_results: usize) -> (Vec<PathCompletion>, bool) {
    if query.is_empty() || max_results == 0 {
        return (Vec::new(), false);
    }
    let needle = query.to_lowercase();
    let needle_chars: Vec<char> = needle.chars().collect();

    // Worst-kept-on-top heap bounded at `max_results` — the same shape
    // completion.rs uses. O(n log k), and only entries that enter the
    // heap ever own a String.
    #[derive(PartialEq, Eq)]
    struct Hit {
        score: u8,
        segments: usize,
        value: String,
        kind_dir: bool,
    }
    impl Ord for Hit {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            self.score
                .cmp(&other.score)
                .then(self.segments.cmp(&other.segments))
                .then(self.value.len().cmp(&other.value.len()))
                .then(self.value.cmp(&other.value))
        }
    }
    impl PartialOrd for Hit {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            Some(self.cmp(other))
        }
    }

    let mut heap: std::collections::BinaryHeap<Hit> = std::collections::BinaryHeap::new();
    let mut matched = 0usize;
    let mut visited = 0usize;
    let mut clipped = false;
    let mut base_buf = String::new();
    let mut hay_buf = String::new();

    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        // Honor .gitignore files even outside a git repository — the
        // default only applies them under a .git root.
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walk {
        let Ok(entry) = entry else { continue };
        visited += 1;
        if visited > MAX_VISITED {
            clipped = true;
            break;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        if rel.as_os_str().is_empty() {
            continue; // the root itself
        }
        base_buf.clear();
        base_buf.extend(entry.file_name().to_string_lossy().chars().flat_map(char::to_lowercase));
        let rel_str = rel.to_string_lossy();
        let score = if base_buf.starts_with(&needle) {
            0
        } else if base_buf.contains(&needle) {
            1
        } else {
            // The full-path haystack is only needed past the basename
            // tiers — most entries never get here.
            hay_buf.clear();
            hay_buf.extend(rel_str.chars().flat_map(char::to_lowercase));
            if hay_buf.contains(&needle) {
                2
            } else if is_subsequence(&needle, &hay_buf) {
                3
            } else if needle_chars.len() >= 4 && one_deletion_subsequence(&needle_chars, &hay_buf) {
                4
            } else {
                continue;
            }
        };
        matched += 1;
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let segments = rel_str.matches('/').count() + 1;
        // Cheap pre-check on the borrowed string before owning it.
        if heap.len() == max_results {
            let worst = heap.peek().expect("non-empty at cap");
            let better = (score, segments, rel_str.len() + usize::from(is_dir))
                < (worst.score, worst.segments, worst.value.len());
            if !better {
                continue;
            }
            heap.pop();
        }
        let mut value = rel_str.into_owned();
        if is_dir {
            value.push('/');
        }
        heap.push(Hit { score, segments, value, kind_dir: is_dir });
    }

    let truncated = clipped || matched > max_results;
    let mut hits = heap.into_sorted_vec();
    (
        hits.drain(..)
            .map(|h| PathCompletion {
                value: h.value,
                kind: if h.kind_dir { PathEntryKind::Directory } else { PathEntryKind::File },
            })
            .collect(),
        truncated,
    )
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut chars = hay.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// Does deleting exactly one character from `needle` yield a
/// subsequence of `hay`? Typo forgiveness: O(len²) over short queries,
/// only reached by entries every other tier rejected, and the needle's
/// chars are collected once per search, not per entry.
fn one_deletion_subsequence(needle: &[char], hay: &str) -> bool {
    (0..needle.len()).any(|skip| {
        let mut hay_chars = hay.chars();
        needle
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .all(|(_, n)| hay_chars.any(|h| h == *n))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("src/lib")).unwrap();
        std::fs::create_dir_all(r.join("target/debug")).unwrap();
        std::fs::create_dir_all(r.join(".github")).unwrap();
        std::fs::write(r.join("src/main.rs"), "").unwrap();
        std::fs::write(r.join("src/lib/mainframe.rs"), "").unwrap();
        std::fs::write(r.join("target/debug/main.rs"), "").unwrap();
        std::fs::write(r.join(".github/ci.yml"), "").unwrap();
        std::fs::write(r.join(".gitignore"), "/target\n").unwrap();
        dir
    }

    #[test]
    fn ranks_basename_prefix_first_and_respects_gitignore() {
        let dir = setup();
        let (hits, truncated) = file_search(dir.path(), "main", 10);
        assert!(!truncated);
        let values: Vec<&str> = hits.iter().map(|h| h.value.as_str()).collect();
        // basename-prefix matches lead; the gitignored target/ copy is absent.
        assert_eq!(values[0], "src/main.rs");
        assert!(values.contains(&"src/lib/mainframe.rs"));
        assert!(!values.iter().any(|v| v.starts_with("target/")));
    }

    #[test]
    fn subsequence_matches_and_hidden_files_are_searchable() {
        let dir = setup();
        let (hits, _) = file_search(dir.path(), "smain", 10);
        assert!(hits.iter().any(|h| h.value == "src/main.rs"));
        let (hits, _) = file_search(dir.path(), "ci.yml", 10);
        assert!(hits.iter().any(|h| h.value == ".github/ci.yml"));
    }

    #[test]
    fn directories_keep_a_trailing_slash() {
        let dir = setup();
        let (hits, _) = file_search(dir.path(), "lib", 10);
        let lib = hits.iter().find(|h| h.value == "src/lib/").unwrap();
        assert_eq!(lib.kind, PathEntryKind::Directory);
    }

    #[test]
    fn one_typo_is_forgiven() {
        let dir = setup();
        std::fs::write(dir.path().join("flagstone.rs"), "").unwrap();
        let (hits, _) = file_search(dir.path(), "flegstone", 10);
        assert!(hits.iter().any(|h| h.value == "flagstone.rs"));
        // Short queries stay strict — one wrong char of three is noise.
        let (hits, _) = file_search(dir.path(), "xzq", 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn empty_query_returns_nothing() {
        let dir = setup();
        assert_eq!(file_search(dir.path(), "", 10).0.len(), 0);
    }
}
