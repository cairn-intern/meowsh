//! Expansion and completion entry points, after zsh's zle_tricky.c.
//!
//! Also here: the removable-suffix machinery of zle_misc.c, and the part
//! of zsh's lexer (lex.c) that completion runs over the line to find the
//! word under the cursor and its context. Token strings are `Vec<char>`
//! holding zsh's token characters (U+0084 to U+00A1, the byte values zsh
//! uses) where the shell syntax is active, exactly as zsh tokenizes them.
//!
//! zsh's lex.c globals wb, we and inwhat are kept by core.rs; [`wb`],
//! [`set_wb`] and friends reach them. addedx lives here.

use super::*;
use crate::zle::widgets::misc;
use crate::zle::{refresh, Zle, CUT_RAW};
use std::cell::{Cell, RefCell};

// ---- tokens -------------------------------------------------------------

pub const POUND: char = '\u{84}';
pub const STRING: char = '\u{85}';
pub const HAT: char = '\u{86}';
pub const STAR: char = '\u{87}';
pub const INPAR: char = '\u{88}';
pub const INPARMATH: char = '\u{89}';
pub const OUTPAR: char = '\u{8a}';
pub const OUTPARMATH: char = '\u{8b}';
pub const QSTRING: char = '\u{8c}';
pub const EQUALS: char = '\u{8d}';
pub const BAR: char = '\u{8e}';
pub const INBRACE: char = '\u{8f}';
pub const OUTBRACE: char = '\u{90}';
pub const INBRACK: char = '\u{91}';
pub const OUTBRACK: char = '\u{92}';
pub const TICK: char = '\u{93}';
pub const INANG: char = '\u{94}';
pub const OUTANG: char = '\u{95}';
pub const OUTANGPROC: char = '\u{96}';
pub const QUEST: char = '\u{97}';
pub const TILDE: char = '\u{98}';
pub const QTICK: char = '\u{99}';
pub const COMMA: char = '\u{9a}';
pub const DASH: char = '\u{9b}';
pub const BANG: char = '\u{9c}';
pub const SNULL: char = '\u{9d}';
pub const DNULL: char = '\u{9e}';
pub const BNULL: char = '\u{9f}';
pub const BNULLKEEP: char = '\u{a0}';
pub const NULARG: char = '\u{a1}';

/// zsh's ztokens: what each token from Pound to Bnullkeep stands for.
const ZTOKENS: [char; 29] = [
    '#', '$', '^', '*', '(', '(', ')', ')', '$', '=', '|', '{', '}', '[', ']', '`', '<', '>', '>',
    '?', '~', '`', ',', '-', '!', '\'', '"', '\\', '\\',
];

pub fn itok(c: char) -> bool {
    (POUND..=NULARG).contains(&c)
}

pub fn inull(c: char) -> bool {
    (SNULL..=NULARG).contains(&c)
}

/// The character token `c` stands for; None for Nularg (and non-tokens).
pub fn ztoken(c: char) -> Option<char> {
    if !itok(c) {
        return None;
    }
    ZTOKENS.get((c as u32 - POUND as u32) as usize).copied()
}

pub fn untokenize(s: &[char]) -> Vec<char> {
    s.iter()
        .filter_map(|&c| if itok(c) { ztoken(c) } else { Some(c) })
        .collect()
}

pub fn has_token(s: &[char]) -> bool {
    s.iter().any(|&c| itok(c))
}

