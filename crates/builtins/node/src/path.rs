// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `node:path`: POSIX path manipulation. See <https://nodejs.org/api/path.html>.
//!
//! WASI paths are POSIX paths, so the module is `path.posix`, which it also exports as `posix`.
//! `path.win32` is not implemented.
//!
//! The algorithms follow Node's `lib/path.js`. They work on UTF-8 bytes where Node works on UTF-16
//! code units. The two agree because they only look for `/` and `.`, which never occur inside a
//! multi-byte UTF-8 sequence, so every index they split at is a character boundary.

use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use crate::errors::{validate_plain_object, validate_string, Thrown};
use crate::glob::{platform_nocase, Glob, GlobOptions};

/// Normalize `path`, resolving `.` and `..` segments. `..` segments above the start are kept if
/// `allow_above_root`, and dropped otherwise.
///
/// Node's `normalizeString`.
fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let bytes = path.as_bytes();
    let mut res = String::new();
    let mut last_segment_length = 0;
    let mut last_slash: isize = -1;
    let mut dots = 0;
    for i in 0..=bytes.len() {
        let code = if i < bytes.len() {
            bytes[i]
        } else if bytes.last() == Some(&b'/') {
            break;
        } else {
            b'/'
        };
        let i = i as isize;
        if code == b'/' {
            if last_slash == i - 1 || dots == 1 {
                // A repeated slash or a `.` segment adds nothing.
            } else if dots == 2 {
                if res.len() < 2 || last_segment_length != 2 || !res.ends_with("..") {
                    if res.len() > 2 {
                        match res.rfind('/') {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(index) => {
                                res.truncate(index);
                                let last_slash_index = res.rfind('/').map_or(-1, |i| i as isize);
                                last_segment_length =
                                    (res.len() as isize - 1 - last_slash_index) as usize;
                            }
                        }
                        last_slash = i;
                        dots = 0;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i;
                        dots = 0;
                        continue;
                    }
                }
                if allow_above_root {
                    res.push_str(if res.is_empty() { ".." } else { "/.." });
                    last_segment_length = 2;
                }
            } else {
                let segment = &path[(last_slash + 1) as usize..i as usize];
                if !res.is_empty() {
                    res.push('/');
                }
                res.push_str(segment);
                last_segment_length = (i - last_slash - 1) as usize;
            }
            last_slash = i;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
    }
    res
}

/// The current working directory, as `process.cwd()` reports it.
fn cwd() -> String {
    crate::process::cwd()
}

/// `path.resolve(...paths)`: the absolute path `paths` resolve to from right to left, starting
/// from the current working directory.
pub fn resolve_paths(paths: &[String]) -> String {
    let mut resolved = String::new();
    let mut absolute = false;
    let mut cwd_path;
    let mut index = paths.len();
    while !absolute {
        let path = if index > 0 {
            index -= 1;
            paths[index].as_str()
        } else {
            cwd_path = cwd();
            absolute = true;
            cwd_path.as_str()
        };
        if path.is_empty() {
            continue;
        }
        resolved = if resolved.is_empty() {
            path.to_string()
        } else {
            format!("{path}/{resolved}")
        };
        absolute = path.starts_with('/');
    }
    let normalized = normalize_string(&resolved, !absolute);
    if absolute {
        format!("/{normalized}")
    } else if normalized.is_empty() {
        ".".into()
    } else {
        normalized
    }
}

/// `path.normalize(path)`.
pub fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing_separator = path.ends_with('/');
    let mut normalized = normalize_string(path, !absolute);
    if normalized.is_empty() {
        if absolute {
            return "/".into();
        }
        return if trailing_separator { "./" } else { "." }.into();
    }
    if trailing_separator {
        normalized.push('/');
    }
    if absolute {
        format!("/{normalized}")
    } else {
        normalized
    }
}

/// `path.dirname(path)`.
pub fn dirname_of(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.is_empty() {
        return ".";
    }
    let has_root = bytes[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None if has_root => "/",
        None => ".",
        Some(1) if has_root => "//",
        Some(end) => &path[..end],
    }
}

