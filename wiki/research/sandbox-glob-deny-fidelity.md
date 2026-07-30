# Sandbox glob→matcher DENY-path fidelity audit

**Scope.** Every glob construct a user can write in an `fs` deny (`!<glob>`) that a
backend SILENTLY UNDER-ENFORCES — the user believes a secret is denied, but the
kernel-level enforcement lets it be read. Silent under-enforcement is the leak class
this audit hunts; over-enforcement (breakage) and fail-closed compile errors are
recorded but are not the target. Investigation-scope: characterize + verify, land no
fix.

**Backends audited:** macOS Seatbelt (SBPL), Linux Landlock, Windows AppContainer.

**The enforcement architecture (why a backend can diverge from the intended
semantics).** nub's *intended* fs semantics is a `globset` matcher — `PathMatcher`
(`matcher/path.rs`), used by the conformance harness (`conformance.rs`) and the
cross-tier layering (`compiler/layering.rs`). `globset` supports `*`, `?`, `[…]`,
`**`, AND `{a,b}` brace alternation. But NONE of the three OS backends enforces
through `PathMatcher` at runtime — each translates the IR globs into its own kernel
primitive with its OWN fidelity:

- **macOS** translates each glob to an anchored SBPL regex (`glob_to_seatbelt_regex`
  in `backend/macos.rs`) — Seatbelt has no glob syntax. This is a per-construct
  *translation*, and any construct the translator mishandles diverges from `globset`.
- **Linux** decides each grant by walking the real tree and calling `globset`
  (`View::decide`/`allows` in `backend/linux_grants.rs`) — so the actual file verdict
  is `globset`-faithful; the only translation is the coarse `literal_prefix` /
  `glob_reaches_under` prefix analysis that decides *whether to carve a subtree*.
- **Windows** uses a default-deny allowlist ACL model; a read deny inside a granted
  subtree cannot be expressed as an inheritable deny-ACE, so the backend REPORTS a
  `Degradation` (`fs-read-deny`) instead of enforcing — a fail-safe, not silent.

## Verdict summary

| Construct (in a `!` deny) | globset (intended) | macOS Seatbelt | Linux Landlock | Windows AppContainer |
|---|---|---|---|---|
| `*`, `?`, `[abc]`, `[a-z]`, `[!x]` | deny | **enforced** | enforced | (see below) |
| `{a,b}` brace alternation | deny a, deny b | **enforced** (fixed) | enforced | (see below) |
| `{a,{b,c}}` nested | deny | **enforced** (fixed) | enforced | (see below) |
| `{a,b}/x` dir-level | deny | **enforced** (fixed) | enforced | (see below) |
| `{a,b}/*.k` brace+star | deny | **enforced** (fixed) | enforced | (see below) |
| `{a}` single-element | deny a | **enforced** (fixed) | enforced | (see below) |

The headline finding: **on macOS, brace alternation `{…}` in an fs deny is silently
inert** — the deny compiles without error or warning, nub's own userspace matcher
honors it, but the Seatbelt profile matches a file literally NAMED `{a,b}.key` and
never `a.key`/`b.key`, so the "denied" secrets are readable. Every other tested
construct (`*`, `?`, character classes `[…]`/`[a-z]`/`[!x]`) IS enforced on macOS.

## Root cause (macOS)

`glob_to_seatbelt_regex` (`backend/macos.rs`) special-cases `*`, `?`, `[`, `]` but
falls through `{`/`}` to `regex_escape_char`, which escapes them as LITERALS (`\{`,
`\}`). No brace expansion happens. A deny `!<D>/secrets/{a,b}.key` compiles to the
SBPL term:

```
(regex #"^<D>/secrets/\{a,b\}\.key(/.*)?$")
```

(Because the pattern contains no `*?[`, `saw_glob` stays false and the subtree
suffix `(/.*)?` is appended.) That regex matches a path component literally spelled
`{a,b}.key` — a filename almost no one creates — and never `a.key` or `b.key`. The
`to_match_term` router treats `{`/`}` as "meta" (so the glob is NOT taken as a literal
subpath) and hands it to the regex path, where the braces then die as literals. The
compiler (`compiler/fold.rs`, `subtree_globs`) passes a brace glob through unexpanded
into the `CanonGlob` IR, and nothing rejects or warns.