/// Removes the quoting nulls, as zsh's remnulargs: an emptied string
/// becomes a lone Nularg.
pub fn remnulargs(s: &[char]) -> Vec<char> {
    let Some(first) = s.iter().position(|&c| inull(c) && c != BNULLKEEP) else {
        return s.to_vec();
    };
    let mut out: Vec<char> = s[..first].to_vec();
    for &c in &s[first..] {
        if c == BNULLKEEP {
            out.push('\\');
        } else if !inull(c) {
            out.push(c);
        }
    }
    if out.is_empty() {
        out.push(NULARG);
    }
    out
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn string(s: &[char]) -> String {
    s.iter().collect()
}

fn is_set(name: &str) -> bool {
    crate::options::is_set(name)
}

fn iblank(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn inblank(c: char) -> bool {
    c == ' ' || c == '\t' || c == '\n'
}

fn iident(c: char) -> bool {
    if itok(c) {
        return false;
    }
    c == '_'
        || c.is_ascii_alphanumeric()
        || (!c.is_ascii() && c.is_alphanumeric() && !is_set("posixidentifiers"))
}

/// zsh's itype_end(s, IIDENT, once), from index `i`.
pub fn itype_end_ident(s: &[char], i: usize, once: bool) -> usize {
    let mut j = i;
    while j < s.len() && iident(s[j]) {
        j += 1;
        if once {
            break;
        }
    }
    j
}

/// zsh's itype_end(s, INAMESPC, once): identifiers with ksh93-style
/// namespaces (`.ns.name`).
pub fn itype_end_namespc(s: &[char], i: usize, once: bool) -> usize {
    let mut ptr = i;
    if !is_set("posixidentifiers") {
        let st = i + usize::from(s.get(i) == Some(&'.'));
        let t = itype_end_ident(s, st, false);
        if t > st {
            if s.get(t) == Some(&'.') {
                ptr = t + 1;
            } else if !once {
                return t;
            }
        }
    }
    itype_end_ident(s, ptr, once)
}

/// zsh's skipparens: with `s[i]` the opening character, the index after
/// its match (or the end) and the nesting left open; -1 if `s[i]` is not
/// `inpar`.
pub fn skipparens(s: &[char], i: usize, inpar: char, outpar: char) -> (i32, usize) {
    if s.get(i) != Some(&inpar) {
        return (-1, i);
    }
    let mut level = 1;
    let mut j = i;
    loop {
        j += 1;
        if j >= s.len() || level == 0 {
            break;
        }
        if s[j] == inpar {
            level += 1;
        } else if s[j] == outpar {
            level -= 1;
        }
    }
    (level, j)
}

// ---- quoting ------------------------------------------------------------

pub const QT_NONE: i32 = 0;
pub const QT_BACKSLASH: i32 = 1;
pub const QT_SINGLE: i32 = 2;
pub const QT_DOUBLE: i32 = 3;
pub const QT_DOLLARS: i32 = 4;
pub const QT_BACKTICK: i32 = 5;
pub const QT_SINGLE_OPTIONAL: i32 = 6;
pub const QT_BACKSLASH_PATTERN: i32 = 7;
pub const QT_BACKSLASH_SHOWNULL: i32 = 8;

pub const IN_NOTHING: i32 = 0;
pub const IN_CMD: i32 = 1;
pub const IN_MATH: i32 = 2;
pub const IN_COND: i32 = 3;
pub const IN_ENV: i32 = 4;
pub const IN_PAR: i32 = 5;

thread_local! {
    static SPECIALCOMMA: Cell<bool> = const { Cell::new(false) };
}

/// Whether a comma needs quoting (inside a brace expansion being completed).
pub fn makecommaspecial(yesno: bool) {
    SPECIALCOMMA.with(|s| s.set(yesno));
}

fn ispecial(c: char) -> bool {
    "#$^*()=|{}[]`<>?~;&\n\t \\'\"".contains(c)
        || (c == ',' && SPECIALCOMMA.with(|s| s.get()))
        || (c == '!' && is_set("banghist"))
}

fn isprint(c: char) -> bool {
    !c.is_control()
}

fn addunprintable(v: &mut Vec<char>, c: char) {
    let mut buf = [0u8; 4];
    for &b in c.encode_utf8(&mut buf).as_bytes() {
        match b {
            0 => v.extend(['\\', '0']),
            7 => v.extend(['\\', 'a']),
            8 => v.extend(['\\', 'b']),
            12 => v.extend(['\\', 'f']),
            b'\n' => v.extend(['\\', 'n']),
            b'\r' => v.extend(['\\', 'r']),
            b'\t' => v.extend(['\\', 't']),
            11 => v.extend(['\\', 'v']),
            _ => {
                v.push('\\');
                v.push(char::from(b'0' + ((b >> 6) & 7)));
                v.push(char::from(b'0' + ((b >> 3) & 7)));
                v.push(char::from(b'0' + (b & 7)));
            }
        }
    }
}

/// Quotes `s` (which may hold tokens) for quoting style `instring`, a
/// QT_* value, as zsh's quotestring.
pub fn quotestring(s: &[char], instring: i32) -> Vec<char> {
    let mut instring = instring;
    let mut shownull = false;
    let mut quotesub = 0;
    match instring {
        QT_BACKSLASH_SHOWNULL => {
            shownull = true;
            instring = QT_BACKSLASH;
        }
        QT_SINGLE_OPTIONAL => {
            quotesub = 1;
            shownull = true;
        }
        _ => {}
    }
    let banghist = is_set("banghist");
    let mut v: Vec<char> = Vec::new();
    let mut quotestart = 0usize;
    if instring == QT_DOLLARS {
        let start = usize::from(s.first().is_some_and(|&c| inull(c)));
        for &c in &s[start..] {
            if isprint(c) {
                if c == '\\' || c == '\'' || (banghist && c == '!') {
                    v.push('\\');
                }
                v.push(c);
            } else {
                addunprintable(&mut v, c);
            }
        }
    } else if instring == QT_BACKSLASH_PATTERN {
        for &c in s {
            if "#^*()|[]<>?~\\".contains(c) {
                v.push('\\');
            }
            v.push(c);
        }
    } else {
        if shownull && s.is_empty() {
            v.extend(['\'', '\'']);
        }
        let magiceq = is_set("magicequalsubst");
        let extglob = is_set("extendedglob");
        let rcquotes = is_set("rcquotes");
        let mut u = 0usize;
        while u < s.len() {
            let c = s[u];
            let mut dobackslash = false;
            if c == TICK || c == QTICK {
                v.push(c);
                u += 1;
                while u < s.len() && s[u] != c {
                    v.push(s[u]);
                    u += 1;
                }
                v.push(c);
                if u < s.len() {
                    u += 1;
                }
                continue;
            } else if (c == QSTRING || c == '$')
                && s.get(u + 1) == Some(&'\'')
                && instring == QT_DOUBLE
            {
                // $'...' needs no quoting inside double quotes.
                v.push(c);
                u += 1;
            } else if (c == STRING || c == QSTRING)
                && matches!(s.get(u + 1), Some(&INPAR) | Some(&INBRACK) | Some(&INBRACE))
            {
                let close = match s[u + 1] {
                    INPAR => OUTPAR,
                    INBRACE => OUTBRACE,
                    _ => OUTBRACK,
                };
                let beg = c;
                let mut level = 0;
                v.push(s[u]);
                v.push(s[u + 1]);
                u += 2;
                while u < s.len() && (s[u] != close || level > 0) {
                    if s[u] == beg {
                        level += 1;
                    } else if s[u] == close {
                        level -= 1;
                    }
                    v.push(s[u]);
                    u += 1;
                }
                if u < s.len() {
                    v.push(s[u]);
                    u += 1;
                }
                continue;
            } else if ispecial(c)
                && ((c != '=' && c != '~')
                    || u == 0
                    || (magiceq && (s[u - 1] == '=' || s[u - 1] == ':'))
                    || (c == '~' && extglob))
                && (instring == QT_BACKSLASH
                    || instring == QT_SINGLE_OPTIONAL
                    || (banghist && c == '!' && instring != QT_SINGLE)
                    || (instring == QT_DOUBLE && matches!(c, '$' | '`' | '"' | '\\'))
                    || (instring == QT_SINGLE && c == '\''))
            {
                if instring == QT_SINGLE_OPTIONAL {
                    if quotesub == 1 {
                        if c == '\'' {
                            v.push('\\');
                        } else {
                            v.insert(quotestart, '\'');
                            quotesub = 2;
                        }
                        v.push(c);
                        u += 1;
                        quotestart = v.len();
                    } else if c == '\'' {
                        if rcquotes {
                            v.extend(['\'', '\'']);
                        } else {
                            v.extend(['\'', '\\', '\'']);
                            quotesub = 1;
                            quotestart = v.len();
                        }
                        u += 1;
                    } else {
                        v.push(c);
                        u += 1;
                    }
                    continue;
                } else if c == '\n' || (instring == QT_SINGLE && c == '\'') {
                    if c == '\n' {
                        v.extend(['$', '\'', '\\', 'n', '\'']);
                    } else if rcquotes {
                        v.extend(['\'', '\'']);
                    } else {
                        v.extend(['\'', '\\', c, '\'']);
                    }
                    u += 1;
                    continue;
                } else {
                    dobackslash = true;
                }
            }
            let c = s[u];
            if itok(c) || instring != QT_BACKSLASH {
                if dobackslash {
                    v.push('\\');
                }
                if c == INPARMATH {
                    // Already syntactically quoted.
                    let mut inmath = 1;
                    v.push(c);
                    u += 1;
                    while let Some(&uc) = s.get(u) {
                        v.push(uc);
                        u += 1;
                        if uc == OUTPARMATH {
                            inmath -= 1;
                            if inmath == 0 {
                                break;
                            }
                        } else if uc == INPARMATH {
                            inmath += 1;
                        }
                    }
                } else {
                    v.push(c);
                    u += 1;
                }
                continue;
            }
            if isprint(c) {
                if dobackslash {
                    v.push('\\');
                }
                v.push(c);
            } else {
                v.extend(['$', '\'']);
                addunprintable(&mut v, c);
                v.push('\'');
            }
            u += 1;
        }
    }
    if quotesub == 2 {
        v.push('\'');
    }
    v
}

/// zsh's quotename macro: quotes for the quoting the word being completed
/// is in.
pub fn quotename(z: &Zle, s: &[char]) -> Vec<char> {
    let q = if z.comp.instring == QT_NONE {
        QT_BACKSLASH
    } else {
        z.comp.instring
    };
    quotestring(s, q)
}

/// Decodes the body of a tokenized `$'...'` up to its closing Snull, as
/// getkeystring with GETKEYS_DOLLARS_QUOTE: the decoded text and the
/// number of characters used, the closing quote included.
fn getkeystring_dollars(s: &[char]) -> (Vec<char>, usize) {
    let mut raw = String::new();
    let mut n = 0;
    while n < s.len() && s[n] != SNULL {
        let c = s[n];
        raw.push(if c == BNULL || c == BNULLKEEP {
            '\\'
        } else {
            c
        });
        n += 1;
    }
    if n < s.len() {
        n += 1;
    }
    (chars(&crate::expand::decode_escapes(&raw)), n)
}

/// A token string as shell source text that the expander reads back to
/// the same word: tokens become the active syntax they stand for, nulls
/// the quotes, and literal special characters are quoted.
fn tokens_to_source(s: &[char]) -> String {
    let mut out = String::new();
    let (mut sq, mut dq) = (false, false);
    for &c in s {
        match c {
            SNULL if !dq => {
                sq = !sq;
                out.push('\'');
            }
            DNULL if !sq => {
                dq = !dq;
                out.push('"');
            }
            BNULL | BNULLKEEP => out.push('\\'),
            NULARG => {
                if !sq && !dq {
                    out.push_str("''");
                }
            }
            c if itok(c) => out.push(ztoken(c).unwrap_or(c)),
            c if sq => out.push(c),
            c if dq => {
                if matches!(c, '$' | '`' | '"' | '\\') {
                    out.push('\\');
                }
                out.push(c);
            }
            '\n' => out.push_str("\"\n\""),
            c => {
                if c.is_whitespace() || "\\'\"$`|&;<>(){}[]*?~#^=!,".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
        }
    }
    out
}

// ---- the removable suffix (zle_misc.c) ----------------------------------

pub const SUFTYP_POSSTR: i32 = 0;
pub const SUFTYP_NEGSTR: i32 = 1;
pub const SUFTYP_POSRNG: i32 = 2;
pub const SUFTYP_NEGRNG: i32 = 3;
pub const SUFFLAGS_SPACE: i32 = 1;

struct SuffixSet {
    tp: i32,
    flags: i32,
    chars: Vec<char>,
    lensuf: i32,
}

#[derive(Default)]
struct SuffixState {
    /// Newest first, as zsh's suffixlist.
    list: Vec<SuffixSet>,
    func: Option<String>,
    noinsrem: bool,
    len: i32,
}

thread_local! {
    static SUFFIX: RefCell<SuffixState> = RefCell::new(SuffixState::default());
}

// utils.rs's fixsuffix resets z.suffix wholesale; `remove_on_space` there
// marks the state here as still current, so that reset fixes it too.
fn sync_suffix(z: &Zle) {
    if !z.suffix.remove_on_space {
        SUFFIX.with(|s| *s.borrow_mut() = SuffixState::default());
    }
}

fn publish_suffix(z: &mut Zle) {
    let (len, any) = SUFFIX.with(|s| {
        let s = s.borrow();
        (s.len, !s.list.is_empty() || s.func.is_some() || s.len != 0)
    });
    z.suffix.remove_on_space = any;
    z.suffix.active = len != 0;
    z.suffix.end = z.cs;
    z.suffix.start = z.cs.saturating_sub(len.max(0) as usize);
}

/// Length of the active auto-removable suffix (zsh's suffixlen).
pub fn suffixlen(z: &Zle) -> i32 {
    if !z.suffix.remove_on_space {
        return 0;
    }
    SUFFIX.with(|s| s.borrow().len)
}

pub fn addsuffix(z: &mut Zle, tp: i32, flags: i32, chars: &[char], lensuf: i32) {
    sync_suffix(z);
    SUFFIX.with(|s| {
        s.borrow_mut().list.insert(
            0,
            SuffixSet {
                tp,
                flags,
                chars: chars.to_vec(),
                lensuf,
            },
        )
    });
    publish_suffix(z);
}

/// The last `n` characters are a suffix removed in the usual word end
/// conditions.
pub fn makesuffix(z: &mut Zle, n: i32) {
    let remove =
        crate::shell::var_get_opt("ZLE_REMOVE_SUFFIX_CHARS").unwrap_or_else(|| " \t\n;&|".into());
    addsuffix(z, SUFTYP_POSSTR, 0, &chars(&remove), n);
    // Second, so it takes precedence.
    if let Some(sp) = crate::shell::var_get_opt("ZLE_SPACE_SUFFIX_CHARS") {
        if !sp.is_empty() {
            addsuffix(z, SUFTYP_POSSTR, SUFFLAGS_SPACE, &chars(&sp), n);
        }
    }
    SUFFIX.with(|s| {
        let mut s = s.borrow_mut();
        s.len = n;
        s.noinsrem = true;
    });
    publish_suffix(z);
}

/// The last `n` characters are a parameter name suffix, removed if what
/// follows can only go right after a name; `br` for `${name}`.
pub fn makeparamsuffix(z: &mut Zle, br: bool, n: i32) {
    let charstr = [':', '[', '#', '%', '?', '-', '+', '='];
    let mut lenstr = 0;
    if br || !is_set("ksharrays") {
        lenstr = 2;
        if br {
            lenstr += 6;
        }
    }
    if lenstr > 0 {
        addsuffix(z, SUFTYP_POSSTR, 0, &charstr[..lenstr], n);
    }
}

/// Set up a suffix removed by function `f`, or on the characters in `s`
/// (`^`/`!` negates, `a-z` ranges, `\-` also on uninsertable keys).
pub fn makesuffixstr(z: &mut Zle, f: Option<&str>, s: Option<&str>, n: i32) {
    sync_suffix(z);
    if let Some(f) = f {
        SUFFIX.with(|st| {
            let mut st = st.borrow_mut();
            st.func = Some(f.to_string());
            st.len = n;
        });
        publish_suffix(z);
    } else if let Some(s) = s {
        let (inv, s) = match s.strip_prefix(['^', '!']) {
            Some(r) => (true, r),
            None => (false, s),
        };
        // GETKEYS_SUFFIX: `\-` is a flag, the rest are escapes.
        let sc = chars(s);
        let mut raw = String::new();
        let mut zflag = false;
        let mut k = 0;
        while k < sc.len() {
            if sc[k] == '\\' && sc.get(k + 1) == Some(&'-') {
                zflag = true;
                k += 2;
                continue;
            }
            raw.push(sc[k]);
            if sc[k] == '\\' {
                if let Some(&nc) = sc.get(k + 1) {
                    raw.push(nc);
                    k += 1;
                }
            }
            k += 1;
        }
        let ws = chars(&crate::expand::decode_escapes(&raw));
        SUFFIX.with(|st| {
            let mut st = st.borrow_mut();
            st.noinsrem = zflag ^ inv;
            st.len = n;
        });
        let (pos, rng) = if inv {
            (SUFTYP_NEGSTR, SUFTYP_NEGRNG)
        } else {
            (SUFTYP_POSSTR, SUFTYP_POSRNG)
        };
        let mut lasts = 0;
        let mut w = 0;
        while w < ws.len() {
            if ws.len() - w >= 3 && ws[w + 1] == '-' {
                if w > lasts {
                    addsuffix(z, pos, 0, &ws[lasts..w], n);
                }
                addsuffix(z, rng, 0, &[ws[w], ws[w + 2]], n);
                w += 3;
                lasts = w;
            } else {
                w += 1;
            }
        }
        if w > lasts {
            addsuffix(z, pos, 0, &ws[lasts..w], n);
        }
        publish_suffix(z);
    } else {
        makesuffix(z, n);
    }
}

/// Removes the suffix, if there is one, when inserting `c` (None: an
/// action that inserts nothing).
pub fn iremovesuffix(z: &mut Zle, c: Option<char>, keep: bool) {
    sync_suffix(z);
    let func = SUFFIX.with(|s| s.borrow_mut().func.take());
    if let Some(f) = func {
        let exists = crate::shell::with_shell(|s| s.functions.contains_key(&f));
        if exists {
            let len = SUFFIX.with(|s| s.borrow().len);
            z.call_user_widget(&f, &[len.to_string()]);
        }
    } else {
        let (sl, flags) = SUFFIX.with(|s| {
            let s = s.borrow();
            match c {
                None => (if s.noinsrem { s.len } else { 0 }, 0),
                Some(ch) => {
                    // The first set that matches decides; failing that, a
                    // negative set passed through gives its length.
                    let mut negsuflen = 0;
                    let mut found = None;
                    for ss in &s.list {
                        let hit = match ss.tp {
                            SUFTYP_POSSTR | SUFTYP_NEGSTR => ss.chars.contains(&ch),
                            _ => ss.chars.len() >= 2 && ss.chars[0] <= ch && ch <= ss.chars[1],
                        };
                        let positive = ss.tp == SUFTYP_POSSTR || ss.tp == SUFTYP_POSRNG;
                        if hit {
                            found = Some((if positive { ss.lensuf } else { 0 }, ss.flags));
                            break;
                        } else if !positive {
                            negsuflen = ss.lensuf;
                        }
                    }
                    found.unwrap_or((negsuflen, 0))
                }
            }
        });
        if sl > 0 {
            z.backdel(sl as usize, CUT_RAW);
            if flags & SUFFLAGS_SPACE != 0 {
                z.spaceinline(1);
                let cs = z.cs;
                z.line[cs] = ' ';
                z.cs += 1;
            }
            if !keep {
                z.invalidatelist();
            }
        }
    }
    fixsuffix(z);
}

/// Fixes the suffix in place, making it non-removable.
pub fn fixsuffix(z: &mut Zle) {
    SUFFIX.with(|s| *s.borrow_mut() = SuffixState::default());
    publish_suffix(z);
}

// ---- lex.c globals shared with completion -------------------------------

thread_local! {
    static ADDEDX: Cell<i32> = const { Cell::new(0) };
    static WB: Cell<i32> = const { Cell::new(0) };
    static WE: Cell<i32> = const { Cell::new(0) };
    static INWHAT: Cell<i32> = const { Cell::new(0) };
}

/// Start of the word being completed in the line (zsh's wb).
pub fn wb() -> i32 {
    WB.with(|c| c.get())
}

pub fn set_wb(v: i32) {
    WB.with(|c| c.set(v));
}

/// End of the word being completed (zsh's we).
pub fn we() -> i32 {
    WE.with(|c| c.get())
}

pub fn set_we(v: i32) {
    WE.with(|c| c.set(v));
}

/// What the word is (an IN_* value; zsh's inwhat).
pub fn inwhat() -> i32 {
    INWHAT.with(|c| c.get())
}

pub fn set_inwhat(v: i32) {
    INWHAT.with(|c| c.set(v));
}

/// 1 if an `x` was put in the line to make a word at the cursor, 2 if an
/// `x` and a space (zsh's addedx).
pub fn addedx() -> i32 {
    ADDEDX.with(|a| a.get())
}

pub fn set_addedx(v: i32) {
    ADDEDX.with(|a| a.set(v));
}

// ---- the lexer (lex.c) --------------------------------------------------

/// zsh's lexical tokens (enum lextok).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lextok {
    Nulltok,
    Seper,
    Newlin,
    Semi,
    Dsemi,
    Amper,
    Inpar,
    Outpar,
    Dbar,
    Damper,
    Outang,
    Outangbang,
    Doutang,
    Doutangbang,
    Inang,
    Inoutang,
    Dinang,
    Dinangdash,
    Inangamp,
    Outangamp,
    Ampoutang,
    Outangampbang,
    Doutangamp,
    Doutangampbang,
    Trinang,
    Bar,
    Baramp,
    Inoutpar,
    Dinpar,
    Doutpar,
    Amperbang,
    Semiamp,
    Semibar,
    Doutbrack,
    String,
    Envstring,
    Envarray,
    Endinput,
    Lexerr,
    Bang,
    Dinbrack,
    Inbrace,
    Outbrace,
    Case,
    Coproc,
    Doloop,
    Done,
    Elif,
    Else,
    Zend,
    Esac,
    Fi,
    For,
    Foreach,
    Func,
    If,
    Nocorrect,
    Repeat,
    Select,
    Then,
    Time,
    Until,
    While,
    Typeset,
}

use Lextok as T;

impl Lextok {
    pub fn is_redirop(self) -> bool {
        matches!(
            self,
            T::Outang
                | T::Outangbang
                | T::Doutang
                | T::Doutangbang
                | T::Inang
                | T::Inoutang
                | T::Dinang
                | T::Dinangdash
                | T::Inangamp
                | T::Outangamp
                | T::Ampoutang
                | T::Outangampbang
                | T::Doutangamp
                | T::Doutangampbang
                | T::Trinang
        )
    }

    /// zsh's tokstrings[].
    pub fn text(self) -> Option<&'static str> {
        Some(match self {
            T::Seper | T::Semi => ";",
            T::Newlin => "\\n",
            T::Dsemi => ";;",
            T::Amper => "&",
            T::Inpar => "(",
            T::Outpar => ")",
            T::Dbar => "||",
            T::Damper => "&&",
            T::Outang => ">",
            T::Outangbang => ">|",
            T::Doutang => ">>",
            T::Doutangbang => ">>|",
            T::Inang => "<",
            T::Inoutang => "<>",
            T::Dinang => "<<",
            T::Dinangdash => "<<-",
            T::Inangamp => "<&",
            T::Outangamp => ">&",
            T::Ampoutang => "&>",
            T::Outangampbang => "&>|",
            T::Doutangamp => ">>&",
            T::Doutangampbang => ">>&|",
            T::Trinang => "<<<",
            T::Bar => "|",
            T::Baramp => "|&",
            T::Inoutpar => "()",
            T::Dinpar => "((",
            T::Doutpar => "))",
            T::Amperbang => "&|",
            T::Semiamp => ";&",
            T::Semibar => ";|",
            _ => return None,
        })
    }
}

/// zsh's reserved word table.
pub fn reswd(s: &str) -> Option<Lextok> {
    let t = match s {
        "!" => T::Bang,
        "[[" => T::Dinbrack,
        "{" => T::Inbrace,
        "}" => T::Outbrace,
        "case" => T::Case,
        "coproc" => T::Coproc,
        "declare" | "export" | "float" | "integer" | "local" | "readonly" | "typeset" => T::Typeset,
        "do" => T::Doloop,
        "done" => T::Done,
        "elif" => T::Elif,
        "else" => T::Else,
        "end" => T::Zend,
        "esac" => T::Esac,
        "fi" => T::Fi,
        "for" => T::For,
        "foreach" => T::Foreach,
        "function" => T::Func,
        "if" => T::If,
        "nocorrect" => T::Nocorrect,
        "repeat" => T::Repeat,
        "select" => T::Select,
        "then" => T::Then,
        "time" => T::Time,
        "until" => T::Until,
        "while" => T::While,
        _ => return None,
    };
    (!crate::builtins::is_disabled('r', s)).then_some(t)
}

const RESWDS: &[&str] = &[
    "!",
    "[[",
    "{",
    "}",
    "case",
    "coproc",
    "declare",
    "do",
    "done",
    "elif",
    "else",
    "end",
    "esac",
    "export",
    "fi",
    "float",
    "for",
    "foreach",
    "function",
    "if",
    "integer",
    "local",
    "nocorrect",
    "readonly",
    "repeat",
    "select",
    "then",
    "time",
    "typeset",
    "until",
    "while",
];

/// Lexer flag: the input is the line being edited (LEXFLAGS_ZLE).
pub const LEXFLAGS_ZLE: u32 = 0x0002;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lx2 {
    Break,
    Outpar,
    Bar,
    String,
    Inbrack,
    Outbrack,
    Inpar,
    Inbrace,
    Outbrace,
    Outang,
    Inang,
    Equals,
    Bkslash,
    Quote,
    Dquote,
    Bquote,
    Comma,
    Dash,
    Bang,
    Other,
}

fn lexact2(c: char) -> Lx2 {
    match c {
        ';' | '&' => Lx2::Break,
        ')' => Lx2::Outpar,
        '|' => Lx2::Bar,
        '$' => Lx2::String,
        '[' => Lx2::Inbrack,
        ']' => Lx2::Outbrack,
        '(' => Lx2::Inpar,
        '{' => Lx2::Inbrace,
        '}' => Lx2::Outbrace,
        '>' => Lx2::Outang,
        '<' => Lx2::Inang,
        '=' => Lx2::Equals,
        '\\' => Lx2::Bkslash,
        '\'' => Lx2::Quote,
        '"' => Lx2::Dquote,
        '`' => Lx2::Bquote,
        ',' => Lx2::Comma,
        '-' => Lx2::Dash,
        '!' => Lx2::Bang,
        _ => Lx2::Other,
    }
}

fn lextok2(c: char) -> char {
    match c {
        '*' => STAR,
        '?' => QUEST,
        '{' => INBRACE,
        '[' => INBRACK,
        '$' => STRING,
        '~' => TILDE,
        '#' => POUND,
        '^' => HAT,
        _ => c,
    }
}

#[derive(PartialEq, Eq)]
enum CmdOrMath {
    Cmd,
    Math,
    Err,
}

#[derive(Clone, Copy)]
struct LexOpts {
    shglob: bool,
    kshglob: bool,
    ignorebraces: bool,
    ignoreclosebraces: bool,
    rcquotes: bool,
    cshjunkiequotes: bool,
    interactivecomments: bool,
    aliases: bool,
    posixaliases: bool,
    shortloops: bool,
    shortrepeat: bool,
}

/// zsh's lexer run over a string, as completion uses it (ctxtlex with
/// the lexer state of lex.c). The input is one buffer; alias text is
/// spliced in where it is read, marked so positions in the line are only
/// taken from the line's own characters.
pub struct ZleLexer {
    buf: Vec<char>,
    from_alias: Vec<bool>,
    pos: usize,
    /// The input being read is alias text (zsh's inbufflags & INP_ALIAS).
    cur_alias: bool,
    /// Aliases being expanded, with the end of their text in `buf`.
    aliases_in_use: Vec<(String, usize)>,
    pub lexstop: bool,
    pub errflag: bool,
    pub lexflags: u32,
    /// The cursor and the line length (zsh's zlemetacs and zlemetall).
    pub cs: i32,
    pub ll: i32,
    pub tok: Lextok,
    pub tokstr: Option<Vec<char>>,
    lexbuf: Vec<char>,
    pub tokfd: i32,
    pub incmdpos: bool,
    pub incond: i32,
    pub inredir: bool,
    pub intypeset: bool,
    pub infor: i32,
    inrepeat: i32,
    dbparens: bool,
    oldpos: bool,
    pub noaliases: bool,
    pub wordbeg: i32,
    pub parbegin: i32,
    pub parend: i32,
    opts: LexOpts,
}

impl ZleLexer {
    /// A lexer reading `input`, set up as strinbeg() leaves it.
    pub fn new(input: &[char], lexflags: u32, cs: i32, ll: i32) -> ZleLexer {
        ZleLexer {
            buf: input.to_vec(),
            from_alias: vec![false; input.len()],
            pos: 0,
            cur_alias: false,
            aliases_in_use: Vec::new(),
            lexstop: false,
            errflag: false,
            lexflags,
            cs,
            ll,
            tok: T::Endinput,
            tokstr: None,
            lexbuf: Vec::new(),
            tokfd: -1,
            incmdpos: true,
            incond: 0,
            inredir: false,
            intypeset: false,
            infor: 0,
            inrepeat: 0,
            dbparens: false,
            oldpos: false,
            noaliases: false,
            wordbeg: 0,
            parbegin: -1,
            parend: -1,
            opts: LexOpts {
                shglob: is_set("shglob"),
                kshglob: is_set("kshglob"),
                ignorebraces: is_set("ignorebraces"),
                ignoreclosebraces: is_set("ignoreclosebraces"),
                rcquotes: is_set("rcquotes"),
                cshjunkiequotes: is_set("cshjunkiequotes"),
                interactivecomments: is_set("interactivecomments"),
                aliases: is_set("aliases"),
                posixaliases: is_set("posixaliases"),
                shortloops: is_set("shortloops"),
                shortrepeat: is_set("shortrepeat"),
            },
        }
    }

    /// Characters left to read (zsh's inbufct).
    pub fn inbufct(&self) -> i32 {
        (self.buf.len() - self.pos) as i32
    }

    fn zle(&self) -> bool {
        self.lexflags & LEXFLAGS_ZLE != 0 && !self.cur_alias
    }

    fn hgetc(&mut self) -> char {
        if self.lexstop {
            return ' ';
        }
        loop {
            if self.pos >= self.buf.len() {
                self.lexstop = true;
                return ' ';
            }
            let c = self.buf[self.pos];
            self.cur_alias = self.from_alias[self.pos];
            self.pos += 1;
            let pos = self.pos;
            self.aliases_in_use.retain(|(_, end)| pos <= *end);
            // Token characters in the input are dropped, as zsh's ingetc.
            if !itok(c) {
                return c;
            }
        }
    }

    /// Puts back the last character read; nothing at end of input.
    fn hungetc(&mut self) {
        if !self.lexstop && self.pos > 0 {
            self.pos -= 1;
            self.cur_alias = self.from_alias[self.pos];
        }
    }

    fn add(&mut self, c: char) {
        self.lexbuf.push(c);
    }

    fn take_tokstr(&mut self) {
        self.tokstr = Some(self.lexbuf.clone());
    }

    fn setparbegin(&mut self) {
        if self.zle() && self.cs >= self.ll + 1 - self.inbufct() {
            self.parbegin = self.inbufct();
        }
    }

    fn setparend(&mut self) {
        if self.zle() && self.parbegin != -1 && self.parend == -1 {
            if self.cs >= self.ll + 1 - self.inbufct() {
                self.parbegin = -1;
            } else {
                self.parend = self.inbufct();
            }
        }
    }

    /// Splices alias text in to be read next.
    fn inpush_alias(&mut self, text: &str, name: &str) {
        let t = chars(text);
        let n = t.len();
        let at = self.pos;
        self.buf.splice(at..at, t);
        self.from_alias.splice(at..at, std::iter::repeat_n(true, n));
        for (_, end) in self.aliases_in_use.iter_mut() {
            if *end >= at {
                *end += n;
            }
        }
        self.aliases_in_use.push((name.to_string(), at + n));
        self.lexstop = false;
    }

    fn alias_in_use(&self, name: &str) -> bool {
        self.aliases_in_use.iter().any(|(n, _)| n == name)
    }

    /// Reads a token and handles aliases and reserved words (zshlex).
    pub fn zshlex(&mut self) {
        if self.tok == T::Lexerr {
            return;
        }
        loop {
            if self.inrepeat > 0 {
                self.inrepeat += 1;
            }
            if self.inrepeat == 3 && (self.opts.shortloops || self.opts.shortrepeat) {
                self.incmdpos = true;
            }
            self.tok = self.gettok();
            if self.tok == T::Endinput || !self.exalias() {
                break;
            }
        }
        if self.tok == T::Semi || self.tok == T::Newlin {
            self.tok = T::Seper;
        }
    }

    /// zshlex, keeping track of command position and redirections.
    pub fn ctxtlex(&mut self) {
        self.zshlex();
        match self.tok {
            T::Seper
            | T::Newlin
            | T::Semi
            | T::Dsemi
            | T::Semiamp
            | T::Semibar
            | T::Amper
            | T::Amperbang
            | T::Inpar
            | T::Inbrace
            | T::Dbar
            | T::Damper
            | T::Bar
            | T::Baramp
            | T::Inoutpar
            | T::Doloop
            | T::Then
            | T::Elif
            | T::Else
            | T::Doutbrack => self.incmdpos = true,
            T::String | T::Typeset | T::Envarray | T::Outpar | T::Case | T::Dinbrack => {
                self.incmdpos = false
            }
            _ => {}
        }
        if self.tok != T::Dinpar {
            self.infor = if self.tok == T::For { 2 } else { 0 };
        }
        if self.tok.is_redirop() || matches!(self.tok, T::For | T::Foreach | T::Select) {
            self.inredir = true;
            self.oldpos = self.incmdpos;
            self.incmdpos = false;
        } else if self.inredir {
            self.incmdpos = self.oldpos;
            self.inredir = false;
        }
    }

    /// Reports the position of a word to completion (gotword).
    fn gotword(&mut self) {
        let addedx = addedx();
        let nwe = self.ll + 1 - self.inbufct() + i32::from(addedx == 2);
        if self.cs <= nwe {
            let nwb = self.ll - self.wordbeg + addedx;
            if self.cs >= nwb {
                set_wb(nwb);
                set_we(nwe);
            } else {
                set_wb(self.cs + addedx);
                if we() < wb() {
                    set_we(wb());
                }
            }
            self.lexflags = 0;
        }
    }

    fn checkalias(&mut self, text: &str) -> bool {
        if text.is_empty() || self.noaliases || !self.opts.aliases {
            return false;
        }
        if self.opts.posixaliases && !(self.tok == T::String && reswd(text).is_none()) {
            return false;
        }
        if !self.alias_in_use(text) {
            let regular = if self.incmdpos && self.tok == T::String {
                crate::shell::alias_get(text)
            } else {
                None
            };
            if let Some(mut a) = regular.or_else(|| crate::shell::galias_get(text)) {
                if !self.lexstop {
                    let c = self.hgetc();
                    self.hungetc();
                    if !iblank(c) {
                        a.push(' ');
                    }
                }
                self.inpush_alias(&a, text);
                return true;
            }
        }
        if self.incmdpos {
            if let Some((stem, ext)) = text.rsplit_once('.') {
                if !stem.is_empty() && !ext.is_empty() && !self.alias_in_use(ext) {
                    if let Some(cmd) = crate::shell::salias_for(text) {
                        self.inpush_alias(&format!("{} {}", cmd, text), ext);
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Expands aliases and recognises reserved words; true if an alias
    /// was expanded and the token must be read again.
    fn exalias(&mut self) -> bool {
        let Some(ts) = self.tokstr.clone() else {
            if self.tok == T::Newlin {
                return false;
            }
            let text = self.tok.text().unwrap_or("");
            return self.checkalias(text);
        };
        let tokenized = has_token(&ts);
        let text: String = ts
            .iter()
            .map_while(|&c| if itok(c) { ztoken(c) } else { Some(c) })
            .collect();
        if self.zle() {
            let zp = self.lexflags;
            self.gotword();
            if zp & LEXFLAGS_ZLE != 0 && self.lexflags == 0 {
                return false;
            }
        }
        if self.tok == T::String {
            if (!tokenized || !self.opts.posixaliases) && self.checkalias(&text) {
                return true;
            }
            let rw = if self.incmdpos
                || (!self.opts.ignorebraces && !self.opts.ignoreclosebraces && text == "}")
            {
                reswd(&text)
            } else {
                None
            };
            if let Some(rw) = rw {
                self.tok = rw;
                self.inrepeat = i32::from(rw == T::Repeat);
                if rw == T::Dinbrack {
                    self.incond = 1;
                }
            } else if self.incond != 0 && text == "]]" {
                self.tok = T::Doutbrack;
                self.incond = 0;
            } else if self.incond == 1 && text == "!" {
                self.tok = T::Bang;
            }
        }
        false
    }

    fn gettok(&mut self) -> Lextok {
        let o = self.opts;
        'beginning: loop {
            self.tokstr = None;
            let mut c;
            loop {
                c = self.hgetc();
                if !(iblank(c) && !self.lexstop) {
                    break;
                }
            }
            if self.lexstop {
                return if self.errflag { T::Lexerr } else { T::Endinput };
            }
            if self.zle() {
                self.wordbeg = self.inbufct();
            }
            if self.dbparens {
                self.lexbuf.clear();
                self.hungetc();
                let r = self.dquote_parse(if self.infor != 0 { ';' } else { ')' }, false);
                self.take_tokstr();
                if r == 0 && self.infor != 0 {
                    self.infor -= 1;
                    return T::Dinpar;
                }
                if r == 0 && self.hgetc() == ')' {
                    self.dbparens = false;
                    return T::Doutpar;
                }
                self.hungetc();
                return T::Lexerr;
            }
            let mut peekfd: i32 = -1;
            if let Some(dig) = c.to_digit(10) {
                let d = self.hgetc();
                if d == '&' {
                    let d2 = self.hgetc();
                    if d2 == '>' {
                        peekfd = dig as i32;
                        self.hungetc();
                        c = '&';
                    } else {
                        self.hungetc();
                        self.lexstop = false;
                        self.hungetc();
                    }
                } else if d == '>' || d == '<' {
                    peekfd = dig as i32;
                    c = d;
                } else {
                    self.hungetc();
                    self.lexstop = false;
                }
            }
            if c == '#' && (o.interactivecomments || self.lexflags == 0) {
                loop {
                    let c2 = self.hgetc();
                    if c2 == '\n' || self.lexstop {
                        break;
                    }
                }
                return if self.errflag { T::Lexerr } else { T::Newlin };
            }
            let mut unpeek = false;
            match c {
                '\\' => {
                    let d = self.hgetc();
                    if d == '\n' {
                        continue 'beginning;
                    }
                    self.hungetc();
                    self.lexstop = false;
                }
                '\n' => return T::Newlin,
                ';' => {
                    let d = self.hgetc();
                    match d {
                        ';' => return T::Dsemi,
                        '&' => return T::Semiamp,
                        '|' => return T::Semibar,
                        _ => {}
                    }
                    self.hungetc();
                    self.lexstop = false;
                    return T::Semi;
                }
                '&' => {
                    let d = self.hgetc();
                    if d == '&' {
                        return T::Damper;
                    } else if d == '!' || d == '|' {
                        return T::Amperbang;
                    } else if d == '>' {
                        self.tokfd = peekfd;
                        let d = self.hgetc();
                        if d == '!' || d == '|' {
                            return T::Outangampbang;
                        } else if d == '>' {
                            let d = self.hgetc();
                            if d == '!' || d == '|' {
                                return T::Doutangampbang;
                            }
                            self.hungetc();
                            self.lexstop = false;
                            return T::Doutangamp;
                        }
                        self.hungetc();
                        self.lexstop = false;
                        return T::Ampoutang;
                    }
                    self.hungetc();
                    self.lexstop = false;
                    return T::Amper;
                }
                '|' => {
                    let d = self.hgetc();
                    if d == '|' {
                        return T::Dbar;
                    } else if d == '&' {
                        return T::Baramp;
                    }
                    self.hungetc();
                    self.lexstop = false;
                    return T::Bar;
                }
                '(' => {
                    let d = self.hgetc();
                    if d == '(' {
                        if self.infor != 0 {
                            self.dbparens = true;
                            return T::Dinpar;
                        }
                        if self.incmdpos || (o.shglob && !o.kshglob) {
                            self.lexbuf.clear();
                            match self.cmd_or_math() {
                                CmdOrMath::Math => {
                                    self.take_tokstr();
                                    return T::Dinpar;
                                }
                                CmdOrMath::Cmd => {
                                    self.tokstr = None;
                                    return T::Inpar;
                                }
                                CmdOrMath::Err => {
                                    self.take_tokstr();
                                    return T::Lexerr;
                                }
                            }
                        }
                    } else if d == ')' {
                        return T::Inoutpar;
                    }
                    self.hungetc();
                    self.lexstop = false;
                    if o.shglob || self.incond == 1 || self.incmdpos {
                        return T::Inpar;
                    }
                }
                ')' => return T::Outpar,
                '<' => {
                    let d = self.hgetc();
                    if d == '(' {
                        self.hungetc();
                        self.lexstop = false;
                        unpeek = true;
                    } else {
                        let peek;
                        if d == '>' {
                            peek = T::Inoutang;
                        } else if d == '<' {
                            let e = self.hgetc();
                            if e == '(' {
                                self.hungetc();
                                self.hungetc();
                                peek = T::Inang;
                            } else if e == '<' {
                                peek = T::Trinang;
                            } else if e == '-' {
                                peek = T::Dinangdash;
                            } else {
                                self.hungetc();
                                self.lexstop = false;
                                peek = T::Dinang;
                            }
                        } else if d == '&' {
                            peek = T::Inangamp;
                        } else {
                            self.hungetc();
                            if self.isnumglob() {
                                unpeek = true;
                                peek = T::Nulltok;
                            } else {
                                peek = T::Inang;
                            }
                        }
                        if !unpeek {
                            self.tokfd = peekfd;
                            return peek;
                        }
                    }
                }
                '>' => {
                    let d = self.hgetc();
                    if d == '(' {
                        self.hungetc();
                        unpeek = true;
                    } else {
                        let peek = if d == '&' {
                            let d = self.hgetc();
                            if d == '!' || d == '|' {
                                T::Outangampbang
                            } else {
                                self.hungetc();
                                self.lexstop = false;
                                T::Outangamp
                            }
                        } else if d == '!' || d == '|' {
                            T::Outangbang
                        } else if d == '>' {
                            let d = self.hgetc();
                            if d == '&' {
                                let d = self.hgetc();
                                if d == '!' || d == '|' {
                                    T::Doutangampbang
                                } else {
                                    self.hungetc();
                                    self.lexstop = false;
                                    T::Doutangamp
                                }
                            } else if d == '!' || d == '|' {
                                T::Doutangbang
                            } else if d == '(' {
                                self.hungetc();
                                self.hungetc();
                                T::Outang
                            } else {
                                self.hungetc();
                                self.lexstop = false;
                                T::Doutang
                            }
                        } else {
                            self.hungetc();
                            self.lexstop = false;
                            T::Outang
                        };
                        self.tokfd = peekfd;
                        return peek;
                    }
                }
                _ => {}
            }
            if unpeek && peekfd != -1 {
                self.hungetc();
                c = char::from_digit(peekfd as u32, 10).unwrap_or('0');
            }
            return self.gettokstr(c, false);
        }
    }

    /// Is the input after `<` a numeric glob `<a-b>`? Reads nothing.
    fn isnumglob(&mut self) -> bool {
        let save = self.pos;
        let mut ec = '-';
        let mut ret = false;
        loop {
            let c = self.hgetc();
            if self.lexstop {
                self.lexstop = false;
                break;
            }
            if !c.is_ascii_digit() {
                if c != ec {
                    break;
                }
                if ec == '>' {
                    ret = true;
                    break;
                }
                ec = '>';
            }
        }
        self.pos = save;
        if save > 0 {
            self.cur_alias = self.from_alias[save - 1];
        }
        ret
    }

    /// After `((`: math, or a command in a subshell (then the second `(`
    /// is left to be read again).
    fn cmd_or_math(&mut self) -> CmdOrMath {
        let oldlen = self.lexbuf.len();
        let entry = self.pos;
        let r = self.dquote_parse(')', false);
        if r == 0 {
            if self.hgetc() == ')' {
                return CmdOrMath::Math;
            }
            self.hungetc();
            self.lexstop = false;
        } else if self.lexstop {
            return CmdOrMath::Err;
        }
        self.lexstop = false;
        self.lexbuf.truncate(oldlen);
        if self.errflag {
            return CmdOrMath::Err;
        }
        self.pos = entry.saturating_sub(1);
        self.cur_alias = self.from_alias.get(self.pos).copied().unwrap_or(false);
        CmdOrMath::Cmd
    }

    /// After `$(`: `$((...))` or `$(...)`.
    fn cmd_or_math_sub(&mut self) -> CmdOrMath {
        let mut c = self.hgetc();
        if c == '\\' {
            c = self.hgetc();
            if c != '\n' {
                self.hungetc();
                self.hungetc();
                self.lexstop = false;
                return if self.skipcomm() {
                    CmdOrMath::Err
                } else {
                    CmdOrMath::Cmd
                };
            }
            c = self.hgetc();
        }
        if c == '(' {
            let lexpos = self.lexbuf.len();
            self.add(INPAR);
            self.add('(');
            match self.cmd_or_math() {
                CmdOrMath::Math => {
                    self.lexbuf[lexpos] = INPARMATH;
                    self.add(')');
                    return CmdOrMath::Math;
                }
                CmdOrMath::Err => return CmdOrMath::Err,
                CmdOrMath::Cmd => self.lexbuf.truncate(lexpos),
            }
        } else {
            self.hungetc();
            self.lexstop = false;
        }
        if self.skipcomm() {
            CmdOrMath::Err
        } else {
            CmdOrMath::Cmd
        }
    }

    /// Skips the `(...)` of a command or process substitution, the `(`
    /// having been read, keeping its text; true if the input ended first.
    fn skipcomm(&mut self) -> bool {
        let save_infor = self.infor;
        self.infor = 0;
        self.setparbegin();
        self.add(INPAR);
        let start = self.pos.saturating_sub(1);
        let rest: Vec<char> = self.buf[start..].to_vec();
        match crate::lexer::scan_paren(&rest, 0) {
            Some(end) if end >= 2 => {
                for &ch in &rest[1..end - 1] {
                    if !itok(ch) {
                        self.add(ch);
                    }
                }
                self.pos = start + end;
                self.cur_alias = self.from_alias[self.pos - 1];
                self.setparend();
            }
            _ => {
                for &ch in rest.iter().skip(1) {
                    if !itok(ch) {
                        self.add(ch);
                    }
                }
                self.pos = self.buf.len();
                self.lexstop = true;
            }
        }
        self.infor = save_infor;
        self.lexstop
    }

    /// Reads up to `endchar` as inside double quotes. 0 on success, else
    /// nonzero (zsh's character to unget, or 1).
    fn dquote_parse(&mut self, endchar: char, sub: bool) -> i32 {
        let (mut pct, mut brct, mut bct, mut intick) = (0i32, 0i32, 0i32, 0i32);
        let mut err: i32 = 0;
        let mut cmdsubst = 0i32;
        let mut bskip = 0i32;
        let math = endchar == ')' || endchar == ']' || self.infor != 0;
        let zlemath = math && self.cs > self.ll + addedx() - self.inbufct();
        let cshjunkie = self.opts.cshjunkiequotes;
        let mut c: char;
        'outer: loop {
            c = self.hgetc();
            if !((c != endchar || bct != 0 || (math && (pct > 0 || brct > 0)) || intick != 0)
                && !self.lexstop)
            {
                break;
            }
            loop {
                match c {
                    '\\' => {
                        c = self.hgetc();
                        if c != '\n' {
                            if c == '$'
                                || c == '\\'
                                || (c == '}' && intick == 0 && bct != 0)
                                || c == endchar
                                || c == '`'
                                || (endchar == ']'
                                    && (matches!(c, '[' | ']' | '(' | ')' | '{' | '}')
                                        || (c == '"' && sub)))
                            {
                                self.add(BNULL);
                            } else {
                                self.add('\\');
                                continue;
                            }
                        } else if sub || !cshjunkie || endchar != '"' {
                            continue 'outer;
                        }
                    }
                    '\n' => {
                        err = i32::from(!sub && cshjunkie && endchar == '"');
                    }
                    '$' => {
                        if intick == 0 {
                            c = self.hgetc();
                            if c == '(' {
                                self.add(QSTRING);
                                match self.cmd_or_math_sub() {
                                    CmdOrMath::Cmd => c = OUTPAR,
                                    CmdOrMath::Math => c = OUTPARMATH,
                                    CmdOrMath::Err => err = 1,
                                }
                            } else if c == '[' {
                                self.add(STRING);
                                self.add(INBRACK);
                                err = self.dquote_parse(']', sub);
                                c = OUTBRACK;
                            } else if c == '{' {
                                self.add(QSTRING);
                                c = INBRACE;
                                bct += 1;
                            } else if c == '$' {
                                self.add(QSTRING);
                            } else {
                                self.hungetc();
                                self.lexstop = false;
                                c = QSTRING;
                            }
                        }
                    }
                    '{' => {
                        if cmdsubst != 0 && intick == 0 {
                            // ${| ... } is tokenized as if unquoted.
                            c = INBRACE;
                            bskip += 1;
                        }
                    }
                    '}' => {
                        if intick == 0 && bct != 0 {
                            c = OUTBRACE;
                            if bskip != 0 {
                                bskip -= 1;
                            } else {
                                if bct == cmdsubst {
                                    cmdsubst = 0;
                                }
                                bct -= 1;
                            }
                        }
                    }
                    '`' => {
                        c = QTICK;
                        intick = i32::from(intick == 0);
                        if intick != 0 {
                            self.setparbegin();
                        } else {
                            self.setparend();
                        }
                    }
                    '\'' => {
                        if intick != 0 {
                            intick = if intick == 1 { 2 } else { 1 };
                        }
                    }
                    '(' => {
                        if !math || bct == 0 {
                            pct += 1;
                        }
                    }
                    ')' => {
                        if !math || bct == 0 {
                            err = i32::from(pct == 0 && math);
                            pct -= 1;
                        }
                    }
                    '[' => {
                        if !math || bct == 0 {
                            brct += 1;
                        }
                    }
                    ']' => {
                        if !math || bct == 0 {
                            err = i32::from(brct == 0 && math);
                            brct -= 1;
                        }
                    }
                    '"' if intick == 0 && (endchar == '"' || bct != 0) => {
                        if bct != 0 {
                            self.add(DNULL);
                            err = self.dquote_parse('"', sub);
                            c = DNULL;
                        } else {
                            err = 1;
                        }
                    }
                    _ => {}
                }
                break;
            }
            if err != 0 || self.lexstop {
                break;
            }
            self.add(c);
            if cmdsubst == 0 && c == INBRACE {
                // ${|...} nofork command substitution.
                let c2 = self.hgetc();
                if !self.lexstop {
                    if c2 == '|' || inblank(c2) {
                        cmdsubst = bct;
                    }
                    self.hungetc();
                }
            }
        }
        if self.lexstop {
            err = 1;
        } else if err == 1 {
            err = c as i32;
        }
        if zlemath && self.cs <= self.ll + 1 - self.inbufct() {
            set_inwhat(IN_MATH);
        }
        err
    }

    /// Reads the rest of a word starting with `c`, tokenizing it
    /// (gettokstr). With `sub`, a whole string is tokenized as by
    /// parse_subst_string.
    fn gettokstr(&mut self, mut c: char, sub: bool) -> Lextok {
        let o = self.opts;
        let (mut bct, mut pct, mut brct) = (0i32, 0i32, 0i32);
        let mut seen_brct = false;
        let mut fdpar = false;
        let mut intpos = 1i32;
        let mut in_brace_param = 0i32;
        let mut cmdsubst = 0i32;
        let mut unmatched = false;
        let mut in_pattern = 0i32;
        let mut peek = T::String;
        if !sub {
            self.lexbuf.clear();
        }
        'lx: loop {
            let inbl = inblank(c);
            if fdpar && !inbl && c != ')' {
                fdpar = false;
            }
            let act = if inbl && in_brace_param == 0 && pct == 0 {
                Lx2::Break
            } else {
                let a = lexact2(c);
                c = lextok2(c);
                a
            };
            match act {
                Lx2::Break => {
                    if in_brace_param == 0 && !sub {
                        break 'lx;
                    }
                }
                Lx2::Outpar => {
                    if fdpar {
                        // A single word `(   )`.
                        self.add(c);
                        self.take_tokstr();
                        return T::Inoutpar;
                    }
                    if !((sub || in_brace_param != 0) && o.shglob) {
                        if in_brace_param == 0 && pct == 0 {
                            if !sub {
                                break 'lx;
                            }
                        } else {
                            if in_brace_param == 0 {
                                pct -= 1;
                            }
                            c = OUTPAR;
                        }
                    }
                }
                Lx2::Bar => {
                    if pct == 0 && in_brace_param == 0 {
                        if !sub {
                            break 'lx;
                        }
                    } else if !o.shglob || (!sub && in_brace_param == 0) {
                        c = BAR;
                    }
                }
                Lx2::String => {
                    let mut e = self.hgetc();
                    let mut handled = false;
                    if e == '\\' {
                        e = self.hgetc();
                        if e != '\n' {
                            self.hungetc();
                            self.hungetc();
                            self.lexstop = false;
                            handled = true;
                        } else {
                            e = self.hgetc();
                        }
                    }
                    if !handled {
                        if e == '[' {
                            self.add(STRING);
                            self.add(INBRACK);
                            if self.dquote_parse(']', sub) != 0 {
                                peek = T::Lexerr;
                                break 'lx;
                            }
                            c = OUTBRACK;
                        } else if e == '(' {
                            self.add(STRING);
                            match self.cmd_or_math_sub() {
                                CmdOrMath::Cmd => c = OUTPAR,
                                CmdOrMath::Math => c = OUTPARMATH,
                                CmdOrMath::Err => {
                                    peek = T::Lexerr;
                                    break 'lx;
                                }
                            }
                        } else if e == '{' {
                            self.add(c);
                            c = INBRACE;
                            bct += 1;
                            if in_brace_param == 0 {
                                in_brace_param = bct;
                                if in_brace_param != 0 {
                                    seen_brct = false;
                                }
                            }
                        } else {
                            self.hungetc();
                            self.lexstop = false;
                        }
                    }
                }
                Lx2::Inbrack => {
                    if in_brace_param == 0 {
                        brct += 1;
                        seen_brct = true;
                    }
                    c = INBRACK;
                }
                Lx2::Outbrack => {
                    if in_brace_param == 0 {
                        brct -= 1;
                    }
                    if brct < 0 {
                        brct = 0;
                    }
                    c = OUTBRACK;
                }
                Lx2::Inpar => {
                    let mut skip = false;
                    if o.shglob {
                        if sub || in_brace_param != 0 {
                            skip = true;
                        } else if !o.kshglob && !self.lexbuf.is_empty() {
                            break 'lx;
                        }
                    }
                    if !skip {
                        if in_brace_param == 0 {
                            if !sub {
                                let e = self.hgetc();
                                self.hungetc();
                                self.lexstop = false;
                                // `()`, or with SH_GLOB what looks like a
                                // ksh function definition `name( )`.
                                if e == ')'
                                    || (o.shglob
                                        && inblank(e)
                                        && bct == 0
                                        && brct == 0
                                        && intpos == 0
                                        && self.incmdpos)
                                {
                                    break 'lx;
                                }
                            }
                            if pct == 0 && o.shglob && intpos != 0 && bct == 0 && brct == 0 {
                                fdpar = true;
                            }
                            pct += 1;
                        }
                        c = INPAR;
                    }
                }
                Lx2::Inbrace => {
                    if (o.ignorebraces && cmdsubst == 0) || sub {
                        c = '{';
                    } else {
                        if self.lexbuf.is_empty() && self.incmdpos {
                            self.add('{');
                            self.take_tokstr();
                            return T::String;
                        }
                        bct += 1;
                    }
                }
                Lx2::Outbrace => {
                    if !((o.ignorebraces || sub) && in_brace_param == 0) && bct != 0 {
                        if bct == in_brace_param {
                            in_brace_param = 0;
                            cmdsubst = 0;
                            in_pattern = 0;
                        }
                        bct -= 1;
                        c = OUTBRACE;
                    }
                }
                Lx2::Comma => {
                    if !o.ignorebraces && !sub && bct > in_brace_param {
                        c = COMMA;
                    }
                }
                Lx2::Outang => {
                    if in_brace_param == 0 && !sub {
                        let e = self.hgetc();
                        if e != '(' {
                            self.hungetc();
                            self.lexstop = false;
                            break 'lx;
                        }
                        self.add(OUTANGPROC);
                        if self.skipcomm() {
                            peek = T::Lexerr;
                            break 'lx;
                        }
                        c = OUTPAR;
                    }
                }
                Lx2::Inang => {
                    if !(o.shglob && sub) {
                        let e = self.hgetc();
                        if !(in_brace_param != 0 || sub) && e == '(' {
                            self.add(INANG);
                            if self.skipcomm() {
                                peek = T::Lexerr;
                                break 'lx;
                            }
                            c = OUTPAR;
                        } else {
                            self.hungetc();
                            if in_brace_param == 0 && self.isnumglob() {
                                self.add(INANG);
                                loop {
                                    c = self.hgetc();
                                    if c == '>' || self.lexstop {
                                        break;
                                    }
                                    self.add(c);
                                }
                                c = OUTANG;
                            } else {
                                self.lexstop = false;
                                if in_brace_param == 0 && !sub {
                                    break 'lx;
                                }
                            }
                        }
                    }
                }
                Lx2::Equals => {
                    if !sub {
                        if intpos != 0 {
                            let e = self.hgetc();
                            if e != '(' {
                                self.hungetc();
                                self.lexstop = false;
                                c = EQUALS;
                            } else {
                                self.add(EQUALS);
                                if self.skipcomm() {
                                    peek = T::Lexerr;
                                    break 'lx;
                                }
                                c = OUTPAR;
                            }
                        } else if peek != T::Envstring
                            && (self.incmdpos || self.intypeset)
                            && bct == 0
                            && brct == 0
                        {
                            let lb = &self.lexbuf;
                            let mut t;
                            if lb.first().is_some_and(|x| x.is_ascii_digit()) {
                                t = 1;
                                while t < lb.len() && lb[t].is_ascii_digit() {
                                    t += 1;
                                }
                            } else {
                                t = itype_end_namespc(lb, 0, false);
                                if t < lb.len() {
                                    t = skipparens(lb, t, INBRACK, OUTBRACK).1;
                                }
                            }
                            if lb.get(t) == Some(&'+') {
                                t += 1;
                            }
                            if t == lb.len() {
                                let e = self.hgetc();
                                if e == '(' {
                                    self.take_tokstr();
                                    return T::Envarray;
                                }
                                self.hungetc();
                                self.lexstop = false;
                                peek = T::Envstring;
                                intpos = 2;
                            } else {
                                c = EQUALS;
                            }
                        } else {
                            c = EQUALS;
                        }
                    }
                }
                Lx2::Bkslash => {
                    c = self.hgetc();
                    if c == '\n' {
                        c = self.hgetc();
                        if !self.lexstop {
                            continue 'lx;
                        }
                    } else {
                        self.add(BNULL);
                    }
                    if self.lexstop {
                        break 'lx;
                    }
                }
                Lx2::Quote => {
                    let strquote = self.lexbuf.last() == Some(&STRING);
                    self.add(SNULL);
                    loop {
                        loop {
                            c = self.hgetc();
                            if c == '\'' || self.lexstop {
                                break;
                            }
                            if strquote && c == '\\' {
                                c = self.hgetc();
                                if self.lexstop {
                                    break;
                                }
                                // Escaped backslashes and quotes in $'...'
                                // must be strippable by completion.
                                if c == '\\' || c == '\'' {
                                    self.add(BNULL);
                                } else {
                                    self.add('\\');
                                }
                            } else if !sub && o.cshjunkiequotes && c == '\n' {
                                if self.lexbuf.last() == Some(&'\\') {
                                    self.lexbuf.pop();
                                } else {
                                    break;
                                }
                            } else if in_pattern != 0 && c == '/' {
                                self.add(BNULL);
                            }
                            self.add(c);
                        }
                        if c != '\'' || self.lexstop {
                            unmatched = true;
                            peek = T::Lexerr;
                            break 'lx;
                        }
                        let e = self.hgetc();
                        if e != '\'' || !o.rcquotes || strquote {
                            break;
                        }
                        self.add(c);
                    }
                    self.hungetc();
                    self.lexstop = false;
                    c = SNULL;
                }
                Lx2::Dquote => {
                    self.add(DNULL);
                    if self.dquote_parse('"', sub) != 0 {
                        unmatched = true;
                        peek = T::Lexerr;
                        break 'lx;
                    }
                    c = DNULL;
                }
                Lx2::Bquote => {
                    self.add(TICK);
                    self.setparbegin();
                    loop {
                        c = self.hgetc();
                        if c == '`' || self.lexstop {
                            break;
                        }
                        if c == '\\' {
                            c = self.hgetc();
                            if c != '\n' {
                                self.add(if c == '`' || c == '\\' || c == '$' {
                                    BNULL
                                } else {
                                    '\\'
                                });
                                self.add(c);
                            } else if !sub && o.cshjunkiequotes {
                                self.add(c);
                            }
                        } else {
                            if !sub && o.cshjunkiequotes && c == '\n' {
                                break;
                            }
                            self.add(c);
                        }
                    }
                    if c != '`' || self.lexstop {
                        unmatched = true;
                        peek = T::Lexerr;
                        break 'lx;
                    }
                    c = TICK;
                    self.setparend();
                }
                Lx2::Dash => c = DASH,
                Lx2::Bang => c = if seen_brct && brct != 0 { BANG } else { '!' },
                Lx2::Other => {
                    if in_brace_param != 0 && c == '/' {
                        if in_pattern == 0 {
                            in_pattern = 2;
                        } else {
                            in_pattern -= 1;
                        }
                    }
                }
            }
            self.add(c);
            c = self.hgetc();
            if intpos != 0 {
                intpos -= 1;
            }
            if self.lexstop {
                break 'lx;
            }
            if cmdsubst == 0
                && in_brace_param != 0
                && act == Lx2::String
                && (c == '|' || c == BAR || c == '{' || c == INBRACE || inblank(c))
            {
                cmdsubst = in_brace_param;
            } else if in_pattern == 2 && c != '/' {
                in_pattern = 1;
            }
        }
        if self.errflag {
            self.take_tokstr();
            return T::Lexerr;
        }
        self.hungetc();
        if unmatched {
            self.errflag = true;
        }
        if in_brace_param != 0 {
            // zsh: "closing brace expected".
            self.errflag = true;
        } else if !o.ignorebraces
            && !sub
            && self.lexbuf.len() > 1
            && peek == T::String
            && self.lexbuf.last() == Some(&'}')
            && self.lexbuf[self.lexbuf.len() - 2] != BNULL
        {
            // So that {foo} command syntax works.
            self.lexbuf.pop();
            self.lexstop = false;
            self.hungetc();
        }
        self.take_tokstr();
        peek
    }
}

/// Tokenizes `s` as a command-line argument that may contain separators
/// (zsh's parse_subst_string); nonzero on a parse error, which leaves `s`
/// untokenized.
pub fn parse_subst_string(z: &Zle, s: &mut Vec<char>) -> i32 {
    if s.is_empty() || s.as_slice() == [NULARG] {
        return 0;
    }
    let raw = untokenize(s);
    let mut lx = ZleLexer::new(&raw, 0, z.cs as i32, z.line.len() as i32);
    let c = lx.hgetc();
    let ctok = lx.gettokstr(c, true);
    if ctok == T::Lexerr {
        *s = raw;
        return 1;
    }
    let mut t = lx.tokstr.unwrap_or_default();
    // $'...' is decoded in place.
    let mut ptr = 0;
    while ptr < t.len() {
        if t[ptr] == STRING && t.get(ptr + 1) == Some(&SNULL) {
            let (dec, used) = getkeystring_dollars(&t[ptr + 2..]);
            let len = used + 2;
            if dec.len() > len {
                *s = t;
                return 1;
            }
            let end = (ptr + len).min(t.len());
            t.splice(ptr..end, dec.iter().copied());
            ptr += dec.len();
        } else {
            ptr += 1;
        }
    }
    *s = t;
    0
}

// ---- state of zle_tricky.c not in CompState -----------------------------

thread_local! {
    /// docomplete() is running (its static `active`).
    static DOCOMPLETE_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// Completing only the prefix (expand-or-complete-prefix; comppref).
    static COMPPREF: Cell<bool> = const { Cell::new(false) };
}

fn is_hashed(name: &str) -> bool {
    crate::shell::assoc_keys(name).is_some()
}

fn param_names() -> Vec<String> {
    crate::shell::with_shell(|s| {
        s.params
            .iter()
            .filter(|(_, p)| p.is_set())
            .map(|(k, _)| k.clone())
            .collect()
    })
}

// ---- widgets ------------------------------------------------------------

/// Whether to insert a TAB instead of completing.
fn usetab(z: &mut Zle) -> bool {
    if z.keybuf.first() != Some(&b'\t') || z.keybuf.len() > 1 {
        return false;
    }
    let mut i = z.cs;
    while i > 0 && z.line[i - 1] != '\n' {
        if z.line[i - 1] != '\t' && z.line[i - 1] != ' ' {
            return false;
        }
        i -= 1;
    }
    if z.comp.compfunc.is_some() {
        z.comp.wouldinstab = 1;
        return false;
    }
    true
}

fn lastchar_is_tab(z: &Zle) -> bool {
    z.lastchar == '\t' as i32
}

/// Runs a completion widget made with `zle -C`: the builtin widget
/// `builtin` with completion function `func`.
pub fn completecall(z: &mut Zle, builtin: &str, func: &str, args: &[String]) -> i32 {
    z.comp.cfargs = args.to_vec();
    z.comp.cfret = 0;
    z.comp.compfunc = Some(func.to_string());
    if let Some(f) = crate::zle::widgets::lookup(builtin.trim_start_matches('.')) {
        if f(z, &[]) != 0 && z.comp.cfret == 0 {
            z.comp.cfret = 1;
        }
    }
    z.comp.compfunc = None;
    z.comp.cfret
}

fn bashlist_or(z: &mut Zle, lst: i32) -> i32 {
    if z.comp.lastambig == 1 && is_set("bashautolist") && z.comp.usemenu == 0 && z.comp.menucmp == 0
    {
        z.comp.bashlistfirst = 1;
        let ret = docomplete(z, COMP_LIST_COMPLETE);
        z.comp.bashlistfirst = 0;
        z.comp.lastambig = 2;
        ret
    } else {
        docomplete(z, lst)
    }
}

fn setusemenu(z: &mut Zle) {
    z.comp.usemenu = i32::from(is_set("menucomplete"));
    z.comp.useglob = i32::from(is_set("globcomplete"));
    z.comp.wouldinstab = 0;
}

pub fn completeword(z: &mut Zle, args: &[String]) -> i32 {
    setusemenu(z);
    if lastchar_is_tab(z) && usetab(z) {
        return misc::selfinsert(z, args);
    }
    bashlist_or(z, COMP_COMPLETE)
}

pub fn menucomplete(z: &mut Zle, args: &[String]) -> i32 {
    z.comp.usemenu = 1;
    z.comp.useglob = i32::from(is_set("globcomplete"));
    z.comp.wouldinstab = 0;
    if lastchar_is_tab(z) && usetab(z) {
        return misc::selfinsert(z, args);
    }
    docomplete(z, COMP_COMPLETE)
}

pub fn listchoices(z: &mut Zle, _args: &[String]) -> i32 {
    setusemenu(z);
    docomplete(z, COMP_LIST_COMPLETE)
}

pub fn spellword(z: &mut Zle, _args: &[String]) -> i32 {
    z.comp.usemenu = 0;
    z.comp.useglob = 0;
    z.comp.wouldinstab = 0;
    docomplete(z, COMP_SPELL)
}

pub fn deletecharorlist(z: &mut Zle, args: &[String]) -> i32 {
    setusemenu(z);
    if z.cs != z.line.len() {
        fixsuffix(z);
        z.invalidatelist();
        return misc::deletechar(z, args);
    }
    docomplete(z, COMP_LIST_COMPLETE)
}

pub fn expandword(z: &mut Zle, args: &[String]) -> i32 {
    z.comp.usemenu = 0;
    z.comp.useglob = 0;
    z.comp.wouldinstab = 0;
    if lastchar_is_tab(z) && usetab(z) {
        return misc::selfinsert(z, args);
    }
    docomplete(z, COMP_EXPAND)
}

pub fn expandorcomplete(z: &mut Zle, args: &[String]) -> i32 {
    setusemenu(z);
    if lastchar_is_tab(z) && usetab(z) {
        return misc::selfinsert(z, args);
    }
    bashlist_or(z, COMP_EXPAND_COMPLETE)
}

pub fn menuexpandorcomplete(z: &mut Zle, args: &[String]) -> i32 {
    z.comp.usemenu = 1;
    z.comp.useglob = i32::from(is_set("globcomplete"));
    z.comp.wouldinstab = 0;
    if lastchar_is_tab(z) && usetab(z) {
        return misc::selfinsert(z, args);
    }
    docomplete(z, COMP_EXPAND_COMPLETE)
}

pub fn listexpand(z: &mut Zle, _args: &[String]) -> i32 {
    setusemenu(z);
    docomplete(z, COMP_LIST_EXPAND)
}

pub fn reversemenucomplete(z: &mut Zle, args: &[String]) -> i32 {
    z.comp.wouldinstab = 0;
    z.zmod.mult = -z.zmod.mult;
    menucomplete(z, args)
}

pub fn acceptandmenucomplete(z: &mut Zle, args: &[String]) -> i32 {
    z.comp.wouldinstab = 0;
    if z.comp.menucmp == 0 {
        return 1;
    }
    crate::zle::comp::result::accept_last(z);
    menucomplete(z, args)
}

pub fn expandorcompleteprefix(z: &mut Zle, args: &[String]) -> i32 {
    COMPPREF.with(|c| c.set(true));
    let ret = expandorcomplete(z, args);
    if z.cs > 0 && z.line[z.cs - 1] == ' ' {
        makesuffixstr(z, None, Some("\\-"), 0);
    }
    COMPPREF.with(|c| c.set(false));
    ret
}

pub fn endoflist(z: &mut Zle, _args: &[String]) -> i32 {
    let n = super::result::lastlistlen();
    if n > 0 {
        super::result::set_clearflag(0);
        z.trash();
        refresh::write_tty(&"\n".repeat(n as usize));
        refresh::flush_tty();
        z.showinglist = 0;
        super::result::set_lastlistlen(0);
        if z.widget_depth > 0 {
            z.zrefresh();
        }
        return 0;
    }
    1
}

// ---- helpers ------------------------------------------------------------

/// Checks whether `p` names a parameter worth expanding.
fn checkparams(z: &Zle, p: &str) -> bool {
    let mut n = 0;
    let mut e = false;
    for name in param_names() {
        if name.starts_with(p) {
            n += 1;
            if name.len() == p.len() {
                e = true;
            }
        }
    }
    if n == 1 {
        crate::shell::var_exists(p)
    } else {
        z.comp.menucmp == 0 && e && is_set("recexact")
    }
}

/// Whether `s` has wildcards, telling job specs (`%?foo`) and parameter
/// expressions apart.
fn cmphaswilds(s: &[char]) -> bool {
    if (s.first() == Some(&INBRACK) || s.first() == Some(&OUTBRACK)) && s.len() == 1 {
        return false;
    }
    let mut i = 0;
    if s.first() == Some(&'%') && s.get(1) == Some(&QUEST) {
        i = 2;
    }
    // In ~[foo] the brackets are not wildcards.
    if s.get(i) == Some(&TILDE) && s.get(i + 1) == Some(&INBRACK) {
        if let Some(k) = s[i + 2..].iter().position(|&c| c == OUTBRACK) {
            i = i + 2 + k + 1;
        }
    }
    let extglob = is_set("extendedglob");
    let ignorebraces = is_set("ignorebraces");
    while i < s.len() {
        let c = s[i];
        if c == STRING || c == QSTRING {
            i += 1;
            if s.get(i) == Some(&INBRACE) {
                i = skipparens(s, i, INBRACE, OUTBRACE).1;
            } else if matches!(s.get(i), Some(&STRING) | Some(&QSTRING)) {
                i += 1;
            } else {
                while i < s.len() && matches!(s[i], '^' | HAT | '=' | EQUALS | '~' | TILDE) {
                    i += 1;
                }
                if matches!(s.get(i), Some(&'#') | Some(&POUND)) {
                    i += 1;
                }
                if matches!(s.get(i), Some(&STAR) | Some(&QUEST)) {
                    i += 1;
                }
            }
        } else {
            if ((c == POUND || c == HAT) && extglob) || c == STAR || c == BAR || c == QUEST {
                return true;
            }
            let (r, ni) = skipparens(s, i, INBRACK, OUTBRACK);
            i = ni;
            if r == 0 {
                return true;
            }
            let (r, ni) = skipparens(s, i, INANG, OUTANG);
            i = ni;
            if r == 0 {
                return true;
            }
            if !ignorebraces {
                let (r, ni) = skipparens(s, i, INBRACE, OUTBRACE);
                i = ni;
                if r == 0 {
                    return true;
                }
            }
            if s.get(i) == Some(&INPAR) && s.get(i + 1) == Some(&':') {
                let (r, ni) = skipparens(s, i, INPAR, OUTPAR);
                i = ni;
                if r == 0 {
                    return true;
                }
            }
            if i < s.len() {
                i += 1;
            }
        }
    }
    false
}

/// If the cursor is in a parameter name in `s`, where the name starts.
pub fn parambeg(z: &Zle, s: &[char]) -> Option<usize> {
    let offs = z.comp.offs.max(0) as usize;
    let isstr = |c: Option<&char>| matches!(c, Some(&STRING) | Some(&QSTRING));
    let mut p = offs.min(s.len());
    while p > 0 && !isstr(s.get(p)) {
        p -= 1;
    }
    if isstr(s.get(p)) {
        // $$'s
        while p > 0 && isstr(s.get(p - 1)) {
            p -= 1;
        }
        while isstr(s.get(p + 1)) && isstr(s.get(p + 2)) {
            p += 2;
        }
    }
    if isstr(s.get(p)) && !matches!(s.get(p + 1), Some(&INPAR) | Some(&INBRACK) | Some(&'\'')) {
        let mut b = p + 1;
        let mut n = 0;
        if s.get(b) == Some(&INBRACE) {
            if skipparens(s, b, INBRACE, OUTBRACE).0 == 0 {
                return None;
            }
            b += 1;
            let (r, nb) = skipparens(s, b, INPAR, OUTPAR);
            n = r;
            b = nb;
        }
        while b < s.len() && matches!(s[b], '^' | HAT | '=' | EQUALS | '~' | TILDE) {
            b += 1;
        }
        if matches!(s.get(b), Some(&'#') | Some(&POUND) | Some(&'+')) {
            b += 1;
        }
        let mut e = b;
        while s.get(e) == Some(&DNULL) {
            e += 1;
        }
        match s.get(e) {
            Some(&(QUEST | STAR | STRING | QSTRING | '?' | '*' | '$' | '-' | '!' | '@')) => e += 1,
            Some(&c) if c.is_ascii_digit() => {
                while s.get(e).is_some_and(|c| c.is_ascii_digit()) {
                    e += 1;
                }
            }
            _ => e = itype_end_namespc(s, e, false),
        }
        if offs <= e && offs >= b && n <= 0 {
            return Some(b);
        }
    }
    None
}

/// Like has_token, but ignoring nulls (and `$'`).
fn has_real_token(s: &[char]) -> bool {
    let mut i = 0;
    while i < s.len() {
        if (s[i] == QSTRING && s.get(i + 1) == Some(&'\''))
            || (s[i] == STRING && s.get(i + 1) == Some(&SNULL))
        {
            i += 2;
            continue;
        }
        if itok(s[i]) && !inull(s[i]) {
            return true;
        }
        i += 1;
    }
    false
}

/// Puts an `x` at the cursor when it is not in a word, so the lexer
/// finds one there; with comppref, also a space after it. Returns the
/// line as it was when it did.
fn addx(z: &Zle, line: &mut Vec<char>) -> Option<Vec<char>> {
    let cs = z.cs;
    let mut addspace = false;
    let add = match line.get(cs).copied() {
        None => true,
        Some(c) => {
            c == '\n'
                || (iblank(c) && (cs == 0 || line[cs - 1] != '\\'))
                || matches!(c, ')' | '`' | '}' | ';' | '|' | '&' | '>' | '<')
                || (z.comp.instring != QT_NONE && (c == '"' || c == '\''))
                || {
                    addspace = COMPPREF.with(|p| p.get()) && !iblank(c);
                    addspace
                }
        }
    };
    if add {
        let orig = line.clone();
        line.insert(cs, 'x');
        if addspace {
            line.insert(cs + 1, ' ');
        }
        set_addedx(1 + i32::from(addspace));
        Some(orig)
    } else {
        set_addedx(0);
        None
    }
}

/// Like dupstring, with a space added at the end.
pub fn dupstrspace(s: &[char]) -> Vec<char> {
    let mut t = s.to_vec();
    t.push(' ');
    t
}

/// Inserts `s` at the cursor; with `mv`, the cursor goes after it. `len`
/// is how many characters to insert, -1 for all. Returns that length.
pub fn inststrlen(z: &mut Zle, s: &str, mv: bool, len: i32) -> i32 {
    let c = chars(s);
    let len = if len < 0 {
        c.len()
    } else {
        (len as usize).min(c.len())
    };
    if len == 0 {
        return 0;
    }
    z.spaceinline(len);
    let cs = z.cs;
    z.line[cs..cs + len].copy_from_slice(&c[..len]);
    if mv {
        z.cs += len;
    }
    len as i32
}

pub fn inststr(z: &mut Zle, s: &str) -> i32 {
    inststrlen(z, s, true, -1)
}

/// Length of the common prefix of `s` and `t`, in characters.
pub fn pfxlen(s: &str, t: &str) -> i32 {
    s.chars().zip(t.chars()).take_while(|(a, b)| a == b).count() as i32
}

fn restore_origline(z: &mut Zle) {
    let orig = chars(z.comp.origline.as_deref().unwrap_or(""));
    z.cs = 0;
    let n = z.line.len();
    z.foredel(n, CUT_RAW);
    z.insert_chars(&orig);
    z.cs = (z.comp.origcs.max(0) as usize).min(z.line.len());
}

fn push_brinfo(
    z: &mut Zle,
    s: &[char],
    bb: usize,
    len: usize,
    pos: i32,
    predup: &[char],
    dbeg: usize,
) {
    let end = (bb + len).min(s.len());
    let str = string(&untokenize(&quotename(z, &s[bb..end])));
    let qpos = quotename(z, &predup[..dbeg.min(predup.len())]).len() as i32;
    z.comp.brbeg.push(Brinfo {
        str: Some(str),
        pos,
        qpos,
        curpos: 0,
    });
    z.comp.nbrbeg += 1;
}

/// Finds the word to complete and its context with the lexer, cleans the
/// quoting out of it and the line, and records brace expansions it is
/// in. Returns the word and the word before brace handling (zsh's
/// origword); None if there is nothing to complete.
fn get_comp_string(z: &mut Zle) -> Option<(Vec<char>, Vec<char>)> {
    let mut varq = false;
    let noaliases = is_set("completealiases");
    let rcquotes = is_set("rcquotes");
    z.comp.brbeg.clear();
    z.comp.lastbrbeg.clear();
    z.comp.brend.clear();
    z.comp.lastbrend.clear();
    z.comp.nbrbeg = 0;
    z.comp.nbrend = 0;
    z.comp.lastprebr = None;
    z.comp.lastpostbr = None;
    z.comp.rdstrs.clear();
    let mut rdop = String::new();
    z.comp.rdstr = None;

    // Are we in backquotes?
    let cs = z.cs as i32;
    {
        let (mut i, mut j, mut k) = (0, 0, 0);
        let mut u = 0;
        while u < z.cs {
            let c = z.line[u];
            if c == '`' && k & 1 == 0 {
                i += 1;
            } else if c == '"' && k & 1 == 0 && i & 1 == 0 {
                j += 1;
            } else if c == '\'' && j & 1 == 0 {
                k += 1;
            } else if c == '\\' && u + 1 < z.line.len() && k & 1 == 0 {
                u += 1;
            }
            u += 1;
        }
        z.comp.inbackt = i & 1;
    }
    z.comp.instring = QT_NONE;
    let mut lline: Vec<char> = z.line.clone();
    let mut tmp = addx(z, &mut lline);
    let mut linptr = 0usize;
    let mut ll = z.line.len() as i32;
    let mut tt: Option<Vec<char>> = None;
    let mut zlemetacs_qsub: i32 = 0;
    let mut s: Option<Vec<char>>;

    'start: loop {
        set_inwhat(IN_NOTHING);
        let input = dupstrspace(&lline[linptr.min(lline.len())..]);
        let mut lx = ZleLexer::new(&input, LEXFLAGS_ZLE, cs, ll);
        lx.noaliases = noaliases;
        z.comp.lincmd = i32::from(lx.incmdpos);
        z.comp.linredir = i32::from(lx.inredir);
        z.comp.cmdstr = None;
        let mut cmdtok = T::Nulltok;
        z.comp.varname = None;
        z.comp.insubscr = 0;
        let mut clwpos: i32 = -1;
        let mut clwords: Vec<Vec<char>> = Vec::new();
        let (mut wordpos, mut cp, mut rd, mut ins, mut oins) = (0i32, 0i32, 0i32, 0i32, 0i32);
        let (mut linarr, mut parct, mut ia, mut redirpos) = (0i32, 0i32, 0i32, 0i32);
        set_we(cs);
        set_wb(cs);
        let mut tt0 = T::Nulltok;

        // Go through the command line with the lexer, a token at a time,
        // to the end of the command the cursor is in.
        'lex: loop {
            let mut qsub = 0;
            let mut noword = false;
            'body: {
                let linredir = lx.inredir && ins == 0;
                let lincmd = !lx.inredir
                    && ((lx.incmdpos && ins == 0 && lx.incond == 0)
                        || (oins == 2 && wordpos == 2)
                        || (ins == 3 && wordpos == 1)
                        || (cmdtok == T::Nulltok && lx.incond == 0));
                z.comp.linredir = i32::from(linredir);
                z.comp.lincmd = i32::from(lincmd);
                oins = ins;
                if linarr != 0 {
                    lx.incmdpos = false;
                }
                // Assignments after typeset, but not in an array.
                if cmdtok == T::Typeset {
                    lx.intypeset = linarr == 0;
                }
                lx.ctxtlex();

                if lx.tok == T::Lexerr {
                    let Some(ts) = &lx.tokstr else {
                        break 'lex;
                    };
                    let n = ts.iter().filter(|&&c| c == SNULL || c == DNULL).count();
                    if n & 1 == 1 {
                        if lincmd && ts.contains(&'=') {
                            varq = true;
                            lx.tok = T::Envstring;
                        } else {
                            lx.tok = T::String;
                        }
                    }
                } else if lx.tok == T::Envstring {
                    varq = false;
                }
                if lx.tok == T::Envarray {
                    linarr = 1;
                    z.comp.varname = lx.tokstr.as_deref().map(string);
                } else if lx.tok == T::Inpar {
                    parct += 1;
                } else if lx.tok == T::Outpar {
                    if parct > 0 {
                        parct -= 1;
                    } else if linarr != 0 {
                        linarr = 0;
                        lx.incmdpos = true;
                    }
                }
                if lx.inredir && lx.tok.is_redirop() {
                    let op = lx.tok.text().unwrap_or("");
                    rdop = if lx.tokfd >= 0 {
                        format!("{}{}", lx.tokfd, op)
                    } else {
                        op.to_string()
                    };
                    z.comp.rdstr = Some(rdop.clone());
                    // Before the command word.
                    if wordpos == redirpos {
                        redirpos += 1;
                    }
                    let inbufct = lx.inbufct();
                    if cs < ll - inbufct && cs >= lx.wordbeg && wb() == we() {
                        // The cursor is in a redirection: a word.
                        let ax = addedx();
                        set_we(ll - (inbufct + ax));
                        if ax != 0 && we() > wb() {
                            // {param}> form, wb at the `{'.
                            set_wb(wb() + 1);
                        } else {
                            set_wb(cs);
                        }
                    }
                }
                if lx.tok == T::Dinpar {
                    lx.tokstr = None;
                }
                let tok = lx.tok;

                if tok == T::Endinput {
                    break 'lex;
                }
                if (ins != 0 && (tok == T::Doloop || tok == T::Seper))
                    || (ins == 2 && wordpos == 2)
                    || (ins == 3 && wordpos == 3)
                    || tok == T::Bar
                    || tok == T::Amper
                    || tok == T::Baramp
                    || tok == T::Amperbang
                    || ((tok == T::Dbar || tok == T::Damper) && lx.incond == 0)
                    || (tt.is_some() && lx.incmdpos)
                {
                    // A command separator: done if we have what we need.
                    if tt.is_some() {
                        break 'lex;
                    }
                    if ins < 2 {
                        noword = true;
                    }
                    wordpos = 0;
                    cp = 0;
                    rd = 0;
                    ins = 0;
                    redirpos = 0;
                    tt0 = T::Nulltok;
                }
                if lincmd
                    && matches!(
                        tok,
                        T::String
                            | T::For
                            | T::Foreach
                            | T::Select
                            | T::Repeat
                            | T::Case
                            | T::Typeset
                    )
                {
                    // The command word.
                    ins = if tok == T::Repeat {
                        2
                    } else {
                        i32::from(tok != T::String && tok != T::Typeset)
                    };
                    let ts = lx.tokstr.clone().unwrap_or_default();
                    z.comp.cmdstr = Some(string(&remnulargs(&untokenize(&ts))));
                    cmdtok = tok;
                    if wordpos != redirpos && clwpos == -1 {
                        wordpos = 0;
                        redirpos = 0;
                    }
                } else if tok == T::Seper {
                    ins = i32::from(cmdtok != T::String && cmdtok != T::Typeset);
                }
                if lx.lexflags == 0 && tt0 == T::Nulltok {
                    // The lexer reached the word the cursor is on.
                    tt = lx.tokstr.clone();
                    // Backslash-newlines the lexer removed.
                    let (i0, i1) = (wb().max(0) as usize, we().max(0) as usize);
                    let (mut bi, mut bj, mut bk) = (0, 0, 0);
                    let mut u = i0;
                    while u < i1 && u < lline.len() {
                        let c = lline[u];
                        if c == '`' && bk & 1 == 0 {
                            bi += 1;
                        } else if c == '"' && bk & 1 == 0 && bi & 1 == 0 {
                            bj += 1;
                        } else if c == '\'' && bj & 1 == 0 {
                            bk += 1;
                        } else if c == '\\' && u + 1 < lline.len() && bk & 1 == 0 {
                            if lline[u + 1] == '\n' {
                                qsub += 2;
                            }
                            u += 1;
                        }
                        u += 1;
                    }
                    // RC_QUOTES pairs the lexer removed.
                    if rcquotes {
                        if let Some(ttv) = &tt {
                            let e = (cs - wb() - qsub).clamp(0, ttv.len() as i32) as usize;
                            for (t1, &ch) in ttv.iter().enumerate() {
                                if ch == SNULL && t1 < e {
                                    qsub +=
                                        ttv[t1..e].iter().filter(|&&c| c == '\'').count() as i32;
                                }
                            }
                        }
                    }
                    if addedx() != 0 {
                        if let Some(ttv) = tt.as_mut() {
                            let at = cs - wb() - qsub;
                            if at >= 0 && (at as usize) < ttv.len() {
                                ttv.remove(at as usize);
                            }
                        }
                    }
                    tt0 = tok;
                    clwpos = wordpos;
                    cp = i32::from(lincmd);
                    rd = i32::from(linredir);
                    ia = linarr;
                    if inwhat() == IN_NOTHING && lx.incond != 0 {
                        set_inwhat(IN_COND);
                    }
                } else if linredir {
                    if !rdop.is_empty() {
                        if let Some(ts) = &lx.tokstr {
                            z.comp.rdstrs.push(format!("{}:{}", rdop, string(ts)));
                        }
                    }
                    break 'body;
                }
                if lx.incond != 0 {
                    if tok == T::Dbar {
                        lx.tokstr = Some(chars("||"));
                    } else if tok == T::Damper {
                        lx.tokstr = Some(chars("&&"));
                    }
                }
                if noword {
                    break 'body;
                }
                let Some(mut w) = lx.tokstr.clone() else {
                    break 'body;
                };
                // Completion after `repeat n do'.
                if oins == 2 && wordpos == 0 && w == ['d', 'o'] {
                    ins = 3;
                }
                // Token strings sometimes end in spaces.
                let mut sl = w.len();
                while sl > 0 && w[sl - 1] == ' ' && (sl < 2 || w[sl - 2] != BNULL) {
                    sl -= 1;
                    w.truncate(sl);
                }
                let wp = wordpos;
                wordpos += 1;
                if clwpos == wp && addedx() != 0 {
                    // The word the cursor is in: take out the `x'.
                    zlemetacs_qsub = cs - qsub;
                    let word_diff = zlemetacs_qsub - wb();
                    let chuck_at = if word_diff >= sl as i32 {
                        sl as i32 - 1
                    } else {
                        word_diff.max(0)
                    };
                    if chuck_at >= 0 && (chuck_at as usize) < w.len() {
                        w.remove(chuck_at as usize);
                    }
                }
                let wpu = wp.max(0) as usize;
                if clwords.len() <= wpu {
                    clwords.resize(wpu + 1, Vec::new());
                }
                clwords[wpu] = w;
            }
            if !(lx.tok != T::Lexerr
                && lx.tok != T::Endinput
                && (lx.tok != T::Seper || (lx.lexflags != 0 && tt0 == T::Nulltok)))
            {
                break 'lex;
            }
        }
        let clwnum = if tt.is_some() || wordpos == 0 {
            wordpos
        } else {
            wordpos - 1
        };
        clwords.truncate(clwnum.max(0) as usize);
        let mut t0 = tt0;
        if ia != 0 {
            z.comp.lincmd = 0;
            z.comp.linredir = 0;
            set_inwhat(IN_ENV);
        } else {
            z.comp.lincmd = cp;
            z.comp.linredir = rd;
        }
        if lx.parbegin != -1 {
            // In a command or process substitution, unless a $((...)):
            // start again with what is inside it.
            if lx.parend >= 0 && tmp.is_none() {
                tmp = Some(lline.clone());
            }
            let lp = ll + addedx() - lx.parbegin + 1;
            let lpu = lp.max(0) as usize;
            let isarith = lp >= 3
                && lline.get(lpu) == Some(&'(')
                && lline[lpu - 1] == '('
                && lline[lpu - 2] == '$';
            if !isarith {
                if lx.parend >= 0 {
                    ll -= lx.parend;
                    lline.truncate((ll + addedx()).max(0) as usize);
                }
                tt = None;
                linptr = lpu;
                continue 'start;
            }
        }

        if inwhat() == IN_MATH {
            s = None;
        } else if t0 == T::Nulltok || t0 == T::Endinput {
            // No word: an empty line.
            s = Some(Vec::new());
            set_we(cs);
            set_wb(cs);
            clwpos = clwnum;
            t0 = T::String;
        } else if t0 == T::String || t0 == T::Typeset {
            s = Some(
                clwords
                    .get(clwpos.max(0) as usize)
                    .cloned()
                    .unwrap_or_default(),
            );
        } else if t0 == T::Envstring {
            // The cursor is in an assignment.
            if varq {
                tt = clwords.get(clwpos.max(0) as usize).cloned();
            }
            let ttv = tt.clone().unwrap_or_default();
            let e = itype_end_namespc(&ttv, 0, false);
            let vn = string(&ttv[..e]);
            let mut si = e;
            if ttv.get(si) == Some(&'+') {
                si += 1;
            }
            let (lvl, ni) = skipparens(&ttv, si, INBRACK, OUTBRACK);
            si = ni;
            if lvl > 0 || si as i32 > zlemetacs_qsub - wb() {
                s = None;
                set_inwhat(IN_MATH);
                z.comp.insubscr = if is_hashed(&vn) { 2 } else { 1 };
            } else if ttv.get(si) == Some(&'=') {
                if zlemetacs_qsub > wb() + si as i32 {
                    si += 1;
                    set_wb(wb() + si as i32);
                    s = Some(ttv[si..].to_vec());
                    set_inwhat(IN_ENV);
                } else {
                    let mut p = si;
                    if p > 0 && ttv[p - 1] == '+' {
                        p -= 1;
                    }
                    set_inwhat(IN_PAR);
                    s = Some(ttv[..p].to_vec());
                    set_we(wb() + p as i32);
                }
                t0 = T::String;
            } else {
                s = Some(ttv[si.min(ttv.len())..].to_vec());
            }
            z.comp.varname = Some(vn);
            z.comp.lincmd = 1;
        } else {
            s = None;
        }
        if we() > ll {
            set_we(ll);
        }
        z.comp.clwords = clwords.iter().map(|w| string(w)).collect();
        z.comp.clwnum = clwnum;
        z.comp.clwsize = clwnum + 1;
        z.comp.clwpos = clwpos;
        if t0 != T::String && t0 != T::Typeset && inwhat() != IN_MATH {
            if tmp.is_some() {
                // Try again without the `x'.
                tmp = None;
                linptr = 0;
                lline = z.line.clone();
                ll = z.line.len() as i32;
                set_addedx(0);
                tt = Some(Vec::new());
                continue 'start;
            }
            return None;
        }
        break 'start;
    }

    // In an array subscript? Assume so inside brackets.
    if inwhat() != IN_MATH {
        let sv = s.as_deref().unwrap_or(&[]);
        let mut i = 0;
        let mut nnb = usize::from(itype_end_ident(sv, 0, true) == 0);
        let lim = zlemetacs_qsub - wb();
        let mut ttp = 0usize;
        if z.comp.lincmd != 0 {
            // `[' at the start of a command has no closing bracket.
            while ttp < sv.len() && sv[ttp] == INBRACK && (ttp as i32) < lim {
                ttp += 1;
            }
        }
        let (mut nb, mut ne) = (None, None);
        while (ttp as i32) < lim && ttp < sv.len() {
            if sv[ttp] == INBRACK {
                i += 1;
                nb = Some(nnb);
                ne = Some(ttp);
                ttp += 1;
            } else if i > 0 && sv[ttp] == OUTBRACK {
                i -= 1;
                ttp += 1;
            } else {
                if itype_end_ident(sv, ttp, true) == ttp {
                    nnb = ttp + 1;
                }
                ttp += 1;
            }
        }
        if i > 0 {
            set_inwhat(IN_MATH);
            z.comp.insubscr = 1;
            if let (Some(b), Some(e)) = (nb, ne) {
                if b < e {
                    let vn = string(&sv[b..e]);
                    if is_hashed(&vn) {
                        z.comp.insubscr = 2;
                    }
                    z.comp.varname = Some(vn);
                }
            }
        }
    }
    if inwhat() == IN_MATH {
        let line = z.line.clone();
        let len = line.len() as i32;
        let mut wbv: i32;
        let mut wev = we();
        if z.comp.compfunc.is_some() || z.comp.insubscr == 2 {
            let mut lev = 0;
            wbv = cs - 1;
            while wbv > 0 {
                let c = line[wbv as usize];
                if c == ']' || c == ')' {
                    lev += 1;
                } else if c == '[' {
                    if lev == 0 {
                        break;
                    }
                    lev -= 1;
                } else if c == '(' {
                    if lev == 0 && line[wbv as usize - 1] == '(' {
                        break;
                    }
                    if lev > 0 {
                        lev -= 1;
                    }
                }
                wbv -= 1;
            }
            let p = wbv;
            wbv += 1;
            if wbv != 0 && p >= 0 && p < len {
                let pc = line[p as usize];
                if pc == '[' || pc == '(' {
                    let close = if pc == '[' { ']' } else { ')' };
                    let (lvl, ni) = skipparens(&line, p as usize, pc, close);
                    if lvl == 0 {
                        wev = ni as i32 - 1;
                        if z.comp.insubscr == 2 {
                            z.comp.insubscr = 3;
                        }
                    }
                }
            }
        } else {
            // A parameter name, with or without `$'.
            wev = itype_end_ident(&line, z.cs.min(line.len()), false) as i32;
            let mut wptr = 0usize;
            loop {
                let mut cptr = itype_end_ident(&line, wptr, false);
                if cptr == wptr {
                    cptr += 1;
                    wptr = cptr;
                }
                if cptr as i32 >= cs {
                    wbv = wptr as i32;
                    break;
                }
                wptr = cptr;
            }
        }
        set_wb(wbv);
        set_we(wev);
        let (b, e) = (wbv.clamp(0, len) as usize, wev.clamp(0, len) as usize);
        let mut sv = if b < e {
            line[b..e].to_vec()
        } else {
            Vec::new()
        };
        if wbv > 2 && line.get(wbv as usize - 1) == Some(&'[') {
            let sqbr = wbv as usize - 1;
            let mut wptr = 0usize;
            loop {
                let mut cptr = itype_end_ident(&line, wptr, false);
                if cptr == wptr {
                    cptr += 1;
                    wptr = cptr;
                }
                if cptr >= sqbr {
                    break;
                }
                wptr = cptr;
            }
            if wptr < sqbr {
                let vn = string(&line[wptr..sqbr]);
                if is_hashed(&vn) {
                    if z.comp.insubscr != 3 {
                        z.comp.insubscr = 2;
                    }
                } else {
                    z.comp.insubscr = 1;
                }
                z.comp.varname = Some(vn);
            }
        }
        parse_subst_string(z, &mut sv);
        s = Some(sv);
    }
    let mut s = s.unwrap_or_default();
    z.comp.offs = z.cs as i32 - wb();
    if parambeg(z, &s).is_some() {
        for c in s.iter_mut() {
            if *c == DNULL {
                *c = '"';
            } else if *c == SNULL {
                *c = '\'';
            }
        }
    } else {
        let mut level = 0;
        let mut k = 0;
        while k < s.len() {
            if level > 0 && s[k] == SNULL {
                s[k] = '\'';
            } else if level > 0 && s[k] == DNULL {
                s[k] = '"';
            } else if (s[k] == STRING || s[k] == QSTRING) && s.get(k + 1) == Some(&INBRACE) {
                level += 1;
            } else if s[k] == OUTBRACE {
                level -= 1;
            }
            k += 1;
        }
    }
    if (s.first() == Some(&SNULL)
        || s.first() == Some(&DNULL)
        || ((s.first() == Some(&STRING) || s.first() == Some(&QSTRING))
            && s.get(1) == Some(&SNULL)))
        && !has_real_token(&s[1..])
    {
        // The word is in quotes.
        let mut sl = s.len();
        let mut qt = 0usize;
        let q: &str = match s[0] {
            SNULL => {
                z.comp.instring = QT_SINGLE;
                "'"
            }
            DNULL => {
                z.comp.instring = QT_DOUBLE;
                "\""
            }
            _ => {
                z.comp.instring = QT_DOLLARS;
                qt = 1;
                sl -= 1;
                "$'"
            }
        };
        let mut qipre = z.comp.qipre.take().unwrap_or_default();
        qipre.push_str(q);
        z.comp.qipre = Some(qipre);
        let q = q.strip_prefix('$').unwrap_or(q);
        if sl > 1 && s[qt + sl - 1] == s[qt] {
            z.comp.qisuf = Some(format!("{}{}", q, z.comp.qisuf.as_deref().unwrap_or("")));
        }
        z.comp.autoq = Some(q.to_string());
        // \! in double quotes is taken out by history before parsing.
        if z.comp.instring == QT_DOUBLE && is_set("banghist") {
            for k in 0..s.len().saturating_sub(1) {
                if s[k] == '\\' && s[k + 1] == '!' {
                    s[k] = BNULL;
                }
            }
        }
    }
    // A leading `=' is tokenized in case EQUALS is set later.
    if s.first() == Some(&EQUALS) && !is_set("equals") {
        s[0] = '=';
    }
    // Build the quoted form, cleaning the quotes out of the line too.
    {
        let mut p: isize = 0;
        let mut i: i32 = wb();
        let mut j = 0;
        while (p as usize) < s.len() {
            let pu = p as usize;
            let c = s[pu];
            let skipchars: i32;
            if c == STRING && s.get(pu + 1) == Some(&SNULL) {
                let mut pe = pu + 2;
                while pe < s.len() && s[pe] != SNULL && i + ((pe - pu) as i32) < z.cs as i32 {
                    pe += 1;
                }
                if s.get(pe) != Some(&SNULL) {
                    // No closing quote: can't substitute.
                    skipchars = 2;
                    if pe < s.len() {
                        j = 1;
                    }
                } else {
                    let (t, used) = getkeystring_dollars(&s[pu + 2..]);
                    let len = used as i32 + 2;
                    let tlen = t.len() as i32;
                    let sk = len - tlen;
                    if sk >= 0 {
                        for (k, &ch) in t.iter().enumerate() {
                            s[pu + k] = ch;
                        }
                        let ocs = z.cs as i32;
                        z.cs = i.max(0) as usize;
                        if sk > 0 {
                            let end = (pu + len as usize).min(s.len());
                            s.drain(pu + tlen as usize..end);
                            if i < ocs {
                                z.comp.offs -= sk;
                            }
                            z.foredel(sk as usize, CUT_RAW);
                            z.cs = (if ocs > i { ocs - sk } else { ocs }).max(0) as usize;
                            set_we(we() - sk);
                        }
                        for (k, &ch) in t.iter().enumerate() {
                            if let Some(slot) = z.line.get_mut(i as usize + k) {
                                *slot = ch;
                            }
                        }
                        p += tlen as isize;
                        i += tlen;
                        continue;
                    }
                    // Longer than the original: give up, as if in quotes.
                    skipchars = 2;
                    j = 1;
                }
            } else if c == QSTRING && s.get(pu + 1) == Some(&SNULL) {
                skipchars = 2;
            } else if inull(c) {
                skipchars = 1;
            } else {
                skipchars = 0;
            }
            if skipchars > 0 {
                let zcs = z.cs as i32;
                if i < zcs {
                    z.comp.offs -= skipchars;
                }
                if c == SNULL && rcquotes {
                    j = 1 - j;
                }
                if pu + 1 < s.len() || c != BNULL {
                    if c == BNULL {
                        if zcs == i + 1 {
                            z.cs += 1;
                            z.comp.offs += 1;
                        }
                    } else {
                        let ocs = zcs;
                        z.cs = i.max(0) as usize;
                        z.foredel(skipchars as usize, CUT_RAW);
                        i -= 1;
                        let mut ncs = ocs;
                        if ncs > i {
                            ncs -= skipchars;
                            // Not before the start of the word.
                            if wb() > ncs {
                                ncs = wb();
                            }
                        }
                        z.cs = ncs.max(0) as usize;
                        set_we(we() - skipchars);
                    }
                } else {
                    let ocs = zcs;
                    let wev = we();
                    z.cs = wev.max(0) as usize;
                    z.backdel(skipchars as usize, CUT_RAW);
                    let mut ncs = if ocs == wev { wev - skipchars } else { ocs };
                    if wb() > ncs {
                        ncs = wb();
                    }
                    z.cs = ncs.max(0) as usize;
                    set_we(wev - skipchars);
                }
                let end = (pu + skipchars as usize).min(s.len());
                s.drain(pu..end);
                p -= 1;
            } else if j != 0 && c == '\'' && i < z.cs as i32 {
                z.comp.offs -= 1;
            }
            p += 1;
            i += 1;
        }
    }
    let origword = s.clone();

    if !is_set("ignorebraces") {
        // Try to deal with foo{xxx etc.
        let curs = if is_set("completeinword") {
            z.comp.offs.max(0) as usize
        } else {
            s.len()
        };
        let mut predup = s.clone();
        let mut dp: isize = 0;
        let mut bbeg: Option<usize> = None;
        let mut bend = 0usize;
        let mut dbeg = 0usize;
        let mut lastp: Option<usize> = None;
        let mut firsts: Option<usize> = None;
        let mut cant = false;
        let mut begi: i32 = 0;
        let mut boffs = z.comp.offs;
        let mut hascom = 0;
        let mut i: i32 = 0;
        let mut p = 0usize;
        while p < s.len() {
            let c = s[p];
            if c == STRING || c == QSTRING {
                // ${... is not a brace expansion.
                let n1 = s.get(p + 1).copied();
                if matches!(n1, Some(INBRACE) | Some(INPAR) | Some(INBRACK)) {
                    let o = n1.unwrap_or(INBRACE);
                    let close = match o {
                        INBRACE => OUTBRACE,
                        INPAR => OUTPAR,
                        _ => OUTBRACK,
                    };
                    let (lvl, tp) = skipparens(&s, p + 1, o, close);
                    if lvl != 0 {
                        break;
                    }
                    i += (tp - p) as i32;
                    dp += (tp - p) as isize;
                    p = tp;
                } else if n1 != Some(SNULL) {
                    let mut tp = p + 1;
                    while tp < s.len()
                        && matches!(
                            s[tp],
                            '^' | HAT | '=' | EQUALS | '~' | TILDE | '#' | POUND | '+'
                        )
                    {
                        tp += 1;
                    }
                    let tc = s.get(tp).copied();
                    if matches!(tc, Some(c) if matches!(c, QUEST | STAR | STRING | QSTRING | '?' | '*' | '$' | '-' | '!' | '@'))
                    {
                        p += 1;
                        i += 1;
                    } else {
                        if tc.is_some_and(|x| x.is_ascii_digit()) {
                            while s.get(tp).is_some_and(|x| x.is_ascii_digit()) {
                                tp += 1;
                            }
                        } else {
                            let ie = itype_end_ident(&s, tp, false);
                            if ie == tp {
                                break;
                            }
                            tp = ie;
                        }
                        if s.get(tp) == Some(&INBRACE) {
                            cant = true;
                            break;
                        }
                        tp -= 1;
                        i += (tp - p) as i32;
                        dp += (tp - p) as isize;
                        p = tp;
                    }
                }
            } else if p < curs {
                if c == OUTBRACE {
                    cant = true;
                    break;
                }
                if c == INBRACE {
                    let (lvl, tp) = skipparens(&s, p, INBRACE, OUTBRACE);
                    if lvl == 0 {
                        // A balanced brace: skip it.
                        i += (tp - p) as i32;
                        dp += (tp - p) as isize;
                        p = tp;
                        continue;
                    }
                    makecommaspecial(true);
                    if let Some(bb) = bbeg {
                        let len = bend - bb;
                        push_brinfo(z, &s, bb, len, begi, &predup, dbeg);
                        i -= len as i32;
                        boffs -= len as i32;
                        let end = (dbeg + len).min(predup.len());
                        predup.drain(dbeg.min(end)..end);
                        dp -= len as isize;
                    }
                    bbeg = Some(p);
                    lastp = Some(p);
                    dbeg = dp.max(0) as usize;
                    bend = p + 1;
                    begi = i;
                } else if c == COMMA && bbeg.is_some() {
                    bend = p + 1;
                    hascom = 1;
                }
            } else {
                // On or after the cursor.
                if c == INBRACE {
                    let (lvl, tp) = skipparens(&s, p, INBRACE, OUTBRACE);
                    if lvl == 0 {
                        i += (tp - p) as i32;
                        dp += (tp - p) as isize;
                        p = tp;
                        continue;
                    }
                    cant = true;
                    makecommaspecial(true);
                    break;
                }
                if p == curs {
                    // At the cursor: stack the text of a pending brace.
                    if let Some(bb) = bbeg {
                        let len = bend - bb;
                        push_brinfo(z, &s, bb, len, begi, &predup, dbeg);
                        i -= len as i32;
                        boffs -= len as i32;
                        let end = (dbeg + len).min(predup.len());
                        predup.drain(dbeg.min(end)..end);
                        dp -= len as isize;
                    }
                    bbeg = None;
                }
                if c == COMMA {
                    if bbeg.is_none() {
                        bbeg = Some(p);
                    }
                    hascom = 2;
                } else if c == OUTBRACE {
                    let bb = bbeg.unwrap_or(p);
                    let len = p + 1 - bb;
                    if firsts.is_none() {
                        firsts = Some(p + 1);
                    }
                    let str = string(&untokenize(&quotename(z, &s[bb..bb + len])));
                    z.comp.brend.insert(
                        0,
                        Brinfo {
                            str: Some(str),
                            pos: (dp - len as isize + 1) as i32,
                            qpos: len as i32,
                            curpos: 0,
                        },
                    );
                    z.comp.nbrend += 1;
                    bbeg = None;
                }
            }
            p += 1;
            dp += 1;
            i += 1;
        }
        if cant {
            z.comp.brbeg.clear();
            z.comp.brend.clear();
            z.comp.nbrbeg = 0;
            z.comp.nbrend = 0;
        } else {
            if p == curs {
                if let Some(bb) = bbeg {
                    let len = bend - bb;
                    push_brinfo(z, &s, bb, len, begi, &predup, dbeg);
                    boffs -= len as i32;
                    let end = (dbeg + len).min(predup.len());
                    predup.drain(dbeg.min(end)..end);
                }
            }
            let mut brend = std::mem::take(&mut z.comp.brend);
            for bp in brend.iter_mut() {
                let pp = bp.pos.max(0) as usize;
                let l = bp.qpos.max(0) as usize;
                let start = (pp + l).min(predup.len());
                let rest = predup[start..].to_vec();
                bp.pos = rest.len() as i32;
                bp.qpos = quotename(z, &rest).len() as i32;
                predup.drain(pp.min(start)..start);
            }
            z.comp.brend = brend;
            if hascom != 0 {
                if let Some(lp) = lastp {
                    z.comp.lastprebr = Some(string(&untokenize(&quotename(z, &s[..lp]))));
                }
                if let Some(f) = firsts {
                    z.comp.lastpostbr = Some(string(&untokenize(&s[f.min(s.len())..])));
                }
            }
            s = predup;
            z.comp.offs = boffs;
        }
    }
    Some((s, origword))
}

