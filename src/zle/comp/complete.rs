//! The complete module's interface, after zsh's Src/Zle/complete.c: the
//! compadd and compset builtins, the match-spec parser, the special
//! completion parameters, the function wrapper that restores them and
//! the -prefix, -suffix, -after and -between conditions.
//!
//! zsh backs the special parameters with C variables. Here they are
//! ordinary locals made when the completion function starts
//! ([`make_comp_params`]); [`pull_comp_params`] reads assignments back
//! into the editor state and [`push_comp_params`] writes it out again.

use super::core::{
    atoi, clen, comp_quoting_string, csub, note_patmatch_set, permmatches, set_comp_sep, st,
};
use super::*;
use crate::params::{Param, Value, INTEGER, READONLY, UNSET};
use crate::shell::builtin_err;
use crate::zle::Zle;
use std::collections::BTreeMap;
use std::rc::Rc;

// The bits of the special parameters (CP_*), from their CPN_* indices.
pub const CP_WORDS: u32 = 1 << CPN_WORDS;
pub const CP_REDIRS: u32 = 1 << CPN_REDIRS;
pub const CP_CURRENT: u32 = 1 << CPN_CURRENT;
pub const CP_PREFIX: u32 = 1 << CPN_PREFIX;
pub const CP_SUFFIX: u32 = 1 << CPN_SUFFIX;
pub const CP_IPREFIX: u32 = 1 << CPN_IPREFIX;
pub const CP_ISUFFIX: u32 = 1 << CPN_ISUFFIX;
pub const CP_QIPREFIX: u32 = 1 << CPN_QIPREFIX;
pub const CP_QISUFFIX: u32 = 1 << CPN_QISUFFIX;
pub const CP_COMPSTATE: u32 = 1 << CPN_COMPSTATE;
pub const CP_NMATCHES: u32 = 1 << CPN_NMATCHES;
pub const CP_CONTEXT: u32 = 1 << CPN_CONTEXT;
pub const CP_PARAMETER: u32 = 1 << CPN_PARAMETER;
pub const CP_REDIRECT: u32 = 1 << CPN_REDIRECT;
pub const CP_QUOTE: u32 = 1 << CPN_QUOTE;
pub const CP_QUOTING: u32 = 1 << CPN_QUOTING;
pub const CP_RESTORE: u32 = 1 << CPN_RESTORE;
pub const CP_LIST: u32 = 1 << CPN_LIST;
pub const CP_INSERT: u32 = 1 << CPN_INSERT;
pub const CP_EXACT: u32 = 1 << CPN_EXACT;
pub const CP_EXACTSTR: u32 = 1 << CPN_EXACTSTR;
pub const CP_PATMATCH: u32 = 1 << CPN_PATMATCH;
pub const CP_PATINSERT: u32 = 1 << CPN_PATINSERT;
pub const CP_UNAMBIG: u32 = 1 << CPN_UNAMBIG;
pub const CP_UNAMBIGC: u32 = 1 << CPN_UNAMBIGC;
pub const CP_UNAMBIGP: u32 = 1 << CPN_UNAMBIGP;
pub const CP_INSERTP: u32 = 1 << CPN_INSERTP;
pub const CP_LISTMAX: u32 = 1 << CPN_LISTMAX;
pub const CP_LASTPROMPT: u32 = 1 << CPN_LASTPROMPT;
pub const CP_TOEND: u32 = 1 << CPN_TOEND;
pub const CP_OLDLIST: u32 = 1 << CPN_OLDLIST;
pub const CP_OLDINS: u32 = 1 << CPN_OLDINS;
pub const CP_VARED: u32 = 1 << CPN_VARED;
pub const CP_LISTLINES: u32 = 1 << CPN_LISTLINES;
pub const CP_QUOTES: u32 = 1 << CPN_QUOTES;
pub const CP_IGNORED: u32 = 1 << CPN_IGNORED;

fn inblank(c: char) -> bool {
    c == ' ' || c == '\t'
}

// ---- match specifications -----------------------------------------------

fn pat_warn(name: Option<&str>, msg: &str) {
    match name {
        Some(n) => builtin_err(n, msg),
        None => crate::shell::shell_err(msg),
    }
}

// Parses a class at s[*i] ('[' or '{'), leaving *i on its closing char;
// None when it is not closed. The class keeps its text as written
// (after any `!` or `^`); the matcher code reads ranges and [:name:]
// from it.
fn parse_class(s: &[char], i: &mut usize) -> Option<Cpattern> {
    let open = s[*i];
    *i += 1;
    let (endchar, negated) = if open == '[' {
        let neg = matches!(s.get(*i), Some('!' | '^')) && s.get(*i + 1) != Some(&']');
        if neg {
            *i += 1;
        }
        (']', neg)
    } else {
        ('}', false)
    };
    // The end; the end char can come first, literally.
    let start = *i;
    let mut o = start;
    loop {
        if o != start && s.get(o) == Some(&endchar) {
            break;
        }
        if o >= s.len() {
            *i = o;
            return None;
        }
        if endchar == ']' && s[o] == '[' && s.get(o + 1) == Some(&':') {
            if let Some(n) = s[o + 2..].iter().position(|&c| c == ':').map(|k| k + o + 2) {
                if s.get(n + 1) == Some(&']') {
                    o = n + 1;
                }
            }
        }
        o += 1;
    }
    *i = o;
    let text = s[start..o].to_vec();
    Some(if open == '[' {
        Cpattern::Class { negated, text }
    } else {
        Cpattern::Equiv(text)
    })
}

