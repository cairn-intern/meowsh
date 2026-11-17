//! Port of zsh's zle_misc.c.

use crate::zle::widgets::{complete, vi};
use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("accept-and-hold", acceptandhold),
    ("accept-line", acceptline),
    ("argument-base", argumentbase),
    ("backward-delete-char", backwarddeletechar),
    ("backward-kill-line", backwardkillline),
    ("bracketed-paste", bracketedpaste),
    ("copy-prev-word", copyprevword),
    ("copy-prev-shell-word", copyprevshellword),
    ("copy-region-as-kill", copyregionaskill),
    ("delete-char", deletechar),
    ("digit-argument", digitargument),
    ("gosmacs-transpose-chars", gosmacstransposechars),
    ("kill-buffer", killbuffer),
    ("kill-line", killline),
    ("kill-region", killregion),
    ("kill-whole-line", killwholeline),
    ("neg-argument", negargument),
    ("overwrite-mode", overwritemode),
    ("pound-insert", poundinsert),
    ("put-replace-selection", putreplaceselection),
    ("quoted-insert", quotedinsert),
    ("quote-line", quoteline),
    ("quote-region", quoteregion),
    ("self-insert", selfinsert),
    ("self-insert-unmeta", selfinsertunmeta),
    ("send-break", sendbreak),
    ("transpose-chars", transposechars),
    ("undefined-key", undefinedkey),
    ("universal-argument", universalargument),
    ("vi-put-after", viputafter),
    ("vi-put-before", viputbefore),
    ("what-cursor-position", whatcursorposition),
    ("yank", yank),
    ("yank-pop", yankpop),
];

/// zsh's kctbuf: the buffer the last yank came from.
#[derive(Clone, Copy)]
enum KctBuf {
    Cut,
    Vi(usize),
}

thread_local! {
    static KCTBUF: std::cell::Cell<Option<KctBuf>> = const { std::cell::Cell::new(None) };
}

fn kctbuf() -> Option<KctBuf> {
    KCTBUF.with(|k| k.get())
}

fn set_kctbuf(k: Option<KctBuf>) {
    KCTBUF.with(|c| c.set(k));
}

fn kctbuf_contents(z: &Zle, k: KctBuf) -> CutBuffer {
    match k {
        KctBuf::Cut => z.cutbuf.clone(),
        KctBuf::Vi(n) => z.vibuf[n].clone(),
    }
}

fn at(z: &Zle, i: usize) -> char {
    z.line.get(i).copied().unwrap_or('\0')
}

/// Inserts `zstr` with repetition and suffix removal.
pub fn doinsert(z: &mut Zle, zstr: &[char]) {
    let len = zstr.len();
    let c1 = zstr.first().copied().unwrap_or('\0');
    let zmult = z.zmod.mult;
    let neg = zmult < 0;
    let m = zmult.unsigned_abs() as usize;

    z.iremovesuffix(Some(c1), false);
    z.invalidatelist();

    // In overwrite mode, don't replace newlines.
    if z.insmode || at(z, z.cs) == '\n' {
        z.spaceinline(m * len);
    } else {
        let mut pos = z.cs;
        let mut i = m * len;
        while pos < z.ll() && z.line[pos] != '\n' && i > 0 {
            i -= 1;
            pos += 1;
        }
        let diff = pos as i64 - z.cs as i64 - (m * len) as i64;
        if diff < 0 {
            z.spaceinline((-diff) as usize);
        } else if diff > 0 {
            let cs = z.cs;
            z.shiftchars(cs, diff as usize);
        }
    }
    for _ in 0..m {
        for &c in zstr {
            let cs = z.cs;
            z.line[cs] = c;
            z.cs += 1;
        }
    }
    if neg {
        z.cs = (z.cs as i64 + zmult * len as i64) as usize;
    }
}

pub fn selfinsert(z: &mut Zle, _args: &[String]) -> i32 {
    // May be redundant with getkeymapcmd(), but other widgets call here too.
    if !z.lastchar_wide_valid && z.getrestchar(z.lastchar as u8).is_none() {
        return 1;
    }
    let Some(tmp) = z.lastchar_wide else {
        return 1;
    };
    doinsert(z, &[tmp]);
    0
}

pub fn fixunmeta(z: &mut Zle) {
    z.lastchar &= 0x7f;
    if z.lastchar == b'\r' as i32 {
        z.lastchar = b'\n' as i32;
    }
    z.lastchar_wide = char::from_u32(z.lastchar as u32);
    z.lastchar_wide_valid = true;
}

pub fn selfinsertunmeta(z: &mut Zle, args: &[String]) -> i32 {
    fixunmeta(z);
    selfinsert(z, args)
}

