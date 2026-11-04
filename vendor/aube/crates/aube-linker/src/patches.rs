use aube_lockfile::LockfileGraph;
use aube_lockfile::dep_path_filename::dep_path_to_filename;
use std::collections::BTreeMap;
use std::path::Path;

/// A map of `name@version` -> raw multi-file unified diff text.
///
/// Keys must match the `spec_key()` value the resolver writes into
/// every `LockedPackage`. The value is the raw multi-file unified diff
/// text written by `aube patch-commit` (or any compatible tool).
pub type Patches = BTreeMap<String, String>;

/// The applied-patch sidecar filename, derived from the tool's identity:
/// `.<name>-applied-patches.json`. Standalone aube:
/// `.aube-applied-patches.json`.
pub(crate) fn applied_patches_sidecar_name() -> String {
    format!(".{}-applied-patches.json", aube_util::embedder().name)
}

pub(crate) fn current_patch_hashes(patches: &Patches) -> BTreeMap<String, String> {
    use sha2::{Digest, Sha256};
    patches
        .iter()
        .map(|(k, v)| {
            // CRLF-normalize before hashing, matching pnpm's
            // `createHexHashFromFile` and `ResolvedPatch::content_hash`,
            // so the patch fingerprint is identical across the
            // applied-patch sidecar, the graph hash, and the lockfile
            // `patchedDependencies` value.
            let normalized = v.replace("\r\n", "\n");
            let mut h = Sha256::new();
            h.update(normalized.as_bytes());
            (k.clone(), hex::encode(h.finalize()))
        })
        .collect()
}

/// Read the previously-applied patch sidecar at
/// `node_modules/.aube-applied-patches.json`. Missing or malformed
/// files return an empty map — the caller treats them as "no patches
/// were ever applied here," which conservatively triggers a re-link
/// on the first run after the linker started writing the sidecar.
pub(crate) fn read_applied_patches(nm_dir: &Path) -> BTreeMap<String, String> {
    let path = nm_dir.join(applied_patches_sidecar_name());
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Default::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Write the applied-patch sidecar.
///
/// Next install reads this to compute which `.aube/<dep_path>`
/// entries need re-materializing because their patch set changed.
/// Old code was `let _ = fs::write(...)`, dropped any IO error. If
/// write silently failed (disk full, read-only mount, perms), the
/// sidecar was missing on next install, and
/// wipe_changed_patched_entries did not know which entries to
/// re-link. Install reported success while node_modules had stale
/// patched content on disk. Return Result, caller logs loudly.
pub(crate) fn write_applied_patches(
    nm_dir: &Path,
    map: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    let path = nm_dir.join(applied_patches_sidecar_name());
    // No patches applied ⇒ no sidecar: an empty `{}` manifest carries no
    // information (a missing file already reads back as the empty map in
    // read_applied_patches, so wipe-on-change is unchanged), and writing it
    // clutters every patch-free `node_modules` with a stray dotfile. Remove a
    // stale non-empty sidecar too, so `patch-remove`'ing the last patch cleans
    // up after itself. Best-effort unlink — a missing file is the success case.
    if map.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        let out = serde_json::to_string(map)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        aube_util::fs_atomic::atomic_write(&path, out.as_bytes())
    }
}

/// Wipe `.aube/<dep_path>` for any package whose patch fingerprint
/// changed between the previous and current install. Used by the
/// per-project (no-global-store) link path, where the directory name
/// doesn't otherwise change when a patch is added or removed.
pub(crate) fn wipe_changed_patched_entries(
    aube_dir: &Path,
    graph: &LockfileGraph,
    prev: &BTreeMap<String, String>,
    curr: &BTreeMap<String, String>,
    max_length: usize,
) {
    let mut affected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for k in prev.keys().chain(curr.keys()) {
        if prev.get(k) != curr.get(k) {
            affected.insert(k.clone());
        }
    }
    if affected.is_empty() {
        return;
    }
    for (dep_path, pkg) in &graph.packages {
        // Patch fingerprints are keyed by the declared selector (the
        // registry identity), so an npm-aliased entry must match on
        // `registry_name()@version` as well as its alias-qualified
        // `spec_key()`. Mirrors `LockedPackage::lookup_patch`.
        let spec_key = pkg.spec_key();
        let registry_key = format!("{}@{}", pkg.registry_name(), pkg.version);
        if affected.contains(&spec_key) || affected.contains(&registry_key) {
            let entry = aube_dir.join(dep_path_to_filename(dep_path, max_length));
            let _ = std::fs::remove_dir_all(entry);
        }
    }
}

/// Apply a multi-file unified diff to a package directory.
///
/// The patch text is split on git-style or plain unified-diff file
/// boundaries; each section is parsed as a single-file unified diff and
/// applied to the matching file under `pkg_dir`. We deliberately unlink
/// the destination
/// before writing, because the linker materializes files via reflink
/// or hardlink — modifying the file in place would corrupt the global
/// content-addressed store the linked file points to.
fn is_safe_rel_component(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\0') || rel.contains('\\') {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute()
        || p.has_root()
        || rel.starts_with('/')
        || rel.len() >= 2 && rel.as_bytes()[1] == b':'
    {
        return false;
    }
    p.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

fn ensure_no_symlink_in_chain(pkg_dir: &Path, rel: &str) -> Result<(), String> {
    let mut cursor = pkg_dir.to_path_buf();
    for comp in Path::new(rel).components() {
        cursor.push(comp);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(format!("{}", cursor.display()));
                }
                // Junctions on Windows are `IO_REPARSE_TAG_MOUNT_POINT`
                // reparse points, not `IO_REPARSE_TAG_SYMLINK`, and
                // `FileType::is_symlink()` returns false for them.
                // Catch every reparse point via the file-attribute
                // bit so a junction can't sneak the patch out of the
                // package directory.
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
                    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                        return Err(format!("{}", cursor.display()));
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("stat {}: {e}", cursor.display())),
        }
    }
    Ok(())
}

/// Refuse a patch path that escapes the package directory. A hostile
/// diff with `b/../../etc/shadow` as the target would otherwise let the
/// patch step overwrite, delete or rename files outside the installed
/// package. Same rules we apply to tar entries over in aube-store (no
/// absolute, no drive prefix, no `..`, no backslash, no NUL).
///
/// Every parent component on disk is walked too, refusing to follow any
/// symlink or junction. Without this guard, a package that planted a
/// directory link inside its own tree (or a workspace where the user has
/// a symlinked dep dir) would let `pkg_dir.join(rel)` resolve through the
/// link, and `atomic_write` would overwrite a file outside `pkg_dir`.
/// CVE-2018-1000156 (GNU patch) class.
fn check_patch_path(pkg_dir: &Path, rel: &str) -> Result<(), String> {
    if !is_safe_rel_component(rel) {
        return Err(format!("patch file path escapes package: {rel:?}"));
    }
    ensure_no_symlink_in_chain(pkg_dir, rel)
        .map_err(|e| format!("patch target contains symlink: {e}"))
}

