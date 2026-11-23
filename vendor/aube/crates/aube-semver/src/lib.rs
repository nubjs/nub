// Ported from npm's `semver` 7.8.6 (https://github.com/npm/node-semver),
// whose notice follows. A copy is in vendor/aube/licenses/npm-semver-LICENSE.
//
// The ISC License
//
// Copyright (c) Isaac Z. Schlueter and Contributors
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR
// IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! Version ranges read and matched exactly as npm's `semver` package (7.x,
//! `includePrerelease: false`) reads and matches them, strict or `loose`.
//! npm and pnpm pick dependency versions in loose mode, which also reads
//! `1.02.3` and `1.2.3beta`; strict mode is `semver.validRange`'s default.
//!
//! The `node-semver` crate disagrees with npm on ranges real manifests use:
//! it reads `<=11` as `<=11.0.0-0` where npm reads `<12.0.0-0`, it treats two
//! comparators that cannot both hold (`>=2 <1`) as alternatives rather than
//! an empty set, it drops the comparator before a comma, and it accepts
//! leading zeros (`1.02.3`) strict npm rejects. Each of those picks a
//! version npm, pnpm and yarn would not, with no error.
//!
//! So this is a port of npm's own algorithm rather than a grammar of its
//! own: the same regular expressions, the same rewrite passes in the same
//! order, and comparators kept as a list per `||` set, every one of which
//! must hold. Arithmetic that npm does in
//! floating point is checked here; a result past `Number.MAX_SAFE_INTEGER`
//! is a range npm rejects, so it is rejected, never wrapped.

use regex::{Captures, Regex};
use std::cmp::Ordering;
use std::fmt;
use std::sync::LazyLock;

const MAX_LENGTH: usize = 256;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// npm's `src` tokens, with `\d` spelled `[0-9]` because Rust's `\d` is
/// Unicode-aware. npm runs the `safeRe` forms, which bound each repetition
/// (`\d*` to 256 digits, an identifier to 250 characters) against
/// backtracking. Loose mode keeps those bounds, because it drops a
/// comparator it cannot read rather than rejecting the range, so a bound
/// decides what survives. Strict mode repeats freely: Rust's engine cannot
/// backtrack, bounded repetition multiplies its compile time, and whatever
/// only a bound refuses in strict mode is refused anyway, by
/// `MAX_SAFE_INTEGER` or by the 256-character limit on a version. `\s*` is
/// always the bounded `\s?`, which runs of whitespace never reach: a range's
/// whitespace is collapsed to single spaces first.
struct Src {
    numeric: &'static str,
    numeric_loose: &'static str,
    non_numeric: &'static str,
    build_id: &'static str,
}

const FREE: Src = Src {
    numeric: "0|[1-9][0-9]*",
    numeric_loose: "[0-9]+",
    non_numeric: "[0-9]*[a-zA-Z-][a-zA-Z0-9-]*",
    build_id: "[a-zA-Z0-9-]+",
};

const SAFE: Src = Src {
    numeric: "0|[1-9][0-9]{0,256}",
    numeric_loose: "[0-9]{1,256}",
    non_numeric: "[0-9]{0,256}[a-zA-Z-][a-zA-Z0-9-]{0,250}",
    build_id: "[a-zA-Z0-9-]{1,250}",
};

impl Src {
    fn main_version(&self, n: &str) -> String {
        format!(r"({n})\.({n})\.({n})")
    }

    fn prerelease(&self) -> String {
        let id = format!("(?:{}|{})", self.non_numeric, self.numeric);
        format!(r"(?:-({id}(?:\.{id})*))")
    }

    fn prerelease_loose(&self) -> String {
        let id = format!("(?:{}|{})", self.non_numeric, self.numeric_loose);
        format!(r"(?:-?({id}(?:\.{id})*))")
    }

    fn build(&self) -> String {
        format!(r"(?:\+({id}(?:\.{id})*))", id = self.build_id)
    }

