//! `PULT_EVENTS` progress-events protocol (v1): line-based events a running
//! command may write to the fd named by `$PULT_EVENTS`. Vocabulary:
//!
//! ```text
//! progress <0-100|?> [text]    # determinate percent, or ? = indeterminate
//! status <text>                # transient activity line
//! step <k>/<n> <name>          # entering step k of n
//! ```
//!
//! Unknown verbs and malformed lines are silently ignored — never an error.
//! This is load-bearing for forward compatibility: a script or surface built
//! against a later vocabulary must never break an older pult, and vice versa.

use std::io::Write;

/// One parsed protocol line — kept minimal, just what the CLI renderer (and,
/// per the passthrough rule, a future richer surface) needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// `progress <0-100|?> [text]` — `pct: None` means indeterminate (`?`).
    Progress {
        pct: Option<u8>,
        text: Option<String>,
    },
    /// `status <text>` — transient activity line; the plain CLI consumes and
    /// drops it (it exists for richer surfaces), so it carries no rendering.
    Status(String),
    /// `step <k>/<n> <name>` — entering step `k` of `n` (1-based, `k <= n`).
    Step { k: u32, n: u32, name: String },
}

/// Parse one line of the protocol. Returns `None` for anything malformed or
/// an unrecognized verb — callers must never error on this.
pub fn parse(line: &str) -> Option<Event> {
    let line = line.trim();
    let (verb, rest) = split_first(line);
    match verb {
        "progress" => parse_progress(rest),
        "status" => {
            if rest.is_empty() {
                None
            } else {
                Some(Event::Status(rest.to_string()))
            }
        }
        "step" => parse_step(rest),
        _ => None,
    }
}

fn parse_progress(rest: &str) -> Option<Event> {
    let (pct_str, text) = split_first(rest);
    let text = if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    };
    if pct_str == "?" {
        return Some(Event::Progress { pct: None, text });
    }
    // Parsed as u32 (not u8): a percent above 255 (e.g. `300`) must still
    // clamp to 100 rather than fail the parse and drop the event entirely —
    // a stuck progress bar is worse than an over-clamped one.
    let pct: u32 = pct_str.parse().ok()?;
    Some(Event::Progress {
        pct: Some(pct.min(100) as u8),
        text,
    })
}

fn parse_step(rest: &str) -> Option<Event> {
    let (kn, name) = split_first(rest);
    let (k_str, n_str) = kn.split_once('/')?;
    let k: u32 = k_str.parse().ok()?;
    let n: u32 = n_str.parse().ok()?;
    if k == 0 || n == 0 || k > n || name.is_empty() {
        return None;
    }
    Some(Event::Step {
        k,
        n,
        name: name.to_string(),
    })
}

/// Split on the first space; `("", "")` for an empty input, `(whole, "")`
/// when there's no space.
fn split_first(s: &str) -> (&str, &str) {
    match s.split_once(' ') {
        Some((a, b)) => (a, b.trim_start()),
        None => (s, ""),
    }
}

