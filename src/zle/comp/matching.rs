//! Completion matching with match specs, after zsh's Src/Zle/compmatch.c.
//!
//! zsh builds its cline lists with pointer surgery: nodes are relinked,
//! shared between lists by shallow copies and cut out of lists while
//! other pointers still refer to them. To keep that behaviour, the work
//! is done on an arena of nodes ([`Mctx`]) whose links are indices, and
//! the boxed [`Cline`] lists of the shared state are converted at the
//! public entry points.
//!
//! zsh's strings in clines are pointers into longer strings, read past
//! their stated lengths in places. They are kept here as tails ([`Sp`]):
//! a shared character buffer and an offset into it; converting one to a
//! `String` gives everything from the offset to the end of the buffer.

use super::*;
use crate::zle::Zle;
use std::cell::Cell;
use std::rc::Rc;

// ---- strings ------------------------------------------------------------

/// A pointer into a string: the buffer and an offset, which may move
/// before or past the buffer as C pointer arithmetic does.
#[derive(Clone, Debug)]
struct Sp {
    b: Rc<Vec<char>>,
    o: isize,
}

impl Sp {
    fn new(s: &str) -> Sp {
        Sp {
            b: Rc::new(s.chars().collect()),
            o: 0,
        }
    }

    fn from_vec(v: Vec<char>) -> Sp {
        Sp {
            b: Rc::new(v),
            o: 0,
        }
    }

    fn at(&self, i: isize) -> char {
        at(&self.b, self.o + i)
    }

    fn add(&self, n: isize) -> Sp {
        Sp {
            b: self.b.clone(),
            o: self.o + n,
        }
    }

    /// The string from this position on.
    fn sl(&self) -> &[char] {
        sl(&self.b, self.o)
    }

    fn text(&self) -> String {
        self.sl().iter().take_while(|&&c| c != '\0').collect()
    }
}

fn sp_add(s: &Option<Sp>, n: isize) -> Option<Sp> {
    s.as_ref().map(|s| s.add(n))
}

fn sp_at(s: &Option<Sp>, i: isize) -> char {
    s.as_ref().map_or('\0', |s| s.at(i))
}

fn sp_sl(s: &Option<Sp>) -> &[char] {
    s.as_ref().map_or(&[], |s| s.sl())
}

/// The character at `i`, or NUL outside the string, as C reads it.
fn at(s: &[char], i: isize) -> char {
    if i < 0 || i as usize >= s.len() {
        '\0'
    } else {
        s[i as usize]
    }
}

/// The string from position `i` on.
fn sl(s: &[char], i: isize) -> &[char] {
    if i < 0 {
        &[]
    } else {
        &s[(i as usize).min(s.len())..]
    }
}

/// The line string from `pos` on, as zsh sees it while it has cut the
/// string at `ltrunc` for a suffix match.
fn lcut(lb: &[char], pos: isize, sfx: bool, ltrunc: isize) -> &[char] {
    if sfx && pos >= 0 && pos <= ltrunc {
        &lb[pos as usize..(ltrunc as usize).min(lb.len())]
    } else {
        sl(lb, pos)
    }
}

/// `!strncmp(a, b, n)`.
fn strneq(a: &[char], b: &[char], n: isize) -> bool {
    for i in 0..n.max(0) {
        let (x, y) = (at(a, i), at(b, i));
        if x != y {
            return false;
        }
        if x == '\0' {
            return true;
        }
    }
    true
}

/// Length up to the first NUL, as strlen.
fn zstrlen(s: &[char]) -> isize {
    s.iter().position(|&c| c == '\0').unwrap_or(s.len()) as isize
}

// ---- character classes --------------------------------------------------

// The [:...:] class numbers (PP_*), as zsh's pattern.c numbers them.
pub const PP_ALPHA: i32 = 1;
pub const PP_ALNUM: i32 = 2;
pub const PP_ASCII: i32 = 3;
pub const PP_BLANK: i32 = 4;
pub const PP_CNTRL: i32 = 5;
pub const PP_DIGIT: i32 = 6;
pub const PP_GRAPH: i32 = 7;
pub const PP_LOWER: i32 = 8;
pub const PP_PRINT: i32 = 9;
pub const PP_PUNCT: i32 = 10;
pub const PP_SPACE: i32 = 11;
pub const PP_UPPER: i32 = 12;
pub const PP_XDIGIT: i32 = 13;
pub const PP_IDENT: i32 = 14;
pub const PP_IFS: i32 = 15;
pub const PP_IFSSPACE: i32 = 16;
pub const PP_WORD: i32 = 17;
pub const PP_INCOMPLETE: i32 = 18;
pub const PP_INVALID: i32 = 19;
pub const PP_RANGE: i32 = 21;

const COLON_STUFFS: [&str; 19] = [
    "alpha",
    "alnum",
    "ascii",
    "blank",
    "cntrl",
    "digit",
    "graph",
    "lower",
    "print",
    "punct",
    "space",
    "upper",
    "xdigit",
    "IDENT",
    "IFS",
    "IFSSPACE",
    "WORD",
    "INCOMPLETE",
    "INVALID",
];

/// One element of a class: what zsh's parse_class() encodes with
/// Meta-tokens.
enum Ritem {
    Ch(char),
    Class(i32),
    Range(char, char),
}

/// Splits the text of a `[...]` or `{...}` class into elements, as
/// parse_class() does; unknown `[:name:]` classes are dropped.
fn parse_range(text: &[char]) -> Vec<Ritem> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if text[i] == '[' && text.get(i + 1) == Some(&':') {
            if let Some(off) = text[i + 2..].iter().position(|&c| c == ':') {
                let e = i + 2 + off;
                if text.get(e + 1) == Some(&']') {
                    let name: String = text[i + 2..e].iter().collect();
                    if let Some(k) = COLON_STUFFS.iter().position(|n| *n == name) {
                        out.push(Ritem::Class(k as i32 + PP_ALPHA));
                    }
                    i = e + 2;
                    continue;
                }
            }
        }
        let c = text[i];
        i += 1;
        if text.get(i) == Some(&'-') && i + 1 < text.len() {
            out.push(Ritem::Range(c, text[i + 1]));
            i += 2;
        } else {
            out.push(Ritem::Ch(c));
        }
    }
    out
}

fn ifs_chars() -> String {
    crate::shell::var_get_opt("IFS").unwrap_or_else(|| " \t\n\0".to_string())
}

fn class_match(tp: i32, ch: char) -> bool {
    match tp {
        PP_ALPHA => ch.is_alphabetic(),
        PP_ALNUM => ch.is_alphanumeric(),
        PP_ASCII => ch.is_ascii(),
        PP_BLANK => ch == ' ' || ch == '\t',
        PP_CNTRL => ch.is_control(),
        PP_DIGIT => ch.is_ascii_digit(),
        PP_GRAPH => !ch.is_control() && !ch.is_whitespace(),
        PP_LOWER => ch.is_lowercase(),
        PP_PRINT => !ch.is_control(),
        PP_PUNCT => !ch.is_control() && !ch.is_alphanumeric() && !ch.is_whitespace(),
        PP_SPACE => ch.is_whitespace(),
        PP_UPPER => ch.is_uppercase(),
        PP_XDIGIT => ch.is_ascii_hexdigit(),
        PP_IDENT => ch.is_alphanumeric() || ch == '_',
        PP_IFS => ifs_chars().contains(ch),
        PP_IFSSPACE => (ch as u32) < 128 && ch.is_ascii_whitespace() && ifs_chars().contains(ch),
        PP_WORD => {
            ch.is_alphanumeric()
                || crate::shell::var_get_opt("WORDCHARS")
                    .unwrap_or_else(|| "*?_-.[]~=/&;!#$%^(){}<>".to_string())
                    .contains(ch)
        }
        _ => false,
    }
}

/// zsh's mb_patmatchrange(): does `ch` match the class text? `indptr`
/// gets the index of the matching element (ranges count each of their
/// characters), `mtp` the kind of element that matched.
fn patmatchrange(text: &[char], ch: char, indptr: Option<&mut u32>, mtp: Option<&mut i32>) -> bool {
    let mut ind: u32 = 0;
    let mut mt = 0;
    let mut hit = false;
    for item in parse_range(text) {
        match item {
            Ritem::Ch(c) => {
                if c == ch {
                    mt = 0;
                    hit = true;
                    break;
                }
            }
            Ritem::Class(tp) => {
                mt = tp;
                if class_match(tp, ch) {
                    hit = true;
                    break;
                }
            }
            Ritem::Range(r1, r2) => {
                mt = PP_RANGE;
                if r1 <= ch && ch <= r2 {
                    ind = ind.wrapping_add(ch as u32 - r1 as u32);
                    hit = true;
                    break;
                }
                if r1 < r2 {
                    ind = ind.wrapping_add(r2 as u32 - r1 as u32);
                }
            }
        }
        ind = ind.wrapping_add(1);
    }
    if let Some(p) = indptr {
        *p = if hit { ind } else { 0 };
    }
    if let Some(m) = mtp {
        *m = mt;
    }
    hit
}

/// zsh's mb_patmatchindex(): the element at index `ind` of the class:
/// a character, or the kind of a `[:...:]` element.
fn patmatchindex(text: &[char], mut ind: u32) -> Option<(Option<char>, i32)> {
    for item in parse_range(text) {
        match item {
            Ritem::Ch(c) => {
                if ind == 0 {
                    return Some((Some(c), 0));
                }
            }
            Ritem::Class(tp) => {
                if ind == 0 {
                    return Some((None, tp));
                }
            }
            Ritem::Range(r1, r2) => {
                let rdiff = (r2 as u32).wrapping_sub(r1 as u32);
                if rdiff >= ind {
                    return Some((char::from_u32(r1 as u32 + ind), 0));
                }
                ind -= rdiff;
            }
        }
        if ind == 0 {
            break;
        }
        ind -= 1;
    }
    None
}

fn tolower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn toupper(c: char) -> char {
    c.to_uppercase().next().unwrap_or(c)
}

/// One pattern element against one character. Non-zero on a match; for
/// an equivalence class, one more than the index of the matching element.
pub fn pattern_match1(p: &Cpattern, c: char, mtp: &mut i32) -> u32 {
    *mtp = 0;
    match p {
        Cpattern::Class { negated, text } => {
            (patmatchrange(text, c, None, None) != *negated) as u32
        }
        Cpattern::Equiv(text) => {
            let mut ind = 0;
            if patmatchrange(text, c, Some(&mut ind), Some(mtp)) {
                ind + 1
            } else {
                0
            }
        }
        Cpattern::Any => 1,
        Cpattern::Char(ch) => (*ch == c) as u32,
    }
}

/// Uses an equivalence class to find the line character for the word
/// character `wchr`, which matched the word pattern at index `wind - 1`
/// with match type `wmtp`. None where zsh returns CHR_INVALID.
pub fn pattern_match_equivalence(lp: &Cpattern, wind: u32, wmtp: i32, wchr: char) -> Option<char> {
    let Cpattern::Equiv(text) = lp else {
        return None;
    };
    let (lchr, lmtp) = patmatchindex(text, wind.wrapping_sub(1))?;
    if let Some(c) = lchr {
        return Some(c);
    }
    if wmtp == PP_UPPER && lmtp == PP_LOWER {
        Some(tolower(wchr))
    } else if wmtp == PP_LOWER && lmtp == PP_UPPER {
        Some(toupper(wchr))
    } else if wmtp == lmtp {
        Some(wchr)
    } else {
        None
    }
}

