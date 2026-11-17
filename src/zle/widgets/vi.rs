//! Port of zsh's zle_vi.c.

use super::{misc, move_};
use crate::zle::keymap::Bind;
use crate::zle::*;
use std::cell::Cell;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("vi-add-eol", viaddeol),
    ("vi-add-next", viaddnext),
    ("vi-backward-delete-char", vibackwarddeletechar),
    ("vi-caps-lock-panic", vicapslockpanic),
    ("vi-change", vichange),
    ("vi-change-eol", vichangeeol),
    ("vi-change-whole-line", vichangewholeline),
    ("vi-cmd-mode", vicmdmode),
    ("vi-delete", videlete),
    ("vi-delete-char", videletechar),
    ("vi-digit-or-beginning-of-line", vidigitorbeginningofline),
    ("vi-down-case", vidowncase),
    ("vi-indent", viindent),
    ("vi-insert", viinsert),
    ("vi-insert-bol", viinsertbol),
    ("vi-join", vijoin),
    ("vi-kill-eol", vikilleol),
    ("vi-kill-line", vikillline),
    ("vi-open-line-above", viopenlineabove),
    ("vi-open-line-below", viopenlinebelow),
    ("vi-oper-swap-case", vioperswapcase),
    ("vi-pound-insert", vipoundinsert),
    ("vi-quoted-insert", viquotedinsert),
    ("vi-repeat-change", virepeatchange),
    ("vi-replace", vireplace),
    ("vi-replace-chars", vireplacechars),
    ("vi-set-buffer", visetbuffer),
    ("vi-substitute", visubstitute),
    ("vi-swap-case", viswapcase),
    ("vi-undo-change", viundochange),
    ("vi-unindent", viunindent),
    ("vi-up-case", viupcase),
    ("vi-yank", viyank),
    ("vi-yank-eol", viyankeol),
    ("vi-yank-whole-line", viyankwholeline),
];

/// zsh's mark = -1 while getvirange runs a movement: "the movement did
/// not set the mark". Not usize::MAX so spaceinline cannot overflow it.
pub const NOMARK: usize = usize::MAX / 2;

thread_local! {
    /// zsh's wordflag: set while getvirange runs the movement for `c`,
    /// so vi-forward-word and friends stop at the end of the word.
    static WORDFLAG: Cell<bool> = const { Cell::new(false) };
}

/// Whether the movement being run is the range of a vi-change (`cw`).
pub fn wordflag() -> bool {
    WORDFLAG.with(|w| w.get())
}

fn set_wordflag(v: bool) {
    WORDFLAG.with(|w| w.set(v));
}

/// zsh's `k == Th(z_name)`; the dotted immortal name is the same widget.
fn is_thingy(name: &str, canon: &str) -> bool {
    name == canon || name.strip_prefix('.') == Some(canon)
}

