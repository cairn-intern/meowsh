//! zsh's `zstyle` builtin and style table (Src/Modules/zutil.c).
//!
//! Each style name maps to its patterns, sorted by weight (more specific
//! first) and, for equal weights, by order of definition. A lookup takes
//! the first pattern that matches the context.

use crate::parser::{ParseError, Parser};
use crate::pattern::Pattern;
use crate::shell::{array_get, array_set, builtin_err, var_get_opt, var_set, with_shell};
use crate::types::Flow;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// A pattern and the values set for it.
struct Stypat {
    pat: String,
    prog: Pattern,
    weight: u64,
    // `zstyle -e`: the values are code run on retrieval.
    eval: bool,
    vals: Vec<String>,
}

#[derive(Default)]
struct Style {
    pats: Vec<Stypat>,
}

// A BTreeMap so scans come out sorted by style name, as zsh's sorted
// scanhashtable does.
static ZSTYLETAB: Mutex<BTreeMap<String, Style>> = Mutex::new(BTreeMap::new());

fn zstyletab() -> MutexGuard<'static, BTreeMap<String, Style>> {
    ZSTYLETAB.lock().unwrap_or_else(|e| e.into_inner())
}

fn zwarnnam(nam: &str, msg: &str) {
    builtin_err(nam, msg);
}

#[derive(Clone, Copy, PartialEq)]
enum ZsList {
    None,
    Basic,
    Syntax,
}

fn printstylenode(name: &str, s: &Style, printflags: ZsList, contprog: Option<&Pattern>) -> String {
    let mut out = String::new();
    if printflags == ZsList::Basic {
        out.push_str(&quotedzputs(name));
        out.push('\n');
    }
    for p in &s.pats {
        if contprog.is_some_and(|c| !c.matches(&p.pat)) {
            continue;
        }
        if printflags == ZsList::Basic {
            out.push_str(if p.eval { "(eval)" } else { "      " });
            out.push_str("  ");
            out.push_str(&p.pat);
        } else {
            out.push_str("zstyle ");
            if p.eval {
                out.push_str("-e ");
            }
            out.push_str(&quotedzputs(&p.pat));
            out.push(' ');
            out.push_str(&quotedzputs(name));
        }
        for v in &p.vals {
            out.push(' ');
            out.push_str(&quotedzputs(v));
        }
        out.push('\n');
    }
    out
}

fn quotedzputs(s: &str) -> String {
    crate::typeset::quote_value(s)
}

/// How specific a pattern is (zsh's setstypat). Each colon-separated
/// component scores 0 if it is just `*`, 1 if it is otherwise a pattern
/// and 2 if literal; the component count goes in the high half so it is
/// compared first.
fn stypat_weight(pat: &str) -> u64 {
    let chars: Vec<char> = pat.chars().collect();
    let mut weight: u64 = 0;
    let mut tmp: u64 = 2;
    let mut first = true;
    for (i, &c) in chars.iter().enumerate() {
        if first && c == '*' && chars.get(i + 1).is_none_or(|&n| n == ':') {
            tmp = 0;
            continue;
        }
        first = false;
        if matches!(c, '(' | '|' | '*' | '[' | '<' | '?' | '#' | '^') {
            tmp = 1;
        }
        if c == ':' {
            weight += 1 << 32;
            first = true;
            weight += tmp;
            tmp = 2;
        }
    }
    weight + tmp
}

/// Stores `vals` for pattern `pat` of `style`. Returns 1 if `eval` code
/// fails to parse.
fn setstypat(style: &str, pat: &str, prog: Pattern, vals: &[String], eval: bool) -> i32 {
    // zsh adds the style before parsing the code, so a parse failure
    // leaves it present with no patterns.
    zstyletab().entry(style.to_string()).or_default();
    if eval {
        let code = vals.join(" ");
        if let Err(e) = Parser::new(&code, true).parse_program() {
            let msg = match e {
                ParseError::Syntax(m) => m,
                ParseError::Incomplete => "syntax error: unexpected end of file".to_string(),
            };
            crate::shell::shell_err(&msg);
            return 1;
        }
    }
    let mut tab = zstyletab();
    let s = tab.entry(style.to_string()).or_default();
    if let Some(p) = s.pats.iter_mut().find(|p| p.pat == pat) {
        // Exists: replace the values, keeping the compiled pattern.
        p.vals = vals.to_vec();
        p.eval = eval;
        return 0;
    }
    let weight = stypat_weight(pat);
    let at = s
        .pats
        .iter()
        .position(|q| q.weight < weight)
        .unwrap_or(s.pats.len());
    s.pats.insert(
        at,
        Stypat {
            pat: pat.to_string(),
            prog,
            weight,
            eval,
            vals: vals.to_vec(),
        },
    );
    0
}

