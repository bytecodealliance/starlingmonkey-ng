// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Glob matching as the minimatch that Node.js bundles implements it, with the options Node.js
//! passes: no negation, no comments, `windowsPathsNoEscape` and `optimizationLevel` 2. This is
//! `path.matchesGlob` and the glob form of `fs.watch`'s `ignore` option.
//!
//! A pattern is brace-expanded, split into path segments, and each segment becomes a literal, a
//! globstar or a regular expression. The regular expressions are minimatch's, and run on the JS
//! engine's `RegExp`. Matching follows minimatch's POSIX rules on every platform.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::{Function, Object};

/// The options of a [`Glob`] that differ between Node's uses of minimatch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlobOptions {
    /// Whether segments with magic match case-insensitively.
    pub nocase: bool,
    /// With `nocase`, whether segments without magic still match case-sensitively.
    pub nocase_magic_only: bool,
    /// Whether a pattern of one segment matches the last segment of a path.
    pub match_base: bool,
}

/// Whether Node.js matches globs case-insensitively: where `process.platform` is `darwin` or
/// `win32`.
pub(crate) fn platform_nocase() -> bool {
    matches!(crate::process::platform(), "darwin" | "win32")
}

/// The longest pattern minimatch accepts, in UTF-16 code units.
const MAX_PATTERN_LENGTH: usize = 1024 * 64;

/// A compiled glob pattern.
pub(crate) struct Glob {
    /// Whether the pattern is empty, which only matches the empty path.
    empty: bool,
    /// The alternatives the pattern expands to, each a list of segment matchers.
    set: Vec<Vec<Part>>,
    options: GlobOptions,
}

/// The matcher of one path segment.
#[derive(Debug)]
enum Part {
    /// `**`, which matches any number of segments.
    Globstar,
    /// A segment matched by string comparison.
    Literal(String),
    /// A segment matched by a regular expression.
    Regex(Regex),
}

/// A regular expression matching a segment, with the faster test minimatch uses in its place for
/// some patterns.
#[derive(Debug)]
struct Regex {
    source: String,
    flags: String,
    fast: Option<FastTest>,
}

/// The tests minimatch substitutes for the regular expressions of common segment patterns.
#[derive(Debug)]
enum FastTest {
    /// `*`: a non-empty name not starting with `.`.
    Star,
    /// `*` followed by text without magic: a name not starting with `.` that ends with the text.
    StarDotExt(String),
    /// `?`s followed by text without magic: a name of the pattern's length not starting with `.`
    /// that ends with the text.
    Qmarks { length: usize, ext: String },
    /// `*.*`: a name not starting with `.` that contains a `.`.
    StarDotStar,
    /// `.*`: a name starting with `.` other than `.` and `..`.
    DotStar,
}

impl FastTest {
    fn test(&self, name: &str, nocase: bool) -> bool {
        let ends_with = |ext: &str| {
            if nocase {
                name.to_lowercase().ends_with(&ext.to_lowercase())
            } else {
                name.ends_with(ext)
            }
        };
        match self {
            FastTest::Star => !name.is_empty() && !name.starts_with('.'),
            FastTest::StarDotExt(ext) => !name.starts_with('.') && ends_with(ext),
            FastTest::Qmarks { length, ext } => {
                name.encode_utf16().count() == *length
                    && !name.starts_with('.')
                    && (ext.is_empty() || ends_with(ext))
            }
            FastTest::StarDotStar => !name.starts_with('.') && name.contains('.'),
            FastTest::DotStar => name != "." && name != ".." && name.starts_with('.'),
        }
    }

    /// The fast test for the segment pattern `pattern`, if minimatch has one.
    fn of(pattern: &str) -> Option<FastTest> {
        let is_ext = |text: &str| !text.contains(['+', '@', '!', '?', '*', '[', '(']);
        let stars = pattern.len() - pattern.trim_start_matches('*').len();
        let qmarks = pattern.len() - pattern.trim_start_matches('?').len();
        if stars > 0 && stars == pattern.len() {
            Some(FastTest::Star)
        } else if stars > 0 && is_ext(&pattern[stars..]) {
            Some(FastTest::StarDotExt(pattern[stars..].to_string()))
        } else if qmarks > 0 && is_ext(&pattern[qmarks..]) {
            Some(FastTest::Qmarks {
                length: pattern.encode_utf16().count(),
                ext: pattern[qmarks..].to_string(),
            })
        } else if stars > 0
            && pattern[stars..].starts_with('.')
            && !pattern[stars + 1..].is_empty()
            && pattern[stars + 1..].bytes().all(|b| b == b'*')
        {
            Some(FastTest::StarDotStar)
        } else if pattern.len() > 1
            && pattern.starts_with('.')
            && pattern[1..].bytes().all(|b| b == b'*')
        {
            Some(FastTest::DotStar)
        } else {
            None
        }
    }
}

/// Whether a segment is `.`, `..` or a dotfile name, which globstars don't match.
fn is_dotted(segment: &str) -> bool {
    segment.starts_with('.')
}

impl Glob {
    /// Compile `pattern`. A pattern longer than minimatch accepts throws a `TypeError`.
    pub(crate) fn new(
        scope: &Scope<'_>,
        pattern: &str,
        options: GlobOptions,
    ) -> Result<Glob, ExnThrown> {
        if pattern.encode_utf16().count() > MAX_PATTERN_LENGTH {
            return Err(js::error::TypeError("pattern is too long".into()).throw(scope));
        }
        let pattern = pattern.replace('\\', "/");
        if pattern.is_empty() {
            return Ok(Glob {
                empty: true,
                set: Vec::new(),
                options,
            });
        }
        let mut glob_set: Vec<String> = Vec::new();
        for expanded in brace_expand(&pattern) {
            if !glob_set.contains(&expanded) {
                glob_set.push(expanded);
            }
        }
        let glob_parts = glob_set.iter().map(|glob| slash_split(glob)).collect();
        let glob_parts = second_phase(first_phase(glob_parts));
        let set: Vec<Vec<Part>> = glob_parts
            .iter()
            .map(|parts| parts.iter().map(|part| parse_part(part, options)).collect())
            .collect();
        // We compile every regular expression here so that an invalid one throws a `SyntaxError`
        // whatever the path, as in Node.js.
        for part in set.iter().flatten() {
            if let Part::Regex(regex) = part {
                compile_regex(scope, &regex.source, &regex.flags)?;
            }
        }
        Ok(Glob {
            empty: false,
            set,
            options,
        })
    }