fn iblank(c: char) -> bool {
    c != '\n' && c.is_whitespace()
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

/// im: >= 0 is an insert mode (1 insert, 0 overwrite); -1 leaves the
/// mode alone; -2 enters viins at the start of editing, synthesising
/// the change that entered insert mode.
pub fn startvichange(z: &mut Zle, im: i32) {
    if im > -1 {
        z.insmode = im != 0;
    }
    if z.viinrepeat != 0 && im != -2 {
        z.zmod = z.lastvichg.zmod;
        z.vichgflag = 0;
    } else if z.vichgflag == 0 {
        z.curvichg.zmod = z.zmod;
        if im == -2 {
            z.vichgflag = 1;
            let c = if z.line.is_empty() {
                b'o'
            } else if z.insmode {
                if z.cs < z.line.len() {
                    b'i'
                } else {
                    b'a'
                }
            } else {
                b'R'
            };
            z.curvichg.buf = vec![c];
        } else {
            z.vichgflag = 2;
            z.curvichg.buf = z.keybuf.clone();
        }
    }
}

fn startvitext(z: &mut Zle, im: i32) {
    startvichange(z, im);
    z.selectkeymap("main", true);
    z.vistartchange = z.undo_changeno;
    z.viinsbegin = z.cs;
}

/// Reads a character argument for a vi command; None aborts.
pub fn vigetkey(z: &mut Zle) -> Option<char> {
    let mn = keymap::open_keymap("main");
    let b = z.getbyte(false).ok()?;
    let cmd = match &mn {
        Some(km) => km.borrow().keybind(&[b]).cloned(),
        None => None,
    };
    match cmd {
        Some(Bind::Str(_)) => return None,
        Some(Bind::Widget(w)) => {
            if is_thingy(&w, "send-break") || is_thingy(&w, "vi-cmd-mode") {
                return None;
            } else if is_thingy(&w, "quoted-insert") {
                z.getfullchar(false)?;
            } else if is_thingy(&w, "vi-quoted-insert") {
                let cs = z.cs;
                let at_end = cs == z.line.len();
                let sav = if at_end {
                    z.line.push('^');
                    None
                } else {
                    Some(std::mem::replace(&mut z.line[cs], '^'))
                };
                z.zrefresh();
                let c = z.getfullchar(false);
                match sav {
                    Some(s) => z.line[cs] = s,
                    None => {
                        z.line.pop();
                    }
                }
                c?;
            }
        }
        None => {}
    }
    if !z.lastchar_wide_valid {
        z.getrestchar(z.lastchar as u8);
    }
    z.lastchar_wide
}

/// Runs the movement of a vi operator and returns the end of the range
/// it covers, leaving the cursor at the start. None on failure.
pub fn getvirange(z: &mut Zle, wf: bool) -> Option<usize> {
    let mut pos = z.cs;
    let mpos = z.mark;
    let mut ret = 0;
    let visual = z.region_active;
    let mult1 = z.zmod.mult;
    let hist1 = z.histline;

    if visual != 0 {
        if z.line.is_empty() {
            return None;
        }
        pos = z.mark;
        z.vilinerange = visual == 2;
        z.region_active = 0;
    } else {
        z.virangeflag = 1;
        set_wordflag(wf);
        z.mark = NOMARK;
        if let Some(km) = keymap::open_keymap("viopp") {
            z.selectlocalmap(Some(km));
        }
        z.zmod.flags &= !MOD_TMULT;
        loop {
            z.vilinerange = false;
            z.prefixflag = false;
            let k2 = match z.getkeycmd() {
                Some(k) if !is_thingy(&k, "send-break") => k,
                _ => {
                    set_wordflag(false);
                    z.virangeflag = 0;
                    z.mark = mpos;
                    return None;
                }
            };
            // The operator key again (dd, cc, yy): a number of lines.
            let r = if z.bindk.as_deref() == Some(k2.as_str()) {
                dovilinerange(z)
            } else {
                z.execzlefunc(&k2, &[], true, false)
            };
            if r != 0 {
                ret = -1;
            }
            if z.viinrepeat != 0 {
                z.zmod.mult = mult1;
            } else {
                z.zmod.mult = mult1 * z.zmod.tmult;
                if z.vichgflag == 2 {
                    z.curvichg.zmod.mult = z.zmod.mult;
                }
            }
            if !(z.prefixflag && ret == 0) {
                break;
            }
        }
        set_wordflag(false);
        z.selectlocalmap(None);

        // A movement that changed the line or history line is an error.
        if z.histline != hist1 || z.line != z.lastline {
            z.histline = hist1;
            z.line = z.lastline.clone();
            z.cs = pos;
            z.mark = mpos;
            z.virangeflag = 0;
            return None;
        }

        if z.line.is_empty()
            || (z.cs == pos && (z.mark == NOMARK || z.mark == z.cs) && z.virangeflag != 2)
            || ret == -1
        {
            z.mark = mpos;
            z.virangeflag = 0;
            return None;
        }
        z.virangeflag = 0;

        if z.mark != NOMARK {
            pos = z.mark;
        }
    }
    z.mark = mpos;

    if z.cs > pos {
        std::mem::swap(&mut z.cs, &mut pos);
    }

    if visual == 1 && pos < z.line.len() && z.invicmdmode() {
        pos += 1;
    }

    z.vilinerange = z.zmod.flags & MOD_LINE != 0 || (z.vilinerange && z.zmod.flags & MOD_CHAR == 0);
    if z.vilinerange {
        let newcs = z.findbol();
        z.lastcol = (z.cs - newcs) as i32;
        z.cs = pos;
        pos = z.findeol();
        z.cs = newcs;
    } else if visual == 0 && pos > 0 {
        // A character-wise range leaves out a newline at its end.
        let prev = pos - 1;
        if z.line.get(prev) == Some(&'\n') {
            pos = prev;
        }
    }
    Some(pos)
}

fn dovilinerange(z: &mut Zle) -> i32 {
    let pos = z.cs;
    let mut n = z.zmod.mult;
    let ll = z.line.len();

    z.vilinerange = true;
    if n == 0 {
        return 1;
    }
    if n > 0 {
        let mut cs = z.cs;
        loop {
            let c = n;
            n -= 1;
            if c == 0 || cs > ll {
                break;
            }
            cs = z.findeol_at(cs) + 1;
        }
        if n != -1 {
            z.cs = pos;
            return 1;
        }
        z.cs = cs - 1;
    } else {
        let mut cs = z.cs as isize;
        loop {
            let c = n;
            n += 1;
            if c == 0 || cs < 0 {
                break;
            }
            cs = z.findbol_at(cs as usize) as isize - 1;
        }
        if n != 1 {
            z.cs = pos;
            return 1;
        }
        z.cs = (cs + 1) as usize;
    }
    z.virangeflag = 2;
    0
}

pub fn viaddnext(z: &mut Zle, _args: &[String]) -> i32 {
    if z.cs != z.findeol() {
        z.cs += 1;
    }
    startvitext(z, 1);
    0
}

pub fn viaddeol(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = z.findeol();
    startvitext(z, 1);
    0
}

pub fn viinsert(z: &mut Zle, _args: &[String]) -> i32 {
    startvitext(z, 1);
    0
}

/// Enters vi insert mode when the line editor starts.
pub fn viinsert_init(z: &mut Zle) {
    startvitext(z, -2);
}

pub fn viinsertbol(z: &mut Zle, _args: &[String]) -> i32 {
    move_::vifirstnonblank(z, &[]);
    startvitext(z, 1);
    0
}

pub fn videlete(z: &mut Zle, _args: &[String]) -> i32 {
    let mut ret = 1;
    startvichange(z, 1);
    if let Some(c2) = getvirange(z, false) {
        z.forekill(c2.saturating_sub(z.cs), CUT_RAW);
        ret = 0;
        if z.vilinerange && !z.line.is_empty() {
            z.lastcol = -1;
            if z.cs == z.line.len() {
                z.cs -= 1;
            }
            z.foredel(1, 0);
            move_::vifirstnonblank(z, &[]);
        }
    }
    ret
}

pub fn videletechar(z: &mut Zle, args: &[String]) -> i32 {
    startvichange(z, -1);
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = vibackwarddeletechar(z, args);
        z.zmod.mult = n;
        return ret;
    }
    if z.cs == z.line.len() || z.line[z.cs] == '\n' {
        return 1;
    }
    let room = (z.findeol() - z.cs) as i64;
    if n > room {
        n = room;
        z.forekill(n as usize, CUT_RAW);
    } else {
        z.forekill(n as usize, 0);
    }
    0
}