fn docomplete_done() {
    DOCOMPLETE_ACTIVE.with(|a| a.set(false));
    makecommaspecial(false);
}

/// The main entry point for completion and expansion.
fn docomplete(z: &mut Zle, lst: i32) -> i32 {
    let mut lst = lst;
    if DOCOMPLETE_ACTIVE.with(|a| a.get()) && z.comp.comprecursive == 0 {
        crate::shell::shell_err("completion cannot be used recursively (yet)");
        return 1;
    }
    DOCOMPLETE_ACTIVE.with(|a| a.set(true));
    z.comp.comprecursive = 0;
    makecommaspecial(false);

    if crate::zle::comp::core::before_complete(z, &mut lst) != 0 {
        docomplete_done();
        return 0;
    }
    // Expand history references first; if anything changed, do no more.
    if doexpandhist(z) {
        docomplete_done();
        return 0;
    }
    let olst = lst;
    let mut ocs = z.cs;
    z.comp.origline = Some(string(&z.line));
    z.comp.origcs = z.cs as i32;
    z.comp.origll = z.line.len() as i32;
    // The earlier lines of a continued command go in front for the lexer.
    let mut chl = 0usize;
    let mut ol = None;
    if !z.isfirstln {
        let pb = crate::zle::prebuffer();
        if !pb.is_empty() {
            ol = Some(z.line.clone());
            z.cs = 0;
            inststr(z, &pb);
            chl = z.cs;
            z.cs += ocs;
        }
    }
    set_inwhat(IN_NOTHING);
    z.comp.qipre = Some(String::new());
    z.comp.qisuf = Some(String::new());
    z.comp.autoq = None;
    let r = get_comp_string(z);
    // In vi mode, insertion starts no later than the word.
    if (z.viinsbegin as i32) > wb() {
        z.viinsbegin = wb().max(0) as usize;
    }
    if let Some(ol) = ol {
        z.cs = z.cs.saturating_sub(chl);
        set_wb(wb() - chl as i32);
        set_we(we() - chl as i32);
        if wb() < 0 {
            z.line = ol;
            z.cs = ocs.min(z.line.len());
            docomplete_done();
            return 1;
        }
        ocs = z.cs;
        z.cs = 0;
        z.foredel(chl, CUT_RAW);
        z.cs = ocs;
    }
    if inwhat() == IN_ENV {
        z.comp.lincmd = 0;
    }
    let ret = match r {
        Some((mut s, origword)) => {
            let lincmd = z.comp.lincmd;
            if lst == COMP_EXPAND_COMPLETE {
                // Expansion or completion?
                let mut q = 0usize;
                if s.first() == Some(&EQUALS) {
                    // `=cmd': expand if it names one command.
                    q = 1;
                    let name = string(&untokenize(&s[1..]));
                    if crate::shell::find_command(&name).is_some() {
                        if is_set("recexact") {
                            lst = COMP_EXPAND;
                        } else {
                            let mut n = 0;
                            for c in crate::shell::all_commands() {
                                if c.starts_with(&name) && crate::shell::find_command(&c).is_some()
                                {
                                    n += 1;
                                    if n == 2 {
                                        break;
                                    }
                                }
                            }
                            if n == 1 {
                                lst = COMP_EXPAND;
                            }
                        }
                    }
                }
                if lst == COMP_EXPAND_COMPLETE {
                    let target = z.cs as i32 - wb();
                    loop {
                        // A parameter expression?
                        while q < s.len() && s[q] != STRING {
                            q += 1;
                        }
                        if q < s.len()
                            && !matches!(
                                s.get(q + 1),
                                Some(&INPAR) | Some(&INPARMATH) | Some(&INBRACK)
                            )
                        {
                            q += 1;
                            if s.get(q) == Some(&INBRACE) {
                                let (lvl, nq) = skipparens(&s, q, INBRACE, OUTBRACE);
                                q = nq;
                                if lvl == 0 && q as i32 == target {
                                    lst = COMP_EXPAND;
                                }
                            } else {
                                while q < s.len()
                                    && matches!(s[q], '^' | HAT | '=' | EQUALS | '~' | TILDE)
                                {
                                    q += 1;
                                }
                                if matches!(s.get(q), Some(&'#') | Some(&POUND) | Some(&'+'))
                                    && s.get(q + 1) != Some(&STRING)
                                {
                                    q += 1;
                                }
                                let t = q;
                                let sav2 = s.get(q).copied();
                                let name: Vec<char> = match sav2 {
                                    Some(c) if matches!(c, QUEST | STAR | STRING | QSTRING) => {
                                        q += 1;
                                        vec![ztoken(c).unwrap_or(c)]
                                    }
                                    Some(c) if matches!(c, '?' | '*' | '$' | '-' | '!' | '@') => {
                                        q += 1;
                                        vec![c]
                                    }
                                    Some(c) if c.is_ascii_digit() => {
                                        while s.get(q).is_some_and(|x| x.is_ascii_digit()) {
                                            q += 1;
                                        }
                                        s[t..q].to_vec()
                                    }
                                    _ => {
                                        q = itype_end_namespc(&s, q, false);
                                        s[t..q].to_vec()
                                    }
                                };
                                if target == q as i32
                                    && (sav2.is_some_and(|c| c.is_ascii_digit())
                                        || checkparams(z, &string(&name)))
                                {
                                    lst = COMP_EXPAND;
                                }
                            }
                            if lst != COMP_EXPAND {
                                lst = COMP_COMPLETE;
                            }
                        } else {
                            break;
                        }
                        if (q as i32) >= target {
                            break;
                        }
                    }
                }
                if lst == COMP_EXPAND_COMPLETE {
                    // Still unclear: expand if there is a `$' or backquote.
                    lst = if s
                        .iter()
                        .any(|&c| matches!(c, TICK | QTICK | STRING | QSTRING))
                    {
                        COMP_EXPAND
                    } else {
                        COMP_COMPLETE
                    };
                }
                // And expand wildcards unless GLOB_COMPLETE.
                if !is_set("globcomplete") && cmphaswilds(&s) {
                    lst = COMP_EXPAND;
                }
            }
            if lincmd != 0 && inwhat() == IN_NOTHING {
                set_inwhat(IN_CMD);
            }
            if lst == COMP_SPELL {
                let w: Vec<char> = origword
                    .iter()
                    .map(|&c| if inull(c) { NULARG } else { c })
                    .collect();
                z.cs = wb().max(0) as usize;
                z.foredel((we() - wb()).max(0) as usize, CUT_RAW);
                let mut x = untokenize(&w);
                if let Some(&w0) = w.first() {
                    if (w0 == TILDE || w0 == EQUALS || w0 == STRING) && !x.is_empty() {
                        x[0] = w0;
                    }
                }
                let ox = x.clone();
                spckword(z, &mut x, lincmd != 0);
                let ret = i32::from(x == ox);
                let x = string(&untokenize(&x));
                inststr(z, &x);
                ret
            } else if comp_isexpand(lst) {
                let ol = z.line.clone();
                let ocs = z.cs;
                let mut ret = doexpansion(z, &origword, lst, olst, lincmd);
                z.comp.lastambig = 0;
                if olst == COMP_EXPAND_COMPLETE && ol == z.line {
                    // Expansion changed nothing: complete.
                    z.cs = ocs;
                    if z.comp.compfunc.is_none() {
                        let mut p = usize::from(matches!(s.first(), Some(&TILDE) | Some(&EQUALS)));
                        while p < s.len() {
                            if itok(s[p]) {
                                if s[p] != STRING && s[p] != QSTRING {
                                    match ztoken(s[p]) {
                                        Some(c) => s[p] = c,
                                        None => {
                                            s.truncate(p);
                                            break;
                                        }
                                    }
                                } else if s.get(p + 1) == Some(&INBRACE) {
                                    p += 1;
                                    p = skipparens(&s, p, INBRACE, OUTBRACE).1;
                                }
                            }
                            p += 1;
                        }
                    }
                    ret = docompletion(z, &s, lst, lincmd);
                } else {
                    if ret != 0 {
                        z.clearlist = true;
                    }
                    if ol == z.line {
                        // Quotes may have been taken out: put them back.
                        restore_origline(z);
                    }
                }
                ret
            } else {
                docompletion(z, &s, lst, lincmd)
            }
        }
        None => 1,
    };
    let mut dat = [lst, ret];
    crate::zle::comp::core::after_complete(z, &mut dat);
    docomplete_done();
    dat[1]
}