    fn full_plain(&self) -> String {
        format!(
            "v?{}{}?{}?",
            self.main_version(self.numeric),
            self.prerelease(),
            self.build()
        )
    }

    fn loose_plain(&self) -> String {
        format!(
            r"[v=\s]*{}{}?{}?",
            self.main_version(self.numeric_loose),
            self.prerelease_loose(),
            self.build()
        )
    }

    fn xrange_plain(&self, numeric: &str, prerelease: String) -> String {
        let id = format!(r"{numeric}|x|X|\*");
        format!(
            r"[v=\s]*({id})(?:\.({id})(?:\.({id})(?:{prerelease})?{}?)?)?",
            self.build()
        )
    }
}

const GTLT: &str = "((?:<|>)?=?)";

/// ECMAScript's `\s`, which is also the set `String.prototype.trim` strips:
/// Rust's `\s` and `char::is_whitespace` add U+0085 and leave out U+FEFF.
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\x0B' | '\x0C' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// JavaScript's `s.trim()`, which is how npm trims a range before reading
/// it. Trim a spec with this, never `str::trim`, before handing it here.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// Compiles one of npm's patterns, written here with `\s` as npm writes it,
/// with `\s` read as ECMAScript reads it.
fn re(pattern: String) -> Regex {
    let pattern = pattern
        .replace(r"[v=\s]", &format!("[v={JS_SPACE}]"))
        .replace(r"\s", &format!("[{JS_SPACE}]"));
    Regex::new(&pattern).expect("npm semver pattern compiles")
}

/// npm's `loose` option.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Strict,
    Loose,
}

/// The regexes one mode reads with: npm's `*LOOSE` variants in loose mode,
/// and the shared ones built from that mode's tokens.
struct Grammar {
    version: Regex,
    comparator: Regex,
    xrange: Regex,
    tilde: Regex,
    caret: Regex,
    hyphen: Regex,
    comparator_trim: Regex,
    build: Regex,
}

impl Grammar {
    fn new(src: &Src, mode: Mode) -> Self {
        let strict_xrange = src.xrange_plain(src.numeric, src.prerelease());
        let (plain, xrange) = match mode {
            Mode::Strict => (src.full_plain(), strict_xrange.clone()),
            Mode::Loose => (
                src.loose_plain(),
                src.xrange_plain(src.numeric_loose, src.prerelease_loose()),
            ),
        };
        Grammar {
            version: re(format!("^{plain}$")),
            comparator: re(format!(r"^{GTLT}\s?({plain})$|^$")),
            xrange: re(format!(r"^{GTLT}\s?{xrange}$")),
            tilde: re(format!("^(?:~>?){xrange}$")),
            caret: re(format!(r"^(?:\^){xrange}$")),
            hyphen: re(format!(r"^\s?({xrange})\s-\s({xrange})\s?$")),
            comparator_trim: re(format!(
                r"(\s?){GTLT}\s?({}|{strict_xrange})",
                src.loose_plain()
            )),
            build: re(src.build()),
        }
    }
}

static STRICT: LazyLock<Grammar> = LazyLock::new(|| Grammar::new(&FREE, Mode::Strict));
static LOOSE: LazyLock<Grammar> = LazyLock::new(|| Grammar::new(&SAFE, Mode::Loose));

impl Mode {
    fn grammar(self) -> &'static Grammar {
        match self {
            Mode::Strict => &STRICT,
            Mode::Loose => &LOOSE,
        }
    }
}

static TILDE_TRIM: LazyLock<Regex> = LazyLock::new(|| re(r"(\s?)(?:~>?)\s".to_string()));
static CARET_TRIM: LazyLock<Regex> = LazyLock::new(|| re(r"(\s?)(?:\^)\s".to_string()));
static STAR: LazyLock<Regex> = LazyLock::new(|| re(r"(<|>)?=?\s?\*".to_string()));
static GTE0: LazyLock<Regex> = LazyLock::new(|| re(r"^\s?>=\s?0\.0\.0\s?$".to_string()));
/// npm strips build metadata with the unbounded `src` form, globally.
static BUILD_STRIP: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?:\+([a-zA-Z0-9-]+(?:\.[a-zA-Z0-9-]+)*))".to_string()));
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| re(r"\s+".to_string()));