pub fn vichange(z: &mut Zle, _args: &[String]) -> i32 {
    let mut ret = 1;
    startvichange(z, 1);
    if let Some(c2) = getvirange(z, true) {
        ret = 0;
        z.forekill(c2.saturating_sub(z.cs), CUT_RAW);
        z.selectkeymap("main", true);
        z.viinsbegin = z.cs;
        z.vistartchange = z.undo_changeno;
    }
    ret
}

pub fn visubstitute(z: &mut Zle, _args: &[String]) -> i32 {
    startvichange(z, 1);
    let mut n = z.zmod.mult;
    if n < 0 {
        return 1;
    }
    if z.cs == z.line.len() || z.line[z.cs] == '\n' {
        return 1;
    }
    if z.region_active != 0 {
        misc::killregion(z, &[]);
    } else {
        let room = (z.findeol() - z.cs) as i64;
        if n > room {
            n = room;
        }
        z.forekill(n as usize, CUT_RAW);
    }
    startvitext(z, 1);
    0
}

pub fn vichangeeol(z: &mut Zle, _args: &[String]) -> i32 {
    if z.region_active != 0 {
        let (a, b) = misc::regionlines(z);
        z.cs = a;
        z.region_active = 0;
        let cs = z.cs;
        z.cut(cs, b - cs, CUT_RAW);
        z.shiftchars(cs, b - cs);
    } else {
        let n = z.findeol() - z.cs;
        z.forekill(n, CUT_RAW);
    }
    startvitext(z, 1);
    0
}