/// A git rename moves the file before its hunks apply, as pnpm does. The
/// rename only re-points a directory entry, so a file still linked to the
/// global store stays byte-identical there; any hunk then replaces it
/// through `atomic_write`. An existing destination is refused rather than
/// clobbered, unless it is the source itself under a case-only rename.
fn rename_in_package(from: &Path, to: &Path) -> Result<(), String> {
    if !from.exists() {
        return Err(format!(
            "cannot rename {}: it does not exist",
            from.display()
        ));
    }
    if to.symlink_metadata().is_ok() && !is_case_only_rename(from, to) {
        return Err(format!(
            "cannot rename {} to {}: the destination already exists",
            from.display(),
            to.display()
        ));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    std::fs::rename(from, to).map_err(|e| {
        format!(
            "failed to rename {} to {}: {e}",
            from.display(),
            to.display()
        )
    })
}

/// `readme.md` → `README.md` on a case-insensitive filesystem (default
/// macOS and Windows): the destination lookup finds the source itself.
/// pnpm's `fs-extra` `moveSync` lets that through, and `rename` then
/// stores the new casing. It must be the source's own directory entry:
/// the linker hardlinks identical files from the store, so another entry
/// can share the source's inode, and `rename` between two links to one
/// inode succeeds without doing anything. So the name has to fold to the
/// source's own name in the same directory, and not exist there verbatim.
/// The directory is compared by identity, since its spelling in the patch
/// may differ in case too (`lib/LICENSE` → `Lib/License`).
fn is_case_only_rename(from: &Path, to: &Path) -> bool {
    let (Some(from_dir), Some(dir), Some(from_name), Some(to_name)) =
        (from.parent(), to.parent(), from.file_name(), to.file_name())
    else {
        return false;
    };
    let folds_to_source = match (from_name.to_str(), to_name.to_str()) {
        (Some(from_name), Some(to_name)) => from_name.to_lowercase() == to_name.to_lowercase(),
        _ => false,
    };
    folds_to_source
        && same_file::is_same_file(from_dir, dir).unwrap_or(false)
        && same_file::is_same_file(from, to).unwrap_or(false)
        && std::fs::read_dir(dir).is_ok_and(|mut entries| {
            entries.all(|entry| entry.is_ok_and(|entry| entry.file_name() != to_name))
        })
}

pub(crate) fn apply_multi_file_patch(pkg_dir: &Path, patch_text: &str) -> Result<(), String> {
    let sections = split_patch_sections(patch_text);
    if sections.is_empty() {
        return Err("patch contained no parseable file sections".to_string());
    }
    for section in sections {
        let rel = section
            .rel_path
            .as_ref()
            .ok_or_else(|| "patch section missing file path".to_string())?;
        check_patch_path(pkg_dir, rel)?;
        let target = pkg_dir.join(rel);
        if let Some(from) = section.ops.rename_from.as_deref() {
            check_patch_path(pkg_dir, from)?;
            rename_in_package(&pkg_dir.join(from), &target)?;
        }
        // A deletion skips the hunk applier entirely — emptying the file
        // would write a zero-byte file in place of the original, leaving
        // `require('./removed')` resolving to an empty module instead of
        // the expected `MODULE_NOT_FOUND`.
        if section.ops.is_deletion {
            if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            }
            continue;
        }
        let permissions = patched_file_permissions(&target, section.ops.new_mode);
        if section.body.is_empty() {
            // A metadata-only section. A bare rename is already done; an
            // empty new file or a mode change is written in full rather
            // than `chmod`ed, which would reach a store-linked inode.
            let bytes = if section.ops.is_creation {
                Vec::new()
            } else if section.ops.new_mode.is_some() {
                std::fs::read(&target)
                    .map_err(|e| format!("failed to read {}: {e}", target.display()))?
            } else {
                continue;
            };
            write_patched_file(&target, &bytes, permissions)?;
            continue;
        }
        let original = if target.exists() {
            std::fs::read_to_string(&target)
                .map_err(|e| format!("failed to read {}: {e}", target.display()))?
        } else {
            String::new()
        };
        // git-style patches always use LF line endings, but published
        // tarballs frequently ship files with CRLF (Windows editors,
        // `core.autocrlf=true` checkouts). The hunk matcher's trailing-ws
        // trim absorbs a lone `\r`, but to keep the WRITTEN bytes CRLF we
        // normalize the original to LF before applying and restore the
        // CRLF on write. This CRLF wrapper is a deliberate improvement
        // over raw pnpm, which has no CRLF awareness and would write LF.
        let was_crlf = original.contains("\r\n");
        let normalized = if was_crlf {
            original.replace("\r\n", "\n")
        } else {
            original
        };
        let hunks = parse_hunks(&section.body)
            .map_err(|e| format!("failed to parse patch for {rel}: {e}"))?;
        let patched_lf = apply_hunks(&normalized, &hunks)
            .map_err(|e| format!("failed to apply patch for {rel}: {e}"))?;
        let patched = if was_crlf {
            // Promote bare `\n` to `\r\n`, then collapse any `\r\r\n`
            // back so a patch line containing a literal `\r` byte (rare
            // but legal for binary-ish text) doesn't gain a second CR.
            patched_lf.replace('\n', "\r\n").replace("\r\r\n", "\r\n")
        } else {
            patched_lf
        };
        write_patched_file(&target, patched.as_bytes(), permissions)?;
    }
    Ok(())
}

/// Break any reflink/hardlink to the global store before writing the
/// patched bytes — otherwise we'd silently mutate every other project
/// sharing this CAS file. Stage the write through a sibling tempfile and
/// `rename` into place so a crash or Ctrl-C mid-patch cannot leave the
/// package with the original file unlinked and no replacement written.
/// POSIX `rename(2)` atomically replaces the destination, so no
/// pre-removal is needed and removing first would create the exact TOCTOU
/// window the rename is supposed to close. Windows `MoveFileExW` fails
/// when the destination exists, so the unlink is gated behind
/// `cfg(windows)`.
fn write_patched_file(
    target: &Path,
    bytes: &[u8],
    permissions: Option<std::fs::Permissions>,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        if target.exists() {
            std::fs::remove_file(target)
                .map_err(|e| format!("failed to unlink {}: {e}", target.display()))?;
        }
    }
    aube_util::fs_atomic::atomic_write_with_permissions(target, bytes, permissions).map_err(|e| {
        format!(
            "failed to write patched file into place {}: {e}",
            target.display()
        )
    })
}

/// The mode a patched file is written with. The atomic write installs a
/// new inode, so without this an executable `bin` script would come back
/// `0644`; pnpm writes patched bytes over the file in place and keeps its
/// mode. A git `new mode` / `new file mode` header wins over both.
#[cfg(unix)]
fn patched_file_permissions(target: &Path, new_mode: Option<u32>) -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    match new_mode {
        Some(mode) => Some(std::fs::Permissions::from_mode(mode)),
        None => std::fs::metadata(target).ok().map(|m| m.permissions()),
    }
}

#[cfg(not(unix))]
fn patched_file_permissions(
    _target: &Path,
    _new_mode: Option<u32>,
) -> Option<std::fs::Permissions> {
    None
}

/// One contiguous run of like-typed lines inside a hunk — a block of
/// context, a block of deletions, or a block of insertions. Mirrors
/// pnpm's `PatchMutationPart` (`@pnpm/patch-package`'s `parse.ts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PartType {
    Context,
    Deletion,
    Insertion,
}

struct HunkPart {
    kind: PartType,
    lines: Vec<String>,
    /// The `\ No newline at end of file` pragma followed this part, so
    /// the part's last line is the file's EOF line and carries no
    /// trailing newline.
    no_newline_at_eof: bool,
}

struct Hunk {
    /// 1-based original-file start line from the `@@ -start,len ...` header.
    original_start: usize,
    /// Original-file span length (the `len` in `-start,len`).
    original_length: usize,
    parts: Vec<HunkPart>,
}

