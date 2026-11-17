//! Line editing primitives, after zsh's zle_utils.c: inserting and
//! removing text, cut buffers and the kill ring, undo, messages.

use super::*;

/// The auto-removable suffix completion leaves (zsh's suffixlen etc.).
#[derive(Clone, Debug, Default)]
pub struct Suffix {
    pub start: usize,
    pub end: usize,
    pub active: bool,
    pub func: Option<String>,
    pub chars: String,
    pub remove_on_space: bool,
}

/// Pushes a line for the next read to start with (push-line, print -z).
pub fn push_bufstack(s: String) {
    crate::print::push_buffer_stack(s);
}

pub fn take_bufstack() -> Option<String> {
    crate::print::pop_buffer_stack()
}

pub fn bufstack_len() -> usize {
    crate::print::buffer_stack_len()
}

impl Zle {
    /// Inserts room for `ct` characters at the cursor.
    pub fn spaceinline(&mut self, ct: usize) {
        let cs = self.cs;
        self.line.splice(cs..cs, std::iter::repeat_n(' ', ct));
        if self.mark > cs {
            self.mark += ct;
        }
        if self.viinsbegin > cs {
            self.viinsbegin = 0;
        }
        let pre = self.predisplay.len() as i64;
        for rh in self.region_highlights.iter_mut() {
            let sub = if rh.predisplay { pre } else { 0 };
            if rh.start - sub >= cs as i64 {
                rh.start += ct as i64;
            }
            if rh.end - sub >= cs as i64 && (pre == 0 || cs != 0) {
                rh.end += ct as i64;
            }
        }
        self.region_active = 0;
    }

    /// Removes `cnt` characters at position `to`.
    pub fn shiftchars(&mut self, to: usize, cnt: usize) {
        if self.mark >= to + cnt {
            self.mark -= cnt;
        } else if self.mark > to {
            self.mark = to;
        }
        let pre = self.predisplay.len() as i64;
        let (t, c) = (to as i64, cnt as i64);
        for rh in self.region_highlights.iter_mut() {
            let sub = if rh.predisplay { pre } else { 0 };
            if rh.start - sub > t {
                if rh.start - sub > t + c {
                    rh.start -= c;
                } else {
                    rh.start = t + sub;
                }
            }
            if rh.end - sub > t {
                if rh.end - sub > t + c {
                    rh.end -= c;
                } else {
                    rh.end = t + sub;
                }
            }
        }
        let end = (to + cnt).min(self.line.len());
        if to < end {
            self.line.drain(to..end);
        }
        self.region_active = 0;
    }

    /// Puts the `ct` characters at `i` in the cut buffer.
    pub fn cut(&mut self, i: usize, ct: usize, flags: u32) {
        let end = (i + ct).min(self.line.len());
        let text: Vec<char> = self.line[i.min(end)..end].to_vec();
        self.cuttext(&text, flags);
    }

