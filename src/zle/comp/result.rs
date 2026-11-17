//! Using the matches, after zsh's Src/Zle/compresult.c: inserting the
//! unambiguous string or a single match, menu completion, and the
//! default (non-complist) listing.
//!
//! Listing goes through a replaceable hook, as zsh's comp_list_matches
//! hookdef does: [`list_matches`] calls the function set with
//! [`set_complistmatches_hook`] (complist's lister), or [`ilistmatches`]
//! when none is set.

use super::matching::{cline_setlens, cline_sublen};
use super::*;
use crate::zle::refresh::{flush_tty, write_tty};
use crate::zle::Zle;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Space between listing columns (zsh's CM_SPACE).
pub const CM_SPACE: i32 = 2;

// fromcomp bits (FC_*).
pub const FC_LINE: i32 = 1;
pub const FC_INWORD: i32 = 2;

/// A lister for [`list_matches`]: complist's replaces the default one.
pub type ListMatchesHook = fn(&mut Zle) -> i32;

/// The per-match printer printlist() calls: group index, match index
/// (None for an empty cell), column, line, whether it is the last
/// column, and the column width.
type PrintFunc = fn(&mut Zle, usize, Option<usize>, i32, i32, bool, i32);

thread_local! {
    static COMPLISTMATCHESHOOK: Cell<Option<ListMatchesHook>> = const { Cell::new(None) };
    // zle_refresh.c's listshown, lastlistlen and clearflag.
    static LISTSHOWN: Cell<i32> = const { Cell::new(0) };
    static LASTLISTLEN: Cell<i32> = const { Cell::new(0) };
    static CLEARFLAG: Cell<i32> = const { Cell::new(0) };
    // calclist()'s static lastinvcount.
    static LASTINVCOUNT: Cell<i32> = const { Cell::new(-1) };
    // unambig_data()'s caches: string, cursor, positions, insert positions.
    static UNAMBIG: RefCell<Option<(String, i32, String, String)>> = const { RefCell::new(None) };
}

/// Sets the lister [`list_matches`] runs; None restores the default.
pub fn set_complistmatches_hook(f: Option<ListMatchesHook>) {
    COMPLISTMATCHESHOOK.with(|h| h.set(f));
}

/// zsh's listshown: 1 if a list is shown below the prompt, -1 if it was
/// printed and scrolled away, 0 if none.
pub fn listshown() -> i32 {
    LISTSHOWN.with(|c| c.get())
}

pub fn set_listshown(z: &mut Zle, v: i32) {
    LISTSHOWN.with(|c| c.set(v));
    z.refresh.listshown = v != 0;
}

pub fn lastlistlen() -> i32 {
    LASTLISTLEN.with(|c| c.get())
}

pub fn set_lastlistlen(v: i32) {
    LASTLISTLEN.with(|c| c.set(v));
}

pub fn clearflag() -> i32 {
    CLEARFLAG.with(|c| c.get())
}

pub fn set_clearflag(v: i32) {
    CLEARFLAG.with(|c| c.set(v));
}

// ---- other modules ------------------------------------------------------

fn wb() -> i32 {
    crate::zle::comp::tricky::wb()
}

fn we() -> i32 {
    crate::zle::comp::tricky::we()
}

/// inststrlen(): inserts `len` characters of `s` (all for -1).
fn ins(z: &mut Zle, s: &str, mv: bool, len: i32) -> i32 {
    crate::zle::comp::tricky::inststrlen(z, s, mv, len)
}

fn ins_opt(z: &mut Zle, s: &Option<String>, mv: bool, len: i32) -> i32 {
    match s {
        Some(s) => ins(z, s, mv, len),
        None => 0,
    }
}

fn printfmt(z: &mut Zle, fmt: &str, n: i32, dopr: i32, doesc: i32) -> i32 {
    crate::zle::comp::tricky::printfmt(z, fmt, n, dopr != 0, doesc != 0)
}

fn makesuffix(z: &mut Zle, n: i32) {
    crate::zle::comp::tricky::makesuffix(z, n);
}

fn makesuffixstr(z: &mut Zle, f: Option<&str>, s: Option<&str>, n: i32) {
    crate::zle::comp::tricky::makesuffixstr(z, f, s, n);
}

fn addsuffix_posstr(z: &mut Zle, chars: &[char], lenstr: i32, lensuf: i32) {
    use crate::zle::comp::tricky;
    let n = (lenstr.max(0) as usize).min(chars.len());
    tricky::addsuffix(z, tricky::SUFTYP_POSSTR, 0, &chars[..n], lensuf);
}

fn makeparamsuffix(z: &mut Zle, br: bool, n: i32) {
    crate::zle::comp::tricky::makeparamsuffix(z, br, n);
}

fn fixsuffix(z: &mut Zle) {
    crate::zle::comp::tricky::fixsuffix(z);
}

fn iremovesuffix(z: &mut Zle, c: char) {
    crate::zle::comp::tricky::iremovesuffix(z, Some(c), true);
}

fn isset(name: &str) -> bool {
    crate::options::is_set(name)
}

// ---- small helpers ------------------------------------------------------

fn cs(z: &Zle) -> i32 {
    z.cs as i32
}

fn set_cs(z: &mut Zle, v: i32) {
    z.cs = v.clamp(0, z.line.len() as i32) as usize;
}

fn foredel(z: &mut Zle, n: i32) {
    if n > 0 {
        z.foredel(n as usize, crate::zle::CUT_RAW);
    }
}

fn clen(s: &str) -> i32 {
    s.chars().count() as i32
}

fn olen(s: &Option<String>) -> i32 {
    s.as_deref().map_or(0, clen)
}

fn line_str(z: &Zle, from: i32, to: i32) -> String {
    let n = z.line.len() as i32;
    let (a, b) = (from.clamp(0, n) as usize, to.clamp(0, n) as usize);
    if a >= b {
        String::new()
    } else {
        z.line[a..b].iter().collect()
    }
}

/// zsh's zterm_columns.
pub fn zterm_columns() -> i32 {
    crate::shell::terminal_size()
        .map(|(c, _)| c as i32)
        .filter(|&c| c > 0)
        .or_else(|| crate::shell::var_get_opt("COLUMNS").and_then(|c| c.parse().ok()))
        .filter(|&c| c > 0)
        .unwrap_or(80)
}

/// zsh's zterm_lines.
pub fn zterm_lines() -> i32 {
    crate::shell::terminal_size()
        .map(|(_, l)| l as i32)
        .filter(|&l| l > 0)
        .or_else(|| crate::shell::var_get_opt("LINES").and_then(|l| l.parse().ok()))
        .filter(|&l| l > 0)
        .unwrap_or(24)
}

fn strwidth(s: &str) -> i32 {
    crate::display_width(s) as i32
}

/// `s` with unprintable characters made visible, as zsh's niceformat.
fn nicestr(s: &str) -> String {
    s.chars().map(crate::zle::utils::nicechar).collect()
}

fn nicewidth(s: &str) -> i32 {
    strwidth(&nicestr(s))
}

fn cleareod() {
    write_tty("\x1b[J");
}

fn tcup(n: i32) {
    if n > 0 {
        write_tty(&format!("\x1b[{}A", n));
    }
}

// ---- the unambiguous string ---------------------------------------------

fn flatten(l: Cline) -> Vec<ClineNode> {
    let mut v = Vec::new();
    let mut cur = l;
    while let Some(mut n) = cur {
        cur = n.next.take();
        v.push(*n);
    }
    v
}

fn setlens_vec(v: &mut [ClineNode]) {
    for n in v.iter_mut() {
        n.min = cline_sublen(n);
    }
}

/// Cuts the cline list before the parts not worth inserting. The list
/// comes back as its top-level nodes.
fn cut_cline(z: &Zle, l: &Cline) -> Vec<ClineNode> {
    if z.comp.hasmatched == 0 {
        let mut c = l.clone();
        cline_setlens(&mut c, false);
        return flatten(c);
    }
    let mut v = flatten(l.clone());
    let n = v.len();
    let next = |i: usize| if i + 1 < n { Some(i + 1) } else { None };
    let mut ls = false;

    let mut e: Option<usize> = if n > 0 { Some(0) } else { None };
    let mut q: Option<usize> = None;
    for (i, p) in v.iter().enumerate() {
        if p.orig.is_some() || p.olen != 0 || p.flags & CLF_NEW == 0 {
            e = next(i);
        }
        if p.suffix.is_none() && (p.wlen != 0 || p.llen != 0 || p.prefix.is_some()) {
            q = Some(i);
        }
    }
    if let (None, Some(qi)) = (e, q) {
        let qn = &mut v[qi];
        if qn.orig.is_none()
            && qn.olen == 0
            && qn.flags & CLF_MISS != 0
            && (qn.flags & CLF_MATCHED == 0 || (qn.prefix.is_none() && qn.suffix.is_none()))
            && (if qn.word.is_some() { qn.wlen } else { qn.llen }) < 3
        {
            qn.word = None;
            qn.line = None;
            qn.wlen = 0;
            qn.llen = 0;
        }
    }
    while let Some(ei) = e {
        if v[ei].flags & CLF_MISS != 0 {
            break;
        }
        e = next(ei);
    }
    if let Some(ei) = e {
        if (ei + 1..n).any(|p| v[p].flags & CLF_MISS != 0) {
            let (mut sum, mut max) = (0, 0);
            let mut maxp = None;
            for (p, node) in v.iter().enumerate().skip(ei) {
                if node.flags & CLF_MISS == 0 {
                    sum += node.max;
                } else {
                    let tmp = cline_sublen(node);
                    if tmp > 2 && tmp > ((node.max + node.min) >> 1) {
                        sum += tmp - (node.max - tmp);
                    } else if tmp < node.min {
                        sum -= (((node.max + node.min) >> 1) - tmp) << (tmp < 2) as i32;
                    }
                }
                if sum > max {
                    max = sum;
                    maxp = Some(p);
                }
            }
            let mut cut = true;
            if max != 0 {
                e = maxp;
            } else {
                setlens_vec(&mut v);
                ls = true;
                let len: i32 = v[ei..].iter().map(|p| p.min).sum();
                if len > ((z.comp.minmlen << 1) / 3) {
                    cut = false;
                }
            }
            if let (true, Some(ee)) = (cut, e) {
                let en = &mut v[ee];
                en.line = None;
                en.word = None;
                en.llen = 0;
                en.wlen = 0;
                en.olen = 0;
                v.truncate(ee + 1);
            }
        }
    }

    // Without parts with missing characters, drop joined substrings.
    let mut ej = None;
    let mut tmp = false;
    let mut miss = false;
    let has_join = |l: &Cline| {
        let mut p = l.as_deref();
        while let Some(q) = p {
            if q.flags & CLF_JOIN != 0 {
                return true;
            }
            p = q.next.as_deref();
        }
        false
    };
    for (i, p) in v.iter().enumerate() {
        if p.flags & (CLF_MISS | CLF_DIFF) != 0 {
            miss = true;
        }
        if has_join(&p.prefix) {
            ej = Some(i);
            tmp = false;
        }
        if has_join(&p.suffix) {
            ej = Some(i);
            tmp = true;
        }
    }
    if let Some(ei) = ej {
        if !miss || cline_sublen(&v[ei]) == v[ei].min {
            let head = if tmp {
                &mut v[ei].suffix
            } else {
                &mut v[ei].prefix
            };
            let mut cur = head.as_deref_mut();
            while let Some(node) = cur {
                if node
                    .next
                    .as_ref()
                    .is_some_and(|nx| nx.flags & CLF_JOIN == 0)
                {
                    cur = node.next.as_deref_mut();
                } else {
                    node.next = None;
                    break;
                }
            }
        }
    }
    if !ls {
        setlens_vec(&mut v);
    }
    v
}

/// Which variable a brace re-insertion position is based on.
#[derive(Clone, Copy)]
enum Bb {
    Pcs,
    Scs,
}

/// Re-inserts the brace strings whose positions `li` has reached: from
/// `brbeg` going forwards, or from `brend` going backwards.
#[allow(clippy::too_many_arguments)]
fn brins(
    z: &mut Zle,
    beg: bool,
    cur: &mut Option<usize>,
    li: i32,
    bb: Bb,
    off: i32,
    sign: i32,
    cbr: Option<&mut i32>,
    pcs: &mut i32,
    scs: &mut i32,
    addscs: bool,
) {
    let mut cbr = cbr;
    while let Some(i) = *cur {
        let b = if beg {
            &z.comp.brbeg[i]
        } else {
            &z.comp.brend[i]
        };
        if li < b.curpos {
            break;
        }
        let s = b.str.clone().unwrap_or_default();
        let curpos = b.curpos;
        let ocs = cs(z);
        let bl = clen(&s);
        let base = match bb {
            Bb::Pcs => *pcs,
            Bb::Scs => *scs,
        };
        set_cs(z, base + off + sign * (li - curpos));
        if let Some(c) = cbr.as_deref_mut() {
            if *c < 0 {
                *c = cs(z);
            }
        }
        ins(z, &s, true, bl);
        set_cs(z, ocs + bl);
        *pcs += bl;
        if addscs {
            *scs += bl;
        }
        *cur = if beg {
            if i + 1 < z.comp.brbeg.len() {
                Some(i + 1)
            } else {
                None
            }
        } else if i > 0 {
            Some(i - 1)
        } else {
            None
        };
    }
}

/// Builds the unambiguous string. With `ins` 1 it is inserted in the
/// line; otherwise it is returned, with the cursor position in it in
/// `csp` and the positions of missing or ambiguous characters in `posl`
/// (line positions, braces included, when `ins` is 2).
pub fn cline_str(
    z: &mut Zle,
    l: &Cline,
    ins_: i32,
    csp: Option<&mut i32>,
    posl: Option<&mut Vec<i32>>,
) -> Option<String> {
    let mut posl = posl;
    let ocs = cs(z);
    let padd = if ins_ != 0 { wb() - ocs } else { -ocs };
    let (mut pcs, mut scs) = (0, 0);
    let (mut pmm, mut pma, mut smm, mut sma, mut dm) = (0u32, false, 0u32, false, 0u32);
    let (mut pm, mut pmax, mut sm, mut smax, mut d, mut mid, mut cbr) =
        (-1, -1, -1, -1, -1, -1, -1);
    let mut opos = -1;
    let mut li = 0;
    let mut brp: Option<usize> = None;
    let mut brs: Option<usize> = None;

    let lst = cut_cline(z, l);

    let addpos = |posl: &mut Option<&mut Vec<i32>>, npos: i32, opos: &mut i32| {
        if let Some(pl) = posl.as_deref_mut() {
            if npos != *opos {
                *opos = npos;
                pl.push(npos);
            }
        }
    };

    if ins_ != 0 {
        let mut wl = we() - wb();
        if !z.comp.brbeg.is_empty() {
            brp = Some(0);
            for bp in z.comp.brbeg.iter_mut() {
                bp.curpos = bp.qpos;
                wl -= olen(&bp.str);
            }
        }
        if !z.comp.brend.is_empty() {
            brs = Some(z.comp.brend.len() - 1);
            for bp in z.comp.brend.iter() {
                wl -= olen(&bp.str);
            }
            for bp in z.comp.brend.iter_mut() {
                bp.curpos = wl - bp.qpos;
            }
        }
        while let Some(i) = brp {
            if z.comp.brbeg[i].curpos != 0 {
                break;
            }
            let s = z.comp.brbeg[i].str.clone();
            ins_opt(z, &s, true, -1);
            brp = if i + 1 < z.comp.brbeg.len() {
                Some(i + 1)
            } else {
                None
            };
        }
        while let Some(i) = brs {
            if z.comp.brend[i].curpos != 0 {
                break;
            }
            if cbr < 0 {
                cbr = cs(z);
            }
            let s = z.comp.brend[i].str.clone();
            ins_opt(z, &s, true, -1);
            brs = if i > 0 { Some(i - 1) } else { None };
        }
    }

    for l in &lst {
        if l.olen != 0 && l.flags & CLF_SUF == 0 && l.prefix.is_none() {
            pcs = cs(z) + l.olen;
            ins_opt(z, &l.orig, true, l.olen);
        } else {
            let mut s = l.prefix.as_deref();
            while let Some(sn) = s {
                pcs = cs(z) + sn.llen;
                if sn.flags & CLF_LINE != 0 {
                    ins_opt(z, &sn.line, true, sn.llen);
                } else {
                    ins_opt(z, &sn.word, true, sn.wlen);
                }
                scs = cs(z);
                if sn.flags & CLF_DIFF != 0 && (dm == 0 || sn.flags & CLF_MATCHED != 0) {
                    d = cs(z);
                    dm = sn.flags & CLF_MATCHED;
                    addpos(&mut posl, cs(z) + padd, &mut opos);
                }
                li += sn.llen;
                s = sn.next.as_deref();
            }
        }
        if ins_ != 0 {
            brins(
                z,
                true,
                &mut brp,
                li,
                Bb::Pcs,
                0,
                -1,
                None,
                &mut pcs,
                &mut scs,
                true,
            );
        }
        if l.flags & CLF_MISS != 0 && l.flags & CLF_SUF == 0 {
            addpos(&mut posl, cs(z) + padd, &mut opos);
            if ((pmax <= (l.max - l.min) || (pma && l.max != l.min))
                && (pmm == 0 || l.flags & CLF_MATCHED != 0))
                || (l.flags & CLF_MATCHED != 0 && pmm == 0)
            {
                pm = cs(z);
                pmax = l.max - l.min;
                pmm = l.flags & CLF_MATCHED;
                pma = (l.prefix.is_some() || l.suffix.is_some()) && l.min == cline_sublen(l);
            }
        }
        if ins_ != 0 {
            brins(
                z,
                false,
                &mut brs,
                li,
                Bb::Scs,
                0,
                -1,
                Some(&mut cbr),
                &mut pcs,
                &mut scs,
                false,
            );
        }
        pcs = cs(z);
        if l.flags & CLF_LINE != 0 {
            ins_opt(z, &l.line, true, l.llen);
        } else {
            ins_opt(z, &l.word, true, l.wlen);
        }
        scs = cs(z);
        if ins_ != 0 {
            li += l.llen;
            brins(
                z,
                true,
                &mut brp,
                li,
                Bb::Pcs,
                l.llen,
                -1,
                None,
                &mut pcs,
                &mut scs,
                true,
            );
        }
        if l.flags & CLF_MISS != 0 {
            if l.flags & CLF_MID != 0 {
                mid = cs(z);
            } else if l.flags & CLF_SUF != 0 {
                addpos(&mut posl, cs(z) + padd, &mut opos);
                if ((smax <= (l.min - l.max) || (sma && l.max != l.min))
                    && (smm == 0 || l.flags & CLF_MATCHED != 0))
                    || (l.flags & CLF_MATCHED != 0 && smm == 0)
                {
                    sm = cs(z);
                    smax = l.min - l.max;
                    smm = l.flags & CLF_MATCHED;
                    sma = (l.prefix.is_some() || l.suffix.is_some()) && l.min == cline_sublen(l);
                }
            }
        }
        if ins_ != 0 {
            brins(
                z,
                false,
                &mut brs,
                li,
                Bb::Scs,
                0,
                -1,
                Some(&mut cbr),
                &mut pcs,
                &mut scs,
                false,
            );
        }
        if l.olen != 0 && l.flags & CLF_SUF != 0 && l.suffix.is_none() {
            pcs = cs(z);
            ins_opt(z, &l.orig, true, l.olen);
            if ins_ != 0 {
                li += l.olen;
                brins(
                    z,
                    true,
                    &mut brp,
                    li,
                    Bb::Pcs,
                    l.olen,
                    -1,
                    None,
                    &mut pcs,
                    &mut scs,
                    false,
                );
                brins(
                    z,
                    false,
                    &mut brs,
                    li,
                    Bb::Pcs,
                    l.olen,
                    -1,
                    Some(&mut cbr),
                    &mut pcs,
                    &mut scs,
                    false,
                );
            }
        } else {
            let mut j = -1;
            let mut i = 0;
            let mut jsm = 0u32;
            let mut s = l.suffix.as_deref();
            while let Some(sn) = s {
                if j < 0 && sn.flags & CLF_DIFF != 0 {
                    j = i;
                    jsm = sn.flags & CLF_MATCHED;
                }
                pcs = cs(z);
                if sn.flags & CLF_LINE != 0 {
                    ins_opt(z, &sn.line, false, sn.llen);
                    i += sn.llen;
                    scs = cs(z) + sn.llen;
                } else {
                    ins_opt(z, &sn.word, false, sn.wlen);
                    i += sn.wlen;
                    scs = cs(z) + sn.wlen;
                }
                if ins_ != 0 {
                    li += sn.llen;
                    brins(
                        z,
                        true,
                        &mut brp,
                        li,
                        Bb::Pcs,
                        0,
                        1,
                        None,
                        &mut pcs,
                        &mut scs,
                        true,
                    );
                    brins(
                        z,
                        false,
                        &mut brs,
                        li,
                        Bb::Scs,
                        0,
                        -1,
                        Some(&mut cbr),
                        &mut pcs,
                        &mut scs,
                        false,
                    );
                }
                s = sn.next.as_deref();
            }
            let c = cs(z) + i;
            set_cs(z, c);
            if j >= 0 && (dm == 0 || jsm != 0) {
                d = cs(z) - j;
                dm = jsm;
                addpos(&mut posl, cs(z) - j + padd, &mut opos);
            }
        }
    }
    addpos(&mut posl, cs(z) + padd, &mut opos);

    if ins_ != 0 {
        let ocs2 = cs(z);
        while let Some(i) = brp {
            let s = z.comp.brbeg[i].str.clone();
            ins_opt(z, &s, true, -1);
            brp = if i + 1 < z.comp.brbeg.len() {
                Some(i + 1)
            } else {
                None
            };
        }
        while let Some(i) = brs {
            if cbr < 0 {
                cbr = cs(z);
            }
            let s = z.comp.brend[i].str.clone();
            ins_opt(z, &s, true, -1);
            brs = if i > 0 { Some(i - 1) } else { None };
        }
        let delta = cs(z) - ocs2;
        for v in [&mut mid, &mut pm, &mut sm, &mut d] {
            if *v >= ocs2 {
                *v += delta;
            }
        }
        if let Some(pl) = posl {
            for p in pl.iter_mut() {
                if *p >= ocs2 {
                    *p += delta;
                }
            }
        }
    }
    let ncs = if mid >= 0 {
        mid
    } else if cbr >= 0 {
        cbr
    } else if pm >= 0 {
        pm
    } else if sm >= 0 {
        sm
    } else if d >= 0 {
        d
    } else {
        cs(z)
    };

    if ins_ != 1 {
        let i = cs(z) - ocs;
        let r = line_str(z, ocs, cs(z));
        set_cs(z, ocs);
        foredel(z, i);
        if let Some(c) = csp {
            *c = ncs - ocs;
        }
        return Some(r);
    }
    z.comp.lastend = cs(z);
    set_cs(z, ncs);
    None
}

/// The positions as a colon-separated string.
fn build_pos_string(list: &[i32]) -> String {
    list.iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(":")
}

/// The unambiguous string and its cursor position (counting from 1),
/// with the positions of missing characters in it and on the line: the
/// values behind compstate[unambiguous], [unambiguous_cursor],
/// [unambiguous_positions] and [insert_positions].
pub fn unambig_data(z: &mut Zle) -> (String, i32, String, String) {
    let mnum = z.comp.mnum;
    let have = UNAMBIG.with(|u| u.borrow().is_some());
    if mnum != 0 && z.comp.ainfo.is_some() {
        if mnum != z.comp.unambig_mnum {
            let line = {
                let ai = z.comp.ainfo.as_ref();
                if ai.is_some_and(|a| a.count != 0) {
                    ai.map(|a| a.line.clone()).unwrap_or_default()
                } else {
                    z.comp
                        .fainfo
                        .as_ref()
                        .map(|a| a.line.clone())
                        .unwrap_or_default()
                }
            };
            let mut list = Vec::new();
            let mut cc = 0;
            let s = cline_str(z, &line, 0, Some(&mut cc), Some(&mut list)).unwrap_or_default();
            let p = build_pos_string(&list);
            let mut list2 = Vec::new();
            cline_str(z, &line, 2, None, Some(&mut list2));
            let i = build_pos_string(&list2);
            UNAMBIG.with(|u| *u.borrow_mut() = Some((s, cc, p, i)));
        }
    } else if mnum != z.comp.unambig_mnum || z.comp.ainfo.is_none() || !have {
        UNAMBIG.with(|u| *u.borrow_mut() = Some((String::new(), 0, String::new(), String::new())));
    }
    z.comp.unambig_mnum = mnum;
    let (s, c, p, i) = UNAMBIG.with(|u| u.borrow().clone()).unwrap_or_default();
    (s, c + 1, p, i)
}

// ---- inserting matches --------------------------------------------------

/// Inserts the match; returns the number of characters inserted. `scs`
/// gets the position for an automatically added suffix.
fn instmatch(z: &mut Zle, m: &Cmatch, scs: Option<&mut i32>) -> i32 {
    let mut r = 0;
    let mut a = cs(z);
    let mut brb = 0;

    z.comp.lastprebr = None;
    z.comp.lastpostbr = None;

    for s in [&m.ipre, &m.pre, &m.ppre].into_iter().flatten() {
        r += ins(z, s, true, clen(s));
    }
    if let Some(s) = &m.str {
        r += ins(z, s, true, clen(s));
    }
    let mut ocs = cs(z);
    if !z.comp.brbeg.is_empty() {
        let mut pcs = cs(z);
        let mut bradd = olen(&m.pre);
        let brs: Vec<Option<String>> = z.comp.brbeg.iter().map(|b| b.str.clone()).collect();
        for (bs, brpos) in brs.iter().zip(m.brpl.iter()) {
            set_cs(z, a + brpos + bradd);
            pcs = cs(z);
            let l = olen(bs);
            bradd += l;
            z.comp.brpcs = cs(z);
            ins_opt(z, bs, true, l);
            r += l;
            ocs += l;
        }
        z.comp.lastprebr = Some(line_str(z, a, pcs));
        set_cs(z, ocs);
    }
    if let Some(s) = &m.psuf {
        r += ins(z, s, true, clen(s));
    }

    let scs_v = if !z.comp.brend.is_empty() {
        a = cs(z);
        let mut bradd = 0;
        let brs: Vec<Option<String>> = z.comp.brend.iter().map(|b| b.str.clone()).collect();
        for (k, bs) in brs.iter().enumerate() {
            let brpos = m.brsl.get(k).copied().unwrap_or(0);
            set_cs(z, a - brpos);
            ocs = cs(z);
            z.comp.brscs = ocs;
            let l = olen(bs);
            bradd += l;
            ins_opt(z, bs, true, l);
            brb = cs(z);
            r += l;
        }
        set_cs(z, a + bradd);
        ocs
    } else {
        z.comp.brscs = -1;
        cs(z)
    };
    if let Some(p) = scs {
        *p = scs_v;
    }
    if let Some(s) = &m.suf {
        r += ins(z, s, true, clen(s));
    }
    if let Some(s) = &m.isuf {
        r += ins(z, s, true, clen(s));
    }
    if !z.comp.brend.is_empty() {
        z.comp.lastpostbr = Some(line_str(z, brb, cs(z)));
    }
    z.comp.lastend = cs(z);
    set_cs(z, ocs);
    r
}

/// Whether the match, inserted, has `pre` before and `suf` after the
/// braces.
pub fn hasbrpsfx(z: &mut Zle, m: &Cmatch, pre: Option<&str>, suf: Option<&str>) -> bool {
    if m.flags & CMF_ALL != 0 {
        return true;
    }
    let op = z.comp.lastprebr.take();
    let os = z.comp.lastpostbr.take();
    let oline = z.line.clone();
    let (ocs, omark) = (z.cs, z.mark);
    let ole = z.comp.lastend;
    let (opcs, oscs) = (z.comp.brpcs, z.comp.brscs);

    instmatch(z, m, None);

    z.line = oline;
    z.cs = ocs;
    z.mark = omark;
    z.comp.lastend = ole;
    z.comp.brpcs = opcs;
    z.comp.brscs = oscs;

    let lp = z.comp.lastprebr.take();
    let lq = z.comp.lastpostbr.take();
    let ret = lp.as_deref() == pre && lq.as_deref() == suf;
    z.comp.lastprebr = op;
    z.comp.lastpostbr = os;
    ret
}

/// Handles more than one match.
pub fn do_ambiguous(z: &mut Zle) -> i32 {
    let mut ret = 0;
    z.comp.menucmp = 0;
    z.comp.menuacc = 0;

    // REC_EXACT: insert the exact match as if it were the only one.
    let exactm = z
        .comp
        .ainfo
        .as_ref()
        .filter(|a| a.exact == 1)
        .and_then(|a| a.exactm.clone());
    if let Some(em) = exactm.filter(|_| z.comp.fromcomp & FC_LINE == 0) {
        z.comp.minfo.cur = None;
        do_single(z, &em);
        invalidate_list(z);
        return ret;
    }
    z.comp.lastambig = 1;

    if z.comp.iforcemenu != -1
        && (z.comp.usemenu != 0
            || (z.comp.haspattern != 0 && z.comp.comppatinsert.as_deref() == Some("menu")))
    {
        do_ambig_menu(z);
    } else if let Some(ainfo_line) = z.comp.ainfo.as_ref().map(|a| a.line.clone()) {
        let (wb, we) = (wb(), we());
        let atend = cs(z) == we;
        z.comp.minfo.cur = None;
        z.comp.minfo.asked = 0;

        fixsuffix(z);

        let mut tcs = cs(z);
        set_cs(z, wb);
        let old = line_str(z, wb, we);
        foredel(z, we - wb);

        cline_str(z, &ainfo_line, 1, None, None);

        // Match specs can make the unambiguous string empty; then put
        // back what was there, unless -U matches were added.
        if z.comp.lastend < we && z.comp.lenchanged == 0 && z.comp.hasunmatched == 0 {
            set_cs(z, wb);
            let le = z.comp.lastend;
            foredel(z, le - wb);
            ins(z, &old, false, we - wb);
            z.comp.lastend = we;
            set_cs(z, tcs);
        }
        if z.comp.eparq != 0 {
            tcs = cs(z);
            let le = z.comp.lastend;
            set_cs(z, le);
            for _ in 0..z.comp.eparq {
                ins(z, "\"", false, 1);
            }
            set_cs(z, tcs);
        }
        let la = z.line.len() as i32 != z.comp.origll || {
            let ol: Vec<char> = z.comp.origline.as_deref().unwrap_or("").chars().collect();
            let n = z.line.len();
            ol.len() < n || ol[..n] != z.line[..]
        };

        z.comp.fromcomp = (if isset("automenu") { FC_LINE } else { 0 })
            | (if atend && cs(z) != z.comp.lastend {
                FC_INWORD
            } else {
                0
            });

        if z.comp.movetoend == 3 {
            let le = z.comp.lastend;
            set_cs(z, le);
        }

        // LIST_AMBIGUOUS: something was inserted, so no list this time.
        if (z.comp.uselist == 3
            || (z.comp.uselist == 0 && isset("bashautolist") && isset("listambiguous")))
            && la
            && z.comp.iforcemenu != -1
        {
            let fc = z.comp.fromcomp;
            invalidate_list(z);
            z.comp.fromcomp = fc;
            z.comp.lastambig = 0;
            z.clearlist = true;
            return ret;
        }
    } else {
        return ret;
    }

    if isset("listbeep") && z.comp.oldlist == 0 {
        ret = 1;
    }
    let ls = listshown();
    if z.comp.uselist != 0
        && (z.comp.usemenu != 2 || (ls == 0 && z.comp.oldlist == 0))
        && ((z.showinglist == 0 && (ls == 0 || z.comp.oldlist == 0))
            || (z.comp.usemenu == 3 && z.comp.oldlist == 0))
        && (z.comp.smatches >= 2 || z.comp.forcelist != 0)
    {
        z.showinglist = -2;
    }
    ret
}

/// stat() (or lstat() with `ls`), retrying with backslashes removed.
pub fn ztat(nam: &str, ls: bool) -> std::io::Result<std::fs::Metadata> {
    let st = |p: &str| {
        if ls {
            std::fs::symlink_metadata(p)
        } else {
            std::fs::metadata(p)
        }
    };
    match st(nam) {
        Ok(m) => Ok(m),
        Err(_) => {
            let mut out = String::new();
            let mut it = nam.chars().peekable();
            while let Some(c) = it.next() {
                if c == '\\' && it.peek().is_some() {
                    if let Some(n) = it.next() {
                        out.push(n);
                    }
                } else {
                    out.push(c);
                }
            }
            st(&out)
        }
    }
}

/// The match at the menu position (minfo.group, index `i`).
fn match_at(z: &Zle, i: usize) -> Option<CmatchRef> {
    let g = z.comp.minfo.group?;
    z.comp.amatches.get(g)?.matches.get(i).cloned()
}

/// Inserts all matches in the command line.
pub fn do_allmatches(z: &mut Zle, _end: i32) {
    let mut first = true;
    let mut nm = z.comp.nmatches - 1;
    let omc = z.comp.menucmp;
    let oma = z.comp.menuacc;
    let p = z.comp.brbeg.last().and_then(|b| b.str.clone());
    let mi = z.comp.minfo.clone();

    z.comp.menucmp = 1;
    z.comp.menuacc = 0;

    z.comp.minfo.group = z.comp.amatches.iter().position(|g| g.mcount != 0);
    let mut mc = z.comp.minfo.group.map(|_| 0usize);

    while let Some(mci) = mc {
        let Some(m) = match_at(z, mci) else {
            break;
        };
        if m.borrow().flags & CMF_ALL == 0 {
            if !first {
                accept_last(z);
            }
            first = false;
            if omc == 0 {
                nm -= 1;
                if nm == 0 {
                    z.comp.menucmp = 0;
                }
            }
            do_single(z, &m);
        }
        z.comp.minfo.cur = Some(mci);

        let Some(g) = z.comp.minfo.group else {
            break;
        };
        if mci + 1 >= z.comp.amatches[g].matches.len() {
            let ng = (g + 1..z.comp.amatches.len()).find(|&k| z.comp.amatches[k].mcount != 0);
            z.comp.minfo.group = ng;
            if ng.is_none() {
                break;
            }
            z.comp.minfo.cur = Some(0);
        } else {
            z.comp.minfo.cur = Some(mci + 1);
        }
        mc = z.comp.minfo.cur;
    }
    z.comp.menucmp = omc;
    z.comp.menuacc = oma;

    let e = z.comp.minfo.end;
    z.comp.minfo = mi;
    z.comp.minfo.end = e;
    z.comp.minfo.len = e - z.comp.minfo.pos;

    if let (Some(p), Some(b)) = (p, z.comp.brbeg.last_mut()) {
        b.str = Some(p);
    }
}

/// Is the parameter named in `p` (`$name` or `${name...}`) something
/// other than a plain scalar?
fn param_not_scalar(p: &str) -> bool {
    let n = if let Some(r) = p.strip_prefix("${") {
        r.strip_suffix('}').unwrap_or(r)
    } else {
        &p[1..]
    };
    crate::shell::with_shell(|sh| {
        sh.param(n).is_some_and(|pm| {
            !matches!(pm.value, crate::params::Value::Scalar(_))
                || pm.attrs & crate::params::NUMERIC != 0
        })
    })
}

/// Inserts a single match in the command line.
pub fn do_single(z: &mut Zle, mr: &CmatchRef) {
    let m = mr.borrow().clone();
    let mut sr = 0;
    let mut scs = 0;
    let mut havesuff = false;
    let partest = m.ripre.is_some() || (m.flags & CMF_ISPAR != 0 && z.comp.parpre.is_some());
    let str = m.orig.clone().unwrap_or_default();
    let psuf = m.psuf.clone().unwrap_or_default();
    let prpre = m.prpre.clone().unwrap_or_default();

    fixsuffix(z);

    if z.comp.minfo.cur.is_none() {
        // Not in a menu completion yet.
        z.comp.minfo.pos = wb();
        let mte = z.comp.movetoend;
        z.comp.minfo.we =
            (mte >= 2 || (mte == 1 && z.comp.menucmp == 0) || (mte == 0 && cs(z) == we())) as i32;
        z.comp.minfo.end = we();
    }
    let l = if z.comp.minfo.cur.is_some() {
        z.comp.minfo.len + z.comp.minfo.insc
    } else {
        we() - wb()
    };
    z.comp.minfo.insc = 0;
    let pos = z.comp.minfo.pos;
    set_cs(z, pos);
    foredel(z, l);

    if m.flags & CMF_ALL != 0 {
        do_allmatches(z, 0);
        return;
    }

    z.comp.minfo.len = instmatch(z, &m, Some(&mut scs));
    z.comp.minfo.end = cs(z);
    let c = z.comp.minfo.pos + z.comp.minfo.len;
    set_cs(z, c);

    if let Some(suf) = &m.suf {
        havesuff = true;
        z.comp.minfo.insc = clen(suf);
        z.comp.minfo.len -= z.comp.minfo.insc;
        if z.comp.minfo.we != 0 {
            z.comp.minfo.end += z.comp.minfo.insc;
            if m.flags & CMF_REMOVE != 0 {
                let wsuf: Vec<char> = suf.chars().collect();
                let len = wsuf.len() as i32;
                makesuffixstr(z, m.remf.as_deref(), m.rems.as_deref(), len);
                if len == 1 {
                    addsuffix_posstr(z, &wsuf, 1, 1);
                }
            }
        }
    } else {
        // No suffix from compadd: make one up.
        set_cs(z, scs);
        if partest && m.flags & CMF_PARBR != 0 {
            let c = cs(z) + z.comp.eparq;
            set_cs(z, c);
            for _ in 0..z.comp.parq {
                ins(z, "\"", true, 1);
            }
            z.comp.minfo.insc += z.comp.parq;
            ins(z, "}", true, 1);
            z.comp.minfo.insc += 1;
            if z.comp.minfo.we != 0 {
                z.comp.minfo.end += z.comp.minfo.insc;
            }
            if m.flags & CMF_PARNEST != 0 {
                havesuff = true;
            }
        }
        if (m.flags & CMF_FILE != 0 || (partest && isset("autoparamslash")))
            && z.cs > 0
            && z.line.get(z.cs - 1) != Some(&'/')
        {
            // A file or parameter naming a directory gets a slash.
            let t;
            if m.ipre.as_deref() == Some("~") {
                t = true;
            } else {
                let p = if partest && psuf.is_empty() && m.flags & CMF_PARNEST == 0 {
                    let pre = if m.flags & CMF_ISPAR != 0 {
                        z.comp.parpre.clone()
                    } else {
                        m.ripre.clone()
                    }
                    .unwrap_or_default();
                    let p = format!(
                        "{}{}{}",
                        pre,
                        str,
                        if m.flags & CMF_PARBR != 0 { "}" } else { "" }
                    );
                    let tryit = !(p.starts_with('$') && param_not_scalar(&p));
                    if tryit {
                        let e = z.shell(|| crate::expand::expand_string(&p));
                        z.errflag = false;
                        e.unwrap_or(p)
                    } else {
                        p
                    }
                } else {
                    format!(
                        "{}{}{}",
                        if prpre.is_empty() {
                            "./"
                        } else {
                            prpre.as_str()
                        },
                        str,
                        psuf
                    )
                };
                match ztat(&p, false) {
                    Ok(md) => t = md.is_dir(),
                    Err(_) => {
                        sr = -1;
                        t = false;
                    }
                }
            }
            if t {
                havesuff = true;
                ins(z, "/", true, 1);
                z.comp.minfo.insc += 1;
                if z.comp.minfo.we != 0 {
                    z.comp.minfo.end += 1;
                }
                if z.comp.menucmp == 0 || z.comp.minfo.we != 0 {
                    if m.remf.is_some() || m.rems.is_some() {
                        makesuffixstr(z, m.remf.as_deref(), m.rems.as_deref(), 1);
                    } else if isset("autoremoveslash") {
                        makesuffix(z, 1);
                        addsuffix_posstr(z, &['/'], 1, 1);
                    }
                }
            }
        }
        if z.comp.minfo.insc == 0 {
            let c = z.comp.minfo.pos + z.comp.minfo.len - m.qisl;
            set_cs(z, c);
        }
    }
    if !z.comp.brbeg.is_empty() {
        if havesuff {
            // A removable suffix is also removed by `,' and `}'.
            if isset("autoparamkeys") {
                let sl = crate::zle::comp::tricky::suffixlen(z);
                addsuffix_posstr(z, &[',', '}'], 2, sl);
            }
        } else if z.comp.menucmp == 0 {
            set_cs(z, scs);
            ins(z, ",", true, 1);
            z.comp.minfo.insc += 1;
            makesuffix(z, 1);
            if (z.comp.menucmp == 0 || z.comp.minfo.we != 0) && isset("autoparamkeys") {
                addsuffix_posstr(z, &[',', '}'], 2, 1);
            }
        }
    } else if !havesuff && (m.flags & CMF_FILE == 0 || sr == 0) {
        // A space, unless menu completing or completing a file name that
        // does not exist.
        if let Some(aq) = &m.autoq {
            if m.isuf
                .as_deref()
                .is_none_or(|s| !s.starts_with(aq.as_str()))
            {
                let al = clen(aq);
                ins(z, aq, true, al);
                z.comp.minfo.insc += al;
            }
        }
        if z.comp.menucmp == 0
            && m.flags & CMF_NOSPACE == 0
            && (z.comp.usemenu != 3 || z.comp.insspace != 0)
        {
            ins(z, " ", true, 1);
            z.comp.minfo.insc += 1;
            if z.comp.minfo.we != 0 {
                makesuffixstr(z, m.remf.as_deref(), m.rems.as_deref(), 1);
            }
        }
    }
    if z.comp.minfo.we != 0
        && partest
        && isset("autoparamkeys")
        && z.comp.minfo.insc - z.comp.parq > 0
    {
        let n = z.comp.minfo.insc - z.comp.parq;
        makeparamsuffix(z, m.flags & CMF_PARBR != 0, n);
    }

    if (z.comp.menucmp != 0 && z.comp.minfo.we == 0) || z.comp.movetoend == 0 {
        let mut c = z.comp.minfo.end;
        if c + m.qisl == z.comp.lastend {
            c += z.comp.minfo.insc;
        }
        // Past compadd -s/-S suffixes.
        c += clen(&psuf) + olen(&m.suf);
        set_cs(z, c);
    }
}

/// The match at `m` in minfo.group, or the next one there (or in the
/// following groups, wrapping around) that should not be skipped. With
/// `next`, moves at least once; zmult gives the direction.
fn valid_match(z: &mut Zle, m: usize, next: bool) -> usize {
    let mut m = m;
    let mut next = next;
    loop {
        let skip = next || {
            let Some(mr) = match_at(z, m) else {
                return m;
            };
            let mm = mr.borrow().clone();
            (z.comp.menuacc != 0 && {
                let (pre, post) = (z.comp.minfo.prebr.clone(), z.comp.minfo.postbr.clone());
                !hasbrpsfx(z, &mm, pre.as_deref(), post.as_deref())
            }) || mm.flags & CMF_DUMMY != 0
                || (mm.flags & (CMF_NOLIST | CMF_MULT) != 0
                    && mm.str.as_deref().is_none_or(str::is_empty))
        };
        if !skip {
            return m;
        }
        let Some(mut g) = z.comp.minfo.group else {
            return m;
        };
        let ng = z.comp.amatches.len();
        if z.zmult() > 0 {
            m += 1;
            if m >= z.comp.amatches[g].matches.len() {
                loop {
                    g += 1;
                    if g >= ng {
                        g = 0;
                    }
                    if z.comp.amatches[g].mcount != 0 {
                        break;
                    }
                }
                z.comp.minfo.group = Some(g);
                m = 0;
            }
        } else if m == 0 {
            loop {
                g = if g == 0 { ng - 1 } else { g - 1 };
                if z.comp.amatches[g].mcount != 0 {
                    break;
                }
            }
            z.comp.minfo.group = Some(g);
            m = (z.comp.amatches[g].mcount - 1).max(0) as usize;
        } else {
            m -= 1;
        }
        next = false;
    }
}

/// Menu completion already running: list the matches, or insert the
/// next one.
pub fn do_menucmp(z: &mut Zle, lst: i32) {
    if lst == COMP_LIST_COMPLETE {
        z.showinglist = -2;
        return;
    }
    let Some(mut cur) = z.comp.minfo.cur else {
        return;
    };
    while z.zmod.mult != 0 {
        cur = valid_match(z, cur, true);
        z.comp.minfo.cur = Some(cur);
        z.zmod.mult -= z.zmod.mult.signum();
    }
    if let Some(m) = match_at(z, cur) {
        do_single(z, &m);
    }
}

/// Accepts the current menu match and starts a new word with the next
/// matches.
pub fn accept_last(z: &mut Zle) -> i32 {
    if z.comp.menuacc == 0 {
        z.comp.minfo.prebr = z.comp.lastprebr.clone();
        z.comp.minfo.postbr = z.comp.lastpostbr.clone();

        if listshown() != 0 && (z.comp.lastprebr.is_some() || z.comp.lastpostbr.is_some()) {
            let (pre, post) = (z.comp.minfo.prebr.clone(), z.comp.minfo.postbr.clone());
            'groups: for gi in 0..z.comp.amatches.len() {
                let ms = z.comp.amatches[gi].matches.clone();
                for m in ms {
                    let mm = m.borrow().clone();
                    if !hasbrpsfx(z, &mm, pre.as_deref(), post.as_deref()) {
                        z.showinglist = -2;
                        break 'groups;
                    }
                }
            }
        }
    }
    z.comp.menuacc += 1;

    if !z.comp.brbeg.is_empty() {
        iremovesuffix(z, ',');
        let l = (if z.comp.brscs >= 0 {
            z.comp.brscs
        } else {
            cs(z)
        }) - z.comp.brpcs;
        let mut s = line_str(z, z.comp.brpcs, z.comp.brpcs + l);
        s.push(',');
        if let Some(b) = z.comp.brbeg.last_mut() {
            b.str = Some(s);
        }
    } else {
        let mi = &z.comp.minfo;
        let base = mi.pos + mi.len + mi.insc;
        set_cs(z, base);
        iremovesuffix(z, ' ');
        let l = cs(z);
        let qisl = z
            .comp
            .minfo
            .cur
            .and_then(|c| match_at(z, c))
            .map_or(0, |m| m.borrow().qisl);
        let target = base - qisl;
        if target < l {
            set_cs(z, target);
            foredel(z, l - target);
        } else {
            let c = target.min(z.line.len() as i32);
            set_cs(z, c);
        }
        ins(z, " ", true, 1);
        z.comp.minfo.insc = 0;
        z.comp.minfo.len = 0;
        z.comp.minfo.pos = cs(z);
        z.comp.minfo.we = 1;
    }
    0
}