// Parses a matcher pattern at s[*i] up to `e` (a blank for None); `lp`
// gets how many characters it matches.
fn parse_pattern(
    name: Option<&str>,
    s: &[char],
    i: &mut usize,
    lp: &mut i32,
    e: Option<char>,
) -> Result<Vec<Cpattern>, BadSpec> {
    let mut ret = Vec::new();
    let mut l = 0;
    while *i < s.len()
        && match e {
            Some(e) => s[*i] != e,
            None => !inblank(s[*i]),
        }
    {
        let c = s[*i];
        if c == '[' || c == '{' {
            match parse_class(s, i) {
                Some(p) => ret.push(p),
                None => {
                    pat_warn(name, "unterminated character class");
                    return Err(BadSpec);
                }
            }
            *i += 1;
        } else if c == '?' {
            ret.push(Cpattern::Any);
            *i += 1;
        } else if matches!(c, '*' | '(' | ')' | '=') {
            pat_warn(name, &format!("invalid pattern character `{}'", c));
            return Err(BadSpec);
        } else {
            if c == '\\' && *i + 1 < s.len() {
                *i += 1;
            }
            ret.push(Cpattern::Char(s[*i]));
            *i += 1;
        }
        l += 1;
    }
    *lp = l;
    Ok(ret)
}

/// A match spec that does not parse (zsh's pcm_err).
#[derive(Debug)]
pub struct BadSpec;

/// Parses match specs `s`. Err when it is invalid (zsh's pcm_err), the
/// error printed for builtin `name`; Ok(None) for an empty spec.
pub fn parse_cmatcher(name: Option<&str>, s: &str) -> Result<Option<Rc<Cmatcher>>, BadSpec> {
    let s: Vec<char> = s.chars().collect();
    let warn = |m: &str| {
        if let Some(n) = name {
            builtin_err(n, m);
        }
    };
    let mut list: Vec<Cmatcher> = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let (mut lal, mut ral, mut fl2) = (0, 0, 0u32);
        let mut left = Vec::new();
        let mut right = Vec::new();
        while i < s.len() && inblank(s[i]) {
            i += 1;
        }
        if i >= s.len() {
            break;
        }
        let fl = match s[i] {
            'b' => {
                fl2 = CMF_INTER;
                CMF_LEFT
            }
            'l' => CMF_LEFT,
            'e' => {
                fl2 = CMF_INTER;
                CMF_RIGHT
            }
            'r' => CMF_RIGHT,
            'm' | 'x' => 0,
            'B' => {
                fl2 = CMF_INTER;
                CMF_LEFT | CMF_LINE
            }
            'L' => CMF_LEFT | CMF_LINE,
            'E' => {
                fl2 = CMF_INTER;
                CMF_RIGHT | CMF_LINE
            }
            'R' => CMF_RIGHT | CMF_LINE,
            'M' => CMF_LINE,
            c => {
                warn(&format!("unknown match specification character `{}'", c));
                return Err(BadSpec);
            }
        };
        if s.get(i + 1) != Some(&':') {
            warn("missing `:'");
            return Err(BadSpec);
        }
        if s[i] == 'x' {
            if s.get(i + 2).is_some_and(|&c| !inblank(c)) {
                warn("unexpected pattern following x: specification");
                return Err(BadSpec);
            }
            return Ok(link_matchers(list));
        }
        i += 2;
        if i >= s.len() {
            warn("missing patterns");
            return Err(BadSpec);
        }
        let mut both = false;
        if fl & CMF_LEFT != 0 && fl2 == 0 {
            left = parse_pattern(name, &s, &mut i, &mut lal, Some('|'))?;
            both = i < s.len() && s.get(i + 1) == Some(&'|');
            if both {
                i += 1;
            }
            let ok = i < s.len() && {
                i += 1;
                i < s.len()
            };
            if !ok {
                warn(if both {
                    "missing right anchor"
                } else {
                    "missing line pattern"
                });
                return Err(BadSpec);
            }
        }
        let mut ll = 0;
        let lend = if fl & CMF_RIGHT != 0 && fl2 == 0 {
            '|'
        } else {
            '='
        };
        let mut line = parse_pattern(name, &s, &mut i, &mut ll, Some(lend))?;
        if both {
            right = std::mem::take(&mut line);
            ral = ll;
            ll = 0;
        }
        if fl & CMF_RIGHT != 0 && fl2 == 0 {
            let ok = i < s.len() && {
                i += 1;
                i < s.len()
            };
            if !ok {
                warn("missing right anchor");
                return Err(BadSpec);
            }
        } else {
            if i >= s.len() {
                warn("missing word pattern");
                return Err(BadSpec);
            }
            i += 1;
        }
        if fl & CMF_RIGHT != 0 && fl2 == 0 {
            if s.get(i) == Some(&'|') {
                left = std::mem::take(&mut line);
                lal = ll;
                ll = 0;
                i += 1;
            }
            right = parse_pattern(name, &s, &mut i, &mut ral, Some('='))?;
            if i >= s.len() {
                warn("missing word pattern");
                return Err(BadSpec);
            }
            i += 1;
        }
        let (word, wl) = if s.get(i) == Some(&'*') {
            if fl & (CMF_LEFT | CMF_RIGHT) == 0 {
                warn("need anchor for `*'");
                return Err(BadSpec);
            }
            i += 1;
            if s.get(i) == Some(&'*') {
                i += 1;
                (Vec::new(), -2)
            } else {
                (Vec::new(), -1)
            }
        } else {
            let mut wl = 0;
            let word = parse_pattern(name, &s, &mut i, &mut wl, None)?;
            if word.is_empty() && line.is_empty() {
                warn("need non-empty word or line pattern");
                return Err(BadSpec);
            }
            (word, wl)
        };
        list.push(Cmatcher {
            next: None,
            flags: fl | fl2,
            line,
            llen: ll,
            word,
            wlen: wl,
            left,
            lalen: lal,
            right,
            ralen: ral,
        });
    }
    Ok(link_matchers(list))
}

fn link_matchers(list: Vec<Cmatcher>) -> Option<Rc<Cmatcher>> {
    let mut next: Option<Rc<Cmatcher>> = None;
    for mut m in list.into_iter().rev() {
        m.next = next;
        next = Some(Rc::new(m));
    }
    next
}

// ---- compadd ------------------------------------------------------------

const ORDEROPTS: [(&str, usize, u32); 4] = [
    ("nosort", 2, CAF_NOSORT),
    ("match", 3, CAF_MATSORT),
    ("numeric", 3, CAF_NUMSORT),
    ("reverse", 3, CAF_REVSORT),
];

