//! Pathname expansion with zsh patterns: `*`, `?`, `[...]`, `(a|b)`,
//! `<n-m>`, `**/` recursion, and glob qualifiers such as `(N)`, `(.)`,
//! `(/)` and `(m-1)`.
//!
//! Patterns arrive with quoted characters backslash-escaped.

use crate::pattern::Pattern;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

/// Parsed glob qualifiers.
#[derive(Default, Debug)]
pub struct Quals {
    pub null: bool,
    pub dots: bool,
    // File-type and permission tests; each must pass.
    tests: Vec<Test>,
    sort: Option<(char, bool)>,
    sort_code: Option<(String, bool)>,
    // `:h`, `:t`, ... applied to each match.
    modifiers: String,
    // `(e:code:)` / `(+func)`: shell code each match must pass, with
    // $REPLY set to it (and able to replace it via REPLY or reply).
    code: Vec<String>,
    // `(P:word:)`: words put before each match.
    prepend: Vec<String>,
    // `([n,m])`: which of the sorted matches to keep.
    range: Option<(i64, i64)>,
    // `(Yn)`: stop after n matches.
    limit: Option<usize>,
}

#[derive(Debug)]
enum Test {
    Regular,
    Dir,
    Link,
    Exec,
    Socket,
    Fifo,
    Perm(u32),
    // Modified less (`-`) / more (`+`) / exactly than `n` units ago.
    Mtime(char, i64, u64),
    // `F`: a directory with something in it.
    Full,
    Negate(Box<Test>),
}

/// Splits a trailing qualifier group `(...)` off `pattern`. A final group
/// counts as qualifiers when it has no `|`, as with zsh's BARE_GLOB_QUAL.
pub fn split_qualifiers(pattern: &str) -> (&str, Option<Quals>) {
    let chars: Vec<char> = pattern.chars().collect();
    if chars.last() != Some(&')') {
        return (pattern, None);
    }
    // The `(` matching the final `)`.
    let mut depth = 0;
    let mut open = None;
    let mut k = chars.len();
    while k > 0 {
        k -= 1;
        let escaped = k > 0 && chars[k - 1] == '\\';
        match chars[k] {
            ')' if !escaped => depth += 1,
            '(' if !escaped => {
                depth -= 1;
                if depth == 0 {
                    open = Some(k);
                    break;
                }
            }
            _ => {}
        }
    }
    // A word that is all `(...)` is qualifiers on an empty pattern, as
    // in zsh (`$^x(N)` with x=('')).
    let Some(open) = open else {
        return (pattern, None);
    };
    let inner: String = chars[open + 1..chars.len() - 1].iter().collect();
    // Without BARE_GLOB_QUAL only `(#q...)` holds qualifiers.
    let inner = match inner.strip_prefix("#q") {
        Some(rest) => rest.to_string(),
        None if !crate::options::is_set("bareglobqual") => return (pattern, None),
        None if inner.contains('|') || inner.starts_with('#') => return (pattern, None),
        None => inner,
    };
    let byte_open: usize = chars[..open].iter().map(|c| c.len_utf8()).sum();
    match parse_quals(&inner) {
        Some(q) => (&pattern[..byte_open], Some(q)),
        None => (pattern, None),
    }
}

// A qualifier argument: text between a delimiter and its partner
// (`(`, `[`, `{`, `<` pair up; anything else closes itself).
fn delimited(chars: &[char], i: &mut usize) -> Option<String> {
    let open = *chars.get(*i)?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        '{' => '}',
        '<' => '>',
        c => c,
    };
    // Quoted characters arrive backslash-escaped: skip them, and drop
    // the escapes from the text.
    let mut out = String::new();
    let mut k = *i + 1;
    while k < chars.len() {
        match chars[k] {
            '\\' if k + 1 < chars.len() => {
                out.push(chars[k + 1]);
                k += 2;
            }
            c if c == close => {
                *i = k + 1;
                return Some(out);
            }
            c => {
                out.push(c);
                k += 1;
            }
        }
    }
    None
}