/// Matches a line pattern whose characters are still being built: each
/// line character comes from `prestrict` (or is deduced from the word)
/// and is written to `new_line`.
fn pattern_match_restrict(
    p: &[Cpattern],
    wp: &[Cpattern],
    wsc: &[char],
    mut wsclen: isize,
    prestrict: &[Cpattern],
    new_line: &mut [char],
) -> bool {
    let (mut pi, mut wi, mut si, mut ri, mut ni) = (0usize, 0usize, 0isize, 0usize, 0usize);
    let mut mt = 0;
    let mut wmt = 0;
    let put = |nl: &mut [char], ni: &mut usize, c: char| {
        if *ni < nl.len() {
            nl[*ni] = c;
        }
        *ni += 1;
    };

    while pi < p.len() && wi < wp.len() && wsclen != 0 && ri < prestrict.len() {
        let wc = at(wsc, si);
        let wind = pattern_match1(&wp[wi], wc, &mut wmt);
        if wind == 0 {
            return false;
        }
        let c;
        if let Cpattern::Char(rc) = prestrict[ri] {
            c = rc;
        } else {
            c = match &p[pi] {
                Cpattern::Char(pc) => *pc,
                Cpattern::Equiv(_) => match pattern_match_equivalence(&p[pi], wind, wmt, wc) {
                    Some(c) => c,
                    None => return false,
                },
                _ => wc,
            };
            if pattern_match1(&prestrict[ri], c, &mut mt) == 0 {
                return false;
            }
        }
        if p[pi] != Cpattern::Any || wp[wi] != Cpattern::Any {
            let ind = pattern_match1(&p[pi], c, &mut mt);
            if ind == 0 || ind != wind {
                return false;
            }
            if mt != wmt {
                if (mt == PP_LOWER || mt == PP_UPPER) && (wmt == PP_LOWER || wmt == PP_UPPER) {
                    if tolower(c) != tolower(wc) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }
        put(new_line, &mut ni, c);
        ri += 1;
        si += 1;
        wsclen -= 1;
        pi += 1;
        wi += 1;
    }

    while pi < p.len() && ri < prestrict.len() {
        let c;
        if let Cpattern::Char(rc) = prestrict[ri] {
            c = rc;
        } else {
            if let Cpattern::Char(pc) = p[pi] {
                c = pc;
            } else {
                return false;
            }
            if pattern_match1(&prestrict[ri], c, &mut mt) == 0 {
                return false;
            }
        }
        if pattern_match1(&p[pi], c, &mut mt) == 0 {
            return false;
        }
        pi += 1;
        put(new_line, &mut ni, c);
        ri += 1;
    }

    if ri < prestrict.len() {
        return false;
    }

    while wi < wp.len() && wsclen != 0 {
        if pattern_match1(&wp[wi], at(wsc, si), &mut wmt) == 0 {
            return false;
        }
        wi += 1;
        si += 1;
        wsclen -= 1;
    }
    true
}

/// Does the pattern `p` match the start of `s` (and `wp` the start of
/// `ws`, with equivalence classes paired by position)? An empty `wp` is
/// zsh's NULL word pattern. Matching stops where a string ends.
pub fn pattern_match(p: &[Cpattern], s: &[char], wp: &[Cpattern], ws: &[char]) -> bool {
    let (mut pi, mut wi, mut si, mut wsi) = (0usize, 0usize, 0isize, 0isize);
    let mut mt = 0;
    let mut wmt = 0;

    while pi < p.len() && wi < wp.len() && at(s, si) != '\0' && at(ws, wsi) != '\0' {
        let wc = at(ws, wsi);
        let wind = pattern_match1(&wp[wi], wc, &mut wmt);
        if wind == 0 {
            return false;
        }
        let c = at(s, si);
        if p[pi] != Cpattern::Any || wp[wi] != Cpattern::Any {
            let ind = pattern_match1(&p[pi], c, &mut mt);
            if ind == 0 || ind != wind {
                return false;
            }
            if mt != wmt {
                if (mt == PP_LOWER || mt == PP_UPPER) && (wmt == PP_LOWER || wmt == PP_UPPER) {
                    if tolower(c) != tolower(wc) {
                        return false;
                    }
                } else {
                    return false;
                }
            }
        }
        si += 1;
        wsi += 1;
        pi += 1;
        wi += 1;
    }

    while pi < p.len() && at(s, si) != '\0' {
        if pattern_match1(&p[pi], at(s, si), &mut mt) == 0 {
            return false;
        }
        pi += 1;
        si += 1;
    }

    while wi < wp.len() && at(ws, wsi) != '\0' {
        if pattern_match1(&wp[wi], at(ws, wsi), &mut wmt) == 0 {
            return false;
        }
        wi += 1;
        wsi += 1;
    }
    true
}

// ---- matcher lists ------------------------------------------------------

fn cpatterns_same(a: &[Cpattern], b: &[Cpattern]) -> bool {
    a == b
}

fn cmatchers_same(a: &Rc<Cmatcher>, b: &Rc<Cmatcher>) -> bool {
    Rc::ptr_eq(a, b)
        || (a.flags == b.flags
            && a.llen == b.llen
            && a.wlen == b.wlen
            && (a.llen == 0 || cpatterns_same(&a.line, &b.line))
            && (a.wlen <= 0 || cpatterns_same(&a.word, &b.word))
            && (a.flags & (CMF_LEFT | CMF_RIGHT) == 0
                || (a.lalen == b.lalen
                    && a.ralen == b.ralen
                    && (a.lalen == 0 || cpatterns_same(&a.left, &b.left))
                    && (a.ralen == 0 || cpatterns_same(&a.right, &b.right)))))
}

/// Puts the matchers of the chain `m` that can build strings in front
/// of the bmatchers list.
pub fn add_bmatchers(z: &mut Zle, m: &Rc<Cmatcher>) {
    let mut new = Vec::new();
    let mut cur = Some(m.clone());
    while let Some(mm) = cur {
        if (mm.flags == 0 && mm.wlen > 0 && mm.llen > 0)
            || (mm.flags == CMF_RIGHT && mm.wlen < 0 && mm.llen == 0)
        {
            new.push(Cmlist {
                matcher: Some(mm.clone()),
                str: String::new(),
            });
        }
        cur = mm.next.clone();
    }
    new.append(&mut z.comp.bmatchers);
    z.comp.bmatchers = new;
}

/// After the matcher stack changed: drops the bmatchers up to the last
/// one no longer on the stack.
pub fn update_bmatchers(z: &mut Zle) {
    let mut start = 0;
    for (i, p) in z.comp.bmatchers.iter().enumerate() {
        let Some(pm) = &p.matcher else {
            start = i + 1;
            continue;
        };
        let mut t = false;
        'ms: for ms in &z.comp.mstack {
            let mut mp = ms.matcher.clone();
            while let Some(m) = mp {
                if cmatchers_same(&m, pm) {
                    t = true;
                    break 'ms;
                }
                mp = m.next.clone();
            }
        }
        if !t {
            start = i + 1;
        }
    }
    z.comp.bmatchers.drain(..start);
}

// ---- the cline arena ----------------------------------------------------

type Ix = Option<usize>;

/// Stands in for zsh's `(Cline) 1` marker while match_str() shifts
/// anchors.
const MARK: usize = usize::MAX;

#[derive(Clone, Debug, Default)]
struct Nd {
    next: Ix,
    flags: u32,
    line: Option<Sp>,
    llen: isize,
    word: Option<Sp>,
    wlen: isize,
    orig: Option<Sp>,
    olen: isize,
    slen: isize,
    prefix: Ix,
    suffix: Ix,
    min: isize,
    max: isize,
}

/// A walk through a sub-list while joining (zsh's struct cmdata).
#[derive(Clone, Default)]
struct Cmdata {
    cl: Ix,
    pcl: Ix,
    str: Option<Sp>,
    astr: Option<Sp>,
    len: isize,
    alen: isize,
    olen: isize,
    line: bool,
}

/// Where a link is stored, for zsh's `Cline *p` link pointers.
#[derive(Clone, Copy)]
enum Link {
    Head,
    Next(usize),
}

thread_local! {
    // zsh's join_strs() keeps its buffer in a static; whether it was
    // ever allocated decides what an empty result means.
    static JOIN_STRS_ALLOC: Cell<bool> = const { Cell::new(false) };
}

/// The matching state: the node arena, zsh's matchparts/matchsubs lists
/// with their tail pointers, matchbuf, and the matchers in use.
struct Mctx {
    nd: Vec<Nd>,
    parts: Ix,
    lastpart: Ix,
    subs: Ix,
    lastsub: Ix,
    buf: Vec<char>,
    mstack: Rc<Vec<Rc<Cmatcher>>>,
    bmatchers: Rc<Vec<Rc<Cmatcher>>>,
    useqbr: bool,
}

impl Mctx {
    fn new(z: &Zle) -> Mctx {
        Mctx {
            nd: Vec::new(),
            parts: None,
            lastpart: None,
            subs: None,
            lastsub: None,
            buf: z.comp.matchbuf.clone(),
            mstack: Rc::new(
                z.comp
                    .mstack
                    .iter()
                    .filter_map(|m| m.matcher.clone())
                    .collect(),
            ),
            bmatchers: Rc::new(
                z.comp
                    .bmatchers
                    .iter()
                    .filter_map(|m| m.matcher.clone())
                    .collect(),
            ),
            useqbr: z.comp.useqbr != 0,
        }
    }

    /// Writes matchbuf and the part lists back to the shared state.
    fn export(&self, z: &mut Zle) {
        z.comp.matchbuf = self.buf.clone();
        z.comp.matchbufadded = self.buf.len() as i32;
        z.comp.matchparts = self.exp(self.parts);
        z.comp.matchsubs = self.exp(self.subs);
    }

    fn imp(&mut self, l: &Cline) -> Ix {
        let mut head = None;
        let mut last: Option<usize> = None;
        let mut cur = l.as_deref();
        while let Some(c) = cur {
            let prefix = self.imp(&c.prefix);
            let suffix = self.imp(&c.suffix);
            let i = self.nd.len();
            self.nd.push(Nd {
                next: None,
                flags: c.flags,
                line: c.line.as_deref().map(Sp::new),
                llen: c.llen as isize,
                word: c.word.as_deref().map(Sp::new),
                wlen: c.wlen as isize,
                orig: c.orig.as_deref().map(Sp::new),
                olen: c.olen as isize,
                slen: c.slen as isize,
                prefix,
                suffix,
                min: c.min as isize,
                max: c.max as isize,
            });
            match last {
                Some(p) => self.nd[p].next = Some(i),
                None => head = Some(i),
            }
            last = Some(i);
            cur = c.next.as_deref();
        }
        head
    }

    fn exp(&self, l: Ix) -> Cline {
        let mut nodes = Vec::new();
        let mut cur = l;
        while let Some(i) = cur {
            // A cycle would hang zsh too; stop instead.
            if i == MARK || nodes.len() > self.nd.len() {
                break;
            }
            let n = &self.nd[i];
            nodes.push(ClineNode {
                next: None,
                flags: n.flags,
                line: n.line.as_ref().map(Sp::text),
                llen: n.llen as i32,
                word: n.word.as_ref().map(Sp::text),
                wlen: n.wlen as i32,
                orig: n.orig.as_ref().map(Sp::text),
                olen: n.olen as i32,
                slen: n.slen as i32,
                prefix: self.exp(n.prefix),
                suffix: self.exp(n.suffix),
                min: n.min as i32,
                max: n.max as i32,
            });
            cur = n.next;
        }
        let mut r: Cline = None;
        while let Some(mut n) = nodes.pop() {
            n.next = r;
            r = Some(Box::new(n));
        }
        r
    }

    fn set_link(&mut self, l: Link, v: Ix, head: &mut Ix) {
        match l {
            Link::Head => *head = v,
            Link::Next(i) => self.nd[i].next = v,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn get_cline(
        &mut self,
        l: Option<Sp>,
        ll: isize,
        w: Option<Sp>,
        wl: isize,
        o: Option<Sp>,
        ol: isize,
        fl: u32,
    ) -> usize {
        self.nd.push(Nd {
            line: l,
            llen: ll,
            word: w,
            wlen: wl,
            orig: o,
            olen: ol,
            flags: fl,
            ..Nd::default()
        });
        self.nd.len() - 1
    }

    fn cp_cline(&mut self, l: Ix, deep: bool) -> Ix {
        let mut head = None;
        let mut last: Option<usize> = None;
        let mut cur = l;
        while let Some(i) = cur {
            let mut t = self.nd[i].clone();
            if deep {
                if t.prefix.is_some() {
                    t.prefix = self.cp_cline(t.prefix, false);
                }
                if t.suffix.is_some() {
                    t.suffix = self.cp_cline(t.suffix, false);
                }
            }
            t.next = None;
            self.nd.push(t);
            let n = self.nd.len() - 1;
            match last {
                Some(p) => self.nd[p].next = Some(n),
                None => head = Some(n),
            }
            last = Some(n);
            cur = self.nd[i].next;
        }
        head
    }

    fn cline_sublen(&self, l: usize) -> isize {
        let n = &self.nd[l];
        let mut len = if n.flags & CLF_LINE != 0 {
            n.llen
        } else {
            n.wlen
        };
        let sub = if n.flags & CLF_SUF != 0 {
            n.suffix
        } else {
            n.prefix
        };
        if n.olen != 0 && sub.is_none() {
            len += n.olen;
        } else {
            for start in [n.prefix, n.suffix] {
                let mut p = start;
                while let Some(i) = p {
                    let q = &self.nd[i];
                    len += if q.flags & CLF_LINE != 0 {
                        q.llen
                    } else {
                        q.wlen
                    };
                    p = q.next;
                }
            }
        }
        len
    }

    fn cline_setlens(&mut self, mut l: Ix, both: bool) {
        while let Some(i) = l {
            let m = self.cline_sublen(i);
            self.nd[i].min = m;
            if both {
                self.nd[i].max = m;
            }
            l = self.nd[i].next;
        }
    }

    fn revert_cline(&mut self, mut p: Ix) -> Ix {
        let mut r = None;
        while let Some(i) = p {
            let n = self.nd[i].next;
            self.nd[i].next = r;
            r = Some(i);
            p = n;
        }
        r
    }

    // ---- building the match string and clines ----

    fn start_match(&mut self) {
        self.buf.clear();
        self.parts = None;
        self.lastpart = None;
        self.subs = None;
        self.lastsub = None;
    }

    fn abort_match(&mut self) {
        self.parts = None;
        self.subs = None;
    }

    fn add_match_str(
        &mut self,
        m: Option<&Cmatcher>,
        l: &[char],
        w: &[char],
        wl: isize,
        sfx: bool,
    ) {
        let (w, wl) = match m {
            Some(m) if m.flags & CMF_LINE != 0 => (l, m.llen as isize),
            _ => (w, wl),
        };
        if wl > 0 {
            let s: Vec<char> = (0..wl).map(|i| at(w, i)).collect();
            if sfx {
                self.buf.splice(0..0, s);
            } else {
                self.buf.extend(s);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_match_part(
        &mut self,
        m: Option<&Cmatcher>,
        l: Option<Sp>,
        w: Option<Sp>,
        wl: isize,
        o: Option<Sp>,
        ol: isize,
        s: Option<Sp>,
        sl: isize,
        osl: isize,
        sfx: bool,
    ) {
        let mut l = l;
        if let Some(lv) = &l {
            if strneq(lv.sl(), sp_sl(&w), wl) {
                l = None;
            }
        }

        let (mut p, mut lp, lprem) = self.bld_parts(s, sl, osl);
        if let (Some(r), Some(m)) = (lprem, m) {
            if m.flags & CMF_LEFT != 0 {
                self.nd[r].flags |= CLF_SUF;
                self.nd[r].suffix = self.nd[r].prefix;
                self.nd[r].prefix = None;
            }
        }
        if sfx {
            lp = p;
            p = self.revert_cline(p);
        }
        let (Some(pi), Some(mut lpi)) = (p, lp) else {
            return;
        };
        if self.subs.is_some() {
            if sfx {
                if let Some(mut q) = self.nd[lpi].prefix {
                    while let Some(n) = self.nd[q].next {
                        q = n;
                    }
                    self.nd[q].next = self.subs;
                } else {
                    self.nd[lpi].prefix = self.subs;
                }
                if let Some(ls) = self.lastsub {
                    self.nd[ls].next = None;
                }
            } else {
                if let Some(ls) = self.lastsub {
                    self.nd[ls].next = self.nd[pi].prefix;
                }
                self.nd[pi].prefix = self.subs;
            }
            self.subs = None;
            self.lastsub = None;
        }
        let has_orig = o.is_some() || ol != 0;
        if self.nd[lpi].llen != 0 || self.nd[lpi].wlen != 0 {
            let n = self.get_cline(l, wl, w, wl, o, ol, CLF_NEW);
            self.nd[lpi].next = Some(n);
            lpi = n;
        } else {
            let n = &mut self.nd[lpi];
            n.line = l;
            n.llen = wl;
            n.word = w;
            n.wlen = wl;
            n.orig = o;
            n.olen = ol;
        }
        if has_orig {
            self.nd[lpi].flags &= !CLF_NEW;
        }
        match self.lastpart {
            Some(lpt) => self.nd[lpt].next = p,
            None => self.parts = p,
        }
        self.lastpart = Some(lpi);
    }

    fn add_match_sub(
        &mut self,
        m: Option<&Cmatcher>,
        l: Option<Sp>,
        ll: isize,
        w: Option<Sp>,
        wl: isize,
    ) {
        let (w, wl, flags) = match m {
            Some(m) if m.flags & CMF_LINE != 0 => (None, 0, CLF_LINE),
            _ => (w, wl, 0),
        };
        if wl != 0 || ll != 0 {
            let (n, lp, _) = self.bld_parts(w.clone(), wl, ll);
            if let Some(ni) = n.filter(|_| n != lp) {
                let mut p = ni;
                while self.nd[p].next != lp {
                    match self.nd[p].next {
                        Some(x) => p = x,
                        None => break,
                    }
                }
                if self.subs.is_some() {
                    if let Some(ls) = self.lastsub {
                        self.nd[ls].next = self.nd[ni].prefix;
                    }
                    self.nd[ni].prefix = self.subs;
                }
                self.subs = lp;
                self.lastsub = lp;
                match self.lastpart {
                    Some(lpt) => self.nd[lpt].next = n,
                    None => self.parts = n,
                }
                self.nd[p].next = None;
                self.lastpart = Some(p);
            } else {
                let skip = if m.is_some_and(|m| m.wlen == -2) {
                    CLF_SKIP
                } else {
                    0
                };
                let n = self.get_cline(l, ll, w, wl, None, 0, flags | skip);
                match self.lastsub {
                    Some(ls) => self.nd[ls].next = Some(n),
                    None => self.subs = Some(n),
                }
                self.lastsub = Some(n);
            }
        }
    }

    /// Splits `str` into clines at the anchors of `*` match specs. Returns
    /// the list, its last node, and that node again if it holds the rest
    /// of the string rather than an anchor.
    fn bld_parts(&mut self, str: Option<Sp>, mut len: isize, mut plen: isize) -> (Ix, Ix, Ix) {
        let base = str.unwrap_or_else(|| Sp::from_vec(Vec::new()));
        let bm = self.bmatchers.clone();
        let mut ret: Ix = None;
        let mut last: Ix = None;
        let mut n: Ix = None;
        let mut op = plen;
        let os: isize = 0;
        let mut s: isize = 0;
        let mut p: isize = 0;
        let lprem;

        while len > 0 {
            let mut t = false;
            for mp in bm.iter() {
                if t {
                    break;
                }
                let (ralen, lalen) = (mp.ralen as isize, mp.lalen as isize);
                if mp.flags == CMF_RIGHT
                    && mp.wlen < 0
                    && ralen != 0
                    && mp.llen == 0
                    && len >= ralen
                    && (s - os) >= lalen
                    && pattern_match(&mp.right, base.add(s).sl(), &[], &[])
                    && (lalen == 0
                        || ((s - os) >= lalen
                            && pattern_match(&mp.left, base.add(s - lalen).sl(), &[], &[])))
                {
                    let olen = s - p;
                    let nn = self.get_cline(
                        None,
                        ralen,
                        Some(base.add(s)),
                        ralen,
                        None,
                        0,
                        if plen <= 0 { CLF_NEW } else { 0 },
                    );
                    match last {
                        Some(lt) => self.nd[lt].next = Some(nn),
                        None => ret = Some(nn),
                    }
                    last = Some(nn);
                    n = Some(nn);
                    if p != s {
                        let llen = op.max(0).min(olen);
                        let pre = self.get_cline(None, llen, Some(base.add(p)), olen, None, 0, 0);
                        self.nd[nn].prefix = Some(pre);
                    }
                    s += ralen;
                    len -= ralen;
                    plen -= ralen;
                    op -= olen;
                    p = s;
                    t = true;
                }
            }
            if !t {
                s += 1;
                len -= 1;
                plen -= 1;
            }
        }
        let fl = if plen <= 0 { CLF_NEW } else { 0 };
        if p != s {
            let olen = s - p;
            let llen = op.max(0).min(olen);
            let nn = self.get_cline(None, 0, None, 0, None, 0, fl);
            match last {
                Some(lt) => self.nd[lt].next = Some(nn),
                None => ret = Some(nn),
            }
            n = Some(nn);
            let pre = self.get_cline(None, llen, Some(base.add(p)), olen, None, 0, 0);
            self.nd[nn].prefix = Some(pre);
            lprem = Some(nn);
        } else if ret.is_none() {
            let nn = self.get_cline(None, 0, None, 0, None, 0, fl);
            ret = Some(nn);
            n = Some(nn);
            lprem = Some(nn);
        } else {
            lprem = None;
        }
        if let Some(ni) = n {
            self.nd[ni].next = None;
        }
        (ret, n, lprem)
    }

    // ---- match_str ----

    /// Test-only match of the first `n` characters of `l` and `w`.
    fn match_parts(&mut self, l: &[char], w: &[char], n: isize, part: bool) -> i32 {
        let n = n.max(0) as usize;
        let l = &l[..n.min(l.len())];
        let w = &w[..n.min(w.len())];
        self.match_str(l, w, &mut [], &mut 0, 0, None, false, 1, part)
    }

    /// zsh's match_str(): does the line string `l` match the word `w`
    /// using the matchers on the stack? With `test` zero, the match
    /// string and clines are built. `brs[*bpp..]` are the brace infos
    /// still to place; `sfx` matches from the ends backwards.
    #[allow(clippy::too_many_arguments)]
    fn match_str(
        &mut self,
        l: &[char],
        w: &[char],
        brs: &mut [Brinfo],
        bpp: &mut usize,
        mut bc: i32,
        rwlp: Option<&mut i32>,
        sfx: bool,
        test: i32,
        part: bool,
    ) -> i32 {
        let lb: Rc<Vec<char>> = Rc::new(l[..zstrlen(l) as usize].to_vec());
        let wb: Rc<Vec<char>> = Rc::new(w[..zstrlen(w) as usize].to_vec());
        let lsp = |i: isize| Sp {
            b: lb.clone(),
            o: i,
        };
        let wsp = |i: isize| Sp {
            b: wb.clone(),
            o: i,
        };

        let mut ll = lb.len() as isize;
        let mut lw = wb.len() as isize;
        let (mut il, mut iw): (isize, isize) = (0, 0);
        let (mut exact, mut wexact): (isize, isize) = (0, 0);
        let mut he = false;
        let mut lm: *const Cmatcher = std::ptr::null();
        let mut bp = usize::MAX;
        let obc = bc;
        let ind: isize = if sfx { -1 } else { 0 };
        let add: isize = if sfx { -1 } else { 1 };
        let original_ll = ll;
        let original_lw = lw;
        let useqbr = self.useqbr;
        let mstack = self.mstack.clone();

        if test == 0 {
            self.start_match();
            bp = *bpp;
        }
        let bpos = |b: &Brinfo| if useqbr { b.qpos } else { b.pos };

        let mut lp: isize = 0;
        let mut wp: isize = 0;
        if sfx {
            lp += ll;
            wp += lw;
        }
        let mut ow = wp;

        if test == 0 && bp < brs.len() && bc >= brs[bp].pos {
            brs[bp].curpos = bc;
            bp += 1;
        }

        'outer: while ll != 0 {
            let mut bslash = false;
            if !sfx
                && lw != 0
                && (!part || test != 0)
                && (at(&lb, lp + ind) == at(&wb, wp + ind) || {
                    bslash =
                        lw > 1 && at(&wb, wp + ind) == '\\' && at(&wb, wp + ind + 1) == at(&lb, lp);
                    bslash
                })
            {
                let b = bslash as isize;
                lp += add;
                wp += if bslash { add + add } else { add };
                il += 1;
                iw += 1 + b;
                ll -= 1;
                lw -= 1 + b;
                bc += 1;
                exact += 1;
                wexact += 1 + b;
                if test == 0 {
                    while bp < brs.len() && bc >= bpos(&brs[bp]) {
                        brs[bp].curpos = self.buf.len() as i32 + (wp - ow) as i32 + obc;
                        bp += 1;
                    }
                }
                lm = std::ptr::null();
                he = false;
                continue;
            }
            'retry: loop {
                let mut found = false;
                'ms: for head in mstack.iter() {
                    let mut mpo: Option<&Cmatcher> = Some(&**head);
                    while let Some(mp) = mpo {
                        mpo = mp.next.as_deref();
                        let mllen = mp.llen as isize;
                        let mwlen = mp.wlen as isize;
                        let (lalen, ralen) = (mp.lalen as isize, mp.ralen as isize);
                        let inter = if mp.flags & CMF_INTER != 0 {
                            if mp.flags & CMF_LINE != 0 {
                                iw != 0
                            } else {
                                il != 0
                            }
                        } else {
                            il != 0 || iw != 0
                        };
                        if std::ptr::eq(lm, mp)
                            || ((original_ll == ll || original_lw == lw)
                                && (test == 1
                                    || (test != 0 && mp.left.is_empty() && mp.right.is_empty()))
                                && mp.wlen < 0)
                        {
                            continue;
                        }

                        if mp.wlen < 0 {
                            let mut llen = mllen;
                            let (mut alen, aol) = if mp.flags & CMF_LEFT != 0 {
                                (lalen, ralen)
                            } else {
                                (ralen, lalen)
                            };
                            if ll < llen + alen || lw < alen + aol {
                                continue;
                            }
                            let left = mp.flags & CMF_LEFT != 0;
                            let (ap, aop) = if left {
                                (&mp.left, &mp.right)
                            } else {
                                (&mp.right, &mp.left)
                            };
                            let moff = if left { alen } else { 0 };
                            let (both, loff, aoff) = match (left, sfx) {
                                (true, true) => (false, -llen, -(llen + alen)),
                                (true, false) => (true, alen, 0),
                                (false, true) => (true, -(llen + alen), -alen),
                                (false, false) => (false, 0, llen),
                            };
                            if !pattern_match(&mp.line, sl(&lb, lp + loff), &[], &[]) {
                                continue;
                            }
                            if !ap.is_empty() {
                                if !pattern_match(ap, sl(&lb, lp + aoff), &[], &[])
                                    || (both
                                        && (!pattern_match(ap, sl(&wb, wp + aoff), &[], &[])
                                            || (aol != 0
                                                && aol <= aoff + iw
                                                && !pattern_match(
                                                    aop,
                                                    sl(&wb, wp + aoff - aol),
                                                    &[],
                                                    &[],
                                                ))
                                            || self.match_parts(
                                                sl(&lb, lp + aoff),
                                                sl(&wb, wp + aoff),
                                                alen,
                                                part,
                                            ) == 0))
                                {
                                    continue;
                                }
                            } else if !both || inter {
                                continue;
                            }

                            // zsh cuts the line string here while it looks
                            // for the end of what the `*' matched.
                            let ltrunc = lp - (llen + alen);
                            let mut t = 0;
                            let mut tp = wp;
                            let mut ct: isize = 0;
                            let mut ict = lw - alen + 1;
                            while ict != 0 {
                                let ok = (both
                                    && (ap.is_empty()
                                        || test == 0
                                        || !pattern_match(ap, sl(&wb, tp + aoff), &[], &[])
                                        || (aol != 0
                                            && aol <= aoff + ct + iw
                                            && !pattern_match(
                                                aop,
                                                sl(&wb, tp + aoff - aol),
                                                &[],
                                                &[],
                                            ))))
                                    || (!both
                                        && pattern_match(ap, sl(&wb, tp - moff), &[], &[])
                                        && (aol == 0
                                            || (aol <= iw + ct - moff
                                                && pattern_match(
                                                    aop,
                                                    sl(&wb, tp - moff - aol),
                                                    &[],
                                                    &[],
                                                )))
                                        && (mp.wlen == -1
                                            || self.match_parts(
                                                lcut(&lb, lp + aoff, sfx, ltrunc),
                                                sl(&wb, tp - moff),
                                                alen,
                                                part,
                                            ) != 0));
                                if ok {
                                    if !both
                                        && mp.wlen == -1
                                        && self.match_parts(
                                            lcut(&lb, lp + aoff, sfx, ltrunc),
                                            sl(&wb, tp - moff),
                                            alen,
                                            part,
                                        ) == 0
                                    {
                                        break;
                                    }
                                    if sfx {
                                        let ls = &lb[0..ltrunc.max(0) as usize];
                                        let we = (tp - alen).clamp(0, wb.len() as isize) as usize;
                                        let ws = &wb[0..we];
                                        t = self.match_str(
                                            ls,
                                            ws,
                                            &mut [],
                                            &mut 0,
                                            0,
                                            None,
                                            sfx,
                                            2,
                                            part,
                                        );
                                    } else {
                                        t = self.match_str(
                                            sl(&lb, lp + llen + moff),
                                            sl(&wb, tp + moff),
                                            &mut [],
                                            &mut 0,
                                            0,
                                            None,
                                            sfx,
                                            1,
                                            part,
                                        );
                                    }
                                    if t != 0 || (mp.wlen == -1 && !both) {
                                        break;
                                    }
                                }
                                tp += add;
                                ct += 1;
                                ict -= 1;
                            }
                            let ict = ct;

                            if t == 0 {
                                continue;
                            }

                            if test == 0 && (!he || (llen + alen) != 0) {
                                let (op, mut ol, lpp, map) = if sfx {
                                    (wp, ow - wp, lp - (llen + alen), tp - alen)
                                } else {
                                    (ow, wp - ow, lp, ow)
                                };
                                let (wap, wmp) = match (left, sfx) {
                                    (true, true) => (tp - alen, tp),
                                    (false, true) => (wp - alen, tp - alen),
                                    (true, false) => (wp, wp + alen),
                                    (false, false) => (tp, ow),
                                };
                                if mp.flags & CMF_LINE != 0 {
                                    self.add_match_str(None, &[], sl(&wb, op), ol, sfx);
                                    self.add_match_str(None, &[], sl(&lb, lpp), llen + alen, sfx);
                                    self.add_match_sub(None, None, ol, Some(wsp(op)), ol);
                                    self.add_match_sub(
                                        None,
                                        None,
                                        llen + alen,
                                        Some(lsp(lpp)),
                                        llen + alen,
                                    );
                                } else if sfx {
                                    self.add_match_str(
                                        None,
                                        &[],
                                        sl(&wb, map),
                                        ct + ol + alen,
                                        sfx,
                                    );
                                    self.add_match_part(
                                        Some(mp),
                                        Some(lsp(lp + aoff)),
                                        Some(wsp(wap)),
                                        alen,
                                        Some(lsp(lp + loff)),
                                        llen,
                                        Some(wsp(op)),
                                        ol,
                                        ol,
                                        sfx,
                                    );
                                    self.add_match_sub(None, None, 0, Some(wsp(wmp)), ct);
                                } else {
                                    self.add_match_str(
                                        None,
                                        &[],
                                        sl(&wb, map),
                                        ct + ol + alen,
                                        sfx,
                                    );
                                    if both {
                                        self.add_match_sub(None, None, ol, Some(wsp(op)), ol);
                                        ol = -1;
                                    } else {
                                        ct += ol;
                                    }
                                    self.add_match_part(
                                        Some(mp),
                                        Some(lsp(lp + aoff)),
                                        Some(wsp(wap)),
                                        alen,
                                        Some(lsp(lp + loff)),
                                        llen,
                                        Some(wsp(wmp)),
                                        ct,
                                        ol,
                                        sfx,
                                    );
                                }
                            }
                            llen += alen;
                            alen += ict;
                            if sfx {
                                lp -= llen;
                                wp -= alen;
                            } else {
                                lp += llen;
                                wp += alen;
                            }
                            ll -= llen;
                            il += llen;
                            lw -= alen;
                            iw += alen;
                            bc += llen as i32;
                            exact = 0;

                            if test == 0 {
                                while bp < brs.len() && bc >= bpos(&brs[bp]) {
                                    let bpc = bpos(&brs[bp]);
                                    brs[bp].curpos = self.buf.len() as i32 + bpc - bc + obc;
                                    bp += 1;
                                }
                            }
                            ow = wp;

                            found = true;
                            if llen == 0 && alen == 0 {
                                lm = mp;
                                if he {
                                    // zsh clears mp so the outer loop
                                    // carries on with the next spec.
                                    found = false;
                                } else {
                                    he = true;
                                }
                            } else {
                                lm = std::ptr::null();
                                he = false;
                            }
                            if found {
                                break 'ms;
                            }
                            continue 'ms;
                        } else if ll >= mllen && lw >= mwlen {
                            let mut t = true;

                            if mp.flags & (CMF_LEFT | CMF_RIGHT) == 0
                                && mp.llen == mp.wlen
                                && (if sfx {
                                    strneq(sl(&lb, lp - mllen), sl(&wb, wp - mwlen), mllen)
                                } else {
                                    strneq(sl(&lb, lp), sl(&wb, wp), mllen)
                                })
                            {
                                continue;
                            }

                            let (tl, tw, til, tiw, tll, tlw) = if sfx {
                                (
                                    lp - mllen,
                                    wp - mwlen,
                                    ll - mllen,
                                    lw - mwlen,
                                    il + mllen,
                                    iw + mwlen,
                                )
                            } else {
                                (lp, wp, il, iw, ll, lw)
                            };
                            if mp.flags & CMF_LEFT != 0 {
                                if til < lalen || tiw < lalen + ralen {
                                    continue;
                                } else if !mp.left.is_empty() {
                                    t = pattern_match(&mp.left, sl(&lb, tl - lalen), &[], &[])
                                        && pattern_match(&mp.left, sl(&wb, tw - lalen), &[], &[])
                                        && (ralen == 0
                                            || pattern_match(
                                                &mp.right,
                                                sl(&wb, tw - lalen - ralen),
                                                &[],
                                                &[],
                                            ));
                                } else {
                                    t = !sfx && !inter;
                                }
                            }
                            if mp.flags & CMF_RIGHT != 0 {
                                if tll < mllen + ralen || tlw < mwlen + ralen + lalen {
                                    continue;
                                } else if !mp.right.is_empty() {
                                    t = pattern_match(&mp.right, sl(&lb, tl + mllen), &[], &[])
                                        && pattern_match(&mp.right, sl(&wb, tw + mwlen), &[], &[])
                                        && (lalen == 0
                                            || pattern_match(
                                                &mp.left,
                                                sl(&wb, tw + mwlen - ralen - lalen),
                                                &[],
                                                &[],
                                            ));
                                } else {
                                    t = sfx && !inter;
                                }
                            }
                            if !t || !pattern_match(&mp.line, sl(&lb, tl), &mp.word, sl(&wb, tw)) {
                                continue;
                            }

                            if test == 0 {
                                if sfx {
                                    if ow >= wp {
                                        self.add_match_str(None, &[], sl(&wb, wp), ow - wp, sfx);
                                    }
                                } else if wp >= ow {
                                    self.add_match_str(None, &[], sl(&wb, ow), wp - ow, sfx);
                                }
                                self.add_match_str(Some(mp), sl(&lb, tl), sl(&wb, tw), mwlen, sfx);
                                if sfx {
                                    if ow >= wp {
                                        self.add_match_sub(None, None, 0, Some(wsp(wp)), ow - wp);
                                    }
                                } else if wp >= ow {
                                    self.add_match_sub(None, None, 0, Some(wsp(ow)), wp - ow);
                                }
                                self.add_match_sub(
                                    Some(mp),
                                    Some(lsp(tl)),
                                    mllen,
                                    Some(wsp(tw)),
                                    mwlen,
                                );
                            }
                            if sfx {
                                lp = tl;
                                wp = tw;
                            } else {
                                lp += mllen;
                                wp += mwlen;
                            }
                            il += mllen;
                            iw += mwlen;
                            ll -= mllen;
                            lw -= mwlen;
                            bc += mllen as i32;
                            exact = 0;

                            if test == 0 {
                                while bp < brs.len() && bc >= bpos(&brs[bp]) {
                                    let bpc = bpos(&brs[bp]);
                                    brs[bp].curpos = self.buf.len() as i32 + bpc - bc + obc;
                                    bp += 1;
                                }
                            }
                            ow = wp;
                            lm = std::ptr::null();
                            he = false;
                            found = true;
                            break 'ms;
                        }
                    }
                }
                if found {
                    continue 'outer;
                }

                let mut bslash = false;
                if (test == 0 || sfx)
                    && lw != 0
                    && (at(&lb, lp + ind) == at(&wb, wp + ind) || {
                        bslash = lw > 1
                            && at(&wb, wp + ind) == '\\'
                            && at(&wb, wp + ind + 1) == at(&lb, lp);
                        bslash
                    })
                {
                    let b = bslash as isize;
                    lp += add;
                    wp += if bslash { add + add } else { add };
                    il += 1;
                    iw += 1 + b;
                    ll -= 1;
                    lw -= 1 + b;
                    bc += 1;
                    if test == 0 {
                        while bp < brs.len() && bc >= bpos(&brs[bp]) {
                            let d = if sfx { ow - wp } else { wp - ow };
                            brs[bp].curpos = self.buf.len() as i32 + d as i32 + obc;
                            bp += 1;
                        }
                    }
                    lm = std::ptr::null();
                    he = false;
                    continue 'outer;
                }
                if lw == 0 {
                    break 'outer;
                }
                if exact != 0 && !part {
                    // Give back the characters taken as equal and try
                    // again, preferring the match specs.
                    il -= exact;
                    iw -= wexact;
                    ll += exact;
                    lw += wexact;
                    bc -= exact as i32;
                    lp -= add * exact;
                    wp -= add * wexact;
                    exact = 0;
                    wexact = 0;
                    continue 'retry;
                }
                if test != 0 {
                    return 0;
                }
                self.abort_match();
                return -1;
            }
        }
        if test != 0 {
            return (part || ll == 0) as i32;
        }
        if !part && ll != 0 {
            self.abort_match();
            return -1;
        }
        if let Some(r) = rwlp {
            *r = (iw - if sfx { ow - wp } else { wp - ow }) as i32;
        }
        // Anchors of a suffix match end up one cline too far; shift them.
        if sfx {
            if let Some(mp0) = self.parts {
                if self.nd[mp0].prefix.is_some() || self.nd[mp0].suffix.is_some() {
                    let t = self.get_cline(None, 0, None, 0, None, 0, 0);
                    self.nd[t].next = self.parts;
                    if self.nd[mp0].prefix.is_some() {
                        self.nd[t].prefix = Some(MARK);
                    } else {
                        self.nd[t].suffix = Some(MARK);
                    }
                    self.parts = Some(t);
                }
                let mut t = self.parts.unwrap_or(mp0);
                while let Some(tn) = self.nd[t].next {
                    let s = if self.nd[tn].prefix.is_some() {
                        self.nd[tn].prefix
                    } else {
                        self.nd[tn].suffix
                    };
                    if self.nd[t].suffix.is_some() {
                        self.nd[t].suffix = s;
                    } else {
                        self.nd[t].prefix = s;
                    }
                    t = tn;
                }
                self.nd[t].prefix = None;
                self.nd[t].suffix = None;
            }
        }
        *bpp = bp;
        (if part { il } else { iw }) as i32
    }

    // ---- joining cline lists ----

    /// A string that both `sa` and `sb` match, built with the matchers.
    fn join_strs(&mut self, mut la: isize, sa: Sp, mut lb: isize, sb: Sp) -> Option<Vec<char>> {
        let bm = self.bmatchers.clone();
        let mut rs: Vec<char> = Vec::new();
        let (mut sa, mut sb) = (sa, sb);

        while la != 0 && lb != 0 {
            if sa.at(0) != sb.at(0) {
                let mut t = false;
                for mp in bm.iter() {
                    if t {
                        break;
                    }
                    let (mllen, mwlen) = (mp.llen as isize, mp.wlen as isize);
                    if mp.flags == 0 && mwlen > 0 && mllen > 0 && mwlen <= la && mwlen <= lb {
                        t = pattern_match(&mp.word, sa.sl(), &[], &[]);
                        if t || pattern_match(&mp.word, sb.sl(), &[], &[]) {
                            let mut line = vec!['\0'; mllen as usize];
                            let bl = if t {
                                self.bld_line(mp, &mut line, sa.sl(), sb.sl(), lb, false)
                            } else {
                                self.bld_line(mp, &mut line, sb.sl(), sa.sl(), la, false)
                            };
                            if bl != 0 {
                                rs.extend_from_slice(&line);
                                JOIN_STRS_ALLOC.with(|a| a.set(true));
                                if t {
                                    sa = sa.add(mwlen);
                                    la -= mwlen;
                                    sb = sb.add(bl);
                                    lb -= bl;
                                } else {
                                    sb = sb.add(mwlen);
                                    lb -= mwlen;
                                    sa = sa.add(bl);
                                    la -= bl;
                                }
                                t = true;
                            } else {
                                t = false;
                            }
                        }
                    }
                }
                if !t {
                    break;
                }
            } else {
                JOIN_STRS_ALLOC.with(|a| a.set(true));
                rs.push(sa.at(0));
                sa = sa.add(1);
                sb = sb.add(1);
                la -= 1;
                lb -= 1;
            }
        }
        if la != 0 || lb != 0 || !JOIN_STRS_ALLOC.with(|a| a.get()) {
            return None;
        }
        Some(rs)
    }

    /// Builds in `line` a line string for the matcher `mp` that matches
    /// `word` (whose first `wlen` characters are used), with `mword` the
    /// string that matched the word pattern before. Returns the number of
    /// word characters matched, zero on failure.
    fn bld_line(
        &mut self,
        mp: &Cmatcher,
        line: &mut [char],
        mword: &[char],
        word: &[char],
        wlen: isize,
        sfx: bool,
    ) -> isize {
        let mut wlen = wlen.max(0);
        let wordchars: Vec<char> = (0..wlen).map(|i| at(word, i)).collect();
        let mut gen: Vec<Cpattern> = Vec::with_capacity(mp.line.len());
        let mut wpi = 0usize;
        let mut mwi: isize = 0;

        for lpat in &mp.line {
            let mut wind = 0;
            let mut wmtp = 0;
            let mut wchr = '\0';
            if matches!(lpat, Cpattern::Equiv(_)) && wpi < mp.word.len() && at(mword, mwi) != '\0' {
                wchr = at(mword, mwi);
                wind = pattern_match1(&mp.word[wpi], wchr, &mut wmtp);
                wpi += 1;
                mwi += 1;
            }
            if wind != 0 {
                match pattern_match_equivalence(lpat, wind, wmtp, wchr) {
                    Some(c) => gen.push(Cpattern::Char(c)),
                    None => return 0,
                }
            } else {
                gen.push(lpat.clone());
            }
        }

        let mut llen = mp.llen as isize;
        let mut rl: isize = 0;
        let (mut lpos, mut wpos, mut gpos): (isize, isize, isize) =
            if sfx { (llen, wlen, llen) } else { (0, 0, 0) };
        let bm = self.bmatchers.clone();
        let gat = |g: &[Cpattern], i: isize| -> Option<Cpattern> {
            if i < 0 {
                None
            } else {
                g.get(i as usize).cloned()
            }
        };

        while llen > 0 && wlen > 0 {
            let (wp, tg) = if sfx {
                (wpos - 1, gpos - 1)
            } else {
                (wpos, gpos)
            };
            let mut wmtp = 0;
            let wc = at(&wordchars, wp);
            let gp = gat(&gen, tg);
            if gp
                .as_ref()
                .is_some_and(|g| pattern_match1(g, wc, &mut wmtp) != 0)
            {
                let lchr = match gp {
                    Some(Cpattern::Char(c)) => c,
                    _ => wc,
                };
                if sfx {
                    lpos -= 1;
                    if lpos >= 0 && (lpos as usize) < line.len() {
                        line[lpos as usize] = lchr;
                    }
                } else {
                    if (lpos as usize) < line.len() {
                        line[lpos as usize] = lchr;
                    }
                    lpos += 1;
                }
                llen -= 1;
                wlen -= 1;
                rl += 1;
                if sfx {
                    wpos = wp;
                    gpos = tg;
                } else {
                    if llen > 0 {
                        gpos += 1;
                    }
                    wpos += 1;
                }
            } else {
                let mut matched = false;
                for mp2 in bm.iter() {
                    let (m2l, m2w) = (mp2.llen as isize, mp2.wlen as isize);
                    if mp2.flags == 0 && m2w <= wlen && m2l <= llen {
                        let (mut lp2, mut wp2, mut tg2) = (lpos, wpos, gpos);
                        if sfx {
                            lp2 -= m2l;
                            wp2 -= m2w;
                            tg2 -= m2l;
                        }
                        let (lp2u, tg2u) = (lp2.max(0) as usize, tg2.max(0) as usize);
                        let line_len = line.len();
                        let ok = pattern_match_restrict(
                            &mp2.line,
                            &mp2.word,
                            sl(&wordchars, wp2),
                            wlen - wp2,
                            &gen[tg2u.min(gen.len())..],
                            &mut line[lp2u.min(line_len)..],
                        );
                        if ok {
                            if sfx {
                                lpos = lp2;
                                wpos = wp2;
                                gpos = tg2;
                            } else {
                                lpos += m2l;
                                wpos += m2w;
                                gpos += m2l;
                            }
                            llen -= m2l;
                            wlen -= m2w;
                            rl += m2w;
                            matched = true;
                            break;
                        }
                    }
                }
                if !matched {
                    return 0;
                }
            }
        }
        if llen == 0 {
            rl
        } else {
            0
        }
    }

    fn cmp_anchors(&mut self, o: usize, n: usize, join: bool) -> i32 {
        let mut line = false;
        let c1 = {
            let (a, b) = (&self.nd[o], &self.nd[n]);
            a.flags & CLF_LINE == 0
                && a.wlen == b.wlen
                && (a.word.is_none() || strneq(sp_sl(&a.word), sp_sl(&b.word), a.wlen))
        };
        if c1
            || ({
                let (a, b) = (&self.nd[o], &self.nd[n]);
                line = (a.line.is_none() && b.line.is_none() && a.wlen == 0 && b.wlen == 0)
                    || (a.llen == b.llen
                        && a.line.is_some()
                        && b.line.is_some()
                        && strneq(sp_sl(&a.line), sp_sl(&b.line), a.llen));
                line
            })
        {
            if line {
                let a = &mut self.nd[o];
                a.flags |= CLF_LINE;
                a.word = None;
                a.wlen = 0;
            }
            return 1;
        }
        if join && self.nd[o].flags & CLF_JOIN == 0 {
            if let (Some(ow), Some(nw)) = (self.nd[o].word.clone(), self.nd[n].word.clone()) {
                let (owl, nwl) = (self.nd[o].wlen, self.nd[n].wlen);
                if let Some(j) = self.join_strs(owl, ow, nwl, nw) {
                    let a = &mut self.nd[o];
                    a.flags |= CLF_JOIN;
                    a.wlen = j.len() as isize;
                    a.word = Some(Sp::from_vec(j));
                    return 2;
                }
            }
        }
        0
    }

    fn check_cmdata(&self, md: &mut Cmdata, sfx: bool) -> bool {
        if md.len == 0 {
            let Some(cl) = md.cl else {
                return true;
            };
            let c = &self.nd[cl];
            if c.flags & CLF_LINE != 0 {
                md.line = true;
                md.len = c.llen;
                md.str = c.line.clone();
            } else {
                md.line = false;
                md.len = c.wlen;
                md.olen = c.wlen;
                md.str = if sfx {
                    sp_add(&c.word, md.len)
                } else {
                    c.word.clone()
                };
                md.alen = c.llen;
                md.astr = if sfx {
                    sp_add(&c.line, md.alen)
                } else {
                    c.line.clone()
                };
            }
            md.pcl = md.cl;
            md.cl = c.next;
        }
        false
    }

    fn undo_cmdata(&mut self, md: &Cmdata, sfx: bool) -> Ix {
        let r = md.pcl?;
        let back = if sfx { -md.len } else { 0 };
        let n = &mut self.nd[r];
        if md.line {
            n.word = None;
            n.wlen = 0;
            n.flags |= CLF_LINE;
            n.llen = md.len;
            n.line = sp_add(&md.str, back);
        } else if md.len != md.olen {
            n.wlen = md.len;
            n.word = sp_add(&md.str, back);
        }
        Some(r)
    }

    #[allow(clippy::too_many_arguments)]
    fn join_sub(
        &mut self,
        md: &mut Cmdata,
        str: Option<Sp>,
        len: isize,
        mlen: &mut isize,
        sfx: bool,
        join: bool,
    ) -> Ix {
        if self.check_cmdata(md, sfx) {
            return None;
        }
        let mut ow = str.clone();
        let nw = md.str.clone();
        let (mut ol, mut nl) = (len, md.len);
        if sfx {
            ow = sp_add(&ow, ol);
        }
        let bm = self.bmatchers.clone();
        let mut t = false;
        for mp in bm.iter() {
            if t {
                break;
            }
            let (mllen, mwlen) = (mp.llen as isize, mp.wlen as isize);
            if mp.flags != 0 || mwlen <= 0 || mllen <= 0 {
                continue;
            }
            let lback = if sfx { mllen } else { 0 };
            let wback = if sfx { mwlen } else { 0 };
            if mllen <= ol
                && mwlen <= nl
                && pattern_match(
                    &mp.line,
                    sp_sl(&sp_add(&ow, -lback)),
                    &mp.word,
                    sp_sl(&sp_add(&nw, -wback)),
                )
            {
                md.str = sp_add(&md.str, if sfx { -mwlen } else { mwlen });
                md.len -= mwlen;
                *mlen = mllen;
                return Some(self.get_cline(None, 0, sp_add(&ow, -lback), mllen, None, 0, 0));
            }
            if join
                && mwlen <= ol
                && mwlen <= nl
                && ({
                    t = pattern_match(&mp.word, sp_sl(&sp_add(&ow, -wback)), &[], &[]);
                    t || pattern_match(&mp.word, sp_sl(&sp_add(&nw, -wback)), &[], &[])
                })
            {
                let mut line = vec!['\0'; mllen as usize];
                let mw = if t {
                    sp_add(&ow, -wback)
                } else {
                    sp_add(&nw, -wback)
                };
                let wstr = if t {
                    sp_add(&nw, if sfx { -nl } else { 0 })
                } else {
                    str.clone()
                };
                let bl = self.bld_line(
                    mp,
                    &mut line,
                    sp_sl(&mw),
                    sp_sl(&wstr),
                    if t { nl } else { ol },
                    sfx,
                );
                if bl != 0 {
                    if t {
                        ol = mwlen;
                        nl = bl;
                    } else {
                        ol = bl;
                        nl = mwlen;
                    }
                    md.str = sp_add(&md.str, if sfx { -nl } else { nl });
                    md.len -= nl;
                    *mlen = ol;
                    return Some(self.get_cline(
                        None,
                        0,
                        Some(Sp::from_vec(line)),
                        mllen,
                        None,
                        0,
                        CLF_JOIN,
                    ));
                }
            }
        }
        None
    }

    fn sub_match(&self, md: &mut Cmdata, str: Option<Sp>, mut len: isize, sfx: bool) -> isize {
        let mut ret = 0;
        let (ind, add): (isize, isize) = if sfx { (-1, -1) } else { (0, 1) };
        let mut str = if sfx { sp_add(&str, len) } else { str };
        while len != 0 {
            if self.check_cmdata(md, sfx) {
                return ret;
            }
            let mut l: isize = 0;
            let (mut p, mut q): (isize, isize) = (0, 0);
            while l < len && l < md.len && sp_at(&str, p + ind) == sp_at(&md.str, q + ind) {
                l += 1;
                p += add;
                q += add;
            }
            if l != 0 {
                md.len -= l;
                len -= l;
                if sfx {
                    md.str = sp_add(&md.str, -l);
                    str = sp_add(&str, -l);
                } else {
                    md.str = sp_add(&md.str, l);
                    str = sp_add(&str, l);
                }
                ret += l;
            } else if md.line || md.len != md.olen || md.astr.is_none() {
                return ret;
            } else {
                md.line = true;
                md.len = md.alen;
                md.str = md.astr.clone();
            }
        }
        ret
    }

    /// The string field of `o` that join_psfx() works on.
    fn get_s(&self, o: usize, line: bool) -> (Option<Sp>, isize) {
        let n = &self.nd[o];
        if line {
            (n.line.clone(), n.llen)
        } else {
            (n.word.clone(), n.wlen)
        }
    }

    fn set_s(&mut self, o: usize, line: bool, s: Option<Sp>, len: isize) {
        let n = &mut self.nd[o];
        if line {
            n.line = s;
            n.llen = len;
        } else {
            n.word = s;
            n.wlen = len;
        }
    }

    /// Builds the common prefix (or suffix) sub-list of `ot` and `nt`;
    /// returns the unmatched rests when asked for them.
    fn join_psfx(
        &mut self,
        ot: usize,
        nt: usize,
        want_or: bool,
        want_nr: bool,
        sfx: bool,
    ) -> (Ix, Ix) {
        let (mut o, n) = if sfx {
            (self.nd[ot].suffix, self.nd[nt].suffix)
        } else {
            (self.nd[ot].prefix, self.nd[nt].prefix)
        };
        let mut p: Ix = None;
        if o.is_none() {
            if let Some(ni) = n {
                if self.nd[ni].wlen != 0 {
                    self.nd[ot].flags |= CLF_MISS;
                }
            }
            return (None, if want_nr { n } else { None });
        }
        if n.is_none() {
            if sfx {
                self.nd[ot].suffix = None;
            } else {
                self.nd[ot].prefix = None;
            }
            return (if want_or { o } else { None }, None);
        }
        let mut md = Cmdata {
            cl: n,
            len: 0,
            ..Cmdata::default()
        };
        let mut line = false;
        let mut len: isize;

        while let Some(oc) = o {
            let mut join = false;
            let omd = md.clone();

            if self.nd[oc].flags & CLF_LINE != 0 {
                let (s, n0) = (self.nd[oc].line.clone(), self.nd[oc].llen);
                len = self.sub_match(&mut md, s, n0, sfx);
                if len != n0 && len >= 0 {
                    join = true;
                    line = true;
                }
            } else {
                let (s, n0) = (self.nd[oc].word.clone(), self.nd[oc].wlen);
                len = self.sub_match(&mut md, s, n0, sfx);
                if len != n0 && len >= 0 {
                    if self.nd[oc].line.is_some() {
                        md = omd;
                        self.nd[oc].flags |= CLF_LINE | CLF_DIFF;
                        continue;
                    }
                    self.nd[oc].llen -= self.nd[ot].slen;
                    join = true;
                    line = false;
                }
            }
            let mut cur = oc;
            if join {
                let (sstr, slen) = self.get_s(oc, line);
                let mut jlen: isize = 0;
                let jn = self.nd[oc].flags & CLF_JOIN == 0;
                if let Some(joinl) =
                    self.join_sub(&mut md, sp_add(&sstr, len), slen - len, &mut jlen, sfx, jn)
                {
                    self.nd[joinl].flags |= CLF_DIFF;
                    if len + jlen != slen {
                        let rest = self.get_cline(
                            None,
                            0,
                            sp_add(&sstr, if sfx { 0 } else { len + jlen }),
                            slen - len - jlen,
                            None,
                            0,
                            0,
                        );
                        self.nd[rest].next = self.nd[oc].next;
                        self.nd[joinl].next = Some(rest);
                    } else {
                        self.nd[joinl].next = self.nd[oc].next;
                    }
                    if len != 0 {
                        let ns = if sfx {
                            sp_add(&sstr, slen - len)
                        } else {
                            sstr.clone()
                        };
                        self.set_s(oc, line, ns, len);
                        self.nd[oc].next = Some(joinl);
                    } else {
                        self.nd[oc].next = None;
                        if let Some(pp) = p {
                            self.nd[pp].next = Some(joinl);
                        } else if sfx {
                            self.nd[ot].suffix = Some(joinl);
                        } else {
                            self.nd[ot].prefix = Some(joinl);
                        }
                    }
                    cur = joinl;
                    join = false;
                }
            }
            if join {
                let (sstr, slen) = self.get_s(oc, line);
                let mut orest: Ix = None;
                if len != 0 {
                    if want_or {
                        let fl = self.nd[oc].flags;
                        let r = if line {
                            self.get_cline(sp_add(&sstr, len), slen - len, None, 0, None, 0, fl)
                        } else {
                            self.get_cline(None, 0, sp_add(&sstr, len), slen - len, None, 0, fl)
                        };
                        self.nd[r].next = self.nd[oc].next;
                        orest = Some(r);
                        self.set_s(oc, line, sstr, len);
                        self.nd[oc].next = None;
                    } else {
                        let ns = if sfx { sp_add(&sstr, slen - len) } else { sstr };
                        self.set_s(oc, line, ns, len);
                        self.nd[oc].next = None;
                    }
                } else {
                    if let Some(pp) = p {
                        self.nd[pp].next = None;
                    } else if sfx {
                        self.nd[ot].suffix = None;
                    } else {
                        self.nd[ot].prefix = None;
                    }
                    if want_or {
                        orest = Some(oc);
                    }
                }
                if !want_or || !want_nr {
                    self.nd[ot].flags |= CLF_MISS;
                }
                let nrest = if want_nr {
                    self.undo_cmdata(&md, sfx)
                } else {
                    None
                };
                return (orest, nrest);
            }
            p = Some(cur);
            o = self.nd[cur].next;
        }
        if md.len != 0 || md.cl.is_some() {
            self.nd[ot].flags |= CLF_MISS;
        }
        let nrest = if want_nr {
            self.undo_cmdata(&md, sfx)
        } else {
            None
        };
        (None, nrest)
    }

    fn join_mid(&mut self, o: usize, n: usize) {
        if self.nd[o].flags & CLF_JOIN != 0 {
            let (_, nr) = self.join_psfx(o, n, false, true, false);
            self.nd[n].suffix = self.revert_cline(nr);
            self.join_psfx(o, n, false, false, true);
        } else {
            self.nd[o].flags |= CLF_JOIN;
            let (or, nr) = self.join_psfx(o, n, true, true, false);
            if let Some(r) = or {
                let (os, rw) = (self.nd[o].slen, self.nd[r].wlen);
                self.nd[r].llen = if os > rw { rw } else { os };
            }
            self.nd[o].suffix = self.revert_cline(or);
            self.nd[n].suffix = self.revert_cline(nr);
            self.join_psfx(o, n, false, false, true);
        }
        self.nd[n].suffix = None;
    }

    /// Turns the anchors from `b` up to `e` into a prefix of `e` and joins
    /// that with the prefix of `a`.
    fn sub_join(&mut self, a: usize, b: usize, e: usize, anew: bool) -> isize {
        if self.nd[e].suffix.is_some() || self.nd[a].prefix.is_none() {
            return 0;
        }
        let op = self.nd[e].prefix;
        let mut head: Ix = None;
        let mut p = Link::Head;
        let (mut min, mut max): (isize, isize) = (0, 0);
        let mut b = b;
        while b != e {
            let t = self.nd[b].prefix;
            self.set_link(p, t, &mut head);
            if let Some(mut tt) = t {
                while let Some(nx) = self.nd[tt].next {
                    tt = nx;
                }
                p = Link::Next(tt);
            }
            let next = self.nd[b].next;
            let bn = &mut self.nd[b];
            bn.suffix = None;
            bn.prefix = None;
            bn.flags &= !CLF_SUF;
            min += bn.min;
            max += bn.max;
            self.set_link(p, Some(b), &mut head);
            p = Link::Next(b);
            match next {
                Some(x) => b = x,
                None => break,
            }
        }
        let ep = self.nd[e].prefix;
        self.set_link(p, ep, &mut head);
        let ca = self.nd[a].prefix;

        let mut n = head;
        while let Some(ni) = n {
            self.nd[e].prefix = self.cp_cline(Some(ni), true);
            self.nd[a].prefix = self.cp_cline(ca, true);

            let f = self.nd[e].flags;
            if anew {
                self.join_psfx(e, a, false, false, false);
                self.nd[e].flags = f;
                if self.nd[e].prefix.is_some() {
                    return max - min;
                }
            } else {
                self.join_psfx(a, e, false, false, false);
                self.nd[e].flags = f;
                if self.nd[a].prefix.is_some() {
                    return max - min;
                }
            }
            min -= self.nd[ni].min;

            if Some(ni) == op {
                break;
            }
            n = self.nd[ni].next;
        }
        max - min
    }

    fn po_miss(&mut self, po: Ix, x: Option<usize>, o: usize, diff: isize) {
        let hit = match (po, x) {
            (Some(p), Some(xx)) => {
                self.nd[p].prefix.is_some() && self.cmp_anchors(xx, p, false) != 0
            }
            _ => false,
        };
        let t = if hit { po.unwrap_or(o) } else { o };
        self.nd[t].flags |= CLF_MISS;
        self.nd[t].max += diff;
    }

    /// zsh's join_clines(): simplifies `o` so that it also matches `n`.
    fn join_clines(&mut self, o: Ix, n: Ix) -> Ix {
        self.cline_setlens(n, true);
        if o.is_none() {
            return n;
        }
        let mut oo = o;
        let (mut o, mut n) = (o, n);
        let (mut po, mut pn): (Ix, Ix) = (None, None);
        const SM: u32 = CLF_SUF | CLF_MID;

        while let (Some(oc), Some(nc)) = (o, n) {
            if self.nd[oc].flags & CLF_NEW != 0 && self.nd[nc].flags & CLF_NEW == 0 {
                let mut t = oc;
                let tn = loop {
                    let tn = self.nd[t].next;
                    match tn {
                        Some(x)
                            if self.nd[x].flags & CLF_NEW != 0
                                || self.cmp_anchors(x, nc, false) == 0 =>
                        {
                            t = x
                        }
                        _ => break tn,
                    }
                };
                if let Some(tnv) = tn {
                    let diff = self.sub_join(nc, oc, tnv, true);
                    match po {
                        Some(p) => self.nd[p].next = Some(tnv),
                        None => oo = Some(tnv),
                    }
                    self.nd[t].next = None;
                    o = Some(tnv);
                    self.po_miss(po, Some(oc), tnv, diff);
                    continue;
                }
            }
            if self.nd[oc].flags & CLF_NEW == 0 && self.nd[nc].flags & CLF_NEW != 0 {
                let mut t = nc;
                let tn = loop {
                    let tn = self.nd[t].next;
                    match tn {
                        Some(x)
                            if self.nd[x].flags & CLF_NEW != 0
                                || self.cmp_anchors(oc, x, false) == 0 =>
                        {
                            t = x
                        }
                        _ => break tn,
                    }
                };
                if let Some(tnv) = tn {
                    let of = self.nd[oc].flags & CLF_MISS;
                    let diff = self.sub_join(oc, nc, tnv, false);
                    self.nd[oc].flags = (self.nd[oc].flags & !CLF_MISS) | of;
                    let hit = match (po, pn) {
                        (Some(p), Some(pnv)) => {
                            self.nd[p].prefix.is_some() && self.cmp_anchors(nc, pnv, false) != 0
                        }
                        _ => false,
                    };
                    let tgt = if hit { po.unwrap_or(oc) } else { oc };
                    self.nd[tgt].flags |= CLF_MISS;
                    self.nd[tgt].max += diff;
                    n = Some(tnv);
                    continue;
                }
            }
            if self.nd[oc].flags & SM != self.nd[nc].flags & SM {
                let of = self.nd[oc].flags & SM;
                let mut t = nc;
                let tn = loop {
                    let tn = self.nd[t].next;
                    match tn {
                        Some(x) if self.nd[x].flags & SM != of => t = x,
                        _ => break tn,
                    }
                };
                if let Some(tnv) = tn {
                    if self.cmp_anchors(oc, tnv, true) != 0 {
                        self.sub_join(oc, nc, tnv, false);
                        n = Some(tnv);
                        continue;
                    }
                }
                let nf = self.nd[nc].flags & SM;
                let mut t = oc;
                let tn = loop {
                    let tn = self.nd[t].next;
                    match tn {
                        Some(x) if self.nd[x].flags & SM != nf => t = x,
                        _ => break tn,
                    }
                };
                if let Some(tnv) = tn {
                    if self.cmp_anchors(tnv, nc, true) != 0 {
                        self.sub_join(nc, oc, tnv, true);
                        match po {
                            Some(p) => self.nd[p].next = Some(tnv),
                            None => oo = Some(tnv),
                        }
                        self.nd[t].next = None;
                        o = Some(tnv);
                        continue;
                    }
                }
                if self.nd[oc].flags & CLF_MID != 0 {
                    let nsuf = self.nd[nc].flags & CLF_SUF;
                    let on = &mut self.nd[oc];
                    on.flags = (on.flags & !CLF_MID) | nsuf;
                    if nsuf != 0 {
                        on.prefix = None;
                    } else {
                        on.suffix = None;
                    }
                }
                break;
            }
            if self.nd[oc].flags & CLF_MID == 0 && self.cmp_anchors(oc, nc, true) == 0 {
                let mut to: Ix = None;
                let mut t = nc;
                let tn = loop {
                    let tn = self.nd[t].next;
                    let Some(x) = tn else { break tn };
                    if self.nd[x].flags & CLF_NEW == 0 && self.nd[x].flags & CLF_SKIP != 0 {
                        let mut tt = oc;
                        loop {
                            to = self.nd[tt].next;
                            let Some(y) = to else { break };
                            if self.nd[y].flags & CLF_NEW == 0
                                && self.nd[y].flags & CLF_SKIP != 0
                                && self.cmp_anchors(x, y, true) != 0
                            {
                                break;
                            }
                            tt = y;
                        }
                        if to.is_some() {
                            break tn;
                        }
                    }
                    t = x;
                };
                if let (Some(tnv), Some(tov)) = (tn, to) {
                    match po {
                        Some(p) => self.nd[p].next = Some(tov),
                        None => oo = Some(tov),
                    }
                    let diff = self.sub_join(tov, nc, tnv, false);
                    self.nd[tov].flags |= CLF_MISS;
                    self.nd[tov].max += diff;
                    po = Some(tov);
                    o = self.nd[tov].next;
                    pn = Some(tnv);
                    n = self.nd[tnv].next;
                    continue;
                }
                let mut t = oc;
                let to = loop {
                    let to = self.nd[t].next;
                    let Some(y) = to else { break to };
                    if self.nd[y].flags & CLF_SKIP != 0 && self.cmp_anchors(nc, y, true) != 0 {
                        break to;
                    }
                    t = y;
                };
                if let Some(tov) = to {
                    let diff = self.sub_join(nc, oc, tov, true);
                    match po {
                        Some(p) => self.nd[p].next = Some(tov),
                        None => oo = Some(tov),
                    }
                    o = Some(tov);
                    self.po_miss(po, Some(oc), tov, diff);
                    continue;
                }
                let mut to: Ix = None;
                let mut t = nc;
                while let Some(x) = self.nd[t].next {
                    if self.nd[x].flags & CLF_SKIP != 0 {
                        let mut tt = oc;
                        loop {
                            to = self.nd[tt].next;
                            let Some(y) = to else { break };
                            if self.nd[y].flags & CLF_SKIP != 0 && self.cmp_anchors(x, y, true) != 0
                            {
                                break;
                            }
                            tt = y;
                        }
                    }
                    if to.is_some() {
                        break;
                    }
                    t = x;
                }
                if let Some(tov) = to {
                    let diff = self.sub_join(nc, oc, tov, true);
                    match po {
                        Some(p) => self.nd[p].next = Some(tov),
                        None => oo = Some(tov),
                    }
                    o = Some(tov);
                    self.po_miss(po, Some(oc), tov, diff);
                    continue;
                }
                let onew = self.nd[oc].flags & CLF_NEW;
                let mut tn = n;
                while let Some(x) = tn {
                    if self.nd[x].flags & CLF_NEW == onew && self.cmp_anchors(x, oc, true) != 0 {
                        break;
                    }
                    tn = self.nd[x].next;
                }
                if let Some(tnv) = tn {
                    let of = self.nd[oc].flags & CLF_MISS;
                    let diff = self.sub_join(oc, nc, tnv, false);
                    if diff != 0 {
                        self.nd[oc].flags = (self.nd[oc].flags & !CLF_MISS) | of;
                        let tgt = match po {
                            Some(p) if self.nd[p].prefix.is_some() => p,
                            _ => oc,
                        };
                        self.nd[tgt].flags |= CLF_MISS;
                        self.nd[tgt].max += diff;
                    }
                    po = Some(oc);
                    o = self.nd[oc].next;
                    pn = Some(tnv);
                    n = self.nd[tnv].next;
                    continue;
                }
                if self.nd[oc].flags & CLF_SUF != 0 {
                    break;
                }
                let on = &mut self.nd[oc];
                on.word = None;
                on.line = None;
                on.orig = None;
                on.wlen = 0;
                on.next = None;
                on.flags |= CLF_MISS;
            }
            if self.nd[oc].orig.is_none() && self.nd[oc].olen == 0 {
                self.nd[oc].orig = self.nd[nc].orig.clone();
                self.nd[oc].olen = self.nd[nc].olen;
            }
            if self.nd[nc].min < self.nd[oc].min {
                self.nd[oc].min = self.nd[nc].min;
            }
            if self.nd[nc].max > self.nd[oc].max {
                self.nd[oc].max = self.nd[nc].max;
            }
            if self.nd[oc].flags & CLF_MID != 0 {
                self.join_mid(oc, nc);
            } else {
                let s = self.nd[oc].flags & CLF_SUF != 0;
                self.join_psfx(oc, nc, false, false, s);
            }
            po = Some(oc);
            o = self.nd[oc].next;
            pn = Some(nc);
            n = self.nd[nc].next;
        }
        if o.is_some() {
            match po {
                Some(p) => self.nd[p].next = None,
                None => oo = None,
            }
        }
        oo
    }
}

// ---- public cline functions ---------------------------------------------

/// A new cline (zsh's get_cline()).
pub fn get_cline(
    l: Option<String>,
    ll: i32,
    w: Option<String>,
    wl: i32,
    o: Option<String>,
    ol: i32,
    fl: u32,
) -> Box<ClineNode> {
    Box::new(ClineNode {
        next: None,
        flags: fl,
        line: l,
        llen: ll,
        word: w,
        wlen: wl,
        orig: o,
        olen: ol,
        slen: 0,
        prefix: None,
        suffix: None,
        min: 0,
        max: 0,
    })
}

pub fn free_cline(l: Cline) {
    drop(l);
}

/// A copy of a cline list. zsh's shallow copies share sub-lists; boxed
/// lists cannot, so the copy is always deep.
pub fn cp_cline(l: &Cline) -> Cline {
    l.clone()
}

/// The length of a cline and its sub-lists.
pub fn cline_sublen(l: &ClineNode) -> i32 {
    let mut len = if l.flags & CLF_LINE != 0 {
        l.llen
    } else {
        l.wlen
    };
    let sub = if l.flags & CLF_SUF != 0 {
        &l.suffix
    } else {
        &l.prefix
    };
    if l.olen != 0 && sub.is_none() {
        len += l.olen;
    } else {
        for start in [&l.prefix, &l.suffix] {
            let mut p = start.as_deref();
            while let Some(q) = p {
                len += if q.flags & CLF_LINE != 0 {
                    q.llen
                } else {
                    q.wlen
                };
                p = q.next.as_deref();
            }
        }
    }
    len
}

pub fn cline_setlens(l: &mut Cline, both: bool) {
    let mut cur = l.as_deref_mut();
    while let Some(n) = cur {
        n.min = cline_sublen(n);
        if both {
            n.max = n.min;
        }
        cur = n.next.as_deref_mut();
    }
}

pub fn cline_matched(p: &mut Cline) {
    let mut cur = p.as_deref_mut();
    while let Some(n) = cur {
        n.flags |= CLF_MATCHED;
        cline_matched(&mut n.prefix);
        cline_matched(&mut n.suffix);
        cur = n.next.as_deref_mut();
    }
}

pub fn revert_cline(p: Cline) -> Cline {
    let mut r: Cline = None;
    let mut p = p;
    while let Some(mut n) = p {
        p = n.next.take();
        n.next = r;
        r = Some(n);
    }
    r
}

/// Splits the first `len` characters of `str` into clines at the anchors
/// of `*` match specs; `plen` is how many of them were on the line.
pub fn bld_parts(z: &mut Zle, str: &str, len: i32, plen: i32) -> Cline {
    let mut c = Mctx::new(z);
    let (r, _, _) = c.bld_parts(Some(Sp::new(str)), len as isize, plen as isize);
    c.exp(r)
}

/// zsh's join_clines(): `o` simplified so it also describes `n`.
pub fn join_clines(z: &mut Zle, o: Cline, n: Cline) -> Cline {
    let mut c = Mctx::new(z);
    let oi = c.imp(&o);
    let ni = c.imp(&n);
    let r = c.join_clines(oi, ni);
    c.exp(r)
}

/// zsh's match_str(). The brace infos are `z.comp.brbeg` for a prefix
/// and `z.comp.brend` for a suffix (`sfx` non-zero), starting at index
/// `*bpp` (None for zsh's NULL), which is advanced past those placed.
/// With `test` zero, matchbuf, matchparts and matchsubs are set.
#[allow(clippy::too_many_arguments)]
pub fn match_str(
    z: &mut Zle,
    l: &str,
    w: &str,
    bpp: &mut Option<usize>,
    bc: i32,
    rwlp: Option<&mut i32>,
    sfx: i32,
    test: i32,
    part: i32,
) -> i32 {
    let lv: Vec<char> = l.chars().collect();
    let wv: Vec<char> = w.chars().collect();
    let sfx = sfx != 0;
    let mut c = Mctx::new(z);
    let mut brs = if test != 0 {
        Vec::new()
    } else if sfx {
        std::mem::take(&mut z.comp.brend)
    } else {
        std::mem::take(&mut z.comp.brbeg)
    };
    let mut bp = bpp.unwrap_or(usize::MAX);
    let r = c.match_str(&lv, &wv, &mut brs, &mut bp, bc, rwlp, sfx, test, part != 0);
    if test == 0 {
        *bpp = if bp < brs.len() { Some(bp) } else { None };
        if sfx {
            z.comp.brend = brs;
        } else {
            z.comp.brbeg = brs;
        }
        c.export(z);
    }
    r
}

/// Removes quoting for testing a quoted word against a pattern, standing
/// in for zsh's parse_subst_string() and untokenize(). None if the
/// quotes do not balance.
fn comp_unquote(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            }
            '\'' => loop {
                match it.next() {
                    Some('\'') => break,
                    Some(x) => out.push(x),
                    None => return None,
                }
            },
            '"' => loop {
                match it.next() {
                    Some('"') => break,
                    Some('\\') => match it.peek() {
                        Some(&n) if matches!(n, '$' | '`' | '"' | '\\') => {
                            out.push(n);
                            it.next();
                        }
                        _ => out.push('\\'),
                    },
                    Some(x) => out.push(x),
                    None => return None,
                }
            },
            _ => out.push(c),
        }
    }
    Some(out)
}