/// Parse the single-file unified-diff body produced by
/// `split_patch_sections` into a list of hunks, faithfully mirroring
/// pnpm's `parsePatchLines` line classification: a leading `@` opens a
/// hunk header, `-`/`+`/` ` are deletion/insertion/context, `\` is the
/// no-newline pragma, and a blank / `\r`-only line is treated as
/// **context** (pnpm's `hunkLinetypes` maps `undefined` and `"\r"` to
/// context). The body's `--- `/`+++ ` header lines and any pre-hunk
/// lines are skipped — the path + deletion handling already happened in
/// `split_patch_sections`.
///
/// We split on `\n` (newline-agnostic, like pnpm's `split(/\n/)`) so a
/// final line without a trailing newline round-trips for free.
fn parse_hunks(body: &str) -> Result<Vec<Hunk>, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut in_hunk = false;

    // Split newline-agnostically, then drop a single trailing empty line —
    // `split_patch_sections` terminates the body with a `\n`, so the split
    // yields a spurious final `""`. pnpm's `parsePatchFile` does the same
    // (`if (lines[lines.length-1] === "") lines.pop()`). A genuinely blank
    // line inside the patch arrives as a `" "`-prefixed context line, not a
    // zero-length one, so this only sheds the terminator artifact.
    let mut split: Vec<&str> = body.split('\n').collect();
    if split.last() == Some(&"") {
        split.pop();
    }

    for raw in split {
        // `split_patch_sections` already trimmed trailing `\r`, but stay
        // defensive: a `\r`-only line classifies as context below.
        let first = raw.chars().next();
        match first {
            Some('@') if raw.starts_with("@@") => {
                let header = parse_hunk_header(raw)?;
                hunks.push(header);
                in_hunk = true;
            }
            _ if !in_hunk => {
                // Pre-hunk header line (`--- `/`+++ `/blank). Skip.
            }
            Some('\\') => {
                // `\ No newline at end of file` pragma attaches to the
                // current part's last line.
                if !raw.starts_with("\\ No newline at end of file") {
                    return Err(format!("unrecognized pragma in patch: {raw:?}"));
                }
                let hunk = hunks
                    .last_mut()
                    .ok_or_else(|| "no-newline pragma before any hunk".to_string())?;
                let part = hunk
                    .parts
                    .last_mut()
                    .ok_or_else(|| "no-newline pragma without a preceding line".to_string())?;
                part.no_newline_at_eof = true;
            }
            Some('-') => push_line(&mut hunks, PartType::Deletion, &raw[1..])?,
            Some('+') => push_line(&mut hunks, PartType::Insertion, &raw[1..])?,
            Some(' ') => push_line(&mut hunks, PartType::Context, &raw[1..])?,
            // Blank line or a lone `\r`: pnpm treats these as context.
            // The line's text is the whole `raw` (no type prefix char).
            None => push_line(&mut hunks, PartType::Context, "")?,
            Some('\r') => push_line(&mut hunks, PartType::Context, raw)?,
            // Anything else (e.g. a stray `diff`/`index` line that slipped
            // through) terminates hunk parsing for this body.
            Some(_) => {
                in_hunk = false;
            }
        }
    }
    Ok(hunks)
}

/// Append a line to the current hunk's trailing part, opening a new part
/// when the type changes — pnpm coalesces consecutive same-type lines
/// into one `PatchMutationPart`.
fn push_line(hunks: &mut [Hunk], kind: PartType, text: &str) -> Result<(), String> {
    let hunk = hunks
        .last_mut()
        .ok_or_else(|| "hunk line encountered before any hunk header".to_string())?;
    match hunk.parts.last_mut() {
        Some(part) if part.kind == kind && !part.no_newline_at_eof => {
            part.lines.push(text.to_string());
        }
        _ => hunk.parts.push(HunkPart {
            kind,
            lines: vec![text.to_string()],
            no_newline_at_eof: false,
        }),
    }
    Ok(())
}

/// Parse `@@ -origStart,origLen +newStart,newLen @@` (lengths default to
/// 1 when omitted), matching pnpm's `parseHunkHeaderLine` — including its
/// `Math.max(start, 1)` clamp.
fn parse_hunk_header(line: &str) -> Result<Hunk, String> {
    let body = line
        .trim()
        .strip_prefix("@@ -")
        .ok_or_else(|| format!("bad hunk header: {line:?}"))?;
    // `body` is now `origStart[,origLen] +newStart[,newLen] @@ ...`.
    let (orig, _rest) = body
        .split_once(" +")
        .ok_or_else(|| format!("bad hunk header: {line:?}"))?;
    let (start_s, len_s) = match orig.split_once(',') {
        Some((s, l)) => (s, Some(l)),
        None => (orig, None),
    };
    let start: usize = start_s
        .parse()
        .map_err(|_| format!("bad hunk header start: {line:?}"))?;
    let length: usize = match len_s {
        Some(l) => l
            .parse()
            .map_err(|_| format!("bad hunk header length: {line:?}"))?,
        None => 1,
    };
    Ok(Hunk {
        original_start: start.max(1),
        original_length: length,
        parts: Vec::new(),
    })
}

/// `trimRight` — strip trailing ASCII whitespace, matching pnpm's
/// `s.replace(/\s+$/, "")`. JS `\s` covers space, tab, CR, LF, vertical
/// tab, form feed; `trim_end()` (Unicode whitespace) is a strict
/// superset on the ASCII bytes patches contain, so it matches pnpm here.
fn trim_right(s: &str) -> &str {
    s.trim_end()
}

/// Trailing-whitespace-tolerant line equality — pnpm's `linesAreEqual`.
/// Trims the RIGHT only; leading whitespace must match exactly (finding
/// 5: leading-ws drift is rejected by pnpm and must be by us).
fn lines_are_equal(a: &str, b: &str) -> bool {
    trim_right(a) == trim_right(b)
}

/// One edit to apply to the working line array — pnpm's `Modificaiton`.
enum Modification {
    Splice {
        index: usize,
        num_to_delete: usize,
        lines_to_insert: Vec<String>,
    },
    Pop,
    Push(String),
}

/// Try to place `hunk` at its stated original line shifted by
/// `fuzzing_offset`, matching context+deletion lines trailing-ws-loosely.
/// Returns the list of modifications on success, `None` if the hunk does
/// not line up at this offset — a faithful port of pnpm's `evaluateHunk`.
fn evaluate_hunk(
    hunk: &Hunk,
    file_lines: &[String],
    fuzzing_offset: isize,
) -> Option<Vec<Modification>> {
    let mut result = Vec::new();
    // `original.start - 1 + fuzzingOffset`, with signed bounds checks
    // before indexing (pnpm returns null on a negative index).
    let base = hunk.original_start as isize - 1 + fuzzing_offset;
    if base < 0 {
        return None;
    }
    let mut context_index = base as usize;
    // `fileLines.length - contextIndex < original.length` → null.
    if file_lines.len() < context_index || file_lines.len() - context_index < hunk.original_length {
        return None;
    }

    for part in &hunk.parts {
        match part.kind {
            PartType::Deletion | PartType::Context => {
                for line in &part.lines {
                    let original_line = file_lines.get(context_index)?;
                    if !lines_are_equal(original_line, line) {
                        return None;
                    }
                    context_index += 1;
                }
                if part.kind == PartType::Deletion {
                    result.push(Modification::Splice {
                        index: context_index - part.lines.len(),
                        num_to_delete: part.lines.len(),
                        lines_to_insert: Vec::new(),
                    });
                    if part.no_newline_at_eof {
                        result.push(Modification::Push(String::new()));
                    }
                }
            }
            PartType::Insertion => {
                result.push(Modification::Splice {
                    index: context_index,
                    num_to_delete: 0,
                    lines_to_insert: part.lines.clone(),
                });
                if part.no_newline_at_eof {
                    result.push(Modification::Pop);
                }
            }
        }
    }
    Some(result)
}