// Parses compadd -o's argument into `flags`; -1 when it is no ordering.
fn parse_ordering(arg: &str, flags: Option<&mut u32>) -> i32 {
    let mut fl = 0;
    for opt in arg.split(',') {
        match ORDEROPTS
            .iter()
            .rev()
            .find(|o| opt.len() >= o.1 && o.0.starts_with(opt))
        {
            Some(o) => fl |= o.2,
            None => {
                // Default to "match".
                if let Some(f) = flags {
                    *f = CAF_MATSORT;
                }
                return -1;
            }
        }
    }
    if let Some(f) = flags {
        *f |= fl;
    }
    0
}

/// The compadd builtin; `words[0]` is its name.
pub fn builtin_compadd(words: &[String]) -> i32 {
    let name = words.first().map(String::as_str).unwrap_or("compadd");
    let args = words.get(1..).unwrap_or(&[]);
    let r = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return None;
        }
        pull_comp_params(z);
        let r = bin_compadd(z, name, args);
        push_comp_params(z);
        Some(r)
    })
    .flatten();
    match r {
        Some(r) => r,
        None => {
            builtin_err(name, "can only be called from completion function");
            1
        }
    }
}

// Where an option's argument goes.
#[derive(Clone, Copy, PartialEq)]
enum Arg {
    Ign,
    Pre,
    Suf,
    Group,
    Ipre,
    Isuf,
    Ppre,
    Psuf,
    Prpre,
    Match,
    Exp,
    Mesg,
    Rems,
    Remf,
    Apar,
    Opar,
    Dpar,
    Disp,
    Order,
}

fn bin_compadd(z: &mut Zle, name: &str, argv: &[String]) -> i32 {
    let mut dat = Cadata {
        aflags: CAF_MATCH,
        dummies: -1,
        ..Default::default()
    };
    let mut mstr: Option<String> = None;
    let mut oarg: Option<String> = None;
    let mut ai = 0;
    'opts: while ai < argv.len() && argv[ai].starts_with('-') {
        let arg: Vec<char> = argv[ai].chars().collect();
        if arg.len() == 1 {
            ai += 1;
            break;
        }
        let mut p = 1;
        while p < arg.len() {
            let mut order = 0;
            let mut sp: Option<Arg> = None;
            let mut e = "";
            match arg[p] {
                'q' => dat.flags |= CMF_REMOVE,
                'Q' => dat.aflags |= CAF_QUOTE,
                'C' => dat.aflags |= CAF_ALL,
                'f' => dat.flags |= CMF_FILE,
                'e' => dat.flags |= CMF_ISPAR,
                'a' => dat.aflags |= CAF_ARRAYS,
                'k' => dat.aflags |= CAF_ARRAYS | CAF_KEYS,
                'F' => {
                    sp = Some(Arg::Ign);
                    e = "string expected after -";
                }
                'n' => dat.flags |= CMF_NOLIST,
                'U' => dat.aflags &= !CAF_MATCH,
                'P' => {
                    sp = Some(Arg::Pre);
                    e = "string expected after -";
                }
                'S' => {
                    sp = Some(Arg::Suf);
                    e = "string expected after -";
                }
                'J' => {
                    sp = Some(Arg::Group);
                    e = "group name expected after -";
                }
                'V' => {
                    if dat.group.is_none() {
                        dat.aflags |= CAF_NOSORT;
                    }
                    sp = Some(Arg::Group);
                    e = "group name expected after -";
                }
                '1' => {
                    if dat.aflags & CAF_UNIQCON == 0 {
                        dat.aflags |= CAF_UNIQALL;
                    }
                }
                '2' => {
                    if dat.aflags & CAF_UNIQALL == 0 {
                        dat.aflags |= CAF_UNIQCON;
                    }
                }
                'i' => {
                    sp = Some(Arg::Ipre);
                    e = "string expected after -";
                }
                'I' => {
                    sp = Some(Arg::Isuf);
                    e = "string expected after -";
                }
                'p' => {
                    sp = Some(Arg::Ppre);
                    e = "string expected after -";
                }
                's' => {
                    sp = Some(Arg::Psuf);
                    e = "string expected after -";
                }
                'W' => {
                    sp = Some(Arg::Prpre);
                    e = "string expected after -";
                }
                'M' => {
                    sp = Some(Arg::Match);
                    e = "matching specification expected after -";
                }
                'X' => {
                    sp = Some(Arg::Exp);
                    e = "string expected after -";
                }
                'x' => {
                    sp = Some(Arg::Mesg);
                    e = "string expected after -";
                }
                'r' => {
                    dat.flags |= CMF_REMOVE;
                    sp = Some(Arg::Rems);
                    e = "string expected after -";
                }
                'R' => {
                    dat.flags |= CMF_REMOVE;
                    sp = Some(Arg::Remf);
                    e = "function name expected after -";
                }
                'A' => {
                    sp = Some(Arg::Apar);
                    e = "parameter name expected after -";
                }
                'O' => {
                    sp = Some(Arg::Opar);
                    e = "parameter name expected after -";
                }
                'D' => {
                    sp = Some(Arg::Dpar);
                    e = "parameter name expected after -";
                }
                'd' => {
                    sp = Some(Arg::Disp);
                    e = "parameter name expected after -";
                }
                'l' => dat.flags |= CMF_DISPLINE,
                'o' => {
                    // Only the first -o counts, but later ones still take
                    // a valid argument.
                    order = if oarg.is_some() { -1 } else { 1 };
                    sp = Some(Arg::Order);
                }
                'E' => {
                    if p + 1 < arg.len() {
                        let rest: String = arg[p + 1..].iter().collect();
                        dat.dummies = atoi(&rest);
                        p = arg.len() - 1;
                    } else if ai + 1 < argv.len() {
                        ai += 1;
                        dat.dummies = atoi(&argv[ai]);
                    } else {
                        builtin_err(name, &format!("number expected after -{}", arg[p]));
                        return 1;
                    }
                    if dat.dummies < 0 {
                        builtin_err(name, &format!("invalid number: {}", dat.dummies));
                        return 1;
                    }
                }
                '-' => {
                    ai += 1;
                    break 'opts;
                }
                c => {
                    builtin_err(name, &format!("bad option: -{}", c));
                    return 1;
                }
            }
            if let Some(target) = sp {
                let mut m: Option<String> = None;
                let value: Option<String> = if p + 1 < arg.len() {
                    // -Xfoo
                    let rest: String = arg[p + 1..].iter().collect();
                    let v = Some(rest);
                    if target == Arg::Order && oarg.is_none() {
                        oarg = v.clone();
                    }
                    let parsed = order != 0
                        && parse_ordering(
                            st(&oarg),
                            if order == 1 {
                                Some(&mut dat.aflags)
                            } else {
                                None
                            },
                        ) != 0;
                    if !parsed {
                        p = arg.len() - 1;
                    }
                    v
                } else if ai + 1 < argv.len() {
                    // -X foo
                    ai += 1;
                    let v = Some(argv[ai].clone());
                    if target == Arg::Order && oarg.is_none() {
                        oarg = v.clone();
                    }
                    if order != 0
                        && parse_ordering(
                            st(&oarg),
                            if order == 1 {
                                Some(&mut dat.aflags)
                            } else {
                                None
                            },
                        ) != 0
                    {
                        ai -= 1;
                    }
                    v
                } else if order == 0 {
                    builtin_err(name, &format!("{}{}", e, arg[p]));
                    return 1;
                } else {
                    None
                };
                if let Some(v) = value {
                    let first = |slot: &mut Option<String>| {
                        if slot.is_none() {
                            *slot = Some(v.clone());
                        }
                    };
                    match target {
                        Arg::Ign => first(&mut dat.ign),
                        Arg::Pre => first(&mut dat.pre),
                        Arg::Suf => first(&mut dat.suf),
                        Arg::Group => first(&mut dat.group),
                        Arg::Ipre => first(&mut dat.ipre),
                        Arg::Isuf => first(&mut dat.isuf),
                        Arg::Ppre => first(&mut dat.ppre),
                        Arg::Psuf => first(&mut dat.psuf),
                        Arg::Prpre => first(&mut dat.prpre),
                        Arg::Exp => first(&mut dat.exp),
                        Arg::Mesg => first(&mut dat.mesg),
                        Arg::Rems => first(&mut dat.rems),
                        Arg::Remf => first(&mut dat.remf),
                        Arg::Apar => first(&mut dat.apar),
                        Arg::Opar => first(&mut dat.opar),
                        Arg::Disp => first(&mut dat.disp),
                        Arg::Dpar => dat.dpar.push(v.clone()),
                        Arg::Match => m = Some(v.clone()),
                        Arg::Order => {}
                    }
                }
                if let Some(m) = m {
                    mstr = Some(match mstr {
                        Some(s) => format!("{} {}", s, m),
                        None => m,
                    });
                }
            }
            p += 1;
        }
        ai += 1;
    }
    let mut matcher = None;
    if let Some(ms) = &mstr {
        if dat.aflags & CAF_MATCH != 0 {
            match parse_cmatcher(Some(name), ms) {
                Ok(m) => matcher = m,
                Err(BadSpec) => return 1,
            }
        }
    }
    let rest = &argv[ai.min(argv.len())..];
    if rest.is_empty()
        && dat.group.is_none()
        && dat.mesg.is_none()
        && dat.aflags & (CAF_NOSORT | CAF_UNIQALL | CAF_UNIQCON | CAF_ALL) == 0
    {
        return 1;
    }
    dat.match_ = matcher;
    super::core::addmatches(z, &mut dat, rest)
}