pub fn vichangewholeline(z: &mut Zle, args: &[String]) -> i32 {
    move_::vifirstnonblank(z, args);
    vichangeeol(z, &[])
}

pub fn viyank(z: &mut Zle, _args: &[String]) -> i32 {
    let mut ret = 1;
    startvichange(z, 1);
    if let Some(c2) = getvirange(z, false) {
        let cs = z.cs;
        z.cut(cs, c2.saturating_sub(cs), CUT_YANK);
        ret = 0;
    }
    // In line mode, go back to the column the cursor was in.
    if z.vilinerange && z.lastcol != -1 {
        let x = z.findeol();
        z.cs += z.lastcol.max(0) as usize;
        if z.cs >= x {
            z.cs = x;
            if z.cs > z.findbol() && z.invicmdmode() {
                z.cs -= 1;
            }
        }
        z.lastcol = -1;
    }
    ret
}

pub fn viyankeol(z: &mut Zle, _args: &[String]) -> i32 {
    let x = z.findeol();
    startvichange(z, -1);
    if x == z.cs {
        return 1;
    }
    let cs = z.cs;
    z.cut(cs, x - cs, CUT_YANK);
    0
}

pub fn viyankwholeline(z: &mut Zle, _args: &[String]) -> i32 {
    let bol = z.findbol();
    let oldcs = z.cs;
    startvichange(z, -1);
    let n = z.zmod.mult;
    if n < 1 {
        return 1;
    }
    for _ in 0..n {
        if z.cs > z.line.len() {
            z.cs = oldcs;
            return 1;
        }
        z.cs = z.findeol() + 1;
    }
    z.vilinerange = true;
    let len = z.cs - bol - 1;
    z.cut(bol, len, CUT_YANK);
    z.cs = oldcs;
    0
}

pub fn vireplace(z: &mut Zle, _args: &[String]) -> i32 {
    startvitext(z, 0);
    0
}

// As in vi, a bad count for r feeps without reading the character; but
// when repeating a change the character is always read, so the repeat
// buffer stays in step.
pub fn vireplacechars(z: &mut Zle, _args: &[String]) -> i32 {
    let mut fail = false;
    let mut newchars: usize = 0;

    startvichange(z, 1);
    let mut n = z.zmod.mult;
    if n > 0 {
        if z.region_active != 0 {
            let (a, mut b) = if z.region_active == 1 {
                if z.mark > z.cs {
                    (z.cs, z.mark + 1)
                } else {
                    (z.mark, z.cs + 1)
                }
            } else {
                misc::regionlines(z)
            };
            z.cs = a;
            if b > z.line.len() {
                b = z.line.len();
            }
            let b = b.max(a);
            n = (b - a) as i64;
            newchars = b - a;
            z.region_active = 0;
        } else {
            let mut pos = z.cs;
            while n > 0 {
                n -= 1;
                if pos == z.line.len() || z.line[pos] == '\n' {
                    fail = true;
                    break;
                }
                newchars += 1;
                pos += 1;
            }
            n = (pos - z.cs) as i64;
        }
    }

    if n < 1 || fail {
        if z.viinrepeat != 0 {
            vigetkey(z);
        }
        return 1;
    }
    let Some(ch) = vigetkey(z) else {
        return 1;
    };
    let n = n as usize;
    if ch == '\r' || ch == '\n' {
        z.cs += n - 1;
        z.backkill(n - 1, CUT_RAW);
        let cs = z.cs;
        z.line[cs] = '\n';
        z.cs += 1;
    } else {
        for _ in 0..newchars {
            let cs = z.cs;
            z.line[cs] = ch;
            z.cs += 1;
        }
        z.cs -= 1;
    }
    0
}

