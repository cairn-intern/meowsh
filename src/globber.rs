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
    // `:h`, `:t`, ... applied to each match.
    modifiers: String,
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
    Negate(Box<Test>),
}

/// Splits a trailing qualifier group `(...)` off `pattern`. A final group
/// counts as qualifiers when it has no `|`, as with zsh's BARE_GLOB_QUAL.
pub fn split_qualifiers(pattern: &str) -> (&str, Option<Quals>) {
    let Some(body) = pattern.strip_suffix(')') else {
        return (pattern, None);
    };
    let Some(open) = body.rfind('(') else {
        return (pattern, None);
    };
    if open == 0 || body[..open].ends_with('\\') {
        return (pattern, None);
    }
    let inner = &body[open + 1..];
    let inner = inner.strip_prefix("#q").unwrap_or(inner);
    if inner.contains(['|', '(', ')', '~']) || inner.contains("\\") {
        return (pattern, None);
    }
    match parse_quals(inner) {
        Some(q) => (&body[..open], Some(q)),
        None => (pattern, None),
    }
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
            '.' => Test::Regular,
            '/' => Test::Dir,
            '@' => Test::Link,
            '*' => Test::Exec,
            '=' => Test::Socket,
            'p' => Test::Fifo,
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
                q.sort = Some((key, c == 'O'));
                continue;
            }
            // Accepted without effect.
            '-' | ',' | 'L' | 'U' | 'G' | 's' | 'S' | 't' | 'T' | 'F' => continue,
            '0'..='9' | '[' | ']' => continue,
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
        None => {
            out.sort();
            out.dedup();
        }
    }
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
    if let Some(q) = quals {
        if !q.modifiers.is_empty() {
            out.retain(|p| q.tests.iter().all(|t| passes(Path::new(p), t)));
            sort_matches(&mut out, q);
            return out
                .iter()
                .map(|p| crate::expand::modify_word(p, &q.modifiers))
                .collect();
        }
        out.retain(|p| q.tests.iter().all(|t| passes(Path::new(p), t)));
        if let Some((key, rev)) = q.sort {
            match key {
                'n' => out.sort(),
                'm' => out.sort_by_key(|p| {
                    std::cmp::Reverse(fs::metadata(p).map(|m| m.mtime()).unwrap_or(0))
                }),
                'L' => out.sort_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0)),
                _ => {}
            }
            if rev {
                out.reverse();
            }
            return out;
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualifiers_split_only_without_alternation() {
        assert!(split_qualifiers("*(N)").1.is_some_and(|q| q.null));
        assert!(split_qualifiers("*.(rs|toml)").1.is_none());
        assert_eq!(split_qualifiers("*(Nm-1)").0, "*");
        assert!(split_qualifiers("(N)").1.is_none());
    }

    #[test]
    fn globbing_with_groups_ranges_and_qualifiers() {
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
