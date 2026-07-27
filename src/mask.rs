//! Masking secret values out of pult's own output — the run journal
//! (`events.jsonl`) and the forwarded/rendered copies of a teed child's
//! stdout/stderr and `PULT_EVENTS` stream. See issue #7 (and its
//! scope-correcting comment) for the exposure this closes: `secret: true`
//! already hides a value from the `running:` banner and preview text
//! (`exec.rs`'s `redact`), but a journaled or teed run echoed it verbatim
//! wherever a script printed it back.
//!
//! [`Masker`] is the one type both the journal and the runner reach for.
//! It's deliberately sealed — see the type's own doc comment — because the
//! whole point of this fix is that a secret, once wrapped in one, can be
//! *replaced* in output but never *read back out* by callers.

use std::borrow::Cow;
use std::sync::Arc;

/// Values shorter than this are never masked. Masking `eu`, `dev`, `true`,
/// or `0` would shred unrelated output that merely happens to contain a
/// short secret's characters as a substring; this is the same floor CI
/// secret-masking (e.g. GitHub Actions) uses, and the only property of a
/// value usable to decide "mask this" without interpreting it.
pub const MIN_LEN: usize = 6;

/// The fixed, machine-recognizable marker a match is replaced with — already
/// the spec's literal for a redacted `Meta.params` value. Fixed-length (so
/// it never leaks how long the original value was) and greppable. This is a
/// different layer from `••••••`, pult's human-facing display rendering in
/// `exec.rs`'s `redact`/the `running:` banner — don't conflate the two.
const MARKER: &str = "<redacted>";

/// Replaces secret values with [`MARKER`] wherever they appear in journaled
/// or forwarded output.
///
/// **Sealed by design.** The only way to learn anything about the secrets a
/// `Masker` holds is to run text through [`Masker::apply`] /
/// [`Masker::apply_bytes`] and see where `<redacted>` came out — there is no
/// getter, no `Serialize` impl, and `Debug` never prints a value (just a
/// count). A derived `Debug` would defeat the whole point: it would print
/// every secret verbatim the moment a `Masker` ended up in a `{:?}` of some
/// enclosing struct during debugging.
///
/// Internally `Arc`-shared, so constructing one is a one-time cost (done
/// once in `exec.rs`, from the post-`fill` param values) and every clone —
/// the `Journal`, both `tee` threads, the events-reader closure — shares the
/// same secret set. No interior mutability and no `push` (yet): issue #6
/// (masking values discovered mid-run, not just supplied up front) will need
/// one, and the `Arc` is here so that lands as a swap of the shared value
/// rather than a signature change at every call site.
#[derive(Clone)]
pub struct Masker(Arc<Inner>);

struct Inner {
    /// Longest-first byte needles — see `Masker::new` for why both the
    /// ordering and the per-line split matter.
    needles: Vec<Vec<u8>>,
}

impl Masker {
    /// Build a masker from raw secret values. Each value is split on `\n`
    /// first — a multi-line secret (a PEM body, say) is masked line by line
    /// rather than as one contiguous span, the same way GitHub Actions masks
    /// multi-line secrets. This is a construction-time decision, not a
    /// buffering one: `tee` and `Journal::append_locked` each see one
    /// already-decoded chunk of text at a time and must decide on the spot,
    /// so there is no cross-line window here to hold a line back waiting for
    /// the rest of a secret to arrive — that would conflict with the
    /// journal-first liveness reasoning in `runner.rs` (see the comment
    /// above `tee`) and its bounded drain deadline, trading a documented,
    /// minor gap (a secret split by a line break in the *middle* of one of
    /// its lines can't be caught) for a new class of lost-record bugs.
    ///
    /// Segments under `MIN_LEN` are dropped silently here (`exec.rs` is
    /// responsible for warning about them — it's the only caller with the
    /// param name to name in the warning).
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Masker {
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for secret in secrets {
            for line in secret.split('\n') {
                if line.len() >= MIN_LEN {
                    needles.push(line.as_bytes().to_vec());
                }
            }
        }
        // Longest-first: if one secret's value is a substring of another's,
        // the longer needle must be tried first at each position, or the
        // shorter one would match inside it and leave a mangled remainder
        // (part marker, part leftover plaintext) instead of one clean match.
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        needles.dedup();
        Masker(Arc::new(Inner { needles }))
    }

