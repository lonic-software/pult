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

// Apart from `inherited_channel` (which `exec.rs` asks once per run, on
// every target, and whose answer decides both the compiled step guards and
// the runner's channel handling), this module's callers are the journal
// (`journal.rs`'s `Event` import) and the unix-only events channel
// (`runner.rs`'s `events_unix`) — both gated off on non-unix by M4
// (journaling disabled entirely there, see `Journal::start`). So on a
// non-unix target most items below are compiled but unreachable; `allow`
// only there, never on unix (where this module is live and dead-code
// detection should stay fully armed). Expected to become vestigial once
// journaling reaches Windows (issue #8).
#![cfg_attr(not(unix), allow(dead_code))]

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

/// Whether `s` is a value `PULT_EVENTS` could legitimately hold: a bare
/// decimal fd number, no sign or surrounding whitespace, small enough to
/// be a real descriptor. Anything else (a path, `""`, `-1`, ` 3`, a
/// number too large to be a real fd) is rejected: bash's `>&word` redirects
/// to a *file* named `word` when `word` isn't a bare number, so an inherited
/// `PULT_EVENTS=events.log` would make every injected `step` guard truncate a
/// file by that name instead of writing to a channel.
pub fn is_valid_events_fd(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit())
        && s.parse::<u32>().is_ok_and(|n| n <= 4096)
}

/// What pult found in its own environment's `PULT_EVENTS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InheritedChannel {
    /// Unset: pult may own a channel itself.
    Absent,
    /// A valid fd number: someone upstream owns the channel. It is passed
    /// through to the child untouched, and pult injects no `step` lines onto
    /// it — only a channel's owner emits `step` — but names each step with a
    /// `status` line instead.
    Owned,
    /// Set, but not a bare fd number. The runner strips it from the child's
    /// environment and warns; pult then behaves as if it were unset.
    Invalid(String),
}

/// Read `PULT_EVENTS` once per run. `exec.rs` asks this before compiling and
/// hands the answer to the runner, so the choice of step guards and the
/// choice of passthrough come from one reading.
pub fn inherited_channel() -> InheritedChannel {
    match std::env::var_os("PULT_EVENTS") {
        None => InheritedChannel::Absent,
        Some(val) => {
            let val = val.to_string_lossy().into_owned();
            if is_valid_events_fd(&val) {
                InheritedChannel::Owned
            } else {
                InheritedChannel::Invalid(val)
            }
        }
    }
}

/// A `progress` percentage mapped onto the whole run: the band of the step
/// it arrived in, `((k-1)*100 + pct) / n`. A child reports `progress` as
/// 0–100 of its own work, which is 0–100 of the step that launched it; with
/// no step seen yet the run is one band and the value passes through. With
/// `pct` 0 this is also where step `k` begins, the value a `step` event
/// draws.
pub fn run_pct(step: Option<(u32, u32)>, pct: u8) -> u8 {
    match step {
        // `parse_step` admits only 1 <= k <= n; anything else is one band.
        Some((k, n)) if k >= 1 && k <= n => {
            ((u64::from(k - 1) * 100 + u64::from(pct)) / u64::from(n)) as u8
        }
        _ => pct,
    }
}