/// zsh's comp_match(): does the word `w` match the prefix `pfx` and
/// suffix `sfx` (none if empty) from the line, or the pattern `cp`?
/// Returns the string to use as the match and its cline list in `clp`.
/// `qu` says how to quote `w` (2 for file names); `bpl`/`bsl` index
/// `z.comp.brbeg` and `z.comp.brend` (None for zsh's NULL).
#[allow(clippy::too_many_arguments)]
pub fn comp_match(
    z: &mut Zle,
    pfx: &str,
    sfx: &str,
    w: &str,
    cp: Option<&crate::pattern::Pattern>,
    clp: &mut Cline,
    qu: i32,
    bpl: &mut Option<usize>,
    bcp: i32,
    bsl: &mut Option<usize>,
    bcs: i32,
    exact: &mut i32,
) -> Option<String> {
    use crate::zle::comp::core::{multiquote, tildequote};

    if let Some(cp) = cp {
        let teststr = if qu == 0 {
            comp_unquote(w).unwrap_or_else(|| w.to_string())
        } else {
            w.to_string()
        };
        if !cp.matches(&teststr) {
            return None;
        }
        let r = if qu == 2 {
            tildequote(z, w, false)
        } else {
            multiquote(z, w, qu == 0)
        };
        let wl = w.chars().count() as isize;
        let mut c = Mctx::new(z);
        let (p, _, _) = c.bld_parts(Some(Sp::new(w)), wl, wl);
        *clp = c.exp(p);
        *exact = 0;
        return Some(r);
    }

    let wq = if qu == 2 {
        tildequote(z, w, false)
    } else {
        multiquote(z, w, qu == 0)
    };
    let wv: Vec<char> = wq.chars().collect();
    let wl = wv.len() as isize;
    let pv: Vec<char> = pfx.chars().collect();

    z.comp.useqbr = qu;
    let mut c = Mctx::new(z);
    let mut bb = std::mem::take(&mut z.comp.brbeg);
    let mut be = std::mem::take(&mut z.comp.brend);
    let sfx = Some(sfx).filter(|s| !s.is_empty());
    let (mut bp, mut bs) = (bpl.unwrap_or(usize::MAX), bsl.unwrap_or(usize::MAX));
    let r = comp_match_parts(
        &mut c, &pv, sfx, &wv, wl, &mut bb, &mut bp, bcp, &mut be, &mut bs, bcs,
    );
    *bpl = if bp < bb.len() { Some(bp) } else { None };
    *bsl = if bs < be.len() { Some(bs) } else { None };
    z.comp.brbeg = bb;
    z.comp.brend = be;
    c.export(z);
    let pli = r?;

    let r: String = c.buf.iter().take_while(|&&ch| ch != '\0').collect();
    *clp = c.exp(pli);

    *exact = match sfx {
        Some(s) => {
            let pl = pv.len();
            let sv: Vec<char> = s.chars().collect();
            pl <= wv.len() && strneq(&pv, &wv, pl as isize) && sv[..] == wv[pl..]
        }
        None => pv == wv,
    } as i32;
    Some(r)
}