/// One dot-separated prerelease identifier. A numeric one keeps its digits
/// without leading zeros (loose mode reads `01` as `1`, as npm does), so any
/// length compares exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Identifier {
    Numeric(String),
    Alphanumeric(String),
}

impl Identifier {
    fn parse(id: &str) -> Self {
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
            let digits = id.trim_start_matches('0');
            Identifier::Numeric(if digits.is_empty() { "0" } else { digits }.to_string())
        } else {
            Identifier::Alphanumeric(id.to_string())
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Identifier::Numeric(s) | Identifier::Alphanumeric(s) => s,
        }
    }
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Identifier::Numeric(a), Identifier::Numeric(b)) => {
                a.len().cmp(&b.len()).then_with(|| a.cmp(b))
            }
            (Identifier::Numeric(_), Identifier::Alphanumeric(_)) => Ordering::Less,
            (Identifier::Alphanumeric(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Alphanumeric(a), Identifier::Alphanumeric(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A version as npm's `new SemVer(v)` reads it. Build metadata is dropped,
/// since nothing compares it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    prerelease: Vec<Identifier>,
}

impl Version {
    /// `None` wherever npm's `semver.valid` returns `null`: leading zeros,
    /// a part past `Number.MAX_SAFE_INTEGER`, more than 256 characters.
    pub fn parse(version: &str) -> Option<Self> {
        Self::parse_in(version, Mode::Strict)
    }

    /// `semver.valid(version, { loose: true })`.
    pub fn parse_loose(version: &str) -> Option<Self> {
        Self::parse_in(version, Mode::Loose)
    }

    fn parse_in(version: &str, mode: Mode) -> Option<Self> {
        if version.encode_utf16().count() > MAX_LENGTH {
            return None;
        }
        let caps = mode.grammar().version.captures(js_trim(version))?;
        let part =
            |i: usize| -> Option<u64> { caps[i].parse().ok().filter(|n| *n <= MAX_SAFE_INTEGER) };
        Some(Version {
            major: part(1)?,
            minor: part(2)?,
            patch: part(3)?,
            prerelease: caps
                .get(4)
                .map(|pr| pr.as_str().split('.').map(Identifier::parse).collect())
                .unwrap_or_default(),
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }

    fn same_release(&self, other: &Self) -> bool {
        (self.major, self.minor, self.patch) == (other.major, other.minor, other.patch)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.prerelease.cmp(&other.prerelease),
                },
            )
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (i, id) in self.prerelease.iter().enumerate() {
            f.write_str(if i == 0 { "-" } else { "." })?;
            f.write_str(id.as_str())?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "",
            Op::Gt => ">",
            Op::Gte => ">=",
            Op::Lt => "<",
            Op::Lte => "<=",
        }
    }
}

/// One comparator; `None` is npm's `ANY`, the empty comparator.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Comparator(Option<(Op, Version)>);

impl Comparator {
    fn parse(comp: &str, mode: Mode) -> Option<Self> {
        let comp = WHITESPACE
            .split(js_trim(comp))
            .collect::<Vec<_>>()
            .join(" ");
        let caps = mode.grammar().comparator.captures(&comp)?;
        let Some(version) = caps.get(2) else {
            return Some(Comparator(None));
        };
        let op = match caps.get(1).map_or("", |m| m.as_str()) {
            "" | "=" => Op::Eq,
            ">" => Op::Gt,
            ">=" => Op::Gte,
            "<" => Op::Lt,
            _ => Op::Lte,
        };
        Some(Comparator(Some((
            op,
            Version::parse_in(version.as_str(), mode)?,
        ))))
    }

    fn test(&self, version: &Version) -> bool {
        let Some((op, bound)) = &self.0 else {
            return true;
        };
        let ord = version.cmp(bound);
        match op {
            Op::Eq => ord == Ordering::Equal,
            Op::Gt => ord == Ordering::Greater,
            Op::Gte => ord != Ordering::Less,
            Op::Lt => ord == Ordering::Less,
            Op::Lte => ord != Ordering::Greater,
        }
    }

    fn is_null_set(&self) -> bool {
        self.to_string() == "<0.0.0-0"
    }
}

impl fmt::Display for Comparator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            None => Ok(()),
            Some((op, version)) => write!(f, "{}{version}", op.as_str()),
        }
    }
}