/// Expands the current word.
fn doexpansion(z: &mut Zle, s: &[char], lst: i32, olst: i32, explincmd: i32) -> i32 {
    let mut ret = 1;
    // get_comp_string() leaves these quotes alone inside parameter
    // expansions.
    let ss: Vec<char> = s
        .iter()
        .map(|&c| match c {
            '"' => DNULL,
            '\'' => SNULL,
            c => c,
        })
        .collect();
    let src = tokens_to_source(&ss);
    let doglob = lst == COMP_LIST_EXPAND || lst == COMP_EXPAND;
    let (oglob, onull) = (is_set("glob"), is_set("nullglob"));
    if doglob {
        crate::options::set("nullglob", true);
    } else {
        crate::options::set("glob", false);
    }
    let r = z.shell(|| crate::sys::with_stderr_silenced(|| crate::expand::expand_words(&[src])));
    crate::options::set("glob", oglob);
    crate::options::set("nullglob", onull);
    let Ok(vl) = r else {
        return ret;
    };
    if vl.is_empty() || vl[0].is_empty() {
        return ret;
    }
    let lit = string(&untokenize(s));
    let unchanged = (vl.len() == 1 && vl[0] == lit)
        || (olst == COMP_EXPAND_COMPLETE
            && vl.len() == 1
            && s.first() == Some(&TILDE)
            && filesubstr_eq(z, s, &vl[0]));
    if unchanged {
        // Try completion if that was asked for, else just beep.
        if lst == COMP_EXPAND_COMPLETE {
            docompletion(z, s, COMP_COMPLETE, explincmd);
        }
        return ret;
    }
    if lst == COMP_LIST_EXPAND {
        // Only the list: put the line back.
        restore_origline(z);
        ret = listlist(z, &vl);
        z.showinglist = 0;
        return ret;
    }
    // Replace the word with the expansions.
    z.cs = wb().max(0) as usize;
    z.foredel((we() - wb()).max(0) as usize, CUT_RAW);
    let n = vl.len();
    for (k, w) in vl.iter().enumerate() {
        ret = 0;
        let q = string(&untokenize(&quotename(z, &chars(w))));
        inststr(z, &q);
        if k + 1 < n || k > 0 {
            z.spaceinline(1);
            let cs = z.cs;
            z.line[cs] = ' ';
            z.cs += 1;
        }
    }
    ret
}

