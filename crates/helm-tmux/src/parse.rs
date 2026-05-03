//! Pure parser for tmux control mode (`tmux -CC`) lines.
//!
//! Protocol summary (from `tmux(1)` and iTerm2's reference impl):
//!
//! - `%begin <ts> <num> <flags>` … `%end <ts> <num> <flags>` brackets a
//!   command response. `%error` replaces `%end` for failures.
//! - `%notification arg1 arg2 …` are single-line state deltas.
//! - `%output %P data` is per-pane output. Backslash and bytes < 040 are
//!   octal-escaped (`\xxx`), so we decode before yielding the bytes.

pub use helm_domain::{OutputMarker, TmuxNotification as Notification};

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
///
/// Walks the decoded bytes once to do three things:
///   1. Strip screen/tmux title escapes (`ESC k … ESC \`) — see the helper
///      for the historical reason.
///   2. Strip OSC 133 prompt-integration sequences (`ESC ] 1 3 3 ; …`) and
///      surface them as `OutputMarker`s for the notifications layer.
///   3. Strip raw BEL bytes (0x07) and surface them as `OutputMarker::Bell`,
///      so xterm doesn't actually beep on every prompt the user wants
///      surfaced in the inbox.
pub fn parse_output_bytes(rest: &[u8]) -> Notification {
    let Some(space) = rest.iter().position(|&b| b == b' ') else {
        return Notification::Unknown {
            name: "%output".into(),
            args: String::from_utf8_lossy(rest).into_owned(),
        };
    };
    let pane_id = String::from_utf8_lossy(&rest[..space]).into_owned();
    let data = &rest[space + 1..];
    let (bytes, markers) = extract_markers_and_strip(decode_octal(data));
    Notification::Output {
        pane_id,
        bytes,
        markers,
    }
}

/// Single-pass scan over decoded pane output. Splits the byte stream into
/// (cleaned bytes for xterm, ordered list of in-band markers).
///
/// Markers handled:
///   - Standalone `0x07` (BEL) → `Bell`. Stripped so xterm doesn't beep.
///   - `ESC ] 1 3 3 ; X [ ; …] (BEL | ESC \)` (OSC 133) → `PromptStart` /
///     `CommandStart` / `OutputStart` / `CommandDone { exit_code }`.
///     Stripped because xterm doesn't render OSC 133 and we don't want
///     the params leaking through as glyphs in older xterm builds.
///   - `ESC k … ESC \` (screen-style title set) → silently dropped, see
///     the historical bug note in `parse_output_bytes`'s docstring above.
///
/// Critical subtlety: OSC sequences use BEL as a valid string terminator.
/// A naive "strip every BEL" misclassifies the BEL ending an OSC 0 (set
/// window title) as a standalone bell. We treat any `ESC ]` as an OSC
/// envelope — when it's not 133, we pass the whole sequence through
/// verbatim including the terminator. Same for DCS (`ESC P`), SOS
/// (`ESC X`), PM (`ESC ^`), and APC (`ESC _`) — all ST-terminated,
/// so their inner BELs/ESCs are protocol, not data.
///
/// Order is preserved: a Bell that arrives between a CommandStart and a
/// CommandDone shows up between them in the returned vec, which lets the
/// helm-app forwarder reason about overlapping events deterministically.
pub fn extract_markers_and_strip(bytes: Vec<u8>) -> (Vec<u8>, Vec<OutputMarker>) {
    // Fast path: no escape sequences and no BEL means nothing to do.
    // Worth the scan because most output chunks are plain text.
    if !bytes.contains(&0x1b) && !bytes.contains(&0x07) {
        return (bytes, Vec::new());
    }

    let mut out = Vec::with_capacity(bytes.len());
    let mut markers = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];

        // ESC-prefixed sequences. We check for these FIRST so the BEL-as-
        // terminator inside an OSC isn't misclassified as a standalone bell.
        if b == 0x1b && i + 1 < bytes.len() {
            // Screen/tmux title set: ESC k <title> ESC \.
            if bytes[i + 1] == b'k' {
                if let Some(end) = find_st(&bytes, i + 2) {
                    i = end;
                    continue;
                }
                // No terminator visible in this chunk — drop the rest
                // rather than leak title bytes as glyphs.
                break;
            }

            // OSC (and similar ST-terminated envelopes). OSC 133 is ours
            // to extract; any other OSC passes through verbatim with its
            // terminator intact.
            //
            // 0x5d = ']' (OSC), 0x50 = 'P' (DCS), 0x58 = 'X' (SOS),
            // 0x5e = '^' (PM), 0x5f = '_' (APC).
            let intro = bytes[i + 1];
            if matches!(intro, b']' | b'P' | b'X' | b'^' | b'_') {
                if intro == b']' {
                    if let Some((marker, end)) = try_parse_osc133(&bytes, i) {
                        markers.push(marker);
                        i = end;
                        continue;
                    }
                }
                // Pass through to terminator. Find the ST and copy the
                // entire range — including the terminator — into the
                // output unchanged.
                if let Some(end) = find_st(&bytes, i + 2) {
                    out.extend_from_slice(&bytes[i..end]);
                    i = end;
                    continue;
                }
                // Unterminated envelope — copy what we have and stop.
                // Better than dropping it: a partial CSI chunk re-stitches
                // on the xterm side once the next %output arrives.
                out.extend_from_slice(&bytes[i..]);
                break;
            }
        }

        // BEL — by elimination, this BEL is *not* part of any OSC/DCS/etc.
        // envelope, so it's a standalone application bell.
        if b == 0x07 {
            markers.push(OutputMarker::Bell);
            i += 1;
            continue;
        }

        out.push(b);
        i += 1;
    }
    (out, markers)
}

