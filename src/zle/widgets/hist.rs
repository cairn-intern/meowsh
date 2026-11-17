//! Port of zsh's zle_hist.c.
//!
//! History events are numbered from 1 (the oldest line in the shell's
//! history list); the line being edited is event `z.curhist`, one past
//! the newest. Edits made to a history line while moving around are
//! kept (zsh's zle_text) until the next read starts.

use crate::pattern::{Pattern, SubFlags};
use crate::zle::widgets::word::zc_iblank;
use crate::zle::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("up-history", uphistory),
    ("up-line", upline),
    ("up-line-or-history", uplineorhistory),
    ("vi-up-line-or-history", viuplineorhistory),
    ("up-line-or-search", uplineorsearch),
    ("down-line", downline),
    ("down-line-or-history", downlineorhistory),
    ("vi-down-line-or-history", vidownlineorhistory),
    ("down-line-or-search", downlineorsearch),
    ("accept-line-and-down-history", acceptlineanddownhistory),
    ("down-history", downhistory),
    ("history-search-backward", historysearchbackward),
    ("history-search-forward", historysearchforward),
    ("beginning-of-buffer-or-history", beginningofbufferorhistory),
    ("beginning-of-history", beginningofhistory),
    ("end-of-buffer-or-history", endofbufferorhistory),
    ("end-of-history", endofhistory),
    ("insert-last-word", insertlastword),
    ("set-local-history", setlocalhistory),
    ("push-line", pushline),
    ("push-line-or-edit", pushlineoredit),
    ("push-input", pushinput),
    ("get-line", zgetline),
    (
        "history-incremental-search-backward",
        historyincrementalsearchbackward,
    ),
    (
        "history-incremental-search-forward",
        historyincrementalsearchforward,
    ),
    (
        "history-incremental-pattern-search-backward",
        historyincrementalpatternsearchbackward,
    ),
    (
        "history-incremental-pattern-search-forward",
        historyincrementalpatternsearchforward,
    ),
    ("accept-and-infer-next-history", acceptandinfernexthistory),
    ("infer-next-history", infernexthistory),
    ("vi-fetch-history", vifetchhistory),
    ("vi-history-search-forward", vihistorysearchforward),
    ("vi-history-search-backward", vihistorysearchbackward),
    ("vi-repeat-search", virepeatsearch),
    ("vi-rev-repeat-search", virevrepeatsearch),
    (
        "history-beginning-search-backward",
        historybeginningsearchbackward,
    ),
    (
        "history-beginning-search-forward",
        historybeginningsearchforward,
    ),
];

#[derive(Clone, Default)]
struct SearchState {
    histpos: usize,
    srch_hl: usize,
    srch_cs: Option<usize>,
    srch_str: Option<Vec<char>>,
}

#[derive(Clone, Default)]
struct LastInsert {
    lastinsert: Option<Vec<char>>,
    lasthist: i64,
    lastpos: usize,
    lastlen: usize,
}

thread_local! {
    // zle_text: edited text of history events, by event number.
    static EDITS: RefCell<HashMap<usize, Vec<char>>> = RefCell::new(HashMap::new());
    static HAVE_EDITS: Cell<bool> = const { Cell::new(false) };
    // set-local-history's HIST_FOREIGN skip flag. The shell's history
    // has no foreign (shared) entries, so it filters nothing.
    static HIST_SKIP_FOREIGN: Cell<bool> = const { Cell::new(false) };
    static STACKHIST: Cell<Option<usize>> = const { Cell::new(None) };
    static STACKCS: Cell<Option<usize>> = const { Cell::new(None) };
    static SRCH: RefCell<SearchState> = RefCell::new(SearchState::default());
    static LASTINS: RefCell<LastInsert> = RefCell::new(LastInsert::default());
    static VIPENULTSRCHSTR: RefCell<Option<String>> = const { RefCell::new(None) };
}

const PATH_MAX: usize = 4096;

/// The cursor position push-line saved for the next read (zsh's
/// stackcs); zleread applies it after loading the pushed line.
pub fn take_stackcs() -> Option<usize> {
    STACKCS.with(|s| s.take())
}

/// Sets the cursor position for the line pushed for the next read.
pub fn set_stackcs(cs: Option<usize>) {
    STACKCS.with(|s| s.set(cs));
}

// ---- history access ------------------------------------------------------

fn hist_len() -> usize {
    crate::shell::with_shell(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).len())
}

fn hist_nam(z: &Zle, ev: usize) -> String {
    if ev == z.curhist {
        return String::new();
    }
    crate::shell::with_shell(|s| {
        s.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(ev.wrapping_sub(1))
            .cloned()
            .unwrap_or_default()
    })
}

fn edited(ev: usize) -> Option<Vec<char>> {
    EDITS.with(|e| e.borrow().get(&ev).cloned())
}

/// zsh's quietgethist: whether event `ev` exists.
fn quietgethist(z: &Zle, ev: i64) -> bool {
    (1..=z.curhist as i64).contains(&ev) && (ev == z.curhist as i64 || ev as usize <= hist_len())
}

/// The text of event `ev`: its edited text if any, else the history line
/// (zsh's GETZLETEXT).
fn getzletext(z: &Zle, ev: usize) -> Vec<char> {
    edited(ev).unwrap_or_else(|| hist_nam(z, ev).chars().collect())
}

// A snapshot of the history list for walking through it.
struct Hist {
    lines: Vec<String>,
    curhist: usize,
    dups: Option<Vec<bool>>,
}

impl Hist {
    fn load(z: &Zle) -> Hist {
        let lines = crate::shell::with_shell(|s| {
            s.history.lock().unwrap_or_else(|e| e.into_inner()).clone()
        });
        Hist {
            lines,
            curhist: z.curhist,
            dups: None,
        }
    }

    fn exists(&self, ev: i64) -> bool {
        (1..=self.curhist as i64).contains(&ev)
            && (ev == self.curhist as i64 || ev as usize <= self.lines.len())
    }

    fn firsthist(&self) -> usize {
        if self.lines.is_empty() {
            self.curhist
        } else {
            1
        }
    }

    // The history line (zsh's node.nam); the line being edited has none.
    fn nam(&self, ev: usize) -> String {
        if ev == self.curhist {
            return String::new();
        }
        self.lines
            .get(ev.wrapping_sub(1))
            .cloned()
            .unwrap_or_default()
    }

    fn zt(&self, ev: usize) -> Vec<char> {
        edited(ev).unwrap_or_else(|| self.nam(ev).chars().collect())
    }

    fn movehistent(&self, ev: usize, n: i64) -> Option<usize> {
        let t = ev as i64 + n;
        if self.exists(t) {
            Some(t as usize)
        } else {
            None
        }
    }