pub fn deletechar(z: &mut Zle, args: &[String]) -> i32 {
    if z.zmod.mult < 0 {
        z.zmod.mult = -z.zmod.mult;
        let ret = backwarddeletechar(z, args);
        z.zmod.mult = -z.zmod.mult;
        return ret;
    }
    for _ in 0..z.zmod.mult {
        if z.cs == z.ll() {
            return 1;
        }
        z.cs += 1;
    }
    z.backdel(z.zmod.mult as usize, 0);
    0
}

pub fn backwarddeletechar(z: &mut Zle, args: &[String]) -> i32 {
    if z.zmod.mult < 0 {
        z.zmod.mult = -z.zmod.mult;
        let ret = deletechar(z, args);
        z.zmod.mult = -z.zmod.mult;
        return ret;
    }
    let n = (z.zmod.mult as usize).min(z.cs);
    z.backdel(n, 0);
    0
}

pub fn killwholeline(z: &mut Zle, _args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        return 1;
    }
    for _ in 0..n {
        let fg = z.cs != 0 && z.cs == z.ll();
        if fg {
            z.cs -= 1;
        }
        while z.cs != 0 && z.line[z.cs - 1] != '\n' {
            z.cs -= 1;
        }
        let mut i = z.cs;
        while i != z.ll() && z.line[i] != '\n' {
            i += 1;
        }
        let ct = i - z.cs + usize::from(i != z.ll());
        z.forekill(ct, if fg { CUT_FRONT | CUT_RAW } else { CUT_RAW });
    }
    z.clearlist = true;
    0
}

pub fn killbuffer(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = 0;
    let ll = z.ll();
    z.forekill(ll, CUT_RAW);
    z.clearlist = true;
    0
}

