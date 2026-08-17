# pnpm `--filter` grammar and resolution algorithm

Research for implementing workspace `--filter` support in Nub's script runner. Source material is pnpm's TypeScript implementation, read directly from `github.com/pnpm/pnpm` (commit on `main`, 2026-05-26).

Source files read:
- `workspace/projects-filter/src/parseProjectSelector.ts` — the parser
- `workspace/projects-filter/src/index.ts` — the resolution and graph-walk engine
- `workspace/projects-filter/src/getChangedProjects.ts` — git-diff based selection
- `workspace/projects-graph/src/index.ts` — dependency graph construction
- `deps/graph-sequencer/src/index.ts` — topological sort (Kahn's algorithm)
- `workspace/projects-sorter/src/index.ts` — thin wrapper over graph-sequencer
- `exec/commands/src/runRecursive.ts` — the actual recursive run loop
- `config/reader/src/concurrency.ts` — `--workspace-concurrency` defaults
- `config/matcher/src/index.ts` — package name glob matching
- `cli/common-cli-options-help/src/index.ts` — canonical help text (source of truth for user-visible grammar)

---

## 1. Complete filter grammar

The `--filter` flag (shorthand `-F`) selects a subset of workspace packages. Multiple `--filter` flags may be passed; results are unioned (include) or differenced (exclude). The grammar for a single filter value:

```
filter       ::= "!" selector | selector
selector     ::= traversal? name_part? dir_part? diff_part? traversal_deps?

traversal    ::= "..."          (prefix: include dependents / suffix: include dependencies)
traversal_x  ::= "...^"        (prefix variant: exclude the matched package itself)

name_part    ::= pattern        (package.json name, supports * glob)
dir_part     ::= "{" path "}"  (directory glob, relative to prefix/cwd)
diff_part    ::= "[" since "]" (git ref: commit SHA, branch, HEAD~N, origin/main, etc.)

traversal_deps ::= "..."        (suffix: include dependencies)
traversal_xdep ::= "^..."      (suffix variant: exclude the matched package itself)
```

### All documented selector forms

Every form pnpm's own help text documents: a bare or globbed package name, a path, a `{dir}` block, a `[ref]` diff block, the traversal prefixes and suffixes, and the `!` negation.

| Syntax | Meaning |
|--------|---------|
| `foo` | Exact package name `foo` |
| `@scope/foo` | Scoped package by exact name |
| `@scope/*` | All packages under `@scope` |
| `*foo` / `foo*` / `*foo*` | Name glob (only `*` is a wildcard; `**` not supported in names) |
| `./packages/foo` | Package at exact relative path (resolved from `--filter` call prefix) |
| `.` | Package at current working directory |
| `..` | Package at parent directory |
| `../foo` | Package at sibling directory |
| `{./packages}` | All packages whose `rootDir` is a subdirectory of `./packages` (exact prefix match by default) |
| `{./packages/*}` | Same but via glob match (only when `useGlobDirFiltering` is true — see §4) |
| `[origin/main]` | All packages with changed files since `origin/main` |
| `[HEAD~2]` | All packages with changed files since 2 commits ago |
| `foo...` | `foo` plus all its direct+transitive dependencies |
| `foo^...` | All dependencies of `foo`, excluding `foo` itself |
| `...foo` | `foo` plus all direct+transitive dependents (packages that depend on `foo`) |
| `...^foo` | All dependents of `foo`, excluding `foo` itself |
| `...foo...` | `foo` + all its dependents + all their dependencies (the full affected set) |
| `...[origin/main]` | All changed packages plus their dependents |
| `[origin/main]...` | All changed packages plus their dependencies |
| `...[origin/main]...` | Full affected set (changed + dependents + their deps) |
| `{./packages}[origin/main]` | Changed packages, scoped to the `packages/` dir |
| `pattern{./dir}[ref]` | Name-pattern AND dir AND diff, all combined |
| `!foo` | Exclude `foo` from the selected set |
| `!./packages` | Exclude all packages under `packages/` |

### Windows note

`^` in `foo^...` / `...^foo` must be doubled on Windows Command Prompt: `foo^^...` / `...^^foo`. This is a CMD escaping issue, not part of the grammar itself.

---

## 2. Parsing algorithm (`parseProjectSelector.ts`)

The parser is a single function operating on the raw filter string. Steps in order:

1. **Strip `!` prefix** → sets `exclude = true`, remove the `!`.

2. **Strip trailing `...`** → sets `includeDependencies = true`. Then check if the remaining string ends with `^` → sets `excludeSelf = true`, strip the `^`.

3. **Strip leading `...`** → sets `includeDependents = true`. Then check if the next char is `^` → sets `excludeSelf = true`, strip the `^`.

4. **Regex match** the remaining string against: `^([^.][^{}[\]]*)?(\{[^}]+\})?(\[[^\]]+\])?$`
   - Group 1 (`namePattern`): the name/glob part — any chars except `.` at the start, and not `{}[]` chars.
   - Group 2 (`parentDir`): `{...}` block — a directory path, resolved to absolute via `path.join(prefix, rawSelector)`.
   - Group 3 (`diff`): `[...]` block — the git ref string, stripped of brackets.

5. **Fallback** if the regex doesn't match: check `isSelectorByLocation()` — true if the string starts with `.` followed by nothing, `/`, `\`, `.`, `.` followed by nothing, `/`, or `\`. If it's a path selector, use it as `parentDir` directly. Otherwise treat it as a `namePattern` (handles edge-case globs that don't fit the regex).

**Result type (`ProjectSelector`):**
```typescript
interface ProjectSelector {
  diff?: string           // git ref, e.g. "origin/main"
  exclude?: boolean       // negate this selector
  excludeSelf?: boolean   // omit the matched package itself (^ modifier)
  includeDependencies?: boolean  // traverse deps (... suffix)
  includeDependents?: boolean    // traverse dependents (... prefix)
  namePattern?: string    // package name glob
  parentDir?: string      // absolute path for dir matching
  followProdDepsOnly?: boolean   // set by --filter-prod
}
```

---

## 3. Package name resolution (`matchProjects` in `index.ts`)

Name matching uses `@pnpm/config.matcher` (`createMatcher`), which compiles patterns to regexps:

- `*` → `.*` (greedy, full-string match via `^...$` anchors)
- Any other char → literal (escaped via `escape-string-regexp`)
- Pattern `*` alone → always true
- Exact strings → identity compare (skips regexp)

**Scope-elision fallback**: if the pattern has no `@` prefix and no `/`, and zero workspace packages match, pnpm retries the match against `@*/<pattern>`. If exactly one scoped package matches, that package is selected. If two or more match (e.g. `@foo/bar` and `@types/bar` both exist when searching for `bar`), zero are selected — no ambiguous implicit-scope resolution.

---

## 4. Directory matching (`matchProjectsByExactPath` / `matchProjectsByGlob`)

There are two modes, controlled by `useGlobDirFiltering`:

**Default (exact prefix match):** Uses `isSubdir(pathStartsWith, parentDir)` — selects all packages whose `rootDir` is a subdirectory of (or equal to) the specified path, treating `{./packages}` as a prefix rather than a glob. So `{./packages}` matches `packages/foo` and `packages/bar` but NOT `packages/foo/subpkg` unless `subpkg` is also a workspace member.

**Glob mode (`useGlobDirFiltering: true`):** Uses `micromatch.isMatch(parentDir, formattedFilter, { format: str => str.replace(/\/$/, '') })`, with the filter normalized (backslashes to forward-slashes, trailing slash removed). This enables `{./packages/*}` and `{./packages/**}`. In glob mode `{./packages}` matches only the exact path, so `{./packages/*}` is needed to match direct children.

**Which mode pnpm uses:** The deprecated `legacyDirFiltering` config option maps to `useGlobDirFiltering = !legacyDirFiltering`; glob mode is the current default. From the integration test `'select by parentDir using glob'`: `{./packages/*}` with glob enabled selects `project-0` and `project-1`; `{/project-5}` without glob matches `project-5` and `project-5/packages/project-6` (prefix walk); with glob and `{/project-5/**}`, same result.

**Recommendation for Nub:** use glob mode (micromatch) by default — the current pnpm default, and more powerful.

---

## 5. Dependency graph construction (`createProjectsGraph`)

The workspace's inter-package dependency graph is built from `package.json` manifests:

1. Collect all projects from `pnpm-workspace.yaml` patterns (or the discovered workspace).
2. For each project, union its `dependencies + devDependencies + optionalDependencies + peerDependencies`.
3. For each dep entry:
   - If spec is `workspace:*` / `workspace:^x.y.z` / etc. → normalize to semver and resolve by name within the workspace.
   - If spec is a directory path (type `directory` from npa) → resolve to absolute path, find the workspace project at that path.
   - If spec is a `version` or `range` type → resolve by package name + semver match within workspace (only if `linkWorkspacePackages` is true; otherwise skip and treat as external).
4. Edge = `projectA → projectB` means A depends on B (B is a dependency of A).

**For `--filter-prod`:** same algorithm but `ignoreDevDeps: true` omits `devDependencies` when building edges. The `followProdDepsOnly` flag on the selector uses this prod-only graph for traversal.

---

## 6. Graph traversal algorithm (`_filterGraph` in `index.ts`)

For each selector:

1. **Entry set** (`entryProjects`): computed from `diff`, `parentDir`, `namePattern` (combined via intersection when multiple fields are set — diff first, then filtered by dir, then filtered by name).

2. **`includeDependencies`** (suffix `...`): run `pickSubgraph(graph, entryProjects, walkedDependencies, { includeRoot: !excludeSelf })`. This is a DFS/BFS traversal following edges forward (dependency direction). The `includeRoot` flag controls whether the entry package itself is added.

3. **`includeDependents`** (prefix `...`): build `reversedGraph` (reverse all edges), then run `pickSubgraph(reversedGraph, entryProjects, walkedDependents, { includeRoot: !excludeSelf })`. Traverses in the dependents direction.

4. **Both `includeDependencies && includeDependents`** (the `...foo...` form): after walking dependents, also walk their dependencies: `pickSubgraph(graph, Array.from(walkedDependents), walkedDependentsDependencies, { includeRoot: false })`. This is the "full affected cone" — all packages that could be affected if `foo` changes.

5. **Neither** (plain cherry-pick): just push `entryProjects` directly into `cherryPickedProjects`.

`pickSubgraph` is iterative BFS using a `Set<ProjectRootDir>` as the visited set (avoids infinite loops in cycles):

```typescript
function pickSubgraph(graph, nextNodeIds, walked, { includeRoot }) {
  for (const nextNodeId of nextNodeIds) {
    if (!walked.has(nextNodeId)) {
      if (includeRoot) walked.add(nextNodeId)
      if (graph[nextNodeId]) pickSubgraph(graph, graph[nextNodeId], walked, { includeRoot: true })
    }
  }
}
```

After all selectors are processed, the final selected set = union of `walkedDependencies ∪ walkedDependents ∪ walkedDependentsDependencies ∪ cherryPickedProjects`.

**Multiple selectors:** results are unioned for include-selectors, then excluded packages (those with `exclude: true`) are subtracted via set difference. If no include selectors are specified, the implicit include set is all packages (the base for exclude-only filters).

---

## 7. Git-diff based selection (`getChangedProjects.ts`)

For `[<since>]` selectors, pnpm runs:

```sh
git diff --name-only <since> -- <workspaceDir>
```

The output (one changed file per line) is processed:
- Each file path's directory is walked up until it matches a workspace project root.
- The `--test-pattern` option (micromatch globs) classifies changed files as `'test'` or `'source'`. Files matching test patterns are `'test'`; all others are `'source'`.
- A project with only `'test'` changes is added to `ignoreDependentForPkgs` — dependents of such packages are NOT included even when `includeDependents` is set.
- A project with at least one `'source'` change is added to `changedProjects` — dependents ARE included.
- Projects with no changes are omitted entirely.
- Korean/non-ASCII paths: git may quote them with `"` prefix/suffix; the implementation strips those.

**Worktree support:** `.git` is found by `findUp` looking for both directory and file types, to handle git worktrees (where `.git` is a file pointing to the main repo's worktree directory).

**Error handling:** git errors (non-zero exit, bad revision) throw `PnpmError` with code `ERR_PNPM_FILTER_CHANGED`.

---

## 8. Topological sort for execution order (`graphSequencer`, `sortProjects`)

pnpm uses a Kahn's algorithm variant that groups packages into "chunks" (batches that can run in parallel):

1. Build a graph of selected packages only (filter out non-selected nodes).
2. Build a reverse graph (dependent → dependency direction) and calculate `outDegree` for each node (number of dependencies within the selected set).
3. Repeatedly:
   a. Collect all nodes with `outDegree == 0` → these form one parallel chunk.
   b. Remove those nodes, decrement `outDegree` of their dependents.
   c. If no nodes have `outDegree == 0` but nodes remain, there's a cycle — extract the longest cycle, add it as a chunk (marked `safe: false`).
4. Return `{ chunks: T[][], safe: boolean, cycles: T[][] }`.

Each chunk is a set of packages that have no inter-dependencies within that chunk (safe to run in parallel). Chunks must be run in order (chunk N+1 starts only after chunk N completes).

**`sortProjects`** wraps this into `ProjectRootDir[][]` — an array of parallel batches.

---

## 9. Execution model (`runRecursive.ts`)

```
if (opts.sort) {
  sortedPackageChunks = sortProjects(selectedProjectsGraph)  // topological chunks
} else {
  sortedPackageChunks = [Object.keys(selectedProjectsGraph).sort()]  // single alphabetical chunk
}

if (opts.reverse) {
  sortedPackageChunks.reverse()
}

const limit = pLimit(getWorkspaceConcurrency(opts.workspaceConcurrency))

for (const chunk of sortedPackageChunks) {
  await Promise.all(chunk.map(pkg => limit(() => runScript(pkg, scriptName))))
}
```

Key points:
- **Chunks run sequentially** (`for...of` with `await`).
- **Within a chunk, packages run in parallel** (up to `workspaceConcurrency`).
- **`pLimit`** enforces the concurrency cap across all packages within a chunk.
- **`--parallel` flag** (mentioned in CLI help / `--aggregate-output` description): effectively sets `sort: false` and `workspaceConcurrency: Infinity` — a single chunk of all packages running fully concurrently.
- **`--reverse`**: reverses the chunk array, so dependents run before dependencies.
- **`--bail` (default: true)**: on first script failure, throw immediately; subsequent packages in the chunk are abandoned.
- **`--if-present`**: skip packages without the script instead of erroring.
- **`--resume-from <pkg>`**: skip all chunks/packages before the named package (useful for resuming interrupted runs).

### `--workspace-concurrency` semantics

From `config/reader/src/concurrency.ts`:
- `undefined` → `min(4, os.availableParallelism())` (max default is 4)
- `> 0` → use that value directly
- `<= 0` → `max(1, availableParallelism - abs(value))` — "leave N cores free"
- `Infinity` → no limit

---

## 10. `--include-workspace-root`

By default when running with `--filter` or `--recursive`, the workspace root package (the `package.json` at `workspaceDir`) is excluded from the selected set. `--include-workspace-root` opts it back in.

This is separate from the filter system — it's a post-filter inclusion flag applied at the CLI layer.

---

## 11. `--filter-prod`

Identical to `--filter` but uses a dependency graph built with `ignoreDevDeps: true`. The selector is processed normally; traversal (`...`) only follows non-dev dependency edges. Useful for "what packages are affected in production if I change X?"

---

## 12. Complete `--filter` flag behavior summary (implementation checklist)

For a Nub implementation:

**Parsing (one selector string → `ProjectSelector`):**
- [ ] Strip leading `!` → `exclude`
- [ ] Strip trailing `...` → `includeDependencies`; strip trailing `^` before `...` → `excludeSelf`
- [ ] Strip leading `...` → `includeDependents`; strip leading `^` after `...` → `excludeSelf`
- [ ] Regex parse remaining: `name{dir}[diff]` structure
- [ ] `.` / `..` / `./x` / `../x` → path selector (no regex match needed)

**Name matching:**
- [ ] Compile `*`-glob to regexp (`^pattern$` with `*` → `.*`)
- [ ] Exact string → identity compare
- [ ] Scope-elision: retry `@*/<pattern>` if unscoped pattern matches zero packages; select only if exactly one scoped match

**Dir matching:**
- [ ] Default: `isSubdir(filterPath, packageRootDir)` — all packages under the path
- [ ] Glob mode: `micromatch.isMatch(packageRootDir, filterPath)` — normalize slashes, strip trailing `/`

**Dependency graph:**
- [ ] Build from workspace `package.json` manifests (deps + devDeps + optionalDeps + peerDeps)
- [ ] Resolve `workspace:` specs by name within workspace
- [ ] Resolve directory specs by absolute path

**Traversal:**
- [ ] `includeDependencies` → BFS forward (A→B = A depends on B, follow B's edges too)
- [ ] `includeDependents` → BFS on reversed graph
- [ ] Both → also BFS-forward from all collected dependents (adds their deps)
- [ ] `excludeSelf` → don't include the entry package itself
- [ ] Multiple include selectors → union
- [ ] Exclude selectors → subtract from include union (if no include selectors, base = all packages)

**Execution:**
- [ ] Topological sort (Kahn's) → `T[][]` chunks
- [ ] Run chunks sequentially, packages within chunk in parallel up to `--workspace-concurrency`
- [ ] `--no-sort` → single alphabetical chunk (full parallel)
- [ ] `--parallel` → same as `--no-sort --workspace-concurrency=Infinity`
- [ ] `--reverse` → reverse chunk order
- [ ] `--bail` (default true) → abort on first failure
- [ ] `--if-present` → skip packages missing the script

**Git diff:**
- [ ] `git diff --name-only <ref> -- <workspaceDir>`, walk file paths up to project roots
- [ ] `--test-pattern` globs classify files as test-only changes (exclude from triggering dependent selection)
- [ ] `--changed-files-ignore-pattern` globs exclude files from change detection entirely

---

## Changelog

Revision history, naming the pnpm source revision each pass was read against.

- 2026-05-26 — Initial write-up. Source: pnpm/pnpm `main` branch, read directly via GitHub API.