**Intended semantics confirmed independently.** A standalone `globset` check (the same
crate + `literal_separator(true)` flag nub's `PathMatcher` uses) proves nub's own
userspace matcher expands braces — the EXACT INVERSE of the Seatbelt behavior:

```
glob "/d/secrets/{a,b}.key":  a.key => true   b.key => true   c.key => false   {a,b}.key => false
glob "/d/p/{a}.key":          a.key => true
```

So a user's `!secrets/{a,b}.key` deny means "deny a.key and b.key" everywhere in nub
EXCEPT the macOS kernel gate, which denies only a file literally named `{a,b}.key`.

## Reproduction (macOS — verified 2026-07-09)

Built `nub` + `nub-sandbox-probe` (fast profile) from branch `sandbox-primitives`
(worktree `glob-deny-audit`). Probe: `nub run --sandbox <pol.json> <probe> read
<abs-file>` → exit 0 = READABLE (leak), 7 = DENIED. Policy shape:
`{ "fs": ["<D>", "!<D>/<deny-glob>"] }` over a temp dir `<D>` with planted secrets.

```
### positive controls — the sandbox IS active and selective
control-allow-pub      deny=secrets/x.pem          probe=pub.txt        => READABLE  (allow works)
control-literal-deny   deny=secrets/plain.key      probe=plain.key      => DENIED    (literal deny enforced)
control-star-deny      deny=secrets/*.pem          probe=x.pem          => DENIED    (* deny enforced)
control-star-nomatch   deny=secrets/*.pem          probe=a.key          => READABLE  (correct non-match)
### brace class — ALL leak
brace-alt-a            deny=secrets/{a,b}.key       probe=a.key          => READABLE  *** LEAK ***
brace-alt-b            deny=secrets/{a,b}.key       probe=b.key          => READABLE  *** LEAK ***
brace-nested           deny=secrets/{a,{b,c}}.key   probe=c.key          => READABLE  *** LEAK ***
brace-dir-level        deny=secrets/{a,b}/x.key     probe=secrets/a/x.key => READABLE *** LEAK ***
brace-plus-star        deny=secrets/{a,b}/*.key     probe=secrets/b/x.key => READABLE *** LEAK ***
brace-suffix           deny=p/{a,b}.key             probe=p/a.key        => READABLE  *** LEAK ***
brace-single-elem      deny=p/{a}.key               probe=p/a.key        => READABLE  *** LEAK ***
### other constructs — enforced
charclass              deny=secrets/[abc].key       probe=a.key          => DENIED    OK
charrange              deny=secrets/[a-z].key       probe=m.key          => DENIED    OK
charclass-neg          deny=secrets/[!x].key        probe=a.key          => DENIED    OK
qmark                  deny=secrets/?.key           probe=a.key          => DENIED    OK
```

The controls are load-bearing: `control-star-deny` denies `x.pem` in the SAME dir that
`brace-alt-a` reads `a.key` from, so the sandbox is provably active and selectively
carving — the brace files leak because the brace deny is inert, not because enforcement
is off.

## Refuted candidates (things that are NOT leaks)

- **macOS case-insensitivity.** Hypothesis: the SBPL regex is emitted without an
  `(?i)` flag while `globset` compiles case-insensitive on macOS, so `!**/.env`
  wouldn't block `.ENV`. REFUTED empirically: `.ENV` and `.env` are both DENIED. The
  macOS kernel resolves the vnode to its canonical on-disk case before Seatbelt
  regex-matches, so the case-variant spelling is normalized away before matching.
- **Character classes / ranges / negation / `?`.** All enforced on macOS (table above)
  — the translator maps them faithfully to regex.

## Linux (Landlock) — *pending VM confirmation*

Source analysis predicts braces are ENFORCED on Linux for existing files: `View::decide`
/ `allows` use `compile_glob` (globset), so the per-file verdict expands braces
correctly; `glob_reaches_under` is conservative (returns true for relative/whole-fs
globs and for any literal-prefix subtree overlap), so the carve is triggered rather than
a whole-subtree grant. To be confirmed empirically on `nub-linux`.

## Windows (AppContainer) — *pending VM confirmation*

Source analysis predicts Windows does NOT silently leak: a read deny inside a granted
subtree is reported as a `Degradation` (`fs-read-deny`, `deny_shadows_grant` in
`backend/windows.rs`), and `literal_prefix` splits at the first metachar (incl. `{`), so
a brace deny is treated as reaching under its literal prefix. If READABLE, it should be
READABLE-with-a-reported-degradation (fail-safe), not silent. To be confirmed on
`nub-win`.

## Resolution (2026-07-09)

Fixed per the decided per-axis rule (branch `glob-braces`, PR into `sandbox-primitives`):

- **fs globs (allow + deny) SUPPORT braces.** `glob_to_seatbelt_regex` now expands
  `{a,b}` → regex alternation `(a|b)` (new `brace_to_regex` + `translate_unit` helpers),
  matching globset. Nested/cartesian fall out of recursive alternation; empty branches
  drop (globset `empty_alternates=false`, so `{a,}` = `a` only); an unbalanced `{` is
  auto-closed (fail-safe). Linux + Windows unchanged.
- **env-var-name globs REJECT braces** (`reject_env_key_braces`, compile error) — a
  narrower grammar (`*` prefix/suffix), same class as the D11 mid-host rejection.
- **net host patterns REJECT braces** (`host_pattern_is_valid` + a `push_net_rule` guard,
  compile error) — only `*`/`*.suffix` wildcards.

Verified: a globset-ORACLE differential unit sweep (emitted-regex match set == globset
match set over brace shapes × candidate paths), focused translation asserts, and REAL
`sandbox-exec` enforcement on this macOS host (`brace_deny_denies_every_expanded_path…`,
`nested_brace_deny_denies_all_alternatives` — `a.key`/`b.key` DENIED, `c.key`/unrelated
readable), plus env/net compile-error asserts.

## Changelog

- 2026-07-09 (later) — RESOLVED. macOS fs braces now expand to alternation (globset-
  consistent); env-key + net-host braces rejected at compile time. See Resolution above.
- 2026-07-09 — Initial write-up. macOS brace-alternation silent under-enforcement
  confirmed empirically + at source; case-insensitivity candidate refuted; Linux/Windows
  pending VM confirmation.
