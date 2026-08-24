//! Stateful streaming parser for the session byte stream.
//!
//! Consumes raw PTY output and produces (a) the bytes to store/forward
//! and (b) semantic events: OSC 133 block markers, standalone bells, and
//! alt-screen transitions. OSC 133 sequences and bells are stripped from
//! the stored stream (the frontend gets blocks and notifications as
//! *data*); everything else passes through byte-exact.
//!
//! The defining property — and the fix for the tmux-era jank — is that
//! the parser is **stateful across feeds**: an escape sequence split at
//! any byte boundary between reads parses identically to the contiguous
//! stream. The old per-chunk `extract_markers_and_strip` in helm-tmux
//! mangled split sequences (a trailing bare ESC leaked through raw, an
//! OSC 8 BEL terminator in the next chunk became a phantom bell while
//! xterm never saw the terminator). Every state here survives a feed
//! boundary; `tests::split_at_every_boundary` locks that in.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

/// OSC 133 prompt-integration markers (emitted by our shell scripts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc133 {
    /// `A` — prompt is about to print.
    PromptStart {
        cwd: Option<String>,
        branch: Option<String>,
        /// Git toplevel of `cwd` (a worktree's own root); `None`
        /// outside a repo or from an older shim that doesn't send it.
        root: Option<String>,
    },
    /// `B` — command line accepted.
    CommandStart { cmdline: Option<String> },
    /// `C` — command output begins.
    OutputStart,
    /// `D[;exit]` — command finished.
    CommandDone { exit_code: Option<i32> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestEvent {
    Marker(Osc133),
    /// Standalone BEL (not an OSC terminator) — an application bell.
    Bell,
    /// OSC 9 notification with a message (stripped from the output).
    Notify(String),
    /// DECSET/DECRST 1049/1047/47 — alt screen entered/left.
    AltScreen(bool),
}

/// An event plus its offset into the *output* bytes produced so far by
/// the feed that generated it (callers add their ring head_seq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAt {
    pub offset: usize,
    pub event: IngestEvent,
}

/// Cap on buffered in-flight sequence bytes. A stream that opens an OSC
/// and never terminates it (garbage, `cat /dev/urandom`) must not buffer
/// unboundedly: past the cap we flush the buffered bytes through raw and
/// reset to ground. 96 KB comfortably exceeds any real OSC (titles,
/// hyperlinks, clipboard OSC 52 of pathological size get flushed).
const MAX_SEQ_BUF: usize = 96 * 1024;

#[derive(Debug)]
enum State {
    Ground,
    /// Saw ESC, awaiting the introducer.
    Esc,
    /// ESC [ — collecting params until a final byte (0x40..=0x7E).
    Csi,
    /// ESC ] — collecting the OSC string.
    Osc,
    /// In OSC, saw ESC (ST is ESC \).
    OscEsc,
    /// ESC P / X / ^ / _ — DCS/SOS/PM/APC string, until ST.
    Str,
    /// In Str, saw ESC.
    StrEsc,
    /// ESC k — screen/tmux title. Dropped entirely (xterm renders the
    /// body as glyphs otherwise — the oh-my-zsh artifact).
    Title,
    /// In Title, saw ESC.
    TitleEsc,
}

pub struct StreamParser {
    state: State,
    /// Bytes of the in-flight sequence, *including* its ESC introducer,
    /// so a flush-through reproduces the input exactly.
    seq_buf: Vec<u8>,
}