/// Whether expanding only the leading `~...` of `s` gives `v` (zsh's
/// filesubstr test in doexpansion).
fn filesubstr_eq(z: &mut Zle, s: &[char], v: &str) -> bool {
    let slash = s.iter().position(|&c| c == '/').unwrap_or(s.len());
    let src = tokens_to_source(&s[..slash]);
    let r = z.shell(|| crate::sys::with_stderr_silenced(|| crate::expand::expand_string(&src)));
    match r {
        Ok(pfx) => {
            let mut w = pfx;
            w.push_str(&string(&untokenize(&s[slash..])));
            w == v
        }
        Err(_) => false,
    }
}

fn docompletion(z: &mut Zle, s: &[char], lst: i32, incmd: i32) -> i32 {
    crate::zle::comp::core::do_completion(z, &string(s), lst, incmd)
}

// ---- listing ------------------------------------------------------------

/// Terminal width (zsh's zterm_columns).
pub fn zterm_columns() -> i32 {
    crate::shell::terminal_size()
        .map(|(c, _)| c as i32)
        .filter(|&c| c > 0)
        .or_else(|| crate::shell::var_get_opt("COLUMNS").and_then(|c| c.trim().parse().ok()))
        .filter(|&c| c > 0)
        .unwrap_or(80)
}

