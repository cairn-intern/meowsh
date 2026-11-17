//! The second part of zsh's Src/Zle/computil.c: the helper builtins
//! compvalues (for _values), compquote, comptags and comptry (the tag
//! machinery), compfiles (for _path_files) and compgroups.
//!
//! zsh's statics (the _values definition cache and its parse state, the
//! tag sets per function nesting level) are thread_locals here.

use super::complete::{
    ignore_prefix, ignore_suffix, parse_cmatcher, pull_comp_params, push_comp_params,
};
use super::core::{
    begcmgroup, endcmgroup, haswilds, pat_from_tokens, quotestring, rembslash, set_list_array, st,
    tokenize,
};
use super::matching::{pattern_match, pattern_match1, pattern_match_equivalence};
use super::tricky::{
    parse_subst_string, remnulargs, untokenize, COMMA, INBRACE, OUTBRACE, QT_BACKSLASH,
    QT_BACKSLASH_PATTERN,
};
use super::{Cmatcher, Cpattern, CGF_NOSORT, CGF_UNIQALL, CGF_UNIQCON, CMF_LEFT, CMF_RIGHT};
use crate::pattern::Pattern;
use crate::shell::builtin_err;
use crate::zle::Zle;
use std::cell::{Cell, RefCell};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::rc::Rc;

// ---- small helpers ------------------------------------------------------

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn string(s: &[char]) -> String {
    s.iter().collect()
}

fn iblank(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn inblank(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\n'
}

fn name_of<'a>(words: &'a [String], dflt: &'a str) -> &'a str {
    words.first().map(String::as_str).unwrap_or(dflt)
}

fn args_of(words: &[String]) -> &[String] {
    words.get(1..).unwrap_or(&[])
}

/// zsh's `incompfunc == 1` test; reads the special parameters back into
/// the completion state when it holds.
fn in_compfunc() -> bool {
    crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return false;
        }
        pull_comp_params(z);
        true
    }) == Some(true)
}

fn not_in_compfunc(nam: &str) -> i32 {
    builtin_err(nam, "can only be called from completion function");
    1
}