/// The matching part of comp_match(): the cline list, or None when the
/// word does not match.
#[allow(clippy::too_many_arguments)]
fn comp_match_parts(
    c: &mut Mctx,
    pv: &[char],
    sfx: Option<&str>,
    wv: &[char],
    wl: isize,
    bb: &mut [Brinfo],
    bpl: &mut usize,
    bcp: i32,
    be: &mut [Brinfo],
    bsl: &mut usize,
    bcs: i32,
) -> Option<Ix> {
    let mut rpl = 0i32;
    let mpl = c.match_str(pv, wv, bb, bpl, bcp, Some(&mut rpl), false, 0, false);
    if mpl < 0 {
        return None;
    }
    let (mpl, rpl) = (mpl as isize, rpl as isize);
    let wsp = Sp::from_vec(wv.to_vec());

    if let Some(s) = sfx.filter(|s| !s.is_empty()) {
        let wpfx = c.buf.clone();
        if c.subs.is_some() {
            let tmp = c.get_cline(None, 0, None, 0, None, 0, 0);
            c.nd[tmp].prefix = c.subs;
            match c.lastpart {
                Some(lp) => c.nd[lp].next = Some(tmp),
                None => c.parts = Some(tmp),
            }
        }
        let mut pli = c.parts;
        let plil = c.lastpart;

        let sv: Vec<char> = s.chars().collect();
        let mut rsl = 0i32;
        let msl = c.match_str(
            &sv,
            sl(wv, mpl),
            be,
            bsl,
            bcs,
            Some(&mut rsl),
            true,
            0,
            false,
        );
        if msl < 0 {
            return None;
        }
        let (msl, rsl) = (msl as isize, rsl as isize);
        if c.subs.is_some() {
            let tmp = c.get_cline(None, 0, None, 0, None, 0, CLF_SUF);
            c.nd[tmp].suffix = c.subs;
            match c.lastpart {
                Some(lp) => c.nd[lp].next = Some(tmp),
                None => c.parts = Some(tmp),
            }
        }
        c.add_match_str(None, &[], sl(wv, rpl), wl - rpl - rsl, true);
        c.add_match_str(None, &[], &wpfx, wpfx.len() as isize, true);

        let (mli, mlil, _) = c.bld_parts(
            Some(wsp.add(rpl)),
            wl - rpl - rsl,
            (mpl - rpl) + (msl - rsl),
        );
        if let Some(ml) = mlil {
            c.nd[ml].flags |= CLF_MID;
            c.nd[ml].slen = msl - rsl;
            let parts = c.parts;
            c.nd[ml].next = c.revert_cline(parts);
        }
        match plil {
            Some(pl) => c.nd[pl].next = mli,
            None => pli = mli,
        }
        Some(pli)
    } else {
        c.add_match_str(None, &[], sl(wv, rpl), wl - rpl, false);
        c.add_match_part(
            None,
            None,
            None,
            0,
            None,
            0,
            Some(wsp.add(rpl)),
            wl - rpl,
            mpl - rpl,
            false,
        );
        Some(c.parts)
    }
}
