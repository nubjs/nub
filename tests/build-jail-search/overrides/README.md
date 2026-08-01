# Hand-authored catalog overrides

A grant here **replaces** whatever the search measured for that package. Use this only when a
sweep genuinely cannot answer the question and a human or agent read the package's source
instead.

## ⛔ Use sparingly — this is a design constraint, not a style note

Every override is a place the catalog stops being *derived from evidence* and becomes an
*assertion*. Measurements re-run and self-correct; assertions rot silently. So the collator:

- prints every override it applies, on every run — an unreported override is how a catalog
  quietly stops reflecting measurement;
- **refuses** an override missing its rationale fields;
- reports an override whose measured result already **agrees** with it, so dead weight can be
  pruned.

**Prefer fixing the generator.** Two harness filters were retired in one day precisely because a
per-package patch does not scale to an ecosystem — one was replaced by the double-control union,
the other by a global env var. If an override would apply to a second package, it is a generator
fix wearing a disguise.

## Shape

`overrides/<package>.json` (scoped packages use `+`, e.g. `@scope+name.json`):

```json
{
  "package": "sharp",
  "grants": [
    { "write": "disk", "network": true, "notes": "…why this specific grant…" }
  ],
  "rationale": {
    "investigator": "who or what established this",
    "evidence": "what was actually read or run — a source path + line, a command, an issue link",
    "date": "2026-08-01"
  }
}
```

`grants` is a list of ordinary v2 grants, first-match-wins, so version and platform matchers
work exactly as they do in a measured entry. The parser validates them identically — an override
cannot express something the schema forbids.

## Investigated and deliberately NOT overridden

**`sharp@0.32.6`** — measured `write.disk + network`. Source scan confirms the measurement
rather than correcting it, so no override was written.

Its install script is a fallback chain:

```
(node install/libvips && node install/dll-copy && prebuild-install)
  || (node install/can-compile && node-gyp rebuild && node install/dll-copy)
```

The right-hand branch runs only when the libvips download fails. `lib/libvips.js:97` then shells
out to `which brew >/dev/null && brew environment --plain` to find `PKG_CONFIG_LIBDIR`, and brew
writes its own API cache and bootsnap compile-cache across `~/Library/Caches/Homebrew` — which is
where the wide grant comes from.

Two things follow. The verdict depends on which branch runs, so a machine whose download succeeds
measures narrower than one whose download fails; and the wider answer is the correct one to ship,
because a user hitting the fallback with a narrow grant gets a broken install. An override here
would restate what the sweep already found, which is the definition of dead weight.