/// compcall: without compctl there is nothing to call.
pub fn builtin_compcall(words: &[String]) -> i32 {
    let name = words.first().map(String::as_str).unwrap_or("compcall");
    if crate::zle::with_active(|z| z.comp.incompfunc == 1) != Some(true) {
        builtin_err(name, "can only be called from completion function");
    }
    1
}

// ---- compset ------------------------------------------------------------

const CVT_RANGENUM: i32 = 0;
const CVT_RANGEPAT: i32 = 1;
const CVT_PRENUM: i32 = 2;
const CVT_PREPAT: i32 = 3;
const CVT_SUFNUM: i32 = 4;
const CVT_SUFPAT: i32 = 5;

/// Moves the first `l` characters of PREFIX to IPREFIX.
pub fn ignore_prefix(z: &mut Zle, l: usize) {
    if l > 0 {
        let pre = st(&z.comp.compprefix).to_string();
        let l = l.min(clen(&pre));
        z.comp.compiprefix = Some(format!("{}{}", st(&z.comp.compiprefix), csub(&pre, 0, l)));
        z.comp.compprefix = Some(pre.chars().skip(l).collect());
    }
}

/// Moves the last `l` characters of SUFFIX to ISUFFIX.
pub fn ignore_suffix(z: &mut Zle, l: usize) {
    if l > 0 {
        let suf = st(&z.comp.compsuffix).to_string();
        let k = clen(&suf).saturating_sub(l);
        z.comp.compisuffix = Some(format!(
            "{}{}",
            suf.chars().skip(k).collect::<String>(),
            st(&z.comp.compisuffix)
        ));
        z.comp.compsuffix = Some(csub(&suf, 0, k));
    }
}

/// Keeps only words b to e (from 0) in $words.
pub fn restrict_range(z: &mut Zle, b: i32, e: i32) {
    let wl = z.comp.compwords.len() as i32 - 1;
    if wl != 0 && b >= 0 && e >= 0 && (b > 0 || e < wl) {
        let e = e.min(wl);
        z.comp.compwords = if e >= b {
            z.comp.compwords[b as usize..=e as usize].to_vec()
        } else {
            Vec::new()
        };
        z.comp.compcurrent -= b as i64;
    }
}

fn pattern_of(sa: &str, expand: bool) -> crate::pattern::Pattern {
    let p = if expand {
        crate::expand::expand_pattern(sa).unwrap_or_else(|_| sa.to_string())
    } else {
        sa.to_string()
    };
    crate::pattern::Pattern::new(&p)
}

