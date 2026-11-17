//! Port of zsh's zle_word.c.

use crate::zle::widgets::vi::wordflag;
use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("forward-word", forwardword),
    ("vi-forward-word", viforwardword),
    ("vi-forward-blank-word", viforwardblankword),
    ("emacs-forward-word", emacsforwardword),
    ("vi-forward-blank-word-end", viforwardblankwordend),
    ("vi-forward-word-end", viforwardwordend),
    ("backward-word", backwardword),
    ("vi-backward-word", vibackwardword),
    ("vi-backward-blank-word", vibackwardblankword),
    ("vi-backward-word-end", vibackwardwordend),
    ("vi-backward-blank-word-end", vibackwardblankwordend),
    ("emacs-backward-word", emacsbackwardword),
    ("backward-delete-word", backwarddeleteword),
    ("vi-backward-kill-word", vibackwardkillword),
    ("backward-kill-word", backwardkillword),
    ("up-case-word", upcaseword),
    ("down-case-word", downcaseword),
    ("capitalize-word", capitalizeword),
    ("delete-word", deleteword),
    ("kill-word", killword),
    ("transpose-words", transposewords),
];

const DEFAULT_WORDCHARS: &str = "*?_-.[]~=/&;!#$%^(){}<>";

/// The characters that count as word characters besides alphanumerics.
pub fn wordchars() -> Vec<char> {
    crate::shell::var_get_opt("WORDCHARS")
        .unwrap_or_else(|| DEFAULT_WORDCHARS.to_string())
        .chars()
        .collect()
}

/// zsh's ZC_iword: alphanumeric or in $WORDCHARS.
pub fn zc_iword(c: char, wc: &[char]) -> bool {
    c.is_alphanumeric() || (c != '\0' && wc.contains(&c))
}

/// zsh's ZC_iblank: a blank other than newline.
pub fn zc_iblank(c: char) -> bool {
    c != '\n' && c.is_whitespace()
}

/// zsh's ZC_inblank: a blank or newline.
pub fn zc_inblank(c: char) -> bool {
    c.is_whitespace()
}

fn zc_ipunct(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_punctuation()
    } else {
        !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control()
    }
}

/// The character at `i`, or NUL past the end (zleline is NUL-terminated).
fn at(z: &Zle, i: usize) -> char {
    z.line.get(i).copied().unwrap_or('\0')
}

fn toupper(c: char) -> char {
    let mut u = c.to_uppercase();
    match (u.next(), u.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn tolower(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

// Runs `f` with the numeric argument negated, as the widgets do for a
// negative count.
fn with_neg_mult(z: &mut Zle, args: &[String], f: WidgetFn) -> i32 {
    let n = z.zmod.mult;
    z.zmod.mult = -n;
    let ret = f(z, args);
    z.zmod.mult = n;
    ret
}

pub fn forwardword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, backwardword);
    }
    let wc = wordchars();
    while n > 0 {
        n -= 1;
        while z.cs != z.ll() && zc_iword(z.line[z.cs], &wc) {
            z.cs += 1;
        }
        if wordflag() && n == 0 {
            return 0;
        }
        while z.cs != z.ll() && !zc_iword(z.line[z.cs], &wc) {
            z.cs += 1;
        }
    }
    0
}

/// Class of a character for vi word motions: 0 blank, 1 alphanumeric or
/// `_`, 2 punctuation, 3 anything else.
pub fn wordclass(x: char) -> i32 {
    if zc_iblank(x) {
        0
    } else if x.is_alphanumeric() || x == '_' {
        1
    } else if zc_ipunct(x) {
        2
    } else {
        3
    }
}

pub fn viforwardword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, vibackwardword);
    }
    while n > 0 {
        n -= 1;
        let cc = wordclass(at(z, z.cs));
        while z.cs != z.ll() && wordclass(z.line[z.cs]) == cc {
            z.cs += 1;
        }
        if wordflag() && n == 0 {
            return 0;
        }
        let mut nl = i32::from(at(z, z.cs) == '\n');
        while z.cs != z.ll() && nl < 2 && zc_inblank(z.line[z.cs]) {
            z.cs += 1;
            nl += i32::from(at(z, z.cs) == '\n');
        }
    }
    0
}