/// A range as npm's `new Range(r)` reads it: `||`-separated sets, each a
/// list of comparators that must all hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Range {
    sets: Vec<Vec<Comparator>>,
    mode: Mode,
}

impl Range {
    /// `None` wherever npm's `semver.validRange` returns `null`.
    pub fn parse(range: &str) -> Option<Self> {
        Self::parse_in(range, Mode::Strict)
    }

    /// `semver.validRange(range, { loose: true })`, the reading npm and pnpm
    /// pick dependency versions with. A comparator loose mode cannot read is
    /// dropped rather than rejected, as npm drops it.
    pub fn parse_loose(range: &str) -> Option<Self> {
        Self::parse_in(range, Mode::Loose)
    }

    fn parse_in(range: &str, mode: Mode) -> Option<Self> {
        let raw = WHITESPACE.replace_all(js_trim(range), " ");
        let mut sets = raw
            .split("||")
            .map(|set| parse_set(js_trim(set), mode))
            .collect::<Option<Vec<_>>>()?;
        sets.retain(|set| !set.is_empty());
        if sets.is_empty() {
            return None;
        }
        if sets.len() > 1 {
            let first = sets[0].clone();
            sets.retain(|set| !set[0].is_null_set());
            if sets.is_empty() {
                sets = vec![first];
            } else if let Some(any) = sets
                .iter()
                .find(|set| set.len() == 1 && set[0].0.is_none())
                .cloned()
            {
                sets = vec![any];
            }
        }
        Some(Range { sets, mode })
    }

    /// npm's `range.test(version)`.
    pub fn satisfies(&self, version: &Version) -> bool {
        self.sets.iter().any(|set| test_set(set, version))
    }

    /// npm's `semver.satisfies(version, range)` for a range already parsed,
    /// reading the version in the range's own mode: a version npm cannot
    /// parse satisfies nothing.
    pub fn satisfies_str(&self, version: &str) -> bool {
        Version::parse_in(version, self.mode).is_some_and(|v| self.satisfies(&v))
    }
}

impl fmt::Display for Range {
    /// What `semver.validRange` returns: npm's `range.range`, or `*` for
    /// the range that admits every release.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sets: Vec<String> = self.sets.iter().map(|set| format_set(set)).collect();
        match sets.join("||") {
            any if any.is_empty() => f.write_str("*"),
            range => f.write_str(&range),
        }
    }
}

/// npm's `semver.satisfies(version, range)`: false when either fails to parse.
pub fn satisfies(version: &str, range: &str) -> bool {
    Range::parse(range).is_some_and(|r| r.satisfies_str(version))
}