fn parse_quals(s: &str) -> Option<Quals> {
    let mut q = Quals::default();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut negate = false;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        let test = match c {
            'N' => {
                q.null = true;
                continue;
            }
            // Modifiers end the qualifiers.
            ':' => {
                q.modifiers = chars[i - 1..].iter().collect();
                break;
            }
            'D' => {
                q.dots = true;
                continue;
            }
            '^' => {
                negate = !negate;
                continue;
            }
            'e' => {
                q.code.push(delimited(&chars, &mut i)?);
                continue;
            }
            '+' => {
                let start = i;
                while chars
                    .get(i)
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                {
                    i += 1;
                }
                q.code.push(chars[start..i].iter().collect());
                continue;
            }
            'P' => {
                q.prepend.push(delimited(&chars, &mut i)?);
                continue;
            }
            'Y' => {
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                // ^Y turns the limit off again.
                q.limit = if negate {
                    None
                } else {
                    Some(chars[start..i].iter().collect::<String>().parse().ok()?)
                };
                continue;
            }
            '[' => {
                i -= 1;
                let spec = delimited(&chars, &mut i)?;
                let eval = |t: &str| crate::arith::eval(t.trim()).ok();
                q.range = Some(match spec.split_once(',') {
                    Some((a, b)) => (eval(a)?, eval(b)?),
                    None => {
                        let n = eval(&spec)?;
                        (n, n)
                    }
                });
                continue;
            }
            '.' => Test::Regular,
            '/' => Test::Dir,
            '@' => Test::Link,
            '*' => Test::Exec,
            '=' => Test::Socket,
            'p' => Test::Fifo,
            'F' => Test::Full,
            'r' => Test::Perm(0o400),
            'w' => Test::Perm(0o200),
            'x' => Test::Perm(0o100),
            'R' => Test::Perm(0o004),
            'W' => Test::Perm(0o002),
            'X' => Test::Perm(0o001),
            'm' => {
                let unit = match chars.get(i) {
                    Some(&u @ ('M' | 'w' | 'h' | 'm' | 's')) => {
                        i += 1;
                        u
                    }
                    _ => 'd',
                };
                let sign = match chars.get(i) {
                    Some(&s @ ('+' | '-')) => {
                        i += 1;
                        s
                    }
                    _ => '=',
                };
                let start = i;
                while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
                    i += 1;
                }
                let n: i64 = chars[start..i].iter().collect::<String>().parse().ok()?;
                let secs = match unit {
                    'M' => 30 * 86400,
                    'w' => 7 * 86400,
                    'h' => 3600,
                    'm' => 60,
                    's' => 1,
                    _ => 86400,
                };
                Test::Mtime(sign, n, secs)
            }
            'o' | 'O' => {
                let key = *chars.get(i)?;
                i += 1;
                let rev = (c == 'O') != negate;
                match key {
                    'e' => {
                        q.sort_code = Some((delimited(&chars, &mut i)?, rev));
                    }
                    '+' => {
                        let start = i;
                        while chars
                            .get(i)
                            .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                        {
                            i += 1;
                        }
                        if start == i {
                            return None;
                        }
                        q.sort_code = Some((chars[start..i].iter().collect(), rev));
                    }
                    _ => q.sort = Some((key, rev)),
                }
                continue;
            }
            // Accepted without effect.
            '-' | ',' | 'L' | 'U' | 'G' | 's' | 'S' | 't' | 'T' => continue,
            '0'..='9' | ']' => continue,
            _ => return None,
        };
        let test = if negate {
            Test::Negate(Box::new(test))
        } else {
            test
        };
        q.tests.push(test);
    }
    Some(q)
}

// `(e:code:)`: runs code with $REPLY set to the match; a zero status
// keeps it, replaced by $reply (if set) or a changed $REPLY.
fn run_code(code: &str, path: &str) -> Option<Vec<String>> {
    crate::shell::var_set("REPLY", path, false);
    crate::shell::with_shell(|s| {
        s.remove_param("reply");
    });
    let st =
        crate::shell::in_eval_context("globqual", || crate::exec::run_string_named(code, None));
    if st != 0 {
        return None;
    }
    let reply = crate::shell::with_shell(|s| s.array("reply").cloned());
    Some(match reply {
        Some(items) => items,
        None => vec![crate::shell::var_get("REPLY")],
    })
}

fn passes(path: &Path, test: &Test) -> bool {
    match test {
        Test::Negate(t) => !passes(path, t),
        Test::Link => fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()),
        t => {
            let Ok(m) = fs::metadata(path) else {
                return false;
            };
            match t {
                Test::Regular => m.is_file(),
                Test::Dir => m.is_dir(),
                Test::Exec => m.is_file() && m.permissions().mode() & 0o111 != 0,
                Test::Socket => m.mode() & libc::S_IFMT == libc::S_IFSOCK,
                Test::Fifo => m.mode() & libc::S_IFMT == libc::S_IFIFO,
                Test::Perm(bits) => m.permissions().mode() & bits != 0,
                Test::Full => {
                    m.is_dir() && fs::read_dir(path).is_ok_and(|mut d| d.next().is_some())
                }
                Test::Mtime(sign, n, secs) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let age = (now - m.mtime()).max(0) / *secs as i64;
                    match sign {
                        '-' => age < *n,
                        '+' => age > *n,
                        _ => age == *n,
                    }
                }
                Test::Link | Test::Negate(_) => unreachable!(),
            }
        }
    }
}