// As `unset name`: a local keeps its slot and only loses its value.
fn unsetparam(name: &str) {
    with_shell(|s| {
        let local = s
            .param(name)
            .is_some_and(|p| p.level > 0 && p.tie.is_none());
        if local {
            if let Some(p) = s.param_mut(name) {
                p.value = crate::params::Value::Scalar(String::new());
                p.attrs = crate::params::UNSET;
            }
        } else {
            s.remove_param(name);
        }
    });
}

/// Runs a `zstyle -e` style's code and returns `$reply`; None if the
/// code fails or leaves `reply` unset.
fn evalstyle(code: &str) -> Option<Vec<String>> {
    unsetparam("reply");
    crate::shell::in_eval_context("style", || crate::exec::run_string(code));
    // An error stops the code as zsh's errflag does; clear it as zsh
    // restores errflag, so the caller carries on.
    let failed = with_shell(|s| {
        if s.error_exit && matches!(s.flow, Flow::Exit(_)) {
            s.error_exit = false;
            s.flow = Flow::None;
            true
        } else {
            false
        }
    });
    if failed {
        return None;
    }
    let ret = array_get("reply").or_else(|| var_get_opt("reply").map(|v| vec![v]));
    unsetparam("reply");
    ret
}

/// Looks up `style` for context `ctxt` (zsh's lookupstyle): the values of
/// the first matching pattern, evaluated for `zstyle -e` styles.
fn lookupstyle(ctxt: &str, style: &str) -> Option<Vec<String>> {
    let (eval, vals) = {
        let tab = zstyletab();
        let p = tab.get(style)?.pats.iter().find(|p| p.prog.matches(ctxt))?;
        (p.eval, p.vals.clone())
    };
    if eval {
        evalstyle(&vals.join(" "))
    } else {
        Some(vals)
    }
}

/// 0 if some pattern for `style` matches `ctxt`, else 1.
fn testforstyle(ctxt: &str, style: &str) -> i32 {
    let tab = zstyletab();
    let found = tab
        .get(style)
        .is_some_and(|s| s.pats.iter().any(|p| p.prog.matches(ctxt)));
    i32::from(!found)
}

/// The values of `style` in `context`, as `zstyle -a` retrieves them.
/// None when no pattern matches (or `-e` code leaves `reply` unset).
pub fn lookup(context: &str, style: &str) -> Option<Vec<String>> {
    lookupstyle(context, style)
}

/// Whether `style` is true in `context`, as `zstyle -t` tests it with no
/// values given: None when unset, else whether its first value is one of
/// yes, true, on, 1.
pub fn lookup_bool(context: &str, style: &str) -> Option<bool> {
    let vals = lookupstyle(context, style)?;
    Some(
        vals.first()
            .is_some_and(|v| matches!(v.as_str(), "yes" | "true" | "on" | "1")),
    )
}

/// Removes every style (`zstyle -d`).
pub fn clear() {
    zstyletab().clear();
}

/// The `zstyle` builtin; `words[0]` is the command name.
pub fn builtin_zstyle(words: &[String]) -> i32 {
    let nam = words.first().map_or("zstyle", String::as_str);
    bin_zstyle(nam, words.get(1..).unwrap_or(&[]))
}