fn format_set(set: &[Comparator]) -> String {
    set.iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// npm's `testSet`: every comparator holds, and a prerelease only matches a
/// set that names a prerelease of the same `major.minor.patch`.
fn test_set(set: &[Comparator], version: &Version) -> bool {
    if !set.iter().all(|c| c.test(version)) {
        return false;
    }
    if !version.is_prerelease() {
        return true;
    }
    set.iter().any(|c| {
        c.0.as_ref()
            .is_some_and(|(_, bound)| bound.is_prerelease() && bound.same_release(version))
    })
}

/// npm's `parseRange`: the rewrite passes, in npm's order, over one set.
fn parse_set(set: &str, mode: Mode) -> Option<Vec<Comparator>> {
    let grammar = mode.grammar();
    let set = BUILD_STRIP.replace_all(set, "");
    let set = match grammar.hyphen.captures(&set) {
        Some(caps) => hyphen_replace(&caps)?,
        None => set.into_owned(),
    };
    let set = grammar
        .comparator_trim
        .replace_all(&set, |c: &Captures| format!("{}{}{}", &c[1], &c[2], &c[3]));
    let set = TILDE_TRIM.replace_all(&set, "${1}~");
    let set = CARET_TRIM.replace_all(&set, "${1}^");
    let rewritten = set
        .split(' ')
        .map(|comp| parse_comparator(comp, grammar))
        .collect::<Option<Vec<_>>>()?
        .join(" ");
    let comparators = WHITESPACE
        .split(&rewritten)
        .map(|comp| GTE0.replace(js_trim(comp), "").into_owned())
        .filter(|comp| mode == Mode::Strict || grammar.comparator.is_match(comp))
        .map(|comp| Comparator::parse(&comp, mode))
        .collect::<Option<Vec<_>>>()?;
    if let Some(null) = comparators.iter().find(|c| c.is_null_set()) {
        return Some(vec![null.clone()]);
    }
    let mut unique: Vec<Comparator> = Vec::new();
    for comp in comparators {
        if !unique.contains(&comp) {
            unique.push(comp);
        }
    }
    if unique.len() > 1 {
        unique.retain(|c| c.0.is_some());
    }
    Some(unique)
}

/// npm's `parseComparator`: carets, tildes, x-ranges, then stars. `None`
/// where npm's floating-point arithmetic would leave a part past
/// `Number.MAX_SAFE_INTEGER`, which npm then rejects.
fn parse_comparator(comp: &str, grammar: &Grammar) -> Option<String> {
    let comp = grammar.build.replace(comp, "");
    let comp = each_token(js_trim(&comp), |c| replace_caret(c, &grammar.caret))?;
    let comp = each_token(js_trim(&comp), |c| replace_tilde(c, &grammar.tilde))?;
    let comp = WHITESPACE
        .split(&comp)
        .map(|c| replace_xrange(c, &grammar.xrange))
        .collect::<Option<Vec<_>>>()?
        .join(" ");
    Some(STAR.replace(js_trim(&comp), "").into_owned())
}

fn each_token(comp: &str, f: impl Fn(&str) -> Option<String>) -> Option<String> {
    Some(
        WHITESPACE
            .split(comp)
            .map(f)
            .collect::<Option<Vec<_>>>()?
            .join(" "),
    )
}

fn is_x(id: &str) -> bool {
    id.is_empty() || id.eq_ignore_ascii_case("x") || id == "*"
}

/// `+n + 1`, checked: `None` past what npm can represent.
fn inc(n: &str) -> Option<String> {
    n.parse::<u64>()
        .ok()?
        .checked_add(1)
        .filter(|n| *n <= MAX_SAFE_INTEGER)
        .map(|n| n.to_string())
}

fn group<'a>(caps: &'a Captures, i: usize) -> &'a str {
    caps.get(i).map_or("", |m| m.as_str())
}

fn replace_tilde(comp: &str, tilde: &Regex) -> Option<String> {
    let Some(caps) = tilde.captures(comp) else {
        return Some(comp.to_string());
    };
    let (major, minor, patch, pr) = (
        group(&caps, 1),
        group(&caps, 2),
        group(&caps, 3),
        group(&caps, 4),
    );
    Some(if is_x(major) {
        String::new()
    } else if is_x(minor) {
        format!(">={major}.0.0 <{}.0.0-0", inc(major)?)
    } else if is_x(patch) {
        format!(">={major}.{minor}.0 <{major}.{}.0-0", inc(minor)?)
    } else if !pr.is_empty() {
        format!(
            ">={major}.{minor}.{patch}-{pr} <{major}.{}.0-0",
            inc(minor)?
        )
    } else {
        format!(">={major}.{minor}.{patch} <{major}.{}.0-0", inc(minor)?)
    })
}