    pub fn cuttext(&mut self, line: &[char], flags: u32) {
        let ct = line.len();
        if (ct == 0 && !self.vilinerange) || self.zmod.flags & MOD_NULL != 0 {
            return;
        }
        let lineflag = if self.vilinerange { CUTBUFFER_LINE } else { 0 };
        if self.zmod.flags & MOD_OSSEL != 0 {
            // The system clipboard, through the terminal (OSC 52).
            let clip = if self.zmod.flags & MOD_CLIP != 0 {
                'c'
            } else {
                'p'
            };
            termctl::clip_put(clip, &line.iter().collect::<String>());
        } else if self.zmod.flags & MOD_VIBUF != 0 {
            let vl = self.vilinerange;
            let b = &mut self.vibuf[self.zmod.vibuf];
            if self.zmod.flags & MOD_VIAPP == 0 || !b.set {
                b.buf = line.to_vec();
                b.flags = lineflag;
                b.set = true;
            } else {
                if vl {
                    b.flags |= CUTBUFFER_LINE;
                }
                if b.flags & CUTBUFFER_LINE != 0 {
                    b.buf.push('\n');
                }
                b.buf.extend_from_slice(line);
            }
        } else if flags & CUT_YANK != 0 {
            self.vibuf[26] = CutBuffer {
                buf: line.to_vec(),
                flags: lineflag,
                set: true,
            };
        } else {
            for n in (28..=35).rev() {
                self.vibuf[n] = self.vibuf[n - 1].clone();
            }
            self.vibuf[27] = CutBuffer {
                buf: line.to_vec(),
                flags: lineflag,
                set: true,
            };
        }
        if !self.cutbuf.set {
            self.cutbuf = CutBuffer {
                buf: Vec::new(),
                flags: 0,
                set: true,
            };
        } else if self.lastcmd & ZLE_KILL == 0 || flags & CUT_REPLACE != 0 {
            if self.kring.is_empty() {
                self.kringsize = KRINGCTDEF;
                self.kring = vec![CutBuffer::default(); KRINGCTDEF];
            } else {
                self.kringnum = (self.kringnum + 1) % self.kringsize;
            }
            let old = std::mem::replace(
                &mut self.cutbuf,
                CutBuffer {
                    buf: Vec::new(),
                    flags: 0,
                    set: true,
                },
            );
            self.kring[self.kringnum] = old;
        }
        if flags & (CUT_FRONT | CUT_REPLACE) != 0 {
            let mut s = line.to_vec();
            s.extend_from_slice(&self.cutbuf.buf);
            self.cutbuf.buf = s;
        } else {
            self.cutbuf.buf.extend_from_slice(line);
        }
        if self.vilinerange {
            self.cutbuf.flags |= CUTBUFFER_LINE;
        } else {
            self.cutbuf.flags &= !CUTBUFFER_LINE;
        }
    }

    pub fn backkill(&mut self, ct: usize, flags: u32) {
        let ct = ct.min(self.cs);
        self.cs -= ct;
        let cs = self.cs;
        self.cut(cs, ct, flags);
        self.shiftchars(cs, ct);
    }

    pub fn forekill(&mut self, ct: usize, flags: u32) {
        let i = self.cs;
        let ct = ct.min(self.line.len() - i);
        self.cut(i, ct, flags);
        self.shiftchars(i, ct);
    }

    pub fn backdel(&mut self, ct: usize, _flags: u32) {
        let ct = ct.min(self.cs);
        self.cs -= ct;
        let cs = self.cs;
        self.shiftchars(cs, ct);
    }

    pub fn foredel(&mut self, ct: usize, _flags: u32) {
        let cs = self.cs;
        let ct = ct.min(self.line.len().saturating_sub(cs));
        self.shiftchars(cs, ct);
    }

    pub fn setline(&mut self, s: &str, flags: u32) {
        self.viinsbegin = 0;
        self.line = s.chars().collect();
        if flags & ZSL_TOEND != 0 {
            self.cs = self.line.len();
            if self.cs > 0 && self.invicmdmode() {
                self.cs -= 1;
            }
        } else if self.cs > self.line.len() {
            self.cs = self.line.len();
        }
    }

    pub fn findbol(&self) -> usize {
        self.findbol_at(self.cs)
    }

    pub fn findbol_at(&self, mut x: usize) -> usize {
        while x > 0 && self.line[x - 1] != '\n' {
            x -= 1;
        }
        x
    }

    pub fn findeol(&self) -> usize {
        self.findeol_at(self.cs)
    }

    pub fn findeol_at(&self, mut x: usize) -> usize {
        while x < self.line.len() && self.line[x] != '\n' {
            x += 1;
        }
        x
    }

    pub fn findline(&self) -> (usize, usize) {
        (self.findbol(), self.findeol())
    }

    /// Inserts `s` at the cursor, leaving the cursor after it.
    pub fn insert_chars(&mut self, s: &[char]) {
        self.spaceinline(s.len());
        let cs = self.cs;
        self.line[cs..cs + s.len()].copy_from_slice(s);
        self.cs += s.len();
    }

