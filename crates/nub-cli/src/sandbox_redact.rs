//! Nub-side output redaction for `nub run --sandbox`.
//!
//! A granted secret's real value legitimately reaches the sandboxed child (the sandbox
//! confines fs/net/reads, not values a program is deliberately handed). But the child
//! writes to stdout/stderr that Nub forwards, so a debug log, stack trace, or
//! `console.log(process.env)` would otherwise persist the secret verbatim in the
//! terminal/CI log. This module scrubs known secret VALUES out of that byte stream
//! before Nub forwards it — the same model as a CI runner's log masking.
//!
//! It is accidental-leak prevention (log hygiene), NOT a containment boundary: a child
//! that WANTS to exfiltrate can (network, filesystem, an fd Nub isn't draining). The
//! secret values live ONLY here, in Nub's process — they are never handed to the sandbox
//! engine and never serialized into the IR (only a per-fd "pipe this" boolean crosses
//! that boundary).

use std::io::{self, Read, Write};
use std::sync::Arc;

/// Values shorter than this are never redacted: an empty value matches nothing, and a
/// 1–3-char value would scrub that fragment everywhere it happens to appear. A real
/// credential is longer; this trades a rare miss for far less false-positive noise.
const MIN_SECRET_LEN: usize = 4;

/// An immutable secret value → replacement-token table, plus the longest value length
/// (the chunk-boundary carry bound). Shared (`Arc`) across the two per-fd streamers.
struct SecretSet {
    /// `(value bytes, token bytes)`, sorted LONGEST value first so a value that is a
    /// substring of another is matched as the longer one.
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    /// `max(value.len())` — a partial match at a chunk's tail can extend at most this far.
    max_len: usize,
}

/// A built redaction table. Cheap to clone (shares the `Arc`); call [`stream`](Self::stream)
/// once per fd to get an independent streaming state.
#[derive(Clone)]
pub struct SecretRedactor {
    set: Arc<SecretSet>,
}

impl SecretRedactor {
    /// Build the table from a compiled policy: the value of every sensitive, non-brokered
    /// constructed key whose value is redaction-eligible (≥ [`MIN_SECRET_LEN`]). Returns
    /// `None` when nothing qualifies — the caller then leaves stdio inherited (zero cost,
    /// byte-identical to a no-secret run).
    pub fn build(policy: &nub_sandbox::SandboxPolicy) -> Option<Self> {
        let env = &policy.env;
        if env.sensitive_keys.is_empty() {
            return None;
        }
        let sensitive: std::collections::HashSet<&str> =
            env.sensitive_keys.iter().map(String::as_str).collect();
        // Brokered names are already absent from `constructed` (the compiler's
        // `withhold_brokered_env` strips them — the child holds an opaque marker, not the
        // real value). Excluded again here so a future change to that ordering can't leak
        // a brokered value into the scrub set.
        let brokered: std::collections::HashSet<&str> = policy
            .net
            .brokers
            .iter()
            .flat_map(|b| b.env.iter())
            .map(String::as_str)
            .collect();
        let named = env.constructed.iter().filter_map(|(name, value)| {
            let name = name.as_str();
            (sensitive.contains(name) && !brokered.contains(name)).then_some((name, value.as_str()))
        });
        Self::from_named_values(named)
    }

    /// Core builder over `(key-name, value)` pairs — the value-set logic, isolated from
    /// policy plumbing so it is unit-testable. Applies the min-length floor, resolves the
    /// per-value token (`<redacted:NAME>` for a single owner, bare `<redacted>` when a
    /// value is shared by several keys), and orders longest-value-first.
    fn from_named_values<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> Option<Self> {
        // value → owning key names (sorted/deduped map for deterministic tokens).
        let mut by_value: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for (name, value) in pairs {
            if value.len() < MIN_SECRET_LEN {
                continue;
            }
            by_value.entry(value).or_default().push(name);
        }
        if by_value.is_empty() {
            return None;
        }
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = by_value
            .into_iter()
            .map(|(value, names)| {
                let token = if names.len() == 1 {
                    format!("<redacted:{}>", names[0])
                } else {
                    // A value shared by several keys has no single owning name.
                    "<redacted>".to_string()
                };
                (value.as_bytes().to_vec(), token.into_bytes())
            })
            .collect();
        // Longest value first; then lexicographic for a stable order among equal lengths.
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        let max_len = pairs.iter().map(|(v, _)| v.len()).max().unwrap_or(0);
        Some(SecretRedactor {
            set: Arc::new(SecretSet { pairs, max_len }),
        })
    }

