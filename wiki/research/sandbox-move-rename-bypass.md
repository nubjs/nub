# Move/rename secret-relocation bypass — per-OS verdict

## The bypass class

A policy grants WRITE over a directory but relies on a per-path READ-DENY to protect a
secret inside it (e.g. `rw ./`, read-deny `./.env`). A child that can't read the secret
directly tries to relocate the bytes to a read-allowed path: `mv .env leaked.txt`,
replace a readable path with a symlink to the secret, or hardlink the secret to a new
name. The read-deny is keyed to a PATH; if the child can change the path the bytes live
at, it escapes the deny. Anthropic's SRT defends this on macOS with
`generateMoveBlockingRules` — `(deny file-write-unlink)` + `(deny file-write-create)` on
each protected path AND all ancestor dirs.

Every verdict below is EMPIRICAL: a differential fixture run against the REAL
`nub_sandbox::apply()` path on each OS, with a no-sandbox negative control proving the
test is live. Branch: `origin/sandbox-primitives`.

## Verdict summary

| OS | Vulnerable? | Mechanism |
|----|-------------|-----------|
| **macOS** (Seatbelt) | **YES — two holes** | (1) trailing confstr scratch grant overrides secret write-denies; (2) ancestor-dir rename escapes a path-anchored deny |
| **Linux** (Landlock) | **NO** | carve derivation withholds `REMOVE_FILE`/`REFER`/`MAKE_REG` on the secret's parent dir — no rename/link/create possible, and relocated bytes wouldn't be read-granted anyway |
| **Windows** (AppContainer) | **NO (class N/A)** | allowlist model, no per-file deny; rename preserves the object ACL and does not inherit the destination's AC grant; the read-deny-under-grant non-enforcement is already REPORTED as `fs-read-deny` degradation |

The reviewer's original macOS theory ("emits only read/write denies, no move-block") was
IMPRECISE. `(deny file-write* <path>)` DOES include `file-write-unlink`, so it genuinely
blocks a direct `mv` of the file in the ordinary case. The real macOS holes are narrower
and were found only by running the fixture, not by reading the emitter.

---

## macOS — VULNERABLE (empirically confirmed)

### Repro matrix (raw SBPL mirroring nub's real emission, `sandbox-exec`)

Secret `TOPSECRET_abc123` in `proj/.env`; generous read + `.env` read-deny + rw region.

| Case | Location | Deny shape | mv file | hardlink | ancestor-dir mv |
|------|----------|-----------|---------|----------|-----------------|
| 1 | UNDER `$TMPDIR` (`/var/folders/…/T`) | `**/.env` | **LEAK** | blocked | — |
| 2 | outside temp (`/private/tmp/…`) | `**/.env` | blocked | blocked | blocked (basename immune) |
| 3 | outside temp | literal `<root>/proj/.env` | blocked | blocked | **LEAK** |

Negative control (no sandbox): direct read leaks → test is live.

### Hole #1 — the confstr scratch grant overrides secret write-denies

`emit_fs` (macos.rs) emits, LAST and UNCONDITIONALLY, a
`(allow file-write* (subpath "<DARWIN_USER_TEMP_DIR>"))` grant so the Apple toolchain can
write `xcrun_db`. SBPL is last-match-wins, so this trailing allow OVERRIDES the earlier
`(deny file-write* <.env>)` for any secret living under `/var/folders/…/T`. The child can
then `mv proj/.env proj/leaked.txt` (unlink of `.env` is re-permitted) and read
`leaked.txt` (generous read covers it). **VULNERABLE.**

This is NOT strictly bounded away from the default: even a plain `sandbox: true` (no
explicit rw grant) emits the confstr temp grant, so a project or secret that lives under
`$TMPDIR` leaks via `mv`. Confirmed empirically — default `sandbox: true` with the secret
under `/var/folders/…/T` leaks; the same default with the secret in `/private/tmp` (no
write grant anywhere) is safe.

### Hole #2 — ancestor-directory rename escapes a path-anchored deny