/// Apply all hunks to `base_image`, porting pnpm's `applyPatch`:
/// split the file into a newline-agnostic line array, fuzz each hunk
/// over offsets `0, -1, +1, -2, +2, …` capped at `|20|` (refusing
/// beyond — finding 4: nub must NOT over-apply where pnpm refuses),
/// then splice/pop/push the recorded modifications and rejoin on `\n`.
fn apply_hunks(base_image: &str, hunks: &[Hunk]) -> Result<String, String> {
    let mut file_lines: Vec<String> = base_image.split('\n').map(str::to_string).collect();

    let mut all_mods: Vec<Vec<Modification>> = Vec::with_capacity(hunks.len());
    for (i, hunk) in hunks.iter().enumerate() {
        let mut fuzzing_offset: isize = 0;
        let mods = loop {
            if let Some(m) = evaluate_hunk(hunk, &file_lines, fuzzing_offset) {
                break m;
            }
            // pnpm: `fuzzingOffset < 0 ? *-1 : *-1 - 1`
            //   → 0, -1, +1, -2, +2, -3, +3, …
            fuzzing_offset = if fuzzing_offset < 0 {
                -fuzzing_offset
            } else {
                -fuzzing_offset - 1
            };
            if fuzzing_offset.abs() > 20 {
                // Refusing is deliberate — see this function's doc comment. The
                // message says WHERE, because the actionable fact is that the
                // patch was cut against a different release of the package: bun
                // applies some of these where pnpm (and so nub) will not, so a
                // patch that works under bun can arrive here already stale.
                return Err(format!(
                    "could not apply hunk {} at line {} (searched 20 lines either way for its context \
                     and found none).\n\x20\x20The file does not match what the patch was cut \
                     against — regenerate it against this version.",
                    i + 1,
                    hunk.original_start
                ));
            }
        };
        all_mods.push(mods);
    }

    // Apply modifications, tracking the cumulative line-count delta so
    // later splices land at the right index (pnpm's `diffOffset`).
    let mut diff_offset: isize = 0;
    for mods in &all_mods {
        for m in mods {
            match m {
                Modification::Splice {
                    index,
                    num_to_delete,
                    lines_to_insert,
                } => {
                    let at = (*index as isize + diff_offset) as usize;
                    let end = (at + num_to_delete).min(file_lines.len());
                    let removed: Vec<String> = file_lines
                        .splice(at..end, lines_to_insert.iter().cloned())
                        .collect();
                    diff_offset += lines_to_insert.len() as isize - removed.len() as isize;
                }
                Modification::Pop => {
                    file_lines.pop();
                }
                Modification::Push(line) => {
                    file_lines.push(line.clone());
                }
            }
        }
    }

    Ok(file_lines.join("\n"))
}

struct PatchSection {
    rel_path: Option<String>,
    /// Single-file unified diff body — `parse_hunks` reads this directly.
    /// Always begins with `--- ` so the parser has a stable anchor.
    body: String,
    ops: FileOps,
}

/// What a section does to its file besides (or instead of) its hunks.
/// git writes a rename, an empty new file, a mode flip and a deletion cut
/// with `--irreversible-delete` (the form `pnpm patch-commit` emits) as
/// extended-header lines with no hunks at all; pnpm's applier
/// (`@pnpm/patch-package`'s `parse.ts`) reads the same lines.
#[derive(Default)]
struct FileOps {
    /// `+++ /dev/null` or `deleted file mode` — the linker should
    /// `remove_file` instead of writing patched bytes (which the hunk
    /// applier would emit as an empty string).
    is_deletion: bool,
    /// `rename from`: move this path to `rel_path` before any hunk
    /// applies, so the hunks match the source's content.
    rename_from: Option<String>,
    /// `new file mode`: the file exists after the patch even when no hunk
    /// gives it content.
    is_creation: bool,
    /// Permission bits from `new mode` / `new file mode`.
    new_mode: Option<u32>,
}

impl FileOps {
    fn is_empty(&self) -> bool {
        !self.is_deletion
            && self.rename_from.is_none()
            && !self.is_creation
            && self.new_mode.is_none()
    }
}

/// `rename from`/`rename to` paths carry no `a/`/`b/` prefix, and git
/// quotes one holding special characters the way it quotes `diff --git`.
fn git_header_path(raw: &str) -> String {
    raw.strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .and_then(unescape_git_quoted)
        .unwrap_or_else(|| raw.to_string())
}

/// Permission bits of a git regular-file mode (`100644`, `100755`). A
/// symlink or gitlink mode is ignored rather than turned into `chmod 000`.
fn regular_file_mode(mode: &str) -> Option<u32> {
    let mode = u32::from_str_radix(mode.trim(), 8).ok()?;
    (mode & 0o170000 == 0o100000).then_some(mode & 0o777)
}

/// Split a multi-file patch into one section per file. Git-style patches
/// use `diff --git` boundaries; plain unified diffs use completed hunk
/// counts to distinguish the next `---` file header from hunk content.
fn parse_diff_git_b_path(rest: &str) -> Option<String> {
    if let Some(after) = rest.strip_prefix("\"a/") {
        let end_a = after.find("\" \"b/")?;
        let after_b = &after[end_a + 5..];
        let close = after_b.rfind('"')?;
        return unescape_git_quoted(&after_b[..close]);
    }
    let body = rest.strip_prefix("a/")?;
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(" b/") {
        let abs = search_from + rel;
        let path_a = &body[..abs];
        let path_b = &body[abs + 3..];
        if path_a == path_b {
            return Some(path_b.to_string());
        }
        search_from = abs + 1;
    }
    body.find(" b/").map(|i| body[i + 3..].to_string())
}

fn unescape_git_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            return None;
        }
        match bytes[i + 1] {
            b'\\' => {
                out.push(b'\\');
                i += 2;
            }
            b'"' => {
                out.push(b'"');
                i += 2;
            }
            b'n' => {
                out.push(b'\n');
                i += 2;
            }
            b't' => {
                out.push(b'\t');
                i += 2;
            }
            b'r' => {
                out.push(b'\r');
                i += 2;
            }
            b'a' => {
                out.push(0x07);
                i += 2;
            }
            b'b' => {
                out.push(0x08);
                i += 2;
            }
            b'f' => {
                out.push(0x0C);
                i += 2;
            }
            b'v' => {
                out.push(0x0B);
                i += 2;
            }
            d0 @ b'0'..=b'3'
                if i + 3 < bytes.len()
                    && (b'0'..=b'7').contains(&bytes[i + 2])
                    && (b'0'..=b'7').contains(&bytes[i + 3]) =>
            {
                let n = ((d0 - b'0') << 6) | ((bytes[i + 2] - b'0') << 3) | (bytes[i + 3] - b'0');
                out.push(n);
                i += 4;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

fn unified_header_path(header: &str) -> Option<String> {
    let path = header
        .split_once('\t')
        .map_or(header, |(path, _timestamp)| path);
    if path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path)
            .to_string(),
    )
}

fn hunk_line_counts(header: &str) -> Option<(usize, usize)> {
    let ranges = header.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let count = |range: &str| {
        range
            .split_once(',')
            .map_or(Some(1), |(_, count)| count.parse().ok())
    };
    Some((count(old)?, count(new)?))
}

