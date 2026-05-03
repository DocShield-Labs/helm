//! Pure parser for tmux control mode (`tmux -CC`) lines.
//!
//! Protocol summary (from `tmux(1)` and iTerm2's reference impl):
//!
//! - `%begin <ts> <num> <flags>` … `%end <ts> <num> <flags>` brackets a
//!   command response. `%error` replaces `%end` for failures.
//! - `%notification arg1 arg2 …` are single-line state deltas.
//! - `%output %P data` is per-pane output. Backslash and bytes < 040 are
//!   octal-escaped (`\xxx`), so we decode before yielding the bytes.

pub use helm_domain::TmuxNotification as Notification;

/// A single parsed line. Streams of these are folded into events by the
/// state machine in `client.rs`.
#[derive(Debug, Clone, PartialEq)]
pub enum TmuxLine {
    Begin {
        timestamp: u64,
        number: u32,
        flags: u32,
    },
    End {
        timestamp: u64,
        number: u32,
        flags: u32,
    },
    /// `%error` — same shape as `%end` but signals the response was a failure.
    ResponseError {
        timestamp: u64,
        number: u32,
        flags: u32,
    },
    Notification(Notification),
    /// A line inside a `%begin`/`%end` block — the actual command response data.
    Data(String),
}

pub fn parse_line(line: &str) -> TmuxLine {
    let line = line.strip_suffix('\r').unwrap_or(line);
    if !line.starts_with('%') {
        return TmuxLine::Data(line.to_string());
    }

    // First word identifies the line; the rest is type-specific.
    let (head, rest) = match line.split_once(' ') {
        Some(pair) => pair,
        None => (line, ""),
    };

    match head {
        "%begin" => parse_block_marker(rest)
            .map(|(timestamp, number, flags)| TmuxLine::Begin {
                timestamp,
                number,
                flags,
            })
            .unwrap_or_else(|| unknown_line(head, rest)),
        "%end" => parse_block_marker(rest)
            .map(|(timestamp, number, flags)| TmuxLine::End {
                timestamp,
                number,
                flags,
            })
            .unwrap_or_else(|| unknown_line(head, rest)),
        "%error" => parse_block_marker(rest)
            .map(|(timestamp, number, flags)| TmuxLine::ResponseError {
                timestamp,
                number,
                flags,
            })
            .unwrap_or_else(|| unknown_line(head, rest)),

        "%output" => TmuxLine::Notification(parse_output(rest)),

        "%window-add" => single_arg(rest, |window_id| Notification::WindowAdded {
            window_id: window_id.to_string(),
        }),
        "%window-close" | "%unlinked-window-close" => {
            single_arg(rest, |window_id| Notification::WindowClosed {
                window_id: window_id.to_string(),
            })
        }
        "%window-renamed" | "%unlinked-window-renamed" => {
            two_args(rest, |window_id, name| Notification::WindowRenamed {
                window_id: window_id.to_string(),
                name: name.to_string(),
            })
        }

        "%session-changed" => two_args(rest, |session_id, name| Notification::SessionChanged {
            session_id: session_id.to_string(),
            name: name.to_string(),
        }),
        "%session-renamed" => two_args(rest, |session_id, name| Notification::SessionRenamed {
            session_id: session_id.to_string(),
            name: name.to_string(),
        }),
        "%sessions-changed" => TmuxLine::Notification(Notification::SessionsChanged),
        "%session-window-changed" => {
            two_args(rest, |session_id, window_id| {
                Notification::SessionWindowChanged {
                    session_id: session_id.to_string(),
                    window_id: window_id.to_string(),
                }
            })
        }

        "%layout-change" => two_args(rest, |window_id, layout| Notification::LayoutChanged {
            window_id: window_id.to_string(),
            layout: layout.to_string(),
        }),
        "%window-pane-changed" => {
            two_args(rest, |window_id, pane_id| Notification::WindowPaneChanged {
                window_id: window_id.to_string(),
                pane_id: pane_id.to_string(),
            })
        }

        "%pane-mode-changed" => single_arg(rest, |pane_id| Notification::PaneModeChanged {
            pane_id: pane_id.to_string(),
        }),
        "%continue" => single_arg(rest, |pane_id| Notification::Continue {
            pane_id: pane_id.to_string(),
        }),
        "%pause" => single_arg(rest, |pane_id| Notification::Pause {
            pane_id: pane_id.to_string(),
        }),
        "%client-detached" => single_arg(rest, |client| Notification::ClientDetached {
            client: client.to_string(),
        }),

        "%exit" => TmuxLine::Notification(Notification::Exit {
            reason: if rest.is_empty() {
                None
            } else {
                Some(rest.to_string())
            },
        }),

        _ => unknown_line(head, rest),
    }
}

