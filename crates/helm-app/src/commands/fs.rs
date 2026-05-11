//! Filesystem queries that span hosts. Used by the schedule editor's
//! path picker so the user can navigate directories on a target host
//! (local *or* remote) without typing a path blind.
//!
//! Local: stdlib `read_dir`. Remote: `ls -1Ap` over the existing
//! `SshSession` — no extra connection needed since the host is already
//! attached for tmux.

use helm_domain::HostId;
use helm_tmux::quote_arg;
use serde::Serialize;
use specta::Type;
use std::path::{Path, PathBuf};
use tauri::State;

use crate::state::AppState;

/// One entry in a directory listing — names plus the cheap
/// flags we need to filter / decorate the picker.
#[derive(Debug, Clone, Serialize, Type)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
}

/// Result of `fs_list_dir`. `path` is the canonicalized absolute path
/// (tilde expanded, symlinks resolved, no trailing slash). `parent` is
/// the path's parent directory, or None at the filesystem root.
#[derive(Debug, Clone, Serialize, Type)]
pub struct DirListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<DirEntry>,
}

/// List directory entries on a host. Empty / null `path` resolves to
/// the user's HOME (local: `dirs::home_dir`; remote: `$HOME`).
///
/// Sorting: directories first, then files, alphabetical within each
/// group. Hidden entries (leading `.`) are included — the picker can
/// filter them client-side based on a toggle.
///
/// Errors come back as a Result::Err so the picker can render a tiny
/// inline message ("permission denied", "not a directory") and keep
/// the user on the previous breadcrumb.
#[tauri::command]
#[specta::specta]
pub async fn fs_list_dir(
    state: State<'_, AppState>,
    host_id: HostId,
    path: Option<String>,
) -> Result<DirListing, String> {
    let entry = state
        .entry(host_id)
        .ok_or_else(|| "unknown host".to_string())?;
    let (host_port, ssh) = {
        let g = entry.lock().await;
        (g.host.port, g.ssh.clone())
    };

    if host_port == 0 {
        list_local(path.as_deref())
    } else {
        let ssh = ssh.ok_or_else(|| "host not connected".to_string())?;
        let path_arg = path.unwrap_or_default();
        tokio::task::spawn_blocking(move || list_remote(&ssh, &path_arg))
            .await
            .map_err(|e| format!("ssh task: {e}"))?
    }
}

fn list_local(path: Option<&str>) -> Result<DirListing, String> {
    let target = match path {
        None | Some("") => dirs::home_dir().ok_or_else(|| "no $HOME".to_string())?,
        Some(p) => expand_tilde_local(p),
    };
    // Canonicalize so the breadcrumb shows the resolved path even when
    // the user clicked through a symlink. Failure (path missing,
    // permission) propagates as the typed error message.
    let canonical = std::fs::canonicalize(&target)
        .map_err(|e| format!("{}: {e}", target.display()))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    let read = std::fs::read_dir(&canonical).map_err(|e| format!("read_dir: {e}"))?;
    let mut entries: Vec<DirEntry> = Vec::new();
    for r in read {
        let de = match r {
            Ok(de) => de,
            Err(_) => continue,
        };
        let name = de.file_name().to_string_lossy().into_owned();
        let ft = de.file_type().ok();
        let is_symlink = ft.as_ref().map(|t| t.is_symlink()).unwrap_or(false);
        // For symlinks, follow once via metadata so the picker still
        // shows them as directories when they point at one.
        let is_dir = if is_symlink {
            std::fs::metadata(de.path()).map(|m| m.is_dir()).unwrap_or(false)
        } else {
            ft.as_ref().map(|t| t.is_dir()).unwrap_or(false)
        };
        entries.push(DirEntry {
            name,
            is_dir,
            is_symlink,
        });
    }
    sort_entries(&mut entries);
    let parent = canonical
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty());
    Ok(DirListing {
        path: canonical.to_string_lossy().into_owned(),
        parent,
        entries,
    })
}