impl Default for StreamParser {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamParser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            seq_buf: Vec::new(),
        }
    }

    /// Emit a completed pass-through sequence.
    fn flush_seq(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.seq_buf);
        self.seq_buf.clear();
    }

    /// Feed raw PTY bytes. Appends pass-through bytes to `out` and
    /// semantic events (with offsets relative to `out`'s length at call
    /// time... no — offsets are absolute into `out`) to `events`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, events: &mut Vec<EventAt>) {
        for &b in input {
            self.step(b, out, events);
            if self.seq_buf.len() > MAX_SEQ_BUF {
                // Runaway unterminated sequence: flush raw, reset.
                out.extend_from_slice(&self.seq_buf);
                self.seq_buf.clear();
                self.state = State::Ground;
            }
        }
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>, events: &mut Vec<EventAt>) {
        match self.state {
            State::Ground => match b {
                0x1b => {
                    self.seq_buf.push(b);
                    self.state = State::Esc;
                }
                0x07 => events.push(EventAt {
                    offset: out.len(),
                    event: IngestEvent::Bell,
                }),
                _ => out.push(b),
            },

            State::Esc => match b {
                b'[' => {
                    self.seq_buf.push(b);
                    self.state = State::Csi;
                }
                b']' => {
                    self.seq_buf.push(b);
                    self.state = State::Osc;
                }
                b'P' | b'X' | b'^' | b'_' => {
                    self.seq_buf.push(b);
                    self.state = State::Str;
                }
                b'k' => {
                    self.seq_buf.push(b);
                    self.state = State::Title;
                }
                0x1b => {
                    // ESC ESC: emit the first, stay in Esc for the second.
                    out.push(0x1b);
                    // seq_buf holds one ESC already; keep it for the new one.
                }
                _ => {
                    // Two-byte escape (ESC c, ESC 7, charset selection, …):
                    // not ours to interpret, pass through.
                    self.seq_buf.push(b);
                    self.flush_seq(out);
                    self.state = State::Ground;
                }
            },

            State::Csi => {
                self.seq_buf.push(b);
                if (0x40..=0x7e).contains(&b) {
                    // Final byte — sequence complete. Always pass through;
                    // inspect for alt-screen transitions.
                    if let Some(enter) = parse_alt_screen(&self.seq_buf) {
                        // Event offset points at the sequence start so the
                        // renderer can switch modes before drawing it.
                        events.push(EventAt {
                            offset: out.len(),
                            event: IngestEvent::AltScreen(enter),
                        });
                    }
                    self.flush_seq(out);
                    self.state = State::Ground;
                }
            }

            State::Osc => match b {
                0x07 => self.finish_osc(true, out, events),
                0x1b => {
                    self.seq_buf.push(b);
                    self.state = State::OscEsc;
                }
                _ => self.seq_buf.push(b),
            },
            State::OscEsc => {
                if b == b'\\' {
                    self.seq_buf.push(b);
                    self.finish_osc(false, out, events);
                } else {
                    // ESC inside an OSC that isn't ST — keep both bytes as
                    // string content and stay in the OSC.
                    self.seq_buf.push(b);
                    self.state = State::Osc;
                }
            }

            State::Str => {
                self.seq_buf.push(b);
                if b == 0x1b {
                    self.state = State::StrEsc;
                }
            }
            State::StrEsc => {
                self.seq_buf.push(b);
                if b == b'\\' {
                    // ST — pass the whole envelope through verbatim.
                    self.flush_seq(out);
                    self.state = State::Ground;
                } else if b != 0x1b {
                    self.state = State::Str;
                }
            }

            State::Title => match b {
                // screen terminates titles with ESC \; some emitters use BEL.
                0x07 => {
                    self.seq_buf.clear();
                    self.state = State::Ground;
                }
                0x1b => {
                    self.seq_buf.push(b);
                    self.state = State::TitleEsc;
                }
                _ => self.seq_buf.push(b),
            },
            State::TitleEsc => {
                if b == b'\\' {
                    self.seq_buf.clear();
                    self.state = State::Ground;
                } else if b == 0x1b {
                    self.seq_buf.push(b);
                    // stay in TitleEsc
                } else {
                    self.seq_buf.push(b);
                    self.state = State::Title;
                }
            }
        }
    }

    /// OSC string complete. `bel_terminated` tells us which terminator to
    /// reproduce for pass-through sequences.
    fn finish_osc(&mut self, bel_terminated: bool, out: &mut Vec<u8>, events: &mut Vec<EventAt>) {
        // seq_buf = ESC ] <payload> [ESC when ST came via OscEsc... already pushed]
        // Payload starts after the 2-byte introducer. For the ESC\ case the
        // trailing "ESC \" is already in seq_buf; strip it to inspect.
        let payload_end = if bel_terminated {
            self.seq_buf.len()
        } else {
            self.seq_buf.len() - 2
        };
        let payload = &self.seq_buf[2..payload_end];

        if let Some(rest) = payload.strip_prefix(b"133;") {
            if let Some(marker) = parse_osc133(rest) {
                events.push(EventAt {
                    offset: out.len(),
                    event: IngestEvent::Marker(marker),
                });
            }
            // Ours either way — strip malformed 133 rather than leak it.
        } else if let Some(text) = payload.strip_prefix(b"9;") {
            events.push(EventAt {
                offset: out.len(),
                event: IngestEvent::Notify(String::from_utf8_lossy(text).into_owned()),
            });
        } else {
            out.extend_from_slice(&self.seq_buf);
            if bel_terminated {
                out.push(0x07);
            }
        }
        self.seq_buf.clear();
        self.state = State::Ground;
    }
}