    // zsh's HIST_DUP: a newer event has the same text.
    fn dup(&mut self, ev: usize) -> bool {
        if self.dups.is_none() {
            let mut seen = std::collections::HashSet::new();
            let mut d = vec![false; self.lines.len()];
            for (i, l) in self.lines.iter().enumerate().rev() {
                d[i] = !seen.insert(l.as_str());
            }
            self.dups = Some(d);
        }
        self.dups
            .as_ref()
            .and_then(|d| d.get(ev.wrapping_sub(1)).copied())
            .unwrap_or(false)
    }

    // zsh's gethistent with a near match in direction `near`.
    fn gethistent(&self, ev: i64, near: i64) -> Option<usize> {
        if self.exists(ev) {
            return Some(ev as usize);
        }
        let first = self.firsthist() as i64;
        let last = self.curhist as i64;
        if near > 0 && ev < first {
            Some(first as usize)
        } else if near < 0 && ev > last {
            Some(last as usize)
        } else {
            None
        }
    }

    fn addhistnum(&self, hl: i64, mut n: i64) -> i64 {
        let dir = n.signum();
        let Some(he) = self.gethistent(hl, dir) else {
            return 0;
        };
        if he as i64 != hl {
            n -= dir;
        }
        let he = if n != 0 {
            self.movehistent(he, n)
        } else {
            Some(he)
        };
        match he {
            Some(e) => e as i64,
            None if dir < 0 => self.firsthist() as i64 - 1,
            None => self.curhist as i64 + 1,
        }
    }
}

/// Sets up history state at the start of a read (curhist, histline).
pub fn init_hist(z: &mut Zle) {
    forget_edits();
    z.curhist = hist_len() + 1;
    z.histline = z.curhist;
    if utils::bufstack_len() > 0 {
        if let Some(sh) = STACKHIST.with(|s| s.take()) {
            z.histline = sh;
        }
    }
}

pub fn remember_edits(z: &mut Zle) {
    if quietgethist(z, z.histline as i64) {
        let line = z.line.clone();
        EDITS.with(|e| {
            let mut e = e.borrow_mut();
            if e.get(&z.histline) != Some(&line) {
                e.insert(z.histline, line);
                HAVE_EDITS.with(|h| h.set(true));
            }
        });
    }
}

pub fn forget_edits() {
    if !HAVE_EDITS.with(|h| h.replace(false)) {
        return;
    }
    EDITS.with(|e| e.borrow_mut().clear());
}

// ---- search utilities -----------------------------------------------------

fn lower(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// 0 if the strings are the same, -1 if `input` is a prefix of `hist`, 1
/// if `input` is the lower case version of `hist`, 2 if it is a lower
/// case prefix, 3 otherwise.
fn zlinecmp(hist: &[char], input: &[char]) -> i32 {
    let mut i = 0;
    while i < input.len() && i < hist.len() && hist[i] == input[i] {
        i += 1;
    }
    if i == input.len() {
        return if i == hist.len() { 0 } else { -1 };
    }
    let mut j = 0;
    while j < hist.len() && j < input.len() {
        if lower(hist[j]) != input[j] {
            return 3;
        }
        j += 1;
    }
    if j == input.len() {
        if j == hist.len() {
            1
        } else {
            2
        }
    } else {
        3
    }
}

/// Searches `haystack` for `needle` from `pos`, forward if `dir` > 0.
fn zlinefind(haystack: &[char], pos: usize, needle: &[char], dir: i32, sens: i32) -> Option<usize> {
    if dir > 0 {
        let mut s = pos;
        while s < haystack.len() {
            if zlinecmp(&haystack[s..], needle) < sens {
                return Some(s);
            }
            s += 1;
        }
    } else {
        let mut s = pos.min(haystack.len());
        loop {
            if zlinecmp(&haystack[s..], needle) < sens {
                return Some(s);
            }
            if s == 0 {
                break;
            }
            s -= 1;
        }
    }
    None
}

fn histbeep() -> bool {
    crate::options::is_set("histbeep")
}

fn histfindnodups() -> bool {
    crate::options::is_set("histfindnodups")
}

fn noargs() -> Vec<String> {
    Vec::new()
}

// ---- widgets ----------------------------------------------------------

pub fn uphistory(z: &mut Zle, _args: &[String]) -> i32 {
    let nodups = crate::options::is_set("histignoredups");
    let n = -z.zmod.mult;
    if !zle_goto_hist_dir(z, z.histline as i64, n, nodups) && histbeep() {
        return 1;
    }
    0
}

// Moves to the line `lastcol` columns in, as upline and downline end.
fn gotocol(z: &mut Zle) {
    let x = z.findeol();
    z.cs += z.lastcol.max(0) as usize;
    if z.cs >= x {
        z.cs = x;
        if z.cs > z.findbol() && z.invicmdmode() {
            z.cs -= 1;
        }
    }
}

pub fn upline(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -z.zmod.mult;
        let r = -downline(z, args);
        z.zmod.mult = -z.zmod.mult;
        return r;
    }
    if z.lastcol == -1 {
        z.lastcol = (z.cs - z.findbol()) as i32;
    }
    z.cs = z.findbol();
    while n > 0 {
        if z.cs == 0 {
            break;
        }
        z.cs -= 1;
        z.cs = z.findbol();
        n -= 1;
    }
    if n == 0 {
        gotocol(z);
    }
    n as i32
}

pub fn uplineorhistory(z: &mut Zle, args: &[String]) -> i32 {
    let ocs = z.cs;
    let n = upline(z, args);
    if n != 0 {
        let m = z.zmod.mult;
        z.cs = ocs;
        if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
            return 1;
        }
        z.zmod.mult = n as i64;
        let ret = uphistory(z, args);
        z.zmod.mult = m;
        return ret;
    }
    0
}

pub fn viuplineorhistory(z: &mut Zle, args: &[String]) -> i32 {
    let col = z.lastcol;
    uplineorhistory(z, args);
    z.lastcol = col;
    crate::zle::widgets::move_::vifirstnonblank(z, args)
}

pub fn uplineorsearch(z: &mut Zle, args: &[String]) -> i32 {
    let ocs = z.cs;
    let n = upline(z, args);
    if n != 0 {
        let m = z.zmod.mult;
        z.cs = ocs;
        if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
            return 1;
        }
        z.zmod.mult = n as i64;
        let ret = historysearchbackward(z, args);
        z.zmod.mult = m;
        return ret;
    }
    0
}

pub fn downline(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -z.zmod.mult;
        let r = -upline(z, args);
        z.zmod.mult = -z.zmod.mult;
        return r;
    }
    if z.lastcol == -1 {
        z.lastcol = (z.cs - z.findbol()) as i32;
    }
    while n > 0 {
        let x = z.findeol();
        if x == z.ll() {
            break;
        }
        z.cs = x + 1;
        n -= 1;
    }
    if n == 0 {
        gotocol(z);
    }
    n as i32
}