/// Terminal height (zsh's zterm_lines).
pub fn zterm_lines() -> i32 {
    crate::shell::terminal_size()
        .map(|(_, l)| l as i32)
        .filter(|&l| l > 0)
        .or_else(|| crate::shell::var_get_opt("LINES").and_then(|l| l.trim().parse().ok()))
        .filter(|&l| l > 0)
        .unwrap_or(24)
}

fn nicestr(s: &str) -> String {
    s.chars().map(crate::zle::utils::nicechar).collect()
}

/// Width of `s` as nicezputs shows it.
pub fn nicewidth(s: &str) -> i32 {
    crate::display_width(&nicestr(s)) as i32
}

fn tcup(n: i32) {
    if n > 0 {
        refresh::write_tty(&format!("\x1b[{}A", n));
    }
}

/// Prints a list of expansions in columns below the line (zsh's
/// listlist); nonzero if the list was empty or not shown.
pub fn listlist(z: &mut Zle, l: &[String]) -> i32 {
    let num = l.len() as i32;
    let mut data: Vec<String> = l.to_vec();
    let numeric = is_set("numericglobsort");
    data.sort_by(|a, b| {
        let ka: String = a.chars().filter(|&c| c != '\\').collect();
        let kb: String = b.chars().filter(|&c| c != '\\').collect();
        if numeric {
            crate::expand::numeric_order(&ka, &kb)
        } else {
            ka.cmp(&kb)
        }
    });
    let cols = zterm_columns();
    let lines = zterm_lines();
    let lens: Vec<i32> = data.iter().map(|p| nicewidth(p) + 2).collect();
    let len_at = |n: i32| lens.get(n.max(0) as usize).copied().unwrap_or(0);
    let mut longest = 0;
    let mut shortest = cols;
    let mut totl = 0;
    for &len in &lens {
        longest = longest.max(len);
        shortest = shortest.min(len);
        totl += len;
    }
    let mut widths = vec![0i32; cols.max(0) as usize + data.len() + 1];
    let mut pack = false;
    let mut ncols = if longest > 0 { (cols + 2) / longest } else { 0 };
    let mut nlines;
    if ncols > 0 {
        nlines = (num + ncols - 1) / ncols;
        if is_set("listpacked") {
            let mut tlines = 0;
            let mut tcols = 0;
            if is_set("listrowsfirst") {
                let mut maxlines = 0;
                tcols = if shortest > 0 { cols / shortest } else { 0 };
                while tcols > ncols {
                    let (mut nth, mut first, mut maxlen, mut width, mut llines) = (0, 0, 0, 0, 0);
                    let mut tcol = 0usize;
                    maxlines = 0;
                    let mut count = num;
                    while count > 0 {
                        if nth % tcols == 0 {
                            llines += 1;
                        }
                        maxlen = maxlen.max(len_at(nth));
                        nth += tcols;
                        tlines += 1;
                        if nth >= num {
                            width += maxlen;
                            if width >= cols {
                                break;
                            }
                            widths[tcol] = maxlen;
                            tcol += 1;
                            maxlen = 0;
                            first += 1;
                            nth = first;
                            maxlines = maxlines.max(llines);
                            llines = 0;
                        }
                        count -= 1;
                    }
                    if nth < num {
                        widths[tcol] = maxlen;
                        width += maxlen;
                    }
                    if count == 0 && width < cols {
                        break;
                    }
                    tcols -= 1;
                }
                if tcols > ncols {
                    tlines = maxlines;
                }
            } else {
                tlines = (totl + cols) / cols;
                while tlines < nlines {
                    let (mut nth, mut tline, mut width, mut maxlen) = (0, 0, 0, 0);
                    tcols = 0;
                    while nth < num {
                        maxlen = maxlen.max(len_at(nth));
                        tline += 1;
                        if tline == tlines {
                            width += maxlen;
                            if width >= cols {
                                break;
                            }
                            widths[tcols as usize] = maxlen;
                            tcols += 1;
                            maxlen = 0;
                            tline = 0;
                        }
                        nth += 1;
                    }
                    if tline > 0 {
                        widths[tcols as usize] = maxlen;
                        tcols += 1;
                        width += maxlen;
                    }
                    if nth == num && width < cols {
                        break;
                    }
                    tlines += 1;
                }
            }
            pack = tlines < nlines;
            if pack {
                nlines = tlines;
                ncols = tcols;
            }
        }
    } else {
        nlines = data
            .iter()
            .map(|p| 1 + p.chars().count() as i32 / cols)
            .sum();
    }
    // Below the prompt.
    z.trash();
    let tolast = (z.zmod.mult == 1) == is_set("alwayslastprompt");
    let mut clearflag = tolast;
    let max: i64 = crate::shell::var_get("LISTMAX").trim().parse().unwrap_or(0);
    if (max != 0 && i64::from(num) > max) || (max == 0 && nlines > lines) {
        let msg = if num > 0 {
            format!(
                "zsh: do you wish to see all {} possibilities ({} lines)? ",
                num, nlines
            )
        } else {
            format!("zsh: do you wish to see all {} lines? ", nlines)
        };
        let l = msg.chars().count() as i32;
        let qup = ((l + cols - 1) / cols) - 1;
        refresh::write_tty(&msg);
        refresh::flush_tty();
        if !z.getzlequery() {
            if clearflag {
                refresh::write_tty("\r");
                tcup(qup);
                refresh::write_tty("\x1b[J");
                tcup(z.refresh.nlnct as i32);
            } else {
                refresh::write_tty("\n");
            }
            refresh::flush_tty();
            super::result::set_clearflag(i32::from(clearflag));
            return 1;
        }
        if clearflag {
            refresh::write_tty("\r");
            tcup(qup);
            refresh::write_tty("\x1b[J");
        } else {
            refresh::write_tty("\n");
        }
    }
    super::result::set_lastlistlen(if clearflag { nlines } else { 0 });
    let n = data.len();
    let colw = |col: i32, k: usize| -> i32 {
        let w = if pack {
            widths.get((col - 1).max(0) as usize).copied().unwrap_or(0)
        } else {
            longest
        };
        w - lens[k] + 2
    };
    if ncols > 0 {
        if is_set("listrowsfirst") {
            let mut col = 1;
            for (k, item) in data.iter().enumerate() {
                refresh::write_tty(&nicestr(item));
                if col == ncols {
                    col = 0;
                    if k + 1 < n {
                        refresh::write_tty("\n");
                    }
                } else {
                    let i = colw(col, k);
                    if i > 0 {
                        refresh::write_tty(&" ".repeat(i as usize));
                    }
                }
                col += 1;
            }
        } else {
            for line in 0..nlines {
                let mut col = 1;
                let mut k = line as usize;
                while k < n {
                    refresh::write_tty(&nicestr(&data[k]));
                    if col == ncols {
                        break;
                    }
                    let i = colw(col, k);
                    if i > 0 {
                        refresh::write_tty(&" ".repeat(i as usize));
                    }
                    k += nlines.max(1) as usize;
                    col += 1;
                }
                if line + 1 < nlines {
                    refresh::write_tty("\n");
                }
            }
        }
    } else {
        for (k, item) in data.iter().enumerate() {
            refresh::write_tty(&nicestr(item));
            if k + 1 < n {
                refresh::write_tty("\n");
            }
        }
    }
    if clearflag {
        nlines += z.refresh.nlnct as i32 - 1;
        if nlines < lines {
            tcup(nlines);
            z.showinglist = -1;
        } else {
            clearflag = false;
            refresh::write_tty("\n");
        }
    } else {
        refresh::write_tty("\n");
    }
    super::result::set_clearflag(i32::from(clearflag));
    if super::result::listshown() != 0 {
        z.comp.showagain = 1;
    }
    refresh::flush_tty();
    i32::from(num == 0)
}