/// `CSI ? 1049 h/l` (and legacy 47 / 1047) → Some(entering).
fn parse_alt_screen(seq: &[u8]) -> Option<bool> {
    // seq = ESC [ <params> <final>
    let final_byte = *seq.last()?;
    let enter = match final_byte {
        b'h' => true,
        b'l' => false,
        _ => return None,
    };
    let params = &seq[2..seq.len() - 1];
    let params = params.strip_prefix(b"?")?;
    // Params can be a list: `?1049h` or `?1;1049h`.
    for p in params.split(|&c| c == b';') {
        if p == b"1049" || p == b"1047" || p == b"47" {
            return Some(enter);
        }
    }
    None
}

fn parse_osc133(body: &[u8]) -> Option<Osc133> {
    let (kind, rest) = match body.split_first()? {
        (k, rest) => (*k, rest),
    };
    let fields = |raw: &[u8]| -> Vec<(String, String)> {
        raw.split(|&c| c == b';')
            .filter_map(|kv| {
                let s = std::str::from_utf8(kv).ok()?;
                let (k, v) = s.split_once('=')?;
                Some((k.to_string(), v.to_string()))
            })
            .collect()
    };
    let b64_field = |raw: &[u8], key: &str| -> Option<String> {
        fields(raw)
            .into_iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| {
                let bytes = B64.decode(v.as_bytes()).ok()?;
                let s = String::from_utf8_lossy(&bytes).into_owned();
                (!s.is_empty()).then_some(s)
            })
    };
    match kind {
        b'A' => Some(Osc133::PromptStart {
            cwd: b64_field(rest, "cwd_b64"),
            branch: b64_field(rest, "branch_b64"),
            root: b64_field(rest, "root_b64"),
        }),
        b'B' => Some(Osc133::CommandStart {
            cmdline: b64_field(rest, "cmdline_b64"),
        }),
        b'C' => Some(Osc133::OutputStart),
        b'D' => {
            let exit_code = rest
                .strip_prefix(b";")
                .and_then(|r| std::str::from_utf8(r).ok())
                .and_then(|s| s.trim().parse::<i32>().ok());
            Some(Osc133::CommandDone { exit_code })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_whole(input: &[u8]) -> (Vec<u8>, Vec<EventAt>) {
        let mut p = StreamParser::new();
        let (mut out, mut events) = (Vec::new(), Vec::new());
        p.feed(input, &mut out, &mut events);
        (out, events)
    }

    fn b64(s: &str) -> String {
        B64.encode(s.as_bytes())
    }

    #[test]
    fn plain_text_passes_through() {
        let (out, events) = run_whole(b"hello world\r\n");
        assert_eq!(out, b"hello world\r\n");
        assert!(events.is_empty());
    }

    #[test]
    fn osc133_markers_stripped_and_parsed() {
        let input = format!(
            "\x1b]133;A;cwd_b64={};branch_b64={};root_b64={}\x07before\x1b]133;B;cmdline_b64={}\x07\x1b]133;C\x07out\x1b]133;D;1\x07",
            b64("/Users/x/code/src"),
            b64("main"),
            b64("/Users/x/code"),
            b64("cargo test"),
        );
        let (out, events) = run_whole(input.as_bytes());
        assert_eq!(out, b"beforeout");
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0].event,
            IngestEvent::Marker(Osc133::PromptStart {
                cwd: Some("/Users/x/code/src".into()),
                branch: Some("main".into()),
                root: Some("/Users/x/code".into()),
            })
        );
        assert_eq!(events[0].offset, 0);
        assert_eq!(
            events[1].event,
            IngestEvent::Marker(Osc133::CommandStart {
                cmdline: Some("cargo test".into())
            })
        );
        assert_eq!(events[1].offset, 6); // after "before"
        assert_eq!(events[2].event, IngestEvent::Marker(Osc133::OutputStart));
        assert_eq!(
            events[3].event,
            IngestEvent::Marker(Osc133::CommandDone { exit_code: Some(1) })
        );
        assert_eq!(events[3].offset, 9); // after "beforeout"
    }

    #[test]
    fn osc8_hyperlink_passes_through_both_terminators() {
        // BEL-terminated
        let input = b"\x1b]8;;file:///x\x07label\x1b]8;;\x07";
        let (out, events) = run_whole(input);
        assert_eq!(out, input);
        assert!(events.is_empty());
        // ST-terminated
        let input = b"\x1b]8;;file:///x\x1b\\label\x1b]8;;\x1b\\";
        let (out, events) = run_whole(input);
        assert_eq!(out.as_slice(), input.as_slice());
        assert!(events.is_empty());
    }

    #[test]
    fn standalone_bell_is_event_not_osc_terminator() {
        let (out, events) = run_whole(b"ding\x07dong");
        assert_eq!(out, b"dingdong");
        assert_eq!(
            events,
            vec![EventAt {
                offset: 4,
                event: IngestEvent::Bell
            }]
        );
    }

    #[test]
    fn osc9_is_a_notification_with_text() {
        let (out, events) = run_whole(b"a\x1b]9;Claude finished\x07b\x1b]9;st\x1b\\c");
        assert_eq!(out, b"abc");
        assert_eq!(
            events,
            vec![
                EventAt {
                    offset: 1,
                    event: IngestEvent::Notify("Claude finished".into())
                },
                EventAt {
                    offset: 2,
                    event: IngestEvent::Notify("st".into())
                },
            ]
        );
    }

    #[test]
    fn screen_titles_dropped() {
        let (out, events) = run_whole(b"a\x1bkwindow title\x1b\\b\x1bkbel title\x07c");
        assert_eq!(out, b"abc");
        assert!(events.is_empty());
    }

    #[test]
    fn alt_screen_transitions() {
        let (out, events) = run_whole(b"\x1b[?1049hTUI\x1b[?1049l");
        assert_eq!(out, b"\x1b[?1049hTUI\x1b[?1049l"); // bytes preserved
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, IngestEvent::AltScreen(true));
        assert_eq!(events[0].offset, 0);
        assert_eq!(events[1].event, IngestEvent::AltScreen(false));
        // Non-mode CSI is untouched and unreported.
        let (out, events) = run_whole(b"\x1b[31mred\x1b[0m");
        assert_eq!(out, b"\x1b[31mred\x1b[0m");
        assert!(events.is_empty());
    }

    #[test]
    fn dcs_passes_through() {
        let input = b"\x1bPq#0;2;0;0;0#0!5~\x1b\\after";
        let (out, events) = run_whole(input);
        assert_eq!(out.as_slice(), input.as_slice());
        assert!(events.is_empty());
    }

    #[test]
    fn esc_esc_and_two_byte_escapes() {
        let (out, _) = run_whole(b"\x1b\x1b[31m");
        // First ESC emitted raw, second starts a CSI which passes through.
        assert_eq!(out, b"\x1b\x1b[31m");
        let (out, _) = run_whole(b"\x1b7save\x1b8");
        assert_eq!(out, b"\x1b7save\x1b8");
    }

    /// The load-bearing test: every split position of a composite stream
    /// must produce byte-identical output and identical events to the
    /// contiguous parse. This is exactly what the per-chunk tmux parser
    /// could not do.
    #[test]
    fn split_at_every_boundary() {
        let input = format!(
            "start\x1b]133;A;cwd_b64={}\x07\x1b]8;;https://x.dev\x07link\x1b]8;;\x07\x1b[?1049h\x1bkTITLE\x1b\\tui\x07\x1b[?1049l\x1b]133;D;0\x07end",
            b64("/home/u"),
        )
        .into_bytes();
        let (whole_out, whole_events) = run_whole(&input);

        for split in 0..=input.len() {
            let mut p = StreamParser::new();
            let (mut out, mut events) = (Vec::new(), Vec::new());
            p.feed(&input[..split], &mut out, &mut events);
            p.feed(&input[split..], &mut out, &mut events);
            assert_eq!(out, whole_out, "output diverged at split {split}");
            assert_eq!(events, whole_events, "events diverged at split {split}");
        }
    }

    /// Same property under a harsher regime: byte-at-a-time feeding.
    #[test]
    fn byte_at_a_time() {
        let input = b"\x1b]133;C\x07mixed \x07 bells\x1b[?47htui\x1b[?47l";
        let (whole_out, whole_events) = run_whole(input);
        let mut p = StreamParser::new();
        let (mut out, mut events) = (Vec::new(), Vec::new());
        for &b in input.iter() {
            p.feed(&[b], &mut out, &mut events);
        }
        assert_eq!(out, whole_out);
        assert_eq!(events, whole_events);
    }

    #[test]
    fn runaway_sequence_flushes_raw() {
        let mut p = StreamParser::new();
        let (mut out, mut events) = (Vec::new(), Vec::new());
        p.feed(b"\x1b]", &mut out, &mut events);
        let junk = vec![b'x'; MAX_SEQ_BUF + 10];
        p.feed(&junk, &mut out, &mut events);
        // Flushed through raw rather than buffered forever.
        assert!(out.len() >= MAX_SEQ_BUF);
        assert!(out.starts_with(b"\x1b]xxx"));
        // Parser recovered: subsequent text flows normally.
        p.feed(b"ok", &mut out, &mut events);
        assert!(out.ends_with(b"ok"));
    }
}