/// Maps `v` into 0..m: positive values count from 1, negative ones from
/// the end.
fn comp_mod(v: i32, m: i32) -> i32 {
    if m <= 0 {
        return 0;
    }
    let mut v = v;
    if v >= 0 {
        v -= 1;
    }
    if v >= 0 {
        v % m
    } else {
        while v < 0 {
            v += m;
        }
        v
    }
}

/// Starts menu completion.
pub fn do_ambig_menu(z: &mut Zle) {
    if z.comp.iforcemenu == -1 {
        do_ambiguous(z);
    }
    if z.comp.usemenu != 3 {
        z.comp.menucmp = 1;
        z.comp.menuacc = 0;
        z.comp.minfo.cur = None;
    } else if z.comp.oldlist != 0 {
        if z.comp.oldins != 0 && z.comp.minfo.cur.is_some() {
            accept_last(z);
        }
    } else {
        z.comp.minfo.cur = None;
    }
    z.comp.insmnum = comp_mod(z.comp.insmnum, z.comp.lastpermmnum);
    let mut g = 0;
    while g < z.comp.amatches.len() && z.comp.amatches[g].mcount <= z.comp.insmnum {
        z.comp.insmnum -= z.comp.amatches[g].mcount;
        g += 1;
    }
    if g >= z.comp.amatches.len() {
        z.comp.minfo.group = None;
        z.comp.minfo.cur = None;
        z.comp.minfo.asked = 0;
        return;
    }
    z.comp.minfo.group = Some(g);
    let im = z.comp.insmnum as usize;
    let mc = valid_match(z, im, false);
    if z.comp.iforcemenu != -1 {
        if let Some(m) = match_at(z, mc) {
            do_single(z, &m);
        }
    }
    z.comp.minfo.cur = Some(mc);
}

