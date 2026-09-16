//! Host-side output redaction for `nub sandbox`.
//!
//! A policy's `secrets` are handed to the child in its constructed env, so a command that echoes
//! one — `printenv`, a crash dump, a verbose HTTP log — would print the secret straight to the
//! user's terminal. When any secret is declared, [`run_confined`](crate::sandbox_run) pipes the
//! child's stdout/stderr and drains them through here: every occurrence of a declared secret VALUE
//! becomes a fixed token, and the rest forwards as it arrives. `vars` are never redacted — that
//! split is exactly what lets scrubbing default on for secrets and off for vars.
//!
//! Streaming, not buffered: a secret split across two `read` chunks is still caught, because the
//! redactor holds back a tail of up to `max_secret_len - 1` bytes between chunks. Byte-oriented,
//! not line-oriented, so a `\r` progress bar that never emits a newline still streams live.
//!
//! Two limits, both inherent and accepted. Piping the fds means the child no longer sees a TTY, so
//! color and interactivity degrade whenever secrets are declared — the price of seeing the bytes
//! at all. And a secret emitted in a DIFFERENT ENCODING (base64, URL-escaped, JSON-quoted) is not
//! matched: only the literal value is known here, a limit every masker shares.

use std::io::{Read, Write};
use std::process::ExitStatus;

use nub_sandbox::PreparedChild;

/// What a scrubbed secret is replaced with. ASCII and greppable so a user who sees it in output
/// knows redaction fired rather than the command misbehaving.
const TOKEN: &[u8] = b"[redacted]";

/// Read granularity. Larger than a pipe's default buffer, so a burst is drained in few syscalls.
const CHUNK: usize = 16 * 1024;

/// A streaming replacer that scrubs a fixed set of secret byte-strings from a byte stream while
/// forwarding everything else. One instance drains one stream (stdout or stderr).
pub(crate) struct StreamRedactor {
    /// Secrets as bytes, sorted longest-first so a secret that contains a shorter one wins the
    /// match at a given position (no fragment left behind).
    secrets: Vec<Vec<u8>>,
    /// Longest secret length; the boundary-carry window is `max_len - 1`.
    max_len: usize,
    /// Bytes held back from the previous chunk that could begin a secret spanning the boundary.
    carry: Vec<u8>,
}

impl StreamRedactor {
    pub(crate) fn new(mut secrets: Vec<Vec<u8>>) -> Self {
        secrets.retain(|s| !s.is_empty());
        // Longest first so `match_at` returns the longest secret matching at a position; the
        // secondary byte order only makes `dedup` (which drops adjacent equals) total.
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        let max_len = secrets.first().map_or(0, Vec::len);
        Self {
            secrets,
            max_len,
            carry: Vec::new(),
        }
    }

    /// Feed one chunk: write the scrubbed, boundary-safe prefix and retain the tail.
    fn push(&mut self, chunk: &[u8], out: &mut impl Write) -> std::io::Result<()> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let (emit, keep) = self.scrub_prefix(&buf);
        out.write_all(&emit)?;
        out.flush()?;
        self.carry = keep;
        Ok(())
    }

    /// At EOF, scrub and flush whatever is still held back.
    fn finish(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let buf = std::mem::take(&mut self.carry);
        let emit = self.replace_all(&buf);
        out.write_all(&emit)?;
        out.flush()
    }

    /// Replace every COMPLETE secret occurrence, then split into (emit, carry). The carry is the
    /// unemitted tail: any byte within `max_len - 1` of the end could begin a secret that only
    /// completes in the next chunk, so it is held back. A complete match is always scrubbed first,
    /// wherever it sits — the guard only stops copying non-matching bytes that might be a prefix.
    fn scrub_prefix(&self, buf: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut out = Vec::with_capacity(buf.len());
        let guard = buf.len().saturating_sub(self.max_len.saturating_sub(1));
        let mut i = 0;
        while i < buf.len() {
            if let Some(hit) = self.match_at(buf, i) {
                out.extend_from_slice(TOKEN);
                i += hit;
                continue;
            }
            if i >= guard {
                break;
            }
            out.push(buf[i]);
            i += 1;
        }
        (out, buf[i..].to_vec())
    }

    /// The length of the longest secret matching `buf` at `i`, if any. `secrets` is sorted
    /// longest-first, so the first hit is the longest.
    fn match_at(&self, buf: &[u8], i: usize) -> Option<usize> {
        self.secrets
            .iter()
            .find(|s| buf[i..].starts_with(s))
            .map(Vec::len)
    }

    /// Replace every complete secret occurrence with no boundary guard — used only at EOF, where
    /// there is no next chunk for a partial to complete in.
    fn replace_all(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if let Some(hit) = self.match_at(buf, i) {
                out.extend_from_slice(TOKEN);
                i += hit;
            } else {
                out.push(buf[i]);
                i += 1;
            }
        }
        out
    }
}