/// The tests of compset and the conditions; with `mod_` the special
/// parameters are changed as compset does.
fn do_comp_vars(
    z: &mut Zle,
    test: i32,
    mut na: i32,
    sa: &str,
    mut nb: i32,
    sb: Option<&str>,
    mod_: bool,
) -> bool {
    match test {
        CVT_RANGENUM => {
            let l = z.comp.compwords.len() as i32;
            if na < 0 {
                na += l;
            } else {
                na -= 1;
            }
            if nb < 0 {
                nb += l;
            } else {
                nb -= 1;
            }
            let cur = z.comp.compcurrent as i32 - 1;
            if cur < na || cur > nb {
                return false;
            }
            if mod_ {
                restrict_range(z, na, nb);
            }
            true
        }
        CVT_RANGEPAT => {
            let l = z.comp.compwords.len() as i32;
            let (mut t, mut b, mut e) = (false, 0, l - 1);
            let mut i = z.comp.compcurrent as i32 - 1;
            if i < 0 || i >= l {
                return false;
            }
            let pp = pattern_of(sa, true);
            i -= 1;
            while i >= 0 {
                if pp.matches(&z.comp.compwords[i as usize]) {
                    b = i + 1;
                    t = true;
                    break;
                }
                i -= 1;
            }
            if let Some(sb) = sb.filter(|_| t) {
                let mut tt = false;
                let pp = pattern_of(sb, true);
                i += 1;
                while i < l {
                    if pp.matches(&z.comp.compwords[i as usize]) {
                        e = i - 1;
                        tt = true;
                        break;
                    }
                    i += 1;
                }
                if tt && (i as i64) < z.comp.compcurrent {
                    t = false;
                }
            }
            if e < b {
                t = false;
            }
            if t && mod_ {
                restrict_range(z, b, e);
            }
            t
        }
        CVT_PRENUM | CVT_SUFNUM => {
            if na < 0 {
                return false;
            }
            if na > 0 && mod_ {
                let s = if test == CVT_PRENUM {
                    st(&z.comp.compprefix)
                } else {
                    st(&z.comp.compsuffix)
                };
                if (clen(s) as i32) < na {
                    return false;
                }
                if test == CVT_PRENUM {
                    ignore_prefix(z, na as usize);
                } else {
                    ignore_suffix(z, na as usize);
                }
            }
            true
        }
        CVT_PREPAT | CVT_SUFPAT => {
            if na == 0 {
                return false;
            }
            let pp = pattern_of(sa, false);
            if test == CVT_PREPAT {
                let pre: Vec<char> = st(&z.comp.compprefix).chars().collect();
                let l = pre.len();
                if l == 0 {
                    return (na == 1 || na == -1) && pp.matches("");
                }
                let (mut p, add) = if na < 0 {
                    na = -na;
                    (l, -1)
                } else {
                    (1, 1)
                };
                loop {
                    if pp.matches_chars(&pre[..p]) {
                        na -= 1;
                        if na == 0 {
                            break;
                        }
                    }
                    if add > 0 {
                        if p == l {
                            return false;
                        }
                        p += 1;
                    } else {
                        if p == 0 {
                            return false;
                        }
                        p -= 1;
                    }
                }
                if mod_ {
                    ignore_prefix(z, p);
                }
            } else {
                let suf: Vec<char> = st(&z.comp.compsuffix).chars().collect();
                let l = suf.len();
                if l == 0 {
                    return (na == 1 || na == -1) && pp.matches("");
                }
                let (mut p, add) = if na < 0 {
                    na = -na;
                    (0, 1)
                } else {
                    (l - 1, -1)
                };
                loop {
                    if pp.matches_chars(&suf[p..]) {
                        na -= 1;
                        if na == 0 {
                            break;
                        }
                    }
                    if add > 0 {
                        if p == l {
                            return false;
                        }
                        p += 1;
                    } else {
                        if p == 0 {
                            return false;
                        }
                        p -= 1;
                    }
                }
                if mod_ {
                    ignore_suffix(z, l - p);
                }
            }
            true
        }
        _ => false,
    }
}

/// The compset builtin; `words[0]` is its name.
pub fn builtin_compset(words: &[String]) -> i32 {
    let name = words.first().map(String::as_str).unwrap_or("compset");
    let args = words.get(1..).unwrap_or(&[]);
    if args.is_empty() {
        builtin_err(name, "not enough arguments");
        return 1;
    }
    if args.len() > 3 {
        builtin_err(name, "too many arguments");
        return 1;
    }
    let r = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return None;
        }
        pull_comp_params(z);
        let r = bin_compset(z, name, args);
        push_comp_params(z);
        Some(r)
    })
    .flatten();
    match r {
        Some(r) => r,
        None => {
            builtin_err(name, "can only be called from completion function");
            1
        }
    }
}

fn bin_compset(z: &mut Zle, name: &str, argv: &[String]) -> i32 {
    let a0: Vec<char> = argv[0].chars().collect();
    if a0.first() != Some(&'-') {
        builtin_err(name, "missing option");
        return 1;
    }
    let test = match a0.get(1) {
        Some('n') => CVT_RANGENUM,
        Some('N') => CVT_RANGEPAT,
        Some('p') => CVT_PRENUM,
        Some('P') => CVT_PREPAT,
        Some('s') => CVT_SUFNUM,
        Some('S') => CVT_SUFPAT,
        Some('q') => return set_comp_sep(z),
        c => {
            builtin_err(
                name,
                &format!(
                    "bad option: -{}",
                    c.map(|c| c.to_string()).unwrap_or_default()
                ),
            );
            return 1;
        }
    };
    let opt = a0[1];
    let (mut sa, sb, na_idx): (String, Option<&String>, usize) = if a0.len() > 2 {
        (a0[2..].iter().collect(), argv.get(1), 2)
    } else {
        match argv.get(1) {
            Some(a) => (a.clone(), argv.get(2), 3),
            None => {
                builtin_err(name, &format!("missing string for option -{}", opt));
                return 1;
            }
        }
    };
    let too_many = if test == CVT_PRENUM || test == CVT_SUFNUM {
        sb.is_some()
    } else {
        sb.is_some() && argv.get(na_idx).is_some()
    };
    if too_many {
        builtin_err(name, "too many arguments");
        return 1;
    }
    let (mut na, mut nb) = (0, 0);
    let mut sb = sb.cloned();
    match test {
        CVT_RANGENUM => {
            na = atoi(&sa);
            nb = sb.as_deref().map(atoi).unwrap_or(-1);
        }
        CVT_PRENUM | CVT_SUFNUM => na = atoi(&sa),
        CVT_PREPAT | CVT_SUFPAT => match sb.take() {
            Some(b) => {
                na = atoi(&sa);
                sa = b;
            }
            None => na = -1,
        },
        _ => {}
    }
    i32::from(!do_comp_vars(z, test, na, &sa, nb, sb.as_deref(), true))
}