pub fn backwardkillline(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = killline(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let mut i = 0;
    for _ in 0..n {
        if z.cs != 0 && z.line[z.cs - 1] == '\n' {
            z.cs -= 1;
            i += 1;
        } else {
            while z.cs != 0 && z.line[z.cs - 1] != '\n' {
                z.cs -= 1;
                i += 1;
            }
        }
    }
    z.forekill(i, CUT_FRONT | CUT_RAW);
    z.clearlist = true;
    0
}

pub fn gosmacstransposechars(z: &mut Zle, _args: &[String]) -> i32 {
    if z.cs < 2 || z.line[z.cs - 1] == '\n' || z.line[z.cs - 2] == '\n' {
        let twice = z.cs == 0 || z.line[z.cs - 1] == '\n';
        if z.cs == z.ll() || z.line[z.cs] == '\n' {
            return 1;
        }
        z.cs += 1;
        if twice {
            if z.cs == z.ll() || z.line[z.cs] == '\n' {
                return 1;
            }
            z.cs += 1;
        }
    }
    let cs = z.cs;
    z.line.swap(cs - 2, cs - 1);
    0
}

pub fn transposechars(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    let neg = n < 0;
    if neg {
        n = -n;
    }
    while n > 0 {
        n -= 1;
        let mut ct = z.cs;
        if ct == 0 || z.line[z.cs - 1] == '\n' {
            if z.ll() == z.cs || z.line[z.cs] == '\n' {
                return 1;
            }
            if !neg {
                z.cs += 1;
            }
            ct += 1;
        }
        if neg {
            if z.cs != 0 && z.line[z.cs - 1] != '\n' {
                z.cs -= 1;
                if ct > 1 && z.line[ct - 2] != '\n' {
                    ct -= 1;
                }
            }
        } else if z.cs != z.ll() && z.line[z.cs] != '\n' {
            z.cs += 1;
        }
        if ct == z.ll() || z.line[ct] == '\n' {
            ct -= 1;
        }
        if ct < 1 || z.line[ct - 1] == '\n' {
            return 1;
        }
        z.line.swap(ct - 1, ct);
    }
    0
}

pub fn poundinsert(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = 0;
    crate::zle::widgets::move_::vifirstnonblank(z, &[]);
    if at(z, z.cs) != '#' {
        z.spaceinline(1);
        let cs = z.cs;
        z.line[cs] = '#';
        z.cs = z.findeol();
        while z.cs != z.ll() {
            z.cs += 1;
            crate::zle::widgets::move_::vifirstnonblank(z, &[]);
            z.spaceinline(1);
            let cs = z.cs;
            z.line[cs] = '#';
            z.cs = z.findeol();
        }
    } else {
        z.foredel(1, 0);
        z.cs = z.findeol();
        while z.cs != z.ll() {
            z.cs += 1;
            crate::zle::widgets::move_::vifirstnonblank(z, &[]);
            if at(z, z.cs) == '#' {
                z.foredel(1, 0);
            }
            z.cs = z.findeol();
        }
    }
    z.done = true;
    0
}

pub fn acceptline(z: &mut Zle, _args: &[String]) -> i32 {
    z.done = true;
    0
}

pub fn acceptandhold(z: &mut Zle, _args: &[String]) -> i32 {
    utils::push_bufstack(z.line.iter().collect());
    crate::zle::widgets::hist::set_stackcs(Some(z.cs));
    z.done = true;
    0
}

pub fn killline(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 0 {
        z.zmod.mult = -n;
        let ret = backwardkillline(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let mut i = 0;
    for _ in 0..n {
        if at(z, z.cs) == '\n' {
            z.cs += 1;
            i += 1;
        } else {
            while z.cs != z.ll() && z.line[z.cs] != '\n' {
                z.cs += 1;
                i += 1;
            }
        }
    }
    z.backkill(i, CUT_RAW);
    z.clearlist = true;
    0
}

/// The start and end of the lines the region covers.
pub fn regionlines(z: &mut Zle) -> (usize, usize) {
    let origcs = z.cs;
    let (start, end);
    if z.cs < z.mark {
        start = z.findbol();
        z.cs = z.mark.min(z.ll());
        end = z.findeol();
    } else {
        end = z.findeol();
        z.cs = z.mark;
        start = z.findbol();
    }
    z.cs = origcs;
    (start, end)
}

pub fn killregion(z: &mut Zle, _args: &[String]) -> i32 {
    if z.mark > z.ll() {
        z.mark = z.ll();
    }
    if z.region_active == 2 {
        let (a, b) = regionlines(z);
        z.cs = a;
        z.region_active = 0;
        z.cut(a, b - a, CUT_RAW);
        z.shiftchars(a, b - a);
        if z.ll() != 0 {
            if z.cs == z.ll() {
                z.cs -= 1;
            }
            z.foredel(1, 0);
            crate::zle::widgets::move_::vifirstnonblank(z, &[]);
        }
    } else if z.mark > z.cs {
        if z.invicmdmode() {
            z.mark += 1;
        }
        let ct = z.mark - z.cs;
        z.forekill(ct, CUT_RAW);
    } else {
        if z.invicmdmode() {
            z.cs += 1;
        }
        let ct = z.cs - z.mark;
        z.backkill(ct, CUT_FRONT | CUT_RAW);
    }
    0
}

pub fn copyregionaskill(z: &mut Zle, args: &[String]) -> i32 {
    if let Some(a) = args.first() {
        let line: Vec<char> = a.chars().collect();
        z.cuttext(&line, CUT_REPLACE);
    } else {
        if z.mark > z.ll() {
            z.mark = z.ll();
        }
        let (start, mut end) = if z.mark > z.cs {
            (z.cs, z.mark)
        } else {
            (z.mark, z.cs)
        };
        if z.invicmdmode() {
            end += 1;
        }
        let flags = if z.mark > z.cs { 0 } else { CUT_FRONT };
        z.cut(start, end - start, flags);
    }
    0
}

pub fn yank(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    if n < 0 {
        return 1;
    }
    let k = if z.zmod.flags & MOD_VIBUF != 0 {
        KctBuf::Vi(z.zmod.vibuf)
    } else {
        KctBuf::Cut
    };
    set_kctbuf(Some(k));
    let buf = kctbuf_contents(z, k);
    if !buf.set {
        return 1;
    }
    z.mark = z.cs;
    z.yankb = z.cs;
    z.yankcs = z.cs;
    while n > 0 {
        n -= 1;
        z.kct = None;
        z.insert_chars(&buf.buf);
        z.yanke = z.cs;
    }
    0
}

/// Pastes `buf`; `position` 0 is before, 1 after, 2 splits the line.
pub fn pastebuf(z: &mut Zle, buf: &CutBuffer, mut mult: i64, mut position: i32) {
    let len = buf.buf.len();
    if buf.flags & CUTBUFFER_LINE != 0 {
        if position == 2 {
            if z.cs == 0 {
                position = 0;
            } else if z.cs == z.ll() {
                position = 1;
            }
        }
        if position == 2 {
            z.yankb = z.cs;
            z.spaceinline(len + 2);
            let cs = z.cs;
            z.line[cs] = '\n';
            z.line[cs + 1..cs + 1 + len].copy_from_slice(&buf.buf);
            z.cs = cs + 1 + len;
            let cs = z.cs;
            z.line[cs] = '\n';
            z.yanke = cs + 1;
        } else if position != 0 {
            z.cs = z.findeol();
            z.yankb = z.cs;
            z.spaceinline(len + 1);
            let cs = z.cs;
            z.line[cs] = '\n';
            z.cs = cs + 1;
            z.yanke = z.cs + len;
            z.line[cs + 1..cs + 1 + len].copy_from_slice(&buf.buf);
        } else {
            z.cs = z.findbol();
            z.yankb = z.cs;
            z.spaceinline(len + 1);
            let cs = z.cs;
            z.line[cs..cs + len].copy_from_slice(&buf.buf);
            z.yanke = cs + len + 1;
            z.line[cs + len] = '\n';
        }
        crate::zle::widgets::move_::vifirstnonblank(z, &[]);
    } else {
        if position == 1 && z.cs != z.findeol() {
            z.cs += 1;
        }
        z.yankb = z.cs;
        while mult > 0 {
            mult -= 1;
            z.insert_chars(&buf.buf);
        }
        z.yanke = z.cs;
        if z.cs != 0 && z.invicmdmode() {
            z.cs -= 1;
        }
    }
}

fn viput(z: &mut Zle, position: i32) -> i32 {
    let n = z.zmod.mult;
    vi::startvichange(z, -1);
    if n < 0 {
        return 1;
    }
    if z.zmod.flags & MOD_NULL != 0 {
        return 0;
    }
    if z.zmod.flags & MOD_OSSEL != 0 {
        // No system clipboard to read from.
        return 1;
    }
    let k = if z.zmod.flags & MOD_VIBUF != 0 {
        KctBuf::Vi(z.zmod.vibuf)
    } else {
        KctBuf::Cut
    };
    set_kctbuf(Some(k));
    let buf = kctbuf_contents(z, k);
    if !buf.set {
        return 1;
    }
    z.kct = None;
    z.yankcs = z.cs;
    pastebuf(z, &buf, n, position);
    0
}

pub fn viputbefore(z: &mut Zle, _args: &[String]) -> i32 {
    viput(z, 0)
}

pub fn viputafter(z: &mut Zle, _args: &[String]) -> i32 {
    viput(z, 1)
}

pub fn putreplaceselection(z: &mut Zle, _args: &[String]) -> i32 {
    let n = z.zmod.mult;
    let mut pos = 2;
    if z.invicmdmode() {
        vi::startvichange(z, -1);
    }
    let isvibuf = z.zmod.flags & MOD_VIBUF != 0;
    if n < 0 || z.zmod.flags & MOD_NULL != 0 {
        return 1;
    }
    if z.zmod.flags & MOD_OSSEL != 0 {
        // No system clipboard to read from.
        return 1;
    }
    let putbuf = if isvibuf {
        &mut z.vibuf[z.zmod.vibuf]
    } else {
        &mut z.cutbuf
    };
    if !putbuf.set {
        return 1;
    }
    let prevbuf = putbuf.clone();
    // zsh hands "9 to the paste and leaves the register empty.
    if z.zmod.vibuf == 35 {
        *putbuf = CutBuffer::default();
    }

    z.zmod.flags = 0; // flags apply to paste not kill
    if z.region_active == 2 && prevbuf.flags & CUTBUFFER_LINE != 0 {
        let (_, b) = regionlines(z);
        pos = i32::from(b == z.ll());
    }
    killregion(z, &[]);
    pastebuf(z, &prevbuf, n, pos);
    0
}

pub fn yankpop(z: &mut Zle, _args: &[String]) -> i32 {
    let kctstart = z.kct;
    let Some(k) = kctbuf().filter(|_| z.lastcmd & ZLE_YANK != 0 && !z.kring.is_empty()) else {
        set_kctbuf(None);
        return 1;
    };
    let buf = loop {
        // The loop runs original buffer -> kill ring in order ->
        // original buffer, starting the ring at kringnum.
        z.kct = match z.kct {
            None => Some(z.kringnum),
            Some(kct) => {
                let kctnew = (kct + z.kringsize - 1) % z.kringsize;
                if kctnew == z.kringnum {
                    None
                } else {
                    Some(kctnew)
                }
            }
        };
        let buf = match z.kct {
            None => kctbuf_contents(z, k),
            Some(i) => z.kring[i].clone(),
        };
        if z.kct == kctstart {
            return 1;
        }
        if buf.set && buf.buf.first().is_some_and(|&c| c != '\0') {
            break buf;
        }
    };
    z.cs = z.yankb;
    let ct = z.yanke - z.yankb;
    z.foredel(ct, CUT_RAW);
    z.cs = z.yankcs;
    let position = i32::from(z.lastcmd & ZLE_YANKAFTER != 0);
    pastebuf(z, &buf, 1, position);
    0
}

/// Reads pasted text up to the end-of-paste sequence.
pub fn bracketedstring(z: &mut Zle) -> String {
    const ENDESC: &[u8] = b"\x1b[201~";
    let mut endpos = 0;
    let mut pbuf: Vec<u8> = Vec::new();
    while endpos < ENDESC.len() {
        let Ok(next) = z.getbyte(true) else {
            break;
        };
        if endpos == 0 {
            endpos = usize::from(next == ENDESC[0]);
        } else {
            let matched = next == ENDESC[endpos];
            endpos += 1;
            if !matched {
                endpos = usize::from(next == ENDESC[0]);
            }
        }
        pbuf.push(if next == b'\r' { b'\n' } else { next });
    }
    pbuf.truncate(pbuf.len() - endpos);
    String::from_utf8_lossy(&pbuf).into_owned()
}

/// zsh's quotestring() with QT_SINGLE_OPTIONAL: single quotes only
/// around the parts that need them.
fn quotestring_single_optional(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let rcquotes = crate::options::is_set("rcquotes");
    let magicequal = crate::options::is_set("magicequalsubst");
    let extglob = crate::options::is_set("extendedglob");
    let bang = crate::options::is_set("banghist");
    let chars: Vec<char> = s.chars().collect();
    let mut v: Vec<char> = Vec::new();
    let mut quotesub = 1;
    let mut quotestart = 0;
    for (i, &u) in chars.iter().enumerate() {
        let ispecial = "#$^*()=|{}[]`<>?~;&\n\t \\'\"".contains(u) || (bang && u == '!');
        let special = ispecial
            && ((u != '=' && u != '~')
                || i == 0
                || (magicequal && (chars[i - 1] == '=' || chars[i - 1] == ':'))
                || (u == '~' && extglob));
        if !special {
            v.push(u);
            continue;
        }
        if quotesub == 1 {
            if u == '\'' {
                v.push('\\');
            } else {
                v.insert(quotestart, '\'');
                quotesub = 2;
            }
            v.push(u);
            quotestart = v.len();
        } else if u == '\'' {
            if !rcquotes {
                v.extend(['\'', '\\', '\'']);
                quotesub = 1;
                quotestart = v.len();
            } else {
                v.extend(['\'', '\'']);
            }
        } else {
            v.push(u);
        }
    }
    if quotesub == 2 {
        v.push('\'');
    }
    v.into_iter().collect()
}

pub fn bracketedpaste(z: &mut Zle, args: &[String]) -> i32 {
    let pbuf = bracketedstring(z);
    if let Some(name) = args.first() {
        z.shell(|| crate::shell::var_set(name, &pbuf, false));
    } else {
        let wpaste: Vec<char> = if z.zmod.mult == 1 {
            pbuf.chars().collect()
        } else {
            quotestring_single_optional(&pbuf).chars().collect()
        };
        z.cuttext(&wpaste, CUT_REPLACE);
        if z.zmod.flags & MOD_VIBUF == 0 {
            z.kct = None;
            set_kctbuf(Some(KctBuf::Cut));
            z.zmod.mult = 1;
            if z.region_active != 0 {
                killregion(z, &[]);
            }
            z.yankb = z.cs;
            z.yankcs = z.cs;
            doinsert(z, &wpaste);
            z.yanke = z.cs;
        }
    }
    0
}

pub fn overwritemode(z: &mut Zle, _args: &[String]) -> i32 {
    z.insmode = !z.insmode;
    0
}

pub fn whatcursorposition(z: &mut Zle, _args: &[String]) -> i32 {
    let bol = z.findbol();
    let mut msg = String::new();
    if z.cs == z.ll() {
        msg.push_str("EOF");
    } else {
        let c = z.line[z.cs];
        msg.push_str("Char: ");
        match c {
            ' ' => msg.push_str("SPC"),
            '\t' => msg.push_str("TAB"),
            '\n' => msg.push_str("LFD"),
            _ => msg.push(c),
        }
        let u = c as u32;
        msg.push_str(&format!(" (0{:o}, {}, 0x{:x})", u, u, u));
    }
    let ll = z.ll();
    msg.push_str(&format!(
        "  point {} of {}({}%)  column {}",
        z.cs + 1,
        ll + 1,
        (100 * z.cs).checked_div(ll).unwrap_or(0),
        z.cs - bol
    ));
    z.showmsg(&msg);
    0
}

pub fn undefinedkey(_z: &mut Zle, _args: &[String]) -> i32 {
    1
}

pub fn quotedinsert(z: &mut Zle, args: &[String]) -> i32 {
    if z.getfullchar(false).is_none() {
        1
    } else {
        selfinsert(z, args)
    }
}

fn parsedigit(z: &Zle, inkey: i32) -> i32 {
    let base = z.zmod.base as i32;
    let a = b'a' as i32;
    let ua = b'A' as i32;
    let zero = b'0' as i32;
    if base > 10 {
        if inkey >= a && inkey < a + base - 10 {
            return inkey - a + 10;
        }
        if inkey >= ua && inkey < ua + base - 10 {
            return inkey - ua + 10;
        }
        if (zero..=b'9' as i32).contains(&inkey) {
            return inkey - zero;
        }
        return -1;
    }
    if inkey >= zero && inkey < zero + base {
        return inkey - zero;
    }
    -1
}

pub fn digitargument(z: &mut Zle, _args: &[String]) -> i32 {
    let sign: i64 = if z.zmod.mult < 0 { -1 } else { 1 };
    let newdigit = parsedigit(z, z.lastchar);
    if newdigit < 0 {
        return 1;
    }
    if z.zmod.flags & MOD_TMULT == 0 {
        z.zmod.tmult = 0;
    }
    if z.zmod.flags & MOD_NEG != 0 {
        // After a negative argument this is the digit, not the -1
        // negargument() assumed.
        z.zmod.tmult = sign * newdigit as i64;
        z.zmod.flags &= !MOD_NEG;
    } else {
        z.zmod.tmult = z.zmod.tmult * z.zmod.base as i64 + sign * newdigit as i64;
    }
    z.zmod.flags |= MOD_TMULT;
    z.prefixflag = true;
    0
}

pub fn negargument(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.flags & MOD_TMULT != 0 {
        return 1;
    }
    z.zmod.tmult = -1;
    z.zmod.flags |= MOD_TMULT | MOD_NEG;
    z.prefixflag = true;
    0
}

/// C's atoi().
fn atoi(s: &str) -> i64 {
    let s = s.trim_start();
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut n: i64 = 0;
    for b in s.bytes().take_while(u8::is_ascii_digit) {
        n = n.wrapping_mul(10).wrapping_add((b - b'0') as i64);
    }
    if neg {
        -n
    } else {
        n
    }
}

/// zsh's zstrtol() with base 0: 0x for hex, 0b for binary, 0 for octal.
fn zstrtol0(s: &str) -> i64 {
    let s = s.trim_start_matches([' ', '\t']);
    let (neg, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let b = s.as_bytes();
    let (base, digits) = if b.first() != Some(&b'0') {
        (10, s)
    } else if matches!(b.get(1), Some(b'x' | b'X')) {
        (16, &s[2..])
    } else if matches!(b.get(1), Some(b'b' | b'B')) {
        (2, &s[2..])
    } else {
        (8, &s[1..])
    };
    let mut n: i64 = 0;
    for c in digits.chars() {
        match c.to_digit(base) {
            Some(d) => n = n.wrapping_mul(base as i64).wrapping_add(d as i64),
            None => break,
        }
    }
    if neg {
        -n
    } else {
        n
    }
}

pub fn universalargument(z: &mut Zle, args: &[String]) -> i32 {
    let mut digcnt = 0;
    let mut pref: i64 = 0;
    let mut minus: i64 = 1;
    if let Some(a) = args.first() {
        z.zmod.mult = atoi(a);
        z.zmod.flags |= MOD_MULT;
        return 0;
    }
    // Byte by byte: what follows the digits may not be a whole character.
    while let Ok(gotk) = z.getbyte(false) {
        if gotk == b'-' && digcnt == 0 {
            minus = -1;
            digcnt += 1;
        } else {
            let newdigit = parsedigit(z, gotk as i32);
            if newdigit >= 0 {
                pref = pref * z.zmod.base as i64 + newdigit as i64;
                digcnt += 1;
            } else {
                z.ungetbyte(gotk);
                break;
            }
        }
    }
    if digcnt != 0 {
        z.zmod.tmult = minus * if pref != 0 { pref } else { 1 };
    } else {
        z.zmod.tmult *= 4;
    }
    z.zmod.flags |= MOD_TMULT;
    z.prefixflag = true;
    0
}

pub fn argumentbase(z: &mut Zle, args: &[String]) -> i32 {
    let multbase = match args.first() {
        Some(a) => zstrtol0(a),
        None => z.zmod.mult,
    };
    if !(2..=36).contains(&multbase) {
        return 1;
    }
    z.zmod.base = multbase as u32;
    // Reset the modifier apart from the base...
    z.zmod.flags = 0;
    z.zmod.mult = 1;
    z.zmod.tmult = 1;
    z.zmod.vibuf = 0;
    // ...but we are still reading a prefix argument.
    z.prefixflag = true;
    0
}

fn ziword(c: char) -> bool {
    c.is_alphanumeric()
        || crate::shell::var_get_opt("WORDCHARS")
            .unwrap_or_default()
            .contains(c)
}

pub fn copyprevword(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.mult <= 0 {
        return 1;
    }
    let mut t0 = z.cs;
    let mut count = z.zmod.mult;
    let t1 = loop {
        let t1 = t0;
        while t0 != 0 && !ziword(z.line[t0 - 1]) {
            t0 -= 1;
        }
        while t0 != 0 && ziword(z.line[t0 - 1]) {
            t0 -= 1;
        }
        count -= 1;
        if count == 0 {
            break t1;
        }
        if t0 == 0 {
            return 1;
        }
    };
    let text = z.line[t0..t1].to_vec();
    z.insert_chars(&text);
    0
}

/// zsh's bufferwords() on the edit buffer: its shell words, and the
/// index of the word the cursor is in (-1 when there are none).
pub fn bufferwords(z: &Zle) -> (Vec<String>, isize) {
    let pre: String = if !z.isfirstln {
        crate::zle::prebuffer()
    } else {
        String::new()
    };
    let off = pre.chars().count();
    let mut text = pre;
    text.extend(z.line.iter());
    // zsh lexes the line with a space added.
    text.push(' ');
    let chars: Vec<char> = text.chars().collect();
    let cs = (z.cs + off).saturating_sub(1);
    let mut lx = crate::lexer::Lexer::new(&text, true);
    lx.comments = crate::options::is_set("interactivecomments");
    let mut list = Vec::new();
    let mut cur: isize = -1;
    let mut got = false;
    loop {
        let before = lx.pos();
        let tok = match lx.next_token() {
            Ok(crate::lexer::Tok::Eof) | Err(_) => break,
            Ok(t) => t,
        };
        let after = lx.pos();
        if matches!(
            tok,
            crate::lexer::Tok::Word(_) | crate::lexer::Tok::IoVar(_) | crate::lexer::Tok::Arith(_)
        ) {
            let mut s = before;
            loop {
                if s < after && matches!(chars[s], ' ' | '\t') {
                    s += 1;
                } else if s + 1 < after && chars[s] == '\\' && chars[s + 1] == '\n' {
                    s += 2;
                } else {
                    break;
                }
            }
            let mut w: String = chars[s..after].iter().collect();
            // Read past the added space: drop it again.
            if after == chars.len() && w.ends_with(' ') {
                w.pop();
            }
            let lexed_cursor = !got && cs <= after + 1;
            if !w.is_empty() {
                list.push(w);
            }
            if lexed_cursor {
                got = true;
                cur = list.len() as isize - 1;
            }
        }
    }
    if cur < 0 && !list.is_empty() {
        cur = list.len() as isize - 1;
    }
    (list, cur)
}

pub fn copyprevshellword(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.mult <= 0 {
        return 1;
    }
    let (l, i) = bufferwords(z);
    let i = i as i64 - (z.zmod.mult - 1);
    if i < 0 {
        return 1;
    }
    if let Some(p) = l.get(i as usize) {
        let lineadd: Vec<char> = p.chars().collect();
        z.insert_chars(&lineadd);
    }
    0
}

pub fn sendbreak(z: &mut Zle, _args: &[String]) -> i32 {
    z.errflag = true;
    1
}

pub fn quoteregion(z: &mut Zle, _args: &[String]) -> i32 {
    let mut extra = z.invicmdmode();
    if z.mark > z.ll() {
        z.mark = z.ll();
    }
    if z.region_active == 2 {
        let (a, b) = regionlines(z);
        z.cs = a;
        z.mark = b;
        extra = false;
    } else if z.mark < z.cs {
        std::mem::swap(&mut z.mark, &mut z.cs);
    }
    if extra {
        z.mark += 1;
    }
    let len = z.mark - z.cs;
    let str = z.line[z.cs..z.cs + len].to_vec();
    z.foredel(len, CUT_RAW);
    let str = makequote(&str);
    z.spaceinline(str.len());
    let cs = z.cs;
    z.line[cs..cs + str.len()].copy_from_slice(&str);
    z.mark = cs;
    z.cs = cs + str.len();
    0
}

pub fn quoteline(z: &mut Zle, _args: &[String]) -> i32 {
    z.line = makequote(&z.line);
    z.cs = z.ll();
    0
}

fn makequote(s: &[char]) -> Vec<char> {
    let mut l = Vec::with_capacity(s.len() + 2);
    l.push('\'');
    for &c in s {
        if c == '\'' {
            l.extend(['\'', '\\', '\'', '\'']);
        } else {
            l.push(c);
        }
    }
    l.push('\'');
    l
}

const NAMLEN: usize = 60;

// Leaves the minibuffer: restores the keymap and the list state.
fn endnamedcommand(z: &mut Zle, okeymap: &str, ols: bool, listed: bool) {
    z.statusline = None;
    z.selectkeymap(okeymap, true);
    z.refresh.listshown = ols;
    if ols {
        z.showinglist = -2;
    } else if listed {
        z.clearlist = true;
        z.refresh.listshown = true;
    }
}

fn listnamed(z: &mut Zle, names: &[String]) {
    let zmultsav = z.zmod.mult;
    z.zmod.mult = 1;
    complete::listlist(z, names);
    z.showinglist = 0;
    z.zmod.mult = zmultsav;
}

/// Reads a widget name in the minibuffer (execute-named-cmd).
pub fn executenamedcommand(z: &mut Zle, prmt: &str) -> Option<String> {
    let mut feep = false;
    let mut listed = false;
    let mut curlist = false;
    let ols = z.refresh.listshown;
    let okeymap = z.curkeymapname.clone();
    let mut namedcmdll: Vec<String> = Vec::new();
    let mut cmdbuf = String::new();

    z.clearlist = true;
    z.selectlocalmap(keymap::open_keymap("command"));
    z.selectkeymap("main", true);
    let retval = loop {
        z.statusline = Some(format!("{}{}_", prmt, cmdbuf));
        z.zrefresh();
        let cmd = match z.getkeycmd() {
            Some(c) if c != "send-break" => c,
            _ => {
                endnamedcommand(z, &okeymap, ols, listed);
                break None;
            }
        };
        let len = cmdbuf.len();
        if cmd == "clear-screen" {
            z.clearscreen();
            if curlist {
                listnamed(z, &namedcmdll);
            }
        } else if cmd == "redisplay" {
            z.redisplay();
            if curlist {
                listnamed(z, &namedcmdll);
            }
        } else if cmd == "vi-quoted-insert" {
            z.statusline = Some(format!("{}{}^", prmt, cmdbuf));
            z.zrefresh();
            match z.getfullchar(false) {
                Some(c) if c != '\0' && len < NAMLEN => {
                    cmdbuf.push(c);
                    curlist = false;
                }
                _ => feep = true,
            }
        } else if cmd == "quoted-insert" {
            match z.getfullchar(false) {
                Some(c) if c != '\0' && len != NAMLEN => {
                    cmdbuf.push(c);
                    curlist = false;
                }
                _ => feep = true,
            }
        } else if cmd == "backward-delete-char" || cmd == "vi-backward-delete-char" {
            if len != 0 {
                cmdbuf.pop();
                curlist = false;
            }
        } else if cmd == "kill-region"
            || cmd == "backward-kill-word"
            || cmd == "vi-backward-kill-word"
        {
            if len != 0 {
                curlist = false;
            }
            while let Some(cc) = cmdbuf.pop() {
                if cc == '-' {
                    break;
                }
            }
        } else if cmd == "kill-whole-line" || cmd == "vi-kill-line" || cmd == "backward-kill-line" {
            cmdbuf.clear();
            if listed {
                z.clearlist = true;
                z.refresh.listshown = true;
            }
            curlist = false;
        } else if cmd == "bracketed-paste" {
            let insert = bracketedstring(z);
            if len + insert.len() > NAMLEN {
                feep = true;
            } else {
                cmdbuf.push_str(&insert);
                if listed {
                    z.clearlist = true;
                    z.refresh.listshown = true;
                    listed = false;
                } else {
                    curlist = false;
                }
            }
        } else {
            let accepting = cmd == "accept-line" || cmd == "vi-cmd-mode";
            if accepting && thingy::lookup(&cmdbuf).is_some() {
                endnamedcommand(z, &okeymap, ols, listed);
                break Some(cmdbuf);
            }
            let mut cmd = cmd;
            if cmd == "self-insert-unmeta" {
                fixunmeta(z);
                cmd = "self-insert".into();
            }
            if matches!(
                cmd.as_str(),
                "list-choices"
                    | "delete-char-or-list"
                    | "expand-or-complete"
                    | "complete-word"
                    | "expand-or-complete-prefix"
                    | "vi-cmd-mode"
                    | "accept-line"
            ) || z.lastchar == b' ' as i32
                || z.lastchar == b'\t' as i32
            {
                namedcmdll = thingy::names()
                    .into_iter()
                    .filter(|n| n.starts_with(cmdbuf.as_str()))
                    .collect();
                let namedcmdambig = namedcmdll
                    .iter()
                    .map(|n| {
                        n.bytes()
                            .zip(namedcmdll[0].bytes())
                            .take_while(|(a, b)| a == b)
                            .count()
                    })
                    .min()
                    .unwrap_or(100)
                    .min(100);
                if namedcmdll.is_empty() {
                    feep = true;
                    if listed {
                        z.clearlist = true;
                        z.refresh.listshown = true;
                    }
                    curlist = false;
                } else if cmd == "list-choices" || cmd == "delete-char-or-list" {
                    listnamed(z, &namedcmdll);
                    listed = true;
                    curlist = true;
                } else if namedcmdll.len() == 1 {
                    cmdbuf = namedcmdll[0].clone();
                    if (cmd == "accept-line" || cmd == "vi-cmd-mode")
                        && thingy::lookup(&cmdbuf).is_some()
                    {
                        endnamedcommand(z, &okeymap, ols, listed);
                        break Some(cmdbuf);
                    }
                } else {
                    let mut ambig = namedcmdambig;
                    while !namedcmdll[0].is_char_boundary(ambig) {
                        ambig -= 1;
                    }
                    cmdbuf = namedcmdll[0][..ambig].to_string();
                    if crate::options::is_set("autolist")
                        && !(crate::options::is_set("listambiguous") && namedcmdambig > len)
                    {
                        if crate::options::is_set("listbeep") {
                            feep = true;
                        }
                        listnamed(z, &namedcmdll);
                        listed = true;
                        curlist = true;
                    }
                }
            } else if len == NAMLEN || cmd != "self-insert" {
                feep = true;
            } else {
                if !z.lastchar_wide_valid {
                    z.getrestchar(z.lastchar as u8);
                }
                match z.lastchar_wide {
                    Some(c) if !c.is_control() => {
                        cmdbuf.push(c);
                        if listed {
                            z.clearlist = true;
                            z.refresh.listshown = true;
                            listed = false;
                        } else {
                            curlist = false;
                        }
                    }
                    _ => feep = true,
                }
            }
        }
        if feep {
            utils::handlefeep(z, &[]);
        }
        feep = false;
    };
    z.selectlocalmap(None);
    retval
}