    /// A fresh per-fd streaming state (independent carry buffer, shared table).
    pub fn stream(&self) -> StreamRedactor {
        StreamRedactor {
            set: Arc::clone(&self.set),
            carry: Vec::new(),
        }
    }
}

/// Per-fd streaming redactor. Byte-oriented (binary-safe; arbitrary non-UTF-8 output
/// passes through unmangled) and correct across arbitrary read-chunk boundaries via a
/// bounded carry buffer of `< max_len` bytes.
pub struct StreamRedactor {
    set: Arc<SecretSet>,
    carry: Vec<u8>,
}

impl StreamRedactor {
    /// Feed a chunk. Emits every byte that cannot be the prefix of a secret straddling
    /// into the next chunk (all but a trailing `max_len - 1` window), holding the rest.
    fn push(&mut self, chunk: &[u8], out: &mut impl Write) -> io::Result<()> {
        self.carry.extend_from_slice(chunk);
        // A match STARTING before this boundary is fully contained in the buffer: start
        // `i < len - (max_len - 1)` ⇒ `i + max_len <= len`, and every value ≤ max_len.
        let boundary = self.carry.len().saturating_sub(self.set.max_len - 1);
        let consumed = self.scan(boundary, out)?;
        self.carry.drain(..consumed);
        out.flush()
    }

    /// EOF flush: replace + emit the entire remaining carry, no hold-back.
    fn flush(&mut self, out: &mut impl Write) -> io::Result<()> {
        let consumed = self.scan(self.carry.len(), out)?;
        self.carry.drain(..consumed);
        out.flush()
    }

    /// Scan `carry[..emit_limit]` left-to-right, writing literal runs in batches and
    /// replacing each matched secret (longest-first) with its token. Returns the count of
    /// leading carry bytes consumed; `carry[consumed..]` is deferred to the next feed.
    fn scan(&self, emit_limit: usize, out: &mut impl Write) -> io::Result<usize> {
        let buf = &self.carry;
        let mut i = 0;
        let mut run_start = 0;
        while i < emit_limit {
            let mut matched = false;
            for (val, tok) in &self.set.pairs {
                if buf.len() - i >= val.len() && &buf[i..i + val.len()] == val.as_slice() {
                    if run_start < i {
                        out.write_all(&buf[run_start..i])?;
                    }
                    out.write_all(tok)?;
                    i += val.len();
                    run_start = i;
                    matched = true;
                    break;
                }
            }
            if !matched {
                i += 1;
            }
        }
        if run_start < i {
            out.write_all(&buf[run_start..i])?;
        }
        Ok(i)
    }
}

/// Drain `reader` through a fresh streaming redactor to `sink` on a dedicated thread.
/// Returns the join handle; the caller joins AFTER the child is reaped so no tail output
/// is lost. A write error (e.g. a downstream `head` closing the pipe) surfaces via the
/// handle — the child's own exit status stays authoritative for the run's exit code.
pub fn spawn_drain<R, W>(
    mut reader: R,
    mut stream: StreamRedactor,
    mut sink: W,
) -> std::thread::JoinHandle<io::Result<()>>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            stream.push(&chunk[..n], &mut sink)?;
        }
        stream.flush(&mut sink)
    })
}

/// Attach redaction drains to a spawned child's piped fds, per the two flags. Returns the
/// drain-thread handles; join them with [`join_drains`] AFTER the child is reaped so no
/// tail output is dropped.
pub fn attach_drains(
    child: &mut nub_sandbox::PreparedChild,
    redactor: &SecretRedactor,
    redact_stdout: bool,
    redact_stderr: bool,
) -> Vec<std::thread::JoinHandle<io::Result<()>>> {
    let mut handles = Vec::new();
    if redact_stdout {
        if let Some(out) = child.take_stdout() {
            handles.push(spawn_drain(out, redactor.stream(), io::stdout()));
        }
    }
    if redact_stderr {
        if let Some(err) = child.take_stderr() {
            handles.push(spawn_drain(err, redactor.stream(), io::stderr()));
        }
    }
    handles
}