pub fn vicmdmode(z: &mut Zle, _args: &[String]) -> i32 {
    if z.invicmdmode() || z.selectkeymap("vicmd", false) != 0 {
        return 1;
    }
    z.mergeundo();
    z.insmode = !crate::options::is_set("overstrike");
    if z.vichgflag == 1 {
        z.vichgflag = 0;
        z.lastvichg = std::mem::take(&mut z.curvichg);
    }
    if z.viinrepeat == 1 {
        z.viinrepeat = 0;
    }
    if z.cs != z.findbol() {
        z.cs -= 1;
    }
    0
}

pub fn viopenlinebelow(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = z.findeol();
    z.spaceinline(1);
    let cs = z.cs;
    z.line[cs] = '\n';
    z.cs += 1;
    startvitext(z, 1);
    z.clearlist = true;
    0
}

pub fn viopenlineabove(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = z.findbol();
    z.spaceinline(1);
    let cs = z.cs;
    z.line[cs] = '\n';
    startvitext(z, 1);
    z.clearlist = true;
    0
}

// The case operators: apply `f` to each character in the range.
fn vicaseop(z: &mut Zle, f: fn(char) -> char) -> i32 {
    startvichange(z, 1);
    match getvirange(z, false) {
        Some(c2) => {
            let oldcs = z.cs;
            while z.cs < c2 {
                let cs = z.cs;
                z.line[cs] = f(z.line[cs]);
                z.cs += 1;
            }
            z.cs = oldcs;
            0
        }
        None => 1,
    }
}

fn swapcase(c: char) -> char {
    if c.is_lowercase() {
        toupper(c)
    } else if c.is_uppercase() {
        tolower(c)
    } else {
        c
    }
}

pub fn vioperswapcase(z: &mut Zle, _args: &[String]) -> i32 {
    vicaseop(z, swapcase)
}

pub fn viupcase(z: &mut Zle, _args: &[String]) -> i32 {
    vicaseop(z, toupper)
}

pub fn vidowncase(z: &mut Zle, _args: &[String]) -> i32 {
    vicaseop(z, tolower)
}

pub fn virepeatchange(z: &mut Zle, _args: &[String]) -> i32 {
    if z.lastvichg.buf.is_empty() || z.vichgflag != 0 || z.virangeflag != 0 {
        return 1;
    }
    if z.zmod.flags & MOD_MULT != 0 {
        z.lastvichg.zmod.mult = z.zmod.mult;
        z.lastvichg.zmod.flags |= MOD_MULT;
    }
    if z.zmod.flags & MOD_VIBUF != 0 {
        z.lastvichg.zmod.vibuf = z.zmod.vibuf;
        z.lastvichg.zmod.flags =
            (z.lastvichg.zmod.flags & !MOD_VIAPP) | MOD_VIBUF | (z.zmod.flags & MOD_VIAPP);
    } else if z.lastvichg.zmod.flags & MOD_VIBUF != 0 && (27..=34).contains(&z.lastvichg.zmod.vibuf)
    {
        // "1 to "8 advance to the next buffer.
        z.lastvichg.zmod.vibuf += 1;
    }
    z.viinrepeat = 3;
    let buf = z.lastvichg.buf.clone();
    z.ungetbytes(&buf);
    0
}