fn replace_caret(comp: &str, caret: &Regex) -> Option<String> {
    let Some(caps) = caret.captures(comp) else {
        return Some(comp.to_string());
    };
    let (major, minor, patch, pr) = (
        group(&caps, 1),
        group(&caps, 2),
        group(&caps, 3),
        group(&caps, 4),
    );
    let pr = if pr.is_empty() {
        String::new()
    } else {
        format!("-{pr}")
    };
    Some(if is_x(major) {
        String::new()
    } else if is_x(minor) {
        format!(">={major}.0.0 <{}.0.0-0", inc(major)?)
    } else if is_x(patch) {
        if major == "0" {
            format!(">={major}.{minor}.0 <{major}.{}.0-0", inc(minor)?)
        } else {
            format!(">={major}.{minor}.0 <{}.0.0-0", inc(major)?)
        }
    } else if major == "0" {
        if minor == "0" {
            format!(
                ">={major}.{minor}.{patch}{pr} <{major}.{minor}.{}-0",
                inc(patch)?
            )
        } else {
            format!(">={major}.{minor}.{patch}{pr} <{major}.{}.0-0", inc(minor)?)
        }
    } else {
        format!(">={major}.{minor}.{patch}{pr} <{}.0.0-0", inc(major)?)
    })
}

fn replace_xrange(comp: &str, xrange: &Regex) -> Option<String> {
    let comp = js_trim(comp);
    let Some(caps) = xrange.captures(comp) else {
        return Some(comp.to_string());
    };
    let (gtlt, major, minor, patch) = (
        group(&caps, 1),
        group(&caps, 2),
        group(&caps, 3),
        group(&caps, 4),
    );
    if (is_x(major) && !is_x(minor)) || (is_x(minor) && !patch.is_empty() && !is_x(patch)) {
        return Some(comp.to_string());
    }
    let xm = is_x(major) || is_x(minor);
    let any_x = xm || is_x(patch);
    let gtlt = if gtlt == "=" && any_x { "" } else { gtlt };
    Some(if is_x(major) {
        if gtlt == ">" || gtlt == "<" {
            "<0.0.0-0"
        } else {
            "*"
        }
        .to_string()
    } else if !gtlt.is_empty() && any_x {
        let minor = if xm { "0" } else { minor };
        let (gtlt, major, minor) = match gtlt {
            ">" if xm => (">=", inc(major)?, "0".to_string()),
            ">" => (">=", major.to_string(), inc(minor)?),
            "<=" if xm => ("<", inc(major)?, minor.to_string()),
            "<=" => ("<", major.to_string(), inc(minor)?),
            _ => (gtlt, major.to_string(), minor.to_string()),
        };
        let pr = if gtlt == "<" { "-0" } else { "" };
        format!("{gtlt}{major}.{minor}.0{pr}")
    } else if xm {
        format!(">={major}.0.0 <{}.0.0-0", inc(major)?)
    } else if any_x {
        format!(">={major}.{minor}.0 <{major}.{}.0-0", inc(minor)?)
    } else {
        comp.to_string()
    })
}

/// npm's `hyphenReplace`: `1.2 - 3` → `>=1.2.0 <4.0.0-0`.
fn hyphen_replace(caps: &Captures) -> Option<String> {
    let (from, f_major, f_minor, f_patch) = (
        group(caps, 1),
        group(caps, 2),
        group(caps, 3),
        group(caps, 4),
    );
    let (to, t_major, t_minor, t_patch, t_pr) = (
        group(caps, 7),
        group(caps, 8),
        group(caps, 9),
        group(caps, 10),
        group(caps, 11),
    );
    let from = if is_x(f_major) {
        String::new()
    } else if is_x(f_minor) {
        format!(">={f_major}.0.0")
    } else if is_x(f_patch) {
        format!(">={f_major}.{f_minor}.0")
    } else {
        format!(">={from}")
    };
    let to = if is_x(t_major) {
        String::new()
    } else if is_x(t_minor) {
        format!("<{}.0.0-0", inc(t_major)?)
    } else if is_x(t_patch) {
        format!("<{t_major}.{}.0-0", inc(t_minor)?)
    } else if !t_pr.is_empty() {
        format!("<={t_major}.{t_minor}.{t_patch}-{t_pr}")
    } else {
        format!("<={to}")
    };
    Some(js_trim(&format!("{from} {to}")).to_string())
}