fn setsparam(name: &str, val: &str) {
    crate::shell::var_set(name, val, false);
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Removes backslashes before colons.
fn rembslashcolon(s: &[char]) -> String {
    let mut out = String::with_capacity(s.len());
    for (k, &c) in s.iter().enumerate() {
        if c != '\\' || s.get(k + 1) != Some(&':') {
            out.push(c);
        }
    }
    out
}

/// Adds backslashes before colons and backslashes, undoing rembslash()
/// for _describe.
fn bslashcolon2(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if c == ':' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// zsh's ztat(): stat (lstat with `ls`), retried without backslashes.
fn ztat(nam: &str, ls: bool) -> Option<Metadata> {
    let get = |p: &str| {
        if ls {
            std::fs::symlink_metadata(p)
        } else {
            std::fs::metadata(p)
        }
    };
    get(nam)
        .ok()
        .or_else(|| get(&string(&rembslash(&chars(nam)))).ok())
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

// ---- compvalues ---------------------------------------------------------

const CVV_NOARG: i32 = 0;
const CVV_ARG: i32 = 1;
const CVV_OPT: i32 = 2;

const MAX_CVCACHE: usize = 8;

/// The parts of zsh's struct caarg that _values uses.
#[derive(Clone)]
struct Caarg {
    descr: String,
    action: String,
    opt: String,
}

/// One value definition.
struct Cvval {
    name: String,
    descr: Option<String>,
    xor: Option<Vec<String>>,
    type_: i32,
    arg: Option<Caarg>,
    active: bool,
    not: bool,
}

/// Definitions for _values.
struct Cvdef {
    descr: String,
    hassep: bool,
    /// '\0' when none was given.
    sep: char,
    argsep: char,
    vals: Vec<Cvval>,
    defs: Vec<String>,
    lastt: u64,
    words: bool,
}

type CvdefRef = Rc<RefCell<Cvdef>>;

/// Parse state.
struct Cvstate {
    d: CvdefRef,
    def: Option<Caarg>,
    vals: Vec<String>,
}

thread_local! {
    static CVDEF_CACHE: RefCell<Vec<Option<CvdefRef>>> =
        RefCell::new((0..MAX_CVCACHE).map(|_| None).collect());
    static CV_LASTSTATE: RefCell<Option<Cvstate>> = const { RefCell::new(None) };
    static CV_PARSED: Cell<bool> = const { Cell::new(false) };
}

/// Parses an argument definition (the non-`*` form _values uses).
fn parse_caarg(oname: &str, def: &[char]) -> Caarg {
    let mut i = 0;
    while i < def.len() && def[i] != ':' {
        if def[i] == '\\' && i + 1 < def.len() {
            i += 1;
        }
        i += 1;
    }
    let descr = rembslashcolon(&def[..i]);
    let action = if i < def.len() {
        rembslashcolon(&def[i + 1..])
    } else {
        String::new()
    };
    Caarg {
        descr,
        action,
        opt: oname.to_string(),
    }
}

/// Parses value definitions.
fn parse_cvdef(nam: &str, args: &[String]) -> Option<Cvdef> {
    let mut a = 0;
    let mut sep = '\0';
    let mut asep = '=';
    let mut hassep = false;
    let mut words = false;
    while a + 1 < args.len() {
        match args[a].as_str() {
            "-s" => {
                hassep = true;
                sep = args[a + 1].chars().next().unwrap_or('\0');
                a += 2;
            }
            "-S" => {
                asep = args[a + 1].chars().next().unwrap_or('\0');
                a += 2;
            }
            "-w" => {
                words = true;
                a += 1;
            }
            _ => break,
        }
    }
    if a + 1 >= args.len() {
        builtin_err(nam, "not enough arguments");
        return None;
    }
    let mut ret = Cvdef {
        descr: args[a].clone(),
        hassep,
        sep,
        argsep: asep,
        vals: Vec::new(),
        defs: args.to_vec(),
        lastt: now(),
        words,
    };
    for arg in &args[a + 1..] {
        let p = chars(arg);
        let n = p.len();
        let mut i = 0;
        let mut bs = false;
        let not = p.first() == Some(&'!');
        if not {
            i += 1;
        }
        // xor list?
        let mut xor: Option<Vec<String>> = None;
        if p.get(i) == Some(&'(') {
            let mut list = Vec::new();
            while i < n && p[i] != ')' {
                i += 1;
                while i < n && inblank(p[i]) {
                    i += 1;
                }
                if i >= n || p[i] == ')' {
                    break;
                }
                let q = i;
                i += 1;
                while i < n && p[i] != ')' && !inblank(p[i]) {
                    i += 1;
                }
                if i >= n {
                    break;
                }
                list.push(string(&rembslash(&p[q..i])));
            }
            if i >= n || p[i] != ')' {
                builtin_err(nam, &format!("invalid argument: {}", arg));
                return None;
            }
            i += 1;
            xor = Some(list);
        }
        // More than once allowed?
        let multi = p.get(i) == Some(&'*');
        if multi {
            i += 1;
        }
        let start = i;
        while i < n && p[i] != ':' && p[i] != '[' {
            if p[i] == '\\' && i + 1 < n {
                i += 1;
                bs = true;
            }
            i += 1;
        }
        if hassep && sep == '\0' && start + usize::from(bs) + 1 < i {
            builtin_err(nam, "no multi-letter values with empty separator allowed");
            return None;
        }
        let name = &p[start..i];
        let mut c = p.get(i).copied();
        let mut descr = None;
        if c == Some('[') {
            i += 1;
            let ds = i;
            while i < n && p[i] != ']' {
                if p[i] == '\\' && i + 1 < n {
                    i += 1;
                }
                i += 1;
            }
            if i >= n {
                builtin_err(nam, &format!("invalid value definition: {}", arg));
                return None;
            }
            descr = Some(string(&p[ds..i]));
            i += 1;
            c = p.get(i).copied();
        }
        if c.is_some_and(|c| c != ':') {
            builtin_err(nam, &format!("invalid value definition: {}", arg));
            return None;
        }
        let (type_, carg) = if c == Some(':') {
            if hassep && sep == '\0' {
                builtin_err(nam, "no value with argument with empty separator allowed");
                return None;
            }
            i += 1;
            let t = if p.get(i) == Some(&':') {
                i += 1;
                CVV_OPT
            } else {
                CVV_ARG
            };
            (t, Some(parse_caarg(&string(name), &p[i..])))
        } else {
            (CVV_NOARG, None)
        };
        let rname = string(&rembslash(name));
        if !multi {
            xor.get_or_insert_with(Vec::new).push(rname.clone());
        }
        ret.vals.push(Cvval {
            name: rname,
            descr,
            xor,
            type_,
            arg: carg,
            active: false,
            not,
        });
    }
    Some(ret)
}

/// Gets the definition from the cache or newly built.
fn get_cvdef(nam: &str, args: &[String]) -> Option<CvdefRef> {
    CVDEF_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if let Some(d) = c.iter().flatten().find(|d| d.borrow().defs == args) {
            d.borrow_mut().lastt = now();
            return Some(d.clone());
        }
        // The first free slot, else the least recently used one.
        let slot = c.iter().position(Option::is_none).unwrap_or_else(|| {
            let mut min = 0;
            for (k, d) in c.iter().enumerate() {
                let t = d.as_ref().map_or(0, |d| d.borrow().lastt);
                let mt = c[min].as_ref().map_or(0, |d| d.borrow().lastt);
                if t < mt {
                    min = k;
                }
            }
            min
        });
        let new = Rc::new(RefCell::new(parse_cvdef(nam, args)?));
        c[slot] = Some(new.clone());
        Some(new)
    })
}

/// Gets the definition for a value.
fn cv_get_val(d: &Cvdef, name: &str) -> Option<usize> {
    d.vals.iter().position(|v| v.name == name)
}

fn cv_quote_get_val(z: &Zle, d: &Cvdef, name: &[char]) -> Option<usize> {
    let mut s = name.to_vec();
    parse_subst_string(z, &mut s);
    let s = string(&untokenize(&remnulargs(&s)));
    cv_get_val(d, &s)
}

/// Handles a xor list.
fn cv_inactive(d: &mut Cvdef, xor: Option<&[String]>) {
    if let Some(xor) = xor {
        for x in xor {
            if let Some(v) = cv_get_val(d, x) {
                d.vals[v].active = false;
            }
        }
    }
}

/// The argument starting at `a`; zsh ends it by writing a NUL just before
/// the next value (`end`), which cuts it only when that lies after `a`.
fn cv_arg(s: &[char], a: usize, end: Option<usize>) -> String {
    let a = a.min(s.len());
    let e = match end {
        Some(e) if e > a => e - 1,
        _ => s.len(),
    };
    string(&s[a..e])
}

/// Gets the next value in `s` from `*sp`. Returns its definition and
/// sets `*sp` after the value (and argument); None when no value
/// follows. `*ap` is where the argument starts, if there is one.
fn cv_next(
    z: &Zle,
    d: &Cvdef,
    s: &[char],
    sp: &mut Option<usize>,
    ap: &mut Option<usize>,
) -> Option<usize> {
    let pos = sp.unwrap_or(s.len());
    if pos >= s.len() {
        *sp = None;
        *ap = None;
        return None;
    }
    let find = |from: usize, c: char| {
        s[from.min(s.len())..]
            .iter()
            .position(|&x| x == c)
            .map(|k| k + from)
    };
    if (d.hassep && d.sep == '\0') || d.argsep == '\0' {
        let ec = if d.hassep && d.sep != '\0' {
            d.sep
        } else {
            d.argsep
        };
        let mut i = pos;
        let mut r;
        loop {
            i += 1;
            r = cv_quote_get_val(z, d, &s[pos..i]);
            if r.is_some() || i >= s.len() || s[i] == ec {
                break;
            }
        }
        let os = i;
        if d.hassep && d.sep != '\0' {
            *sp = find(i, d.sep).map(|k| k + 1);
        } else {
            *sp = Some(i);
        }
        if d.argsep != '\0' && s.get(os) == Some(&d.argsep) {
            *ap = Some(os + 1);
            *sp = None;
        } else if r.is_some_and(|r| d.vals[r].type_ != CVV_NOARG) {
            *ap = Some(os);
        } else {
            *ap = None;
        }
        r
    } else if d.hassep {
        let mut ns = find(pos, d.sep);
        let mut skip = false;
        let as_ = find(pos, d.argsep).filter(|&a| ns.is_none_or(|n| a <= n));
        let sap = if let Some(a) = as_ {
            *ap = Some(a + 1);
            ns = find(a + 1, d.sep);
            skip = true;
            Some(a)
        } else {
            *ap = None;
            ns
        };
        let r = cv_quote_get_val(z, d, &s[pos..sap.unwrap_or(s.len())]);
        let noarg = r.is_none_or(|r| d.vals[r].type_ == CVV_NOARG);
        if noarg && skip {
            ns = as_;
        }
        *sp = match ns {
            Some(n) if !(Some(n) == as_ && !noarg) => Some(n + 1),
            _ => None,
        };
        r
    } else {
        *sp = None;
        let as_ = find(pos, d.argsep);
        *ap = as_.map(|a| a + 1);
        cv_quote_get_val(z, d, &s[pos..as_.unwrap_or(s.len())])
    }
}

/// Parses the current word.
fn cv_parse_word(z: &mut Zle, dref: &CvdefRef) {
    let mut d = dref.borrow_mut();
    for v in d.vals.iter_mut() {
        v.active = true;
    }
    let mut vals: Vec<String> = Vec::new();
    let mut arg: Option<usize> = None;
    let mut val: Option<usize> = None;
    let mut nosfx = false;

    if d.words && !z.comp.compwords.is_empty() {
        let words = z.comp.compwords.clone();
        let cur = z.comp.compcurrent;
        for (i, w) in words.iter().enumerate().skip(1) {
            if i as i64 == cur - 1 {
                continue;
            }
            let s = chars(w);
            let mut sp = Some(0);
            while sp.is_some_and(|p| p < s.len()) {
                if let Some(v) = cv_next(z, &d, &s, &mut sp, &mut arg) {
                    vals.push(d.vals[v].name.clone());
                    vals.push(match arg {
                        Some(a) => cv_arg(&s, a, sp),
                        None => String::new(),
                    });
                    if (i as i64) + 1 < cur {
                        let x = d.vals[v].xor.clone();
                        cv_inactive(&mut d, x.as_deref());
                    }
                }
            }
        }
        arg = None;
    }
    let pre = chars(st(&z.comp.compprefix));
    let mut pign = 0;
    let mut sp = Some(0);
    while sp.is_some_and(|p| p < pre.len()) {
        val = cv_next(z, &d, &pre, &mut sp, &mut arg);
        if let Some(v) = val {
            vals.push(d.vals[v].name.clone());
            match arg {
                Some(a) if sp.is_some() => vals.push(cv_arg(&pre, a, sp)),
                Some(a) => {
                    vals.push(format!("{}{}", string(&pre[a..]), st(&z.comp.compsuffix)));
                    nosfx = true;
                }
                None => vals.push(String::new()),
            }
            let x = d.vals[v].xor.clone();
            cv_inactive(&mut d, x.as_deref());
            match sp {
                Some(p) => pign = p,
                None => d.vals[v].active = true,
            }
        }
    }
    let def = match val {
        Some(v) if arg.is_some() && sp.is_none() => d.vals[v].arg.clone(),
        _ => None,
    };

    if !nosfx && d.hassep {
        let mut ign = 0;
        let mut more: Option<Vec<char>> = None;

        ignore_prefix(z, pign);

        let suf = chars(st(&z.comp.compsuffix));
        if d.sep == '\0' && val.is_none_or(|v| d.vals[v].type_ == CVV_NOARG) {
            ign = suf.len();
            more = Some(suf.clone());
        } else if d.sep != '\0' {
            let ns = suf.iter().position(|&c| c == d.sep);
            let as_ = if d.argsep != '\0' {
                suf.iter().position(|&c| c == d.argsep)
            } else {
                None
            };
            ign = match as_ {
                Some(a) if ns.is_none_or(|n| a <= n) => suf.len() - a,
                _ => ns.map_or(0, |n| suf.len() - n),
            };
            more = ns.map(|n| suf[n + 1..].to_vec());
        } else if d.argsep != '\0' {
            if let Some(a) = suf.iter().position(|&c| c == d.argsep) {
                ign = suf.len() - a;
            }
        }
        if ign > 0 {
            ignore_suffix(z, ign);
        }
        if let Some(more) = more {
            let mut mstr = Some(0);
            while mstr.is_some_and(|p| p < more.len()) {
                if let Some(v) = cv_next(z, &d, &more, &mut mstr, &mut arg) {
                    vals.push(d.vals[v].name.clone());
                    match arg {
                        Some(a) if mstr.is_some() => vals.push(cv_arg(&more, a, mstr)),
                        Some(a) => {
                            vals.push(format!("{}{}", string(&more[a..]), st(&z.comp.compsuffix)))
                        }
                        None => vals.push(String::new()),
                    }
                    let x = d.vals[v].xor.clone();
                    cv_inactive(&mut d, x.as_deref());
                }
            }
        }
    } else if let Some(a) = arg {
        ignore_prefix(z, a);
    } else {
        ignore_prefix(z, pign);
    }
    drop(d);

    CV_LASTSTATE.with(|l| {
        *l.borrow_mut() = Some(Cvstate {
            d: dref.clone(),
            def,
            vals,
        })
    });
}

/// The compvalues builtin; `words[0]` is its name.
pub fn builtin_compvalues(words: &[String]) -> i32 {
    let nam = name_of(words, "compvalues");
    let args = args_of(words);
    if args.is_empty() {
        builtin_err(nam, "not enough arguments");
        return 1;
    }
    if !in_compfunc() {
        return not_in_compfunc(nam);
    }
    bin_compvalues(nam, args)
}

fn bin_compvalues(nam: &str, args: &[String]) -> i32 {
    let a0 = chars(&args[0]);
    if a0.first() != Some(&'-') || a0.len() != 2 {
        builtin_err(nam, &format!("invalid argument: {}", args[0]));
        return 1;
    }
    let op = a0[1];
    if op != 'i' && !CV_PARSED.get() {
        builtin_err(nam, "no parsed state");
        return 1;
    }
    let (min, max): (usize, Option<usize>) = match op {
        'i' => (2, None),
        'D' => (2, Some(2)),
        'C' => (1, Some(1)),
        'V' => (3, Some(3)),
        's' => (1, Some(1)),
        'S' => (1, Some(1)),
        'd' => (1, Some(1)),
        'L' => (3, Some(4)),
        'v' => (1, Some(1)),
        _ => {
            builtin_err(nam, &format!("bad option: {}", args[0]));
            return 1;
        }
    };
    let n = args.len() - 1;
    if n < min {
        builtin_err(nam, "not enough arguments");
        return 1;
    } else if max.is_some_and(|m| n > m) {
        builtin_err(nam, "too many arguments");
        return 1;
    }
    if op == 'i' {
        // This initialises the internal data structures. The arguments
        // are those given to _values itself.
        let def = get_cvdef(nam, &args[1..]);
        let cvp = CV_PARSED.replace(false);
        let Some(def) = def else {
            return 1;
        };
        CV_PARSED.set(cvp);
        crate::zle::with_active(|z| {
            cv_parse_word(z, &def);
            push_comp_params(z);
        });
        CV_PARSED.set(true);
        return 0;
    }
    // What the other options read, taken out before parameters are set.
    enum Out {
        Ret(i32),
        Set(Vec<(String, String)>),
        Lists([Vec<String>; 3]),
        Hash(Vec<String>),
    }
    let out = CV_LASTSTATE.with(|l| {
        let l = l.borrow();
        let Some(state) = l.as_ref() else {
            return Out::Ret(1);
        };
        let d = state.d.borrow();
        match op {
            // The description and action to use if some action has to
            // be used at all.
            'D' => match &state.def {
                Some(arg) => Out::Set(vec![
                    (args[1].clone(), arg.descr.clone()),
                    (args[2].clone(), arg.action.clone()),
                ]),
                None => Out::Ret(1),
            },
            // The sub-context (the tag) for the action.
            'C' => match &state.def {
                Some(arg) => Out::Set(vec![(args[1].clone(), arg.opt.clone())]),
                None => Out::Ret(1),
            },
            // The values without, with and with optional arguments, for
            // _describe.
            'V' => {
                let mut lists: [Vec<String>; 3] = Default::default();
                for p in d.vals.iter().filter(|p| p.active && !p.not) {
                    let l = match p.type_ {
                        CVV_NOARG => 0,
                        CVV_ARG => 1,
                        _ => 2,
                    };
                    let n = bslashcolon2(&p.name);
                    lists[l].push(match &p.descr {
                        Some(descr) => format!("{}:{}", n, descr),
                        None => n,
                    });
                }
                Out::Lists(lists)
            }
            // The value separator, if any.
            's' => {
                if d.hassep {
                    Out::Set(vec![(args[1].clone(), sep_str(d.sep))])
                } else {
                    Out::Ret(1)
                }
            }
            // The separator between values and their arguments.
            'S' => Out::Set(vec![(args[1].clone(), sep_str(d.argsep))]),
            // The description string, passed down to _describe.
            'd' => Out::Set(vec![(args[1].clone(), d.descr.clone())]),
            // The description and action of a value's argument.
            'L' => match cv_get_val(&d, &args[1]).map(|v| &d.vals[v]) {
                Some(Cvval {
                    arg: Some(arg),
                    name,
                    ..
                }) => {
                    let mut set = vec![
                        (args[2].clone(), arg.descr.clone()),
                        (args[3].clone(), arg.action.clone()),
                    ];
                    if let Some(a4) = args.get(4) {
                        set.push((a4.clone(), name.clone()));
                    }
                    Out::Set(set)
                }
                _ => Out::Ret(1),
            },
            // The values and their arguments, for $val_args.
            _ => Out::Hash(state.vals.clone()),
        }
    });
    match out {
        Out::Ret(r) => r,
        Out::Set(set) => {
            for (name, val) in set {
                setsparam(&name, &val);
            }
            0
        }
        Out::Lists([noarg, arg, opt]) => {
            set_list_array(&args[1], noarg);
            set_list_array(&args[2], arg);
            set_list_array(&args[3], opt);
            0
        }
        Out::Hash(vals) => {
            crate::shell::assoc_declare(&args[1]);
            crate::shell::array_set(&args[1], vals);
            0
        }
    }
}

fn sep_str(c: char) -> String {
    if c == '\0' {
        String::new()
    } else {
        c.to_string()
    }
}

// ---- compquote ----------------------------------------------------------

fn comp_quote(s: &str, prefix: bool, qt: i32) -> String {
    let x = prefix && s.starts_with('=');
    let s = if x {
        format!("x{}", &s[1..])
    } else {
        s.to_string()
    };
    let ret = quotestring(&s, qt);
    if x {
        let mut c = ret.chars();
        c.next();
        format!("={}", c.as_str())
    } else {
        ret
    }
}

/// The compquote builtin; `words[0]` is its name.
pub fn builtin_compquote(words: &[String]) -> i32 {
    let nam = name_of(words, "compquote");
    let mut args = args_of(words);
    let mut pflag = false;
    while let Some(a) = args.first() {
        if !a.starts_with('-') {
            break;
        }
        if a == "-" || a == "--" {
            args = &args[1..];
            break;
        }
        for c in a.chars().skip(1) {
            if c != 'p' {
                builtin_err(nam, &format!("bad option: -{}", c));
                return 1;
            }
            pflag = true;
        }
        args = &args[1..];
    }
    if args.is_empty() {
        builtin_err(nam, "not enough arguments");
        return 1;
    }
    let qstack = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return None;
        }
        pull_comp_params(z);
        Some(st(&z.comp.compqstack).to_string())
    })
    .flatten();
    let Some(qstack) = qstack else {
        return not_in_compfunc(nam);
    };
    // Anything to do?
    let Some(qt) = qstack.chars().next() else {
        return 0;
    };
    let qt = qt as i32;

    enum Kind {
        Scalar,
        Array,
        Other,
    }
    for name in args {
        let kind = crate::shell::with_shell(|s| {
            let r = s.resolve_name(name);
            s.param(&r).filter(|p| p.is_set()).map(|p| {
                use crate::params::{Value, FLOAT_E, FLOAT_F, INTEGER};
                if p.attrs & (INTEGER | FLOAT_E | FLOAT_F) != 0 {
                    return Kind::Other;
                }
                match p.value {
                    Value::Scalar(_) => Kind::Scalar,
                    Value::Array(_) => Kind::Array,
                    Value::Assoc(_) => Kind::Other,
                }
            })
        });
        match kind {
            Some(Kind::Scalar) => {
                let v = crate::shell::var_get(name);
                setsparam(name, &comp_quote(&v, pflag, qt));
            }
            Some(Kind::Array) => {
                let v = crate::shell::array_get(name).unwrap_or_default();
                let new = v.iter().map(|s| comp_quote(s, pflag, qt)).collect();
                crate::shell::array_set(name, new);
            }
            Some(Kind::Other) => {
                builtin_err(nam, &format!("invalid parameter type: {}", name));
            }
            None => builtin_err(nam, &format!("unknown parameter: {}", name)),
        }
    }
    0
}

