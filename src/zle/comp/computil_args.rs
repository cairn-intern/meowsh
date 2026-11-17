//! The compdescribe and comparguments builtins, after the first half of
//! zsh's Src/Zle/computil.c: the helpers behind `_describe` and
//! `_arguments`.
//!
//! zsh's linked lists of strings, options and argument definitions are
//! vectors or `Rc` chains here; the `active` flags that a command-line
//! parse toggles on cached definitions are `Cell`s.

use super::complete::{ignore_prefix, pull_comp_params, push_comp_params, restrict_range};
use super::core::{atoi, get_user_var, pat_from_tokens, set_list_array, tokenize, zstrcmp};
use super::result::CM_SPACE;
use super::tricky::{nicewidth, remnulargs, untokenize, zterm_columns};
use crate::pattern::Pattern;
use crate::shell::builtin_err;
use crate::zle::Zle;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn string(s: &[char]) -> String {
    s.iter().collect()
}

// The character at `i`, or NUL past the end, as C reads a string.
fn at(s: &[char], i: usize) -> char {
    s.get(i).copied().unwrap_or('\0')
}

fn inblank(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\n'
}

fn iblank(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn plusminus(c: char) -> bool {
    c == '-' || c == '+'
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---- compdescribe -------------------------------------------------------

const CRT_SIMPLE: i32 = 0;
const CRT_DESC: i32 = 1;
const CRT_SPEC: i32 = 2;
const CRT_DUMMY: i32 = 3;
const CRT_EXPL: i32 = 4;

/// One string of a set (zsh's struct cdstr). `other` and `run` link
/// strings by their index in `CdState::strs`.
struct Cdstr {
    str_: String,
    desc: Option<String>,
    match_: String,
    width: i32,
    other: Option<usize>,
    kind: i32,
    set: usize,
    run: Option<usize>,
}

/// A set of matches (zsh's struct cdset): its strings are
/// `strs[start..end]`.
struct Cdset {
    opts: Vec<String>,
    start: usize,
    end: usize,
    count: i32,
    desc: i32,
}

struct Cdrun {
    type_: i32,
    strs: Option<usize>,
    count: i32,
}

#[derive(Default)]
struct CdState {
    parsed: bool,
    showd: bool,
    sep: String,
    swidth: i32,
    maxmlen: i32,
    sets: Vec<Cdset>,
    strs: Vec<Cdstr>,
    premaxw: i32,
    maxg: i32,
    maxglen: i32,
    groups: i32,
    descs: i32,
    gprew: i32,
    runs: VecDeque<Cdrun>,
}

thread_local! {
    static CD_STATE: RefCell<CdState> = RefCell::new(CdState::default());
}

/// Finds matches with the same description and groups them.
fn cd_group(cd: &mut CdState, maxg: i32) {
    cd.groups = 0;
    cd.descs = 0;
    cd.maxglen = 0;
    cd.maxg = 0;

    for s in cd.strs.iter_mut() {
        s.kind = 0;
        s.other = None;
    }
    // Sets keep their strings contiguous and in order, so "the rest of
    // this set and all later sets" is every later index.
    for i in 0..cd.strs.len() {
        if cd.strs[i].desc.is_none() || cd.strs[i].kind != 0 {
            continue;
        }
        let mut num = 1;
        let mut width = cd.strs[i].width + cd.swidth;
        if width > cd.maxglen {
            cd.maxglen = width;
        }
        let mut tail = i;
        for j in i + 1..cd.strs.len() {
            if cd.strs[j].desc.is_some() && cd.strs[j].desc == cd.strs[i].desc {
                width += CM_SPACE + cd.strs[j].width;
                if width > cd.maxmlen || num == maxg {
                    break;
                }
                if width > cd.maxglen {
                    cd.maxglen = width;
                }
                cd.strs[i].kind = 1;
                cd.strs[j].kind = 2;
                num += 1;
                cd.strs[tail].other = Some(j);
                tail = j;
            }
        }
        cd.strs[tail].other = None;

        if num > 1 {
            cd.groups += 1;
        } else {
            cd.descs += 1;
        }
        if num > cd.maxg {
            cd.maxg = num;
        }
    }
}

/// Counts the strings with descriptions and finds the widest string.
fn cd_calc(cd: &mut CdState) {
    for set in cd.sets.iter_mut() {
        set.count = 0;
        set.desc = 0;
        for s in &cd.strs[set.start..set.end] {
            set.count += 1;
            let l = nicewidth(&s.str_);
            if l > cd.premaxw {
                cd.premaxw = l;
            }
            if s.desc.is_some() {
                set.desc += 1;
            }
        }
    }
}

fn cd_groups_want_sorting(cd: &CdState) -> bool {
    for set in &cd.sets {
        for o in &set.opts {
            if o.starts_with("-V") {
                return false;
            } else if o.starts_with("-J") {
                return true;
            }
        }
    }
    true
}

fn cd_desc_eq(cd: &CdState, a: usize, b: usize) -> bool {
    cd.strs[a].desc == cd.strs[b].desc
}

// Links `ids` through their `run` fields; the first one, if any.
fn cd_link_run(cd: &mut CdState, ids: &[usize]) -> Option<usize> {
    for w in ids.windows(2) {
        cd.strs[w[0]].run = Some(w[1]);
    }
    if let Some(&l) = ids.last() {
        cd.strs[l].run = None;
    }
    ids.first().copied()
}

/// Builds the runs reported to shell code; true when the groups are too
/// wide and must be made again with fewer members.
fn cd_prep(cd: &mut CdState) -> bool {
    cd.runs.clear();

    if cd.groups != 0 {
        let maxg = cd.maxg.max(1) as usize;
        let mut wids = vec![0i32; maxg];
        let mut grps: Vec<usize> = Vec::with_capacity((cd.groups + cd.descs).max(0) as usize);

        for si in 0..cd.strs.len() {
            if cd.strs[si].kind != 1 {
                if cd.strs[si].kind == 0 && cd.strs[si].desc.is_some() {
                    if cd.strs[si].width > wids[0] {
                        wids[0] = cd.strs[si].width;
                    }
                    cd.strs[si].other = None;
                    grps.push(si);
                }
                continue;
            }
            let mut gs = si;
            cd.strs[si].kind = 2;
            let mut gp = cd.strs[si].other;
            cd.strs[si].other = None;
            // Sort the members by decreasing width.
            while let Some(g) = gp {
                let gn = cd.strs[g].other;
                cd.strs[g].other = None;
                let mut prev = None;
                let mut c = Some(gs);
                while let Some(x) = c {
                    if cd.strs[x].width > cd.strs[g].width {
                        prev = Some(x);
                        c = cd.strs[x].other;
                    } else {
                        break;
                    }
                }
                cd.strs[g].other = c;
                match prev {
                    None => gs = g,
                    Some(p) => cd.strs[p].other = Some(g),
                }
                gp = gn;
            }
            let mut c = Some(gs);
            let mut i = 0;
            while let Some(x) = c {
                if let Some(w) = wids.get_mut(i) {
                    if cd.strs[x].width > *w {
                        *w = cd.strs[x].width;
                    }
                }
                c = cd.strs[x].other;
                i += 1;
            }
            grps.push(gs);
        }
        let preplines = grps.len();

        cd.gprew = 0;
        for w in wids.iter().take(cd.maxg.max(0) as usize) {
            cd.gprew += w + CM_SPACE;
        }
        if cd.gprew > cd.maxmlen && cd.maxglen > 1 {
            return true;
        }
        if cd_groups_want_sorting(cd) {
            let strs = &cd.strs;
            grps.sort_by(|&a, &b| zstrcmp(&strs[a].str_, &strs[b].str_, false, 0).cmp(&0));
        }
        // Bring lines with the same description together.
        let mut k = 0usize;
        let mut i = preplines;
        while i > 1 {
            let mut k2 = k + 1;
            if cd_desc_eq(cd, grps[k], grps[k2]) {
                i -= 1;
                k += 1;
                continue;
            }
            k2 += 1;
            let mut j = i as isize - 2;
            while j > 0 {
                if cd_desc_eq(cd, grps[k], grps[k2]) {
                    let tmp = grps[k2];
                    grps.copy_within(k + 1..k2, k + 2);
                    k += 1;
                    grps[k] = tmp;
                    i -= 1;
                }
                j -= 1;
                k2 += 1;
            }
            i -= 1;
            k += 1;
        }
        let expl_strs = grps.first().copied();
        let expl = Cdrun {
            type_: CRT_EXPL,
            strs: expl_strs,
            count: preplines as i32,
        };
        let mut cols: Vec<Option<usize>> = Vec::with_capacity(preplines);
        let mut prev: Option<usize> = None;
        for &s in &grps {
            cols.push(cd.strs[s].other);
            if let Some(p) = prev {
                cd.strs[p].run = Some(s);
            }
            prev = Some(s);
            cd.runs.push_back(Cdrun {
                type_: CRT_SPEC,
                strs: Some(s),
                count: 1,
            });
        }
        if let Some(p) = prev {
            cd.strs[p].run = None;
        }
        for _ in 1..cd.maxg.max(1) {
            let mut d = 0;
            for c in cols.iter_mut() {
                if let Some(s) = *c {
                    if d != 0 {
                        cd.runs.push_back(Cdrun {
                            type_: CRT_DUMMY,
                            strs: expl_strs,
                            count: d,
                        });
                        d = 0;
                    }
                    cd.runs.push_back(Cdrun {
                        type_: CRT_SPEC,
                        strs: Some(s),
                        count: 1,
                    });
                    cd.strs[s].run = None;
                    *c = cd.strs[s].other;
                } else {
                    d += 1;
                }
            }
            if d != 0 {
                cd.runs.push_back(Cdrun {
                    type_: CRT_DUMMY,
                    strs: expl_strs,
                    count: d,
                });
            }
        }
        cd.runs.push_back(expl);

        for k in 0..cd.sets.len() {
            let ids: Vec<usize> = (cd.sets[k].start..cd.sets[k].end)
                .filter(|&i| cd.strs[i].kind == 0 && cd.strs[i].desc.is_none())
                .collect();
            if !ids.is_empty() {
                let head = cd_link_run(cd, &ids);
                cd.runs.push_back(Cdrun {
                    type_: CRT_SIMPLE,
                    strs: head,
                    count: ids.len() as i32,
                });
            }
        }
    } else if cd.showd {
        for k in 0..cd.sets.len() {
            let (start, end) = (cd.sets[k].start, cd.sets[k].end);
            if cd.sets[k].desc != 0 {
                let ids: Vec<usize> = (start..end)
                    .filter(|&i| cd.strs[i].desc.is_some())
                    .collect();
                let head = cd_link_run(cd, &ids);
                cd.runs.push_back(Cdrun {
                    type_: CRT_DESC,
                    strs: head,
                    count: cd.sets[k].desc,
                });
            }
            if cd.sets[k].desc != cd.sets[k].count {
                let ids: Vec<usize> = (start..end)
                    .filter(|&i| cd.strs[i].desc.is_none())
                    .collect();
                let head = cd_link_run(cd, &ids);
                cd.runs.push_back(Cdrun {
                    type_: CRT_SIMPLE,
                    strs: head,
                    count: cd.sets[k].count - cd.sets[k].desc,
                });
            }
        }
    } else {
        for k in 0..cd.sets.len() {
            if cd.sets[k].count != 0 {
                let ids: Vec<usize> = (cd.sets[k].start..cd.sets[k].end).collect();
                let head = cd_link_run(cd, &ids);
                cd.runs.push_back(Cdrun {
                    type_: CRT_SIMPLE,
                    strs: head,
                    count: cd.sets[k].count,
                });
            }
        }
    }
    false
}

/// Stores the strings and matches and works out the runs.
fn cd_init(
    nam: &str,
    hide: &str,
    mlen: &str,
    sep: &str,
    opts: Option<&[String]>,
    args: &[String],
    disp: bool,
) -> i32 {
    CD_STATE.with(|c| c.borrow_mut().parsed = false);
    let mut cd = CdState {
        sep: sep.to_string(),
        swidth: nicewidth(sep),
        showd: disp,
        maxmlen: atoi(mlen),
        ..Default::default()
    };
    let itmp = zterm_columns() - cd.swidth - 4;
    if cd.maxmlen > itmp {
        cd.maxmlen = itmp;
    }
    if cd.maxmlen < 4 {
        cd.maxmlen = 4;
    }
    let mut k = 0;
    let mut grp = false;
    if args.first().is_some_and(|a| a == "-g") {
        k += 1;
        grp = true;
    }
    while k < args.len() {
        let Some(ap) = get_user_var(Some(&args[k])) else {
            builtin_err(nam, &format!("invalid argument: {}", args[k]));
            return 1;
        };
        let setno = cd.sets.len();
        let start = cd.strs.len();
        for e in ap {
            let e = chars(&e);
            // The first unescaped colon.
            let mut t = 0;
            while t < e.len() && e[t] != ':' {
                if e[t] == '\\' && t + 1 < e.len() {
                    t += 1;
                }
                t += 1;
            }
            let desc = if t < e.len() {
                Some(string(&super::core::rembslash(&e[t + 1..])))
            } else {
                None
            };
            let s = string(&super::core::rembslash(&e[..t]));
            cd.strs.push(Cdstr {
                width: nicewidth(&s),
                match_: s.clone(),
                str_: s,
                desc,
                other: None,
                kind: 0,
                set: setno,
                run: None,
            });
        }
        k += 1;
        if k < args.len() && !args[k].starts_with('-') {
            let Some(ap) = get_user_var(Some(&args[k])) else {
                builtin_err(nam, &format!("invalid argument: {}", args[k]));
                return 1;
            };
            for (s, m) in cd.strs[start..].iter_mut().zip(ap) {
                s.match_ = m;
            }
            k += 1;
        }
        if !hide.is_empty() {
            let long = hide.chars().count() > 1;
            for s in cd.strs[start..].iter_mut() {
                let n = if long && s.str_.starts_with("--") {
                    2
                } else if s.str_.starts_with(['-', '+']) {
                    1
                } else {
                    0
                };
                s.str_.drain(..n);
            }
        }
        let ap0 = k;
        while k < args.len() && args[k] != "--" {
            k += 1;
        }
        let mut sopts: Vec<String> = args[ap0..k].to_vec();
        if let Some(o) = opts {
            sopts.extend(o.iter().cloned());
        }
        if k < args.len() {
            k += 1;
        }
        cd.sets.push(Cdset {
            opts: sopts,
            start,
            end: cd.strs.len(),
            count: 0,
            desc: 0,
        });
    }
    if disp && grp {
        let mut mg = zterm_columns();
        loop {
            cd_group(&mut cd, mg);
            mg = cd.maxg - 1;
            cd_calc(&mut cd);
            if !cd_prep(&mut cd) {
                break;
            }
        }
    } else {
        cd_calc(&mut cd);
        cd_prep(&mut cd);
    }
    cd.parsed = true;
    CD_STATE.with(|c| *c.borrow_mut() = cd);
    0
}

// The strings of a run, following the `run` links.
fn cd_run_strs(cd: &CdState, head: Option<usize>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut c = head;
    while let Some(s) = c {
        out.push(s);
        c = cd.strs[s].run;
    }
    out
}

// The compadd options of the set of a run's first string.
fn cd_run_opts(cd: &CdState, run: &Cdrun) -> Vec<String> {
    run.strs
        .map(|s| cd.sets[cd.strs[s].set].opts.clone())
        .unwrap_or_default()
}

// Appends as much of `d` as fits in `remw` columns; returns what is left.
fn cd_fit(buf: &mut String, d: &str, mut remw: i32) -> i32 {
    let w = nicewidth(d);
    if w <= remw {
        buf.push_str(d);
        return remw - w;
    }
    for ch in d.chars() {
        if remw <= 0 {
            break;
        }
        let w = nicewidth(&ch.to_string());
        if w > remw {
            break;
        }
        buf.push(ch);
        remw -= w;
    }
    remw
}

/// Sets the four parameters to the next run; 1 when there is none.
fn cd_get(params: &[String]) -> i32 {
    let got = CD_STATE.with(|c| {
        let mut cd = c.borrow_mut();
        let run = cd.runs.pop_front()?;
        let cols = zterm_columns();
        let mut csl = "";
        let ids = cd_run_strs(&cd, run.strs);
        let mut mats: Vec<String> = Vec::new();
        let mut dpys: Vec<String> = Vec::new();
        let mut opts: Vec<String>;
        match run.type_ {
            CRT_SIMPLE => {
                for &s in &ids {
                    mats.push(cd.strs[s].match_.clone());
                    dpys.push(cd.strs[s].str_.clone());
                }
                opts = cd_run_opts(&cd, &run);
                if cd.groups != 0 {
                    // Matches without descriptions go in a group of their
                    // own, which must not get the explanation. zsh's loop
                    // for this stops copying at the first -X option.
                    if let Some(x) = opts.iter().position(|o| o.starts_with("-X")) {
                        opts.truncate(x);
                    }
                }
            }
            CRT_DESC => {
                let slen = cd.sep.chars().count() as i32;
                for &s in &ids {
                    let st = &cd.strs[s];
                    mats.push(st.match_.clone());
                    let mut buf = st.str_.clone();
                    let pad = cd.premaxw - st.width + CM_SPACE;
                    buf.push_str(&" ".repeat(pad.max(0) as usize));
                    let mut remw = cols - cd.premaxw - cd.swidth - 3;
                    while remw < 0 && cols != 0 {
                        remw += cols;
                    }
                    if slen < remw {
                        buf.push_str(&cd.sep);
                        remw -= slen;
                        cd_fit(&mut buf, st.desc.as_deref().unwrap_or(""), remw);
                    }
                    dpys.push(buf);
                }
                opts = vec!["-l".to_string()];
                opts.extend(cd_run_opts(&cd, &run));
            }
            CRT_SPEC => {
                if let Some(s) = run.strs {
                    mats.push(cd.strs[s].match_.clone());
                    dpys.push(cd.strs[s].str_.clone());
                }
                opts = vec![String::new()];
                opts.extend(cd_run_opts(&cd, &run));
                // -2V, reusing the group name of a -J or -V option.
                match opts
                    .iter()
                    .skip(1)
                    .position(|o| o.starts_with("-J") || o.starts_with("-V"))
                {
                    Some(k) => {
                        let v = format!("-2V{}", &opts[k + 1][2..]);
                        opts[k + 1] = v;
                        opts.remove(0);
                    }
                    None => opts[0] = "-2V-default-".to_string(),
                }
                csl = "packed";
            }
            CRT_DUMMY => {
                opts = vec![format!("-E{}", run.count)];
                opts.extend(cd_run_opts(&cd, &run));
                csl = "packed";
            }
            _ => {
                for &s in &ids {
                    let st = &cd.strs[s];
                    if let Some(r) = st.run {
                        if st.desc == cd.strs[r].desc {
                            dpys.push(String::new());
                            continue;
                        }
                    }
                    let mut dbuf = cd.sep.clone();
                    let remw = cols - cd.gprew - cd.swidth - CM_SPACE;
                    let remw = cd_fit(&mut dbuf, st.desc.as_deref().unwrap_or(""), remw);
                    dbuf.push_str(&" ".repeat(remw.max(0) as usize));
                    dpys.push(dbuf);
                }
                opts = vec![format!("-E{}", run.count)];
                opts.extend(cd_run_opts(&cd, &run));
                csl = "packed";
            }
        }
        Some((csl, opts, mats, dpys))
    });
    match got {
        Some((csl, opts, mats, dpys)) => {
            crate::shell::var_set(&params[0], csl, false);
            crate::shell::array_set(&params[1], opts);
            crate::shell::array_set(&params[2], mats);
            crate::shell::array_set(&params[3], dpys);
            0
        }
        None => 1,
    }
}

fn bin_compdescribe(nam: &str, args: &[String]) -> i32 {
    let n = args.len();
    let a0 = chars(&args[0]);
    if a0.len() != 2 {
        builtin_err(nam, &format!("invalid argument: {}", args[0]));
        return 1;
    }
    match a0[1] {
        'i' => {
            if n < 3 {
                builtin_err(nam, "not enough arguments");
                return 1;
            }
            cd_init(nam, &args[1], &args[2], "", None, &args[3..], false)
        }
        'I' => {
            if n < 6 {
                builtin_err(nam, "not enough arguments");
                return 1;
            }
            let Some(opts) = crate::shell::array_get(&args[4]) else {
                builtin_err(nam, &format!("unknown parameter: {}", args[4]));
                return 1;
            };
            cd_init(
                nam,
                &args[1],
                &args[2],
                &args[3],
                Some(opts.as_slice()),
                &args[5..],
                true,
            )
        }
        'g' => {
            if CD_STATE.with(|c| c.borrow().parsed) {
                if n != 5 {
                    builtin_err(
                        nam,
                        if n < 5 {
                            "not enough arguments"
                        } else {
                            "too many arguments"
                        },
                    );
                    return 1;
                }
                cd_get(&args[1..])
            } else {
                builtin_err(nam, "no parsed state");
                1
            }
        }
        _ => {
            builtin_err(nam, &format!("bad option: {}", args[0]));
            1
        }
    }
}

/// The compdescribe builtin; `words[0]` is its name.
pub fn builtin_compdescribe(words: &[String]) -> i32 {
    let name = words.first().map(String::as_str).unwrap_or("compdescribe");
    let args = words.get(1..).unwrap_or(&[]);
    if args.len() < 3 {
        builtin_err(name, "not enough arguments");
        return 1;
    }
    if crate::zle::with_active(|z| z.comp.incompfunc == 1) != Some(true) {
        builtin_err(name, "can only be called from completion function");
        return 1;
    }
    bin_compdescribe(name, args)
}

// ---- comparguments ------------------------------------------------------

const CDF_SEP: i32 = 1;
const CDF_ZSEP: i32 = 2;

const CAO_NEXT: i32 = 1;
const CAO_DIRECT: i32 = 2;
const CAO_ODIRECT: i32 = 3;
const CAO_EQUAL: i32 = 4;
const CAO_OEQUAL: i32 = 5;

const CAA_NORMAL: i32 = 1;
const CAA_OPT: i32 = 2;
const CAA_REST: i32 = 3;
const CAA_RARGS: i32 = 4;
const CAA_RREST: i32 = 5;

const MAX_CACACHE: usize = 8;

/// An argument definition (zsh's struct caarg); `next` chains the
/// arguments of an option or the normal arguments of a set.
struct Caarg {
    next: Option<Rc<Caarg>>,
    descr: String,
    xor: Option<Vec<String>>,
    action: String,
    type_: i32,
    /// The end pattern of `*PATTERN:...`, as crate::pattern reads it.
    end: Option<String>,
    opt: Option<String>,
    num: i32,
    min: i32,
    direct: bool,
    active: Cell<bool>,
    gsname: Option<String>,
}

/// An option definition (zsh's struct caopt).
struct Caopt {
    name: String,
    descr: Option<String>,
    xor: Option<Vec<String>>,
    type_: i32,
    args: Option<Rc<Caarg>>,
    active: Cell<bool>,
    num: usize,
    gsname: Option<String>,
    not: bool,
}

/// The definitions of one set (zsh's struct cadef); the sets of one
/// `_arguments` call are a `Vec` in zsh's snext order.
struct Cadef {
    opts: Vec<Rc<Caopt>>,
    args: Option<Rc<Caarg>>,
    rest: Option<Rc<Caarg>>,
    single: Option<Vec<Option<Rc<Caopt>>>>,
    match_: String,
    argsactive: Cell<bool>,
    flags: i32,
    /// The -A pattern, as crate::pattern reads it.
    nonarg: Option<String>,
}

type Cadefs = Rc<Vec<Rc<Cadef>>>;

struct CacheEnt {
    defs: Vec<String>,
    lastt: u64,
    all: Cadefs,
}

type Oargs = Rc<RefCell<Vec<Option<Vec<String>>>>>;

/// The state of parsing the line against one set (zsh's struct
/// castate). zsh copies these with memcpy and so shares the `args` and
/// `oargs` lists between copies; the `Rc`s keep that.
#[derive(Clone)]
struct Castate {
    d: Rc<Cadef>,
    def: Option<Rc<Caarg>>,
    ddef: Option<Rc<Caarg>>,
    curopt: Option<Rc<Caopt>>,
    dopt: Option<Rc<Caopt>>,
    opt: i32,
    arg: i32,
    argbeg: i32,
    optbeg: i32,
    nargbeg: i32,
    restbeg: i32,
    curpos: i32,
    argend: i32,
    inopt: i32,
    inarg: i32,
    nth: i32,
    singles: i32,
    oopt: i32,
    actopts: i32,
    args: Rc<RefCell<Vec<String>>>,
    oargs: Oargs,
}

/// zsh's ca_laststate (the first element) and its snext chain, with
/// ca_parsed and ca_doff.
#[derive(Default)]
struct CaGlobal {
    states: Vec<Castate>,
    parsed: bool,
    doff: i32,
}

thread_local! {
    static CADEF_CACHE: RefCell<Vec<CacheEnt>> = const { RefCell::new(Vec::new()) };
    static CA: RefCell<CaGlobal> = RefCell::new(CaGlobal::default());
}

/// Removes backslashes before colons.
fn rembslashcolon(s: &[char]) -> String {
    let mut out = String::new();
    for (i, &c) in s.iter().enumerate() {
        if c != '\\' || at(s, i + 1) != ':' {
            out.push(c);
        }
    }
    out
}

/// Adds backslashes before colons.
fn bslashcolon(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c == ':' {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The index into a cadef's single-letter array of option `opt` with
/// prefix `pre` (`-` or `+`); None outside `!`..`~`.
fn single_index(pre: char, opt: char) -> Option<usize> {
    let o = opt as u32;
    if o <= 0x20 || o > 0x7e {
        return None;
    }
    Some((o - 0x21) as usize + if pre == '-' { 0 } else { 94 })
}

fn is_rest(t: i32) -> bool {
    t == CAA_REST || t == CAA_RARGS || t == CAA_RREST
}

/// Parses an argument definition starting at `p[i]`; returns it and
/// where parsing stopped.
#[allow(clippy::too_many_arguments)]
fn parse_caarg(
    mult: bool,
    type_: i32,
    num: i32,
    opt: i32,
    oname: Option<&str>,
    p: &[char],
    mut i: usize,
    set: Option<String>,
) -> (Caarg, usize) {
    // The description.
    let d = i;
    while at(p, i) != '\0' && at(p, i) != ':' {
        if at(p, i) == '\\' && at(p, i + 1) != '\0' {
            i += 1;
        }
        i += 1;
    }
    let descr = rembslashcolon(&p[d..i.min(p.len())]);
    // The action, if any.
    let action = if at(p, i) != '\0' {
        if mult {
            i += 1;
            let d = i;
            while at(p, i) != '\0' && at(p, i) != ':' {
                if at(p, i) == '\\' && at(p, i + 1) != '\0' {
                    i += 1;
                }
                i += 1;
            }
            rembslashcolon(&p[d..i.min(p.len())])
        } else {
            rembslashcolon(&p[i + 1..])
        }
    } else {
        String::new()
    };
    (
        Caarg {
            next: None,
            descr,
            xor: None,
            action,
            type_,
            end: None,
            opt: oname.map(str::to_string),
            num,
            min: num - opt,
            direct: false,
            active: Cell::new(false),
            gsname: set,
        },
        i,
    )
}

// Chains a list of argument definitions through `next`.
fn link_caargs(v: Vec<Caarg>) -> Option<Rc<Caarg>> {
    let mut next = None;
    for mut a in v.into_iter().rev() {
        a.next = next;
        next = Some(Rc::new(a));
    }
    next
}

/// A set being parsed.
struct SetBuild {
    opts: Vec<Rc<Caopt>>,
    args: Vec<Caarg>,
    rest: Option<Caarg>,
    single: Option<Vec<Option<Rc<Caopt>>>>,
}

impl SetBuild {
    fn new(single: bool) -> SetBuild {
        SetBuild {
            opts: Vec::new(),
            args: Vec::new(),
            rest: None,
            single: if single { Some(vec![None; 188]) } else { None },
        }
    }
}

// zsh's set_cadef_opts, then the finished set.
fn finish_cadef(mut b: SetBuild, match_: &str, nonarg: &Option<String>, flags: i32) -> Rc<Cadef> {
    let mut xnum = 0;
    for a in b.args.iter_mut() {
        if !a.direct {
            a.min = a.num - xnum;
        }
        if a.type_ == CAA_OPT {
            xnum += 1;
        }
    }
    Rc::new(Cadef {
        opts: b.opts,
        args: link_caargs(b.args),
        rest: b.rest.map(Rc::new),
        single: b.single,
        match_: match_.to_string(),
        argsactive: Cell::new(true),
        flags,
        nonarg: nonarg.clone(),
    })
}

// `(name)` without its parentheses (as an exclusion too), or `name`.
fn set_name(p: &str) -> (String, Option<String>) {
    let c = chars(p);
    if c.len() >= 2 && c[0] == '(' && c[c.len() - 1] == ')' {
        let s = string(&c[1..c.len() - 1]);
        (s.clone(), Some(s))
    } else {
        (p.to_string(), None)
    }
}

/// Parses an array of definitions.
fn parse_cadef(nam: &str, args: &[String]) -> Option<Cadefs> {
    // The first string is the auto-description.
    let a0 = chars(&args[0]);
    let (adpre, adsuf) = match (0..a0.len()).find(|&k| a0[k] == '%' && at(&a0, k + 1) == 'd') {
        Some(k) => (Some(string(&a0[..k])), Some(string(&a0[k + 2..]))),
        None => (None, None),
    };

    // The -s, -A, -S and -M options.
    let mut ai = 1;
    let mut single = false;
    let mut flags = 0;
    let mut match_ = "r:|[_-]=* r:|=*".to_string();
    let mut nonarg: Option<String> = None;
    while ai < args.len() {
        let p = chars(&args[ai]);
        if at(&p, 0) != '-' || p.len() < 2 {
            break;
        }
        let mut valid = true;
        for &q in &p[1..] {
            if q == 'M' || q == 'A' {
                break;
            } else if q != 's' && q != 'S' {
                valid = false;
                break;
            }
        }
        if !valid {
            break;
        }
        let mut i = 1;
        while i < p.len() {
            match p[i] {
                's' => single = true,
                'S' => {
                    flags |= if flags & CDF_SEP != 0 {
                        CDF_ZSEP
                    } else {
                        CDF_SEP
                    }
                }
                c @ ('A' | 'M') => {
                    let v = if i + 1 < p.len() {
                        let v = string(&p[i + 1..]);
                        i = p.len() - 1;
                        v
                    } else if ai + 1 < args.len() {
                        ai += 1;
                        args[ai].clone()
                    } else {
                        break;
                    };
                    if c == 'A' {
                        nonarg = Some(v);
                    } else {
                        match_ = v;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if i < p.len() {
            break;
        }
        ai += 1;
    }
    if ai < args.len() && args[ai] == ":" {
        ai += 1;
    }
    if ai >= args.len() {
        return None;
    }
    let nonarg = nonarg.map(|n| pat_from_tokens(&tokenize(&chars(&n))));

    let mut sets: Vec<Rc<Cadef>> = Vec::new();
    let mut ret = SetBuild::new(single);
    let sargs = ai;
    let mut anum = 1;
    let mut doset: Option<String> = None;
    let mut axor: Option<String> = None;
    let mut pendset: Option<usize> = None;
    let mut curset: Option<usize> = None;
    let mut foreignset = false;

    loop {
        if ai >= args.len() {
            if pendset.is_none() {
                break;
            }
            // Start a new set, parsing the common definitions again.
            ai = sargs;
            doset = None;
            let done = std::mem::replace(&mut ret, SetBuild::new(single));
            sets.push(finish_cadef(done, &match_, &nonarg, flags));
            anum = 1;
            foreignset = false;
            curset = pendset;
            pendset = None;
        }
        'def: {
            let orig = &args[ai];
            if orig == "-" && ai + 1 < args.len() {
                foreignset = curset.is_some_and(|c| c != ai);
                if foreignset {
                    if pendset.is_none() && curset.is_some_and(|c| ai > c) {
                        pendset = Some(ai);
                    }
                    ai += 1;
                } else {
                    ai += 1;
                    let (p, x) = set_name(&args[ai]);
                    axor = x;
                    if p.is_empty() {
                        builtin_err(nam, "empty set name");
                        return None;
                    }
                    doset = Some(format!("{}-", p));
                    curset = Some(ai);
                }
                break 'def;
            } else if orig == "+" && ai + 1 < args.len() {
                foreignset = false;
                ai += 1;
                let (p, x) = set_name(&args[ai]);
                axor = x;
                if p.is_empty() {
                    builtin_err(nam, "empty group name");
                    return None;
                }
                doset = Some(format!("{}-", p));
                break 'def;
            } else if foreignset {
                break 'def;
            }
            let mut p = chars(orig);
            let mut i = 0;
            let mut xnum = 0usize;
            let not = at(&p, 0) == '!';
            if not {
                i += 1;
            }
            let mut xor: Option<Vec<String>>;
            if at(&p, i) == '(' {
                // An exclusion list.
                let mut list = Vec::new();
                while at(&p, i) != '\0' && at(&p, i) != ')' {
                    i += 1;
                    while inblank(at(&p, i)) {
                        i += 1;
                    }
                    if at(&p, i) == ')' {
                        break;
                    }
                    let q = i;
                    i += 1;
                    while at(&p, i) != '\0' && at(&p, i) != ')' && !inblank(at(&p, i)) {
                        i += 1;
                    }
                    if at(&p, i) == '\0' {
                        break;
                    }
                    list.push(string(&p[q..i]));
                    xnum += 1;
                }
                if at(&p, i) != ')' {
                    builtin_err(nam, &format!("invalid argument: {}", orig));
                    return None;
                }
                if doset.is_some() {
                    if let Some(a) = &axor {
                        xnum += 1;
                        list.push(a.clone());
                    }
                }
                xor = Some(list);
                i += 1;
            } else if let (Some(_), Some(a)) = (&doset, &axor) {
                xnum = 1;
                xor = Some(vec![a.clone()]);
            } else {
                xor = None;
            }

            let c0 = at(&p, i);
            if plusminus(c0) || (c0 == '*' && plusminus(at(&p, i + 1))) {
                // An option.
                let mut again = 0;
                let mut againp: Vec<char> = Vec::new();
                loop {
                    let multi = at(&p, i) == '*';
                    if multi {
                        i += 1;
                    }
                    let name_start;
                    let (c1, c2) = (at(&p, i + 1), at(&p, i + 2));
                    if ((at(&p, i) == '-' && c1 == '+') || (at(&p, i) == '+' && c1 == '-'))
                        && c2 != '\0'
                        && !matches!(c2, ':' | '[' | '=' | '-' | '+')
                    {
                        // A -+ or +- definition: the whole thing twice.
                        againp = p[i..].to_vec();
                        i += 1;
                        name_start = i;
                        p[i] = if again != 0 { '-' } else { '+' };
                        again += 1;
                    } else {
                        name_start = i;
                        if at(&p, i) == '-' && at(&p, i + 1) == '-' {
                            i += 1;
                        }
                    }
                    if at(&p, i + 1) == '\0' {
                        builtin_err(nam, &format!("invalid argument: {}", orig));
                        return None;
                    }
                    // Skip over the name.
                    i += 1;
                    loop {
                        let c = at(&p, i);
                        let n = at(&p, i + 1);
                        if c == '\0'
                            || c == ':'
                            || c == '['
                            || (plusminus(c) && (n == ':' || n == '['))
                            || (c == '=' && (n == ':' || n == '[' || n == '-'))
                        {
                            break;
                        }
                        if c == '\\' && n != '\0' {
                            i += 1;
                        }
                        i += 1;
                    }
                    let name: Vec<char> = p[name_start..i.min(p.len())].to_vec();
                    let name_s = string(&name);

                    // The character after the name gives the type.
                    let mut c = at(&p, i);
                    let mut otype = CAO_NEXT;
                    if c == '-' {
                        otype = CAO_DIRECT;
                        i += 1;
                        c = at(&p, i);
                    } else if c == '+' {
                        otype = CAO_ODIRECT;
                        i += 1;
                        c = at(&p, i);
                    } else if c == '=' {
                        otype = CAO_OEQUAL;
                        i += 1;
                        c = at(&p, i);
                        if c == '-' {
                            otype = CAO_EQUAL;
                            i += 1;
                            c = at(&p, i);
                        }
                    }
                    // The optional description.
                    let mut descr: Option<String> = None;
                    if c == '[' {
                        i += 1;
                        let ds = i;
                        while at(&p, i) != '\0' && at(&p, i) != ']' {
                            if at(&p, i) == '\\' && at(&p, i + 1) != '\0' {
                                i += 1;
                            }
                            i += 1;
                        }
                        if at(&p, i) == '\0' {
                            builtin_err(nam, &format!("invalid option definition: {}", orig));
                            return None;
                        }
                        // An empty description is no description.
                        if i != ds {
                            descr = Some(string(&p[ds..i]));
                        }
                        i += 1;
                        c = at(&p, i);
                    }
                    if c != '\0' && c != ':' {
                        builtin_err(nam, &format!("invalid option definition: {}", orig));
                        return None;
                    }
                    // Unless `*-...', the option excludes itself.
                    if !multi {
                        let x = xor.get_or_insert_with(Vec::new);
                        let nm = rembslashcolon(&name);
                        if x.len() > xnum {
                            x[xnum] = nm;
                        } else {
                            x.push(nm);
                        }
                    }
                    let mut oargs: Vec<Caarg> = Vec::new();
                    if c == ':' {
                        let mut oanum = 1;
                        let mut onum = 0;
                        while c == ':' {
                            let mut rest = false;
                            let mut end: Option<String> = None;
                            let atype;
                            i += 1;
                            if at(&p, i) == ':' {
                                atype = CAA_OPT;
                                i += 1;
                            } else if at(&p, i) == '*' {
                                i += 1;
                                if at(&p, i) != ':' {
                                    let es = i;
                                    i += 1;
                                    while at(&p, i) != '\0' && at(&p, i) != ':' {
                                        if at(&p, i) == '\\' && at(&p, i + 1) != '\0' {
                                            i += 1;
                                        }
                                        i += 1;
                                    }
                                    let e = &p[es..i.min(p.len())];
                                    end = Some(pat_from_tokens(&tokenize(e)));
                                }
                                if at(&p, i) != ':' {
                                    builtin_err(
                                        nam,
                                        &format!("invalid option definition: {}", orig),
                                    );
                                    return None;
                                }
                                i += 1;
                                if at(&p, i) == ':' {
                                    i += 1;
                                    if at(&p, i) == ':' {
                                        atype = CAA_RREST;
                                        i += 1;
                                    } else {
                                        atype = CAA_RARGS;
                                    }
                                } else {
                                    atype = CAA_REST;
                                }
                                rest = true;
                            } else {
                                atype = CAA_NORMAL;
                            }
                            let (mut a, ni) = parse_caarg(
                                !rest,
                                atype,
                                oanum,
                                onum,
                                Some(name_s.as_str()),
                                &p,
                                i,
                                doset.clone(),
                            );
                            i = ni;
                            oanum += 1;
                            if atype == CAA_OPT {
                                onum += 1;
                            }
                            a.end = end;
                            oargs.push(a);
                            if rest {
                                break;
                            }
                            c = at(&p, i);
                        }
                    }
                    // Store the option.
                    let descr = if descr.is_some() {
                        descr
                    } else if adpre.is_some() && oargs.len() == 1 {
                        if oargs[0].descr.chars().any(|ch| !iblank(ch)) {
                            Some(format!(
                                "{}{}{}",
                                adpre.as_deref().unwrap_or(""),
                                oargs[0].descr,
                                adsuf.as_deref().unwrap_or("")
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    let opt = Rc::new(Caopt {
                        name: rembslashcolon(&name),
                        descr,
                        xor: xor.clone(),
                        type_: otype,
                        args: link_caargs(oargs),
                        active: Cell::new(false),
                        num: ret.opts.len(),
                        gsname: doset.clone(),
                        not,
                    });
                    // Single-letter options also go in the lookup array;
                    // `--' is not an option called `-'.
                    if let Some(sv) = ret.single.as_mut() {
                        if name.len() == 2 && name[1] != '-' {
                            if let Some(sidx) = single_index(name[0], name[1]) {
                                sv[sidx] = Some(opt.clone());
                            }
                        }
                    }
                    ret.opts.push(opt);
                    if again == 1 {
                        p = againp.clone();
                        i = 0;
                        continue;
                    }
                    break;
                }
            } else if c0 == '*' {
                // The rest-argument.
                if not {
                    break 'def;
                }
                i += 1;
                if at(&p, i) != ':' {
                    builtin_err(nam, &format!("invalid rest argument definition: {}", orig));
                    return None;
                }
                if ret.rest.is_some() {
                    builtin_err(nam, &format!("doubled rest argument definition: {}", orig));
                    return None;
                }
                let mut type_ = CAA_REST;
                i += 1;
                if at(&p, i) == ':' {
                    i += 1;
                    if at(&p, i) == ':' {
                        type_ = CAA_RREST;
                        i += 1;
                    } else {
                        type_ = CAA_RARGS;
                    }
                }
                let (mut r, _) = parse_caarg(false, type_, -1, 0, None, &p, i, doset.clone());
                r.xor = xor;
                ret.rest = Some(r);
            } else {
                // A normal argument.
                if not {
                    break 'def;
                }
                let direct = at(&p, i).is_ascii_digit();
                if direct {
                    let mut num = 0;
                    while at(&p, i).is_ascii_digit() {
                        num = num * 10 + (at(&p, i) as i32 - '0' as i32);
                        i += 1;
                    }
                    anum = num + 1;
                } else {
                    anum += 1;
                }
                if at(&p, i) != ':' {
                    builtin_err(nam, &format!("invalid argument: {}", orig));
                    return None;
                }
                let mut type_ = CAA_NORMAL;
                i += 1;
                if at(&p, i) == ':' {
                    type_ = CAA_OPT;
                    i += 1;
                }
                let (mut arg, _) =
                    parse_caarg(false, type_, anum - 1, 0, None, &p, i, doset.clone());
                arg.xor = xor;
                arg.direct = direct;
                // Sort it into the list.
                let pos = ret
                    .args
                    .iter()
                    .position(|a| a.num >= anum - 1)
                    .unwrap_or(ret.args.len());
                if ret.args.get(pos).is_some_and(|a| a.num == anum - 1) {
                    builtin_err(nam, &format!("doubled argument definition: {}", orig));
                    return None;
                }
                ret.args.insert(pos, arg);
            }
        }
        ai += 1;
    }
    sets.push(finish_cadef(ret, &match_, &nonarg, flags));
    Some(Rc::new(sets))
}

/// The definitions for `args`, from the cache or newly parsed.
fn get_cadef(nam: &str, args: &[String]) -> Option<Cadefs> {
    let hit = CADEF_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let e = c.iter_mut().find(|e| e.defs == args)?;
        e.lastt = now();
        Some(e.all.clone())
    });
    if hit.is_some() {
        return hit;
    }
    let new = parse_cadef(nam, args)?;
    CADEF_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let ent = CacheEnt {
            defs: args.to_vec(),
            lastt: now(),
            all: new.clone(),
        };
        if c.len() < MAX_CACACHE {
            c.push(ent);
        } else {
            let min = c
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.lastt)
                .map(|(k, _)| k)
                .unwrap_or(0);
            c[min] = ent;
        }
    });
    Some(new)
}

fn name_is(name: &str, line: &[char]) -> bool {
    name.chars().eq(line.iter().copied())
}

/// The option used in word `line`, if any, and where the option ends in
/// it. `full`: only a whole word counts.
fn ca_get_opt(
    d: &Cadef,
    line: &[char],
    ign_active: bool,
    full: bool,
) -> Option<(Rc<Caopt>, usize)> {
    for p in &d.opts {
        if (ign_active || p.active.get()) && name_is(&p.name, line) {
            return Some((p.clone(), line.len()));
        }
    }
    if !full {
        // The word may only begin with an option.
        for p in &d.opts {
            if !(ign_active || p.active.get()) {
                continue;
            }
            let name = chars(&p.name);
            let hit = if p.args.is_none() || p.type_ == CAO_NEXT {
                name.as_slice() == line
            } else {
                line.starts_with(&name)
            };
            if hit {
                let mut l = name.len();
                let eq = p.type_ == CAO_OEQUAL || p.type_ == CAO_EQUAL;
                if eq && at(line, l) != '\0' && at(line, l) != '=' {
                    continue;
                }
                if eq && at(line, l) == '=' {
                    l += 1;
                }
                return Some((p.clone(), l));
            }
        }
    }
    None
}

/// The same for clumped single-letter options: the option, where it
/// ends, and the options with separate arguments seen before it (zsh
/// sets that list even when it finds no option).
fn ca_get_sopt(
    d: &Cadef,
    line: &[char],
    ign_active: bool,
) -> (Option<Rc<Caopt>>, Option<usize>, Vec<Rc<Caopt>>) {
    let pre = at(line, 0);
    let mut l: Vec<Rc<Caopt>> = Vec::new();
    let mut pp: Option<Rc<Caopt>> = None;
    let mut i = 1;
    while i < line.len() {
        let p: Option<Rc<Caopt>> = match (&d.single, single_index(pre, line[i])) {
            (Some(s), Some(idx)) => s[idx].clone(),
            _ => None,
        };
        let o = match p {
            Some(o) if ign_active || o.active.get() => o,
            _ => return (None, None, l),
        };
        if o.args.is_some() {
            if o.type_ == CAO_NEXT {
                l.push(o.clone());
            } else {
                i += 1;
                if (o.type_ == CAO_OEQUAL || o.type_ == CAO_EQUAL) && at(line, i) == '=' {
                    i += 1;
                }
                return (Some(o), Some(i), l);
            }
        }
        pp = if o.name.starts_with(pre) {
            Some(o)
        } else {
            None
        };
        i += 1;
    }
    let end = pp.as_ref().map(|_| i);
    (pp, end, l)
}

/// Whether `option` is an option of a set other than `curset`.
fn ca_foreign_opt(curset: &Rc<Cadef>, all: &[Rc<Cadef>], option: &[char]) -> bool {
    for d in all {
        if Rc::ptr_eq(d, curset) {
            continue;
        }
        if ca_get_opt(d, option, true, false).is_some() {
            return true;
        }
        if ca_get_sopt(d, option, true).0.is_some() {
            return true;
        }
    }
    false
}

/// The n'th argument definition.
fn ca_get_arg(d: &Cadef, mut n: i32) -> Option<Rc<Caarg>> {
    if !d.argsactive.get() {
        return None;
    }
    let mut a = d.args.clone();
    while let Some(x) = a.clone() {
        if x.active.get() && n >= x.min && n <= x.num {
            break;
        }
        if !x.active.get() {
            n += 1;
        }
        a = x.next.clone();
    }
    if let Some(x) = &a {
        if x.min <= n && x.num >= n && x.active.get() {
            return a;
        }
    }
    d.rest.clone().filter(|r| r.active.get())
}

// Whether set or group name `gs` is `grp` (plus `extra` characters).
fn gs_is(gs: &Option<String>, grp: &str, extra: usize) -> bool {
    gs.as_deref()
        .is_some_and(|g| g.starts_with(grp) && g.chars().count() == grp.chars().count() + extra)
}

fn gs_pfx(gs: &Option<String>, grp: &str) -> bool {
    gs.as_deref().is_some_and(|g| g.starts_with(grp))
}

fn longname(name: &str) -> bool {
    name.chars().count() >= 3
}

/// Marks options and arguments inactive: those in `xor`, or with `opts`
/// all options. `cur` is the word that excludes them.
fn ca_inactive(d: &Cadef, xor: Option<&[String]>, cur: i32, opts: bool, cc: i32) {
    if !((xor.is_some() || opts) && cur <= cc) {
        return;
    }
    // The current word could be a prefix of a longer one, so only
    // single-letter options are excluded for it (for clumping).
    let single = !opts && cur == cc;
    let list: Vec<String> = if opts {
        vec!["-".to_string()]
    } else {
        xor.unwrap_or(&[]).to_vec()
    };
    for xs in &list {
        let x = chars(xs);
        let mut excludeall = false;
        let mut sep = 0usize;
        loop {
            let c = at(&x, sep);
            if matches!(c, '+' | '-' | ':' | '*') || c.is_ascii_digit() {
                break;
            }
            match x[sep..].iter().position(|&ch| ch == '-') {
                Some(off) if sep + off + 1 < x.len() => sep += off + 1,
                _ => {
                    // Just the name of a set or group.
                    excludeall = true;
                    sep = x.len();
                    break;
                }
            }
        }
        let grp: Option<String> = if sep > 0 {
            Some(string(&x[..sep]))
        } else {
            None
        };
        let xr = &x[sep..];
        let ea = usize::from(excludeall);

        if excludeall || xr == [':'] {
            match &grp {
                Some(g) => {
                    let mut a = d.args.clone();
                    while let Some(y) = a {
                        if gs_is(&y.gsname, g, ea) {
                            y.active.set(false);
                        }
                        a = y.next.clone();
                    }
                    if let Some(r) = &d.rest {
                        if gs_is(&r.gsname, g, ea) {
                            r.active.set(false);
                        }
                    }
                }
                None => d.argsactive.set(false),
            }
        }
        if excludeall || xr == ['-'] {
            for p in &d.opts {
                if grp.as_ref().is_none_or(|g| gs_is(&p.gsname, g, ea))
                    && !(single && longname(&p.name))
                {
                    p.active.set(false);
                }
            }
        }
        if excludeall || xr == ['*'] {
            if let Some(r) = &d.rest {
                if grp.as_ref().is_none_or(|g| gs_is(&r.gsname, g, ea)) {
                    r.active.set(false);
                }
            }
        }
        if !excludeall {
            if at(xr, 0).is_ascii_digit() {
                let n = atoi(&string(xr));
                let mut a = d.args.clone();
                while let Some(y) = a.clone() {
                    if y.num >= n {
                        break;
                    }
                    a = y.next.clone();
                }
                if let Some(y) = a {
                    if y.num == n && grp.as_ref().is_none_or(|g| gs_pfx(&y.gsname, g)) {
                        y.active.set(false);
                    }
                }
            } else if let Some((o, _)) = ca_get_opt(d, xr, false, true) {
                if grp.as_ref().is_none_or(|g| gs_pfx(&o.gsname, g))
                    && !(single && longname(&o.name))
                {
                    o.active.set(false);
                }
            }
            if opts {
                break;
            }
        }
    }
}

/// An option's argument in `line`, ignoring quoting in the option name.
fn ca_opt_arg(opt: &Caopt, line: &[char]) -> String {
    let o = chars(&opt.name);
    let (mut oi, mut li) = (0, 0);
    loop {
        if at(&o, oi) == '\\' {
            oi += 1;
        }
        if matches!(at(line, li), '\\' | '\'' | '"') {
            li += 1;
        }
        if at(&o, oi) == '\0' || at(&o, oi) != at(line, li) {
            break;
        }
        oi += 1;
        li += 1;
    }
    if at(line, li) != '\0' && (opt.type_ == CAO_EQUAL || opt.type_ == CAO_OEQUAL) {
        if at(line, li) == '\\' {
            li += 1;
        }
        if at(line, li) == '=' {
            li += 1;
        }
    }
    string(&line[li.min(line.len())..])
}

fn wd(words: &[String], k: i32) -> Option<&String> {
    if k >= 0 {
        words.get(k as usize)
    } else {
        None
    }
}

fn oargs_add(st: &Castate, num: usize, items: impl IntoIterator<Item = String>) {
    let mut oa = st.oargs.borrow_mut();
    oa[num].get_or_insert_with(Vec::new).extend(items);
}

fn oargs_make(st: &Castate, num: usize) {
    oargs_add(st, num, std::iter::empty());
}

// Whether an option's argument is in the same word as the option.
fn arg_in_word(type_: i32, pe0: char, pm1: char) -> bool {
    type_ == CAO_DIRECT
        || type_ == CAO_EQUAL
        || (type_ == CAO_ODIRECT && pe0 != '\0')
        || (type_ == CAO_OEQUAL && (pe0 != '\0' || pm1 == '='))
}

/// Parses the line for set `d`. Returns 1 if the set should be skipped
/// because it doesn't match the options on the line, and the state
/// (zsh's ca_laststate) for the current word.
fn ca_parse_line(
    d: &Rc<Cadef>,
    all: &[Rc<Cadef>],
    multi: bool,
    words: &[String],
    cc: i32,
    ca_doff: &mut i32,
) -> (i32, Castate) {
    // Mark everything as active.
    for p in &d.opts {
        p.active.set(true);
    }
    d.argsactive.set(true);
    if let Some(r) = &d.rest {
        r.active.set(true);
    }
    let mut a = d.args.clone();
    while let Some(x) = a {
        x.active.set(true);
        a = x.next.clone();
    }

    let argend = words.len() as i32 - 1;
    let mut state = Castate {
        d: d.clone(),
        def: None,
        ddef: None,
        curopt: None,
        dopt: None,
        opt: 1,
        arg: 1,
        argbeg: 1,
        optbeg: 1,
        nargbeg: 1,
        restbeg: 1,
        curpos: cc,
        argend,
        inopt: 0,
        inarg: 1,
        nth: 1,
        singles: 0,
        oopt: 0,
        actopts: 1,
        args: Rc::new(RefCell::new(Vec::new())),
        oargs: Rc::new(RefCell::new(vec![None; d.opts.len()])),
    };
    let mut last = state.clone();
    let word = |k: i32| wd(words, k);
    let rest_words =
        |from: i32| -> Vec<String> { words.iter().skip(from.max(0) as usize).cloned().collect() };

    if word(1).is_none() {
        last.opt = 0;
        last.arg = 0;
    } else {
        let napat = d.nonarg.as_deref().map(Pattern::new);
        let mut endpat: Option<Pattern> = None;
        let mut sopts: Vec<Rc<Caopt>> = Vec::new();
        let mut argxor: Option<Vec<String>> = None;
        let mut wasopt: Option<Rc<Caopt>> = None;
        let mut doff: i32 = 0;
        let mut cur: i32 = 1;

        // Each word: `line` is words[cur - 1].
        'words: loop {
            cur += 1;
            let Some(oline) = word(cur - 1).cloned() else {
                break 'words;
            };
            let mut ddef: Option<Rc<Caarg>> = None;
            let mut adef: Option<Rc<Caarg>> = None;
            let mut dopt: Option<Rc<Caopt>> = None;
            state.singles = 0;
            let mut arglast = false;

            let line: Vec<char> = untokenize(&remnulargs(&chars(&oline)));
            let line_s = string(&line);
            let l0 = at(&line, 0);

            if let Some(x) = argxor.take() {
                ca_inactive(d, Some(x.as_slice()), cur - 1, false, cc);
            }
            if cur != cc
                && state.actopts != 0
                && !(state.def.is_some() && state.inopt != 0)
                && ((d.flags & CDF_SEP != 0 && line_s == "--")
                    || (d.flags & CDF_ZSEP != 0 && line_s == "-"))
            {
                ca_inactive(d, None, cur, true, cc);
                state.actopts = 0;
                continue 'words;
            }

            'cont: {
                // With a definition for an option or rest argument, this
                // word is an argument for it.
                if let Some(def) = state.def.clone() {
                    state.arg = 0;
                    if let Some(co) = &state.curopt {
                        oargs_add(&state, co.num, [oline.clone()]);
                    }
                    state.opt = i32::from(def.type_ == CAA_OPT);
                    if state.opt != 0 && def.opt.is_some() {
                        state.oopt += 1;
                    }
                    if is_rest(def.type_) {
                        if def.end.is_some()
                            && endpat.as_ref().is_some_and(|p| p.matches_chars(&line))
                        {
                            state.def = None;
                            state.curopt = None;
                            state.opt = 1;
                            state.arg = 1;
                            state.argend = cur - 1;
                            last.argend = cur - 1;
                            break 'cont;
                        }
                    } else {
                        state.def = def.next.clone();
                        if state.def.is_some() {
                            state.argbeg = cur;
                            state.argend = argend;
                            break 'cont;
                        } else if !sopts.is_empty() {
                            let co = sopts.remove(0);
                            state.def = co.args.clone();
                            state.opt = 0;
                            state.argbeg = cur;
                            state.optbeg = cur;
                            state.inopt = cur;
                            state.argend = argend;
                            doff = 0;
                            state.singles = 1;
                            oargs_make(&state, co.num);
                            state.curopt = Some(co);
                            break 'cont;
                        } else {
                            state.curopt = None;
                            state.opt = 1;
                            break 'cont;
                        }
                    }
                } else {
                    state.opt = 1;
                    state.arg = 1;
                    state.curopt = None;
                }
                if state.opt != 0 {
                    state.opt = match line.len() {
                        0 => 0,
                        1 => 1,
                        _ => 2,
                    };
                }
                let mut pe: Option<usize> = None;
                wasopt = None;

                // Is it an option?
                let mut is_opt = false;
                if state.opt == 2 && plusminus(l0) {
                    let r = ca_get_opt(d, &line, false, false);
                    state.curopt = r.as_ref().map(|x| x.0.clone());
                    if let Some((co, e)) = r {
                        pe = Some(e);
                        let pm1 = if e > 0 { at(&line, e - 1) } else { '\0' };
                        is_opt = match co.type_ {
                            CAO_OEQUAL => word(cur).is_some() || pm1 == '=',
                            CAO_EQUAL => pm1 == '=' || at(&line, e) == '\0',
                            _ => true,
                        };
                    }
                }
                let mut is_sopt = false;
                if !is_opt && state.opt == 2 && d.single.is_some() && plusminus(l0) {
                    let (o, e, l) = ca_get_sopt(d, &line, false);
                    state.curopt = o;
                    sopts = l;
                    if e.is_some() {
                        pe = e;
                    }
                    is_sopt = state.curopt.is_some() || (cur != cc && !sopts.is_empty());
                }
                if is_opt {
                    let Some(co) = state.curopt.clone() else {
                        break 'cont;
                    };
                    let e = pe.unwrap_or(0);
                    let pm1 = if e > 0 { at(&line, e - 1) } else { '\0' };
                    let pe0 = at(&line, e);
                    state.def = if co.type_ != CAO_EQUAL || pm1 == '=' {
                        co.args.clone()
                    } else {
                        None
                    };
                    ddef = state.def.clone();
                    if ddef.is_some() {
                        dopt = Some(co.clone());
                    }
                    doff = e as i32;
                    state.optbeg = cur;
                    state.argbeg = cur;
                    state.inopt = cur;
                    state.argend = argend;
                    let nm = chars(&co.name);
                    // `--' is not a single option called `-'.
                    state.singles = i32::from(
                        d.single.is_some()
                            && pe0 == '\0'
                            && at(&nm, 1) != '\0'
                            && at(&nm, 2) == '\0'
                            && at(&nm, 1) != '-',
                    );
                    oargs_make(&state, co.num);
                    ca_inactive(d, co.xor.as_deref(), cur, false, cc);

                    // Collect the argument strings. Maybe.
                    if let Some(def) = state.def.clone() {
                        if arg_in_word(co.type_, pe0, pm1) {
                            if !is_rest(def.type_) {
                                state.def = def.next.clone();
                            }
                            oargs_add(&state, co.num, [ca_opt_arg(&co, &chars(&oline))]);
                        }
                    }
                    if state.def.is_some() {
                        state.opt = 0;
                    } else {
                        if d.single.is_none() || (at(&nm, 1) != '\0' && at(&nm, 2) != '\0') {
                            wasopt = Some(co);
                        }
                        state.curopt = None;
                    }
                } else if is_sopt {
                    // A single-letter option.
                    if cur != cc && !sopts.is_empty() {
                        state.curopt = Some(sopts.remove(0));
                    }
                    let Some(co) = state.curopt.clone() else {
                        break 'cont;
                    };
                    oargs_make(&state, co.num);
                    state.def = co.args.clone();
                    ddef = if co.type_ == CAO_NEXT && cur == cc {
                        None
                    } else {
                        state.def.clone()
                    };
                    dopt = Some(co.clone());
                    let e = pe.unwrap_or(0);
                    doff = e as i32;
                    state.optbeg = cur;
                    state.argbeg = cur;
                    state.inopt = cur;
                    state.argend = argend;
                    state.singles = i32::from(pe.is_none() || at(&line, e) == '\0');

                    for k in 1..e {
                        let t = match (&d.single, single_index(l0, at(&line, k))) {
                            (Some(s), Some(idx)) => s[idx].clone(),
                            _ => None,
                        };
                        if let Some(t) = t {
                            oargs_make(&state, t.num);
                            ca_inactive(d, t.xor.as_deref(), cur, false, cc);
                        }
                    }
                    if let Some(def) = state.def.clone() {
                        let pm1 = if e > 0 { at(&line, e - 1) } else { '\0' };
                        if arg_in_word(co.type_, at(&line, e), pm1) {
                            if !is_rest(def.type_) {
                                state.def = def.next.clone();
                            }
                            oargs_add(&state, co.num, [ca_opt_arg(&co, &line)]);
                        }
                    }
                    if state.def.is_some() {
                        state.opt = 0;
                    } else {
                        state.curopt = None;
                    }
                } else if multi && plusminus(l0) && cur != cc && ca_foreign_opt(d, all, &line) {
                    return (1, last);
                } else if state.arg != 0 && cur <= cc {
                    // A normal argument.

                    // A word matching the -A pattern is an unknown
                    // option: go on to the next word.
                    if let Some(np) = &napat {
                        if cur < cc && state.actopts != 0 {
                            if np.matches_chars(&line) {
                                continue 'words;
                            }
                            ca_inactive(d, None, cur + 1, true, cc);
                            state.actopts = 0;
                        }
                    }
                    arglast = true;
                    // The first normal argument after an option.
                    if state.inopt != 0 {
                        state.inopt = 0;
                        state.nargbeg = cur - 1;
                        state.argend = argend;
                    }
                    if d.args.is_none() && d.rest.is_none() && l0 != '\0' && !plusminus(l0) {
                        if !multi && cur > cc {
                            break 'words;
                        }
                        return (1, last);
                    }
                    adef = ca_get_arg(d, state.nth);
                    state.def = adef.clone();
                    if let Some(ad) = adef.clone() {
                        if ad.type_ == CAA_RREST || ad.type_ == CAA_RARGS {
                            // The "rest" definition: unless another one
                            // already describes the current word, narrow
                            // the scope to the remaining words.
                            if last.def.is_some() {
                                break 'words;
                            }
                            state.opt = i32::from(
                                cur == state.nargbeg + 1 && (!multi || l0 == '\0' || plusminus(l0)),
                            );
                            state.optbeg = state.nargbeg;
                            state.argbeg = cur - 1;
                            state.argend = argend;
                            {
                                let mut sa = state.args.borrow_mut();
                                sa.push(line_s.clone());
                                sa.extend(rest_words(cur));
                            }
                            last = state.clone();
                            last.ddef = None;
                            last.dopt = None;
                            break 'words;
                        }
                    }
                    state.args.borrow_mut().push(line_s.clone());
                    if let Some(ad) = &adef {
                        state.oopt = ad.num - state.nth;
                    }
                    if let Some(def) = &state.def {
                        if cur != cc {
                            argxor = def.xor.clone();
                        }
                    }
                    let normal = |t: i32| t == CAA_NORMAL || t == CAA_OPT;
                    match &state.def {
                        Some(def) if !normal(def.type_) && state.inarg != 0 => {
                            state.restbeg = cur;
                            state.inarg = 0;
                        }
                        Some(def) if !normal(def.type_) => {}
                        _ => state.inarg = 1,
                    }
                    state.nth += 1;
                    state.def = None;
                }
            }

            // The end-pattern test, if needed.
            if let (Some(def), Some(co)) = (state.def.clone(), state.curopt.clone()) {
                if def.type_ == CAA_RREST || def.type_ == CAA_RARGS {
                    if let Some(e) = &def.end {
                        endpat = Some(Pattern::new(e));
                    } else {
                        if cur < cc {
                            last = state.clone();
                        }
                        let mut items = vec![line_s.clone()];
                        items.extend(rest_words(cur));
                        oargs_add(&state, co.num, items);
                        last.ddef = None;
                        last.dopt = None;
                        break 'words;
                    }
                } else if let Some(e) = &def.end {
                    endpat = Some(Pattern::new(e));
                }
            } else if let Some(e) = state.def.as_ref().and_then(|d| d.end.as_ref()) {
                endpat = Some(Pattern::new(e));
            }

            // Copy the state into the global one.
            if cur + 1 == cc {
                last = state.clone();
                last.ddef = None;
                last.dopt = None;
            } else if cur == cc && last.def.is_none() {
                last.def = ddef.clone();
                if ddef.is_some() {
                    last.singles = state.singles;
                    match state.curopt.as_ref().filter(|c| c.type_ == CAO_NEXT) {
                        Some(co) => {
                            last.ddef = ddef;
                            last.dopt = dopt;
                            last.def = None;
                            last.opt = 1;
                            co.active.set(true);
                        }
                        None => {
                            *ca_doff = doff;
                            last.opt = 0;
                        }
                    }
                } else {
                    last.def = adef;
                    last.opt = i32::from(!arglast || !multi || l0 == '\0' || plusminus(l0));
                    last.ddef = None;
                    last.dopt = None;
                    last.optbeg = state.nargbeg;
                    last.argbeg = state.restbeg;
                    last.argend = state.argend;
                    last.singles = state.singles;
                    last.oopt = state.oopt;
                    if let Some(w) = &wasopt {
                        w.active.set(true);
                    }
                }
            }
        }
    }
    last.actopts = d.opts.iter().filter(|p| p.active.get()).count() as i32;
    (0, last)
}

/// $opt_args values joined with NULs.
fn ca_nullist(l: &Option<Vec<String>>) -> String {
    l.as_ref().map(|v| v.join("\0")).unwrap_or_default()
}

/// $opt_args values joined with colons, escaping colons and backslashes.
fn ca_colonlist(l: &Option<Vec<String>>) -> String {
    let Some(v) = l else {
        return String::new();
    };
    v.iter()
        .map(|s| {
            let mut o = String::new();
            for c in s.chars() {
                if c == ':' || c == '\\' {
                    o.push('\\');
                }
                o.push(c);
            }
            o
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// The descriptions, actions and sub-contexts for comparguments -D/-L.
#[derive(Default)]
struct SetData {
    descr: Vec<String>,
    act: Vec<String>,
    subc: Vec<String>,
}

fn rest_if_active(d: &Cadef) -> Option<Rc<Caarg>> {
    d.rest.clone().filter(|r| r.active.get())
}

/// Adds the descriptions, actions and sub-contexts for `arg` (an
/// argument of option `opt`, or with None a normal argument) and the
/// arguments after it.
fn ca_set_data(
    z: &mut Zle,
    sd: &mut SetData,
    mut opt: Option<String>,
    mut arg: Option<Rc<Caarg>>,
    optdef: Option<&Rc<Caopt>>,
    single: bool,
    ls: &Castate,
) {
    let mut restr = false;
    let mut miss = false;
    let mut oopt = 1;
    let mut lopt = false;

    loop {
        let addopt = if opt.is_some() { 0 } else { ls.oopt };
        while let Some(a) = arg.clone() {
            if !(opt.is_some() || a.num < 0 || (a.min <= ls.nth + addopt && a.num >= ls.nth)) {
                break;
            }
            lopt = a.type_ == CAA_OPT;
            if opt.is_none() && !lopt && oopt > 0 {
                oopt = 0;
            }
            let found = sd
                .descr
                .iter()
                .zip(sd.act.iter())
                .any(|(d, ac)| *d == a.descr && *ac == a.action);

            // With an ignored prefix, no normal arguments are completed.
            if single && a.opt.is_none() {
                return;
            }
            if !found {
                sd.descr.push(a.descr.clone());
                sd.act.push(a.action.clone());
                if !restr {
                    if a.type_ == CAA_RARGS {
                        restr = true;
                        restrict_range(z, ls.optbeg, ls.argend);
                    } else if a.type_ == CAA_RREST {
                        restr = true;
                        restrict_range(z, ls.argbeg, ls.argend);
                    }
                }
                let gs = a.gsname.as_deref().unwrap_or("");
                let buf = if let Some(o) = &a.opt {
                    if a.num > 0 && a.type_ < CAA_REST {
                        format!("{}option{}-{}", gs, o, a.num)
                    } else {
                        format!("{}option{}-rest", gs, o)
                    }
                } else if a.num > 0 {
                    format!("{}argument-{}", gs, a.num)
                } else {
                    format!("{}argument-rest", gs)
                };
                sd.subc.push(buf);
            }
            // An argument that must follow its option in this word
            // excludes normal arguments.
            if a.type_ == CAA_NORMAL && opt.is_some() {
                if let Some(od) = optdef {
                    if od.type_ == CAO_NEXT || od.type_ == CAO_ODIRECT || od.type_ == CAO_OEQUAL {
                        return;
                    }
                }
            }
            if single {
                break;
            }
            if opt.is_none() {
                if a.num >= 0 && a.next.is_none() && miss {
                    arg = rest_if_active(&ls.d);
                } else {
                    let onum = a.num;
                    let rest = onum != a.min && onum == ls.nth;
                    arg = a.next.clone();
                    if let Some(n) = &arg {
                        if n.num != onum + 1 {
                            miss = true;
                        }
                    } else if rest || oopt > 0 {
                        arg = rest_if_active(&ls.d);
                        oopt = -1;
                    }
                }
            } else {
                if !lopt {
                    break;
                }
                arg = a.next.clone();
            }
        }
        if !single && opt.is_some() && (lopt || ls.oopt != 0) {
            opt = None;
            arg = ca_get_arg(&ls.d, ls.nth);
            continue;
        }
        if opt.is_none() && oopt > 0 {
            oopt = -1;
            arg = rest_if_active(&ls.d);
            continue;
        }
        break;
    }
}

fn bin_comparguments(z: &mut Zle, nam: &str, args: &[String]) -> i32 {
    let a0 = chars(&args[0]);
    if at(&a0, 0) != '-' || a0.len() != 2 {
        builtin_err(nam, &format!("invalid argument: {}", args[0]));
        return 1;
    }
    let op = a0[1];
    let parsed = CA.with(|c| c.borrow().parsed);
    if op != 'i' && op != 'I' && !parsed {
        builtin_err(nam, "no parsed state");
        return 1;
    }
    let (min, max): (usize, Option<usize>) = match op {
        'i' => (2, None),
        'D' => (3, Some(3)),
        'O' => (4, Some(4)),
        'L' => (3, Some(4)),
        's' => (1, Some(1)),
        'M' => (1, Some(1)),
        'a' => (0, Some(0)),
        'W' => (3, Some(3)),
        'n' => (1, Some(1)),
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
        // Parse the definitions and the line.
        if z.comp.compcurrent > 1 && !z.comp.compwords.is_empty() {
            CA.with(|c| c.borrow_mut().parsed = false);
            let Some(all) = get_cadef(nam, &args[1..]) else {
                return 1;
            };
            let multi = all.len() > 1;
            let words = z.comp.compwords.clone();
            let cc = z.comp.compcurrent as i32;
            let mut doff = 0;
            let mut ret = 0;
            let mut laststate: Option<Castate> = None;
            // The saved states, most recent first.
            let mut states: VecDeque<Castate> = VecDeque::new();
            for (k, def) in all.iter().enumerate() {
                let (r, ls) = ca_parse_line(def, &all, multi, &words, cc, &mut doff);
                let use_ = r == 0;
                let more = k + 1 < all.len();
                if use_ && more {
                    states.push_front(ls.clone());
                    laststate = Some(ls);
                } else if !use_ && !more {
                    match states.pop_front() {
                        Some(s) => laststate = Some(s),
                        None => {
                            laststate = Some(ls);
                            ret = 1;
                        }
                    }
                } else {
                    laststate = Some(ls);
                }
            }
            let mut chain: Vec<Castate> = laststate.into_iter().collect();
            chain.extend(states);
            CA.with(|c| {
                let mut c = c.borrow_mut();
                c.states = chain;
                c.parsed = true;
                c.doff = doff;
            });
            return ret;
        }
        return 1;
    }
    let (states, doff) = CA.with(|c| {
        let c = c.borrow();
        (c.states.clone(), c.doff)
    });
    let Some(first) = states.first() else {
        return 1;
    };
    match op {
        'D' => {
            // The descriptions, actions and sub-contexts of the arguments
            // to complete here; nonzero if there are none.
            let mut sd = SetData::default();
            let mut ret = 1;
            ignore_prefix(z, doff.max(0) as usize);
            for ls in &states {
                if let Some(arg) = ls.def.clone() {
                    ret = 0;
                    let o = arg.opt.clone();
                    ca_set_data(
                        z,
                        &mut sd,
                        o,
                        Some(arg),
                        ls.curopt.as_ref(),
                        doff > 0,
                        first,
                    );
                }
            }
            if ret == 0 {
                set_list_array(&args[1], sd.descr);
                set_list_array(&args[2], sd.act);
                set_list_array(&args[3], sd.subc);
            }
            ret
        }
        'O' => {
            // The options to complete, in _describe's form, by how their
            // arguments follow them.
            let mut lists: [Vec<String>; 4] = Default::default();
            let mut ret = 1;
            let cc = z.comp.compcurrent;
            for ls in &states {
                let def = ls.def.as_ref();
                let want = ls.actopts != 0
                    && (ls.opt != 0
                        || (doff != 0 && def.is_some())
                        || def.is_some_and(|d| {
                            d.opt.is_some()
                                && (d.type_ == CAA_OPT || (d.type_ >= CAA_RARGS && d.num < 0))
                        }))
                    && def.is_none_or(|d| {
                        d.type_ < CAA_RARGS
                            || if d.type_ == CAA_RARGS {
                                ls.curpos == ls.argbeg + 1
                            } else {
                                cc == 1
                            }
                    });
                if !want {
                    continue;
                }
                ret = 0;
                for p in &ls.d.opts {
                    if !p.active.get() || p.not {
                        continue;
                    }
                    let l = match p.type_ {
                        CAO_NEXT => 0,
                        CAO_DIRECT => 1,
                        CAO_ODIRECT => 2,
                        _ => 3,
                    };
                    let s = match &p.descr {
                        Some(d) => format!("{}:{}", bslashcolon(&p.name), d),
                        None => bslashcolon(&p.name),
                    };
                    if !lists[l].contains(&s) {
                        lists[l].push(s);
                    }
                }
            }
            if ret == 0 {
                let [next, direct, odirect, equal] = lists;
                set_list_array(&args[1], next);
                set_list_array(&args[2], direct);
                set_list_array(&args[3], odirect);
                set_list_array(&args[4], equal);
                return 0;
            }
            if first.singles != 0 {
                2
            } else {
                1
            }
        }
        'L' => {
            // The option the start of the current word names, for
            // completing its argument (`--pre=/<TAB>').
            let mut sd = SetData::default();
            let mut ret = 1;
            let line = chars(&args[1]);
            for ls in &states {
                if let Some((o, _)) = ca_get_opt(&ls.d, &line, false, true) {
                    if o.args.is_some() {
                        ret = 0;
                        let name = o.name.clone();
                        ca_set_data(
                            z,
                            &mut sd,
                            Some(name),
                            o.args.clone(),
                            Some(&o),
                            true,
                            first,
                        );
                    }
                }
            }
            if ret == 0 {
                set_list_array(&args[2], sd.descr);
                set_list_array(&args[3], sd.act);
                if let Some(a) = args.get(4) {
                    set_list_array(a, sd.subc);
                }
            }
            ret
        }
        's' => {
            // Zero when completing clumped single-letter options; the
            // parameter says how the last one takes its argument.
            for ls in &states {
                if ls.d.single.is_some() && ls.singles != 0 && ls.actopts != 0 {
                    let v = match (&ls.ddef, &ls.dopt) {
                        (Some(_), Some(o)) => match o.type_ {
                            CAO_DIRECT => "direct",
                            CAO_OEQUAL | CAO_EQUAL => "equal",
                            _ => "next",
                        },
                        _ => "",
                    };
                    crate::shell::var_set(&args[1], v, false);
                    return 0;
                }
            }
            1
        }
        'M' => {
            crate::shell::var_set(&args[1], &first.d.match_, false);
            0
        }
        'a' => {
            // Zero if there are or were normal arguments to complete.
            if states
                .iter()
                .any(|ls| ls.d.args.is_some() || ls.d.rest.is_some())
            {
                0
            } else {
                1
            }
        }
        'W' => {
            // $line and $opt_args.
            let use_nul = !args[3].starts_with('0');
            let mut line: Vec<String> = Vec::new();
            for ls in &states {
                line.extend(ls.args.borrow().iter().cloned());
            }
            crate::shell::array_set(&args[1], line);
            let mut pairs: Vec<String> = Vec::new();
            for ls in &states {
                let oa = ls.oargs.borrow();
                for (o, a) in ls.d.opts.iter().zip(oa.iter()) {
                    if a.is_some() {
                        pairs.push(format!("{}{}", o.gsname.as_deref().unwrap_or(""), o.name));
                        pairs.push(if use_nul {
                            ca_nullist(a)
                        } else {
                            ca_colonlist(a)
                        });
                    }
                }
            }
            crate::shell::assoc_declare(&args[2]);
            crate::shell::array_set(&args[2], pairs);
            0
        }
        'n' => {
            // The index of the word where normal arguments began.
            let base = i32::from(!crate::options::is_set("ksharrays"));
            crate::shell::var_set(&args[1], &(first.optbeg + base).to_string(), false);
            0
        }
        _ => 1,
    }
}

/// The comparguments builtin; `words[0]` is its name.
pub fn builtin_comparguments(words: &[String]) -> i32 {
    let name = words.first().map(String::as_str).unwrap_or("comparguments");
    let args = words.get(1..).unwrap_or(&[]);
    if args.is_empty() {
        builtin_err(name, "not enough arguments");
        return 1;
    }
    let r = crate::zle::with_active(|z| {
        if z.comp.incompfunc != 1 {
            return None;
        }
        pull_comp_params(z);
        let r = bin_comparguments(z, name, args);
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