fn bin_zstyle(nam: &str, mut args: &[String]) -> i32 {
    let mut add = false;
    let mut eval = false;
    let mut list = ZsList::None;

    if args.is_empty() {
        list = ZsList::Basic;
    } else if let Some(rest) = args[0].strip_prefix('-') {
        let mut cs = rest.chars();
        match cs.next() {
            Some(oc) if oc != '-' => {
                if cs.next().is_some() {
                    zwarnnam(nam, &format!("invalid argument: {}", args[0]));
                    return 1;
                }
                if oc == 'L' {
                    list = ZsList::Syntax;
                    args = &args[1..];
                } else if oc == 'e' {
                    eval = true;
                    add = true;
                    args = &args[1..];
                }
            }
            _ => {
                add = true;
                args = &args[1..];
            }
        }
    } else {
        add = true;
    }

    if add {
        if args.len() < 2 {
            zwarnnam(nam, "not enough arguments");
            return 1;
        }
        let prog = Pattern::new(&args[0]);
        return setstypat(&args[1], &args[0], prog, &args[2..], eval);
    }

    if list != ZsList::None {
        let (context, stylename) = match args.len() {
            0 => (None, None),
            1 => (Some(&args[0]), None),
            2 => (Some(&args[0]), Some(&args[1])),
            _ => {
                zwarnnam(nam, "too many arguments");
                return 1;
            }
        };
        let contprog = context.map(|c| Pattern::new(c.as_str()));
        let text = {
            let tab = zstyletab();
            match stylename {
                Some(name) => match tab.get(name.as_str()) {
                    Some(s) => printstylenode(name, s, list, contprog.as_ref()),
                    None => return 1,
                },
                None => tab
                    .iter()
                    .map(|(name, s)| printstylenode(name, s, list, contprog.as_ref()))
                    .collect(),
            }
        };
        let _ = crate::sys::out(&text);
        return 0;
    }

    let opt = args[0].chars().nth(1).unwrap_or('\0');
    let (min, max): (usize, Option<usize>) = match opt {
        'd' => (0, None),
        's' => (3, Some(4)),
        'b' => (3, Some(3)),
        'a' => (3, Some(3)),
        't' => (2, None),
        'T' => (2, None),
        'm' => (3, Some(3)),
        'q' => (2, Some(2)),
        'g' => (1, Some(3)),
        _ => {
            zwarnnam(nam, &format!("bad option: {}", args[0]));
            return 1;
        }
    };
    let n = args.len() - 1;
    if n < min {
        zwarnnam(nam, "not enough arguments");
        return 1;
    } else if max.is_some_and(|m| n > m) {
        zwarnnam(nam, "too many arguments");
        return 1;
    }

    match opt {
        'd' => {
            let mut tab = zstyletab();
            match args.get(1) {
                None => tab.clear(),
                Some(pat) if args.len() > 2 => {
                    for name in &args[2..] {
                        freestypat(&mut tab, name, pat);
                    }
                }
                Some(pat) => {
                    let names: Vec<String> = tab.keys().cloned().collect();
                    for name in &names {
                        freestypat(&mut tab, name, pat);
                    }
                }
            }
            0
        }
        's' => {
            let (ret, val) = match lookupstyle(&args[1], &args[2]) {
                Some(vals) if !vals.is_empty() => {
                    (vals.join(args.get(4).map_or(" ", String::as_str)), 0)
                }
                _ => (String::new(), 1),
            };
            var_set(&args[3], &ret, false);
            val
        }
        'b' => {
            let yes = lookupstyle(&args[1], &args[2]).is_some_and(|vals| {
                vals.len() == 1 && matches!(vals[0].as_str(), "yes" | "true" | "on" | "1")
            });
            var_set(&args[3], if yes { "yes" } else { "no" }, false);
            i32::from(!yes)
        }
        'a' => {
            let (ret, val) = match lookupstyle(&args[1], &args[2]) {
                Some(vals) => (vals, 0),
                None => (Vec::new(), 1),
            };
            array_set(&args[3], ret);
            val
        }
        't' | 'T' => {
            let vals = lookupstyle(&args[1], &args[2]);
            match &vals {
                Some(v) if !v.is_empty() => {
                    if args.len() > 3 {
                        i32::from(!args[3..].iter().any(|a| v.contains(a)))
                    } else {
                        i32::from(!matches!(v[0].as_str(), "true" | "yes" | "on" | "1"))
                    }
                }
                _ if opt == 't' => {
                    if vals.is_some() {
                        1
                    } else {
                        2
                    }
                }
                _ => 0,
            }
        }
        'm' => {
            let hit = lookupstyle(&args[1], &args[2])
                .is_some_and(|vals| vals.iter().any(|v| crate::pattern::matches(&args[3], v)));
            i32::from(!hit)
        }
        'q' => testforstyle(&args[1], &args[2]),
        'g' => {
            let mut ret = 1;
            let mut zstyle_list: Vec<String> = Vec::new();
            {
                let tab = zstyletab();
                match (args.get(2), args.get(3)) {
                    (Some(pat), Some(name)) => {
                        if let Some(p) = tab
                            .get(name.as_str())
                            .and_then(|s| s.pats.iter().find(|p| &p.pat == pat))
                        {
                            zstyle_list.extend(p.vals.iter().cloned());
                            ret = 0;
                        }
                    }
                    (Some(pat), None) => {
                        for (name, s) in tab.iter() {
                            if s.pats.iter().any(|p| &p.pat == pat) {
                                zstyle_list.push(name.clone());
                            }
                        }
                        ret = 0;
                    }
                    _ => {
                        for s in tab.values() {
                            for p in &s.pats {
                                if !zstyle_list.contains(&p.pat) {
                                    zstyle_list.push(p.pat.clone());
                                }
                            }
                        }
                        ret = 0;
                    }
                }
            }
            array_set(&args[1], zstyle_list);
            ret
        }
        _ => 0,
    }
}

/// Removes pattern `pat` from style `name`, and the style itself once it
/// has no patterns left.
fn freestypat(tab: &mut BTreeMap<String, Style>, name: &str, pat: &str) {
    let Some(s) = tab.get_mut(name) else {
        return;
    };
    let Some(i) = s.pats.iter().position(|p| p.pat == pat) else {
        return;
    };
    s.pats.remove(i);
    if s.pats.is_empty() {
        tab.remove(name);
    }
}