/// The number of screen lines the list needs (compstate[list_lines]).
pub fn list_lines(z: &mut Zle) -> i64 {
    crate::zle::comp::core::permmatches(z, false);

    let pm = z.comp.pmatches.clone();
    let oam = std::mem::replace(&mut z.comp.amatches, pm);
    z.comp.listdat.valid = 0;
    calclist(z, false);
    z.comp.listdat.valid = 0;
    // zsh's amatches was pmatches itself, so the layout stays there.
    z.comp.pmatches = std::mem::replace(&mut z.comp.amatches, oam);

    z.comp.listdat.nlines as i64
}

/// Sets compstate[list].
pub fn comp_list(z: &mut Zle, v: Option<String>) {
    z.comp.onlyexpl = v.as_deref().map_or(0, |v| {
        (if v.contains("expl") { 1 } else { 0 }) | (if v.contains("messages") { 2 } else { 0 })
    });
    z.comp.complist = v;
}

// ---- listing ------------------------------------------------------------

/// The first match from index `p` on in group `g` that is listed.
pub fn skipnolist(g: &Cmgroup, p: usize, showall: bool) -> usize {
    let mask = (if showall { 0 } else { CMF_NOLIST | CMF_MULT }) | CMF_HIDE;
    let mut p = p;
    while let Some(m) = g.matches.get(p) {
        let m = m.borrow();
        if m.flags & mask != 0 || (m.disp.is_some() && m.flags & (CMF_DISPLINE | CMF_HIDE) != 0) {
            p += 1;
        } else {
            break;
        }
    }
    p
}