// ---- the special parameters ---------------------------------------------

const REAL_NAMES: [&str; 9] = [
    "words",
    "redirections",
    "CURRENT",
    "PREFIX",
    "SUFFIX",
    "IPREFIX",
    "ISUFFIX",
    "QIPREFIX",
    "QISUFFIX",
];
// Those that shell code may assign (the rest are read-only).
const REAL_WRITABLE: usize = 7;

const KEY_NAMES: [&str; 26] = [
    "nmatches",
    "context",
    "parameter",
    "redirect",
    "quote",
    "quoting",
    "restore",
    "list",
    "insert",
    "exact",
    "exact_string",
    "pattern_match",
    "pattern_insert",
    "unambiguous",
    "unambiguous_cursor",
    "unambiguous_positions",
    "insert_positions",
    "list_max",
    "last_prompt",
    "to_end",
    "old_list",
    "old_insert",
    "vared",
    "list_lines",
    "all_quotes",
    "ignored",
];
const KEY_READONLY: u32 = CP_NMATCHES
    | CP_QUOTE
    | CP_QUOTING
    | CP_UNAMBIG
    | CP_UNAMBIGC
    | CP_UNAMBIGP
    | CP_INSERTP
    | CP_LISTLINES
    | CP_QUOTES
    | CP_IGNORED;

fn real_attrs(k: usize) -> u32 {
    match k as u32 {
        CPN_CURRENT => INTEGER,
        CPN_QIPREFIX | CPN_QISUFFIX => READONLY,
        _ => 0,
    }
}

fn real_slot(c: &mut CompState, k: u32) -> Option<&mut Option<String>> {
    Some(match k {
        CPN_PREFIX => &mut c.compprefix,
        CPN_SUFFIX => &mut c.compsuffix,
        CPN_IPREFIX => &mut c.compiprefix,
        CPN_ISUFFIX => &mut c.compisuffix,
        CPN_QIPREFIX => &mut c.compqiprefix,
        CPN_QISUFFIX => &mut c.compqisuffix,
        _ => return None,
    })
}

fn real_value(z: &mut Zle, k: usize) -> Value {
    let c = &mut z.comp;
    match k as u32 {
        CPN_WORDS => Value::Array(c.compwords.clone()),
        CPN_REDIRS => Value::Array(c.compredirs.clone()),
        CPN_CURRENT => Value::Scalar(c.compcurrent.to_string()),
        k => Value::Scalar(real_slot(c, k).and_then(|s| s.clone()).unwrap_or_default()),
    }
}

fn key_slot(c: &mut CompState, k: u32) -> Option<&mut Option<String>> {
    Some(match k {
        CPN_CONTEXT => &mut c.compcontext,
        CPN_PARAMETER => &mut c.compparameter,
        CPN_REDIRECT => &mut c.compredirect,
        CPN_QUOTE => &mut c.compquote,
        CPN_QUOTING => &mut c.compquoting,
        CPN_RESTORE => &mut c.comprestore,
        CPN_INSERT => &mut c.compinsert,
        CPN_EXACT => &mut c.compexact,
        CPN_EXACTSTR => &mut c.compexactstr,
        CPN_PATMATCH => &mut c.comppatmatch,
        CPN_PATINSERT => &mut c.comppatinsert,
        CPN_LASTPROMPT => &mut c.complastprompt,
        CPN_TOEND => &mut c.comptoend,
        CPN_OLDLIST => &mut c.compoldlist,
        CPN_OLDINS => &mut c.compoldins,
        CPN_VARED => &mut c.compvared,
        _ => return None,
    })
}

/// $compstate[all_quotes]: the quoting of every level.
fn get_compqstack(z: &Zle) -> String {
    st(&z.comp.compqstack)
        .chars()
        .filter_map(|q| comp_quoting_string(q as i32).chars().next())
        .collect()
}

// The value of compstate key `k`; some are worked out when read.
fn key_value(z: &mut Zle, k: u32) -> String {
    use super::result::{list_lines, unambig_data};
    match k {
        CPN_NMATCHES => {
            let fi = permmatches(z, false);
            (if fi { 0 } else { z.comp.nmatches }).to_string()
        }
        CPN_LIST => st(&z.comp.complist).to_string(),
        CPN_UNAMBIG => unambig_data(z).0,
        CPN_UNAMBIGC => unambig_data(z).1.to_string(),
        CPN_UNAMBIGP => unambig_data(z).2,
        CPN_INSERTP => unambig_data(z).3,
        CPN_LISTMAX => z.comp.complistmax.to_string(),
        CPN_LISTLINES => list_lines(z).to_string(),
        CPN_QUOTES => get_compqstack(z),
        CPN_IGNORED => z.comp.compignored.to_string(),
        k => key_slot(&mut z.comp, k)
            .and_then(|s| s.clone())
            .unwrap_or_default(),
    }
}

fn int_of(s: &str) -> i64 {
    s.trim()
        .parse()
        .ok()
        .or_else(|| crate::arith::eval(s).ok())
        .unwrap_or(0)
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Scalar(s) => s.clone(),
        Value::Array(a) => a.join(" "),
        Value::Assoc(m) => m.values().cloned().collect::<Vec<_>>().join(" "),
    }
}