pub fn viindent(z: &mut Zle, _args: &[String]) -> i32 {
    let mut oldcs = z.cs;
    startvichange(z, 1);
    if z.region_active == 1 {
        z.region_active = 2;
    }
    let Some(mut c2) = getvirange(z, false) else {
        return 1;
    };
    if !z.vilinerange {
        z.cs = oldcs;
        return 1;
    }
    oldcs = z.cs;
    while z.cs <= c2 {
        if z.line.get(z.cs) == Some(&'\n') {
            // Blank lines are left alone.
            z.cs += 1;
        } else {
            z.spaceinline(1);
            c2 += 1;
            let cs = z.cs;
            z.line[cs] = '\t';
            z.cs = z.findeol() + 1;
        }
    }
    z.cs = oldcs;
    move_::vifirstnonblank(z, &[]);
    0
}

pub fn viunindent(z: &mut Zle, _args: &[String]) -> i32 {
    let mut oldcs = z.cs;
    startvichange(z, 1);
    if z.region_active == 1 {
        z.region_active = 2;
    }
    let Some(mut c2) = getvirange(z, false) else {
        return 1;
    };
    if !z.vilinerange {
        z.cs = oldcs;
        return 1;
    }
    oldcs = z.cs;
    while z.cs < c2 {
        if z.line[z.cs] == '\t' {
            z.foredel(1, 0);
            c2 -= 1;
        }
        z.cs = z.findeol() + 1;
    }
    z.cs = oldcs;
    move_::vifirstnonblank(z, &[]);
    0
}

pub fn vibackwarddeletechar(z: &mut Zle, args: &[String]) -> i32 {
    if z.invicmdmode() {
        startvichange(z, -1);
    }

    let mut n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = videletechar(z, args);
        z.zmod.mult = n;
        return ret;
    }
    // Not past the start of the line, nor (inserting) of the insertion.
    if (!z.invicmdmode() && (z.cs as i64) - n < z.viinsbegin as i64) || z.cs == z.findbol() {
        return 1;
    }
    let room = (z.cs - z.findbol()) as i64;
    if n > room {
        n = room;
        z.backkill(n as usize, CUT_FRONT | CUT_RAW);
    } else {
        z.backkill(n as usize, CUT_FRONT);
    }
    0
}

pub fn vikillline(z: &mut Zle, _args: &[String]) -> i32 {
    if z.viinsbegin > z.cs {
        return 1;
    }
    let n = z.cs - z.viinsbegin;
    z.backdel(n, CUT_RAW);
    0
}

pub fn vijoin(z: &mut Zle, _args: &[String]) -> i32 {
    let visual = z.region_active;

    startvichange(z, -1);
    let mut n = z.zmod.mult;
    if n < 1 {
        return 1;
    }
    let mut x;
    if visual != 0 && z.cs > z.mark {
        move_::exchangepointandmark(z, &[]);
        x = z.findeol();
        if x >= z.mark {
            move_::exchangepointandmark(z, &[]);
            return 1;
        }
    } else {
        x = z.findeol();
        if x == z.line.len() || (visual != 0 && x >= z.mark) {
            return 1;
        }
    }

    loop {
        z.cs = x + 1;
        let pos = z.cs;
        while z.cs != z.line.len() && iblank(z.line[z.cs]) {
            z.cs += 1;
        }
        x = 1 + (z.cs - pos);
        z.backdel(x, CUT_RAW);
        let mut joined_blank = false;
        if z.cs > 0 {
            let p = z.cs - 1;
            if iblank(z.line[p]) {
                z.cs = p;
                joined_blank = true;
            }
        }
        if !joined_blank {
            z.spaceinline(1);
            let cs = z.cs;
            z.line[cs] = ' ';
        }
        if visual == 0 {
            n -= 1;
            if n < 2 {
                break;
            }
        }
        x = z.findeol();
        if x == z.line.len() || (visual != 0 && x >= z.mark) {
            break;
        }
    }
    0
}

pub fn viswapcase(z: &mut Zle, _args: &[String]) -> i32 {
    startvichange(z, -1);
    let mut n = z.zmod.mult;
    if n < 1 {
        return 1;
    }
    let eol = z.findeol();
    if z.cs == eol {
        return 1;
    }
    while z.cs < eol && n > 0 {
        n -= 1;
        let cs = z.cs;
        z.line[cs] = swapcase(z.line[cs]);
        z.cs += 1;
    }
    if z.cs > 0 && z.cs == eol {
        z.cs -= 1;
    }
    0
}