// ---- comptags / comptry -------------------------------------------------

/// A tag set.
struct Ctset {
    tags: Vec<String>,
    /// The last tag checked for -A, and where in `tags` it got to.
    tag: Option<String>,
    ptr: usize,
}

/// A bunch of tag sets.
struct Ctags {
    all: Vec<String>,
    context: String,
    init: bool,
    sets: Vec<Ctset>,
}

const MAX_TAGS: usize = 256;

thread_local! {
    /// Tag-set infos, indexed by the function nesting level.
    static COMPTAGS: RefCell<Vec<Option<Ctags>>> =
        RefCell::new((0..MAX_TAGS).map(|_| None).collect());
    /// The level at the last `comptags -i`.
    static LASTTAGLEVEL: Cell<usize> = const { Cell::new(0) };
}

/// zsh's locallevel.
fn locallevel() -> usize {
    crate::shell::with_shell(|s| s.local_scopes.len())
}

/// Sets the tags for the given local level.
fn settags(level: usize, tags: &[String]) {
    COMPTAGS.with(|c| {
        c.borrow_mut()[level] = Some(Ctags {
            all: tags[1..].to_vec(),
            context: tags[0].clone(),
            init: true,
            sets: Vec::new(),
        })
    });
}

/// Checks if an array contains a string; with `colon` only the parts
/// before colons are compared.
fn arrcontains(a: &[String], s: &str, colon: bool) -> bool {
    let tag = |x: &str| x.split(':').next().unwrap_or("").to_string();
    a.iter()
        .any(|x| if colon { tag(x) == tag(s) } else { x == s })
}