/// Lays out the list: columns, widths and lines of each group, and the
/// totals in listdat. Returns 0 if the old layout is still valid.
pub fn calclist(z: &mut Zle, showall: bool) -> i32 {
    let zcols = zterm_columns();
    let zlines = zterm_lines();
    {
        let ld = &z.comp.listdat;
        if LASTINVCOUNT.with(|c| c.get()) == z.comp.invcount
            && ld.valid != 0
            && z.comp.onlyexpl == ld.onlyexpl
            && z.comp.menuacc == ld.menuacc
            && showall as i32 == ld.showall
            && zlines == ld.zterm_lines
            && zcols == ld.zterm_columns
        {
            return 0;
        }
    }
    LASTINVCOUNT.with(|c| c.set(z.comp.invcount));

    let onlyexpl = z.comp.onlyexpl;
    let (mut hidden, mut nlist, mut nlines, mut max) = (0, 0, 0, 0);
    let mut mlens: HashMap<i32, i32> = HashMap::new();

    for gi in 0..z.comp.amatches.len() {
        let mut nl = false;
        let (mut glong, mut gshort, mut ndisp, mut totl) = (1, zcols, 0, 0);
        let mut hasf = false;
        let mut gflags = z.comp.amatches[gi].flags | CGF_PACKED | CGF_ROWS;
        let ylist = z.comp.amatches[gi].ylist.clone();

        if onlyexpl == 0 && !ylist.is_empty() {
            if !isset("listpacked") {
                gflags &= !CGF_PACKED;
            }
            if !isset("listrowsfirst") {
                gflags &= !CGF_ROWS;
            }
            hidden = 1;
            for s in &ylist {
                if nl {
                    break;
                }
                nl = strwidth(s) >= zcols || s.contains('\n');
            }
            if nl || ylist.len() < 2 {
                gflags |= CGF_LINES;
                for s in &ylist {
                    let mut sptr: &str = s;
                    while !sptr.is_empty() {
                        if let Some(pos) = sptr.find('\n') {
                            nlines += 1 + (strwidth(&sptr[..pos]) - 1) / zcols;
                            sptr = &sptr[pos + 1..];
                        } else {
                            nlines += (strwidth(sptr) - 1) / zcols;
                            break;
                        }
                    }
                    nlines += 1;
                }
            } else {
                for s in &ylist {
                    let l = strwidth(s);
                    ndisp += 1;
                    glong = glong.max(l);
                    gshort = gshort.min(l);
                    totl += l;
                    nlist += 1;
                }
            }
        } else if onlyexpl == 0 {
            let ms = z.comp.amatches[gi].matches.clone();
            for mr in ms {
                let mm = mr.borrow().clone();
                if mm.flags & CMF_FILE != 0 {
                    hasf = true;
                }
                if z.comp.menuacc != 0 {
                    let (pre, post) = (z.comp.minfo.prebr.clone(), z.comp.minfo.postbr.clone());
                    if !hasbrpsfx(z, &mm, pre.as_deref(), post.as_deref()) {
                        mr.borrow_mut().flags |= CMF_HIDE;
                        continue;
                    }
                }
                mr.borrow_mut().flags &= !CMF_HIDE;

                if showall || mm.flags & (CMF_NOLIST | CMF_MULT) == 0 {
                    if mm.flags & (CMF_NOLIST | CMF_MULT) != 0
                        && mm.str.as_deref().is_none_or(str::is_empty)
                    {
                        mr.borrow_mut().flags |= CMF_HIDE;
                        continue;
                    }
                    if let Some(disp) = &mm.disp {
                        if mm.flags & CMF_DISPLINE != 0 {
                            nlines += 1 + printfmt(z, disp, 0, 0, 0);
                            gflags |= CGF_HASDL;
                        } else {
                            let l = nicewidth(disp) + (mm.modec != '\0') as i32;
                            ndisp += 1;
                            glong = glong.max(l);
                            gshort = gshort.min(l);
                            totl += l;
                            mlens.insert(mm.gnum, l);
                        }
                    } else {
                        let l =
                            nicewidth(mm.str.as_deref().unwrap_or("")) + (mm.modec != '\0') as i32;
                        ndisp += 1;
                        glong = glong.max(l);
                        gshort = gshort.min(l);
                        totl += l;
                        mlens.insert(mm.gnum, l);
                    }
                    nlist += 1;
                    if mm.flags & CMF_PACKED == 0 {
                        gflags &= !CGF_PACKED;
                    }
                    if mm.flags & CMF_ROWS == 0 {
                        gflags &= !CGF_ROWS;
                    }
                } else {
                    hidden = 1;
                }
            }
        }
        let expls = z.comp.amatches[gi].expls.clone();
        for e in &expls {
            if (e.count != 0 || e.always)
                && (onlyexpl == 0 || onlyexpl & (if e.always { 2 } else { 1 }) != 0)
            {
                let s = e.str.clone().unwrap_or_default();
                nlines += 1 + printfmt(z, &s, if e.always { -1 } else { e.count }, 0, 1);
            }
        }
        if isset("listtypes") && hasf {
            gflags |= CGF_FILES;
        }
        let g = &mut z.comp.amatches[gi];
        g.flags = gflags;
        g.totl = totl + ndisp * CM_SPACE;
        g.dcount = ndisp;
        g.width = glong + CM_SPACE;
        g.shortest = gshort + CM_SPACE;
        g.cols = (zcols / g.width).min(g.dcount);
        if g.cols != 0 {
            let i = g.cols * g.width - CM_SPACE;
            if i > max {
                max = i;
            }
        }
    }

    let ml = |gnum: i32| mlens.get(&gnum).copied().unwrap_or(0);
    if onlyexpl == 0 {
        for gi in 0..z.comp.amatches.len() {
            let mut glines = 0;
            let g = &mut z.comp.amatches[gi];
            g.widths.clear();
            if !g.ylist.is_empty() {
                if g.flags & CGF_LINES == 0 {
                    if g.cols != 0 {
                        glines += (g.ylist.len() as i32 + g.cols - 1) / g.cols;
                        if g.cols > 1 {
                            g.width += (max - (g.width * g.cols - CM_SPACE)) / g.cols;
                        }
                    } else {
                        g.cols = 1;
                        g.width = 1;
                        for s in &g.ylist {
                            glines += 1 + strwidth(s) / zcols;
                        }
                    }
                }
            } else if g.cols != 0 {
                glines += (g.dcount + g.cols - 1) / g.cols;
                if g.cols > 1 {
                    g.width += (max - (g.width * g.cols - CM_SPACE)) / g.cols;
                }
            } else if g.flags & CGF_LINES == 0 {
                g.cols = 1;
                g.width = 0;
                for mr in &g.matches {
                    let m = mr.borrow();
                    if m.flags & CMF_HIDE == 0 {
                        if m.disp.is_some() {
                            if m.flags & CMF_DISPLINE == 0 {
                                glines += 1 + ((ml(m.gnum) - 1) / zcols);
                            }
                        } else if showall || m.flags & (CMF_NOLIST | CMF_MULT) == 0 {
                            glines += 1 + ((ml(m.gnum) - 1) / zcols);
                        }
                    }
                }
            }
            g.lins = glines;
            nlines += glines;
        }
        for gi in 0..z.comp.amatches.len() {
            let g = &mut z.comp.amatches[gi];
            if g.flags & CGF_PACKED == 0 {
                continue;
            }
            let mut ws = vec![0i32; zcols as usize];
            let mut tlines = g.lins;
            let mut tcols = g.cols;
            let mut width = 0;
            let rows = g.flags & CGF_ROWS != 0;

            if !g.ylist.is_empty() {
                if g.flags & CGF_LINES == 0 {
                    let ylens: Vec<i32> = g.ylist.iter().map(|s| strwidth(s) + CM_SPACE).collect();
                    let yl = |n: i32| ylens.get(n as usize).copied().unwrap_or(0);
                    tcols = zcols / (g.shortest + CM_SPACE);
                    while tcols > g.cols {
                        ws[..tcols as usize].iter_mut().for_each(|w| *w = 0);
                        let (mut nth, mut tcol, mut tline) = (0, 0, 0);
                        width = 0;
                        if rows {
                            tlines = 1;
                        } else {
                            tlines = (g.dcount + tcols - 1) / tcols;
                            if tlines <= 0 {
                                tlines = 1;
                            }
                        }
                        while width < zcols && nth < g.dcount {
                            if rows {
                                if tcol == tcols {
                                    tcol = 0;
                                    tlines += 1;
                                }
                            } else {
                                if tline == tlines {
                                    tcol += 1;
                                    tline = 0;
                                }
                                if tcol == tcols {
                                    tcol = 0;
                                    tlines += 1;
                                }
                            }
                            let len = yl(nth);
                            let w = &mut ws[tcol as usize];
                            if len > *w {
                                width += len - *w;
                                *w = len;
                            }
                            nth += 1;
                            if rows {
                                tcol += 1;
                            } else {
                                tline += 1;
                            }
                        }
                        if width < zcols {
                            break;
                        }
                        tcols -= 1;
                    }
                }
            } else if g.width != 0 {
                tcols = zcols / (g.shortest + CM_SPACE);
                while tcols > g.cols {
                    ws[..tcols as usize].iter_mut().for_each(|w| *w = 0);
                    let (mut nth, mut tcol, mut tline) = (0, 0, 0);
                    width = 0;
                    if rows {
                        tlines = 1;
                    } else {
                        tlines = (g.dcount + tcols - 1) / tcols;
                        if tlines <= 0 {
                            tlines = 1;
                        }
                    }
                    let mut p = skipnolist(g, 0, showall);
                    while p < g.matches.len() && width < zcols && nth < g.dcount {
                        let gnum = g.matches[p].borrow().gnum;
                        if rows {
                            if tcol == tcols {
                                tcol = 0;
                                tlines += 1;
                            }
                        } else {
                            if tline == tlines {
                                tcol += 1;
                                tline = 0;
                            }
                            if tcol == tcols {
                                tcol = 0;
                                tlines += 1;
                            }
                        }
                        let len = ml(gnum) + if tcol == tcols - 1 { 0 } else { CM_SPACE };
                        let w = &mut ws[tcol as usize];
                        if len > *w {
                            width += len - *w;
                            *w = len;
                        }
                        nth += 1;
                        p = skipnolist(g, p + 1, showall);
                        if rows {
                            tcol += 1;
                        } else {
                            tline += 1;
                        }
                    }
                    if width < zcols {
                        if !rows {
                            tcol += 1;
                            if tcol < tcols {
                                tcols = tcol;
                            }
                        }
                        break;
                    }
                    tcols -= 1;
                }
            }
            if tcols <= g.cols {
                tlines = g.lins;
            }
            if tlines != g.lins {
                nlines += tlines - g.lins;
                g.lins = tlines;
                g.cols = tcols;
                g.totl = width;
                width -= CM_SPACE;
                if width > max {
                    max = width;
                }
                g.widths = ws;
            }
        }
        for g in z.comp.amatches.iter_mut() {
            if !g.widths.is_empty() {
                let a = (max - g.totl + CM_SPACE) / g.cols.max(1);
                for w in g.widths.iter_mut().take(g.cols.max(0) as usize) {
                    *w += a;
                }
            } else if g.width != 0 && g.cols > 1 {
                g.width += (max - (g.width * g.cols - CM_SPACE)) / g.cols;
            }
        }
    } else {
        for g in z.comp.amatches.iter_mut() {
            g.widths.clear();
        }
    }
    let ld = &mut z.comp.listdat;
    ld.valid = 1;
    ld.hidden = hidden;
    ld.nlist = nlist;
    ld.nlines = nlines;
    ld.menuacc = z.comp.menuacc;
    ld.onlyexpl = onlyexpl;
    ld.zterm_columns = zcols;
    ld.zterm_lines = zlines;
    ld.showall = showall as i32;
    1
}