    /// Whether `path` matches the pattern.
    pub(crate) fn matches(&self, scope: &Scope<'_>, path: &str) -> Result<bool, ExnThrown> {
        if self.empty {
            return Ok(path.is_empty());
        }
        let segments = slash_split(path);
        let mut filename = segments.last().cloned().unwrap_or_default();
        let mut index = segments.len() as isize - 2;
        while filename.is_empty() && index >= 0 {
            filename = segments[index as usize].clone();
            index -= 1;
        }
        for pattern in &self.set {
            let file = if self.options.match_base && pattern.len() == 1 {
                vec![filename.clone()]
            } else {
                segments.clone()
            };
            if self.match_one(scope, level_two_file_optimize(file), pattern)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// minimatch's `matchOne`: whether the segments `file` match `pattern`.
    fn match_one(
        &self,
        scope: &Scope<'_>,
        file: Vec<String>,
        pattern: &[Part],
    ) -> Result<bool, ExnThrown> {
        if pattern.iter().any(|part| matches!(part, Part::Globstar)) {
            self.match_globstar(scope, &file, pattern)
        } else {
            self.match_segments(scope, &file, pattern, 0, 0)
        }
    }

    /// Whether `segment` matches `part`, which isn't a globstar.
    fn test(&self, scope: &Scope<'_>, part: &Part, segment: &str) -> Result<bool, ExnThrown> {
        match part {
            Part::Globstar => Ok(false),
            Part::Literal(literal) => Ok(literal == segment),
            Part::Regex(regex) => match &regex.fast {
                Some(fast) => Ok(fast.test(segment, self.options.nocase)),
                None => regex_test(scope, &regex.source, &regex.flags, segment),
            },
        }
    }

    /// minimatch's private `#matchOne`: whether `file` from `file_index` matches `pattern`, which
    /// has no globstar, from `pattern_index`. A trailing empty segment of `file` is ignored.
    fn match_segments(
        &self,
        scope: &Scope<'_>,
        file: &[String],
        pattern: &[Part],
        file_index: usize,
        pattern_index: usize,
    ) -> Result<bool, ExnThrown> {
        let (mut fi, mut pi) = (file_index, pattern_index);
        while fi < file.len() && pi < pattern.len() {
            if matches!(pattern[pi], Part::Globstar)
                || !self.test(scope, &pattern[pi], &file[fi])?
            {
                return Ok(false);
            }
            fi += 1;
            pi += 1;
        }
        Ok(if fi == file.len() && pi == pattern.len() {
            true
        } else if fi >= file.len() {
            false
        } else if pi >= pattern.len() {
            fi + 1 == file.len() && file[fi].is_empty()
        } else {
            false
        })
    }

    /// minimatch's `#matchGlobstar`: match the segments before the first globstar and after the
    /// last one at the ends of `file`, and the sections between globstars in order in between.
    fn match_globstar(
        &self,
        scope: &Scope<'_>,
        file: &[String],
        pattern: &[Part],
    ) -> Result<bool, ExnThrown> {
        let is_globstar = |part: &Part| matches!(part, Part::Globstar);
        let first = pattern.iter().position(is_globstar).unwrap_or(0);
        let last = pattern.iter().rposition(is_globstar).unwrap_or(0);
        let head = &pattern[..first];
        let body = if first < last {
            &pattern[first + 1..last]
        } else {
            &pattern[..0]
        };
        let tail = &pattern[last + 1..];
        let mut file_index = 0;
        if !head.is_empty() {
            let end = head.len().min(file.len());
            if !self.match_segments(scope, &file[..end], head, 0, 0)? {
                return Ok(false);
            }
            file_index += head.len();
        }
        let mut file_tail_match = 0;
        if !tail.is_empty() {
            if tail.len() + file_index > file.len() {
                return Ok(false);
            }
            let tail_start = file.len() - tail.len();
            if self.match_segments(scope, file, tail, tail_start, 0)? {
                file_tail_match = tail.len();
            } else {
                if file.last().is_some_and(|last| !last.is_empty())
                    || file_index + tail.len() == file.len()
                {
                    return Ok(false);
                }
                if !self.match_segments(scope, file, tail, tail_start - 1, 0)? {
                    return Ok(false);
                }
                file_tail_match = tail.len() + 1;
            }
        }
        let file_length = file.len().saturating_sub(file_tail_match);
        if body.is_empty() {
            let mut saw_some = file_tail_match > 0;
            for segment in file.iter().take(file_length).skip(file_index) {
                saw_some = true;
                if is_dotted(segment) {
                    return Ok(false);
                }
            }
            return Ok(saw_some);
        }
        // Each section is a run of `body` between globstars, with the last file index it may
        // start at, computed as minimatch computes it.
        let mut sections: Vec<(&[Part], isize)> = Vec::new();
        let mut sums = vec![0];
        let mut non_globstar = 0;
        let mut start = 0;
        for (index, part) in body.iter().enumerate() {
            if is_globstar(part) {
                sections.push((&body[start..index], 0));
                sums.push(non_globstar);
                start = index + 1;
            } else {
                non_globstar += 1;
            }
        }
        sections.push((&body[start..], 0));
        let count = sections.len();
        for (index, section) in sections.iter_mut().enumerate() {
            section.1 = file_length as isize - (sums[count - 1 - index] + section.0.len()) as isize;
        }
        Ok(self.match_sections(
            scope,
            file,
            &sections,
            file_index,
            0,
            0,
            file_tail_match > 0,
        )? == Some(true))
    }

    /// minimatch's `#matchGlobStarBodySections`: whether the sections from `section` on match
    /// `file` from `file_index`, each at the earliest index that lets the rest match. `None` means
    /// no match, without trying further positions of the previous section.
    #[allow(clippy::too_many_arguments)]
    fn match_sections(
        &self,
        scope: &Scope<'_>,
        file: &[String],
        sections: &[(&[Part], isize)],
        file_index: usize,
        section: usize,
        depth: usize,
        saw_tail: bool,
    ) -> Result<Option<bool>, ExnThrown> {
        let Some(&(body, after)) = sections.get(section) else {
            let mut saw_tail = saw_tail;
            for segment in file.iter().skip(file_index) {
                saw_tail = true;
                if is_dotted(segment) {
                    return Ok(Some(false));
                }
            }
            return Ok(Some(saw_tail));
        };
        let mut index = file_index;
        while index as isize <= after {
            let end = (index + body.len()).min(file.len());
            if self.match_segments(scope, &file[..end], body, index, 0)? && depth < 200 {
                let rest = self.match_sections(
                    scope,
                    file,
                    sections,
                    index + body.len(),
                    section + 1,
                    depth + 1,
                    saw_tail,
                )?;
                if rest != Some(false) {
                    return Ok(rest);
                }
            }
            if file.get(index).is_none_or(|segment| is_dotted(segment)) {
                return Ok(Some(false));
            }
            index += 1;
        }
        Ok(None)
    }
}

/// `new RegExp(source, flags)`.
fn compile_regex<'s>(
    scope: &'s Scope<'_>,
    source: &str,
    flags: &str,
) -> Result<Object<'s>, ExnThrown> {
    let class = js::class::get_class_object(scope, js::class_spec::JSProtoKey::JSProto_RegExp)?;
    let class = Object::from_handle(class).ok_or(ExnThrown)?;
    let source = js::JSString::from_str(scope, source)?.to_jsval_throwing(scope)?;
    let flags = js::JSString::from_str(scope, flags)?.to_jsval_throwing(scope)?;
    Function::construct(scope, class, &[source, flags])
}

/// `new RegExp(source, flags).test(text)`.
fn regex_test(scope: &Scope<'_>, source: &str, flags: &str, text: &str) -> Result<bool, ExnThrown> {
    let regex = compile_regex(scope, source, flags)?;
    let text = js::JSString::from_str(scope, text)?.to_jsval_throwing(scope)?;
    let result = Function::call_by_name(scope, regex.handle(), c"test", &[text])?;
    Ok(result.is_boolean() && result.to_boolean())
}

// ============================================================================
// Splitting and preprocessing
// ============================================================================

/// `text.split(/\/+/)`.
fn slash_split(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut after_slash = false;
    for c in text.chars() {
        if c == '/' {
            if !after_slash {
                parts.push(std::mem::take(&mut current));
            }
            after_slash = true;
        } else {
            current.push(c);
            after_slash = false;
        }
    }
    parts.push(current);
    parts
}

/// `parts.indexOf(value, from)` for a non-negative `from`.
fn index_of(parts: &[String], value: &str, from: isize) -> Option<usize> {
    let from = from.max(0) as usize;
    parts
        .iter()
        .skip(from)
        .position(|part| part == value)
        .map(|index| index + from)
}