With a LITERAL deny (`<root>/proj/.env`), the direct file `mv` is blocked (write-deny on
the file holds), but `mv proj proj2` succeeds — `proj` is rw-granted and not itself
denied — relocating the secret to `proj2/.env`, which no longer matches the literal
`<root>/proj/.env` deny → readable. **VULNERABLE for anchored user denies.** nub's
BUILT-IN secret denies are basename-globs (`**/.env`, `**/.env.*`, `**/.envrc`), which are
IMMUNE to ancestor rename (the basename stays `.env`), so this hole only bites
USER-authored path-anchored denies like `!./secrets/prod.key`.

**Scope refinement (2026-07-09, from the fix's own adversarial + correctness review):** the
"basename-glob immune" line above is TOO BROAD. Only basename-ONLY *file* globs (`**/.env`,
`**/*.key`) are immune — a glob that PINS an ancestor DIRECTORY name is NOT, because renaming
that directory relocates the secret out from under the pin. Concretely, these user denies each
LEAK via an ancestor rename (empirically confirmed): `!secrets/*.key` (`mv secrets secrets2`),
`!packages/*/.env` monorepo (`mv packages pkgs`), `!**/secrets/*.key` and `!**/secrets/**`
(`mv secrets safe` at any depth — the `(.*/)?` float re-absorbs ancestors ABOVE `secrets`, so
only renaming the pinned `secrets` component escapes). The mechanistic cause: these patterns
compile to `MatchTerm::Regex`, and the applied Fix 2 walks ancestors ONLY for `MatchTerm::Subpath`
(literal) denies — every regex-classified deny is skipped, so its pinned ancestor dirs get no
move-block. Default posture stays SAFE (built-ins are basename-only file globs), and an anchored
literal or `dir/**`-subtree user deny canonicalizes to `Subpath` and IS covered; the gap is
specifically regex-classified directory-pinning globs. See the fix section below.

### The fix — APPLIED (Fix 1 + Fix 2-literal landed on `sandbox-primitives`, unmerged)

**Status (2026-07-09):** `emit_move_block` in `backend/macos.rs` implements Fix 1 (re-assert each
`Deny` arm's `file-write-unlink`/`file-write-create` after the confstr grant) and Fix 2 for
`MatchTerm::Subpath` (literal) denies (walk ancestor dirs up to the enclosing write-grant root,
where grant roots = surviving rw-Allow subpaths + the confstr scratch dirs, so a `$TMPDIR`-resident
literal-anchored secret is covered). Committed macOS-gated integration tests (`tests/macos_moveblock.rs`)
reproduce both holes and the non-regressions through the real `apply()` path. Landed in the reviewable
PR (NOT merged) — security-posture, pending maintainer sign-off.

**Recommended follow-up (maintainer scope call): extend Fix 2 to regex-classified directory-pinning
denies.** Per the scope refinement above, Fix 2 currently skips every `MatchTerm::Regex` deny, leaving
`!secrets/*.key` / `!packages/*/.env` / `!**/secrets/**` relocatable by an ancestor rename. Closing it
means, for a regex deny, deriving the rename-dangerous ancestor DIRECTORY nodes from the glob and
move-blocking them: for the fixed-depth absolute-literal prefix, `(literal <dir>)` on each ancestor up
to the grant root (as Fix 2 already does for `Subpath`); for a `**/`-floated pinned literal component,
a `(regex ^<abs-prefix>/(.*/)?<pinned-run>$)` on the dir node (the float re-absorbs everything above,
so only the pinned component needs a guard). The posture nuance the maintainer owns: BLOCK these
(consistent with the applied fix) vs. surface them as a reported `fs-read-deny` degradation. Not
default-reachable (built-ins immune); the derivation carries over-block risk, so it wants its own
reviewed pass, not a reflexive bolt-on.

### The originally-designed fix (proven raw-SBPL fragments; see APPLIED status above)

Emit SRT-style move-blocking denies AFTER the confstr grants, so they survive the
last-match-wins override. Both SBPL fragments below were empirically validated (the mv is
blocked, legit temp scratch writes still succeed).

**Fix 1 (closes hole #1) — re-assert the deny-arm write-denies after the confstr loop.**
Append, immediately after the `for dir in confstr_scratch_dirs()` loop in `emit_fs`:

```rust
    // Re-assert secret write-denies AFTER the confstr scratch grant so the trailing
    // `(allow file-write* <temp>)` can't re-open write (hence unlink/rename) on a denied
    // path that lives under the DARWIN temp dir — the move/rename relocation bypass. Only
    // the Deny entries are re-emitted (NOT the read-only-allow `/` write-cap), so a
    // non-secret temp scratch write (xcrun_db) stays permitted.
    for rule in &policy.fs.rules.entries {
        if rule.effect == Effect::Deny {
            let term = emit_term(&to_match_term(rule.matcher.as_str()));
            out.push_str(&format!("(deny file-write-unlink {term})\n"));
            out.push_str(&format!("(deny file-write-create {term})\n"));
        }
    }
```

(Re-emitting only the `Deny` arm — never the `(Effect::Allow, FsAccess::Read)` generous-`/`
write-cap — is load-bearing: re-emitting the `/` cap after confstr would re-deny the whole
temp dir and break the `xcrun_db` write. Empirically: `.env` mv blocked, `echo > $TMPDIR/x`
still OK.)

**Fix 2 (closes hole #2) — ancestor move-block for anchored denies.** For each `Deny`
entry with a concrete literal directory prefix, deny `file-write-unlink` +
`file-write-create` on every ancestor directory from the denied path's parent up to the
outermost enclosing write-grant root. A basename-glob deny (`**/.env`) has no literal
ancestors and needs nothing (already immune). Sketch:

```rust
// For a literal-prefixed deny `/root/proj/.env` (or `/root/secrets/**`), walk parents:
//   /root/proj, /root  → for each: (deny file-write-unlink (literal P))
//                                  (deny file-write-create (literal P))
// stopping at (and including) the outermost rw-grant root that contains the deny.
```

Empirically, `(deny file-write-unlink (literal <ancestor>))` on each ancestor blocks both
`mv proj proj2` and `mv proj/b proj/b2`, while `echo > proj/other.txt` still works.

The unified mechanism is exactly SRT's `generateMoveBlockingRules`, with the nub-specific
twist that it must be emitted AFTER the confstr grant (SRT has no equivalent trailing
scratch allow, so ordering is nub's own gotcha).

### Default bound (macOS)

The hole opens only for a policy that grants write over a region CONTAINING a secret —
the explicit `rw`-over-secret shape, PLUS the confstr-temp case where the "region" is
`$TMPDIR` even under a plain `sandbox: true`. A `sandbox: true` project outside `$TMPDIR`
grants no write anywhere → nothing to relocate → SAFE. Severity: real for the rw-over-secret
policy shape and for any secret under `$TMPDIR`; not a default-everywhere break.

---

## Linux — NOT VULNERABLE (empirically confirmed on real kernel)

Ubuntu 24.04, kernel 6.8 (Lima `landlock-vm`, aarch64), real Landlock+seccomp
`apply()` path. Every vector blocked; negative control leaks.

```
Scenario A (rw <root>/**, read-deny **/.env, secret proj/.env):
  control direct read           safe  (Permission denied)
  (a) mv within dir             safe  (mv: Permission denied)
  (a2) mv to other writable dir safe  (mv: Permission denied)
  (b) symlink                   safe  (ln: Permission denied)
  (c) hardlink                  safe  (ln: Permission denied)
  (d) ancestor dir rename       safe  (mv: Permission denied)
  (e) cp                        safe  (cp: cannot open for reading)
  (f) create NEW file in dir    safe  (cannot create proj/new.txt)   <-- carved dir is not mutable
  neg-control no-sandbox        leaked=true
Scenario B (literal <root>/proj/.env deny): all four vectors safe.
```

### Why Linux is structurally immune

Landlock is ALLOW-ONLY and hierarchy-based — it has no per-file deny. nub's grant
DERIVATION (`linux_grants::derive_read_grants` / `derive_write_grants`) turns a deny-inside-
a-grant into a CARVE: the secret's parent dir `proj` is granted only `ReadDir` (listable),
NOT `ReadSubtree`, and the write carve grants `WriteSubtree` only on individually-allowed
FILES — never on the carved dir itself. Landlock governs rename/unlink/link/create by the
PARENT directory's rights (`REMOVE_FILE`, `REFER`, `MAKE_REG`), and the carve deliberately
withholds all of them on `proj`. So the child cannot rename, unlink, hardlink,
symlink-create, or even create a new file in the carved dir (test (f) confirms the
"new entry in a carved directory is not grantable" fail-safe). Two independent properties
close the class: (1) the carved dir is not mutable; (2) even a hypothetically relocated
file would not be read-granted (only `ReadDir`, no `ReadSubtree`). Stronger than macOS by
construction. `$TMPDIR` has no analog of the macOS confstr grant on Linux.

Default bound: `sandbox: true` grants no write at all → immune a fortiori.

---

## Windows — NOT VULNERABLE (class does not apply; code-grounded)

AppContainer LowBox with a pure ALLOWLIST model (`backend/windows.rs`). There are NO
per-file deny-ACEs (the deny-ACE denylist is explicitly abandoned — module header). A
secret is protected only by EXCLUSION (never granted). The move/rename class does not apply:

- **Rename preserves the object ACL.** A move within a volume keeps the file's own ACL and
  does NOT pick up the destination dir's inheritable AC-SID grant (only a COPY inherits
  destination ACEs). So relocating a secret INTO a granted dir does not make it readable by
  the LowBox child.
- **The child can't operate on an ungranted source** — no read/traverse/delete on a file
  outside its grants, so it cannot move/hardlink/copy the secret in the first place.
- **The one in-grant shape (rw `./` + read-deny `.env`-inside) is already a NON-protection
  that nub REPORTS.** `deny_shadows_grant` detects a deny landing inside a granted subtree
  and surfaces `fs-read-deny` degradation ("inheritable allow wins — deny not enforced").
  The secret is directly readable (`cat .env`), no rename needed — but this is honestly
  reported as reduced-mode, not a silent bypass. There is nothing for a rename to
  circumvent.

Residual test to run if desired (real windows-latest CI, not required for the verdict):
confirm that `mv secret out\` of an ungranted file fails, and that a moved file does not
inherit a destination AC grant. Both follow from Windows ACL semantics + the allowlist
model; the code path leaves no room for a per-file-deny relocation bypass because there are
no per-file denies.

Default bound: `sandbox: true` degrades to the explicit allow-set (generous-read is not
expressible), so it is MORE restrictive, not less — no write-over-secret relocation.

---

## Changelog

- 2026-07-09 — Initial write-up. macOS VULNERABLE (confstr-override + anchored-ancestor-rename),
  both fixes empirically validated; Linux empirically SAFE on real kernel 6.8; Windows class
  N/A (allowlist model, reported degradation). Corrected the reviewer's imprecise macOS theory:
  `(deny file-write*)` does block a direct file rename; the holes are the trailing confstr grant
  and ancestor-dir rename of anchored denies.
- 2026-07-09 — Fix APPLIED to `backend/macos.rs` (`emit_move_block`): Fix 1 + Fix 2 for
  `MatchTerm::Subpath` (literal) denies, with committed integration tests; landed on
  `sandbox-primitives` (unmerged, pending sign-off). **Scope refinement:** the fix's own
  adversarial + correctness review found the "basename-glob immune" claim was too broad — Fix 2
  skips ALL `MatchTerm::Regex` denies, so regex-classified directory-pinning globs
  (`!secrets/*.key`, `!packages/*/.env`, `!**/secrets/**`) remain relocatable by an ancestor
  rename (three leaks confirmed empirically). Default posture still SAFE (built-ins are
  basename-only file globs). Recorded the recommended regex-deny extension + the block-vs-report
  posture call for the maintainer (see the fix section).
