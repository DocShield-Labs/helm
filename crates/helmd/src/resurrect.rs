//! Session resurrection across a daemon self-upgrade.
//!
//! `upgrade` snapshots every session — name, cwd, absolute line
//! numbering, scrollback rows, block table — kills the sessions, and
//! `exec()`s the new binary in place, handing it the listener fd and
//! the snapshot path. Because exec preserves open descriptors, the
//! socket never closes: there is no respawn race and no window where a
//! stale client could resurrect an old binary. The new daemon recreates
//! each session as a fresh login shell in the same cwd, seeds the old
//! scrollback (absolute line numbers continue, so blocks and jumps stay
//! valid), and appends a seam block: `— daemon restarted for X —`.
//!
//! The snapshot is transactional (tmp + rename); if it cannot be
//! written the upgrade aborts and the running daemon is untouched. The
//! failure mode of upgrading is not upgrading.

use std::path::{Path, PathBuf};

use helm_proto::{attrs, BlockMeta, Color, Row, Span, Style};
use serde::{Deserialize, Serialize};

/// Rows kept per session in a snapshot. Bounds the file, not the
/// daemon's live retention.
const SNAPSHOT_ROWS: usize = 20_000;

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    /// Version of the binary this snapshot was written FOR.
    pub target_version: String,
    pub sessions: Vec<SessionSnapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub name: String,
    pub cwd: Option<String>,
    /// Absolute line of `rows[0]`.
    pub history_start: u64,
    pub rows: Vec<Row>,
    pub blocks: Vec<BlockMeta>,
    pub next_block_id: u64,
}

/// Default snapshot location, next to the socket.
pub fn default_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".helm")
        .join("resurrect.json")
}

pub fn write(path: &Path, snapshot: &Snapshot) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec(snapshot).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read and CONSUME a snapshot: the file is removed on a successful
/// parse so a crash loop can't resurrect the same ghosts twice.
pub fn take(path: &Path) -> Option<Snapshot> {
    let data = std::fs::read(path).ok()?;
    let snapshot = serde_json::from_slice(&data).ok()?;
    let _ = std::fs::remove_file(path);
    Some(snapshot)
}

/// Trim a session's rows to the snapshot cap, keeping the newest and
/// advancing `history_start` to match.
pub fn trim(history_start: u64, mut rows: Vec<Row>) -> (u64, Vec<Row>) {
    if rows.len() > SNAPSHOT_ROWS {
        let drop = rows.len() - SNAPSHOT_ROWS;
        rows.drain(..drop);
        (history_start + drop as u64, rows)
    } else {
        (history_start, rows)
    }
}

/// The seam the resurrected session shows where the restart happened.
pub fn seam_row(target_version: &str) -> Row {
    Row {
        spans: vec![Span {
            text: format!("— daemon restarted for {target_version} —"),
            style: Style { fg: Color::Default, bg: Color::Default, attrs: attrs::DIM, link: None },
        }],
        wrapped: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Row {
        Row {
            spans: vec![Span {
                text: text.into(),
                style: Style { fg: Color::Default, bg: Color::Default, attrs: 0, link: None },
            }],
            wrapped: false,
        }
    }

    #[test]
    fn round_trips_and_consumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resurrect.json");
        let snap = Snapshot {
            target_version: "9.9.9".into(),
            sessions: vec![SessionSnapshot {
                name: "work".into(),
                cwd: Some("/tmp".into()),
                history_start: 40,
                rows: vec![row("a"), row("b")],
                blocks: vec![],
                next_block_id: 7,
            }],
        };
        write(&path, &snap).unwrap();
        let back = take(&path).unwrap();
        assert_eq!(back.sessions[0].name, "work");
        assert_eq!(back.sessions[0].history_start, 40);
        assert_eq!(back.sessions[0].rows.len(), 2);
        // Consumed: a second take finds nothing.
        assert!(take(&path).is_none());
    }

    #[test]
    fn trim_keeps_newest_and_moves_start() {
        let rows: Vec<Row> = (0..SNAPSHOT_ROWS + 10).map(|i| row(&i.to_string())).collect();
        let (start, kept) = trim(100, rows);
        assert_eq!(kept.len(), SNAPSHOT_ROWS);
        assert_eq!(start, 110);
        assert_eq!(kept[0].spans[0].text, "10");
    }
}