fn split_patch_sections(text: &str) -> Vec<PatchSection> {
    let mut out: Vec<PatchSection> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut body = String::new();
    let mut in_body = false;
    let mut ops = FileOps::default();
    let mut hunk_remaining: Option<(usize, usize)> = None;
    let mut old_header_path: Option<String> = None;
    let mut in_git_header = false;

    let flush = |out: &mut Vec<PatchSection>,
                 path: &mut Option<String>,
                 body: &mut String,
                 ops: &mut FileOps| {
        if !body.is_empty() || !ops.is_empty() {
            out.push(PatchSection {
                rel_path: path.take(),
                body: std::mem::take(body),
                ops: std::mem::take(ops),
            });
        } else {
            *path = None;
        }
    };

    for line in text.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\n', '\r']);
        if let Some(rest) = stripped.strip_prefix("diff --git ") {
            // New file boundary — flush whatever we were collecting.
            flush(&mut out, &mut current_path, &mut body, &mut ops);
            in_body = false;
            hunk_remaining = None;
            old_header_path = None;
            // Parse `a/<path> b/<path>` and prefer the post-edit
            // (`b/`) path so renames land on the new name.
            current_path = parse_diff_git_b_path(rest);
            in_git_header = true;
            continue;
        }
        let at_section_boundary = !in_body || hunk_remaining.is_none();
        if at_section_boundary && let Some(header) = stripped.strip_prefix("--- ") {
            if in_body {
                flush(&mut out, &mut current_path, &mut body, &mut ops);
            }
            in_body = true;
            in_git_header = false;
            hunk_remaining = None;
            old_header_path = unified_header_path(header);
            body.push_str(stripped);
            body.push('\n');
            continue;
        }
        if !in_body {
            // git's extended headers. `index` / `similarity index` /
            // `old mode` decorate the section and are not needed.
            if !in_git_header {
                continue;
            }
            if let Some(path) = stripped.strip_prefix("rename from ") {
                ops.rename_from = Some(git_header_path(path));
            } else if let Some(path) = stripped.strip_prefix("rename to ") {
                // Authoritative over the `diff --git` split, which cannot
                // place the a/b boundary when a renamed path holds ` b/`.
                current_path = Some(git_header_path(path));
            } else if let Some(mode) = stripped.strip_prefix("new file mode ") {
                ops.is_creation = true;
                ops.new_mode = regular_file_mode(mode);
            } else if let Some(mode) = stripped.strip_prefix("new mode ") {
                ops.new_mode = regular_file_mode(mode);
            } else if stripped.starts_with("deleted file mode ") {
                ops.is_deletion = true;
            }
            continue;
        }
        if let Some(header) = stripped.strip_prefix("+++ ") {
            if current_path.is_none() {
                current_path = unified_header_path(header).or_else(|| old_header_path.clone());
            }
            if header == "/dev/null" {
                // File deletion — note it and drop this header line. The
                // linker will `remove_file` and skip the hunk applier
                // entirely, so the rest of the body (the hunk that empties
                // the file) is intentionally discarded.
                ops.is_deletion = true;
                continue;
            }
            // `--- /dev/null` (file addition) carries no old path, so
            // rewrite the anchor to `--- a/<path>`. The original content
            // applied against is empty for additions, which the hunk
            // applier handles directly.
            if old_header_path.is_none()
                && let Some(rel) = current_path.as_deref()
            {
                body.clear();
                body.push_str(&format!("--- a/{rel}\n"));
            }
        }
        body.push_str(stripped);
        body.push('\n');
        if let Some(counts) = hunk_line_counts(stripped) {
            hunk_remaining = (counts != (0, 0)).then_some(counts);
        } else if let Some((old, new)) = hunk_remaining.as_mut() {
            match stripped.as_bytes().first() {
                Some(b' ') => {
                    *old = old.saturating_sub(1);
                    *new = new.saturating_sub(1);
                }
                Some(b'-') => *old = old.saturating_sub(1),
                Some(b'+') => *new = new.saturating_sub(1),
                _ => {}
            }
            if *old == 0 && *new == 0 {
                hunk_remaining = None;
            }
        }
    }
    flush(&mut out, &mut current_path, &mut body, &mut ops);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_patch_set_writes_no_sidecar_and_cleans_a_stale_one() {
        // A patch-free install must not litter `node_modules` with an empty
        // `{}` manifest — a missing file already reads back as the empty map,
        // so the sidecar carries no information. And removing the last patch
        // (non-empty → empty) must delete the now-stale sidecar.
        let dir = tempfile::tempdir().unwrap();
        let nm = dir.path();
        let sidecar = nm.join(applied_patches_sidecar_name());

        write_applied_patches(nm, &BTreeMap::new()).unwrap();
        assert!(
            !sidecar.exists(),
            "empty patch set must not create the sidecar"
        );

        let one: BTreeMap<String, String> =
            [("left-pad@1.0.0".to_string(), "deadbeef".to_string())].into();
        write_applied_patches(nm, &one).unwrap();
        assert!(sidecar.exists(), "a real patch set writes the sidecar");

        write_applied_patches(nm, &BTreeMap::new()).unwrap();
        assert!(
            !sidecar.exists(),
            "dropping the last patch must remove the stale sidecar"
        );
    }

    #[cfg(windows)]
    #[test]
    fn apply_multi_file_patch_refuses_to_follow_junction_outside_pkg() {
        let outside = tempfile::tempdir().unwrap();
        let pkg_root = tempfile::tempdir().unwrap();
        let pkg = pkg_root.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let escape = pkg.join("escape");
        junction::create(outside.path(), &escape).unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        let patch = "diff --git a/escape/victim.txt b/escape/victim.txt\n\
                     --- a/escape/victim.txt\n\
                     +++ b/escape/victim.txt\n\
                     @@ -1 +1 @@\n\
                     -untouched\n\
                     +PWNED\n";
        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err(), "patch must refuse junction-bearing rel");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "untouched\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_multi_file_patch_refuses_to_follow_symlink_outside_pkg() {
        let outside = tempfile::tempdir().unwrap();
        let pkg_root = tempfile::tempdir().unwrap();
        let pkg = pkg_root.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let escape = pkg.join("escape");
        std::os::unix::fs::symlink(outside.path(), &escape).unwrap();
        let target = outside.path().join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        let patch = "diff --git a/escape/victim.txt b/escape/victim.txt\n\
                     --- a/escape/victim.txt\n\
                     +++ b/escape/victim.txt\n\
                     @@ -1 +1 @@\n\
                     -untouched\n\
                     +PWNED\n";
        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err(), "patch must refuse symlink-bearing rel");
        let after = std::fs::read_to_string(&target).unwrap();
        assert_eq!(after, "untouched\n");
    }

    #[test]
    fn round_trips_simple_patch() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports = 'old';\n").unwrap();

        let patch = "diff --git a/index.js b/index.js\n\
                     --- a/index.js\n\
                     +++ b/index.js\n\
                     @@ -1 +1 @@\n\
                     -module.exports = 'old';\n\
                     +module.exports = 'new';\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("index.js")).unwrap(),
            "module.exports = 'new';\n"
        );
    }

    #[test]
    fn applies_plain_unified_patch() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports = 'old';\n").unwrap();

        let patch = "--- a/index.js\n\
                     +++ b/index.js\n\
                     @@ -1 +1 @@\n\
                     -module.exports = 'old';\n\
                     +module.exports = 'new';\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("index.js")).unwrap(),
            "module.exports = 'new';\n"
        );
    }

    #[test]
    fn applies_plain_unified_patch_with_bare_paths() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("index.js"), "old\n").unwrap();

        let patch = "--- index.js\t2026-07-11 00:00:00\n\
                     +++ index.js\t2026-07-11 00:00:01\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("index.js")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn applies_multi_hunk_plain_unified_patch() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("index.js"), "one\ntwo\nthree\nfour\n").unwrap();

        let patch = "--- a/index.js\n\
                     +++ b/index.js\n\
                     @@ -1 +1 @@\n\
                     -one\n\
                     +ONE\n\
                     @@ -4 +4 @@\n\
                     -four\n\
                     +FOUR\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("index.js")).unwrap(),
            "ONE\ntwo\nthree\nFOUR\n"
        );
    }

    #[test]
    fn applies_multi_file_plain_unified_patch() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.js"), "a\n").unwrap();
        std::fs::write(pkg.join("b.js"), "b\n").unwrap();

        let patch = "--- a/a.js\n\
                     +++ b/a.js\n\
                     @@ -1 +1 @@\n\
                     -a\n\
                     +A\n\
                     --- a/b.js\n\
                     +++ b/b.js\n\
                     @@ -1 +1 @@\n\
                     -b\n\
                     +B\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(std::fs::read_to_string(pkg.join("a.js")).unwrap(), "A\n");
        assert_eq!(std::fs::read_to_string(pkg.join("b.js")).unwrap(), "B\n");
    }

    #[test]
    fn plain_unified_patch_adds_and_deletes_files() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("old.js"), "old\n").unwrap();

        let patch = "--- /dev/null\n\
                     +++ b/new.js\n\
                     @@ -0,0 +1 @@\n\
                     +new\n\
                     --- a/old.js\n\
                     +++ /dev/null\n\
                     @@ -1 +0,0 @@\n\
                     -old\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(
            std::fs::read_to_string(pkg.join("new.js")).unwrap(),
            "new\n"
        );
        assert!(!pkg.join("old.js").exists());
    }

    #[test]
    fn plain_unified_patch_path_cannot_escape_package() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        let patch = "--- a/index.js\n\
                     +++ b/../../outside.js\n\
                     @@ -0,0 +1 @@\n\
                     +pwned\n";

        let result = apply_multi_file_patch(&pkg, patch);
        assert!(result.is_err());
        assert!(!dir.path().join("outside.js").exists());
    }

    #[test]
    fn zero_count_hunk_does_not_hide_next_file_boundary() {
        let patch = "--- a/empty.js\n\
                     +++ b/empty.js\n\
                     @@ -0,0 +0,0 @@\n\
                     --- a/index.js\n\
                     +++ b/index.js\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n";

        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].rel_path.as_deref(), Some("empty.js"));
        assert_eq!(sections[1].rel_path.as_deref(), Some("index.js"));
    }

    #[test]
    fn crlf_patch_path_does_not_carry_carriage_return() {
        let patch = "diff --git a/index.js b/index.js\r\n\
                     --- a/index.js\r\n\
                     +++ b/index.js\r\n\
                     @@ -1 +1 @@\r\n\
                     -module.exports = 'old';\r\n\
                     +module.exports = 'new';\r\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("index.js"));
    }

    #[test]
    fn crlf_deletion_patch_recognized() {
        let patch = "diff --git a/removed.js b/removed.js\r\n\
                     deleted file mode 100644\r\n\
                     --- a/removed.js\r\n\
                     +++ /dev/null\r\n\
                     @@ -1 +0,0 @@\r\n\
                     -gone\r\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].ops.is_deletion);
    }

    #[test]
    fn diff_git_path_with_space_b_substring() {
        let patch = "diff --git a/a b/c.js b/a b/c.js\n\
                     --- a/a b/c.js\n\
                     +++ b/a b/c.js\n\
                     @@ -1 +1 @@\n\
                     -x\n\
                     +y\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("a b/c.js"));
    }

    #[test]
    fn diff_git_quoted_path_form() {
        let patch = "diff --git \"a/path with spaces.js\" \"b/path with spaces.js\"\n\
                     --- a/path with spaces.js\n\
                     +++ b/path with spaces.js\n\
                     @@ -1 +1 @@\n\
                     -x\n\
                     +y\n";
        let sections = split_patch_sections(patch);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rel_path.as_deref(), Some("path with spaces.js"));
    }

    #[test]
    fn applies_lf_patch_against_crlf_file() {
        // Tarballs published from Windows editors ship CRLF text. pnpm
        // / git emit LF-only patches even against those files. The apply
        // path normalizes CRLF -> LF before matching and restores CRLF
        // on write, so the patched file keeps its CRLF endings.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"one\r\ntwo\r\nthree\r\n").unwrap();

        let patch = "diff --git a/a.txt b/a.txt\n\
                     --- a/a.txt\n\
                     +++ b/a.txt\n\
                     @@ -1,3 +1,3 @@\n\
                     \x20one\n\
                     -two\n\
                     +TWO\n\
                     \x20three\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        let bytes = std::fs::read(pkg.join("a.txt")).unwrap();
        assert_eq!(bytes, b"one\r\nTWO\r\nthree\r\n");
    }

    #[test]
    fn crlf_restore_preserves_embedded_cr_byte() {
        // A patch line that adds a literal `\r` byte mid-line must not
        // gain a second `\r` when we re-CRLF the output. Naive
        // `replace('\n', "\r\n")` would turn `\r\n` into `\r\r\n`; the
        // `\r\r\n` collapse undoes that.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("a.txt"), b"one\r\ntwo\r\n").unwrap();
        let patch = "diff --git a/a.txt b/a.txt\n\
                     --- a/a.txt\n\
                     +++ b/a.txt\n\
                     @@ -1,2 +1,2 @@\n\
                     -one\n\
                     +has\rcr\n\
                     \x20two\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        let bytes = std::fs::read(pkg.join("a.txt")).unwrap();
        assert_eq!(bytes, b"has\rcr\r\ntwo\r\n");
    }

    #[test]
    fn diff_git_quoted_path_unescapes_git_escapes() {
        let path = parse_diff_git_b_path(r#""a/foo\".js" "b/foo\".js""#).expect("quoted parse");
        assert_eq!(path, "foo\".js");
        let path = parse_diff_git_b_path(r#""a/back\\slash.js" "b/back\\slash.js""#)
            .expect("backslash parse");
        assert_eq!(path, "back\\slash.js");
        let path = parse_diff_git_b_path("\"a/caf\\303\\251.js\" \"b/caf\\303\\251.js\"")
            .expect("octal parse");
        assert_eq!(path, "café.js");
    }

    fn pkg_with(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        for (rel, content) in files {
            std::fs::write(pkg.join(rel), content).unwrap();
        }
        (dir, pkg)
    }

    // git's rename-with-edit form (#845): the hunk was cut against the
    // source's content, so the move has to happen first.
    #[test]
    fn git_rename_moves_the_source_then_applies_hunks_to_the_destination() {
        let (dir, pkg) = pkg_with(&[]);
        // The package file is a hardlink into a store, as the linker leaves it.
        let store_file = dir.path().join("store-LICENSE");
        std::fs::write(&store_file, "ISC License\n\nCopyright\n").unwrap();
        std::fs::hard_link(&store_file, pkg.join("LICENSE")).unwrap();
        let patch = "diff --git a/LICENSE b/LICENSE.txt\n\
                     similarity index 97%\n\
                     rename from LICENSE\n\
                     rename to LICENSE.txt\n\
                     index 82adb48..f352d52 100644\n\
                     --- a/LICENSE\n\
                     +++ b/LICENSE.txt\n\
                     @@ -1 +1 @@\n\
                     -ISC License\n\
                     +ISC License (patched)\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert!(!pkg.join("LICENSE").exists(), "rename source must be gone");
        assert_eq!(
            std::fs::read_to_string(pkg.join("LICENSE.txt")).unwrap(),
            "ISC License (patched)\n\nCopyright\n"
        );
        assert_eq!(
            std::fs::read_to_string(&store_file).unwrap(),
            "ISC License\n\nCopyright\n",
            "patching the renamed file must not write through to the store"
        );
    }

    #[test]
    fn git_rename_without_hunks_moves_the_file_into_a_new_directory() {
        let (_dir, pkg) = pkg_with(&[("old.js", "same\n")]);
        let patch = "diff --git a/old.js b/lib/new.js\n\
                     similarity index 100%\n\
                     rename from old.js\n\
                     rename to lib/new.js\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert!(!pkg.join("old.js").exists());
        assert_eq!(
            std::fs::read_to_string(pkg.join("lib/new.js")).unwrap(),
            "same\n"
        );
    }

    #[test]
    fn git_rename_refuses_paths_outside_the_package_and_an_existing_destination() {
        let (dir, pkg) = pkg_with(&[("a.js", "a\n"), ("b.js", "b\n")]);
        std::fs::write(dir.path().join("outside.js"), "outside\n").unwrap();
        for (from, to) in [("../outside.js", "stolen.js"), ("a.js", "../escaped.js")] {
            let patch = format!(
                "diff --git a/{from} b/{to}\n\
                 similarity index 100%\n\
                 rename from {from}\n\
                 rename to {to}\n"
            );
            let err = apply_multi_file_patch(&pkg, &patch).unwrap_err();
            assert!(err.contains("escapes package"), "{from} -> {to}: {err}");
        }
        assert!(dir.path().join("outside.js").exists());
        assert!(!pkg.join("stolen.js").exists());
        assert!(!dir.path().join("escaped.js").exists());

        let patch = "diff --git a/a.js b/b.js\n\
                     similarity index 100%\n\
                     rename from a.js\n\
                     rename to b.js\n";
        let err = apply_multi_file_patch(&pkg, patch).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(pkg.join("b.js")).unwrap(), "b\n");
    }

    // On a case-insensitive filesystem the destination already "exists" as
    // the source. On a case-sensitive one (Linux CI) it does not, so there
    // this covers only the plain rename path; both must end on the new name.
    #[test]
    fn git_case_only_rename_applies_hunks_under_the_new_name() {
        let (_dir, pkg) = pkg_with(&[]);
        std::fs::create_dir(pkg.join("lib")).unwrap();
        std::fs::write(pkg.join("LICENSE"), "ISC License\n").unwrap();
        std::fs::write(pkg.join("lib/LICENSE"), "ISC License\n").unwrap();
        let case_insensitive = pkg.join("License").exists();
        let names = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        // The second section's parent differs only in case as well. As
        // with pnpm, the file takes the new casing and the directory keeps
        // its own.
        let patch = "diff --git a/LICENSE b/License\n\
                     similarity index 97%\n\
                     rename from LICENSE\n\
                     rename to License\n\
                     --- a/LICENSE\n\
                     +++ b/License\n\
                     @@ -1 +1 @@\n\
                     -ISC License\n\
                     +ISC License (patched)\n\
                     diff --git a/lib/LICENSE b/Lib/License\n\
                     similarity index 97%\n\
                     rename from lib/LICENSE\n\
                     rename to Lib/License\n\
                     --- a/lib/LICENSE\n\
                     +++ b/Lib/License\n\
                     @@ -1 +1 @@\n\
                     -ISC License\n\
                     +ISC License (patched)\n";
        apply_multi_file_patch(&pkg, patch)
            .unwrap_or_else(|e| panic!("case_insensitive={case_insensitive}: {e}"));
        let moved = if case_insensitive {
            assert_eq!(names(&pkg), ["License", "lib"]);
            pkg.join("lib")
        } else {
            assert_eq!(names(&pkg), ["Lib", "License", "lib"]);
            pkg.join("Lib")
        };
        assert_eq!(names(&moved), ["License"]);
        for file in [pkg.join("License"), moved.join("License")] {
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                "ISC License (patched)\n",
                "{}",
                file.display()
            );
        }
    }

    #[test]
    fn git_rename_onto_another_name_of_the_same_or_a_case_folded_file_is_refused() {
        // A second hard link to the source's inode is a distinct entry.
        let (_dir, pkg) = pkg_with(&[
            ("LICENSE", "license\n"),
            ("a.js", "a\n"),
            ("README.md", "readme\n"),
        ]);
        std::fs::hard_link(pkg.join("LICENSE"), pkg.join("COPYING")).unwrap();
        let rename = |from: &str, to: &str| {
            apply_multi_file_patch(
                &pkg,
                &format!("diff --git a/{from} b/{to}\nrename from {from}\nrename to {to}\n"),
            )
        };
        let err = rename("LICENSE", "COPYING").unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(pkg.join("LICENSE").exists() && pkg.join("COPYING").exists());

        // Folded onto a different entry: these collide only where the
        // filesystem is case-insensitive; on Linux the names are free. The
        // second shares the source's inode, so identity cannot tell it apart.
        let case_insensitive = pkg.join("readme.md").exists();
        let entries = || {
            let mut names: Vec<String> = std::fs::read_dir(&pkg)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let folded_onto_other = rename("a.js", "readme.MD");
        let folded_onto_hard_link = rename("LICENSE", "Copying");
        if case_insensitive {
            for result in [folded_onto_other, folded_onto_hard_link] {
                let err = result.unwrap_err();
                assert!(err.contains("already exists"), "{err}");
            }
            assert_eq!(entries(), ["COPYING", "LICENSE", "README.md", "a.js"]);
            assert_eq!(
                std::fs::read_to_string(pkg.join("README.md")).unwrap(),
                "readme\n"
            );
        } else {
            folded_onto_other.unwrap();
            folded_onto_hard_link.unwrap();
        }
    }

    // `pnpm patch-commit` diffs with `--irreversible-delete`, and git
    // writes the same header-only form for an empty file (#844).
    #[test]
    fn git_deletion_without_hunks_removes_the_file() {
        let (_dir, pkg) = pkg_with(&[("index.ts", "export {}\n"), ("keep.js", "keep\n")]);
        let patch = "diff --git a/index.ts b/index.ts\n\
                     deleted file mode 100644\n\
                     index 611bbbc777f9f82283b88a67c29f3697806eea0f..0000000000000000000000000000000000000000\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert!(!pkg.join("index.ts").exists());
        assert!(pkg.join("keep.js").exists());
    }

    #[test]
    fn git_new_file_without_hunks_creates_an_empty_file() {
        let (_dir, pkg) = pkg_with(&[]);
        let patch = "diff --git a/lib/empty.js b/lib/empty.js\n\
                     new file mode 100644\n\
                     index 0000000..e69de29\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(std::fs::read(pkg.join("lib/empty.js")).unwrap(), b"");
    }

    #[cfg(unix)]
    #[test]
    fn git_mode_change_applies_and_a_patched_file_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, pkg) = pkg_with(&[("cli.js", "old\n")]);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(pkg.join("cli.js"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let store_file = dir.path().join("store-run.sh");
        std::fs::write(&store_file, "run\n").unwrap();
        std::fs::set_permissions(&store_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&store_file, pkg.join("run.sh")).unwrap();
        let patch = "diff --git a/cli.js b/cli.js\n\
                     --- a/cli.js\n\
                     +++ b/cli.js\n\
                     @@ -1 +1 @@\n\
                     -old\n\
                     +new\n\
                     diff --git a/run.sh b/run.sh\n\
                     old mode 100644\n\
                     new mode 100755\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        assert_eq!(mode(&pkg.join("cli.js")), 0o755, "patched bin lost +x");
        assert_eq!(mode(&pkg.join("run.sh")), 0o755);
        assert_eq!(
            std::fs::read_to_string(pkg.join("run.sh")).unwrap(),
            "run\n"
        );
        assert_eq!(mode(&store_file), 0o644, "chmod must not reach the store");
    }

    // The `pnpm patch` output for `@convex-dev/resend@0.2.4` in issue #25:
    // the file's last line has no trailing newline, and pnpm omits the
    // `\ No newline at end of file` marker. pnpm + GNU `patch` apply it;
    // `git apply` + a bare `diffy::apply` reject it.
    #[test]
    fn applies_pnpm_patch_with_no_trailing_newline_and_missing_marker() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        // Pristine tarball file: final line has NO trailing newline.
        std::fs::write(
            pkg.join("shared.d.ts"),
            "export type RunMutationCtx = {\n    runMutation: GenericMutationCtx;\n};\n//# sourceMappingURL=shared.d.ts.map",
        )
        .unwrap();

        // pnpm-authored patch: the final context line carries the patch
        // file's own `\n`, and there is NO `\ No newline` marker.
        let patch = "diff --git a/shared.d.ts b/shared.d.ts\n\
                     --- a/shared.d.ts\n\
                     +++ b/shared.d.ts\n\
                     @@ -1,4 +1,4 @@\n\
                     \x20export type RunMutationCtx = {\n\
                     -    runMutation: GenericMutationCtx;\n\
                     +    runMutation: import(\"convex/server\").GenericActionCtx;\n\
                     \x20};\n\
                     \x20//# sourceMappingURL=shared.d.ts.map\n";
        apply_multi_file_patch(&pkg, patch).unwrap();
        // The patched line is applied AND the no-trailing-newline EOF
        // state is preserved (matching pnpm / GNU patch byte-for-byte).
        assert_eq!(
            std::fs::read_to_string(pkg.join("shared.d.ts")).unwrap(),
            "export type RunMutationCtx = {\n    runMutation: import(\"convex/server\").GenericActionCtx;\n};\n//# sourceMappingURL=shared.d.ts.map"
        );
    }

    // ---- Ported-applier core: helper to drive parse_hunks + apply_hunks
    // directly on a single-file body (the unit the section split feeds in).
    fn apply_body(original: &str, body: &str) -> Result<String, String> {
        let hunks = parse_hunks(body)?;
        apply_hunks(original, &hunks)
    }

    #[test]
    fn marker_terminated_no_eof_patch_preserves_eof() {
        // A patch that carries the `\ No newline at end of file` marker
        // on its final context line still applies and keeps the
        // no-trailing-newline EOF. With the newline-agnostic line array
        // this round-trips for free — the marker is informational here.
        let original = "a\nb\nlast";
        let body =
            "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n last\n\\ No newline at end of file\n";
        assert_eq!(apply_body(original, body).unwrap(), "a\nB\nlast");
    }

    #[test]
    fn newline_terminated_file_keeps_trailing_newline() {
        // A normally newline-terminated file round-trips its trailing
        // newline: `"a\nb\nc\n".split('\n')` → ["a","b","c",""], and the
        // empty final element rejoins to a trailing `\n`.
        let original = "a\nb\nc\n";
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        assert_eq!(apply_body(original, body).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn rejects_patch_whose_context_is_absent() {
        // The ported applier must NOT become "accept anything": a hunk
        // whose deletion line matches nowhere within ±20 still fails.
        let original = "a\nb\n//# map";
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-NONEXISTENT\n+X\n //# map\n";
        assert!(apply_body(original, body).is_err());
    }

    // ===== The 5 differential findings vs pnpm (`@pnpm/patch-package`).
    // Each closes a divergence the patch-applier-fidelity investigation
    // found between aube's old diffy (byte-exact) apply and pnpm's
    // lenient line-array apply. Behavior here matches pnpm exactly.

    #[test]
    fn finding1_no_eol_final_context_marker_omitted_applies() {
        // #25: file's last line has NO trailing newline and the patch
        // OMITS the `\ No newline` marker (pnpm/git routinely do). The
        // old diffy byte-exact match rejected this; the line array
        // matches and preserves the no-eol, exactly like pnpm.
        let original = "a\nb\nlast";
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n last\n";
        assert_eq!(apply_body(original, body).unwrap(), "a\nB\nlast");
    }

    #[test]
    fn finding2_trailing_ws_drift_on_context_line_tolerated() {
        // The patch's context line lacks trailing whitespace the file
        // line has (or vice versa). pnpm `trimRight`s both before
        // comparing; the old diffy byte-exact match rejected. We match.
        let original = "alpha   \nbeta\ngamma\n"; // "alpha" has trailing spaces
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n";
        assert_eq!(
            apply_body(original, body).unwrap(),
            "alpha   \nBETA\ngamma\n"
        );
    }

    #[test]
    fn finding3_trailing_ws_drift_on_deleted_line_tolerated() {
        // Same tolerance applies to a DELETED (`-`) line: the file's
        // deleted line carries trailing whitespace the patch omits.
        let original = "one\ntwo  \nthree\n"; // "two" has trailing spaces
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,2 @@\n one\n-two\n three\n";
        assert_eq!(apply_body(original, body).unwrap(), "one\nthree\n");
    }

    #[test]
    fn finding4_offset_drift_beyond_20_lines_rejected() {
        // diffy's offset search was UNBOUNDED — nub silently applied a
        // patch pnpm REFUSES. pnpm caps fuzz at ±20; beyond that it
        // throws. The hunk claims line 1, but the matching context sits
        // 30 lines down → must be REJECTED (anti-over-apply guarantee).
        let mut original = String::new();
        for _ in 0..30 {
            original.push_str("filler\n");
        }
        original.push_str("anchor\ntarget\ntail\n");
        // Hunk header says the context is at line 1, but it's at line 31.
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n anchor\n-target\n+TARGET\n tail\n";
        let err = apply_body(&original, body).unwrap_err();
        assert!(
            err.contains("could not apply hunk 1 at line 1"),
            "the rejection must say which hunk and where it looked: {err}"
        );
        assert!(
            err.contains("regenerate it"),
            "and what to do about it, since the patch is simply stale: {err}"
        );
    }

    #[test]
    fn finding4_offset_drift_within_20_lines_applies() {
        // The complement to finding 4: drift of exactly 20 lines is at
        // the boundary pnpm still accepts, so nub must apply it.
        let mut original = String::new();
        for _ in 0..20 {
            original.push_str("filler\n");
        }
        original.push_str("anchor\ntarget\ntail\n");
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n anchor\n-target\n+TARGET\n tail\n";
        let out = apply_body(&original, body).unwrap();
        assert!(out.contains("anchor\nTARGET\ntail\n"));
    }

    #[test]
    fn finding5_leading_ws_drift_rejected() {
        // pnpm's whitespace tolerance is TRAILING-only — `trimRight`,
        // never `trimLeft`. A LEADING-whitespace mismatch on a context
        // line must be REJECTED, matching pnpm (and bounding the trim).
        let original = "  indented\nbody\ntail\n"; // two leading spaces
        // Patch's context line has NO leading spaces → must not match.
        let body = "--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n indented\n-body\n+BODY\n tail\n";
        assert!(apply_body(original, body).is_err());
    }

    // ===== Happy-path byte-identity guards (g01/g05/g09 from the matrix):
    // the common case must produce the SAME bytes as before the port.

    #[test]
    fn happy_path_simple_edit_byte_identical() {
        let original = "module.exports = 'old';\n";
        let body =
            "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-module.exports = 'old';\n+module.exports = 'new';\n";
        assert_eq!(
            apply_body(original, body).unwrap(),
            "module.exports = 'new';\n"
        );
    }

    #[test]
    fn happy_path_multi_hunk_byte_identical() {
        let original = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let body = "--- a/x\n+++ b/x\n\
                    @@ -1,3 +1,3 @@\n 1\n-2\n+TWO\n 3\n\
                    @@ -6,3 +6,3 @@\n 6\n-7\n+SEVEN\n 8\n";
        assert_eq!(
            apply_body(original, body).unwrap(),
            "1\nTWO\n3\n4\n5\n6\nSEVEN\n8\n"
        );
    }

    #[test]
    fn happy_path_append_to_no_eol_file() {
        // g06: append a line to a file that ends without a newline. The
        // final inserted line should become the new no-eol EOF line, and
        // the previously-final line gains a newline.
        let original = "first\nsecond";
        // pnpm-style: the old last line `second` carries the no-newline
        // marker (deletion side), and the new content does too.
        let body = "--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n first\n-second\n\\ No newline at end of file\n+second\n+third\n\\ No newline at end of file\n";
        assert_eq!(apply_body(original, body).unwrap(), "first\nsecond\nthird");
    }
}