/// Remove the `.` and empty segments other than the first and last, and an empty segment after
/// a leading `.`. Returns whether a segment was removed.
fn remove_empty_segments(parts: &mut Vec<String>) -> bool {
    let mut removed = false;
    let mut i = 1;
    while i + 1 < parts.len() {
        if i == 1 && parts[1].is_empty() && parts[0].is_empty() {
            i += 1;
            continue;
        }
        if parts[i] == "." || parts[i].is_empty() {
            removed = true;
            parts.remove(i);
            continue;
        }
        i += 1;
    }
    if parts.len() == 2 && parts[0] == "." && (parts[1] == "." || parts[1].is_empty()) {
        removed = true;
        parts.pop();
    }
    removed
}

/// Whether `segment` is one a following `..` cancels.
fn cancels(segment: Option<&String>) -> bool {
    segment.is_some_and(|segment| {
        !segment.is_empty() && segment != "." && segment != ".." && segment != "**"
    })
}

/// minimatch's `levelTwoFileOptimize`: the segments of a path with `.` and empty segments removed
/// and `<segment>/..` collapsed.
fn level_two_file_optimize(mut parts: Vec<String>) -> Vec<String> {
    loop {
        let mut changed = remove_empty_segments(&mut parts);
        let mut dd: isize = 0;
        while let Some(found) = index_of(&parts, "..", dd + 1) {
            dd = found as isize;
            if found >= 1 && cancels(parts.get(found - 1)) {
                changed = true;
                parts.drain(found - 1..=found);
                dd -= 2;
            }
        }
        if !changed {
            break;
        }
    }
    if parts.is_empty() {
        vec![String::new()]
    } else {
        parts
    }
}

/// minimatch's `firstPhasePreProcess`: collapse repeated globstars, rewrite `**/../<p>/<p>` into
/// two patterns, and remove `.` segments, empty segments and `<p>/..`.
fn first_phase(mut glob_parts: Vec<Vec<String>>) -> Vec<Vec<String>> {
    loop {
        let mut changed = false;
        let mut index = 0;
        while index < glob_parts.len() {
            let mut parts = std::mem::take(&mut glob_parts[index]);
            let mut gs: isize = -1;
            while let Some(found) = index_of(&parts, "**", gs + 1) {
                gs = found as isize;
                let mut end = found;
                while parts.get(end + 1).is_some_and(|part| part == "**") {
                    end += 1;
                }
                if end > found {
                    parts.drain(found + 1..=end);
                }
                if parts.get(found + 1).is_none_or(|next| next != "..") {
                    continue;
                }
                let plain = |part: Option<&String>| {
                    part.is_some_and(|part| !part.is_empty() && part != "." && part != "..")
                };
                if !plain(parts.get(found + 2)) || !plain(parts.get(found + 3)) {
                    continue;
                }
                changed = true;
                parts.remove(found);
                let mut other = parts.clone();
                other[found] = "**".into();
                glob_parts.push(other);
                gs -= 1;
            }
            changed |= remove_empty_segments(&mut parts);
            let mut dd: isize = 0;
            while let Some(found) = index_of(&parts, "..", dd + 1) {
                dd = found as isize;
                if found >= 1 && cancels(parts.get(found - 1)) {
                    changed = true;
                    let need_dot = found == 1 && parts.get(found + 1).is_some_and(|p| p == "**");
                    let replacement = if need_dot {
                        vec![".".to_string()]
                    } else {
                        Vec::new()
                    };
                    parts.splice(found - 1..=found, replacement);
                    if parts.is_empty() {
                        parts.push(String::new());
                    }
                    dd -= 2;
                }
            }
            glob_parts[index] = parts;
            index += 1;
        }
        if !changed {
            break;
        }
    }
    glob_parts
}

/// minimatch's `secondPhasePreProcess`: merge pairs of patterns that `parts_match` combines.
fn second_phase(mut glob_parts: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let mut i = 0;
    while i + 1 < glob_parts.len() {
        for j in i + 1..glob_parts.len() {
            if let Some(matched) = parts_match(&glob_parts[i], &glob_parts[j]) {
                glob_parts[i] = Vec::new();
                glob_parts[j] = matched;
                break;
            }
        }
        i += 1;
    }
    glob_parts.retain(|parts| !parts.is_empty());
    glob_parts
}

/// minimatch's `partsMatch` with empty globstar matching: the one pattern that matches what `a`
/// and `b` match, if the two differ only where one has a `*` or a globstar.
fn parts_match(a: &[String], b: &[String]) -> Option<Vec<String>> {
    let (mut ai, mut bi) = (0, 0);
    let mut result = Vec::new();
    let mut which = ' ';
    while ai < a.len() && bi < b.len() {
        if a[ai] == b[bi] {
            result.push(if which == 'b' { &b[bi] } else { &a[ai] }.clone());
            ai += 1;
            bi += 1;
        } else if a[ai] == "**" && Some(&b[bi]) == a.get(ai + 1) {
            result.push(a[ai].clone());
            ai += 1;
        } else if b[bi] == "**" && Some(&a[ai]) == b.get(bi + 1) {
            result.push(b[bi].clone());
            bi += 1;
        } else if a[ai] == "*" && !b[bi].is_empty() && !b[bi].starts_with('.') && b[bi] != "**" {
            if which == 'b' {
                return None;
            }
            which = 'a';
            result.push(a[ai].clone());
            ai += 1;
            bi += 1;
        } else if b[bi] == "*" && !a[ai].is_empty() && !a[ai].starts_with('.') && a[ai] != "**" {
            if which == 'a' {
                return None;
            }
            which = 'b';
            result.push(b[bi].clone());
            ai += 1;
            bi += 1;
        } else {
            return None;
        }
    }
    (a.len() == b.len()).then_some(result)
}

// ============================================================================
// Brace expansion
// ============================================================================

/// Placeholders for the `{` and `}` that brace expansion leaves unexpanded, restored after it.
const ESC_OPEN: &str = "\0OPEN\0";
const ESC_CLOSE: &str = "\0CLOSE\0";

/// The most expansions, and the most characters over all expansions, brace expansion produces.
const EXPANSION_MAX: usize = 100_000;
const EXPANSION_MAX_LENGTH: usize = 4_000_000;