    /// Mask `s`, returning it unchanged (`Cow::Borrowed`) when nothing
    /// matches — the common case on the hot path of every journaled line,
    /// where the caller already pays for serde serialization and a `flush()`
    /// syscall regardless.
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        match self.apply_bytes(s.as_bytes()) {
            Cow::Borrowed(_) => Cow::Borrowed(s),
            Cow::Owned(bytes) => {
                // SAFETY: every needle is byte-for-byte a substring of some
                // valid UTF-8 `&str` (a secret value or one `\n`-split line
                // of one), and `s` is valid UTF-8 by the type of this
                // parameter. Replacing an exact match of one valid-UTF-8
                // byte sequence with another (`MARKER`, plain ASCII) can
                // never land on or split a multi-byte boundary, so the
                // result is valid UTF-8 too.
                Cow::Owned(unsafe { String::from_utf8_unchecked(bytes) })
            }
        }
    }

    /// Byte-level masking — UTF-8 substrings are byte substrings, so
    /// matching on the decoded secret's raw bytes is exact even against
    /// invalid-UTF-8 or partial-line input (`tee`'s case). Naive
    /// substring search per needle: N is a command's secret-param count
    /// (single digits), and every call site already pays for a line's worth
    /// of I/O — a syscall (`tee`'s forward) or serde serialization plus a
    /// flush (the journal). Revisit if issue #6 grows the set to dozens of
    /// entries; an aho-corasick dependency is not worth taking for this.
    pub fn apply_bytes<'a>(&self, b: &'a [u8]) -> Cow<'a, [u8]> {
        if self.0.needles.is_empty() {
            return Cow::Borrowed(b);
        }
        let mut out: Option<Vec<u8>> = None;
        let mut i = 0;
        while i < b.len() {
            let hit = self
                .0
                .needles
                .iter()
                .find(|n| b[i..].starts_with(n.as_slice()));
            match hit {
                Some(needle) => {
                    out.get_or_insert_with(|| b[..i].to_vec())
                        .extend_from_slice(MARKER.as_bytes());
                    i += needle.len();
                }
                None => {
                    if let Some(out) = out.as_mut() {
                        out.push(b[i]);
                    }
                    i += 1;
                }
            }
        }
        match out {
            Some(v) => Cow::Owned(v),
            None => Cow::Borrowed(b),
        }
    }
}

impl std::fmt::Debug for Masker {
    /// Never the values — just how many needles are loaded. A derived
    /// `Debug` would print every secret the instant a `Masker` sat inside
    /// something logged with `{:?}`; this is the whole reason it's
    /// hand-written.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Masker {{ secrets: {} }}", self.0.needles.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_replaces_a_matching_secret_with_the_marker() {
        let masker = Masker::new(["hunter2000".to_string()]);
        assert_eq!(masker.apply("token=hunter2000!"), "token=<redacted>!");
    }

    #[test]
    fn apply_returns_borrowed_when_nothing_matches() {
        let masker = Masker::new(["hunter2000".to_string()]);
        let input = "nothing secret here";
        assert!(matches!(masker.apply(input), Cow::Borrowed(_)));
    }

    #[test]
    fn values_under_the_length_floor_are_never_masked() {
        let masker = Masker::new(["eu".to_string(), "0".to_string(), "true".to_string()]);
        assert_eq!(
            masker.apply("region=eu count=0 ok=true"),
            Cow::Borrowed("region=eu count=0 ok=true")
        );
    }

    #[test]
    fn multiline_secrets_are_masked_line_by_line() {
        let masker = Masker::new(["line-one-x\nline-two-y".to_string()]);
        let text = "before line-one-x middle line-two-y after";
        assert_eq!(
            masker.apply(text),
            "before <redacted> middle <redacted> after"
        );
    }

    #[test]
    fn overlapping_secrets_mask_longest_first() {
        // "ab-secret" contains "secret123" as... no, construct real overlap:
        // one value is a substring of another.
        let masker = Masker::new(["abc123short".to_string(), "abc123".to_string()]);
        assert_eq!(
            masker.apply("prefix abc123short suffix"),
            "prefix <redacted> suffix"
        );
    }

    #[test]
    fn apply_bytes_preserves_invalid_utf8_around_a_match() {
        let masker = Masker::new(["hunter2000".to_string()]);
        let mut input = vec![0xFFu8, b' '];
        input.extend_from_slice(b"hunter2000");
        input.push(0xFEu8);
        let masked = masker.apply_bytes(&input);
        let mut expected = vec![0xFFu8, b' '];
        expected.extend_from_slice(MARKER.as_bytes());
        expected.push(0xFEu8);
        assert_eq!(masked.into_owned(), expected);
    }

    #[test]
    fn debug_never_prints_the_secret_value() {
        let masker = Masker::new(["hunter2000".to_string()]);
        let debug = format!("{masker:?}");
        assert!(!debug.contains("hunter2000"));
        assert_eq!(debug, "Masker { secrets: 1 }");
    }
}