// ---------- helpers ----------

fn parse_block_marker(rest: &str) -> Option<(u64, u32, u32)> {
    let mut parts = rest.split_whitespace();
    let timestamp: u64 = parts.next()?.parse().ok()?;
    let number: u32 = parts.next()?.parse().ok()?;
    let flags: u32 = parts.next()?.parse().ok()?;
    Some((timestamp, number, flags))
}

fn single_arg(rest: &str, mk: impl FnOnce(&str) -> Notification) -> TmuxLine {
    if rest.is_empty() {
        return unknown_line("?", rest);
    }
    TmuxLine::Notification(mk(rest))
}

fn two_args(rest: &str, mk: impl FnOnce(&str, &str) -> Notification) -> TmuxLine {
    let Some((a, b)) = rest.split_once(' ') else {
        return unknown_line("?", rest);
    };
    TmuxLine::Notification(mk(a, b))
}

fn unknown_line(head: &str, rest: &str) -> TmuxLine {
    TmuxLine::Notification(Notification::Unknown {
        name: head.to_string(),
        args: rest.to_string(),
    })
}

fn parse_output(rest: &str) -> Notification {
    // `%output` is special: its data section can carry raw bytes (anything
    // tmux didn't octal-escape, including UTF-8 multi-byte sequences). The
    // `&str` API forced lossy conversion on the way in, which mangles
    // multi-byte codepoints split across tmux's chunk boundaries. The
    // byte-precise path lives in `parse_output_bytes`.
    parse_output_bytes(rest.as_bytes())
}

/// Byte-precise `%output` parser. Used by the live client where raw bytes
/// are available directly; preserves multi-byte UTF-8 codepoints that span
/// chunk boundaries by passing them through unchanged for xterm.js to
/// re-stitch with its own buffering.
pub fn parse_output_bytes(rest: &[u8]) -> Notification {
    let Some(space) = rest.iter().position(|&b| b == b' ') else {
        return Notification::Unknown {
            name: "%output".into(),
            args: String::from_utf8_lossy(rest).into_owned(),
        };
    };
    let pane_id = String::from_utf8_lossy(&rest[..space]).into_owned();
    let data = &rest[space + 1..];
    Notification::Output {
        pane_id,
        bytes: strip_screen_title_escapes(decode_octal(data)),
    }
}