/// Whether `text` has a `{` and a later `}` with neither a `{` nor a line terminator between
/// them, the test minimatch makes before expanding braces.
fn has_brace_pair(text: &str) -> bool {
    let mut open = false;
    for c in text.chars() {
        match c {
            '{' => open = true,
            '}' if open => return true,
            c if is_line_terminator(c) => open = false,
            _ => {}
        }
    }
    false
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The brace expansions of `pattern`, as minimatch's `braceExpand` produces them.
fn brace_expand(pattern: &str) -> Vec<String> {
    if !has_brace_pair(pattern) {
        return vec![pattern.to_string()];
    }
    let text = match pattern.strip_prefix("{}") {
        Some(rest) => format!("{ESC_OPEN}{ESC_CLOSE}{rest}"),
        None => pattern.to_string(),
    };
    expand(&text, true)
        .into_iter()
        .map(|expanded| expanded.replace(ESC_OPEN, "{").replace(ESC_CLOSE, "}"))
        .collect()
}

/// The first balanced `{…}` of `text`, as `pre`, `body` and `post`, following the
/// `balanced-match` package.
fn balanced(text: &str) -> Option<(&str, &str, &str)> {
    let find = |c: u8, from: isize| -> isize {
        let from = from.max(0) as usize;
        if from > text.len() {
            return -1;
        }
        text.as_bytes()[from..]
            .iter()
            .position(|b| *b == c)
            .map_or(-1, |index| (from + index) as isize)
    };
    let mut ai = find(b'{', 0);
    let mut bi = find(b'}', ai + 1);
    if ai < 0 || bi <= 0 {
        return None;
    }
    let mut i = ai;
    let mut begs: Vec<isize> = Vec::new();
    let mut left = text.len() as isize;
    let mut right = None;
    let mut result = None;
    while i >= 0 && result.is_none() {
        if i == ai {
            begs.push(i);
            ai = find(b'{', i + 1);
        } else if begs.len() == 1 {
            result = begs.pop().map(|start| (start, bi));
        } else {
            if let Some(start) = begs.pop() {
                if start < left {
                    left = start;
                    right = Some(bi);
                }
            }
            bi = find(b'}', i + 1);
        }
        i = if ai < bi && ai >= 0 { ai } else { bi };
    }
    if !begs.is_empty() {
        if let Some(right) = right {
            result = Some((left, right));
        }
    }
    let (start, end) = result?;
    let (start, end) = (start as usize, end as usize);
    Some((&text[..start], &text[start + 1..end], &text[end + 1..]))
}

/// The expansions of every `acc` element followed by `pre` and every `values` element, limited
/// as `brace-expansion` limits them. Empty expansions are dropped if `drop_empties`.
fn combine(acc: &[String], pre: &str, values: &[String], drop_empties: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut length = 0;
    for a in acc {
        for value in values {
            if out.len() >= EXPANSION_MAX {
                return out;
            }
            let expansion = format!("{a}{pre}{value}");
            if drop_empties && expansion.is_empty() {
                continue;
            }
            if length + expansion.len() > EXPANSION_MAX_LENGTH {
                return out;
            }
            length += expansion.len();
            out.push(expansion);
        }
    }
    out
}

/// The length of a `-?\d+` token at the start of `text`, if there is one.
fn integer_token(text: &str) -> Option<usize> {
    let sign = usize::from(text.starts_with('-'));
    let digits = text[sign..].bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then_some(sign + digits)
}

/// Whether `text` is `<first>..<second>` with an optional `..<integer>` step, where `token` gives
/// the lengths of `first` and `second`.
fn is_sequence(text: &str, token: fn(&str) -> Option<usize>) -> bool {
    let Some(first) = token(text) else {
        return false;
    };
    let Some(rest) = text[first..].strip_prefix("..") else {
        return false;
    };
    let Some(second) = token(rest) else {
        return false;
    };
    let rest = &rest[second..];
    if rest.is_empty() {
        return true;
    }
    rest.strip_prefix("..")
        .is_some_and(|step| integer_token(step) == Some(step.len()))
}

/// The length of an ASCII letter at the start of `text`, if there is one.
fn letter_token(text: &str) -> Option<usize> {
    text.bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic())
        .then_some(1)
}

/// The elements of the sequence `body`, such as `1..3` or `a..e..2`.
fn expand_sequence(body: &str, alpha: bool) -> Vec<String> {
    let n: Vec<&str> = body.split("..").collect();
    let numeric = |text: &str| -> f64 {
        if text.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()) {
            f64::from(text.encode_utf16().next().unwrap_or(0))
        } else {
            text.parse().unwrap_or(f64::NAN)
        }
    };
    let x = numeric(n[0]);
    let y = numeric(n[1]);
    let width = n[0].len().max(n[1].len());
    let mut incr = if n.len() == 3 {
        numeric(n[2]).abs().max(1.0)
    } else {
        1.0
    };
    let reverse = y < x;
    if reverse {
        incr = -incr;
    }
    let pad = n.iter().any(|part| {
        let digits = part.strip_prefix('-').unwrap_or(part);
        digits.len() >= 2 && digits.starts_with('0') && digits.as_bytes()[1].is_ascii_digit()
    });
    let mut out = Vec::new();
    let mut length = 0;
    let mut i = x;
    while (if reverse { i >= y } else { i <= y }) && out.len() < EXPANSION_MAX {
        let c = if alpha {
            let c = char::from_u32(i as u32)
                .map(String::from)
                .unwrap_or_default();
            if c == "\\" {
                String::new()
            } else {
                c
            }
        } else {
            let mut c = crate::errors::number_to_string(i);
            if pad && width > c.len() {
                let zeros = "0".repeat(width - c.len());
                c = if i < 0.0 {
                    format!("-{zeros}{}", &c[1..])
                } else {
                    format!("{zeros}{c}")
                };
            }
            c
        };
        if length + c.len() > EXPANSION_MAX_LENGTH {
            break;
        }
        length += c.len();
        out.push(c);
        i += incr;
    }
    out
}

/// The comma-separated parts of `text`, keeping nested `{…}` together.
fn parse_comma_parts(text: &str) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let Some((pre, body, post)) = balanced(text) else {
        return text.split(',').map(String::from).collect();
    };
    let mut parts: Vec<String> = pre.split(',').map(String::from).collect();
    let last = parts.len() - 1;
    parts[last].push_str(&format!("{{{body}}}"));
    let mut post_parts = parse_comma_parts(post);
    if !post.is_empty() {
        let first = post_parts.remove(0);
        parts[last].push_str(&first);
        parts.extend(post_parts);
    }
    parts
}

/// Whether `text` matches `/,(?!,).*\}/`: a comma not followed by a comma, with a `}` after it
/// on the same line.
fn has_later_option(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    (0..chars.len()).any(|i| {
        chars[i] == ','
            && chars.get(i + 1) != Some(&',')
            && chars[i + 1..]
                .iter()
                .take_while(|c| !is_line_terminator(**c))
                .any(|c| *c == '}')
    })
}

/// `brace-expansion`'s `expand_`.
fn expand(input: &str, mut is_top: bool) -> Vec<String> {
    let mut acc = vec![String::new()];
    let mut drop_empties = false;
    let mut first_group = true;
    let mut text = input.to_string();
    loop {
        let Some((pre, body, post)) = balanced(&text) else {
            return combine(&acc, &text, &[String::new()], drop_empties);
        };
        let (pre, body, post) = (pre.to_string(), body.to_string(), post.to_string());
        if pre.ends_with('$') {
            acc = combine(
                &acc,
                &format!("{pre}{{{body}}}"),
                &[String::new()],
                drop_empties && post.is_empty(),
            );
            first_group = false;
            if post.is_empty() {
                break;
            }
            text = post;
            continue;
        }
        let alpha = is_sequence(&body, letter_token);
        let sequence = alpha || is_sequence(&body, integer_token);
        if !sequence && !body.contains(',') {
            if has_later_option(&post) {
                text = format!("{pre}{{{body}{ESC_CLOSE}{post}");
                is_top = true;
                continue;
            }
            return combine(
                &acc,
                &format!("{pre}{{{body}}}{post}"),
                &[String::new()],
                drop_empties,
            );
        }
        if first_group {
            drop_empties = is_top && !sequence;
            first_group = false;
        }
        let values = if sequence {
            expand_sequence(&body, alpha)
        } else {
            let mut n = parse_comma_parts(&body);
            if n.len() == 1 {
                n = expand(&n[0], false)
                    .into_iter()
                    .map(|expanded| format!("{{{expanded}}}"))
                    .collect();
                if n.len() == 1 {
                    acc = combine(
                        &acc,
                        &format!("{pre}{}", n[0]),
                        &[String::new()],
                        drop_empties && post.is_empty(),
                    );
                    if post.is_empty() {
                        break;
                    }
                    text = post;
                    continue;
                }
            }
            let drops_empties = drop_empties
                && post.is_empty()
                && pre.is_empty()
                && acc.iter().all(String::is_empty);
            let mut values = Vec::new();
            let mut length = 0;
            'outer: for part in &n {
                for value in expand(part, false) {
                    if drops_empties && value.is_empty() {
                        continue;
                    }
                    if values.len() >= EXPANSION_MAX || length + value.len() > EXPANSION_MAX_LENGTH
                    {
                        break 'outer;
                    }
                    length += value.len();
                    values.push(value);
                }
            }
            values
        };
        acc = combine(&acc, &pre, &values, drop_empties && post.is_empty());
        if post.is_empty() {
            break;
        }
        text = post;
    }
    acc
}