pub fn downlineorhistory(z: &mut Zle, args: &[String]) -> i32 {
    let ocs = z.cs;
    let n = downline(z, args);
    if n != 0 {
        let m = z.zmod.mult;
        z.cs = ocs;
        if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
            return 1;
        }
        z.zmod.mult = n as i64;
        let ret = downhistory(z, args);
        z.zmod.mult = m;
        return ret;
    }
    0
}

pub fn vidownlineorhistory(z: &mut Zle, args: &[String]) -> i32 {
    let col = z.lastcol;
    downlineorhistory(z, args);
    z.lastcol = col;
    crate::zle::widgets::move_::vifirstnonblank(z, &noargs())
}

pub fn downlineorsearch(z: &mut Zle, args: &[String]) -> i32 {
    let ocs = z.cs;
    let n = downline(z, args);
    if n != 0 {
        let m = z.zmod.mult;
        z.cs = ocs;
        if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
            return 1;
        }
        z.zmod.mult = n as i64;
        let ret = historysearchforward(z, args);
        z.zmod.mult = m;
        return ret;
    }
    0
}

pub fn acceptlineanddownhistory(z: &mut Zle, _args: &[String]) -> i32 {
    let h = Hist::load(z);
    if h.exists(z.histline as i64) {
        if let Some(e) = h.movehistent(z.histline, 1) {
            utils::push_bufstack(h.nam(e));
            STACKHIST.with(|s| s.set(Some(e)));
        }
    }
    z.done = true;
    0
}

pub fn downhistory(z: &mut Zle, _args: &[String]) -> i32 {
    let nodups = crate::options::is_set("histignoredups");
    let n = z.zmod.mult;
    if !zle_goto_hist_dir(z, z.histline as i64, n, nodups) && histbeep() {
        return 1;
    }
    0
}

// The search string of history-search-backward and -forward: the first
// word of the line, kept while the search repeats.
fn srch_string(z: &Zle, args: &[String]) -> Vec<char> {
    if let Some(a) = args.first() {
        return a.chars().collect();
    }
    SRCH.with(|st| {
        let mut st = st.borrow_mut();
        let same = match &st.srch_str {
            Some(s) => z.line.len() >= st.histpos && z.line[..st.histpos] == s[..],
            None => false,
        };
        if z.histline == z.curhist
            || z.histline != st.srch_hl
            || Some(z.cs) != st.srch_cs
            || z.mark != 0
            || !same
        {
            let ll = z.ll();
            let mut hp = 0;
            while hp < ll && !zc_iblank(z.line[hp]) {
                hp += 1;
            }
            if hp < ll {
                hp += 1;
            }
            st.histpos = hp;
            st.srch_str = Some(z.line[..hp].to_vec());
        }
        st.srch_str.clone().unwrap_or_default()
    })
}

fn srch_done(z: &Zle) {
    SRCH.with(|st| {
        let mut st = st.borrow_mut();
        st.srch_hl = z.histline;
        st.srch_cs = Some(z.cs);
    });
}