/// Join redaction drains best-effort: it never changes the run's exit code (the child's own
/// exit status is authoritative). A broken pipe is the normal "downstream closed early"
/// case (e.g. `… | head`) and is silent; any other error is reported.
pub fn join_drains(handles: Vec<std::thread::JoinHandle<io::Result<()>>>) {
    for h in handles {
        match h.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.kind() == io::ErrorKind::BrokenPipe => {}
            Ok(Err(e)) => eprintln!("warning: sandbox output redaction drain error: {e}"),
            Err(_) => eprintln!("warning: sandbox output redaction drain thread panicked"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a redactor from `(name, value)` pairs and run `input` through a single
    /// streaming pass fed one chunk at a time as `chunks` dictates, returning the output.
    fn redact_chunks(pairs: &[(&str, &str)], chunks: &[&[u8]]) -> Vec<u8> {
        let redactor =
            SecretRedactor::from_named_values(pairs.iter().map(|(n, v)| (*n, *v))).unwrap();
        let mut stream = redactor.stream();
        let mut out = Vec::new();
        for c in chunks {
            stream.push(c, &mut out).unwrap();
        }
        stream.flush(&mut out).unwrap();
        out
    }

    #[test]
    fn redacts_a_secret_value_with_its_name() {
        let out = redact_chunks(&[("SECRET", "abc123xyz")], &[b"token=abc123xyz done"]);
        assert_eq!(out, b"token=<redacted:SECRET> done");
    }

    #[test]
    fn redacts_a_value_split_across_a_chunk_boundary() {
        // "abc123xyz" is fed as "abc1" + "23xyz" — the carry buffer must still catch it.
        let out = redact_chunks(&[("SECRET", "abc123xyz")], &[b"x abc1", b"23xyz y"]);
        assert_eq!(out, b"x <redacted:SECRET> y");
    }

    #[test]
    fn longest_first_handles_a_value_that_is_a_substring_of_another() {
        // "abc123" is a substring of "abc123xyz"; the longer secret must win where both
        // could match, and the shorter one still redacts on its own.
        let out = redact_chunks(
            &[("SHORT", "abc123"), ("LONG", "abc123xyz")],
            &[b"[abc123xyz] and [abc123]"],
        );
        assert_eq!(out, b"[<redacted:LONG>] and [<redacted:SHORT>]");
    }

    #[test]
    fn skips_values_below_the_min_length_floor() {
        // "no" (2) and "abc" (3) are below the floor; "abcd" (4) redacts. A short value
        // being skipped means it is NOT built at all — here only ABCD qualifies.
        let out = redact_chunks(&[("SHORT", "abc"), ("OK", "abcd")], &[b"abc abcd"]);
        assert_eq!(out, b"abc <redacted:OK>");
    }

    #[test]
    fn leaves_non_secret_text_untouched() {
        let out = redact_chunks(&[("SECRET", "supersecret")], &[b"nothing to see here"]);
        assert_eq!(out, b"nothing to see here");
    }

    #[test]
    fn redacts_a_value_appearing_as_a_substring_of_a_larger_token() {
        let out = redact_chunks(&[("SECRET", "s3cr3t")], &[b"pre-s3cr3t-post"]);
        assert_eq!(out, b"pre-<redacted:SECRET>-post");
    }

    #[test]
    fn handles_multiline_output_and_a_value_spanning_a_newline() {
        let out = redact_chunks(&[("SECRET", "line1\nline2")], &[b"a=line1\nline2;b=1"]);
        assert_eq!(out, b"a=<redacted:SECRET>;b=1");
    }

    #[test]
    fn passes_non_utf8_bytes_through_and_still_matches() {
        let mut chunk = vec![0xff, 0xfe, b'k', b'e', b'y', b'='];
        chunk.extend_from_slice(b"abcd1234");
        chunk.push(0x00);
        let out = redact_chunks(&[("K", "abcd1234")], &[&chunk]);
        let mut expected = vec![0xff, 0xfe, b'k', b'e', b'y', b'='];
        expected.extend_from_slice(b"<redacted:K>");
        expected.push(0x00);
        assert_eq!(out, expected);
    }

    #[test]
    fn shared_value_across_keys_uses_the_bare_token() {
        // The same value under two keys has no single owner → bare <redacted>.
        let out = redact_chunks(
            &[("A", "sharedval"), ("B", "sharedval")],
            &[b"x sharedval y"],
        );
        assert_eq!(out, b"x <redacted> y");
    }

    #[test]
    fn empty_set_when_no_value_qualifies() {
        assert!(SecretRedactor::from_named_values([("A", "no")].into_iter()).is_none());
        assert!(SecretRedactor::from_named_values(std::iter::empty::<(&str, &str)>()).is_none());
    }
}