pub fn viforwardblankword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, vibackwardblankword);
    }
    while n > 0 {
        n -= 1;
        while z.cs != z.ll() && !zc_inblank(z.line[z.cs]) {
            z.cs += 1;
        }
        if wordflag() && n == 0 {
            return 0;
        }
        let mut nl = i32::from(at(z, z.cs) == '\n');
        while z.cs != z.ll() && nl < 2 && zc_inblank(z.line[z.cs]) {
            z.cs += 1;
            nl += i32::from(at(z, z.cs) == '\n');
        }
    }
    0
}

pub fn emacsforwardword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, emacsbackwardword);
    }
    let wc = wordchars();
    while n > 0 {
        n -= 1;
        while z.cs != z.ll() && !zc_iword(z.line[z.cs], &wc) {
            z.cs += 1;
        }
        if wordflag() && n == 0 {
            return 0;
        }
        while z.cs != z.ll() && zc_iword(z.line[z.cs], &wc) {
            z.cs += 1;
        }
    }
    0
}

pub fn viforwardblankwordend(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, vibackwardblankwordend);
    }
    while n > 0 {
        n -= 1;
        while z.cs != z.ll() {
            let pos = z.cs + 1;
            if !zc_inblank(at(z, pos)) {
                break;
            }
            z.cs = pos;
        }
        while z.cs != z.ll() {
            let pos = z.cs + 1;
            if zc_inblank(at(z, pos)) {
                break;
            }
            z.cs = pos;
        }
    }
    if z.cs != z.ll() && z.virangeflag != 0 {
        z.cs += 1;
    }
    0
}

pub fn viforwardwordend(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, vibackwardwordend);
    }
    while n > 0 {
        n -= 1;
        while z.cs != z.ll() {
            let pos = z.cs + 1;
            if !zc_inblank(at(z, pos)) {
                break;
            }
            z.cs = pos;
        }
        if z.cs != z.ll() {
            let mut pos = z.cs + 1;
            let cc = wordclass(at(z, pos));
            loop {
                z.cs = pos;
                if z.cs == z.ll() {
                    break;
                }
                pos += 1;
                if wordclass(at(z, pos)) != cc {
                    break;
                }
            }
        }
    }
    if z.cs != z.ll() && z.virangeflag != 0 {
        z.cs += 1;
    }
    0
}

pub fn backwardword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, forwardword);
    }
    let wc = wordchars();
    while n > 0 {
        n -= 1;
        while z.cs > 0 {
            let pos = z.cs - 1;
            if zc_iword(z.line[pos], &wc) {
                break;
            }
            z.cs = pos;
        }
        while z.cs > 0 {
            let pos = z.cs - 1;
            if !zc_iword(z.line[pos], &wc) {
                break;
            }
            z.cs = pos;
        }
    }
    0
}

pub fn vibackwardword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, viforwardword);
    }
    while n > 0 {
        n -= 1;
        let mut nl = 0;
        while z.cs > 0 {
            z.cs -= 1;
            if !zc_inblank(z.line[z.cs]) {
                break;
            }
            nl += i32::from(z.line[z.cs] == '\n');
            if nl == 2 {
                z.cs += 1;
                break;
            }
        }
        if z.cs > 0 {
            let mut pos = z.cs;
            let cc = wordclass(z.line[pos]);
            loop {
                z.cs = pos;
                if z.cs == 0 {
                    break;
                }
                pos -= 1;
                if wordclass(z.line[pos]) != cc || zc_inblank(z.line[pos]) {
                    break;
                }
            }
        }
    }
    0
}

pub fn vibackwardblankword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, viforwardblankword);
    }
    while n > 0 {
        n -= 1;
        let mut nl = 0;
        while z.cs > 0 {
            let pos = z.cs - 1;
            if !zc_inblank(z.line[pos]) {
                break;
            }
            nl += i32::from(z.line[pos] == '\n');
            if nl == 2 {
                break;
            }
            z.cs = pos;
        }
        while z.cs > 0 {
            let pos = z.cs - 1;
            if zc_inblank(z.line[pos]) {
                break;
            }
            z.cs = pos;
        }
    }
    0
}