// Splits on `/` that are not escaped.
fn components(pattern: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                out.last_mut().unwrap().push('\\');
                if let Some(n) = chars.next() {
                    out.last_mut().unwrap().push(n);
                }
            }
            '/' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

fn list_dir(dir: &str) -> Vec<String> {
    let path = if dir.is_empty() { "." } else { dir };
    let mut names: Vec<String> = match fs::read_dir(path) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

// Every directory under `dir`, `dir` itself first, not following links.
fn walk_dirs(dir: &str, dots: bool, out: &mut Vec<String>) {
    out.push(dir.to_string());
    for name in list_dir(dir) {
        if name.starts_with('.') && !dots {
            continue;
        }
        let p = join(dir, &name);
        if fs::symlink_metadata(&p).is_ok_and(|m| m.is_dir()) {
            walk_dirs(&p, dots, out);
        }
    }
}

fn sort_matches(out: &mut Vec<String>, q: &Quals) {
    if let Some((code, rev)) = &q.sort_code {
        let saved = crate::shell::var_get_opt("REPLY");
        let mut keyed: Vec<(String, String)> = out
            .iter()
            .map(|p| {
                crate::shell::var_set("REPLY", p, false);
                crate::shell::in_eval_context("globsort", || {
                    crate::exec::run_string_named(code, None);
                });
                (crate::shell::var_get("REPLY"), p.clone())
            })
            .collect();
        match saved {
            Some(v) => {
                crate::shell::var_set("REPLY", &v, false);
            }
            None => crate::shell::with_shell(|s| {
                s.remove_param("REPLY");
            }),
        }
        keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        if *rev {
            keyed.reverse();
        }
        *out = keyed.into_iter().map(|(_, p)| p).collect();
        return;
    }
    match q.sort {
        Some((key, rev)) => {
            match key {
                'm' => out.sort_by_key(|p| {
                    std::cmp::Reverse(fs::metadata(p).map(|m| m.mtime()).unwrap_or(0))
                }),
                'L' => out.sort_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0)),
                _ => out.sort(),
            }
            if rev {
                out.reverse();
            }
        }
        None => sort_names(out),
    }
}

// The default order: by name, numbers by value with NUMERIC_GLOB_SORT.
fn sort_names(out: &mut Vec<String>) {
    if crate::options::is_set("numericglobsort") {
        out.sort_by(|a, b| crate::expand::numeric_order(a, b));
    } else {
        out.sort();
    }
    out.dedup();
}