/// Prints an explanation string with its `%` escapes (%n is `n`); with
/// `dopr` unset only measures it. Returns the number of lines.
pub fn printfmt(_z: &mut Zle, fmt: &str, n: i32, dopr: bool, doesc: bool) -> i32 {
    let p = chars(fmt);
    let cols = zterm_columns();
    let out = |s: &str| {
        if dopr {
            refresh::write_tty(s);
        }
    };
    let (mut l, mut cc) = (0i32, 0i32);
    let mut i = 0usize;
    while i < p.len() {
        if doesc && p[i] == '%' {
            i += 1;
            let mut arg = 0i32;
            while i < p.len() && p[i].is_ascii_digit() {
                arg = arg * 10 + p[i].to_digit(10).unwrap_or(0) as i32;
                i += 1;
            }
            if i >= p.len() {
                break;
            }
            match p[i] {
                '%' => {
                    out("%");
                    cc += 1;
                }
                'n' => {
                    let nc = n.to_string();
                    out(&nc);
                    cc += crate::display_width(&nc) as i32;
                }
                'B' => out("\x1b[1m"),
                'b' => out("\x1b[22m"),
                'S' => out("\x1b[7m"),
                's' => out("\x1b[27m"),
                'U' => out("\x1b[4m"),
                'u' => out("\x1b[24m"),
                'F' | 'K' => {
                    let key = if p[i] == 'F' { "fg" } else { "bg" };
                    let colour = if p.get(i + 1) == Some(&'{') {
                        i += 2;
                        let st = i;
                        while i < p.len() && p[i] != '}' {
                            i += 1;
                        }
                        let c = string(&p[st..i]);
                        if i >= p.len() {
                            i -= 1;
                        }
                        c
                    } else {
                        arg.to_string()
                    };
                    out(&refresh::attr_to_sgr(&format!("{}={}", key, colour)));
                }
                'f' => out("\x1b[39m"),
                'k' => out("\x1b[49m"),
                'H' => {
                    if p.get(i + 1) == Some(&'{') {
                        let st = i + 2;
                        let mut e = st;
                        while e < p.len() && p[e] != '}' {
                            e += 1;
                        }
                        let spec = string(&p[st.min(e)..e]);
                        i = e.min(p.len() - 1);
                        out("\x1b[0m");
                        out(&refresh::attr_to_sgr(&spec));
                    } else {
                        out("\x1b[0m");
                    }
                }
                '{' => {
                    cc += arg;
                    i += 1;
                    while i < p.len() && !(p[i] == '%' && p.get(i + 1) == Some(&'}')) {
                        out(&p[i].to_string());
                        i += 1;
                    }
                    if i < p.len() {
                        i += 1;
                    } else {
                        i -= 1;
                    }
                }
                _ => {}
            }
            i += 1;
        } else if p[i] == '\n' {
            cc += 1;
            out("\x1b[K");
            l += 1 + (cc - 1) / cols;
            cc = 0;
            out("\n");
            i += 1;
        } else {
            out(&p[i].to_string());
            cc += crate::display_width(&p[i].to_string()) as i32;
            if dopr && cc % cols == 0 {
                out(" \x08");
            }
            i += 1;
        }
    }
    if dopr {
        out("\x1b[0m");
        if cc % cols == 0 {
            out(" \x08");
        }
        out("\x1b[K");
    }
    l + cc / cols
}