pub fn vibackwardwordend(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, viforwardwordend);
    }
    while n > 0 && z.cs > 0 {
        n -= 1;
        let cc = wordclass(at(z, z.cs));
        z.cs -= 1;
        while z.cs > 0 {
            if wordclass(z.line[z.cs]) != cc || zc_iblank(z.line[z.cs]) {
                break;
            }
            z.cs -= 1;
        }
        while z.cs > 0 && zc_iblank(z.line[z.cs]) {
            z.cs -= 1;
        }
    }
    0
}

pub fn vibackwardblankwordend(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, viforwardblankwordend);
    }
    while n > 0 {
        n -= 1;
        while z.cs > 0 && !zc_inblank(at(z, z.cs)) {
            z.cs -= 1;
        }
        while z.cs > 0 && zc_inblank(at(z, z.cs)) {
            z.cs -= 1;
        }
    }
    0
}

pub fn emacsbackwardword(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, emacsforwardword);
    }
    let wc = wordchars();
    while n > 0 {
        n -= 1;
        while z.cs > 0 {
            let pos = z.cs - 1;
            if zc_iword(z.line[pos], &wc) {
                break;
            }
            z.cs = pos;
        }
        while z.cs > 0 {
            let pos = z.cs - 1;
            if !zc_iword(z.line[pos], &wc) {
                break;
            }
            z.cs = pos;
        }
    }
    0
}

// The start of the `n`th word back from the cursor (backward-kill-word,
// backward-delete-word).
fn back_word_start(z: &Zle, mut n: i64) -> usize {
    let wc = wordchars();
    let mut x = z.cs;
    while n > 0 {
        n -= 1;
        while x > 0 {
            let pos = x - 1;
            if zc_iword(z.line[pos], &wc) {
                break;
            }
            x = pos;
        }
        while x > 0 {
            let pos = x - 1;
            if !zc_iword(z.line[pos], &wc) {
                break;
            }
            x = pos;
        }
    }
    x
}

// The end of the `n`th word forward from the cursor (kill-word,
// delete-word).
fn fore_word_end(z: &Zle, mut n: i64) -> usize {
    let wc = wordchars();
    let mut x = z.cs;
    while n > 0 {
        n -= 1;
        while x != z.ll() && !zc_iword(z.line[x], &wc) {
            x += 1;
        }
        while x != z.ll() && zc_iword(z.line[x], &wc) {
            x += 1;
        }
    }
    x
}

pub fn backwarddeleteword(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, deleteword);
    }
    let x = back_word_start(z, n);
    z.backdel(z.cs - x, CUT_RAW);
    0
}

pub fn vibackwardkillword(z: &mut Zle, _args: &[String]) -> i32 {
    let bol = z.findbol();
    let lim = z.viinsbegin.max(bol);
    let mut x = z.cs;
    let mut n = z.zmod.mult;
    if n < 0 {
        return 1;
    }
    while n > 0 {
        n -= 1;
        while x > lim {
            let pos = x - 1;
            if !zc_iblank(z.line[pos]) {
                break;
            }
            x = pos;
        }
        if x > lim {
            let mut pos = x - 1;
            let cc = wordclass(z.line[pos]);
            loop {
                x = pos;
                if x <= lim {
                    break;
                }
                pos -= 1;
                if wordclass(z.line[pos]) != cc {
                    break;
                }
            }
        }
    }
    z.backkill(z.cs - x, CUT_FRONT | CUT_RAW);
    0
}

pub fn backwardkillword(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, killword);
    }
    let x = back_word_start(z, n);
    z.backkill(z.cs - x, CUT_FRONT | CUT_RAW);
    0
}