#[cfg(test)]
mod tests;

/// A spec npm reads as no range at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidRange(pub String);

impl fmt::Display for InvalidRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid semver range `{}`", self.0)
    }
}

impl std::error::Error for InvalidRange {}

/// npm's loose reading of `spec`: how npm and pnpm pick dependency versions,
/// and the one way the engine reads a dependency range.
///
/// An empty spec is refused, though npm reads it as `*`: the engine's
/// callers that mean `*` by it say so first (`normalize_range`), while the
/// rest rely on the refusal, which they had from `node-semver`.
pub fn parse_range(spec: &str) -> Result<Range, InvalidRange> {
    if js_trim(spec).is_empty() {
        return Err(InvalidRange(spec.to_string()));
    }
    Range::parse_loose(spec).ok_or_else(|| InvalidRange(spec.to_string()))
}

impl Range {
    /// Whether some set of each leaves a version between the bounds both
    /// sets' comparators put on it, ignoring npm's prerelease rule.
    ///
    /// A conservative overlap, wider than npm's `intersects`: it answers
    /// "could a version here also be there", never "does one exist", which
    /// is what a caller that refetches or keeps a fallback when unsure asks.
    pub fn overlaps(&self, other: &Range) -> bool {
        self.sets.iter().any(|ours| {
            other
                .sets
                .iter()
                .any(|theirs| bounds_leave_room(ours.iter().chain(theirs)))
        })
    }

    /// npm's `range.intersects(other)`: some set of each could share a
    /// version, judged comparator by comparator as npm judges it.
    pub fn intersects(&self, other: &Range) -> bool {
        self.sets.iter().any(|ours| {
            set_is_satisfiable(ours)
                && other.sets.iter().any(|theirs| {
                    set_is_satisfiable(theirs)
                        && ours.iter().all(|a| theirs.iter().all(|b| a.intersects(b)))
                })
        })
    }

    /// The range both admit: every set of one joined with every set of the
    /// other, each joined set needing all comparators of both.
    pub fn intersect(&self, other: &Range) -> Range {
        let mut sets = Vec::new();
        for ours in &self.sets {
            for theirs in &other.sets {
                let mut joined: Vec<Comparator> = ours
                    .iter()
                    .chain(theirs)
                    .filter(|c| c.0.is_some())
                    .cloned()
                    .collect();
                if joined.is_empty() {
                    joined.push(Comparator(None));
                }
                sets.push(joined);
            }
        }
        Range {
            sets,
            mode: self.mode,
        }
    }

    /// npm's `semver.minVersion(range)`: the lowest version the range admits,
    /// or `None` when it admits none.
    pub fn min_version(&self) -> Option<Version> {
        let zero = |pre: bool| Version {
            major: 0,
            minor: 0,
            patch: 0,
            prerelease: if pre {
                vec![Identifier::Numeric("0".into())]
            } else {
                vec![]
            },
        };
        for candidate in [zero(false), zero(true)] {
            if self.satisfies(&candidate) {
                return Some(candidate);
            }
        }
        let mut min: Option<Version> = None;
        for set in &self.sets {
            let mut set_min: Option<Version> = None;
            for (op, bound) in set.iter().filter_map(|c| c.0.as_ref()) {
                let mut v = bound.clone();
                match op {
                    Op::Gt if v.prerelease.is_empty() => v.patch += 1,
                    Op::Gt => v.prerelease.push(Identifier::Numeric("0".into())),
                    Op::Eq | Op::Gte => {}
                    Op::Lt | Op::Lte => continue,
                }
                if set_min.as_ref().is_none_or(|m| v > *m) {
                    set_min = Some(v);
                }
            }
            if let Some(v) = set_min
                && min.as_ref().is_none_or(|m| *m > v)
            {
                min = Some(v);
            }
        }
        min.filter(|v| self.satisfies(v))
    }
}