/// Asks whether to show a long list; non-zero means do not list.
pub fn asklist(z: &mut Zle) -> i32 {
    z.trash();
    z.showinglist = 0;
    set_listshown(z, 0);

    set_clearflag((isset("zle") && z.comp.dolastprompt != 0) as i32);
    set_lastlistlen(0);

    let zcols = zterm_columns();
    let ld = z.comp.listdat.clone();
    let clm = z.comp.complistmax;
    if (z.comp.minfo.cur.is_none() || z.comp.minfo.asked == 0)
        && ((clm > 0 && ld.nlist as i64 >= clm)
            || (clm < 0 && ld.nlines as i64 <= -clm)
            || (clm == 0 && ld.nlines >= zterm_lines()))
    {
        let msg = if ld.nlist > 0 {
            format!(
                "zsh: do you wish to see all {} possibilities ({} lines)? ",
                ld.nlist, ld.nlines
            )
        } else {
            format!("zsh: do you wish to see all {} lines? ", ld.nlines)
        };
        write_tty(&msg);
        let l = clen(&msg);
        let qup = ((l + zcols - 1) / zcols) - 1;
        flush_tty();
        let nlnct = z.refresh.nlnct as i32;
        if !z.getzlequery() {
            if clearflag() != 0 {
                write_tty("\r");
                tcup(qup);
                cleareod();
                tcup(nlnct);
            } else {
                write_tty("\n");
            }
            z.comp.minfo.asked = 2;
            return 1;
        }
        if clearflag() != 0 {
            write_tty("\r");
            tcup(qup);
            cleareod();
        } else {
            write_tty("\n");
        }
        z.comp.minfo.asked = 1;
    } else if z.comp.minfo.asked == 2 {
        tcup(z.refresh.nlnct as i32);
    }
    if z.comp.minfo.asked != 0 {
        z.comp.minfo.asked - 1
    } else {
        0
    }
}