/// Drain a confined child's piped stdout/stderr through the redactor, forwarding to the host's
/// real streams, and return the child's exit status. Each fd is read on its own thread so a full
/// pipe on one never blocks the other or the child, and both are drained concurrently with
/// `wait()` — the deadlock the inline `status()` path warns about.
pub(crate) fn drain_confined(
    mut child: PreparedChild,
    secret_values: Vec<Vec<u8>>,
) -> std::io::Result<ExitStatus> {
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();

    let mut pumps = Vec::new();
    if let Some(reader) = stdout {
        let redactor = StreamRedactor::new(secret_values.clone());
        pumps.push(std::thread::spawn(move || {
            pump(reader, redactor, &mut std::io::stdout())
        }));
    }
    if let Some(reader) = stderr {
        let redactor = StreamRedactor::new(secret_values.clone());
        pumps.push(std::thread::spawn(move || {
            pump(reader, redactor, &mut std::io::stderr())
        }));
    }

    // Reap the child while the pumps drain; the pipes hit EOF when it exits, ending the pumps.
    let status = child.wait()?;
    for handle in pumps {
        // A pump only errors if the host's own terminal write fails; nothing to recover, and the
        // exit status is what the caller needs.
        let _ = handle.join();
    }
    Ok(status)
}

fn pump(
    mut reader: impl Read,
    mut redactor: StreamRedactor,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let mut buf = [0u8; CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => redactor.push(&buf[..n], out)?,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    redactor.finish(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a redactor over a sequence of chunks and return everything it emitted.
    fn run(secrets: &[&str], chunks: &[&str]) -> Vec<u8> {
        let mut r = StreamRedactor::new(secrets.iter().map(|s| s.as_bytes().to_vec()).collect());
        let mut out = Vec::new();
        for chunk in chunks {
            r.push(chunk.as_bytes(), &mut out).unwrap();
        }
        r.finish(&mut out).unwrap();
        out
    }

    #[test]
    fn scrubs_a_secret_within_one_chunk() {
        assert_eq!(run(&["SECRET"], &["xSECRETy"]), b"x[redacted]y");
    }

    #[test]
    fn scrubs_a_secret_split_across_two_chunks() {
        // "SEC" ends chunk 1, "RET" opens chunk 2 — the boundary case the carry window exists for.
        assert_eq!(run(&["SECRET"], &["abcSEC", "RETxyz"]), b"abc[redacted]xyz");
    }

    #[test]
    fn scrubs_a_secret_split_one_byte_at_a_time() {
        let out = run(&["TOKEN"], &["a", "T", "O", "K", "E", "N", "b"]);
        assert_eq!(out, b"a[redacted]b");
    }

    #[test]
    fn scrubs_consecutive_occurrences() {
        assert_eq!(run(&["AB"], &["ABAB"]), b"[redacted][redacted]");
    }

    #[test]
    fn longest_overlapping_secret_wins() {
        // With both "ABC" and "AB" registered, "ABC" must match so no stray "C" survives.
        assert_eq!(run(&["AB", "ABC"], &["xABCy"]), b"x[redacted]y");
    }

    #[test]
    fn passes_non_secret_output_through_unchanged() {
        assert_eq!(
            run(&["SECRET"], &["nothing to see here\n"]),
            b"nothing to see here\n"
        );
    }

    #[test]
    fn empty_secret_values_are_ignored() {
        // An empty secret would match at every position; it must be dropped, leaving only "REAL".
        assert_eq!(run(&["", "REAL"], &["aREALb"]), b"a[redacted]b");
    }

    #[test]
    fn no_secrets_passes_everything_through() {
        // A redactor with no (or only empty) secrets forwards its input verbatim.
        assert_eq!(run(&[], &["anything at all\n"]), b"anything at all\n");
        assert_eq!(run(&[""], &["also untouched"]), b"also untouched");
    }

    #[test]
    fn a_trailing_partial_that_never_completes_is_flushed_verbatim() {
        // "SEC" at the very end is a partial of "SECRET"; at EOF it must be emitted, not eaten.
        assert_eq!(run(&["SECRET"], &["done SEC"]), b"done SEC");
    }

    #[test]
    fn multiple_distinct_secrets_are_all_scrubbed() {
        assert_eq!(
            run(&["ALPHA", "BRAVO"], &["ALPHA and BRAVO"]),
            b"[redacted] and [redacted]"
        );
    }
}