    /// Asks a yes-or-no question already shown; true for yes.
    pub fn getzlequery(&mut self) -> bool {
        let mut n: libc::c_int = 0;
        unsafe {
            libc::ioctl(self.fd, libc::FIONREAD, &mut n);
        }
        if n > 0 {
            refresh::write_tty("n");
            return false;
        }
        let c = self.getfullchar(false);
        let c = match c {
            Some('\t') => 'y',
            Some(c) if c.is_control() => 'n',
            None => 'n',
            Some(c) => c.to_lowercase().next().unwrap_or(c),
        };
        if c != '\n' {
            refresh::write_tty(&c.to_string());
        }
        c == 'y'
    }

    /// Shows `msg` below the line.
    pub fn showmsg(&mut self, msg: &str) {
        if !zle_active() {
            crate::sys::err_line(msg);
            return;
        }
        self.trash();
        let mut out = String::new();
        for c in msg.chars() {
            if c == '\n' {
                out.push('\n');
            } else {
                out.push_str(&nicechar(c));
            }
        }
        out.push('\n');
        refresh::write_tty(&out);
        self.showinglist = 0;
    }

    // ---- undo -----------------------------------------------------------

    pub fn initundo(&mut self) {
        self.nextchanges.clear();
        self.changes.clear();
        self.ucur = 0;
        self.undo_changeno = 0;
        self.undo_limitno = 0;
        self.lastline = self.line.clone();
        self.lastcs = self.cs;
    }

    pub fn handleundo(&mut self) {
        self.mkundoent();
        if !self.nextchanges.is_empty() {
            self.setlastline();
            self.changes.truncate(self.ucur);
            let next = std::mem::take(&mut self.nextchanges);
            self.changes.extend(next);
            self.ucur = self.changes.len();
        }
    }

    pub fn mkundoent(&mut self) {
        if self.lastline == self.line {
            self.lastcs = self.cs;
            return;
        }
        let (ll, lastll) = (self.line.len(), self.lastline.len());
        let sh = ll.min(lastll);
        let mut pre = 0;
        while pre < sh && self.line[pre] == self.lastline[pre] {
            pre += 1;
        }
        let mut suf = 0;
        while suf < sh - pre && self.line[ll - 1 - suf] == self.lastline[lastll - 1 - suf] {
            suf += 1;
        }
        let del = self.lastline[pre..lastll - suf].to_vec();
        let ins = self.line[pre..ll - suf].to_vec();
        self.undo_changeno += 1;
        let mut ch = Change {
            flags: 0,
            hist: self.histline,
            off: pre,
            del,
            ins,
            old_cs: self.lastcs,
            new_cs: self.cs,
            changeno: self.undo_changeno,
        };
        if let Some(last) = self.nextchanges.last_mut() {
            ch.flags = CH_PREV;
            last.flags |= CH_NEXT;
        }
        self.nextchanges.push(ch);
    }

    pub fn setlastline(&mut self) {
        self.lastline = self.line.clone();
        self.lastcs = self.cs;
    }

    fn unapplychange(&mut self, i: usize) -> bool {
        let ch = self.changes[i].clone();
        if ch.hist != self.histline {
            if !widgets::hist::zle_goto_hist(self, ch.hist) {
                return true;
            }
            self.cs = ch.new_cs;
            return false;
        }
        self.cs = ch.off;
        if !ch.ins.is_empty() {
            self.foredel(ch.ins.len(), CUT_RAW);
        }
        if !ch.del.is_empty() {
            self.insert_chars(&ch.del);
        }
        self.cs = ch.old_cs.min(self.line.len());
        true
    }