/// A newline in the list; past the lines counted down in `cl`, clear
/// the rest of the screen.
fn listnl(cl: &mut i32) {
    write_tty("\n");
    if *cl >= 0 {
        *cl -= 1;
        if *cl <= 1 {
            *cl = -1;
            cleareod();
        }
    }
}

fn printlist(z: &mut Zle, over: bool, printm: PrintFunc, showall: bool) -> i32 {
    let mut pnl = false;
    let mut cl = if over { z.comp.listdat.nlines } else { -1 };
    let mut ml = 0;
    let mut printed = 0;
    let zcols = zterm_columns();
    let onlyexpl = z.comp.listdat.onlyexpl;

    if cl < 2 {
        cl = -1;
        cleareod();
    }
    for gi in 0..z.comp.amatches.len() {
        let expls = z.comp.amatches[gi].expls.clone();
        for e in &expls {
            if (e.count != 0 || e.always)
                && (onlyexpl == 0 || onlyexpl & (if e.always { 2 } else { 1 }) != 0)
            {
                if pnl {
                    ml += 1;
                    listnl(&mut cl);
                }
                let s = e.str.clone().unwrap_or_default();
                let l = printfmt(z, &s, if e.always { -1 } else { e.count }, 1, 1);
                ml += l;
                if cl >= 0 {
                    cl -= l;
                    if cl <= 1 {
                        cl = -1;
                        cleareod();
                    }
                }
                pnl = true;
            }
        }
        let g = z.comp.amatches[gi].clone();
        if onlyexpl == 0 && !g.ylist.is_empty() {
            if pnl {
                pnl = false;
                ml += 1;
                listnl(&mut cl);
            }
            if g.flags & CGF_LINES != 0 {
                for (k, p) in g.ylist.iter().enumerate() {
                    write_tty(p);
                    if k + 1 < g.ylist.len() {
                        if strwidth(p) % zcols != 0 {
                            write_tty("\n");
                        } else {
                            write_tty(" \x08");
                        }
                    }
                }
            } else {
                let mut n = g.lcount;
                let mut nl = g.lins;
                let nc = g.lins;
                let mut pp: i32 = 0;
                while n != 0 && {
                    let t = nl;
                    nl -= 1;
                    t != 0
                } {
                    let mut i = g.cols;
                    let mut mc = 0;
                    let mut pq = pp;
                    while n != 0 && {
                        let t = i;
                        i -= 1;
                        t != 0
                    } {
                        if pq >= g.lcount {
                            break;
                        }
                        let s = g.ylist.get(pq as usize).cloned().unwrap_or_default();
                        write_tty(&s);
                        if i != 0 {
                            let w = if g.widths.is_empty() {
                                g.width
                            } else {
                                g.widths.get(mc as usize).copied().unwrap_or(0)
                            };
                            let a = w - strwidth(&s);
                            if a > 0 {
                                write_tty(&" ".repeat(a as usize));
                            }
                        }
                        pq += if g.flags & CGF_ROWS != 0 { 1 } else { nc };
                        mc += 1;
                        n -= 1;
                    }
                    if n != 0 {
                        ml += 1;
                        listnl(&mut cl);
                    }
                    pp += if g.flags & CGF_ROWS != 0 { g.cols } else { 1 };
                }
            }
        } else if onlyexpl == 0 && (g.lcount != 0 || (showall && g.mcount != 0)) {
            let mut n = g.dcount;
            let mut nl = g.lins;
            let nc = g.lins;
            let wid_at = |mc: i32| {
                if g.widths.is_empty() {
                    g.width
                } else {
                    g.widths.get(mc as usize).copied().unwrap_or(0)
                }
            };

            if g.flags & CGF_HASDL != 0 {
                for (pi, mr) in g.matches.iter().enumerate() {
                    let show = {
                        let m = mr.borrow();
                        m.disp.is_some()
                            && m.flags & CMF_DISPLINE != 0
                            && (showall || m.flags & (CMF_HIDE | CMF_NOLIST) == 0)
                    };
                    if show {
                        if pnl {
                            ml += 1;
                            listnl(&mut cl);
                        }
                        printed += 1;
                        printm(z, gi, Some(pi), 0, ml, true, 0);
                        pnl = true;
                    }
                }
            }
            if n != 0 && pnl {
                pnl = false;
                ml += 1;
                listnl(&mut cl);
            }
            let len = g.matches.len();
            let mut p = skipnolist(&g, 0, showall);
            while n != 0 && {
                let t = nl;
                nl -= 1;
                t != 0
            } {
                let mut i = g.cols;
                let mut mc = 0;
                let mut q = p;
                while n != 0 && {
                    let t = i;
                    i -= 1;
                    t != 0
                } {
                    let wid = wid_at(mc);
                    if q >= len {
                        printm(z, gi, None, mc, ml, i == 0, wid);
                        break;
                    }
                    printm(z, gi, Some(q), mc, ml, i == 0, wid);
                    printed += 1;

                    n -= 1;
                    if n != 0 {
                        let mut j = if g.flags & CGF_ROWS != 0 { 1 } else { nc };
                        while j != 0 && q < len {
                            q = skipnolist(&g, q + 1, showall);
                            j -= 1;
                        }
                    }
                    mc += 1;
                }
                while {
                    let t = i;
                    i -= 1;
                    t > 0
                } {
                    printm(z, gi, None, mc, ml, i == 0, wid_at(mc));
                    mc += 1;
                }
                if n != 0 {
                    ml += 1;
                    listnl(&mut cl);
                    if nl != 0 {
                        let mut j = if g.flags & CGF_ROWS != 0 { g.cols } else { 1 };
                        while j != 0 && p < len {
                            p = skipnolist(&g, p + 1, showall);
                            j -= 1;
                        }
                    }
                }
            }
        } else {
            continue;
        }
        if g.lcount != 0 || (showall && g.mcount != 0) {
            pnl = true;
        }
    }
    set_lastlistlen(0);
    if clearflag() != 0 {
        // Back up to the prompt, if the list fits.
        let up = z.comp.listdat.nlines + z.refresh.nlnct as i32 - 1;
        if up < zterm_lines() {
            tcup(up);
            z.showinglist = -1;
            set_lastlistlen(z.comp.listdat.nlines);
        } else {
            set_clearflag(0);
            write_tty("\n");
        }
    } else {
        write_tty("\n");
    }
    let v = if clearflag() != 0 { 1 } else { -1 };
    set_listshown(z, v);
    printed
}