/// `path.basename(path, suffix)`.
pub fn basename_of<'p>(path: &'p str, suffix: Option<&str>) -> &'p str {
    let bytes = path.as_bytes();
    let mut start = 0;
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    if let Some(suffix) = suffix.filter(|s| !s.is_empty() && s.len() <= path.len()) {
        if suffix == path {
            return "";
        }
        let suffix = suffix.as_bytes();
        let mut ext_index = suffix.len() as isize - 1;
        let mut first_non_slash_end: Option<usize> = None;
        for i in (0..bytes.len()).rev() {
            let code = bytes[i];
            if code == b'/' {
                if !matched_slash {
                    start = i + 1;
                    break;
                }
            } else {
                if first_non_slash_end.is_none() {
                    matched_slash = false;
                    first_non_slash_end = Some(i + 1);
                }
                if ext_index >= 0 {
                    if code == suffix[ext_index as usize] {
                        ext_index -= 1;
                        if ext_index == -1 {
                            end = Some(i);
                        }
                    } else {
                        ext_index = -1;
                        end = first_non_slash_end;
                    }
                }
            }
        }
        let end = match end {
            Some(end) if end == start => first_non_slash_end.unwrap_or(bytes.len()),
            Some(end) => end,
            None => bytes.len(),
        };
        return &path[start..end.max(start)];
    }
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'/' {
            if !matched_slash {
                start = i + 1;
                break;
            }
        } else if end.is_none() {
            matched_slash = false;
            end = Some(i + 1);
        }
    }
    end.map_or("", |end| &path[start..end])
}

/// The parts of a path `parse` finds: the start of its last segment, the dot starting its
/// extension, and the end of its last segment. Node's shared scan of `extname` and `parse`.
struct Extension {
    start_part: usize,
    start_dot: Option<usize>,
    end: Option<usize>,
    /// Whether the extension is a real one: `start_dot` is set and the segment isn't `.` or `..`
    /// or a dotfile name.
    valid: bool,
}

/// Scan `path` backwards from its end down to `stop` for its last segment and extension.
fn scan_extension(bytes: &[u8], stop: usize) -> Extension {
    let mut start_dot: Option<usize> = None;
    let mut start_part = 0;
    let mut end: Option<usize> = None;
    let mut matched_slash = true;
    // 0: no non-dot character before the last dot, 1: one, -1: a dot right after one.
    let mut pre_dot_state = 0;
    for i in (stop..bytes.len()).rev() {
        let code = bytes[i];
        if code == b'/' {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            continue;
        }
        if end.is_none() {
            matched_slash = false;
            end = Some(i + 1);
        }
        if code == b'.' {
            if start_dot.is_none() {
                start_dot = Some(i);
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot.is_some() {
            pre_dot_state = -1;
        }
    }
    let valid = match (start_dot, end) {
        (Some(dot), Some(end)) => {
            pre_dot_state != 0 && !(pre_dot_state == 1 && dot + 1 == end && dot == start_part + 1)
        }
        _ => false,
    };
    Extension {
        start_part,
        start_dot,
        end,
        valid,
    }
}

/// `path.extname(path)`.
pub fn extname_of(path: &str) -> &str {
    let scan = scan_extension(path.as_bytes(), 0);
    match (scan.valid, scan.start_dot, scan.end) {
        (true, Some(dot), Some(end)) => &path[dot..end],
        _ => "",
    }
}

#[core_runtime::jsmodule(
    node_args,
    name = "node:path",
    default_namespace,
    self_export = "posix"
)]
pub mod path {
    use super::*;

    /// The path segment separator.
    #[allow(non_upper_case_globals)]
    pub const sep: &str = "/";
    /// The `PATH` delimiter.
    #[allow(non_upper_case_globals)]
    pub const delimiter: &str = ":";

    /// <https://nodejs.org/api/path.html#pathresolvepaths>
    ///
    /// The arguments are validated from right to left up to the first absolute path, and those to
    /// its left are ignored.
    pub fn resolve(scope: &Scope<'_>, paths: RestArgs<HandleValue<'_>>) -> Result<String, Thrown> {
        let mut used = Vec::new();
        for (index, path) in paths.iter().enumerate().rev() {
            let path = if path.is_string() {
                validate_string(scope, *path, "")?
            } else {
                validate_string(scope, *path, &format!("paths[{index}]"))?
            };
            let absolute = path.starts_with('/');
            used.push(path);
            if absolute {
                break;
            }
        }
        used.reverse();
        Ok(resolve_paths(&used))
    }

    /// <https://nodejs.org/api/path.html#pathnormalizepath>
    pub fn normalize(scope: &Scope<'_>, path: HandleValue<'_>) -> Result<String, Thrown> {
        Ok(normalize_path(&validate_string(scope, path, "path")?))
    }