// ============================================================================
// Segment patterns
// ============================================================================

/// The regular expression of `?`.
const QMARK: &str = "[^/]";
/// The regular expression of `*`.
const STAR: &str = "[^/]*?";
/// The regular expression of `*` where it must match something.
const STAR_NO_EMPTY: &str = "[^/]+?";
/// The start of a pattern that may not match `.` or `..`.
const START_NO_TRAVERSAL: &str = "(?!(?:^|/)\\.\\.?(?:$|/))";
/// The start of a pattern that may not match a name starting with `.`.
const START_NO_DOT: &str = "(?!\\.)";

/// The matcher of the segment pattern `pattern`, as minimatch's `parse` compiles it.
fn parse_part(pattern: &str, options: GlobOptions) -> Part {
    if pattern == "**" {
        return Part::Globstar;
    }
    if pattern.is_empty() {
        return Part::Literal(String::new());
    }
    let mut ast = Ast::new(pattern, options);
    match ast.mm_pattern() {
        MmPattern::Literal(literal) => Part::Literal(literal),
        MmPattern::Regex(source, flags) => Part::Regex(Regex {
            source,
            flags,
            fast: FastTest::of(pattern),
        }),
    }
}

/// `c` escaped for a regular expression, as minimatch's `regExpEscape` escapes it.
fn regexp_escape(c: char, out: &mut String) {
    let special = "-[]{}()*+?.,\\^$|#".contains(c)
        || matches!(
            c,
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        );
    if special {
        out.push('\\');
    }
    out.push(c);
}

/// `text` with `[`, `]`, `\` and `-` escaped for a character class.
fn brace_escape(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if matches!(c, '[' | ']' | '\\' | '-') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The POSIX character classes: the name, the class contents, whether they need the `u` flag, and
/// whether they are excluded rather than included.
const POSIX_CLASSES: [(&str, &str, bool, bool); 14] = [
    ("[:alnum:]", "\\p{L}\\p{Nl}\\p{Nd}", true, false),
    ("[:alpha:]", "\\p{L}\\p{Nl}", true, false),
    ("[:ascii:]", "\\x00-\\x7f", false, false),
    ("[:blank:]", "\\p{Zs}\\t", true, false),
    ("[:cntrl:]", "\\p{Cc}", true, false),
    ("[:digit:]", "\\p{Nd}", true, false),
    ("[:graph:]", "\\p{Z}\\p{C}", true, true),
    ("[:lower:]", "\\p{Ll}", true, false),
    ("[:print:]", "\\p{C}", true, false),
    ("[:punct:]", "\\p{P}", true, false),
    ("[:space:]", "\\p{Z}\\t\\r\\n\\v\\f", true, false),
    ("[:upper:]", "\\p{Lu}", true, false),
    ("[:word:]", "\\p{L}\\p{Nl}\\p{Nd}\\p{Pc}", true, false),
    ("[:xdigit:]", "A-Fa-f0-9", false, false),
];

/// Whether `chars` has `prefix` at `index`.
fn starts_with_at(chars: &[char], index: usize, prefix: &str) -> bool {
    prefix
        .chars()
        .enumerate()
        .all(|(offset, c)| chars.get(index + offset) == Some(&c))
}

/// minimatch's `parseClass` for the `[` at `pos`: the regular expression, whether it needs the `u`
/// flag, the number of characters consumed (0 if this isn't a class) and whether it has magic.
fn parse_class(glob: &[char], pos: usize) -> (String, bool, usize, bool) {
    let mut ranges: Vec<String> = Vec::new();
    let mut negs: Vec<&str> = Vec::new();
    let mut i = pos + 1;
    let mut saw_start = false;
    let mut uflag = false;
    let mut negate = false;
    let mut end_pos = pos;
    let mut range_start: Option<char> = None;
    'outer: while i < glob.len() {
        let c = glob[i];
        if (c == '!' || c == '^') && i == pos + 1 {
            negate = true;
            i += 1;
            continue;
        }
        if c == ']' && saw_start {
            end_pos = i + 1;
            break;
        }
        saw_start = true;
        if c == '[' {
            for (name, class, needs_u, excluded) in POSIX_CLASSES {
                if starts_with_at(glob, i, name) {
                    if range_start.is_some() {
                        return ("$.".into(), false, glob.len() - pos, true);
                    }
                    i += name.chars().count();
                    if excluded {
                        negs.push(class);
                    } else {
                        ranges.push(class.into());
                    }
                    uflag = uflag || needs_u;
                    continue 'outer;
                }
            }
        }
        if let Some(start) = range_start.take() {
            if c > start {
                ranges.push(format!(
                    "{}-{}",
                    brace_escape(&start.to_string()),
                    brace_escape(&c.to_string())
                ));
            } else if c == start {
                ranges.push(brace_escape(&c.to_string()));
            }
            i += 1;
            continue;
        }
        if starts_with_at(glob, i + 1, "-]") {
            ranges.push(brace_escape(&format!("{c}-")));
            i += 2;
            continue;
        }
        if starts_with_at(glob, i + 1, "-") {
            range_start = Some(c);
            i += 2;
            continue;
        }
        ranges.push(brace_escape(&c.to_string()));
        i += 1;
    }
    if end_pos < i {
        return (String::new(), false, 0, false);
    }
    if ranges.is_empty() && negs.is_empty() {
        return ("$.".into(), false, glob.len() - pos, true);
    }
    if negs.is_empty() && ranges.len() == 1 && !negate {
        let single: Vec<char> = ranges[0].chars().collect();
        let r = match single.as_slice() {
            ['\\', c] if !is_line_terminator(*c) => Some(*c),
            [c] if !is_line_terminator(*c) => Some(*c),
            _ => None,
        };
        if let Some(r) = r {
            let mut source = String::new();
            regexp_escape(r, &mut source);
            return (source, false, end_pos - pos, false);
        }
    }
    let ranges_text = ranges.concat();
    let sranges = format!("[{}{ranges_text}]", if negate { "^" } else { "" });
    let snegs = format!("[{}{}]", if negate { "" } else { "^" }, negs.concat());
    let class = if !ranges.is_empty() && !negs.is_empty() {
        format!("({sranges}|{snegs})")
    } else if !ranges.is_empty() {
        sranges
    } else {
        snegs
    };
    (class, uflag, end_pos - pos, true)
}