pub fn historysearchbackward(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = historysearchforward(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let s = srch_string(z, args);
    let mut h = Hist::load(z);
    if !h.exists(z.histline as i64) {
        return 1;
    }
    let nodups = histfindnodups();
    let line = z.line.clone();
    let mut ev = z.histline;
    while let Some(e) = h.movehistent(ev, -1) {
        ev = e;
        if nodups && h.dup(e) {
            continue;
        }
        let zt = h.zt(e);
        if zlinecmp(&zt, &s) < 0 && (!args.is_empty() || zt != line) {
            n -= 1;
            if n <= 0 {
                zle_setline(z, e);
                srch_done(z);
                return 0;
            }
        }
    }
    1
}

pub fn historysearchforward(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = historysearchbackward(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let s = srch_string(z, args);
    let mut h = Hist::load(z);
    if !h.exists(z.histline as i64) {
        return 1;
    }
    let nodups = histfindnodups();
    let line = z.line.clone();
    let mut ev = z.histline;
    while let Some(e) = h.movehistent(ev, 1) {
        ev = e;
        if nodups && h.dup(e) {
            continue;
        }
        let zt = h.zt(e);
        if zlinecmp(&zt, &s) < i32::from(e == z.curhist) && (!args.is_empty() || zt != line) {
            n -= 1;
            if n <= 0 {
                zle_setline(z, e);
                srch_done(z);
                return 0;
            }
        }
    }
    1
}

pub fn beginningofbufferorhistory(z: &mut Zle, args: &[String]) -> i32 {
    if z.findbol() != 0 {
        z.cs = 0;
    } else {
        return beginningofhistory(z, args);
    }
    0
}

pub fn beginningofhistory(z: &mut Zle, _args: &[String]) -> i32 {
    let first = Hist::load(z).firsthist();
    if !zle_goto_hist_dir(z, first as i64, 0, false) && histbeep() {
        return 1;
    }
    0
}

pub fn endofbufferorhistory(z: &mut Zle, args: &[String]) -> i32 {
    if z.findeol() != z.ll() {
        z.cs = z.ll();
    } else {
        return endofhistory(z, args);
    }
    0
}

pub fn endofhistory(z: &mut Zle, _args: &[String]) -> i32 {
    zle_goto_hist_dir(z, z.curhist as i64, 0, false);
    0
}

// zstrtol in base 10: the leading integer of `s`.
fn zstrtol(s: &str) -> i64 {
    let s = s.trim_start();
    let (neg, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let v: i64 = digits.parse().unwrap_or(0);
    if neg {
        -v
    } else {
        v
    }
}

pub fn insertlastword(z: &mut Zle, args: &[String]) -> i32 {
    let mut st = LASTINS.with(|l| l.borrow().clone());
    let ret = insertlastword_st(z, args, &mut st);
    LASTINS.with(|l| *l.borrow_mut() = st);
    ret
}

fn insertlastword_st(z: &mut Zle, args: &[String], st: &mut LastInsert) -> i32 {
    let mut histstep: i64 = -1;
    let mut wordpos: i64 = 0;
    let mut deleteword = false;
    let curhist = z.curhist as i64;
    // With three arguments, the search starts from the current line and
    // does not skip lines without words.
    let three = args.len() >= 3;
    if let Some(a) = args.first() {
        histstep = zstrtol(a);
        if let Some(b) = args.get(1) {
            wordpos = zstrtol(b);
            if three {
                st.lasthist = curhist;
            }
        }
    }
    z.fixsuffix();
    let cs = z.cs;
    let same = match &st.lastinsert {
        Some(li) => {
            st.lastlen > 0
                && st.lastpos <= cs
                && st.lastlen == cs - st.lastpos
                && z.line[st.lastpos..cs] == li[..]
        }
        None => false,
    };
    if same {
        deleteword = true;
    } else {
        st.lasthist = curhist;
    }
    let h = Hist::load(z);
    let mut evhist = if histstep != 0 {
        h.addhistnum(st.lasthist, histstep)
    } else {
        st.lasthist
    };
    let words: Vec<String>;
    if evhist == curhist {
        if deleteword {
            let pos = z.cs;
            z.cs = st.lastpos;
            z.foredel(pos - z.cs, CUT_RAW);
            deleteword = false;
        }
        let line: String = z.line.iter().collect();
        words = crate::expand::split_shell_words(&line);
    } else {
        let mut found: Option<Vec<String>> = None;
        while h.exists(evhist) {
            let w = crate::expand::split_shell_words(&h.nam(evhist as usize));
            if histstep != -1 || three || !w.is_empty() {
                found = Some(w);
                break;
            }
            evhist = h.addhistnum(evhist, histstep);
        }
        match found {
            Some(w) if !w.is_empty() => words = w,
            _ => return 1,
        }
    }
    let nwords = words.len() as i64;
    let mult = z.zmod.mult;
    let n = if wordpos != 0 {
        if wordpos > 0 {
            wordpos
        } else {
            nwords + wordpos + 1
        }
    } else if mult > 0 {
        nwords - (mult - 1)
    } else {
        1 - mult
    };
    if n < 1 || n > nwords {
        st.lasthist = evhist;
        return 1;
    }
    if deleteword {
        let pos = z.cs;
        z.cs = st.lastpos;
        z.foredel(pos - z.cs, CUT_RAW);
    }
    let s: Vec<char> = words[(n - 1) as usize].chars().collect();
    st.lasthist = evhist;
    st.lastpos = z.cs;
    st.lastlen = s.len();
    st.lastinsert = Some(s.clone());
    z.zmod.mult = 1;
    crate::zle::widgets::misc::doinsert(z, &s);
    z.zmod.mult = mult;
    0
}

/// Shows history event `ev` in the buffer, keeping edits to the line
/// left.
pub fn zle_setline(z: &mut Zle, ev: usize) {
    remember_edits(z);
    z.mkundoent();
    z.histline = ev;
    let text: String = getzletext(z, ev).iter().collect();
    z.setline(&text, ZSL_COPY | ZSL_TOEND);
    z.callhook("zle-history-line-set", None);
    z.setlastline();
    z.clearlist = true;
}

pub fn setlocalhistory(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.flags & MOD_MULT != 0 {
        let on = z.zmod.mult != 0;
        HIST_SKIP_FOREIGN.with(|f| f.set(on));
    } else {
        HIST_SKIP_FOREIGN.with(|f| f.set(!f.get()));
    }
    0
}

/// Puts history line `n` in the buffer; false if there is none.
pub fn zle_goto_hist(z: &mut Zle, n: usize) -> bool {
    if !quietgethist(z, n as i64) {
        return false;
    }
    zle_setline(z, n);
    true
}

/// zsh's three-argument zle_goto_hist: moves `n` events from `ev`,
/// skipping lines the same as the buffer with `skipdups`. Like zsh, true
/// when there is no such event; false when only duplicates are left.
pub fn zle_goto_hist_dir(z: &mut Zle, ev: i64, n: i64, skipdups: bool) -> bool {
    let h = Hist::load(z);
    if !h.exists(ev) {
        return true;
    }
    let line = z.line.clone();
    let Some(mut he) = h.movehistent(ev as usize, n) else {
        return true;
    };
    if skipdups && n != 0 {
        let d = n.signum();
        loop {
            if zlinecmp(&h.zt(he), &line) != 0 {
                break;
            }
            match h.movehistent(he, d) {
                Some(e) => he = e,
                None => return false,
            }
        }
    }
    zle_setline(z, he);
    true
}

pub fn pushline(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return 1;
    }
    utils::push_bufstack(z.line.iter().collect());
    n -= 1;
    while n > 0 {
        utils::push_bufstack(String::new());
        n -= 1;
    }
    if z.invicmdmode() {
        z.cs += 1;
    }
    set_stackcs(Some(z.cs));
    z.line.clear();
    z.cs = 0;
    z.clearlist = true;
    0
}

pub fn pushlineoredit(z: &mut Zle, args: &[String]) -> i32 {
    // hgetline: the lines of the command read so far, which it resets.
    let hline = crate::zle::prebuffer();
    crate::zle::set_prebuffer("");
    if z.zmod.mult < 0 {
        return 1;
    }
    if !hline.is_empty() {
        let mut zh: Vec<char> = hline.chars().collect();
        let ics = zh.len();
        zh.extend_from_slice(&z.line);
        z.line = zh;
        z.cs += ics;
    }
    let ret = pushline(z, args);
    if !z.isfirstln {
        z.errflag = true;
        z.done = true;
    }
    z.clearlist = true;
    ret
}

pub fn pushinput(z: &mut Zle, args: &[String]) -> i32 {
    if z.zmod.mult < 0 {
        return 1;
    }
    let i = i64::from(!z.isfirstln);
    z.zmod.mult += i;
    let ret = pushlineoredit(z, args);
    z.zmod.mult -= i;
    ret
}

pub fn zgetline(z: &mut Zle, _args: &[String]) -> i32 {
    match utils::take_bufstack() {
        None => 1,
        Some(s) => {
            let lineadd: Vec<char> = s.chars().collect();
            z.insert_chars(&lineadd);
            z.clearlist = true;
            // Not restoring stackhist: the text goes into the current line.
            STACKHIST.with(|s| s.set(None));
            0
        }
    }
}

pub fn historyincrementalsearchbackward(z: &mut Zle, args: &[String]) -> i32 {
    doisearch(z, args, -1, false)
}

pub fn historyincrementalsearchforward(z: &mut Zle, args: &[String]) -> i32 {
    doisearch(z, args, 1, false)
}

pub fn historyincrementalpatternsearchbackward(z: &mut Zle, args: &[String]) -> i32 {
    doisearch(z, args, -1, true)
}

pub fn historyincrementalpatternsearchforward(z: &mut Zle, args: &[String]) -> i32 {
    doisearch(z, args, 1, true)
}

#[derive(Clone, Copy, Default)]
struct IsrchSpot {
    hl: usize,
    pos: usize,
    pat_hl: usize,
    pat_pos: usize,
    end_pos: usize,
    cs: usize,
    len: usize,
    dir: i32,
    nomatch: i32,
}

fn set_isrch_spot(spots: &mut Vec<IsrchSpot>, num: usize, spot: IsrchSpot) {
    if num >= spots.len() {
        spots.resize(num + 1, IsrchSpot::default());
    }
    spots[num] = spot;
}

// The match in `matchlist` at or before (backward) or after (forward)
// `curpos`, as (start, end).
fn isearch_newpos(matchlist: &[(usize, usize)], curpos: i64, dir: i32) -> Option<(usize, usize)> {
    if dir < 0 {
        matchlist
            .iter()
            .rev()
            .find(|m| m.0 as i64 <= curpos)
            .copied()
    } else {
        matchlist.iter().find(|m| m.0 as i64 >= curpos).copied()
    }
}

// The text a pattern is matched against. With `lc` (PAT_LCMATCHUC) lower
// case in the pattern also matches upper case; done by folding the line
// when the pattern has no upper case of its own.
fn pat_subject(zt: &[char], lc: bool) -> String {
    if lc {
        zt.iter().map(|&c| lower(c)).collect()
    } else {
        zt.iter().collect()
    }
}

fn parse_nums(s: &str) -> Vec<usize> {
    s.split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect()
}

// A match of `p` anchored at the start of the line: its end.
fn pat_prefix(p: &Pattern, zt: &[char], lc: bool) -> Option<usize> {
    let s = pat_subject(zt, lc);
    let fl = SubFlags {
        long: true,
        eind: true,
        ..Default::default()
    };
    let v = parse_nums(&p.getmatch(&s, fl, 1, None));
    let e = *v.first()?;
    if e == 1 && !p.matches_chars(&[]) {
        None
    } else {
        Some(e.saturating_sub(1))
    }
}

// All non-overlapping longest matches of `p` in the line (zsh's
// getmatchlist), as (start, end) pairs.
fn pat_matchlist(p: &Pattern, zt: &[char], lc: bool) -> Vec<(usize, usize)> {
    let s = pat_subject(zt, lc);
    let fl = SubFlags {
        substr: true,
        long: true,
        bind: true,
        eind: true,
        ..Default::default()
    };
    let empty_ok = p.matches_chars(&[]);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut next_t = 0;
    let mut last_b: Option<usize> = None;
    let mut k = 1;
    loop {
        let v = parse_nums(&p.getmatch(&s, fl, k, None));
        if v.len() != 2 || v[0] == 0 || v[1] == 0 {
            break;
        }
        let (b, e) = (v[0] - 1, v[1] - 1);
        if last_b.is_some_and(|lb| b <= lb) || (k == 1 && b == 0 && e == 0 && !empty_ok) {
            break;
        }
        last_b = Some(b);
        if b >= next_t && (e > b || out.last().is_none_or(|l| l.1 != b)) {
            out.push((b, e));
            next_t = if e > b { e } else { b + 1 };
        }
        k += 1;
    }
    out
}

fn put(buf: &mut Vec<char>, at: usize, c: char) {
    if at < buf.len() {
        buf[at] = c;
    } else {
        buf.push(c);
    }
}

// The character last typed (zsh's LASTFULLCHAR).
fn lastfullchar(z: &Zle) -> char {
    if z.lastchar_wide_valid {
        z.lastchar_wide.unwrap_or('?')
    } else {
        char::from(z.lastchar as u8)
    }
}

const FAILING_TEXT: &str = "failing";
const INVALID_TEXT: &str = "invalid";

// The isearch status line: `[failing ]bck-i-search: text_`.
fn isearch_status(
    bad: Option<bool>,
    badtxt: &str,
    dirtxt: &str,
    sbuf: &[char],
    tail: char,
) -> Option<String> {
    bad.map(|bad| {
        let mut s = String::new();
        if bad {
            s.push_str(badtxt);
            s.push(' ');
        }
        s.push_str(dirtxt);
        s.push_str("-i-search: ");
        s.extend(sbuf);
        s.push(tail);
        s
    })
}

fn doisearch(z: &mut Zle, args: &[String], mut dir: i32, pattern: bool) -> i32 {
    let mut h = Hist::load(z);
    let mut hl = z.histline;
    if !h.exists(hl as i64) {
        return 1;
    }
    z.selectlocalmap(keymap::open_keymap("isearch"));
    z.clearlist = true;
    let mut savekeys: Option<usize> = None;
    if let Some(a) = args.first() {
        savekeys = Some(z.unget.len());
        let k = keymap::getkeystring(a);
        z.ungetbytes(&k);
    }
    // The status line: None until set, Some(true) with the failing or
    // invalid text in front.
    let mut stline: Option<bool> = None;
    let mut badtxt = FAILING_TEXT;
    let mut dirtxt = if dir == 1 { "fwd" } else { "bck" };
    let okeymap = z.curkeymapname.clone();
    z.selectkeymap("main", true);
    remember_edits(z);
    let mut zt = h.zt(hl);
    let mut sbuf: Vec<char> = Vec::new();
    let mut sbptr: usize = 0;
    let mut spots: Vec<IsrchSpot> = Vec::new();
    let mut top_spot: usize = 0;
    let mut nomatch: i32 = 0;
    let mut skip_line = false;
    let mut skip_pos = false;
    let odir = dir;
    let sens = if z.zmod.mult == 1 { 3 } else { 1 };
    let mut pos = z.cs;
    let mut pat_hl = hl;
    let mut pat_pos = pos;
    let mut revert_patpos = false;
    let mut dup_ok = false;
    let mut end_pos: usize = 0;
    let mut feep = false;
    let mut nosearch = false;
    let mut patprog: Option<Pattern> = None;
    let mut patlc = false;
    let mut matchlist: Option<Vec<(usize, usize)>> = None;
    let mut exitfn: Option<WidgetFn> = None;
    let mut aborted = false;
    let nodups = histfindnodups();

    macro_rules! cur_spot {
        () => {
            IsrchSpot {
                hl,
                pos,
                pat_hl,
                pat_pos,
                end_pos,
                cs: z.cs,
                len: sbptr,
                dir,
                nomatch,
            }
        };
    }

    'search: loop {
        set_isrch_spot(&mut spots, top_spot, cur_spot!());
        if sbptr == 1 && sbuf[0] == '^' {
            z.cs = 0;
            nomatch = 0;
            stline = Some(false);
        } else if sbptr > 0 {
            let mut t: Option<usize> = None;
            let last_line = zt.clone();
            let anchored = sbuf[0] == '^';
            if pattern && patprog.is_none() && !nosearch {
                let patstring: String = if anchored {
                    sbuf[1..sbptr].iter().collect()
                } else {
                    sbuf[..sbptr].iter().collect()
                };
                patlc = sens == 3 && !patstring.chars().any(|c| c.is_uppercase());
                patprog = Some(Pattern::new(&patstring));
                matchlist = None;
                revert_patpos = true;
                skip_pos = false;
            }
            if !nosearch {
                loop {
                    if let Some(p) = &patprog {
                        if revert_patpos {
                            // Search from where the previous pattern search
                            // started: a longer pattern may match earlier.
                            revert_patpos = false;
                            dup_ok = true;
                            hl = pat_hl;
                            zt = h.zt(hl);
                            pos = pat_pos;
                        }
                        if !skip_line {
                            if anchored {
                                if !skip_pos {
                                    if let Some(e) = pat_prefix(p, &zt, patlc) {
                                        end_pos = e;
                                        t = Some(0);
                                    }
                                }
                            } else {
                                if matchlist.is_none() && !skip_pos {
                                    let l = pat_matchlist(p, &zt, patlc);
                                    if !l.is_empty() {
                                        matchlist = Some(l);
                                    }
                                }
                                if let Some(ml) = &matchlist {
                                    let newpos = if !skip_pos {
                                        pos as i64
                                    } else if dir < 0 {
                                        pos as i64 - 1
                                    } else {
                                        pos as i64 + 1
                                    };
                                    match isearch_newpos(ml, newpos, dir) {
                                        None => matchlist = None,
                                        Some((b, e)) => {
                                            end_pos = e;
                                            t = Some(b);
                                        }
                                    }
                                }
                            }
                        }
                        skip_pos = false;
                    } else {
                        if skip_pos {
                            if dir < 0 {
                                if pos == 0 {
                                    skip_line = true;
                                } else {
                                    pos -= 1;
                                }
                            } else if !anchored {
                                if pos as i64 >= zt.len() as i64 - 1 {
                                    skip_line = true;
                                } else {
                                    pos += 1;
                                }
                            } else {
                                skip_line = true;
                            }
                            skip_pos = false;
                        }
                        if !skip_line {
                            if anchored {
                                if zlinecmp(&zt, &sbuf[1..sbptr]) < sens {
                                    t = Some(0);
                                }
                            } else {
                                t = zlinefind(&zt, pos, &sbuf[..sbptr], dir, sens);
                            }
                            if let Some(tt) = t {
                                end_pos = tt + sbptr - usize::from(anchored);
                            }
                        }
                    }
                    if let Some(tt) = t {
                        pos = tt;
                        break;
                    }
                    // Not found in that line: move through the history.
                    let next = if z.zlereadflags & ZLRF_HISTORY == 0 {
                        None
                    } else {
                        h.movehistent(hl, dir as i64)
                    };
                    match next {
                        None => {
                            if top_spot > 0
                                && sbptr == spots[top_spot - 1].len
                                && spots[top_spot - 1].nomatch != 0
                            {
                                top_spot -= 1;
                            }
                            let s = spots[top_spot];
                            hl = s.hl;
                            pos = s.pos;
                            pat_hl = s.pat_hl;
                            pat_pos = s.pat_pos;
                            end_pos = s.end_pos;
                            z.cs = s.cs;
                            sbptr = s.len;
                            dir = s.dir;
                            nomatch = s.nomatch;
                            if nomatch != 1 {
                                feep = true;
                                nomatch = 1;
                            }
                            zt = h.zt(hl);
                            skip_line = false;
                            badtxt = if nomatch == 2 {
                                INVALID_TEXT
                            } else {
                                FAILING_TEXT
                            };
                            stline = Some(true);
                            break;
                        }
                        Some(e) => {
                            hl = e;
                            zt = h.zt(e);
                            pos = if dir == 1 { 0 } else { zt.len() };
                            skip_line = if dup_ok {
                                false
                            } else if nodups {
                                h.dup(e)
                            } else {
                                zt == last_line
                            };
                        }
                    }
                }
            }
            dup_ok = false;
            if t.is_some() || (nosearch && nomatch == 0) {
                zle_setline(z, hl);
                z.cs = if dir == 1 { end_pos } else { pos };
                stline = Some(false);
                nomatch = 0;
            }
        } else {
            top_spot = 0;
            nomatch = 0;
            stline = Some(false);
        }
        nosearch = false;
        if feep {
            utils::handlefeep(z, &[]);
            feep = false;
        }
        if nomatch == 0 && sbptr > 0 && (sbptr > 1 || sbuf[0] != '^') {
            z.isearch = (Some(pos), Some(end_pos), true);
        } else {
            z.isearch.2 = false;
        }
        let cmd = loop {
            z.statusline = isearch_status(stline, badtxt, dirtxt, &sbuf[..sbptr], '_');
            z.callhook("zle-isearch-update", None);
            z.redrawhook();
            z.statusline = isearch_status(stline, badtxt, dirtxt, &sbuf[..sbptr], '_');
            z.zrefresh();
            let cmd = match z.getkeycmd() {
                Some(c) if c != "send-break" => c,
                _ => {
                    aborted = true;
                    z.lastabortedsearch = Some(sbuf[..sbptr].iter().collect());
                    let s = spots[0];
                    hl = s.hl;
                    sbptr = s.len;
                    nomatch = s.nomatch;
                    zle_setline(z, hl);
                    z.cs = s.cs;
                    break 'search;
                }
            };
            match cmd.as_str() {
                "clear-screen" => z.clearscreen(),
                "redisplay" => z.redisplay(),
                "vi-cmd-mode" => {
                    let km = if z.invicmdmode() { "main" } else { "vicmd" };
                    if z.selectkeymap(km, false) != 0 {
                        feep = true;
                    }
                }
                _ => break cmd,
            }
        };
        let mut ins = false;
        match cmd.as_str() {
            "vi-backward-delete-char"
            | "backward-delete-char"
            | "vi-backward-kill-word"
            | "backward-kill-word"
            | "backward-delete-word" => {
                let only_one = cmd == "vi-backward-delete-char" || cmd == "backward-delete-char";
                let old_sbptr = sbptr;
                if top_spot > 0 {
                    loop {
                        top_spot -= 1;
                        let s = spots[top_spot];
                        hl = s.hl;
                        pos = s.pos;
                        pat_hl = s.pat_hl;
                        pat_pos = s.pat_pos;
                        end_pos = s.end_pos;
                        z.cs = s.cs;
                        sbptr = s.len;
                        dir = s.dir;
                        nomatch = s.nomatch;
                        if only_one || top_spot == 0 || old_sbptr != sbptr {
                            break;
                        }
                    }
                    patprog = None;
                    nosearch = true;
                    skip_pos = false;
                } else {
                    feep = true;
                }
                if nomatch != 0 {
                    badtxt = if nomatch == 2 {
                        INVALID_TEXT
                    } else {
                        FAILING_TEXT
                    };
                    stline = Some(true);
                    skip_pos = true;
                }
                zt = h.zt(hl);
                if nomatch != 0 || sbptr == 0 || (sbptr == 1 && sbuf[0] == '^') {
                    let i = z.cs;
                    zle_setline(z, hl);
                    z.cs = i;
                }
                dirtxt = if dir == 1 { "fwd" } else { "bck" };
                continue 'search;
            }
            "accept-and-hold" => {
                exitfn = Some(crate::zle::widgets::misc::acceptandhold);
                break 'search;
            }
            "accept-and-infer-next-history" => {
                exitfn = Some(acceptandinfernexthistory);
                break 'search;
            }
            "accept-line-and-down-history" => {
                exitfn = Some(acceptlineanddownhistory);
                break 'search;
            }
            "accept-line" => {
                exitfn = Some(crate::zle::widgets::misc::acceptline);
                break 'search;
            }
            "history-incremental-search-backward"
            | "history-incremental-pattern-search-backward"
            | "history-incremental-search-forward"
            | "history-incremental-pattern-search-forward"
            | "vi-rev-repeat-search"
            | "vi-repeat-search" => {
                pat_hl = hl;
                pat_pos = pos;
                set_isrch_spot(&mut spots, top_spot, cur_spot!());
                top_spot += 1;
                match cmd.as_str() {
                    "history-incremental-search-backward"
                    | "history-incremental-pattern-search-backward" => {
                        if dir != -1 {
                            dir = -1;
                        } else {
                            skip_pos = true;
                        }
                    }
                    "history-incremental-search-forward"
                    | "history-incremental-pattern-search-forward" => {
                        if dir != 1 {
                            dir = 1;
                        } else {
                            skip_pos = true;
                        }
                    }
                    "vi-rev-repeat-search" => {
                        dir = -odir;
                        skip_pos = true;
                    }
                    _ => {
                        dir = odir;
                        skip_pos = true;
                    }
                }
                if sbptr == 0 && dir == odir {
                    if let Some(prev) = z.lastsearch.clone().filter(|p| !p.is_empty()) {
                        sbptr = 0;
                        for c in prev.chars() {
                            put(&mut sbuf, sbptr, c);
                            sbptr += 1;
                        }
                    }
                }
                dirtxt = if dir == 1 { "fwd" } else { "bck" };
                continue 'search;
            }
            "vi-quoted-insert" | "quoted-insert" => {
                if cmd == "vi-quoted-insert" {
                    z.statusline = isearch_status(stline, badtxt, dirtxt, &sbuf[..sbptr], '^');
                    z.zrefresh();
                }
                if z.getfullchar(false).is_none() {
                    feep = true;
                } else {
                    ins = true;
                }
            }
            "bracketed-paste" => {
                let paste = crate::zle::widgets::misc::bracketedstring(z);
                set_isrch_spot(&mut spots, top_spot, cur_spot!());
                top_spot += 1;
                for c in paste.chars() {
                    put(&mut sbuf, sbptr, c);
                    sbptr += 1;
                }
                patprog = None;
            }
            "accept-search" => break 'search,
            _ => {
                if cmd == "self-insert-unmeta" {
                    crate::zle::widgets::misc::fixunmeta(z);
                } else if cmd == "magic-space" {
                    crate::zle::widgets::complete::fixmagicspace(z);
                } else if cmd == "self-insert" {
                    if !z.lastchar_wide_valid && z.getrestchar(z.lastchar as u8).is_none() {
                        utils::handlefeep(z, &[]);
                        continue 'search;
                    }
                } else {
                    z.ungetkeycmd();
                    if cmd == "send-break" {
                        aborted = true;
                        z.lastabortedsearch = Some(sbuf[..sbptr].iter().collect());
                        sbptr = 0;
                    }
                    break 'search;
                }
                ins = true;
            }
        }
        if ins {
            if sbptr == PATH_MAX {
                feep = true;
                continue 'search;
            }
            set_isrch_spot(&mut spots, top_spot, cur_spot!());
            top_spot += 1;
            put(&mut sbuf, sbptr, lastfullchar(z));
            sbptr += 1;
            patprog = None;
        }
        if feep {
            utils::handlefeep(z, &[]);
        }
        feep = false;
    }
    if sbptr > 0 {
        z.lastsearch = Some(sbuf[..sbptr].iter().collect());
    }
    z.statusline = None;
    z.callhook("zle-isearch-exit", None);
    z.redrawhook();
    if let Some(f) = exitfn {
        f(z, &[]);
    }
    z.selectkeymap(&okeymap, true);
    z.isearch.2 = false;
    // Keys given as an argument must not run as commands afterwards.
    if let Some(s) = savekeys {
        if z.unget.len() > s {
            z.unget.truncate(s);
        }
    }
    z.selectlocalmap(None);
    if aborted {
        3
    } else {
        nomatch
    }
}

// The event after the last earlier occurrence of the current line.
fn infernexthist(z: &Zle, h: &Hist, ev: usize) -> Option<usize> {
    let line = &z.line;
    let mut he = h.movehistent(ev, -2);
    while let Some(e) = he {
        if zlinecmp(&h.zt(e), line) == 0 {
            return h.movehistent(e, 1);
        }
        he = h.movehistent(e, -1);
    }
    None
}

pub fn acceptandinfernexthistory(z: &mut Zle, _args: &[String]) -> i32 {
    if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
        return 1;
    }
    let h = Hist::load(z);
    let Some(e) = infernexthist(z, &h, z.curhist) else {
        return 1;
    };
    utils::push_bufstack(h.nam(e));
    z.done = true;
    STACKHIST.with(|s| s.set(Some(e)));
    0
}