    fn applychange(&mut self, i: usize) -> bool {
        let ch = self.changes[i].clone();
        if ch.hist != self.histline {
            if !widgets::hist::zle_goto_hist(self, ch.hist) {
                return true;
            }
            self.cs = ch.old_cs;
            return false;
        }
        self.cs = ch.off;
        if !ch.del.is_empty() {
            self.foredel(ch.del.len(), CUT_RAW);
        }
        if !ch.ins.is_empty() {
            self.insert_chars(&ch.ins);
        }
        self.cs = ch.new_cs.min(self.line.len());
        true
    }

    pub fn undo(&mut self, args: &[String]) -> i32 {
        let last_change: i64 = match args.first() {
            Some(a) => crate::arith::eval(a).unwrap_or(0),
            None => -1,
        };
        self.handleundo();
        loop {
            if self.ucur == 0 {
                return 1;
            }
            let prev = self.ucur - 1;
            if self.changes[prev].changeno <= last_change {
                break;
            }
            if self.changes[prev].changeno <= self.undo_limitno && args.is_empty() {
                return 1;
            }
            if !self.unapplychange(prev) {
                if last_change >= 0 {
                    self.unapplychange(prev);
                    self.ucur = prev;
                }
            } else {
                self.ucur = prev;
            }
            if !(last_change >= 0 || self.changes[self.ucur].flags & CH_PREV != 0) {
                break;
            }
        }
        self.setlastline();
        0
    }

    pub fn redo(&mut self) -> i32 {
        self.handleundo();
        loop {
            if self.ucur >= self.changes.len() {
                return 1;
            }
            if self.applychange(self.ucur) {
                self.ucur += 1;
            } else {
                break;
            }
            if self.changes[self.ucur - 1].flags & CH_NEXT == 0 {
                break;
            }
        }
        self.setlastline();
        0
    }

    pub fn viundochange(&mut self, args: &[String]) -> i32 {
        self.handleundo();
        if self.ucur < self.changes.len() {
            while self.ucur < self.changes.len() {
                self.applychange(self.ucur);
                self.ucur += 1;
            }
            self.setlastline();
            0
        } else {
            self.undo(args)
        }
    }

    pub fn splitundo(&mut self) -> i32 {
        if self.vistartchange >= 0 {
            self.mergeundo();
            self.vistartchange = self.undo_changeno;
        }
        self.handleundo();
        0
    }

    pub fn mergeundo(&mut self) {
        let mut i = self.ucur;
        while i >= 2 && self.changes[i - 1].changeno > self.vistartchange + 1 {
            self.changes[i - 1].flags |= CH_PREV;
            self.changes[i - 2].flags |= CH_NEXT;
            i -= 1;
        }
        self.vistartchange = -1;
    }

    /// The number of the last change, after recording pending ones.
    pub fn undo_current_change(&mut self) -> i64 {
        self.mkundoent();
        self.setlastline();
        self.undo_changeno
    }

    // ---- suffixes and lists (completion) ---------------------------------

    pub fn removesuffix(&mut self) {
        self.iremovesuffix(None, false);
    }

    pub fn iremovesuffix(&mut self, c: Option<char>, keep: bool) {
        widgets::complete::iremovesuffix(self, c, keep);
    }

    pub fn fixsuffix(&mut self) {
        self.suffix = Suffix::default();
    }

    pub fn invalidatelist(&mut self) {
        widgets::complete::invalidatelist(self);
    }
}

/// A character as zsh's nicechar shows it.
pub fn nicechar(c: char) -> String {
    let u = c as u32;
    if u < 32 {
        format!("^{}", char::from_u32(u ^ 64).unwrap_or('?'))
    } else if u == 0x7f {
        "^?".into()
    } else if (0x80..0xa0).contains(&u) {
        format!("\\M-^{}", char::from_u32((u & 0x7f) ^ 64).unwrap_or('?'))
    } else {
        c.to_string()
    }
}

pub fn handlefeep(z: &mut Zle, _args: &[String]) -> i32 {
    if !crate::options::is_set("beep") {
        return 0;
    }
    let _ = z;
    refresh::write_tty("\x07");
    0
}