// An assignment to compstate key `k`.
fn set_key(z: &mut Zle, k: u32, v: &str, was_set: bool) {
    match k {
        CPN_LIST => {
            if !was_set || st(&z.comp.complist) != v {
                super::result::comp_list(z, Some(v.to_string()));
            }
        }
        CPN_LISTMAX => z.comp.complistmax = int_of(v),
        _ => {
            if k == CPN_PATMATCH && (!was_set || st(&z.comp.comppatmatch) != v) {
                note_patmatch_set();
            }
            if let Some(slot) = key_slot(&mut z.comp, k) {
                *slot = Some(v.to_string());
            }
        }
    }
}

// The keys that are set, with their values.
fn key_map(z: &mut Zle) -> Vec<(u32, Option<String>)> {
    let keys = z.comp.comp_set_keys;
    (0..CP_KEYPARAMS)
        .map(|k| {
            let v = (keys & (1 << k) != 0).then(|| key_value(z, k));
            (k, v)
        })
        .collect()
}

/// Makes the special completion parameters as locals of the scope just
/// pushed, set or unset as $compstate's bits say.
pub fn make_comp_params(z: &mut Zle) {
    let reals: Vec<Value> = (0..REAL_NAMES.len()).map(|k| real_value(z, k)).collect();
    let keys: BTreeMap<String, String> = key_map(z)
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (KEY_NAMES[k as usize].to_string(), v)))
        .collect();
    let rbits = z.comp.comp_set_reals;
    crate::shell::with_shell(|sh| {
        let level = sh.local_scopes.len();
        for (k, v) in reals.into_iter().enumerate() {
            let name = REAL_NAMES[k];
            sh.make_local(name);
            let mut p = Param::with(v);
            p.level = level;
            p.attrs |= real_attrs(k);
            if rbits & (1 << k) == 0 {
                p.attrs |= UNSET;
            }
            sh.params.insert(name.to_string(), p);
        }
        sh.make_local("compstate");
        let mut p = Param::with(Value::Assoc(keys));
        p.level = level;
        if rbits & CP_COMPSTATE == 0 {
            p.attrs |= UNSET;
        }
        sh.params.insert("compstate".to_string(), p);
    });
}

/// Reads the special parameters back after shell code may have assigned
/// or unset them.
pub fn pull_comp_params(z: &mut Zle) {
    let (reals, cs) = crate::shell::with_shell(|sh| {
        let reals: Vec<Option<Value>> = REAL_NAMES[..REAL_WRITABLE]
            .iter()
            .map(|n| {
                sh.params
                    .get(*n)
                    .filter(|p| p.attrs & UNSET == 0)
                    .map(|p| p.value.clone())
            })
            .collect();
        let cs = sh.params.get("compstate").map(|p| p.value.clone());
        (reals, cs)
    });
    for (k, v) in reals.into_iter().enumerate() {
        let bit = 1u32 << k;
        let k = k as u32;
        match v {
            Some(v) => {
                match k {
                    CPN_WORDS | CPN_REDIRS => {
                        let a = match v {
                            Value::Array(a) => a,
                            Value::Scalar(s) => vec![s],
                            Value::Assoc(m) => m.into_values().collect(),
                        };
                        if k == CPN_WORDS {
                            z.comp.compwords = a;
                        } else {
                            z.comp.compredirs = a;
                        }
                    }
                    CPN_CURRENT => z.comp.compcurrent = int_of(&text_of(&v)),
                    _ => {
                        if let Some(slot) = real_slot(&mut z.comp, k) {
                            *slot = Some(text_of(&v));
                        }
                    }
                }
                z.comp.comp_set_reals |= bit;
            }
            None => {
                if z.comp.comp_set_reals & bit != 0 {
                    // Unsetting empties it.
                    match k {
                        CPN_WORDS => z.comp.compwords.clear(),
                        CPN_REDIRS => z.comp.compredirs.clear(),
                        CPN_CURRENT => {}
                        _ => {
                            if let Some(slot) = real_slot(&mut z.comp, k) {
                                *slot = Some(String::new());
                            }
                        }
                    }
                    z.comp.comp_set_reals &= !bit;
                }
            }
        }
    }
    let Some(Value::Assoc(m)) = cs else {
        return;
    };
    for k in 0..CP_KEYPARAMS {
        let bit = 1u32 << k;
        if KEY_READONLY & bit != 0 {
            continue;
        }
        let was_set = z.comp.comp_set_keys & bit != 0;
        match m.get(KEY_NAMES[k as usize]) {
            Some(v) => {
                set_key(z, k, v, was_set);
                z.comp.comp_set_keys |= bit;
            }
            None => {
                if was_set {
                    if let Some(slot) = key_slot(&mut z.comp, k) {
                        *slot = Some(String::new());
                    }
                    z.comp.comp_set_keys &= !bit;
                }
            }
        }
    }
}

/// Writes the completion state into the special parameters.
pub fn push_comp_params(z: &mut Zle) {
    let reals: Vec<Value> = (0..REAL_NAMES.len()).map(|k| real_value(z, k)).collect();
    let keys = key_map(z);
    let rbits = z.comp.comp_set_reals;
    crate::shell::with_shell(|sh| {
        let level = sh.local_scopes.len();
        for (k, v) in reals.into_iter().enumerate() {
            let name = REAL_NAMES[k];
            let set = rbits & (1 << k) != 0;
            match sh.params.get_mut(name) {
                Some(p) => {
                    p.value = v;
                    if set {
                        p.attrs &= !UNSET;
                    } else {
                        p.attrs |= UNSET;
                    }
                }
                None if set => {
                    let mut p = Param::with(v);
                    p.level = level;
                    p.attrs |= real_attrs(k);
                    sh.params.insert(name.to_string(), p);
                }
                None => {}
            }
        }
        if let Some(Param {
            value: Value::Assoc(m),
            ..
        }) = sh.params.get_mut("compstate")
        {
            for (k, v) in keys {
                let name = KEY_NAMES[k as usize];
                match v {
                    Some(v) => {
                        m.insert(name.to_string(), v);
                    }
                    None => {
                        m.remove(name);
                    }
                }
            }
        }
    });
}