    /// <https://nodejs.org/api/path.html#pathisabsolutepath>
    pub fn is_absolute(scope: &Scope<'_>, path: HandleValue<'_>) -> Result<bool, Thrown> {
        Ok(validate_string(scope, path, "path")?.starts_with('/'))
    }

    /// <https://nodejs.org/api/path.html#pathjoinpaths>
    pub fn join(scope: &Scope<'_>, paths: RestArgs<HandleValue<'_>>) -> Result<String, Thrown> {
        let mut joined = String::new();
        for path in paths.iter() {
            let path = validate_string(scope, *path, "path")?;
            if path.is_empty() {
                continue;
            }
            if !joined.is_empty() {
                joined.push('/');
            }
            joined.push_str(&path);
        }
        Ok(normalize_path(&joined))
    }

    /// <https://nodejs.org/api/path.html#pathrelativefrom-to>
    pub fn relative(
        scope: &Scope<'_>,
        from: HandleValue<'_>,
        to: HandleValue<'_>,
    ) -> Result<String, Thrown> {
        let from = validate_string(scope, from, "from")?;
        let to = validate_string(scope, to, "to")?;
        if from == to {
            return Ok(String::new());
        }
        let from = resolve_paths(&[from]);
        let to = resolve_paths(&[to]);
        if from == to {
            return Ok(String::new());
        }
        let (from_bytes, to_bytes) = (from.as_bytes(), to.as_bytes());
        // Both are absolute, so their first byte is the root slash.
        let from_start = 1;
        let from_end = from_bytes.len();
        let from_len = from_end - from_start;
        let to_start = 1;
        let to_len = to_bytes.len() - to_start;
        let length = from_len.min(to_len);
        let mut last_common_sep: isize = -1;
        let mut i = 0;
        while i < length {
            let from_code = from_bytes[from_start + i];
            if from_code != to_bytes[to_start + i] {
                break;
            }
            if from_code == b'/' {
                last_common_sep = i as isize;
            }
            i += 1;
        }
        if i == length {
            if to_len > length {
                if to_bytes[to_start + i] == b'/' {
                    return Ok(to[to_start + i + 1..].to_string());
                }
                if i == 0 {
                    return Ok(to[to_start + i..].to_string());
                }
            } else if from_len > length {
                if from_bytes[from_start + i] == b'/' {
                    last_common_sep = i as isize;
                } else if i == 0 {
                    last_common_sep = 0;
                }
            }
        }
        let mut out = String::new();
        let mut j = (from_start as isize + last_common_sep + 1) as usize;
        while j <= from_end {
            if j == from_end || from_bytes[j] == b'/' {
                out.push_str(if out.is_empty() { ".." } else { "/.." });
            }
            j += 1;
        }
        out.push_str(&to[(to_start as isize + last_common_sep) as usize..]);
        Ok(out)
    }