/// The display string of a CMF_ALL match: as many of the other matches
/// as fit on a line.
pub fn bld_all_str(z: &mut Zle, all: &CmatchRef) {
    let mut len = zterm_columns() - 5;
    let mut add = 0;
    let mut buf = String::new();

    let Some(mut g) = z.comp.amatches.iter().position(|g| g.mcount != 0) else {
        return;
    };
    let mut mp = 0usize;
    'all: loop {
        if let Some(mr) = z.comp.amatches[g].matches.get(mp) {
            let m = mr.borrow();
            if m.flags & (CMF_ALL | CMF_HIDE) == 0 {
                if let Some(s) = &m.str {
                    let t = clen(s) + add;
                    if len >= t {
                        if add != 0 {
                            buf.push(' ');
                        }
                        buf.push_str(s);
                        len -= t;
                        add = 1;
                    } else {
                        if len > add + 2 {
                            if add != 0 {
                                buf.push(' ');
                            }
                            buf.extend(s.chars().take(len.max(0) as usize));
                        }
                        buf.push_str("...");
                        break 'all;
                    }
                }
            }
        }
        mp += 1;
        if mp >= z.comp.amatches[g].matches.len() {
            loop {
                g += 1;
                if g >= z.comp.amatches.len() {
                    break 'all;
                }
                if z.comp.amatches[g].mcount != 0 {
                    break;
                }
            }
            mp = 0;
        }
    }
    all.borrow_mut().disp = Some(buf);
}