// ---- history expansion --------------------------------------------------

/// History expansion of the whole line; true if it changed.
pub fn doexpandhist(z: &mut Zle) -> bool {
    let old: String = z.line.iter().collect();
    let hist =
        crate::shell::with_shell(|s| s.history.lock().map(|h| h.clone()).unwrap_or_default());
    let Ok(new) = crate::history::expand(&old, &hist) else {
        return false;
    };
    if new == old {
        return false;
    }
    let tail = z.line.len() - z.cs;
    z.line = new.chars().collect();
    z.cs = z.line.len().saturating_sub(tail);
    // For vi mode, insertion starts no later than the line.
    let bol = z.findbol();
    if z.viinsbegin > bol {
        z.viinsbegin = bol;
    }
    true
}

/// Makes a following space count as typed (magic-space after expansion).
pub fn fixmagicspace(z: &mut Zle) {
    z.lastchar = b' ' as i32;
    z.lastchar_wide = Some(' ');
    z.lastchar_wide_valid = true;
}

pub fn magicspace(z: &mut Zle, args: &[String]) -> i32 {
    fixmagicspace(z);
    let bang = '!';
    let bangq = (0..z.line.len()).find(|&i| {
        z.line[i] == bang && z.line.get(i + 1) == Some(&'"') && (i == 0 || z.line[i - 1] == '\\')
    });
    let ret = misc::selfinsert(z, args);
    if ret == 0 && bangq.is_none_or(|b| b + 2 > z.cs) {
        doexpandhist(z);
    }
    ret
}

pub fn expandhistory(z: &mut Zle, _args: &[String]) -> i32 {
    i32::from(!doexpandhist(z))
}

// ---- the current command ------------------------------------------------

/// The command word of the command the cursor is in, with its start and
/// end in the line.
fn getcurcmd(z: &Zle) -> Option<(Vec<char>, i32, i32)> {
    let ll = z.line.len() as i32;
    let mut lx = ZleLexer::new(&dupstrspace(&z.line), LEXFLAGS_ZLE, z.cs as i32, ll);
    lx.noaliases = true;
    let mut s = None;
    let (mut cmdwb, mut cmdwe) = (0, 0);
    loop {
        let curlincmd = lx.incmdpos;
        lx.ctxtlex();
        if lx.tok == T::Endinput || lx.tok == T::Lexerr {
            break;
        }
        if lx.tok == T::String && curlincmd {
            s = lx.tokstr.clone();
            cmdwb = ll - lx.wordbeg;
            cmdwe = ll + 1 - lx.inbufct();
        }
        if lx.lexflags == 0 {
            break;
        }
    }
    s.map(|s| (s, cmdwb, cmdwe))
}

/// run-help and which-command: runs `$WIDGET cmd`, keeping the line on
/// the buffer stack.
pub fn processcmd(z: &mut Zle, _args: &[String]) -> i32 {
    let Some((s, _, _)) = getcurcmd(z) else {
        return 1;
    };
    let m = z.zmod.mult;
    z.zmod.mult = 1;
    crate::zle::widgets::hist::pushline(z, &[]);
    z.zmod.mult = m;
    let name = z.bindk.clone().unwrap_or_default();
    inststr(z, &name);
    inststr(z, " ");
    let q = string(&quotename(z, &untokenize(&s)));
    inststr(z, &q);
    z.done = true;
    0
}

pub fn expandcmdpath(z: &mut Zle, _args: &[String]) -> i32 {
    let oldcs = z.cs as i32;
    let Some((s, cmdwb, cmdwe)) = getcurcmd(z) else {
        return 1;
    };
    if cmdwb < 0 || cmdwe < cmdwb {
        return 1;
    }
    let Some(path) = crate::shell::find_command(&string(&untokenize(&s))) else {
        return 1;
    };
    z.cs = (cmdwb as usize).min(z.line.len());
    z.foredel((cmdwe - cmdwb) as usize, CUT_RAW);
    let strll = inststrlen(z, &path, false, -1);
    let mut cs = oldcs;
    if cs >= cmdwb {
        cs += strll - (cmdwe - cmdwb);
    }
    z.cs = (cs.max(0) as usize).min(z.line.len());
    0
}

// ---- spelling correction (utils.c) --------------------------------------

const QWERTY: &str = concat!(
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
    "\t1234567890-=\t",
    "\tqwertyuiop[]\t",
    "\tasdfghjkl;'\n\t",
    "\tzxcvbnm,./\t\t\t",
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
    "\t!@#$%^&*()_+\t",
    "\tQWERTYUIOP{}\t",
    "\tASDFGHJKL:\"\n\t",
    "\tZXCVBNM<>?\n\n\t",
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
);

const DVORAK: &str = concat!(
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
    "\t1234567890[]\t",
    "\t',.pyfgcrl/=\t",
    "\taoeuidhtns-\n\t",
    "\t;qjkxbmwvz\t\t\t",
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
    "\t!@#$%^&*(){}\t",
    "\t\"<>PYFGCRL?+\t",
    "\tAOEUIDHTNS_\n\t",
    "\t:QJKXBMWVZ\n\n\t",
    "\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
);

/// Spelling distance of `s` from `t`, counting typos found within
/// `thresh` steps; 200 for too far.
fn spdist(s: &[char], t: &[char], thresh: i32) -> i32 {
    if s == t {
        return 0;
    }
    // Case differences cost 1.
    let mut p = 0;
    while p < s.len()
        && t.get(p)
            .is_some_and(|&q| s[p].to_lowercase().eq(q.to_lowercase()))
    {
        p += 1;
    }
    if p == s.len() && p == t.len() {
        return 1;
    }
    if thresh == 0 {
        return 200;
    }
    let mut p = 0;
    while p < s.len() && p < t.len() {
        if s[p] == t[p] {
            p += 1;
            continue;
        }
        if s.get(p + 1) == Some(&t[p]) && t.get(p + 1) == Some(&s[p]) {
            return spdist(&s[p + 2..], &t[p + 2..], thresh - 1) + 1;
        } else if s.get(p + 1) == Some(&t[p]) {
            return spdist(&s[p + 1..], &t[p..], thresh - 1) + 2;
        } else if t.get(p + 1) == Some(&s[p]) {
            return spdist(&s[p..], &t[p + 1..], thresh - 1) + 2;
        }
        break;
    }
    if (p == s.len() && t.len() - p.min(t.len()) == 1)
        || (p == t.len() && s.len() - p.min(s.len()) == 1)
    {
        return 2;
    }
    let keymap: Vec<char> = chars(if is_set("dvorak") { DVORAK } else { QWERTY });
    let mut p = 0;
    while p < s.len() && p < t.len() {
        if s[p] != t[p] && s.get(p + 1) == t.get(p + 1) {
            // A mistyped letter: a neighbouring key costs less.
            let Some(t0) = keymap
                .iter()
                .position(|&k| k == s[p])
                .filter(|&k| keymap[k] != '\n' && keymap[k] != '\t')
            else {
                return spdist(&s[p + 1..], &t[p + 1..], thresh - 1) + 1;
            };
            let near = [-15i32, -14, -13, -1, 1, 13, 14, 15].iter().any(|&d| {
                let k = t0 as i32 + d;
                k >= 0 && keymap.get(k as usize) == Some(&t[p])
            });
            if near {
                return spdist(&s[p + 1..], &t[p + 1..], thresh - 1) + 2;
            }
            return 200;
        } else if s[p] != t[p] {
            break;
        }
        p += 1;
    }
    200
}

/// The entry of `dir` closest to `guess` and its distance (directories
/// only with `wantdir`).
fn mindist(dir: &str, guess: &[char], wantdir: bool) -> (i32, String) {
    let dir = if dir.is_empty() { "." } else { dir };
    let g = string(guess);
    if let Ok(m) = std::fs::metadata(format!("{}/{}", dir, g)) {
        if !wantdir || m.is_dir() {
            return (0, g);
        }
    }
    let ignore = crate::shell::var_get_opt("CORRECT_IGNORE_FILE");
    let mut best = String::new();
    let mut d = 100;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if ignore
                .as_deref()
                .is_some_and(|pat| crate::pattern::matches(pat, &name))
            {
                continue;
            }
            let nd = spdist(&chars(&name), guess, guess.len() as i32 / 4 + 1);
            if nd <= d {
                if wantdir
                    && !std::fs::metadata(format!("{}/{}", dir, name)).is_ok_and(|m| m.is_dir())
                {
                    continue;
                }
                best = name;
                d = nd;
                if d == 0 {
                    break;
                }
            }
        }
    }
    (d, best)
}

/// Corrects each component of a path, as far as corrections stay within
/// the threshold.
fn spname(oldname: &[char]) -> Option<String> {
    let mut new = String::new();
    let mut bestdist = 0;
    let mut maxthresh = 0;
    let mut i = 0;
    loop {
        while i < oldname.len() && oldname[i] == '/' {
            new.push('/');
            i += 1;
        }
        if i == oldname.len() {
            return Some(new);
        }
        let start = i;
        while i < oldname.len() && oldname[i] != '/' {
            i += 1;
        }
        let guess = &oldname[start..i];
        let thresh = (guess.len() as i32 / 4 + 1).clamp(3, 100);
        let (thisdist, best) = mindist(&new, guess, i < oldname.len());
        if thisdist >= thresh {
            if bestdist < maxthresh {
                new.push_str(&string(guess));
                new.push_str(&string(&oldname[i..]));
                return Some(new);
            }
            return None;
        }
        maxthresh = bestdist + thresh;
        bestdist += thisdist;
        new.push_str(&best);
    }
}

/// Spelling correction of the word `s` (a command word with `cmd`), as
/// zsh's spckword without asking.
pub fn spckword(z: &mut Zle, s: &mut Vec<char>, cmd: bool) {
    if s.len() < 2 {
        return;
    }
    // A leading % is a job, else a leading hyphen is an option.
    let skip = if cmd {
        s[0] == '%'
    } else {
        s[0] == '-' || s[0] == DASH
    };
    if skip {
        return;
    }
    let word = string(s);
    if word == "in" {
        return;
    }
    if cmd
        && (crate::shell::with_shell(|sh| sh.functions.contains_key(&word))
            || crate::builtins::is_builtin(&word)
            || crate::shell::find_command(&word).is_some()
            || crate::shell::alias_get(&word).is_some()
            || crate::shell::galias_get(&word).is_some()
            || reswd(&word).is_some())
    {
        return;
    }
    let start = usize::from(matches!(s[0], TILDE | EQUALS | STRING));
    for c in s.iter_mut().skip(start) {
        if itok(*c) {
            if *c == DASH {
                *c = '-';
            } else {
                return;
            }
        }
    }
    let t = s.iter().position(|&c| c == '/').unwrap_or(s.len());
    if s[0] == TILDE && t == s.len() {
        return;
    }
    let ignore = crate::shell::var_get_opt("CORRECT_IGNORE");
    let mut best: Option<String> = None;
    let mut d = 100;
    let mut guess: Vec<char>;
    let mut ic: Option<char> = None;
    let mut preflen = 0usize;
    let spscan = |name: &str, guess: &[char], best: &mut Option<String>, d: &mut i32| {
        if ignore
            .as_deref()
            .is_some_and(|pat| crate::pattern::matches(pat, name))
        {
            return;
        }
        let nd = spdist(&chars(name), guess, guess.len() as i32 / 4 + 1);
        if nd <= *d {
            *best = Some(name.to_string());
            *d = nd;
        }
    };
    if s[0] == STRING && t == s.len() {
        guess = s[1..].to_vec();
        if itype_end_namespc(&guess, 0, true) == 0 {
            return;
        }
        ic = Some(STRING);
        for name in param_names() {
            spscan(&name, &guess, &mut best, &mut d);
        }
    } else if s[0] == EQUALS {
        if t < s.len() {
            return;
        }
        guess = s[1..].to_vec();
        if crate::shell::find_command(&string(&guess)).is_some() {
            return;
        }
        ic = Some(EQUALS);
        for name in alias_names() {
            spscan(&name, &guess, &mut best, &mut d);
        }
        for name in crate::shell::all_commands() {
            spscan(&name, &guess, &mut best, &mut d);
        }
    } else {
        guess = s.clone();
        if guess[0] == TILDE || guess[0] == STRING {
            ic = Some(guess[0]);
            if t + 1 >= s.len() {
                return;
            }
            let src = tokens_to_source(&guess);
            let Ok(g) =
                z.shell(|| crate::sys::with_stderr_silenced(|| crate::expand::expand_string(&src)))
            else {
                return;
            };
            guess = chars(&g);
            preflen = guess.len().saturating_sub(s.len() - t - 1);
        }
        let gs = string(&guess);
        if std::path::Path::new(&gs).exists() {
            return;
        }
        best = spname(&guess);
        if t == s.len() && cmd {
            if crate::shell::find_command(&gs).is_some() {
                return;
            }
            d = 100;
            for name in RESWDS {
                spscan(name, &guess, &mut best, &mut d);
            }
            for name in alias_names() {
                spscan(&name, &guess, &mut best, &mut d);
            }
            let funcs: Vec<String> =
                crate::shell::with_shell(|sh| sh.functions.keys().cloned().collect());
            for name in funcs {
                spscan(&name, &guess, &mut best, &mut d);
            }
            for name in crate::builtins::builtin_names() {
                spscan(name, &guess, &mut best, &mut d);
            }
            for name in crate::shell::all_commands() {
                spscan(&name, &guess, &mut best, &mut d);
            }
            if is_set("autocd") && word != "." && word != ".." {
                let cdpath = crate::shell::array_get("cdpath").unwrap_or_default();
                for dir in cdpath {
                    let (thisdist, bestcd) = mindist(&dir, s, true);
                    // Only when better than anything so far.
                    if thisdist < d {
                        best = Some(bestcd);
                        d = thisdist;
                    }
                }
            }
        }
    }
    let Some(b) = best else {
        return;
    };
    let mut bv = chars(&b);
    if bv.len() > 1 && bv != guess {
        if let Some(icc) = ic {
            if preflen > 0 {
                // Don't correct the result of an expansion.
                if guess.len() < preflen || bv.len() < preflen || guess[..preflen] != bv[..preflen]
                {
                    return;
                }
                let mut u = s[..(t + 1).min(s.len())].to_vec();
                u.extend_from_slice(&bv[preflen..]);
                bv = u;
            } else {
                bv.insert(0, ' ');
            }
            bv[0] = icc;
        }
        *s = bv;
    }
}

fn alias_names() -> Vec<String> {
    let mut v = crate::shell::assoc_keys("aliases").unwrap_or_default();
    v.extend(crate::shell::assoc_keys("galiases").unwrap_or_default());
    v
}