/// Expands `pattern`. Returns the sorted matches; empty if none.
pub fn glob(pattern: &str, quals: Option<&Quals>) -> Vec<String> {
    // `a~b`: a's matches, less the paths b matches.
    if let Some((main, excl)) = crate::pattern::glob_exclusion(pattern) {
        let ex = Pattern::new(&excl);
        let mut out = glob(&main, quals);
        out.retain(|p| !ex.matches(p));
        return out;
    }
    let dots = quals.is_some_and(|q| q.dots);
    let comps = components(pattern);
    let mut current = vec![String::new()];
    let absolute = pattern.starts_with('/');
    let last = comps.len() - 1;
    for (k, comp) in comps.iter().enumerate() {
        if k == 0 && absolute {
            current = vec!["/".to_string()];
            continue;
        }
        if comp.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        if (comp == "**" || comp == "***") && k < last {
            for dir in &current {
                walk_dirs(dir, dots, &mut next);
            }
            current = next;
            continue;
        }
        if !crate::pattern::has_meta(comp) {
            let lit = crate::pattern::unescape(comp);
            for dir in &current {
                next.push(join(dir, &lit));
            }
            current = next;
            continue;
        }
        let pat = Pattern::new(comp);
        let explicit_dot = comp.starts_with('.') || comp.starts_with("\\.");
        for dir in &current {
            for name in list_dir(dir) {
                if name.starts_with('.') && !(dots || explicit_dot) {
                    continue;
                }
                if name == "." || name == ".." {
                    continue;
                }
                if pat.matches(&name) {
                    let p = join(dir, &name);
                    if k == last || fs::metadata(&p).is_ok_and(|m| m.is_dir()) {
                        next.push(p);
                    }
                }
            }
        }
        current = next;
    }
    let mut out: Vec<String> = current
        .into_iter()
        .filter(|p| !p.is_empty() && fs::symlink_metadata(p).is_ok())
        .collect();
    let Some(q) = quals else {
        sort_names(&mut out);
        return out;
    };
    out.retain(|p| q.tests.iter().all(|t| passes(Path::new(p), t)));
    sort_matches(&mut out, q);
    if !q.code.is_empty() {
        let saved = (
            crate::shell::var_get_opt("REPLY"),
            crate::shell::with_shell(|s| s.array("reply").cloned()),
        );
        let mut kept = Vec::new();
        for p in &out {
            let mut cur = vec![p.clone()];
            for code in &q.code {
                let mut next = Vec::new();
                for c in &cur {
                    if let Some(r) = run_code(code, c) {
                        next.extend(r);
                    }
                }
                cur = next;
            }
            kept.extend(cur);
        }
        match saved.0 {
            Some(v) => {
                crate::shell::var_set("REPLY", &v, false);
            }
            None => crate::shell::with_shell(|s| {
                s.remove_param("REPLY");
            }),
        }
        match saved.1 {
            Some(a) => {
                crate::shell::array_set("reply", a);
            }
            None => crate::shell::with_shell(|s| {
                s.remove_param("reply");
            }),
        }
        out = kept;
    }
    if let Some(n) = q.limit {
        out.truncate(n);
    }
    if let Some((a, b)) = q.range {
        let len = out.len() as i64;
        let idx = |n: i64| if n < 0 { len + n } else { n - 1 };
        let (lo, hi) = (idx(a).max(0), idx(b).min(len - 1));
        out = if lo <= hi && lo < len {
            out[lo as usize..=hi as usize].to_vec()
        } else {
            Vec::new()
        };
    }
    if !q.modifiers.is_empty() {
        out = out
            .iter()
            .map(|p| crate::expand::modify_word(p, &qualifier_text(&q.modifiers)))
            .collect();
        // Sorting by name sorts the modified names, as zsh does.
        // (Names that become the same all stay.)
        if q.range.is_none() && q.sort.is_none_or(|(k, _)| k == 'n') {
            if crate::options::is_set("numericglobsort") {
                out.sort_by(|a, b| crate::expand::numeric_order(a, b));
            } else {
                out.sort();
            }
            if q.sort.is_some_and(|(_, rev)| rev) {
                out.reverse();
            }
        }
    }
    if !q.prepend.is_empty() {
        out = out
            .into_iter()
            .flat_map(|p| {
                q.prepend
                    .iter()
                    .cloned()
                    .chain(std::iter::once(p))
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    out
}

// A qualifier's modifiers as zsh sees them: quoting in the word has made
// expansion characters literal (`\$`), but a replacement's `${match[1]}`
// is expanded per match.
fn qualifier_text(mods: &str) -> String {
    let mut out = String::new();
    let mut cs = mods.chars().peekable();
    while let Some(c) = cs.next() {
        if c == '\\' && cs.peek().is_some_and(|n| "$`{}[]".contains(*n)) {
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifiers_split_only_without_alternation() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::shell::shell_init();
        crate::shell::with_shell(|s| crate::options::init(s, crate::options::Emulation::Zsh));
        assert!(split_qualifiers("*(N)").1.is_some_and(|q| q.null));
        assert!(split_qualifiers("*.(rs|toml)").1.is_none());
        assert_eq!(split_qualifiers("*(Nm-1)").0, "*");
        // As in zsh, a word that is all qualifiers has an empty pattern.
        let (base, q) = split_qualifiers("(N)");
        assert!(base.is_empty() && q.is_some_and(|q| q.null));
    }

    #[test]
    fn globbing_with_groups_ranges_and_qualifiers() {
        let _lock = crate::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::shell::shell_init();
        crate::shell::with_shell(|s| crate::options::init(s, crate::options::Emulation::Zsh));
        let dir = tempfile::TempDir::new().unwrap();
        let d = dir.path().to_str().unwrap();
        for f in ["a.rs", "b.toml", "c.txt", "f1", "f2", "f10", ".hidden"] {
            fs::write(dir.path().join(f), "").unwrap();
        }
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/x.rs"), "").unwrap();
        let names = |v: Vec<String>| -> Vec<String> {
            v.into_iter()
                .map(|p| p[d.len() + 1..].to_string())
                .collect()
        };
        assert_eq!(
            names(glob(&format!("{}/*.(rs|toml)", d), None)),
            ["a.rs", "b.toml"]
        );
        assert_eq!(names(glob(&format!("{}/f<1-2>", d), None)), ["f1", "f2"]);
        assert_eq!(
            names(glob(&format!("{}/**/*.rs", d), None)),
            ["a.rs", "sub/x.rs"]
        );
        let q = parse_quals("/").unwrap();
        assert_eq!(names(glob(&format!("{}/*", d), Some(&q))), ["sub"]);
        let q = parse_quals("D.").unwrap();
        assert!(names(glob(&format!("{}/*", d), Some(&q)))
            .iter()
            .any(|n| n == ".hidden"));
        assert!(glob(&format!("{}/nope*", d), None).is_empty());
    }
}