/// Marks special parameters set (`rset`, `kset`) or unset (`runset`,
/// `kunset`), by their CP_* bits.
pub fn comp_setunset(z: &mut Zle, rset: u32, runset: u32, kset: u32, kunset: u32) {
    let c = &mut z.comp;
    c.comp_set_reals = (c.comp_set_reals | rset) & !runset;
    c.comp_set_keys = (c.comp_set_keys | kset) & !kunset;
}

// What comp_wrapper saves around a function.
struct Saved {
    orest: Option<String>,
    ocur: i64,
    strs: [Option<String>; 11],
    owords: Vec<String>,
    oredirs: Vec<String>,
    runset: u32,
    kunset: u32,
}

fn saved_strs(c: &mut CompState) -> [&mut Option<String>; 11] {
    [
        &mut c.compprefix,
        &mut c.compsuffix,
        &mut c.compiprefix,
        &mut c.compisuffix,
        &mut c.compqiprefix,
        &mut c.compqisuffix,
        &mut c.compquote,
        &mut c.compquoting,
        &mut c.compqstack,
        &mut c.autoq,
        &mut c.comprestore,
    ]
}

/// Runs a shell function called during completion: with
/// $compstate[restore] left at `auto`, the special parameters it changed
/// are put back when it returns. The function runs in `run`.
pub fn comp_wrapper<R>(run: impl FnOnce() -> R) -> R {
    let saved = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return None;
        }
        pull_comp_params(z);
        let m = CP_WORDS
            | CP_REDIRS
            | CP_CURRENT
            | CP_PREFIX
            | CP_SUFFIX
            | CP_IPREFIX
            | CP_ISUFFIX
            | CP_QIPREFIX
            | CP_QISUFFIX;
        let runset = m & !z.comp.comp_set_reals;
        let kunset = if z.comp.comp_set_keys & CP_RESTORE == 0 {
            CP_RESTORE
        } else {
            0
        };
        let orest = z.comp.comprestore.replace("auto".to_string());
        let c = &mut z.comp;
        let strs = saved_strs(c).map(|s| s.clone());
        let sv = Saved {
            orest,
            ocur: c.compcurrent,
            strs,
            owords: c.compwords.clone(),
            oredirs: c.compredirs.clone(),
            runset,
            kunset,
        };
        push_comp_params(z);
        Some(sv)
    })
    .flatten();
    let r = run();
    if let Some(sv) = saved {
        crate::zle::with_active(|z| {
            pull_comp_params(z);
            let m = CP_WORDS
                | CP_REDIRS
                | CP_CURRENT
                | CP_PREFIX
                | CP_SUFFIX
                | CP_IPREFIX
                | CP_ISUFFIX
                | CP_QIPREFIX
                | CP_QISUFFIX;
            if st(&z.comp.comprestore) == "auto" {
                let c = &mut z.comp;
                c.compcurrent = sv.ocur;
                // All but comprestore, which goes back below.
                for (slot, v) in saved_strs(c).into_iter().zip(sv.strs).take(10) {
                    *slot = v;
                }
                c.compwords = sv.owords;
                c.compredirs = sv.oredirs;
                comp_setunset(
                    z,
                    CP_COMPSTATE | (!sv.runset & m),
                    sv.runset & CP_ALLREALS,
                    !sv.kunset & CP_RESTORE,
                    sv.kunset & CP_ALLKEYS,
                );
            } else {
                comp_setunset(
                    z,
                    CP_COMPSTATE,
                    0,
                    !sv.kunset & CP_RESTORE,
                    sv.kunset & CP_RESTORE,
                );
            }
            z.comp.comprestore = sv.orest;
            push_comp_params(z);
        });
    }
    r
}

// ---- conditions ---------------------------------------------------------

fn comp_check(z: &Zle) -> bool {
    if z.comp.incompfunc != 1 {
        crate::shell::shell_err("condition can only be used in completion function");
        return false;
    }
    true
}

fn cond_str(a: &str) -> String {
    crate::expand::expand_pattern(a).unwrap_or_else(|_| a.to_string())
}

fn cond_val(a: &str) -> i32 {
    let s = crate::expand::expand_string(a).unwrap_or_else(|_| a.to_string());
    crate::arith::eval(&s).unwrap_or(0) as i32
}

/// The -prefix and -suffix conditions (`id` CVT_PREPAT or CVT_SUFPAT);
/// `a` holds their operands as written.
fn cond_psfix(z: &mut Zle, a: &[String], id: i32) -> bool {
    if !comp_check(z) {
        return false;
    }
    if a.len() > 1 {
        let n = cond_val(&a[0]);
        do_comp_vars(z, id, n, &cond_str(&a[1]), 0, None, false)
    } else {
        do_comp_vars(z, id, -1, &cond_str(&a[0]), 0, None, false)
    }
}

/// The -after and -between conditions.
fn cond_range(z: &mut Zle, a: &[String], between: bool) -> bool {
    let sb = if between {
        a.get(1).map(String::as_str)
    } else {
        None
    };
    do_comp_vars(z, CVT_RANGEPAT, 0, &a[0], 0, sb, false)
}

/// The completion conditions `[[ -prefix ... ]]` and so on: `name`
/// without its dash, `args` the operands unexpanded. None when `name`
/// is not one of them or the operand count is wrong.
pub fn comp_cond(name: &str, args: &[String]) -> Option<bool> {
    let (id, min, max) = match name {
        "after" => (-1, 1, 1),
        "between" => (-2, 2, 2),
        "prefix" => (CVT_PREPAT, 1, 2),
        "suffix" => (CVT_SUFPAT, 1, 2),
        _ => return None,
    };
    if args.len() < min || args.len() > max {
        return None;
    }
    let r = crate::zle::with_active(|z| match id {
        -1 => cond_range(z, args, false),
        -2 => cond_range(z, args, true),
        _ => cond_psfix(z, args, id),
    });
    Some(r.unwrap_or_else(|| {
        if id >= 0 {
            crate::shell::shell_err("condition can only be used in completion function");
        }
        false
    }))
}