/// The comptags builtin; `words[0]` is its name.
pub fn builtin_comptags(words: &[String]) -> i32 {
    let nam = name_of(words, "comptags");
    let args = args_of(words);
    if args.is_empty() {
        builtin_err(nam, "not enough arguments");
        return 1;
    }
    if !in_compfunc() {
        return not_in_compfunc(nam);
    }
    bin_comptags(nam, args)
}

fn bin_comptags(nam: &str, args: &[String]) -> i32 {
    let a0 = chars(&args[0]);
    if a0.first() != Some(&'-') || a0.len() < 2 || (a0.len() > 2 && (a0[2] != '-' || a0.len() > 3))
    {
        builtin_err(nam, &format!("invalid argument: {}", args[0]));
        return 1;
    }
    // `-x-` works on the caller's level; at the top level there is none.
    let Some(level) = locallevel().checked_sub(usize::from(a0.len() > 2)) else {
        builtin_err(nam, "no tags registered");
        return 1;
    };
    if level >= MAX_TAGS {
        builtin_err(nam, "nesting level too deep");
        return 1;
    }
    let op = a0[1];
    let registered = COMPTAGS.with(|c| c.borrow()[level].is_some());
    if op != 'i' && op != 'I' && !registered {
        builtin_err(nam, "no tags registered");
        return 1;
    }
    let (min, max): (usize, Option<usize>) = match op {
        'i' => (2, None),
        'C' => (1, Some(1)),
        'T' => (0, Some(0)),
        'N' => (0, Some(0)),
        'R' => (1, Some(1)),
        'S' => (1, Some(1)),
        'A' => (2, Some(3)),
        _ => {
            builtin_err(nam, &format!("bad option: {}", args[0]));
            return 1;
        }
    };
    let n = args.len() - 1;
    if n < min {
        builtin_err(nam, "not enough arguments");
        return 1;
    } else if max.is_some_and(|m| n > m) {
        builtin_err(nam, "too many arguments");
        return 1;
    }
    match op {
        'i' => {
            settags(level, &args[1..]);
            LASTTAGLEVEL.set(level);
            0
        }
        'C' => {
            let ctx = COMPTAGS.with(|c| {
                c.borrow()[level]
                    .as_ref()
                    .map(|t| t.context.clone())
                    .unwrap_or_default()
            });
            setsparam(&args[1], &ctx);
            0
        }
        'T' => COMPTAGS
            .with(|c| i32::from(c.borrow()[level].as_ref().is_none_or(|t| t.sets.is_empty()))),
        'N' => COMPTAGS.with(|c| {
            let mut c = c.borrow_mut();
            let Some(t) = c[level].as_mut() else {
                return 1;
            };
            if t.init {
                t.init = false;
            } else if !t.sets.is_empty() {
                t.sets.remove(0);
            }
            i32::from(t.sets.is_empty())
        }),
        'R' => COMPTAGS.with(|c| {
            let c = c.borrow();
            let hit = c[level]
                .as_ref()
                .and_then(|t| t.sets.first())
                .is_some_and(|s| arrcontains(&s.tags, &args[1], true));
            i32::from(!hit)
        }),
        'A' => {
            let found = COMPTAGS.with(|c| {
                let mut c = c.borrow_mut();
                let s = c[level].as_mut()?.sets.first_mut()?;
                if s.tag.as_deref() != Some(args[1].as_str()) {
                    s.tag = Some(args[1].clone());
                    s.ptr = 0;
                }
                let pre = chars(&args[1]);
                let l = pre.len();
                let mut hit = None;
                for (q, t) in s.tags.iter().enumerate().skip(s.ptr) {
                    let tq = chars(t);
                    if tq.starts_with(&pre) {
                        if tq.len() == l {
                            hit = Some((q, tq, 0));
                            break;
                        } else if tq[l] == ':' {
                            hit = Some((q, tq, l + 1));
                            break;
                        }
                    }
                }
                let Some((q, tq, off)) = hit else {
                    s.tag = None;
                    return None;
                };
                s.ptr = q + 1;
                let v = string(&tq[off..]);
                let v = if v.starts_with('-') {
                    format!("{}{}", args[1], v)
                } else {
                    v
                };
                let end = tq[off..]
                    .iter()
                    .position(|&c| c == ':')
                    .map_or(tq.len(), |e| off + e);
                Some((v, string(&tq[..end])))
            });
            match found {
                Some((v, r)) => {
                    setsparam(&args[2], &v);
                    if let Some(a3) = args.get(3) {
                        setsparam(a3, &r);
                    }
                    0
                }
                None => 1,
            }
        }
        _ => {
            // 'S'
            let tags = COMPTAGS.with(|c| {
                c.borrow()[level]
                    .as_ref()
                    .and_then(|t| t.sets.first())
                    .map(|s| s.tags.clone())
            });
            match tags {
                Some(tags) => {
                    crate::shell::array_set(&args[1], tags);
                    0
                }
                None => 1,
            }
        }
    }
}

