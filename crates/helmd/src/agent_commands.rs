//! Slash commands an agent (Claude Code) accepts, enumerated on the
//! host that runs the agent — sessions are often remote, so the app
//! can't read these directories itself.
//!
//! Sources, later ones shadowing earlier ones by name:
//!   - a small hardcoded list of agent built-ins (they live in the CLI,
//!     not on disk — the one part that needs occasional upkeep);
//!   - `~/.claude/commands/*.md` and `~/.claude/skills/*/SKILL.md`;
//!   - `{project}/.claude/commands/*.md` and `{project}/.claude/skills/*/SKILL.md`,
//!     where `{project}` is the session's git toplevel, else its cwd.
//!
//! Descriptions come from YAML frontmatter's `description:` line — a
//! full YAML parser buys nothing here, every real file uses one line.

use std::path::{Path, PathBuf};

use helm_proto::AgentCommand;

/// Claude Code built-ins that aren't discoverable on disk. Names only
/// need to be right; descriptions are cosmetic.
const BUILTINS: &[(&str, &str)] = &[
    ("clear", "Clear conversation history"),
    ("compact", "Compact the conversation"),
    ("config", "Open settings"),
    ("context", "Show context usage"),
    ("cost", "Show session cost"),
    ("doctor", "Check installation health"),
    ("exit", "Exit the session"),
    ("help", "Show help"),
    ("init", "Generate CLAUDE.md"),
    ("mcp", "Manage MCP servers"),
    ("memory", "Edit memory files"),
    ("model", "Switch model"),
    ("permissions", "Manage permissions"),
    ("resume", "Resume a conversation"),
    ("review", "Review a pull request"),
    ("rewind", "Rewind the conversation"),
    ("status", "Show session status"),
    ("todos", "List current todos"),
];

/// Cap on enumerated commands; past this the directory is misconfigured
/// and the menu would be noise anyway.
const MAX_COMMANDS: usize = 400;

/// Enumerate commands for a session rooted at `project_dir` (git
/// toplevel, else cwd; `None` when the session has no cwd yet).
pub fn list(project_dir: Option<&Path>) -> Vec<AgentCommand> {
    let mut out: Vec<AgentCommand> = BUILTINS
        .iter()
        .map(|(name, description)| AgentCommand {
            name: (*name).to_string(),
            description: (*description).to_string(),
        })
        .collect();

    if let Some(home) = dirs::home_dir() {
        scan_claude_dir(&home.join(".claude"), &mut out);
    }
    if let Some(dir) = project_dir {
        scan_claude_dir(&dir.join(".claude"), &mut out);
    }

    // Later sources shadow earlier ones (project > user > builtin),
    // matching how the agent itself resolves a name.
    let mut seen = std::collections::HashSet::new();
    let mut deduped: Vec<AgentCommand> = Vec::with_capacity(out.len());
    for cmd in out.into_iter().rev() {
        if seen.insert(cmd.name.clone()) {
            deduped.push(cmd);
        }
    }
    deduped.reverse();
    deduped.sort_by(|a, b| a.name.cmp(&b.name));
    deduped.truncate(MAX_COMMANDS);
    deduped
}

fn scan_claude_dir(claude: &Path, out: &mut Vec<AgentCommand>) {
    // commands/*.md — name is the file stem.
    if let Ok(entries) = std::fs::read_dir(claude.join("commands")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(AgentCommand {
                    name: name.to_string(),
                    description: frontmatter_description(&path),
                });
            }
        }
    }
    // skills/<name>/SKILL.md — name is the directory.
    if let Ok(entries) = std::fs::read_dir(claude.join("skills")) {
        for entry in entries.flatten() {
            let dir = entry.path();
            let skill = dir.join("SKILL.md");
            if !skill.is_file() {
                continue;
            }
            if let Some(name) = dir.file_name().and_then(|s| s.to_str()) {
                out.push(AgentCommand {
                    name: name.to_string(),
                    description: frontmatter_description(&skill),
                });
            }
        }
    }
}

/// `description:` from the leading YAML frontmatter, first line only,
/// trimmed to something a tooltip can show. Empty when absent. Reads
/// at most the frontmatter's opening lines — SKILL.md bodies run tens
/// of KB and none of it is wanted here.
fn frontmatter_description(path: &PathBuf) -> String {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut lines = std::io::BufReader::new(file).lines().take(64).map_while(Result::ok);
    if lines.next().as_deref().map(str::trim) != Some("---") {
        return String::new();
    }
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("description:") {
            let d = rest.trim().trim_matches('"').trim_matches('\'');
            let mut out: String = d.chars().take(160).collect();
            if d.chars().count() > 160 {
                out.push('…');
            }
            return out;
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_commands_and_skills_enumerate_with_descriptions() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(claude.join("commands")).unwrap();
        std::fs::create_dir_all(claude.join("skills/deploy")).unwrap();
        std::fs::write(
            claude.join("commands/ship.md"),
            "---\ndescription: Ship it\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            claude.join("skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: \"Deploy the app\"\n---\n",
        )
        .unwrap();

        let cmds = list(Some(dir.path()));
        let ship = cmds.iter().find(|c| c.name == "ship").unwrap();
        assert_eq!(ship.description, "Ship it");
        let deploy = cmds.iter().find(|c| c.name == "deploy").unwrap();
        assert_eq!(deploy.description, "Deploy the app");
        // Built-ins are present alongside.
        assert!(cmds.iter().any(|c| c.name == "help"));
    }

    #[test]
    fn project_shadows_builtin_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(claude.join("commands")).unwrap();
        std::fs::write(claude.join("commands/review.md"), "---\ndescription: Ours\n---\n").unwrap();
        let cmds = list(Some(dir.path()));
        let reviews: Vec<_> = cmds.iter().filter(|c| c.name == "review").collect();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].description, "Ours");
    }

    #[test]
    fn no_frontmatter_means_empty_description() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join(".claude");
        std::fs::create_dir_all(claude.join("commands")).unwrap();
        std::fs::write(claude.join("commands/bare.md"), "just a prompt body").unwrap();
        let cmds = list(Some(dir.path()));
        assert_eq!(cmds.iter().find(|c| c.name == "bare").unwrap().description, "");
    }
}