/// minimatch's `unescape` with its default options: `[x]` becomes `x` unless preceded by a
/// backslash, then backslashes before anything but `/` are removed.
fn unescape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let plain = |c: char| c != '/' && c != '\\';
    let mut first = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 3 < chars.len()
            && chars[i] != '\\'
            && !is_line_terminator(chars[i])
            && chars[i + 1] == '['
            && plain(chars[i + 2])
            && chars[i + 3] == ']'
        {
            first.push(chars[i]);
            first.push(chars[i + 2]);
            i += 4;
            continue;
        }
        if i == 0 && chars.len() > 2 && chars[0] == '[' && plain(chars[1]) && chars[2] == ']' {
            first.push(chars[1]);
            i += 3;
            continue;
        }
        first.push(chars[i]);
        i += 1;
    }
    let mut out = String::new();
    let mut i = 0;
    while i < first.len() {
        if first[i] == '\\' && i + 1 < first.len() && first[i + 1] != '/' {
            out.push(first[i + 1]);
            i += 2;
        } else {
            out.push(first[i]);
            i += 1;
        }
    }
    out
}

/// minimatch's `#parseGlob` for the text of a pattern without extglobs: the regular expression,
/// whether it has magic, and whether it needs the `u` flag.
fn parse_glob(glob: &str, has_magic: bool, no_empty: bool) -> (String, bool, bool) {
    let chars: Vec<char> = glob.chars().collect();
    let all_stars = !chars.is_empty() && chars.iter().all(|c| *c == '*');
    let mut re = String::new();
    let mut uflag = false;
    let mut has_magic = has_magic;
    let mut in_star = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' {
            if !in_star {
                in_star = true;
                re.push_str(if no_empty && all_stars {
                    STAR_NO_EMPTY
                } else {
                    STAR
                });
                has_magic = true;
            }
            i += 1;
            continue;
        }
        in_star = false;
        if c == '[' {
            let (source, needs_u, consumed, magic) = parse_class(&chars, i);
            if consumed > 0 {
                re.push_str(&source);
                uflag = uflag || needs_u;
                has_magic = has_magic || magic;
                i += consumed;
                continue;
            }
        }
        if c == '?' {
            re.push_str(QMARK);
            has_magic = true;
            i += 1;
            continue;
        }
        regexp_escape(c, &mut re);
        i += 1;
    }
    (re, has_magic, uflag)
}

/// What minimatch's `toMMPattern` makes of a segment pattern.
enum MmPattern {
    Literal(String),
    /// The regular expression's source and flags.
    Regex(String, String),
}

/// An element of an [`AstNode`]: text, or a nested node by index.
#[derive(Clone, Debug)]
enum Item {
    Text(String),
    Node(usize),
}

/// A node of minimatch's `AST`: a sequence (no `kind`), or an extglob whose `kind` is one of
/// `!?+*@` and whose items are the alternatives.
#[derive(Debug)]
struct AstNode {
    kind: Option<char>,
    items: Vec<Item>,
    parent: Option<usize>,
    /// The parent's item count when this node was created.
    parent_index: usize,
    has_magic: Option<bool>,
    uflag: bool,
    /// Whether this is an extglob whose last alternative is empty, as `!()` is.
    empty_ext: bool,
    string: Option<String>,
}

/// The extglob kinds an extglob of each kind takes the alternatives of, when its only item is a
/// sequence holding one extglob of that kind.
fn adoption(kind: char) -> &'static [char] {
    match kind {
        '!' => &['@'],
        '?' => &['?', '@'],
        '@' => &['@'],
        '*' => &['*', '+', '?', '@'],
        '+' => &['+', '@'],
        _ => &[],
    }
}

/// The kinds [`adoption`] admits when an empty alternative is added to the adopted extglob.
fn adoption_with_space(kind: char) -> &'static [char] {
    match kind {
        '!' | '@' => &['?'],
        '+' => &['?', '*'],
        _ => &[],
    }
}

/// The kinds a nested extglob may have without counting towards the nesting limit.
fn adoption_any(kind: char) -> &'static [char] {
    match kind {
        '!' | '?' | '@' => &['?', '@'],
        '*' => &['*', '+', '?', '@'],
        '+' => &['+', '@', '?', '*'],
        _ => &[],
    }
}

/// The kind an extglob of kind `outer` with a single nested extglob of kind `inner` becomes.
fn usurp(outer: char, inner: char) -> Option<char> {
    match (outer, inner) {
        ('!', '!') => Some('@'),
        ('?', '*' | '+') => Some('*'),
        ('@', inner) if "!?@*+".contains(inner) => Some(inner),
        ('+', '?' | '*') => Some('*'),
        _ => None,
    }
}

fn is_extglob_kind(c: char) -> bool {
    "!?+*@".contains(c)
}

/// minimatch's `AST` of one segment pattern, with the root at index 0.
struct Ast {
    nodes: Vec<AstNode>,
    /// The `!` extglobs to complete with the rest of the pattern.
    negs: Vec<usize>,
    filled_negs: bool,
    options: GlobOptions,
}

impl Ast {
    /// The AST of `pattern`, as `AST.fromGlob` parses it.
    fn new(pattern: &str, options: GlobOptions) -> Ast {
        let mut ast = Ast {
            nodes: Vec::new(),
            negs: Vec::new(),
            filled_negs: false,
            options,
        };
        ast.add_node(None, None);
        let chars: Vec<char> = pattern.chars().collect();
        ast.parse(&chars, 0, 0, 0);
        ast
    }

    fn add_node(&mut self, kind: Option<char>, parent: Option<usize>) -> usize {
        let parent_index = parent.map_or(0, |parent| self.nodes[parent].items.len());
        let id = self.nodes.len();
        self.nodes.push(AstNode {
            kind,
            items: Vec::new(),
            parent,
            parent_index,
            has_magic: kind.map(|_| true),
            uflag: false,
            empty_ext: false,
            string: None,
        });
        if kind == Some('!') && !self.filled_negs {
            self.negs.push(id);
        }
        id
    }

    /// Append `item` to `node`, unless it is empty text.
    fn push(&mut self, node: usize, item: Item) {
        if matches!(&item, Item::Text(text) if text.is_empty()) {
            return;
        }
        self.nodes[node].items.push(item);
    }

    fn is_extglob(&self, node: usize) -> bool {
        self.nodes[node].kind.is_some()
    }

    /// The pattern text of `node`.
    fn pattern_text(&mut self, node: usize) -> String {
        if let Some(string) = &self.nodes[node].string {
            return string.clone();
        }
        let items = self.nodes[node].items.clone();
        let texts: Vec<String> = items
            .iter()
            .map(|item| match item {
                Item::Text(text) => text.clone(),
                Item::Node(child) => self.pattern_text(*child),
            })
            .collect();
        let string = match self.nodes[node].kind {
            None => texts.concat(),
            Some(kind) => format!("{kind}({})", texts.join("|")),
        };
        self.nodes[node].string = Some(string.clone());
        string
    }