    /// <https://nodejs.org/api/path.html#pathtonamespacedpathpath>: the path unchanged on POSIX.
    pub fn to_namespaced_path<'v>(path: HandleValue<'v>) -> HandleValue<'v> {
        path
    }

    /// <https://nodejs.org/api/path.html#pathdirnamepath>
    pub fn dirname(scope: &Scope<'_>, path: HandleValue<'_>) -> Result<String, Thrown> {
        Ok(dirname_of(&validate_string(scope, path, "path")?).to_string())
    }

    /// <https://nodejs.org/api/path.html#pathbasenamepath-suffix>
    #[function(length = 2)]
    pub fn basename(
        scope: &Scope<'_>,
        path: HandleValue<'_>,
        suffix: Option<HandleValue<'_>>,
    ) -> Result<String, Thrown> {
        let suffix = match suffix.filter(|suffix| !suffix.is_undefined()) {
            Some(suffix) => Some(validate_string(scope, suffix, "suffix")?),
            None => None,
        };
        let path = validate_string(scope, path, "path")?;
        Ok(basename_of(&path, suffix.as_deref()).to_string())
    }

    /// <https://nodejs.org/api/path.html#pathextnamepath>
    pub fn extname(scope: &Scope<'_>, path: HandleValue<'_>) -> Result<String, Thrown> {
        Ok(extname_of(&validate_string(scope, path, "path")?).to_string())
    }

    /// <https://nodejs.org/api/path.html#pathformatpathobject>
    pub fn format(scope: &Scope<'_>, path_object: HandleValue<'_>) -> Result<String, Thrown> {
        let object = validate_plain_object(scope, path_object, "pathObject", false)?;
        let field = |name: &'static std::ffi::CStr| -> Result<Option<String>, Thrown> {
            let value = object.get_property(scope, name)?;
            if value.is_undefined() || value.is_null() {
                return Ok(None);
            }
            let string = js::JSString::from_value(scope, value)?.to_utf8(scope)?;
            Ok(Some(string).filter(|string| !string.is_empty()))
        };
        let root = field(c"root")?;
        let dir = field(c"dir")?.or_else(|| root.clone());
        let base = match field(c"base")? {
            Some(base) => base,
            None => {
                let name = field(c"name")?.unwrap_or_default();
                match field(c"ext")? {
                    Some(ext) if ext.starts_with('.') => format!("{name}{ext}"),
                    Some(ext) => format!("{name}.{ext}"),
                    None => name,
                }
            }
        };
        Ok(match dir {
            None => base,
            Some(dir) if Some(&dir) == root.as_ref() => format!("{dir}{base}"),
            Some(dir) => format!("{dir}/{base}"),
        })
    }

    /// <https://nodejs.org/api/path.html#pathparsepath>
    pub fn parse<'s>(scope: &'s Scope<'_>, path: HandleValue<'_>) -> Result<Object<'s>, Thrown> {
        let path = validate_string(scope, path, "path")?;
        let bytes = path.as_bytes();
        let mut root = "";
        let mut dir = "";
        let mut base = "";
        let mut ext = "";
        let mut name = "";
        if !bytes.is_empty() {
            let absolute = bytes[0] == b'/';
            if absolute {
                root = "/";
            }
            let scan = scan_extension(bytes, usize::from(absolute));
            if let Some(end) = scan.end {
                let start = if scan.start_part == 0 && absolute {
                    1
                } else {
                    scan.start_part
                };
                match (scan.valid, scan.start_dot) {
                    (true, Some(dot)) => {
                        name = &path[start..dot];
                        base = &path[start..end];
                        ext = &path[dot..end];
                    }
                    _ => {
                        base = &path[start..end];
                        name = base;
                    }
                }
            }
            if scan.start_part > 0 {
                dir = &path[..scan.start_part - 1];
            } else if absolute {
                dir = "/";
            }
        }
        let parsed = Object::new_plain(scope)?;
        parsed.set_property(scope, c"root", root)?;
        parsed.set_property(scope, c"dir", dir)?;
        parsed.set_property(scope, c"base", base)?;
        parsed.set_property(scope, c"ext", ext)?;
        parsed.set_property(scope, c"name", name)?;
        Ok(parsed)
    }

    /// <https://nodejs.org/api/path.html#pathmatchesglobpath-pattern>: whether `path` matches the
    /// glob `pattern`. Segments with magic match case-insensitively on macOS and Windows.
    pub fn matches_glob(
        scope: &Scope<'_>,
        path: HandleValue<'_>,
        pattern: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let path = validate_string(scope, path, "path")?;
        let pattern = validate_string(scope, pattern, "pattern")?;
        let options = GlobOptions {
            nocase: platform_nocase(),
            nocase_magic_only: true,
            match_base: false,
        };
        let glob = js::heap::RootedTraceableBox::new(Glob::new(scope, &pattern, options)?);
        Ok(glob.matches(scope, &path)?)
    }

    /// `path._makeLong`, which `register_module` makes the same function as `toNamespacedPath` on
    /// the `default` export. The named export is a separate function that does the same.
    #[function(name = "_makeLong")]
    pub fn make_long<'v>(path: HandleValue<'v>) -> HandleValue<'v> {
        path
    }
}

/// Register `node:path`, whose `default` export has the property order of Node.js's, with
/// `_makeLong` last as the same function as `toNamespacedPath`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    if !crate::stub::register_implemented::<path::js_module>(scope, name) {
        return false;
    }
    let order = || -> Result<(), ExnThrown> {
        let default = crate::stub::order_default_export(
            scope,
            name,
            &[
                c"resolve",
                c"normalize",
                c"isAbsolute",
                c"join",
                c"relative",
                c"toNamespacedPath",
                c"dirname",
                c"basename",
                c"extname",
                c"format",
                c"parse",
                c"matchesGlob",
                c"sep",
                c"delimiter",
                c"win32",
                c"posix",
                c"_makeLong",
            ],
        )?;
        let to_namespaced_path = default.get_property(scope, c"toNamespacedPath")?;
        default.set_property(scope, c"_makeLong", to_namespaced_path)
    };
    order().is_ok()
}