fn list_remote(ssh: &helm_ssh::SshSession, path: &str) -> Result<DirListing, String> {
    // Resolve to absolute + canonical form remotely. `cd` handles
    // tilde, env, relative paths; `pwd -P` strips symlinks. On failure
    // the shell exits non-zero and we surface stderr.
    //
    // Quote with a heredoc-safe pattern: single quote around the
    // user-supplied bytes, escaping any embedded single quote.
    let quoted = quote_arg(path);
    let resolve_cmd = if path.is_empty() {
        "cd \"$HOME\" && pwd -P".to_string()
    } else {
        format!("cd {quoted} && pwd -P")
    };
    let resolved = ssh
        .run_oneshot(resolve_cmd)
        .map_err(|e| e.to_string())?;
    if !matches!(resolved.exit_code, Some(0) | None) {
        return Err(resolved.stderr.trim().to_string());
    }
    let canonical = resolved.stdout.trim().to_string();
    if canonical.is_empty() {
        return Err("could not resolve path".into());
    }

    // List entries. `ls -1Ap` prints one per line, includes dotfiles
    // except `.` and `..`, and appends `/` to directories — saves us
    // a stat per file. Symlinks pointing at directories get the slash
    // too because `-p` follows for the type test.
    //
    // We don't get an explicit symlink flag from `ls -p`; for the
    // picker that's fine. Detecting symlinks separately would need
    // `-l` parsing, which is brittle across `ls` variants.
    let list_cmd = format!("ls -1Ap -- {} 2>&1", quote_arg(&canonical));
    let listed = ssh.run_oneshot(list_cmd).map_err(|e| e.to_string())?;
    if !matches!(listed.exit_code, Some(0) | None) {
        return Err(listed.stdout.trim().to_string());
    }
    let mut entries: Vec<DirEntry> = listed
        .stdout
        .lines()
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
            let is_dir = line.ends_with('/');
            let name = if is_dir {
                line.trim_end_matches('/').to_string()
            } else {
                line.to_string()
            };
            Some(DirEntry {
                name,
                is_dir,
                is_symlink: false,
            })
        })
        .collect();
    sort_entries(&mut entries);

    let parent = parent_path(&canonical);
    Ok(DirListing {
        path: canonical,
        parent,
        entries,
    })
}

/// Compute the parent directory string from an absolute path, returning
/// None at root. Works the same on local and remote paths since both
/// pass through here as canonical absolute strings.
fn parent_path(s: &str) -> Option<String> {
    if s == "/" {
        return None;
    }
    let p = Path::new(s);
    p.parent().map(|x| {
        let parent_str = x.to_string_lossy().into_owned();
        if parent_str.is_empty() {
            "/".to_string()
        } else {
            parent_str
        }
    })
}

fn sort_entries(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
}

fn expand_tilde_local(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_root_is_none() {
        assert_eq!(parent_path("/"), None);
    }

    #[test]
    fn parent_of_top_level_is_root() {
        assert_eq!(parent_path("/Users"), Some("/".to_string()));
    }

    #[test]
    fn parent_of_nested() {
        assert_eq!(parent_path("/Users/azhar/Code"), Some("/Users/azhar".to_string()));
    }

    #[test]
    fn sort_dirs_before_files_alphabetical() {
        let mut e = vec![
            DirEntry { name: "zfile".into(), is_dir: false, is_symlink: false },
            DirEntry { name: "Bdir".into(), is_dir: true, is_symlink: false },
            DirEntry { name: "afile".into(), is_dir: false, is_symlink: false },
            DirEntry { name: "Cdir".into(), is_dir: true, is_symlink: false },
        ];
        sort_entries(&mut e);
        assert_eq!(e.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
                   vec!["Bdir", "Cdir", "afile", "zfile"]);
    }
}