/// The default printer for printlist().
fn iprintm(z: &mut Zle, gi: usize, mp: Option<usize>, _mc: i32, _ml: i32, lastc: bool, width: i32) {
    let Some(mi) = mp else {
        return;
    };
    let Some(mr) = z
        .comp
        .amatches
        .get(gi)
        .and_then(|g| g.matches.get(mi))
        .cloned()
    else {
        return;
    };
    let (all, empty) = {
        let m = mr.borrow();
        (
            m.flags & CMF_ALL != 0,
            m.disp.as_deref().is_none_or(str::is_empty),
        )
    };
    if all && empty {
        bld_all_str(z, &mr);
    }
    let m = mr.borrow().clone();
    let mut len;
    if let Some(d) = &m.disp {
        if m.flags & CMF_DISPLINE != 0 {
            printfmt(z, d, 0, 1, 0);
            return;
        }
        let s = nicestr(d);
        write_tty(&s);
        len = strwidth(&s);
    } else {
        let s = nicestr(m.str.as_deref().unwrap_or(""));
        write_tty(&s);
        len = strwidth(&s);
        if z.comp.amatches[gi].flags & CGF_FILES != 0 && m.modec != '\0' {
            write_tty(&m.modec.to_string());
            len += 1;
        }
    }
    if !lastc {
        let pad = width - len;
        if pad > 0 {
            write_tty(&" ".repeat(pad as usize));
        }
    }
}

/// The default lister (zsh's ilistmatches()).
pub fn ilistmatches(z: &mut Zle) -> i32 {
    calclist(z, false);

    if z.comp.listdat.nlines == 0 {
        z.showinglist = 0;
        set_listshown(z, 0);
        return 1;
    }
    if asklist(z) != 0 {
        return 0;
    }
    printlist(z, false, iprintm, false);
    0
}

/// Lists the matches, through the lister hook.
pub fn list_matches(z: &mut Zle) -> i32 {
    match COMPLISTMATCHESHOOK.with(|h| h.get()) {
        Some(f) => f(z),
        None => ilistmatches(z),
    }
}

/// Invalidates the completion list.
pub fn invalidate_list(z: &mut Zle) -> i32 {
    z.comp.invcount += 1;
    if z.comp.validlist != 0 {
        if z.showinglist == -2 {
            z.zrefresh();
        }
        z.comp.lastmatches.clear();
        z.comp.hasoldlist = 0;
    }
    z.comp.lastambig = 0;
    z.comp.menucmp = 0;
    z.comp.menuacc = 0;
    z.comp.validlist = 0;
    z.showinglist = 0;
    z.comp.fromcomp = 0;
    z.comp.listdat.valid = 0;
    if listshown() < 0 {
        set_listshown(z, 0);
    }
    z.comp.minfo.cur = None;
    z.comp.minfo.asked = 0;
    z.comp.minfo.prebr = None;
    z.comp.minfo.postbr = None;
    z.comp.nmatches = 0;
    z.comp.amatches.clear();
    0
}