    /// minimatch's `#parseAST`: parse `text` from `pos` into `node`, and return the index after
    /// what was parsed.
    fn parse(&mut self, text: &[char], node: usize, pos: usize, ext_depth: usize) -> usize {
        const MAX_DEPTH: usize = 2;
        let mut in_brace = false;
        let mut brace_start = 0;
        let mut brace_neg = false;
        if self.nodes[node].kind.is_none() {
            let mut i = pos;
            let mut acc = String::new();
            while i < text.len() {
                let c = text[i];
                i += 1;
                if in_brace {
                    if i == brace_start + 1 {
                        if c == '^' || c == '!' {
                            brace_neg = true;
                        }
                    } else if c == ']' && !(i == brace_start + 2 && brace_neg) {
                        in_brace = false;
                    }
                    acc.push(c);
                    continue;
                } else if c == '[' {
                    in_brace = true;
                    brace_start = i;
                    brace_neg = false;
                    acc.push(c);
                    continue;
                }
                if is_extglob_kind(c) && text.get(i) == Some(&'(') && ext_depth <= MAX_DEPTH {
                    self.push(node, Item::Text(std::mem::take(&mut acc)));
                    let ext = self.add_node(Some(c), Some(node));
                    i = self.parse(text, ext, i, ext_depth + 1);
                    self.push(node, Item::Node(ext));
                    continue;
                }
                acc.push(c);
            }
            self.push(node, Item::Text(acc));
            return i;
        }
        let mut i = pos + 1;
        let mut part = self.add_node(None, Some(node));
        let mut parts = Vec::new();
        let mut acc = String::new();
        while i < text.len() {
            let c = text[i];
            i += 1;
            if in_brace {
                if i == brace_start + 1 {
                    if c == '^' || c == '!' {
                        brace_neg = true;
                    }
                } else if c == ']' && !(i == brace_start + 2 && brace_neg) {
                    in_brace = false;
                }
                acc.push(c);
                continue;
            } else if c == '[' {
                in_brace = true;
                brace_start = i;
                brace_neg = false;
                acc.push(c);
                continue;
            }
            let kind = self.nodes[node].kind.unwrap_or(' ');
            let adoptable = adoption_any(kind).contains(&c);
            if is_extglob_kind(c)
                && text.get(i) == Some(&'(')
                && (ext_depth <= MAX_DEPTH || adoptable)
            {
                let depth_add = usize::from(!adoptable);
                self.push(part, Item::Text(std::mem::take(&mut acc)));
                let ext = self.add_node(Some(c), Some(part));
                self.push(part, Item::Node(ext));
                i = self.parse(text, ext, i, ext_depth + depth_add);
                continue;
            }
            if c == '|' {
                self.push(part, Item::Text(std::mem::take(&mut acc)));
                parts.push(part);
                part = self.add_node(None, Some(node));
                continue;
            }
            if c == ')' {
                if acc.is_empty() && self.nodes[node].items.is_empty() {
                    self.nodes[node].empty_ext = true;
                }
                self.push(part, Item::Text(std::mem::take(&mut acc)));
                parts.push(part);
                for part in parts {
                    self.push(node, Item::Node(part));
                }
                return i;
            }
            acc.push(c);
        }
        let rest: String = text[pos - 1..].iter().collect();
        let node = &mut self.nodes[node];
        node.kind = None;
        node.has_magic = None;
        node.items = vec![Item::Text(rest)];
        i
    }

    fn is_start(&self, node: usize) -> bool {
        if node == 0 {
            return true;
        }
        let Some(parent) = self.nodes[node].parent else {
            return false;
        };
        if !self.is_start(parent) {
            return false;
        }
        let parent_index = self.nodes[node].parent_index;
        (0..parent_index).all(|i| {
            matches!(self.nodes[parent].items.get(i),
                Some(Item::Node(sibling)) if self.nodes[*sibling].kind == Some('!'))
        })
    }

    fn is_end(&self, node: usize) -> bool {
        if node == 0 {
            return true;
        }
        let Some(parent) = self.nodes[node].parent else {
            return false;
        };
        if self.nodes[parent].kind == Some('!') {
            return true;
        }
        if !self.is_end(parent) {
            return false;
        }
        if self.nodes[node].kind.is_none() {
            return true;
        }
        self.nodes[node].parent_index as isize == self.nodes[parent].items.len() as isize - 1
    }

    /// minimatch's `#fillNegs`: append the items after each `!` extglob, at every level, to its
    /// alternatives.
    fn fill_negs(&mut self) {
        if self.filled_negs {
            return;
        }
        self.pattern_text(0);
        self.filled_negs = true;
        while let Some(neg) = self.negs.pop() {
            if self.nodes[neg].kind != Some('!') {
                continue;
            }
            let mut p = neg;
            while let Some(pp) = self.nodes[p].parent {
                let mut i = self.nodes[p].parent_index + 1;
                while self.nodes[pp].kind.is_none() && i < self.nodes[pp].items.len() {
                    let sibling = self.nodes[pp].items[i].clone();
                    for item in self.nodes[neg].items.clone() {
                        if let Item::Node(alternative) = item {
                            self.copy_in(alternative, &sibling);
                        }
                    }
                    i += 1;
                }
                p = pp;
            }
        }
    }

    fn copy_in(&mut self, node: usize, item: &Item) {
        match item {
            Item::Text(text) => self.push(node, Item::Text(text.clone())),
            Item::Node(child) => {
                let clone = self.clone_node(*child, node);
                self.push(node, Item::Node(clone));
            }
        }
    }

    fn clone_node(&mut self, node: usize, parent: usize) -> usize {
        let clone = self.add_node(self.nodes[node].kind, Some(parent));
        for item in self.nodes[node].items.clone() {
            self.copy_in(clone, &item);
        }
        clone
    }

    /// The only item of `child` as an extglob, if `node` is an extglob and `child` is a sequence
    /// holding just that.
    fn sole_extglob(&self, node: usize, child: usize) -> Option<usize> {
        let c = &self.nodes[child];
        if c.kind.is_some() || c.items.len() != 1 || self.nodes[node].kind.is_none() {
            return None;
        }
        match c.items[0] {
            Item::Node(grandchild) if self.nodes[grandchild].kind.is_some() => Some(grandchild),
            _ => None,
        }
    }

    fn can_adopt(&self, node: usize, child: usize, kinds: fn(char) -> &'static [char]) -> bool {
        let Some(grandchild) = self.sole_extglob(node, child) else {
            return false;
        };
        let (Some(kind), Some(inner)) = (self.nodes[node].kind, self.nodes[grandchild].kind) else {
            return false;
        };
        kinds(kind).contains(&inner)
    }

    /// Replace the item `index` of `node`, the sequence `child`, with the alternatives of the
    /// extglob it holds.
    fn adopt(&mut self, node: usize, child: usize, index: usize) {
        let Item::Node(grandchild) = self.nodes[child].items[0] else {
            return;
        };
        let items = self.nodes[grandchild].items.clone();
        self.nodes[node].items.splice(index..=index, items.clone());
        for item in items {
            if let Item::Node(item) = item {
                self.nodes[item].parent = Some(node);
            }
        }
        self.nodes[node].string = None;
    }

    fn adopt_with_space(&mut self, node: usize, child: usize, index: usize) {
        let Item::Node(grandchild) = self.nodes[child].items[0] else {
            return;
        };
        let blank = self.add_node(None, Some(grandchild));
        self.nodes[blank].items.push(Item::Text(String::new()));
        self.push(grandchild, Item::Node(blank));
        self.adopt(node, child, index);
    }

    fn can_usurp(&self, node: usize, child: usize) -> bool {
        if self.nodes[node].items.len() != 1 {
            return false;
        }
        let Some(grandchild) = self.sole_extglob(node, child) else {
            return false;
        };
        matches!((self.nodes[node].kind, self.nodes[grandchild].kind),
            (Some(outer), Some(inner)) if usurp(outer, inner).is_some())
    }

    /// Make `node` the extglob that `child` holds, with the kind [`usurp`] gives.
    fn usurp(&mut self, node: usize, child: usize) {
        let Item::Node(grandchild) = self.nodes[child].items[0] else {
            return;
        };
        let (Some(outer), Some(inner)) = (self.nodes[node].kind, self.nodes[grandchild].kind)
        else {
            return;
        };
        let Some(kind) = usurp(outer, inner) else {
            return;
        };
        let items = self.nodes[grandchild].items.clone();
        for item in &items {
            if let Item::Node(item) = item {
                self.nodes[*item].parent = Some(node);
            }
        }
        let node = &mut self.nodes[node];
        node.items = items;
        node.kind = Some(kind);
        node.string = None;
        node.empty_ext = false;
    }