/// Look for the ST (string terminator) after `ESC k …` titles.
/// Returns the index *after* the terminator (so the caller can resume
/// from there). Accepts both ST forms used in the wild:
///   - 7-bit ST: `ESC \` (0x1b 0x5c)
///   - BEL ST:   `0x07` (xterm-style abbreviation)
fn find_st(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start;
    while j < bytes.len() {
        if bytes[j] == 0x07 {
            return Some(j + 1);
        }
        if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
            return Some(j + 2);
        }
        j += 1;
    }
    None
}

/// Parse an OSC 133 sequence starting at `i` (`bytes[i]` is `ESC`).
/// Returns the extracted marker plus the index *after* the terminator.
/// Returns None if the prefix doesn't match `ESC ] 1 3 3 ;` or if no
/// terminator is found in the remaining bytes.
fn try_parse_osc133(bytes: &[u8], i: usize) -> Option<(OutputMarker, usize)> {
    // Need at least: ESC ] 1 3 3 ; X (BEL) — 8 bytes minimum.
    if i + 7 > bytes.len() {
        return None;
    }
    if &bytes[i..i + 6] != b"\x1b]133;" {
        return None;
    }
    let body_start = i + 6;
    let end = find_st(bytes, body_start)?;
    // `end` points just past the terminator; the body sits in
    // [body_start, term_start). Strip the terminator length (1 for BEL,
    // 2 for ESC \) by re-detecting which one we hit.
    let term_len = if bytes[end - 1] == 0x07 { 1 } else { 2 };
    let body = &bytes[body_start..end - term_len];

    // body is `<kind>` or `<kind>;<params...>`. We only look at the first
    // char as the kind, and parse params if present.
    let kind = body.first().copied()?;
    let marker = match kind {
        b'A' => OutputMarker::PromptStart,
        b'B' => OutputMarker::CommandStart,
        b'C' => OutputMarker::OutputStart,
        b'D' => {
            // `D` may be bare or `D;<exit_code>[;...]`. Extract the first
            // semicolon-separated field after `D` if present.
            let exit_code = if body.len() > 2 && body[1] == b';' {
                let rest = &body[2..];
                let end_field = rest.iter().position(|&c| c == b';').unwrap_or(rest.len());
                std::str::from_utf8(&rest[..end_field])
                    .ok()
                    .and_then(|s| s.trim().parse::<i32>().ok())
            } else {
                None
            };
            OutputMarker::CommandDone { exit_code }
        }
        _ => return None,
    };
    Some((marker, end))
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
        let TmuxLine::Notification(Notification::Output { pane_id, bytes, markers }) = out else {
            panic!("expected Output");
        };
        assert_eq!(pane_id, "%1");
        assert_eq!(bytes, b"hello\r\n");
        assert!(markers.is_empty());

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
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"helloworld");
        // Title stripping doesn't surface a marker — it's purely cosmetic.
        assert!(markers.is_empty());
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
    fn output_extracts_bell_and_strips_it() {
        // Plain BEL embedded in output. xterm should never see the 0x07.
        let line = "%output %1 done\\007\\015\\012";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"done\r\n");
        assert_eq!(markers, vec![OutputMarker::Bell]);
    }

    #[test]
    fn output_extracts_osc133_prompt_and_command_markers() {
        // ESC ] 1 3 3 ; A BEL  …  ESC ] 1 3 3 ; B BEL
        let line = "%output %1 \\033]133;A\\007$ \\033]133;B\\007ls -la";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"$ ls -la");
        assert_eq!(
            markers,
            vec![OutputMarker::PromptStart, OutputMarker::CommandStart]
        );
    }

    #[test]
    fn output_extracts_osc133_command_done_with_exit_code() {
        // OSC 133 ; D ; 0 — successful exit
        let line = "%output %1 \\033]133;C\\007output\\015\\012\\033]133;D;0\\007";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"output\r\n");
        assert_eq!(
            markers,
            vec![
                OutputMarker::OutputStart,
                OutputMarker::CommandDone { exit_code: Some(0) },
            ]
        );
    }

    #[test]
    fn output_extracts_osc133_command_done_with_nonzero_exit() {
        let line = "%output %1 \\033]133;D;127\\007";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert!(bytes.is_empty());
        assert_eq!(
            markers,
            vec![OutputMarker::CommandDone { exit_code: Some(127) }]
        );
    }

    #[test]
    fn output_handles_bare_command_done() {
        // No exit code field. Some integration scripts emit `D` alone
        // when they don't have access to $? (e.g., signal-based exit).
        let line = "%output %1 \\033]133;D\\007";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert!(bytes.is_empty());
        assert_eq!(
            markers,
            vec![OutputMarker::CommandDone { exit_code: None }]
        );
    }

    #[test]
    fn output_accepts_st_terminator_for_osc133() {
        // ESC \ instead of BEL for the OSC terminator. xterm.js handles
        // both; our parser must too — newer shells lean toward ST.
        let line = "%output %1 \\033]133;A\\033\\\\prompt$ ";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        assert_eq!(bytes, b"prompt$ ");
        assert_eq!(markers, vec![OutputMarker::PromptStart]);
    }

    #[test]
    fn output_passes_through_unrelated_osc() {
        // OSC 0 (set window title) and OSC 11 (foreground color) — must
        // pass through unchanged, since they're standard terminal escapes
        // xterm renders correctly. Only OSC 133 is ours.
        let line = "%output %1 \\033]0;title\\007hello";
        let TmuxLine::Notification(Notification::Output { bytes, markers, .. }) =
            parse_line(line)
        else {
            panic!("expected Output");
        };
        // We pass the OSC 0 sequence through intact (xterm handles it).
        assert!(markers.is_empty());
        assert_eq!(bytes, b"\x1b]0;title\x07hello");
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