/// Renders parsed events as OSC 9;4 progress sequences (the ConEmu/Windows
/// Terminal/WezTerm/Ghostty progress protocol) to a writer — stderr, in the
/// plain CLI. Tracks the current step so a `progress` value is drawn within
/// that step's band (see `run_pct`), and every `step` keeps moving the bar.
#[derive(Default)]
pub struct Renderer {
    step: Option<(u32, u32)>,
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
                write_osc(out, 1, run_pct(self.step, *p));
                true
            }
            Event::Progress { pct: None, .. } => {
                write_osc(out, 3, 0);
                true
            }
            // Consumed, not rendered — `status` exists for richer surfaces.
            Event::Status(_) => false,
            Event::Step { k, n, .. } => {
                self.step = Some((*k, *n));
                write_osc(out, 1, run_pct(self.step, 0));
                true
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

    fn osc(buf: &[u8]) -> String {
        String::from_utf8_lossy(buf).into_owned()
    }

    #[test]
    fn progress_is_drawn_within_its_steps_band() {
        let mut renderer = Renderer::new();
        let mut buf = Vec::new();

        // Step 2 of 3 begins at (2-1)*100/3 = 33.
        renderer.handle(
            &Event::Step {
                k: 2,
                n: 3,
                name: "x".to_string(),
            },
            &mut buf,
        );
        assert_eq!(osc(&buf), "\x1b]9;4;1;33\x1b\\");

        // A child's 60% of step 2 is (100+60)/3 = 53 of the run — not the raw
        // 60, the step's start 33, or the unscaled 33+60 = 93.
        buf.clear();
        renderer.handle(
            &Event::Progress {
                pct: Some(60),
                text: None,
            },
            &mut buf,
        );
        assert_eq!(osc(&buf), "\x1b]9;4;1;53\x1b\\");
    }

    #[test]
    fn a_step_after_progress_still_moves_the_bar() {
        let mut renderer = Renderer::new();
        let mut buf = Vec::new();
        renderer.handle(
            &Event::Progress {
                pct: Some(90),
                text: None,
            },
            &mut buf,
        );

        // Explicit progress no longer silences milestones: step 3 of 4
        // draws its start, 50.
        buf.clear();
        let wrote = renderer.handle(
            &Event::Step {
                k: 3,
                n: 4,
                name: "y".to_string(),
            },
            &mut buf,
        );
        assert!(wrote);
        assert_eq!(osc(&buf), "\x1b]9;4;1;50\x1b\\");
    }

    #[test]
    fn progress_before_any_step_spans_the_whole_run() {
        let mut renderer = Renderer::new();
        let mut buf = Vec::new();
        renderer.handle(
            &Event::Progress {
                pct: Some(42),
                text: None,
            },
            &mut buf,
        );
        assert_eq!(osc(&buf), "\x1b]9;4;1;42\x1b\\");
    }

    #[test]
    fn run_pct_bands() {
        assert_eq!(run_pct(None, 42), 42);
        assert_eq!(run_pct(Some((1, 1)), 42), 42);
        assert_eq!(run_pct(Some((2, 3)), 60), 53);
        assert_eq!(run_pct(Some((1, 4)), 100), 25);
        assert_eq!(run_pct(Some((4, 4)), 100), 100);
        assert_eq!(run_pct(Some((3, 4)), 0), 50);
        // Never panics on a step `parse` would have refused.
        assert_eq!(run_pct(Some((0, 4)), 42), 42);
        assert_eq!(run_pct(Some((1, 0)), 42), 42);
        assert_eq!(run_pct(Some((5, 4)), 42), 42);
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

    #[test]
    fn accepts_plain_fd_numbers() {
        assert!(is_valid_events_fd("3"));
        assert!(is_valid_events_fd("0"));
        assert!(is_valid_events_fd("4096"));
    }

    #[test]
    fn rejects_a_file_path() {
        assert!(!is_valid_events_fd("events.log"));
        assert!(!is_valid_events_fd("/tmp/events.log"));
    }

    #[test]
    fn rejects_empty_string() {
        assert!(!is_valid_events_fd(""));
    }

    #[test]
    fn rejects_signs_and_whitespace() {
        assert!(!is_valid_events_fd("+3"));
        assert!(!is_valid_events_fd("-1"));
        assert!(!is_valid_events_fd(" 3"));
        assert!(!is_valid_events_fd("3 "));
    }

    #[test]
    fn rejects_a_number_too_large_to_be_an_fd() {
        assert!(!is_valid_events_fd("4097"));
        assert!(!is_valid_events_fd("99999999999999999999"));
    }
}