    /// minimatch's `#flatten`: merge nested extglobs that [`Ast::can_adopt`] and
    /// [`Ast::can_usurp`] allow.
    fn flatten(&mut self, node: usize) {
        if !self.is_extglob(node) {
            for item in self.nodes[node].items.clone() {
                if let Item::Node(child) = item {
                    self.flatten(child);
                }
            }
        } else {
            let mut iterations = 0;
            loop {
                let mut done = true;
                let mut i = 0;
                while i < self.nodes[node].items.len() {
                    if let Item::Node(child) = self.nodes[node].items[i] {
                        self.flatten(child);
                        if self.can_adopt(node, child, adoption) {
                            done = false;
                            self.adopt(node, child, i);
                        } else if self.can_adopt(node, child, adoption_with_space) {
                            done = false;
                            self.adopt_with_space(node, child, i);
                        } else if self.can_usurp(node, child) {
                            done = false;
                            self.usurp(node, child);
                        }
                    }
                    i += 1;
                }
                iterations += 1;
                if done || iterations >= 10 {
                    break;
                }
            }
        }
        self.nodes[node].string = None;
    }

    /// minimatch's `#partsToRegExp`: the alternatives of the extglob `node`, joined with `|`.
    fn parts_to_regexp(&mut self, node: usize, dot: bool) -> String {
        let items = self.nodes[node].items.clone();
        let mut sources = Vec::new();
        for item in items {
            let source = match item {
                Item::Node(child) => {
                    let (source, _, _, uflag) = self.regexp_source(child, Some(dot));
                    self.nodes[node].uflag = self.nodes[node].uflag || uflag;
                    source
                }
                Item::Text(text) => {
                    let mut source = String::new();
                    text.chars().for_each(|c| regexp_escape(c, &mut source));
                    source
                }
            };
            sources.push(source);
        }
        let whole = self.is_start(node) && self.is_end(node);
        sources.retain(|source| !whole || !source.is_empty());
        sources.join("|")
    }

    /// minimatch's `toRegExpSource`: the regular expression of `node`, the literal it stands
    /// for, whether it has magic, and whether it needs the `u` flag.
    fn regexp_source(
        &mut self,
        node: usize,
        allow_dot: Option<bool>,
    ) -> (String, String, bool, bool) {
        let dot = allow_dot.unwrap_or(false);
        let allows_dot = allow_dot == Some(true);
        if node == 0 {
            self.flatten(0);
            self.fill_negs();
        }
        let Some(kind) = self.nodes[node].kind else {
            let no_empty = self.is_start(node)
                && self.is_end(node)
                && self.nodes[node]
                    .items
                    .iter()
                    .all(|item| matches!(item, Item::Text(_)));
            let mut src = String::new();
            for item in self.nodes[node].items.clone() {
                let (re, has_magic, uflag) = match item {
                    Item::Text(text) => {
                        parse_glob(&text, self.nodes[node].has_magic == Some(true), no_empty)
                    }
                    Item::Node(child) => {
                        let (re, _, has_magic, uflag) = self.regexp_source(child, allow_dot);
                        (re, has_magic, uflag)
                    }
                };
                let state = &mut self.nodes[node];
                state.has_magic = Some(state.has_magic == Some(true) || has_magic);
                state.uflag = state.uflag || uflag;
                src.push_str(&re);
            }
            let mut start = "";
            if self.is_start(node) {
                if let Some(Item::Text(first)) = self.nodes[node].items.first() {
                    let dot_traversal =
                        self.nodes[node].items.len() == 1 && (first == "." || first == "..");
                    if !dot_traversal {
                        let at = |index: usize| matches!(src.chars().nth(index), Some('[' | '.'));
                        let need_no_traversal = (dot && at(0))
                            || (src.starts_with("\\.") && at(2))
                            || (src.starts_with("\\.\\.") && at(4));
                        let need_no_dot = !dot && !allows_dot && at(0);
                        start = if need_no_traversal {
                            START_NO_TRAVERSAL
                        } else if need_no_dot {
                            START_NO_DOT
                        } else {
                            ""
                        };
                    }
                }
            }
            let parent_is_neg = self.nodes[node]
                .parent
                .is_some_and(|parent| self.nodes[parent].kind == Some('!'));
            let end = if self.is_end(node) && self.filled_negs && parent_is_neg {
                "(?:$|\\/)"
            } else {
                ""
            };
            let has_magic = self.nodes[node].has_magic == Some(true);
            self.nodes[node].has_magic = Some(has_magic);
            return (
                format!("{start}{src}{end}"),
                unescape(&src),
                has_magic,
                self.nodes[node].uflag,
            );
        };
        let repeated = kind == '*' || kind == '+';
        let open = if kind == '!' { "(?:(?!(?:" } else { "(?:" };
        let mut body = self.parts_to_regexp(node, dot);
        if self.is_start(node) && self.is_end(node) && body.is_empty() && kind != '!' {
            let string = self.pattern_text(node);
            let state = &mut self.nodes[node];
            state.items = vec![Item::Text(string.clone())];
            state.kind = None;
            state.has_magic = None;
            return (string.clone(), unescape(&string), false, false);
        }
        let mut body_dot_allowed = if !repeated || allows_dot || dot {
            String::new()
        } else {
            self.parts_to_regexp(node, true)
        };
        if body_dot_allowed == body {
            body_dot_allowed.clear();
        }
        if !body_dot_allowed.is_empty() {
            body = format!("(?:{body})(?:{body_dot_allowed})*?");
        }
        let start_no_dot = |ast: &Ast| {
            if ast.is_start(node) && !dot && !allows_dot {
                START_NO_DOT
            } else {
                ""
            }
        };
        let final_source = if kind == '!' && self.nodes[node].empty_ext {
            let start = if self.is_start(node) && !dot {
                START_NO_DOT
            } else {
                ""
            };
            format!("{start}{STAR_NO_EMPTY}")
        } else {
            let close = match kind {
                '!' => format!(")){}{STAR})", start_no_dot(self)),
                '@' => ")".into(),
                '?' => ")?".into(),
                '+' if !body_dot_allowed.is_empty() => ")".into(),
                '*' if !body_dot_allowed.is_empty() => ")?".into(),
                kind => format!("){kind}"),
            };
            format!("{open}{body}{close}")
        };
        let has_magic = self.nodes[node].has_magic == Some(true);
        self.nodes[node].has_magic = Some(has_magic);
        (
            final_source,
            unescape(&body),
            has_magic,
            self.nodes[node].uflag,
        )
    }

    /// minimatch's `toMMPattern`: the literal the segment pattern stands for, or its regular
    /// expression if it has magic.
    fn mm_pattern(&mut self) -> MmPattern {
        let glob = self.pattern_text(0);
        let (re, body, has_magic, uflag) = self.regexp_source(0, None);
        let options = self.options;
        let any_magic = has_magic
            || self.nodes[0].has_magic == Some(true)
            || (options.nocase
                && !options.nocase_magic_only
                && glob.to_uppercase() != glob.to_lowercase());
        if !any_magic {
            return MmPattern::Literal(body);
        }
        let mut flags = String::new();
        if options.nocase {
            flags.push('i');
        }
        if uflag {
            flags.push('u');
        }
        MmPattern::Regex(format!("^{re}$"), flags)
    }
}