/// Strip screen/tmux-style "set window title" escape sequences:
/// `ESC k <title> ESC \`. They are emitted by `oh-my-zsh` (and many other
/// zsh setups) to set the tmux window name to the last command. xterm.js
/// doesn't recognise this older format — it only handles the OSC variant
/// `\033]2;<title>\007` — so without stripping, the title bytes leak through
/// as literal characters at the cursor position. That's the source of the
/// "command pasted into the output line" artefact (`lsApplications`,
/// `echohi`, `cd%`, etc.).
fn strip_screen_title_escapes(bytes: Vec<u8>) -> Vec<u8> {
    if !contains_esc_k(&bytes) {
        return bytes;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == 0x1b && bytes[i + 1] == b'k' {
            // Scan forward for the string terminator (ESC \).
            let mut j = i + 2;
            while j + 1 < bytes.len() {
                if bytes[j] == 0x1b && bytes[j + 1] == b'\\' {
                    break;
                }
                j += 1;
            }
            if j + 1 < bytes.len() {
                i = j + 2;
                continue;
            }
            // No terminator in this chunk — drop the rest. Worst case we
            // lose a partial title; better than leaking title bytes as text.
            break;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn contains_esc_k(bytes: &[u8]) -> bool {
    bytes
        .windows(2)
        .any(|w| w[0] == 0x1b && w[1] == b'k')
}

/// Decode tmux's control-mode escaping:
///   - `\\` → single backslash
///   - `\xxx` (three octal digits) → byte with that value
///   - everything else → literal
/// Decode tmux's `-CC` escape sequences (`\xyz` octal, `\\` for backslash)
/// back to raw bytes. tmux escapes control characters (including the tab
/// we use as a delimiter in `list-windows -F …`) in command-response
/// block data and in `%output` data alike.
pub fn decode_octal(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            // Three-octal-digit escape: \xyz where each digit is 0–7.
            if i + 3 < bytes.len()
                && (b'0'..=b'7').contains(&bytes[i + 1])
                && (b'0'..=b'7').contains(&bytes[i + 2])
                && (b'0'..=b'7').contains(&bytes[i + 3])
            {
                let value = ((bytes[i + 1] - b'0') as u16) * 64
                    + ((bytes[i + 2] - b'0') as u16) * 8
                    + ((bytes[i + 3] - b'0') as u16);
                if value <= 0xff {
                    out.push(value as u8);
                    i += 4;
                    continue;
                }
            }
            // Unrecognised escape — keep the backslash literally.
            out.push(b'\\');
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_lines_pass_through() {
        assert_eq!(parse_line("hello world"), TmuxLine::Data("hello world".into()));
        // trailing CR (sometimes seen with Windows line endings) is stripped
        assert_eq!(parse_line("hello\r"), TmuxLine::Data("hello".into()));
    }

    #[test]
    fn block_markers_parse() {
        assert_eq!(
            parse_line("%begin 1735689600 7 1"),
            TmuxLine::Begin {
                timestamp: 1735689600,
                number: 7,
                flags: 1
            }
        );
        assert_eq!(
            parse_line("%end 1735689601 7 1"),
            TmuxLine::End {
                timestamp: 1735689601,
                number: 7,
                flags: 1
            }
        );
        assert_eq!(
            parse_line("%error 1735689601 7 1"),
            TmuxLine::ResponseError {
                timestamp: 1735689601,
                number: 7,
                flags: 1
            }
        );
    }

    #[test]
    fn window_lifecycle() {
        assert_eq!(
            parse_line("%window-add @5"),
            TmuxLine::Notification(Notification::WindowAdded { window_id: "@5".into() })
        );
        assert_eq!(
            parse_line("%window-close @5"),
            TmuxLine::Notification(Notification::WindowClosed { window_id: "@5".into() })
        );
        assert_eq!(
            parse_line("%window-renamed @5 logs"),
            TmuxLine::Notification(Notification::WindowRenamed {
                window_id: "@5".into(),
                name: "logs".into()
            })
        );
    }

    #[test]
    fn session_window_changed_parses() {
        assert_eq!(
            parse_line("%session-window-changed $0 @5"),
            TmuxLine::Notification(Notification::SessionWindowChanged {
                session_id: "$0".into(),
                window_id: "@5".into(),
            })
        );
    }

    #[test]
    fn window_pane_changed_parses() {
        assert_eq!(
            parse_line("%window-pane-changed @5 %3"),
            TmuxLine::Notification(Notification::WindowPaneChanged {
                window_id: "@5".into(),
                pane_id: "%3".into(),
            })
        );
    }

    #[test]
    fn session_changed() {
        assert_eq!(
            parse_line("%session-changed $0 helm"),
            TmuxLine::Notification(Notification::SessionChanged {
                session_id: "$0".into(),
                name: "helm".into()
            })
        );
    }

    #[test]
    fn output_decodes_octal_and_backslash() {
        // \015\012 == \r\n
        let out = parse_line("%output %1 hello\\015\\012");
        let TmuxLine::Notification(Notification::Output { pane_id, bytes }) = out else {
            panic!("expected Output");
        };
        assert_eq!(pane_id, "%1");
        assert_eq!(bytes, b"hello\r\n");

        // \\ → single backslash
        let out = parse_line("%output %1 a\\\\b");
        let TmuxLine::Notification(Notification::Output { bytes, .. }) = out else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"a\\b");
    }

    #[test]
    fn exit_with_and_without_reason() {
        assert_eq!(
            parse_line("%exit"),
            TmuxLine::Notification(Notification::Exit { reason: None })
        );
        assert_eq!(
            parse_line("%exit detached"),
            TmuxLine::Notification(Notification::Exit { reason: Some("detached".into()) })
        );
    }

    #[test]
    fn unknown_notification_doesnt_crash() {
        let parsed = parse_line("%does-not-exist a b c");
        let TmuxLine::Notification(Notification::Unknown { name, args }) = parsed else {
            panic!("expected Unknown")
        };
        assert_eq!(name, "%does-not-exist");
        assert_eq!(args, "a b c");
    }

    #[test]
    fn output_strips_screen_title_escape() {
        // ESC k l s ESC \  ←  set-title sequence sandwiched between data
        let line = "%output %1 hello\\033kls\\033\\\\world";
        let TmuxLine::Notification(Notification::Output { bytes, .. }) = parse_line(line) else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"helloworld");
    }

    #[test]
    fn output_passes_through_when_no_screen_title() {
        let line = "%output %1 plain\\015\\012text";
        let TmuxLine::Notification(Notification::Output { bytes, .. }) = parse_line(line) else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"plain\r\ntext");
    }

    #[test]
    fn layout_change_carries_layout_string() {
        let parsed = parse_line("%layout-change @1 abc1,80x24,0,0,0");
        assert_eq!(
            parsed,
            TmuxLine::Notification(Notification::LayoutChanged {
                window_id: "@1".into(),
                layout: "abc1,80x24,0,0,0".into()
            })
        );
    }
}