/// The first brace group with a comma in tokenized `s`: its start, end
/// and the commas at its top level.
fn brace_group(s: &[char]) -> Option<(usize, usize, Vec<usize>)> {
    for (b, &c) in s.iter().enumerate() {
        if c != INBRACE {
            continue;
        }
        let mut depth = 0;
        let mut commas = Vec::new();
        for (k, &d) in s.iter().enumerate().skip(b + 1) {
            if d == INBRACE {
                depth += 1;
            } else if d == OUTBRACE {
                if depth == 0 {
                    if !commas.is_empty() {
                        return Some((b, k, commas));
                    }
                    break;
                }
                depth -= 1;
            } else if d == COMMA && depth == 0 {
                commas.push(k);
            }
        }
    }
    None
}

fn hasbraces(s: &[char]) -> bool {
    brace_group(s).is_some()
}

/// Brace-expands tokenized `s` (comma lists), in zsh's order.
fn xpandbraces(s: &[char]) -> Vec<Vec<char>> {
    let Some((b, e, commas)) = brace_group(s) else {
        return vec![s.to_vec()];
    };
    let mut bounds = vec![b];
    bounds.extend(commas);
    bounds.push(e);
    bounds
        .windows(2)
        .flat_map(|w| {
            let mut t = s[..b].to_vec();
            t.extend_from_slice(&s[w[0] + 1..w[1]]);
            t.extend_from_slice(&s[e + 1..]);
            xpandbraces(&t)
        })
        .collect()
}

/// The comptry builtin; `words[0]` is its name.
pub fn builtin_comptry(words: &[String]) -> i32 {
    let nam = name_of(words, "comptry");
    let args = args_of(words);
    if !in_compfunc() {
        return not_in_compfunc(nam);
    }
    let lvl = LASTTAGLEVEL.get();
    if lvl == 0 || !COMPTAGS.with(|c| c.borrow()[lvl].is_some()) {
        builtin_err(nam, "no tags registered");
        return 1;
    }
    if !args.is_empty() {
        COMPTAGS.with(|c| {
            if let Some(t) = c.borrow_mut()[lvl].as_mut() {
                bin_comptry(t, args);
            }
        });
    }
    0
}

fn new_ctset(tags: Vec<String>) -> Ctset {
    Ctset {
        tags,
        tag: None,
        ptr: 0,
    }
}

fn bin_comptry(t: &mut Ctags, args: &[String]) {
    if args[0] == "-m" {
        // zsh keeps one list across all the arguments, so each new set
        // also holds the tags of the arguments before.
        let mut list: Vec<String> = Vec::new();
        for arg in &args[1..] {
            let s = chars(arg);
            let mut i = 0;
            while i < s.len() {
                while i < s.len() && iblank(s[i]) {
                    i += 1;
                }
                let mut q: Vec<char> = Vec::new();
                let mut c: Option<usize> = None;
                while i < s.len() && !inblank(s[i]) {
                    if c.is_none() && s[i] == ':' {
                        c = Some(q.len());
                    }
                    if s[i] == '\\' && i + 1 < s.len() {
                        i += 1;
                    }
                    q.push(s[i]);
                    i += 1;
                }
                if i < s.len() {
                    i += 1;
                }
                if q.is_empty() {
                    continue;
                }
                let tag = &q[..c.unwrap_or(q.len())];
                let mut qq = tag.to_vec();
                let mut k = 0;
                while k < qq.len() {
                    if qq[k] == '\\' && k + 1 < qq.len() {
                        k += 1;
                    } else if qq[k] == '{' {
                        qq[k] = INBRACE;
                    } else if qq[k] == '}' {
                        qq[k] = OUTBRACE;
                    } else if qq[k] == ',' {
                        qq[k] = COMMA;
                    }
                    k += 1;
                }
                let qq = tokenize(&qq);
                if haswilds(&qq) || hasbraces(&qq) {
                    for alt in xpandbraces(&qq) {
                        let prog = Pattern::new(&pat_from_tokens(&alt));
                        for a in &t.all {
                            if list
                                .iter()
                                .any(|l| l.split(':').next().unwrap_or("") == a.as_str())
                            {
                                continue;
                            }
                            if prog.matches(a) {
                                list.push(match c {
                                    Some(c) => format!("{}:{}", a, string(&q[c + 1..])),
                                    None => a.clone(),
                                });
                            }
                        }
                    }
                } else {
                    let tag = string(tag);
                    if arrcontains(&t.all, &tag, false)
                        && !t.sets.iter().any(|set| arrcontains(&set.tags, &tag, false))
                    {
                        list.push(string(&q));
                    }
                }
            }
            if !list.is_empty() {
                t.sets.push(new_ctset(list.clone()));
            }
        }
    } else {
        let (sep, rest) = if args[0] == "-s" {
            (true, &args[1..])
        } else {
            (false, args)
        };
        let kept: Vec<String> = rest
            .iter()
            .filter(|p| {
                arrcontains(&t.all, p, true)
                    && !t.sets.iter().any(|s| arrcontains(&s.tags, p, false))
            })
            .cloned()
            .collect();
        if sep {
            for k in kept {
                t.sets.push(new_ctset(vec![k]));
            }
        } else if !kept.is_empty() {
            t.sets.push(new_ctset(kept));
        }
    }
}