/// Renders parsed events as OSC 9;4 progress sequences (the ConEmu/Windows
/// Terminal/WezTerm/Ghostty progress protocol) to a writer — stderr, in the
/// plain CLI. Tracks whether an explicit `progress` has arrived so `step`
/// milestones stop driving the percentage once one has (explicit beats
/// derived — see `handle`).
#[derive(Default)]
pub struct Renderer {
    explicit_progress_seen: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one parsed event, writing OSC bytes to `out` as needed. Returns
    /// whether this call actually wrote an OSC sequence — callers use this
    /// to track whether a run ever rendered anything at all, since a run
    /// that never does must never get a final OSC either (see
    /// `render_final`).
    pub fn handle(&mut self, event: &Event, out: &mut impl Write) -> bool {
        match event {
            Event::Progress { pct: Some(p), .. } => {
                self.explicit_progress_seen = true;
                write_osc(out, 1, *p);
                true
            }
            Event::Progress { pct: None, .. } => {
                self.explicit_progress_seen = true;
                write_osc(out, 3, 0);
                true
            }
            // Consumed, not rendered — `status` exists for richer surfaces.
            Event::Status(_) => false,
            Event::Step { k, n, .. } => {
                // Coarse progress from milestones — only until a real
                // `progress` event takes over.
                if !self.explicit_progress_seen {
                    let pct = ((*k - 1) as u64 * 100 / *n as u64) as u8;
                    write_osc(out, 1, pct);
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// The run-ending OSC: always clears (state 0), regardless of the command's
/// exit code. There used to be a persistent "error" state (2) for non-zero
/// exits, but a progress badge stuck red forever (nothing ever un-sets it
/// outside another `pult` run) is worse than no badge at all — so this
/// always clears.
///
/// Callers must only invoke this when at least one event was actually
/// rendered during the run — a command that never emits anything must
/// produce zero bytes of OSC, including no final clear.
pub fn render_final(out: &mut impl Write) {
    write_osc(out, 0, 0);
}

/// `ESC ] 9 ; 4 ; <state> ; <pct> ESC \` — state 0=clear, 1=set pct,
/// 2=error, 3=indeterminate. Best-effort: a write failure to stderr isn't
/// something progress rendering should ever surface as an error.
///
/// Builds the whole sequence in one buffer and issues a single `write_all`
/// rather than a `write!` directly on `out`: the latter emits one syscall
/// per formatted fragment, leaving a window where the child's own stdout
/// writes (sharing the same tty) can splice into the middle of an escape
/// sequence — observed in practice as a corrupted OSC code.
fn write_osc(out: &mut impl Write, state: u8, pct: u8) {
    let seq = format!("\x1b]9;4;{state};{pct}\x1b\\");
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_determinate_progress() {
        assert_eq!(
            parse("progress 40 restoring"),
            Some(Event::Progress {
                pct: Some(40),
                text: Some("restoring".to_string())
            })
        );
    }

    #[test]
    fn parses_progress_without_text() {
        assert_eq!(
            parse("progress 40"),
            Some(Event::Progress {
                pct: Some(40),
                text: None
            })
        );
    }

    #[test]
    fn parses_indeterminate_progress() {
        assert_eq!(
            parse("progress ?"),
            Some(Event::Progress {
                pct: None,
                text: None
            })
        );
        assert_eq!(
            parse("progress ? thinking"),
            Some(Event::Progress {
                pct: None,
                text: Some("thinking".to_string())
            })
        );
    }

    #[test]
    fn clamps_percent_over_100() {
        assert_eq!(
            parse("progress 150 uploading"),
            Some(Event::Progress {
                pct: Some(100),
                text: Some("uploading".to_string())
            })
        );
    }

    #[test]
    fn parses_status() {
        assert_eq!(
            parse("status restoring the database"),
            Some(Event::Status("restoring the database".to_string()))
        );
    }

    #[test]
    fn status_without_text_is_malformed() {
        assert_eq!(parse("status"), None);
        assert_eq!(parse("status "), None);
    }

    #[test]
    fn parses_step() {
        assert_eq!(
            parse("step 2/5 run-migrations"),
            Some(Event::Step {
                k: 2,
                n: 5,
                name: "run-migrations".to_string()
            })
        );
    }

    #[test]
    fn rejects_step_zero_of_n() {
        assert_eq!(parse("step 0/5 x"), None);
    }

    #[test]
    fn rejects_step_zero_total() {
        assert_eq!(parse("step 1/0 x"), None);
    }

    #[test]
    fn rejects_k_greater_than_n() {
        assert_eq!(parse("step 6/5 x"), None);
    }

    #[test]
    fn allows_k_equal_n() {
        assert!(parse("step 5/5 last").is_some());
    }

    #[test]
    fn junk_lines_are_ignored() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("hello world"), None);
        assert_eq!(parse("progress abc"), None);
        assert_eq!(parse("step notaslash x"), None);
        assert_eq!(parse("step 1/2"), None); // missing name
    }

    #[test]
    fn unknown_verb_is_ignored() {
        assert_eq!(parse("frobnicate 42"), None);
    }

    #[test]
    fn step_derived_pct_yields_to_explicit_progress() {
        let mut renderer = Renderer::new();
        let mut buf = Vec::new();

        // First a milestone: derived pct = (2-1)*100/4 = 25 → state 1;25.
        renderer.handle(
            &Event::Step {
                k: 2,
                n: 4,
                name: "x".to_string(),
            },
            &mut buf,
        );
        assert!(
            String::from_utf8_lossy(&buf).contains("9;4;1;25"),
            "got: {:?}",
            buf
        );

        // Then an explicit progress — drives the percent from here on.
        buf.clear();
        renderer.handle(
            &Event::Progress {
                pct: Some(60),
                text: None,
            },
            &mut buf,
        );
        assert!(String::from_utf8_lossy(&buf).contains("9;4;1;60"));

        // A further step milestone must NOT override the explicit percent.
        buf.clear();
        renderer.handle(
            &Event::Step {
                k: 3,
                n: 4,
                name: "y".to_string(),
            },
            &mut buf,
        );
        assert!(
            buf.is_empty(),
            "step must yield once explicit progress was seen, got: {:?}",
            buf
        );
    }

    #[test]
    fn status_renders_nothing() {
        let mut renderer = Renderer::new();
        let mut buf = Vec::new();
        renderer.handle(&Event::Status("working".to_string()), &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn final_osc_always_clears() {
        // No persistent error state anymore (fix: a stuck red badge is
        // worse than none) — `render_final` always emits the clear state,
        // independent of how the run went.
        let mut buf = Vec::new();
        render_final(&mut buf);
        assert!(String::from_utf8_lossy(&buf).contains("9;4;0;0"));
    }

    #[test]
    fn clamps_percent_far_over_100() {
        // `300` doesn't fit in a u8, so parsing it as one used to fail the
        // event entirely (a frozen progress bar); parsed as u32 first, it
        // clamps to 100 like any other over-range value.
        assert_eq!(
            parse("progress 300 uploading"),
            Some(Event::Progress {
                pct: Some(100),
                text: Some("uploading".to_string())
            })
        );
    }

    #[test]
    fn renderer_emits_nothing_before_first_event() {
        let mut renderer = Renderer::new();
        let mut buf = Vec::new();

        // A status line is a valid event but never renders — the renderer
        // (and, transitively, the reader thread's "did we ever render"
        // flag) must reflect that nothing was actually written yet.
        let wrote = renderer.handle(&Event::Status("working".to_string()), &mut buf);
        assert!(!wrote);
        assert!(buf.is_empty());

        // The first event that actually produces OSC bytes reports it via
        // the return value, which callers use to gate the final OSC.
        let wrote = renderer.handle(
            &Event::Progress {
                pct: Some(10),
                text: None,
            },
            &mut buf,
        );
        assert!(wrote);
        assert!(!buf.is_empty());
    }
}