// up-case-word and down-case-word: maps each word character of the next
// words through `f`.
fn caseword(z: &mut Zle, f: fn(char) -> char) -> i32 {
    let mut n = z.zmod.mult;
    let neg = n < 0;
    let ocs = z.cs;
    if neg {
        n = -n;
    }
    let wc = wordchars();
    while n > 0 {
        n -= 1;
        while z.cs != z.ll() && !zc_iword(z.line[z.cs], &wc) {
            z.cs += 1;
        }
        while z.cs != z.ll() && zc_iword(z.line[z.cs], &wc) {
            z.line[z.cs] = f(z.line[z.cs]);
            z.cs += 1;
        }
    }
    if neg {
        z.cs = ocs;
    }
    0
}

pub fn upcaseword(z: &mut Zle, _args: &[String]) -> i32 {
    caseword(z, toupper)
}

pub fn downcaseword(z: &mut Zle, _args: &[String]) -> i32 {
    caseword(z, tolower)
}

pub fn capitalizeword(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    let neg = n < 0;
    let ocs = z.cs;
    if neg {
        n = -n;
    }
    let wc = wordchars();
    while n > 0 {
        n -= 1;
        let mut first = true;
        while z.cs != z.ll() && !zc_iword(z.line[z.cs], &wc) {
            z.cs += 1;
        }
        while z.cs != z.ll() && zc_iword(z.line[z.cs], &wc) && !z.line[z.cs].is_alphabetic() {
            z.cs += 1;
        }
        while z.cs != z.ll() && zc_iword(z.line[z.cs], &wc) {
            let c = z.line[z.cs];
            z.line[z.cs] = if first { toupper(c) } else { tolower(c) };
            first = false;
            z.cs += 1;
        }
    }
    if neg {
        z.cs = ocs;
    }
    0
}

pub fn deleteword(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, backwarddeleteword);
    }
    let x = fore_word_end(z, n);
    z.foredel(x - z.cs, CUT_RAW);
    0
}

pub fn killword(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        return with_neg_mult(z, args, backwardkillword);
    }
    let x = fore_word_end(z, n);
    z.forekill(x - z.cs, CUT_RAW);
    0
}

pub fn transposewords(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    let neg = n < 0;
    let ocs = z.cs;
    if neg {
        n = -n;
    }
    let wc = wordchars();
    let ll = z.ll();
    let mut x = z.cs;
    while x != ll && z.line[x] != '\n' && !zc_iword(z.line[x], &wc) {
        x += 1;
    }
    if x == ll || z.line[x] == '\n' {
        x = z.cs;
        while x > 0 {
            if zc_iword(at(z, x), &wc) {
                break;
            }
            let pos = x - 1;
            if z.line[pos] == '\n' {
                break;
            }
            x = pos;
        }
        if x == 0 {
            return 1;
        }
        if z.line[x - 1] == '\n' {
            return 1;
        }
    }
    let mut p4 = x;
    while p4 != ll && zc_iword(z.line[p4], &wc) {
        p4 += 1;
    }
    let mut p3 = p4;
    while p3 > 0 {
        let pos = p3 - 1;
        if !zc_iword(z.line[pos], &wc) {
            break;
        }
        p3 = pos;
    }
    if p3 == 0 {
        return 1;
    }
    let mut p1 = p3;
    let mut p2 = p3;
    let mut pt = p3;
    while n > 0 {
        n -= 1;
        p2 = pt;
        while p2 > 0 {
            let pos = p2 - 1;
            if zc_iword(z.line[pos], &wc) {
                break;
            }
            p2 = pos;
        }
        if p2 == 0 {
            return 1;
        }
        p1 = p2;
        while p1 > 0 {
            let pos = p1 - 1;
            if !zc_iword(z.line[pos], &wc) {
                break;
            }
            p1 = pos;
        }
        pt = p1;
    }
    let mut temp: Vec<char> = Vec::with_capacity(p4 - p1);
    temp.extend_from_slice(&z.line[p3..p4]);
    temp.extend_from_slice(&z.line[p2..p3]);
    temp.extend_from_slice(&z.line[p1..p2]);
    z.line[p1..p4].copy_from_slice(&temp);
    z.cs = if neg { ocs } else { p4 };
    0
}