// ---- compfiles ----------------------------------------------------------

const PATH_MAX2: usize = 8192;

/// The parts of the word being completed that compfiles reads.
struct CompWord {
    prefix: String,
    suffix: String,
    patmatch: String,
}

/// The files to accept exactly, without pattern matching (the
/// accept-exact style, `accept`); None when there are none.
fn cfp_test_exact(
    cw: &CompWord,
    names: &[String],
    accept: Option<&[String]>,
    skipped: &str,
) -> Option<Vec<String>> {
    if cw.prefix.is_empty() && cw.suffix.is_empty() {
        return None;
    }
    let accept = accept.unwrap_or(&[]);
    if accept.is_empty()
        || (accept.len() == 1 && ["false", "no", "off", "0"].contains(&accept[0].as_str()))
    {
        return None;
    }
    let mut alist: Option<Vec<Pattern>> = None;
    if accept.len() > 1 || !["true", "yes", "on", "1"].contains(&accept[0].as_str()) {
        let mut l = Vec::new();
        let mut any = false;
        for p in accept {
            if p == "*" {
                any = true;
                break;
            }
            l.push(Pattern::new(&pat_from_tokens(&tokenize(&chars(p)))));
        }
        if !any {
            alist = Some(l);
        }
    }
    let sl = skipped.len() + cw.prefix.len() + cw.suffix.len();
    if sl > PATH_MAX2 {
        return None;
    }
    let suf = format!(
        "{}{}",
        skipped,
        string(&rembslash(&chars(&format!("{}{}", cw.prefix, cw.suffix))))
    );
    let mut ret = Vec::new();
    for p in names {
        if p.len() + sl < PATH_MAX2 {
            let buf = format!("{}{}", p, suf);
            if ztat(&buf, false).is_some() {
                if let Some(al) = &alist {
                    if !al.iter().any(|pp| pp.matches(&buf)) {
                        continue;
                    }
                }
                ret.push(buf);
            }
        }
    }
    (!ret.is_empty()).then_some(ret)
}

/// A pattern matching, at each position, the character of `add` or what
/// the matcher in `ms` for that position allows.
fn cfp_matcher_range(ms: &[Option<Rc<Cmatcher>>], add: &[char]) -> Vec<char> {
    let mut out = Vec::new();
    for (k, &ac) in add.iter().enumerate() {
        let Some(m) = ms.get(k).and_then(Option::as_ref) else {
            out.push(ac);
            continue;
        };
        if m.flags & CMF_RIGHT != 0 {
            out.push('*');
            out.push(ac);
            continue;
        }
        let (Some(lp), Some(wp)) = (m.line.first(), m.word.first()) else {
            out.push(ac);
            continue;
        };
        let mut mt = 0;
        if matches!(lp, Cpattern::Equiv(_)) && matches!(wp, Cpattern::Equiv(_)) {
            out.push('[');
            out.push(ac);
            let ind = pattern_match1(lp, ac, &mut mt);
            if ind != 0 {
                if let Some(c) = pattern_match_equivalence(wp, ind, mt, ac) {
                    out.push(c);
                }
            }
            out.push(']');
            continue;
        }
        match wp {
            Cpattern::Class {
                negated: true,
                text,
            } => {
                out.push('[');
                out.push('^');
                out.extend_from_slice(text);
                out.push(']');
            }
            Cpattern::Class { text, .. } | Cpattern::Equiv(text) => {
                out.push('[');
                let addadd = pattern_match1(wp, ac, &mut mt) == 0;
                if addadd && ac == ']' {
                    out.push(ac);
                }
                out.extend_from_slice(text);
                if addadd && ac != ']' {
                    out.push(ac);
                }
                out.push(']');
            }
            Cpattern::Char(ch) => {
                out.push('[');
                let addadd = *ch != ac;
                if addadd && ac == ']' {
                    out.push(ac);
                }
                out.push(*ch);
                if addadd && ac != ']' {
                    out.push(ac);
                }
                out.push(']');
            }
            Cpattern::Any => out.push('?'),
        }
    }
    out
}

/// Turns the literal prefix `add` into a pattern allowing what the match
/// spec `matcher` allows, cut where the matchers stop applying.
fn cfp_matcher_pats(matcher: &str, add: Vec<char>) -> Vec<char> {
    let Ok(Some(m)) = parse_cmatcher(None, matcher) else {
        return add;
    };
    let mut add = add;
    let mut ms: Vec<Option<Rc<Cmatcher>>> = vec![None; add.len()];
    let mut cur = Some(m);
    while let Some(m) = cur {
        if add.is_empty() {
            break;
        }
        let mut stopp: Option<&[Cpattern]> = None;
        let mut stopl = 0usize;
        if m.flags & (CMF_LEFT | CMF_RIGHT) == 0 {
            if m.llen == 1 && m.wlen == 1 {
                let mut k = 0;
                while k < add.len() {
                    if pattern_match(&m.line, &add[k..], &[], &[]) {
                        if ms[k].is_some() {
                            add.truncate(k);
                            break;
                        }
                        ms[k] = Some(m.clone());
                    }
                    k += 1;
                }
            } else {
                stopp = Some(m.line.as_slice());
                stopl = m.llen.max(0) as usize;
            }
        } else if m.flags & CMF_RIGHT != 0 {
            if m.wlen < 0 && m.llen == 0 && m.ralen == 1 {
                let mut k = 0;
                while k < add.len() {
                    if pattern_match(&m.right, &add[k..], &[], &[]) {
                        if ms[k].is_some() || (k == 0 && add[0] == '.') {
                            add.truncate(k);
                            break;
                        }
                        ms[k] = Some(m.clone());
                    }
                    k += 1;
                }
            } else if m.llen != 0 {
                stopp = Some(m.line.as_slice());
                stopl = m.llen.max(0) as usize;
            } else {
                stopp = Some(m.right.as_slice());
                stopl = m.ralen.max(0) as usize;
            }
        } else {
            if m.lalen == 0 {
                return Vec::new();
            }
            stopp = Some(m.left.as_slice());
            stopl = m.lalen.max(0) as usize;
        }
        if let Some(sp) = stopp {
            let mut k = 0;
            while k <= add.len() && add.len() - k >= stopl {
                if pattern_match(sp, &add[k..], &[], &[]) {
                    add.truncate(k);
                    break;
                }
                k += 1;
            }
        }
        cur = m.next.clone();
    }
    if !add.is_empty() {
        return cfp_matcher_range(&ms, &add);
    }
    add
}