impl Comparator {
    /// npm's `Comparator#intersects` with `includePrerelease: false`.
    fn intersects(&self, other: &Comparator) -> bool {
        let (Some((a_op, a)), Some((b_op, b))) = (&self.0, &other.0) else {
            return true;
        };
        if *a_op == Op::Eq {
            return Comparator(Some((*b_op, b.clone()))).test_alone(a);
        }
        if *b_op == Op::Eq {
            return Comparator(Some((*a_op, a.clone()))).test_alone(b);
        }
        // `value.startsWith('<0.0.0')`: below every release.
        let below_zero =
            |op: Op, v: &Version| op == Op::Lt && (v.major, v.minor, v.patch) == (0, 0, 0);
        if below_zero(*a_op, a) || below_zero(*b_op, b) {
            return false;
        }
        let gt = |op: Op| matches!(op, Op::Gt | Op::Gte);
        let inclusive = |op: Op| matches!(op, Op::Gte | Op::Lte);
        (gt(*a_op) && gt(*b_op))
            || (!gt(*a_op) && !gt(*b_op))
            || (a == b && inclusive(*a_op) && inclusive(*b_op))
            || (a < b && gt(*a_op) && !gt(*b_op))
            || (a > b && !gt(*a_op) && gt(*b_op))
    }

    /// `new Range(this.value).test(version)`: the comparator as a one-set
    /// range, prerelease rule included.
    fn test_alone(&self, version: &Version) -> bool {
        test_set(std::slice::from_ref(self), version)
    }
}

/// npm's `isSatisfiable`: every comparator intersects every other.
fn set_is_satisfiable(set: &[Comparator]) -> bool {
    set.iter()
        .enumerate()
        .all(|(i, a)| set[i + 1..].iter().all(|b| a.intersects(b)))
}

/// Whether the lower bounds (`>`, `>=`, `=`) and upper bounds (`<`, `<=`,
/// `=`) of `comparators` leave any room between them.
fn bounds_leave_room<'a>(comparators: impl Iterator<Item = &'a Comparator>) -> bool {
    let mut lower: Option<(&Version, bool)> = None;
    let mut upper: Option<(&Version, bool)> = None;
    for (op, v) in comparators.filter_map(|c| c.0.as_ref()) {
        let (is_lower, is_upper, inclusive) = match op {
            Op::Eq => (true, true, true),
            Op::Gt => (true, false, false),
            Op::Gte => (true, false, true),
            Op::Lt => (false, true, false),
            Op::Lte => (false, true, true),
        };
        if is_lower && lower.is_none_or(|(l, inc)| v > l || (v == l && inc && !inclusive)) {
            lower = Some((v, inclusive));
        }
        if is_upper && upper.is_none_or(|(u, inc)| v < u || (v == u && inc && !inclusive)) {
            upper = Some((v, inclusive));
        }
    }
    match (lower, upper) {
        (Some((l, l_inc)), Some((u, u_inc))) => l < u || (l == u && l_inc && u_inc),
        _ => true,
    }
}

#[cfg(feature = "node-semver")]
impl From<&node_semver::Version> for Version {
    fn from(v: &node_semver::Version) -> Self {
        Version {
            major: v.major,
            minor: v.minor,
            patch: v.patch,
            prerelease: v
                .pre_release
                .iter()
                .map(|id| match id {
                    node_semver::Identifier::Numeric(n) => Identifier::Numeric(n.to_string()),
                    node_semver::Identifier::AlphaNumeric(s) => Identifier::parse(s),
                })
                .collect(),
        }
    }
}

#[cfg(feature = "node-semver")]
impl Range {
    /// [`Range::satisfies`] for a version the engine parsed with `node-semver`,
    /// which still reads published versions; ranges are read only here.
    pub fn satisfies_node(&self, version: &node_semver::Version) -> bool {
        self.satisfies(&Version::from(version))
    }
}