pub fn infernexthistory(z: &mut Zle, _args: &[String]) -> i32 {
    if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
        return 1;
    }
    let h = Hist::load(z);
    if !h.exists(z.histline as i64) {
        return 1;
    }
    let Some(e) = infernexthist(z, &h, z.histline) else {
        return 1;
    };
    zle_setline(z, e);
    0
}

pub fn vifetchhistory(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.mult < 0 {
        return 1;
    }
    let nohist = z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0;
    if z.histline == z.curhist || nohist {
        if z.zmod.flags & MOD_MULT == 0 {
            z.cs = z.ll();
            z.cs = z.findbol();
            return 0;
        }
        if nohist {
            return 1;
        }
    }
    let ev = if z.zmod.flags & MOD_MULT != 0 {
        z.zmod.mult
    } else {
        z.curhist as i64
    };
    if !zle_goto_hist_dir(z, ev, 0, false) && histbeep() {
        return 1;
    }
    0
}

// Reads a vi search string in the minibuffer. True if one was given.
fn getvisrchstr(z: &mut Zle) -> bool {
    let mut sbuf: Vec<char> = vec![if z.visrchsense == -1 { '?' } else { '/' }];
    let mut sptr: usize = 1;
    let mut ret = false;
    let mut feep = false;
    let okeymap = z.curkeymapname.clone();
    let penult = z.visrchstr.take();
    VIPENULTSRCHSTR.with(|p| *p.borrow_mut() = penult);
    z.clearlist = true;
    z.selectkeymap("main", true);
    let show = |sbuf: &[char], sptr: usize, tail: char| -> Option<String> {
        let mut s: String = sbuf[..sptr].iter().collect();
        s.push(tail);
        Some(s)
    };
    while sptr != 0 {
        z.statusline = show(&sbuf, sptr, '_');
        z.zrefresh();
        let mut cmd = match z.getkeycmd() {
            Some(c) if c != "send-break" => c,
            _ => {
                ret = false;
                break;
            }
        };
        if cmd == "magic-space" {
            crate::zle::widgets::complete::fixmagicspace(z);
            cmd = "self-insert".to_string();
        }
        let mut ins = false;
        match cmd.as_str() {
            "redisplay" => z.redisplay(),
            "clear-screen" => z.clearscreen(),
            "accept-line" | "vi-cmd-mode" => {
                let s: String = sbuf[1..sptr].iter().collect();
                z.visrchstr = if s.is_empty() {
                    VIPENULTSRCHSTR.with(|p| p.borrow().clone())
                } else {
                    Some(s)
                };
                ret = true;
                sptr = 0;
            }
            "backward-delete-char" | "vi-backward-delete-char" => {
                if sptr > 1 {
                    sptr -= 1;
                }
            }
            "backward-kill-word" | "vi-backward-kill-word" => {
                while sptr != 1 {
                    if !zc_iblank(sbuf[sptr - 1]) {
                        break;
                    }
                    sptr -= 1;
                }
                if sptr > 1 {
                    let ident = crate::lexer::is_name_char(sbuf[sptr - 1]);
                    loop {
                        sptr -= 1;
                        if sptr == 1 {
                            break;
                        }
                        let cc = sbuf[sptr - 1];
                        let stop = if ident {
                            !crate::lexer::is_name_char(cc)
                        } else {
                            crate::lexer::is_name_char(cc) || zc_iblank(cc)
                        };
                        if stop {
                            break;
                        }
                    }
                }
            }
            "vi-quoted-insert" | "quoted-insert" => {
                if cmd == "vi-quoted-insert" {
                    z.statusline = show(&sbuf, sptr, '^');
                    z.zrefresh();
                }
                if z.getfullchar(false).is_none() {
                    feep = true;
                } else {
                    ins = true;
                }
            }
            "self-insert-unmeta" | "self-insert" => {
                if cmd == "self-insert-unmeta" {
                    crate::zle::widgets::misc::fixunmeta(z);
                } else if !z.lastchar_wide_valid && z.getrestchar(z.lastchar as u8).is_none() {
                    utils::handlefeep(z, &[]);
                    continue;
                }
                ins = true;
            }
            _ => feep = true,
        }
        if ins {
            put(&mut sbuf, sptr, lastfullchar(z));
            sptr += 1;
        }
        if feep {
            utils::handlefeep(z, &[]);
        }
        feep = false;
    }
    z.statusline = None;
    z.selectkeymap(&okeymap, true);
    ret
}