/// Cuts `add` at the first character `f` accepts.
fn trunc_at(add: &mut Vec<char>, f: impl Fn(char) -> bool) {
    if let Some(k) = add.iter().position(|&c| f(c)) {
        add.truncate(k);
    }
}

/// Puts the literal part of the prefix in front of the patterns that
/// start with `*`, so fewer files are read (skipped by _approximate).
fn cfp_opt_pats(cw: &CompWord, pats: &mut [String], matcher: &str) {
    if cw.prefix.is_empty() {
        return;
    }
    if !cw.patmatch.is_empty() {
        let t = rembslash(&chars(&format!("{}{}", cw.prefix, cw.suffix)));
        let t = remnulargs(&tokenize(&t));
        if haswilds(&t) {
            return;
        }
    }
    let special = |c: char| "*?<>()[]|#^~=".contains(c);
    let s = chars(&cw.prefix);
    let mut add: Vec<char> = Vec::with_capacity(s.len() * 2);
    for (k, &c) in s.iter().enumerate() {
        let next = s.get(k + 1).copied();
        if c != '\\' || next.is_none_or(special) {
            if (k == 0 || s[k - 1] != '\\') && special(c) {
                add.push('\\');
            }
            add.push(c);
        }
    }
    for p in pats.iter() {
        if add.is_empty() {
            break;
        }
        let mut q = chars(p);
        if q.is_empty() {
            continue;
        }
        let mut t = q.len() - 1;
        if q[t] == ')' && t > 0 {
            loop {
                t -= 1;
                if t == 0 || matches!(q[t], ')' | '|' | '~' | '(') {
                    break;
                }
            }
            if t != 0 && q[t] == '(' {
                q.truncate(t);
            }
        }
        let mut i = 0;
        while i < q.len() && !add.is_empty() {
            let c = q[i];
            if c == '\\' && i + 1 < q.len() {
                i += 1;
                let e = q[i];
                trunc_at(&mut add, |x| x == e);
            } else if c == '<' {
                trunc_at(&mut add, |x| x.is_ascii_digit());
            } else if c == '[' {
                i += 1;
                let mut x = i;
                if x < q.len() && (q[x] == '!' || q[x] == '^') {
                    x += 1;
                }
                while x < q.len() {
                    if q.get(x + 1) == Some(&'-') && x + 2 < q.len() {
                        // zsh tests the range's first character instead
                        // of the prefix's here, so a forward range
                        // empties the prefix.
                        let (c1, c2) = (q[x], q[x + 2]);
                        if !(q[x] < c1 || q[x] > c2) {
                            add.clear();
                        }
                    } else {
                        let e = q[x];
                        trunc_at(&mut add, |y| y == e);
                    }
                    x += 1;
                }
            } else if !"?*()|~#".contains(c) {
                trunc_at(&mut add, |y| y == c);
            }
            i += 1;
        }
    }
    if !add.is_empty() {
        if !matcher.is_empty() {
            add = cfp_matcher_pats(matcher, add);
        }
        let add = string(&add);
        for p in pats.iter_mut() {
            if p.starts_with('*') {
                *p = format!("{}{}", add, p);
            }
        }
    }
}

fn cfp_bld_pats(cw: &CompWord, names: &[String], skipped: &str, pats: &[String]) -> Vec<String> {
    let dot = !crate::options::is_set("globdots") && cw.prefix.starts_with('.');
    let mut ret = Vec::new();
    for o in names {
        for p in pats {
            ret.push(format!("{}{}{}", o, skipped, p));
            if dot && !p.starts_with('.') {
                ret.push(format!("{}{}.{}", o, skipped, p));
            }
        }
    }
    ret
}

/// Adds the special directories (the special-dirs style, `sdirs`) and
/// the fake files (the fake-files style, `fake`).
fn cfp_add_sdirs(
    cw: &CompWord,
    mut fin: Vec<String>,
    orig: &[String],
    skipped: &str,
    sdirs: &str,
    fake: Option<&[String]>,
) -> Vec<String> {
    let mut add = 0;
    if !sdirs.is_empty() && (crate::options::is_set("globdots") || cw.prefix.starts_with('.')) {
        if ["yes", "true", "on", "1"].contains(&sdirs) {
            add = 2;
        } else if sdirs == ".." {
            add = 1;
        }
    }
    if add != 0 {
        for m in orig {
            fin.push(format!("{}{}..", m, skipped));
            if add == 2 {
                fin.push(format!("{}{}.", m, skipped));
            }
        }
    }
    for f0 in fake.unwrap_or(&[]) {
        let fc = chars(f0);
        let mut f: Vec<char> = Vec::new();
        let mut k = 0;
        while k < fc.len() {
            if fc[k] == ':' {
                break;
            }
            // Quoted colons are stripped here; tokenizing strips other
            // backslashes.
            if fc[k] == '\\' && fc.get(k + 1) == Some(&':') {
                k += 1;
            }
            f.push(fc[k]);
            k += 1;
        }
        if k + 1 >= fc.len() {
            continue;
        }
        let rest = &fc[k + 1..];
        // zsh walks the file list once for all the directories it fits.
        let mut p = 0;
        let prog = Pattern::new(&pat_from_tokens(&tokenize(&f)));
        let fs = string(&f);
        let fst = std::fs::metadata(&fs).ok();
        for m in orig {
            let hit = prog.matches(m)
                || fst.as_ref().is_some_and(|st1| {
                    std::fs::metadata(if m.is_empty() { "." } else { m.as_str() })
                        .is_ok_and(|st2| same_file(st1, &st2))
                });
            if !hit {
                continue;
            }
            while p < rest.len() {
                while p < rest.len() && inblank(rest[p]) {
                    p += 1;
                }
                if p >= rest.len() {
                    break;
                }
                let mut w = String::new();
                while p < rest.len() && !inblank(rest[p]) {
                    if rest[p] == '\\' && p + 1 < rest.len() {
                        p += 1;
                    }
                    w.push(rest[p]);
                    p += 1;
                }
                fin.push(format!("{}{}{}", m, skipped, w));
            }
        }
    }
    fin
}

/// What `compfiles -p`/`-P` passes to cf_pats().
struct CfPats<'a> {
    dirs: bool,
    noopt: bool,
    accept: Option<Vec<String>>,
    skipped: &'a str,
    matcher: &'a str,
    sdirs: &'a str,
    fake: Option<Vec<String>>,
    pats: Vec<String>,
}

fn cf_pats(cw: &CompWord, a: CfPats, names: &[String]) -> Vec<String> {
    if let Some(ret) = cfp_test_exact(cw, names, a.accept.as_deref(), a.skipped) {
        return cfp_add_sdirs(cw, ret, names, a.skipped, a.sdirs, a.fake.as_deref());
    }
    let mut pats = if a.dirs {
        vec!["*(-/)".to_string()]
    } else {
        a.pats
    };
    if !a.noopt {
        cfp_opt_pats(cw, &mut pats, a.matcher);
    }
    cfp_add_sdirs(
        cw,
        cfp_bld_pats(cw, names, a.skipped, &pats),
        names,
        a.skipped,
        a.sdirs,
        a.fake.as_deref(),
    )
}