pub fn vicapslockpanic(z: &mut Zle, _args: &[String]) -> i32 {
    z.clearlist = true;
    utils::handlefeep(z, &[]);
    z.statusline = Some("press a lowercase key to continue".into());
    z.zrefresh();
    loop {
        match z.getfullchar(false) {
            Some(c) if c.is_lowercase() => break,
            None => break,
            Some(_) => {}
        }
    }
    z.statusline = None;
    0
}

pub fn visetbuffer(z: &mut Zle, args: &[String]) -> i32 {
    let ch = if let Some(a) = args.first() {
        let mut chars = a.chars();
        let c = chars.next();
        if args.len() > 1 || (c.is_some() && chars.next().is_some()) {
            return 1;
        }
        c
    } else {
        z.getfullchar(false)
    };
    let regmod = match ch {
        Some('_') => Some(MOD_NULL),
        Some('*') => Some(MOD_PRI),
        Some('+') => Some(MOD_CLIP),
        _ => None,
    };
    if let Some(m) = regmod {
        z.zmod.flags |= m;
        z.prefixflag = true;
        return 0;
    }
    z.zmod.flags &= !(MOD_NULL | MOD_OSSEL);
    let Some(ch) = ch.filter(|c| c.is_ascii_alphanumeric()) else {
        return 1;
    };
    if ch.is_ascii_uppercase() {
        z.zmod.flags |= MOD_VIAPP;
    } else {
        z.zmod.flags &= !MOD_VIAPP;
    }
    z.zmod.vibuf = if ch.is_ascii_digit() {
        (ch as u8 - b'0') as usize + 26
    } else {
        (ch.to_ascii_lowercase() as u8 - b'a') as usize
    };
    z.zmod.flags |= MOD_VIBUF;
    z.prefixflag = true;
    0
}

pub fn vikilleol(z: &mut Zle, _args: &[String]) -> i32 {
    let n = z.findeol() - z.cs;
    startvichange(z, -1);
    if n == 0 {
        return 1;
    }
    let n = z.findeol() - z.cs;
    z.forekill(n, CUT_RAW);
    0
}

pub fn vipoundinsert(z: &mut Zle, _args: &[String]) -> i32 {
    let mut oldcs = z.cs;

    startvichange(z, -1);
    move_::vifirstnonblank(z, &[]);
    if z.line.get(z.cs) != Some(&'#') {
        z.spaceinline(1);
        let cs = z.cs;
        z.line[cs] = '#';
        if cs <= z.viinsbegin {
            z.viinsbegin += 1;
        }
        if cs <= oldcs {
            oldcs += 1;
        }
    } else {
        z.foredel(1, 0);
        let cs = z.cs;
        if cs < z.viinsbegin {
            z.viinsbegin -= 1;
        }
        if cs < oldcs {
            oldcs -= 1;
        }
    }
    z.cs = oldcs;
    0
}

pub fn viquotedinsert(z: &mut Zle, args: &[String]) -> i32 {
    z.spaceinline(1);
    let cs = z.cs;
    z.line[cs] = '^';
    z.zrefresh();
    let c = z.getfullchar(false);
    z.foredel(1, 0);
    if c.is_none() {
        1
    } else {
        misc::selfinsert(z, args)
    }
}

/// The 0 key: continues a count like digit-argument, else goes to the
/// start of the line.
pub fn vidigitorbeginningofline(z: &mut Zle, args: &[String]) -> i32 {
    if z.zmod.flags & MOD_TMULT != 0 {
        misc::digitargument(z, args)
    } else {
        z.removesuffix();
        z.invalidatelist();
        move_::vibeginningofline(z, args)
    }
}

pub fn viundochange(z: &mut Zle, args: &[String]) -> i32 {
    z.viundochange(args)
}