// vi-history-search-forward and -backward in direction `sense`.
fn vihistorysearch(z: &mut Zle, args: &[String], sense: i32) -> i32 {
    if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
        return 1;
    }
    if let Some(a) = args.first() {
        let ose = z.visrchsense;
        let ost = z.visrchstr.take();
        z.visrchsense = sense;
        z.visrchstr = Some(a.clone());
        let ret = virepeatsearch(z, &noargs());
        z.visrchsense = ose;
        z.visrchstr = ost;
        return ret;
    }
    z.visrchsense = sense;
    if getvisrchstr(z) {
        return virepeatsearch(z, &noargs());
    }
    1
}

pub fn vihistorysearchforward(z: &mut Zle, args: &[String]) -> i32 {
    vihistorysearch(z, args, 1)
}

pub fn vihistorysearchbackward(z: &mut Zle, args: &[String]) -> i32 {
    vihistorysearch(z, args, -1)
}

pub fn virepeatsearch(z: &mut Zle, _args: &[String]) -> i32 {
    let Some(vs) = z.visrchstr.clone() else {
        return 1;
    };
    if z.virangeflag != 0 || z.zlereadflags & ZLRF_HISTORY == 0 {
        return 1;
    }
    let mut n = z.zmod.mult;
    if n < 0 {
        n = -n;
        z.visrchsense = -z.visrchsense;
    }
    let mut h = Hist::load(z);
    if !h.exists(z.histline as i64) || z.visrchsense == 0 {
        return 1;
    }
    let nodups = histfindnodups();
    let vs: Vec<char> = vs.chars().collect();
    let line = z.line.clone();
    let mut ev = z.histline;
    while let Some(e) = h.movehistent(ev, z.visrchsense as i64) {
        ev = e;
        if nodups && h.dup(e) {
            continue;
        }
        let zt = h.zt(e);
        let found = if vs.first() == Some(&'^') {
            zt.starts_with(&vs[1..])
        } else {
            zlinefind(&zt, 0, &vs, 1, 1).is_some()
        };
        if zlinecmp(&zt, &line) != 0 && found {
            n -= 1;
            if n <= 0 {
                zle_setline(z, e);
                return 0;
            }
        }
    }
    1
}