/// Adds to `ign` the directories that are the current one or a parent
/// of another name (by device and inode, not following links), as the
/// ignore-parents style `style` asks.
fn cf_ignore(names: &[String], ign: &mut Vec<String>, style: &str, path: &str) {
    let pl = path.len();
    let tpar = style.contains("parent");
    let est = if style.contains("pwd") {
        std::fs::symlink_metadata(crate::shell::var_get("PWD")).ok()
    } else {
        None
    };
    if !tpar && est.is_none() {
        return;
    }
    for n in names {
        let Some(nst) = ztat(n, true).filter(|m| m.is_dir()) else {
            continue;
        };
        if est.as_ref().is_some_and(|e| same_file(&nst, e)) {
            ign.push(quotestring(n, QT_BACKSLASH));
            continue;
        }
        if tpar && n.as_bytes().starts_with(path.as_bytes()) {
            let mut c = n.clone();
            let mut found = false;
            while let Some(e) = c.rfind('/').filter(|&e| e > pl) {
                c.truncate(e);
                if ztat(&c, false).is_some_and(|st| same_file(&st, &nst)) {
                    found = true;
                    break;
                }
            }
            if found
                || (c.rfind('/').is_some_and(|e| e > pl)
                    && ztat(&c, true).is_some_and(|st| same_file(&st, &nst)))
            {
                ign.push(quotestring(n, QT_BACKSLASH));
            }
        }
    }
}

/// With a `/` in `pre`, keeps the names in the same first directory;
/// else tells (`amb`) whether the names differ. None when unchanged.
fn cf_remove_other(names: &[String], pre: &str, amb: &mut i32) -> Option<Vec<String>> {
    if let Some(k) = pre.find('/') {
        let pre = &pre[..=k];
        if names.iter().any(|n| n.starts_with(pre)) {
            *amb = 0;
            return Some(
                names
                    .iter()
                    .filter(|n| n.starts_with(pre))
                    .cloned()
                    .collect(),
            );
        }
        match names.first() {
            None => *amb = 0,
            Some(first) => {
                let p = format!("{}/", first.split('/').next().unwrap_or(""));
                if names[1..].iter().any(|n| !n.starts_with(&p)) {
                    *amb = 1;
                    return None;
                }
            }
        }
    } else {
        match names.first() {
            None => *amb = 0,
            Some(first) => {
                if names[1..].iter().any(|n| n != first) {
                    *amb = 1;
                    return None;
                }
            }
        }
    }
    None
}

/// The compfiles builtin; `words[0]` is its name.
///
/// - `compfiles -p parnam1 parnam2 skipped matcher sdirs parnam3 pats...`
///   sets array parnam1 (holding the directories) to the patterns to
///   glob; parnam2 is the accept-exact style, parnam3 fake-files.
/// - `-p-`: the same without cfp_opt_pats() (for _approximate).
/// - `-P`: the same with the pattern `*(-/)`.
/// - `-i names ign style path`: the ignore-parents style.
/// - `-r names prefix`: keeps the names fitting the prefix's directory.
pub fn builtin_compfiles(words: &[String]) -> i32 {
    let nam = name_of(words, "compfiles");
    let args = args_of(words);
    if args.is_empty() {
        builtin_err(nam, "not enough arguments");
        return 1;
    }
    let cw = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return None;
        }
        pull_comp_params(z);
        Some(CompWord {
            prefix: st(&z.comp.compprefix).to_string(),
            suffix: st(&z.comp.compsuffix).to_string(),
            patmatch: st(&z.comp.comppatmatch).to_string(),
        })
    })
    .flatten();
    let Some(cw) = cw else {
        return not_in_compfunc(nam);
    };
    bin_compfiles(nam, args, &cw)
}

fn bin_compfiles(nam: &str, args: &[String], cw: &CompWord) -> i32 {
    let a0 = chars(&args[0]);
    if a0.first() != Some(&'-') {
        builtin_err(nam, &format!("missing option: {}", args[0]));
        return 1;
    }
    match a0.get(1).copied() {
        Some(o @ ('p' | 'P')) => {
            if a0.len() > 2 && (a0[2] != '-' || a0.len() > 3) {
                builtin_err(nam, &format!("bad option: {}", args[0]));
                return 1;
            }
            if args.len() < 7 || (o == 'p' && args.len() < 8) {
                builtin_err(nam, "too few arguments");
                return 1;
            }
            let Some(tmp) = crate::shell::array_get(&args[1]) else {
                builtin_err(nam, &format!("unknown parameter: {}", args[1]));
                return 0;
            };
            let l: Vec<String> = tmp
                .iter()
                .map(|s| quotestring(s, QT_BACKSLASH_PATTERN))
                .collect();
            let a = CfPats {
                dirs: o == 'P',
                noopt: a0.len() > 2,
                accept: crate::shell::array_get(&args[2]),
                skipped: &args[3],
                matcher: &args[4],
                sdirs: &args[5],
                fake: crate::shell::array_get(&args[6]),
                pats: args[7..].to_vec(),
            };
            set_list_array(&args[1], cf_pats(cw, a, &l));
            0
        }
        Some('i') => {
            if a0.len() > 2 {
                builtin_err(nam, &format!("bad option: {}", args[0]));
                return 1;
            }
            if args.len() < 5 {
                builtin_err(nam, "too few arguments");
                return 1;
            }
            if args.len() > 5 {
                builtin_err(nam, "too many arguments");
                return 1;
            }
            let mut l = crate::shell::array_get(&args[2]).unwrap_or_default();
            let Some(tmp) = crate::shell::array_get(&args[1]) else {
                builtin_err(nam, &format!("unknown parameter: {}", args[1]));
                return 0;
            };
            cf_ignore(&tmp, &mut l, &args[3], &args[4]);
            set_list_array(&args[2], l);
            0
        }
        Some('r') => {
            if args.len() < 3 {
                builtin_err(nam, "too few arguments");
                return 1;
            }
            if args.len() > 3 {
                builtin_err(nam, "too many arguments");
                return 1;
            }
            let Some(tmp) = crate::shell::array_get(&args[1]) else {
                builtin_err(nam, &format!("unknown parameter: {}", args[1]));
                return 0;
            };
            let mut ret = 0;
            if let Some(l) = cf_remove_other(&tmp, &args[2], &mut ret) {
                set_list_array(&args[1], l);
            }
            ret
        }
        _ => {
            builtin_err(nam, &format!("bad option: {}", args[0]));
            1
        }
    }
}

// ---- compgroups ---------------------------------------------------------

/// The compgroups builtin; `words[0]` is its name.
pub fn builtin_compgroups(words: &[String]) -> i32 {
    let nam = name_of(words, "compgroups");
    let args = args_of(words);
    if args.is_empty() {
        builtin_err(nam, "not enough arguments");
        return 1;
    }
    let ok = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return false;
        }
        for n in args {
            let n = Some(n.as_str());
            endcmgroup(z, None);
            begcmgroup(z, n, CGF_NOSORT | CGF_UNIQCON);
            endcmgroup(z, None);
            begcmgroup(z, n, CGF_UNIQALL);
            endcmgroup(z, None);
            begcmgroup(z, n, CGF_NOSORT | CGF_UNIQCON);
            endcmgroup(z, None);
            begcmgroup(z, n, CGF_UNIQALL);
            endcmgroup(z, None);
            begcmgroup(z, n, CGF_NOSORT);
            endcmgroup(z, None);
            begcmgroup(z, n, 0);
        }
        true
    });
    if ok != Some(true) {
        return not_in_compfunc(nam);
    }
    0
}