pub fn virevrepeatsearch(z: &mut Zle, args: &[String]) -> i32 {
    z.visrchsense = -z.visrchsense;
    let ret = virepeatsearch(z, args);
    z.visrchsense = -z.visrchsense;
    ret
}

pub fn historybeginningsearchbackward(z: &mut Zle, args: &[String]) -> i32 {
    let cpos = z.cs;
    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = historybeginningsearchforward(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let mut h = Hist::load(z);
    if !h.exists(z.histline as i64) {
        return 1;
    }
    let nodups = histfindnodups();
    let line = z.line.clone();
    let pre = &line[..cpos.min(line.len())];
    let mut ev = z.histline;
    while let Some(e) = h.movehistent(ev, -1) {
        ev = e;
        if nodups && h.dup(e) {
            continue;
        }
        let zt = h.zt(e);
        if zlinecmp(&zt, pre) < 0 && zlinecmp(&zt, &line) != 0 {
            n -= 1;
            if n <= 0 {
                zle_setline(z, e);
                z.cs = cpos;
                return 0;
            }
        }
    }
    1
}

pub fn historybeginningsearchforward(z: &mut Zle, args: &[String]) -> i32 {
    let cpos = z.cs;
    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = historybeginningsearchbackward(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let mut h = Hist::load(z);
    if !h.exists(z.histline as i64) {
        return 1;
    }
    let nodups = histfindnodups();
    let line = z.line.clone();
    let pre = &line[..cpos.min(line.len())];
    let mut ev = z.histline;
    while let Some(e) = h.movehistent(ev, 1) {
        ev = e;
        if nodups && h.dup(e) {
            continue;
        }
        let zt = h.zt(e);
        if zlinecmp(&zt, pre) < i32::from(e == z.curhist) && zlinecmp(&zt, &line) != 0 {
            n -= 1;
            if n <= 0 {
                zle_setline(z, e);
                z.cs = cpos;
                return 0;
            }
        }
    }
    1
}
