//! Drawing the line, after zsh's zle_refresh.c.
//!
//! Output to the terminal is buffered, as zsh's shout is, and written
//! when a refresh ends or the display is trashed. Output from widgets
//! (print and so on) goes straight to the terminal, so it can appear
//! before display updates queued earlier, exactly as with zsh.

use super::*;
use std::cell::RefCell;

thread_local! {
    static TTYBUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static TTYFD: Cell<i32> = const { Cell::new(1) };
}

/// Queues `s` for the terminal.
pub fn write_tty(s: &str) {
    TTYBUF.with(|b| b.borrow_mut().extend_from_slice(s.as_bytes()));
}

/// Writes queued terminal output.
pub fn flush_tty() {
    let data = TTYBUF.with(|b| std::mem::take(&mut *b.borrow_mut()));
    if !data.is_empty() {
        let fd = TTYFD.with(|f| f.get());
        let _ = crate::sys::write_fd(fd, &data);
    }
}

pub fn set_tty_fd(fd: i32) {
    TTYFD.with(|f| f.set(fd));
}

/// The terminal's descriptor (zsh's SHTTY).
pub fn tty_fd() -> i32 {
    TTYFD.with(|f| f.get())
}

#[derive(Default, Debug, Clone)]
pub struct RefreshState {
    /// Rows the last display took.
    pub nlnct: usize,
    /// The row and column of the terminal cursor, from the display top.
    pub vln: usize,
    pub vcs: usize,
    /// Rows the prompt takes.
    pub lprompth: usize,
    /// Whether a completion list or message is shown below the line.
    pub listshown: bool,
}

fn columns() -> usize {
    crate::shell::terminal_size()
        .map(|(c, _)| c as usize)
        .filter(|&c| c > 0)
        .or_else(|| crate::shell::var_get_opt("COLUMNS").and_then(|c| c.parse().ok()))
        .unwrap_or(80)
}

/// The display width of `s`, leaving out escape sequences.
fn width(s: &str) -> usize {
    crate::display_width(&crate::strip_ansi(s))
}

/// How a line character is shown.
fn show_char(c: char, col: usize) -> String {
    let u = c as u32;
    if c == '\t' {
        " ".repeat(8 - col % 8)
    } else if u < 32 {
        format!("^{}", char::from_u32(u ^ 64).unwrap_or('?'))
    } else if u == 0x7f {
        "^?".into()
    } else {
        c.to_string()
    }
}

impl Zle {
    /// Moves the terminal cursor to row `ln`, column `col` of the display.
    fn moveto(&mut self, ln: usize, col: usize) {
        let r = &mut self.refresh;
        if ln < r.vln {
            write_tty(&format!("\x1b[{}A", r.vln - ln));
        } else if ln > r.vln {
            write_tty("\r");
            write_tty(&"\n".repeat(ln - r.vln));
            r.vcs = 0;
        }
        r.vln = ln;
        if col != r.vcs {
            write_tty("\r");
            if col > 0 {
                write_tty(&format!("\x1b[{}C", col));
            }
            r.vcs = col;
        }
    }

    /// Redraws the prompt and the line.
    pub fn zrefresh(&mut self) {
        if !zle_active() {
            return;
        }
        let cols = columns().max(1);
        if self.resetneeded {
            // Draw from wherever the cursor is, as a new display.
            self.refresh.vln = 0;
            self.refresh.vcs = 0;
            self.refresh.nlnct = 0;
            write_tty("\r");
        } else {
            let vln = self.refresh.vln;
            if vln > 0 {
                write_tty(&format!("\x1b[{}A", vln));
            }
            write_tty("\r");
            self.refresh.vln = 0;
            self.refresh.vcs = 0;
        }
        self.resetneeded = false;
        self.trashed = false;

        // Lay out prompt, predisplay, line and postdisplay.
        let mut out = String::new();
        let mut row = 0usize;
        let mut col = 0usize;
        let prompt = self.lprompt.clone();
        for (i, pl) in prompt.split('\n').enumerate() {
            if i > 0 {
                out.push_str("\x1b[K\n");
                row += 1;
                col = 0;
            }
            out.push_str(pl);
            let w = width(pl);
            row += (col + w) / cols;
            col = (col + w) % cols;
        }
        self.refresh.lprompth = row + 1;
        let pre = self.predisplay.clone();
        let post = self.postdisplay.clone();
        let text: Vec<char> = pre
            .iter()
            .chain(self.line.iter())
            .chain(post.iter())
            .copied()
            .collect();
        let cursor_at = pre.len() + self.cs;
        let hl = self.highlight_spans(pre.len(), text.len());
        let mut cur = (row, col);
        let mut attr_on = String::new();
        for (i, &c) in text.iter().enumerate() {
            if i == cursor_at {
                cur = (row, col);
            }
            let want = hl.get(i).cloned().unwrap_or_default();
            if want != attr_on {
                out.push_str("\x1b[0m");
                out.push_str(&want);
                attr_on = want;
            }
            if c == '\n' {
                out.push_str("\x1b[K\r\n");
                row += 1;
                col = 0;
                continue;
            }
            let s = show_char(c, col);
            let w = width(&s);
            if col + w > cols {
                out.push_str("\r\n");
                row += 1;
                col = 0;
            }
            out.push_str(&s);
            col += w;
            if col == cols {
                // Wrap now, so the next character starts the next row.
                out.push_str(" \r");
                row += 1;
                col = 0;
            }
        }
        if !attr_on.is_empty() {
            out.push_str("\x1b[0m");
        }
        if cursor_at >= text.len() {
            cur = (row, col);
        }
        // The right prompt, on the first line when it fits.
        let rp = self.rprompt.clone();
        if !rp.is_empty() && row == self.refresh.lprompth - 1 {
            let rw = width(&rp);
            if col + rw + 1 < cols {
                out.push_str(&format!("\x1b[{}C", cols - 1 - rw - col));
                out.push_str(&rp);
                out.push_str(&format!("\r\x1b[{}C", col).replace("\x1b[0C", ""));
            }
        }
        out.push_str("\x1b[K");
        let mut lastrow = row;
        if let Some(st) = self.statusline.clone() {
            out.push_str("\r\n");
            lastrow += 1;
            col = 0;
            out.push_str(&st);
            out.push_str("\x1b[K");
            col += width(&st);
            lastrow += col / cols;
            col %= cols;
        }
        out.push_str("\x1b[J");
        write_tty(&out);
        self.refresh.vln = lastrow;
        self.refresh.vcs = col;
        self.refresh.nlnct = lastrow + 1;
        self.moveto(cur.0, cur.1);
        flush_tty();
    }

    /// Escape sequences for each display position, from region_highlight
    /// and the active region.
    fn highlight_spans(&self, prelen: usize, len: usize) -> Vec<String> {
        let mut v = vec![String::new(); len];
        let mut paint = |start: i64, end: i64, attr: &str| {
            let code = attr_to_sgr(attr);
            if code.is_empty() {
                return;
            }
            let (s, e) = (start.max(0) as usize, (end.max(0) as usize).min(len));
            for slot in v.iter_mut().take(e).skip(s) {
                slot.push_str(&code);
            }
        };
        if self.region_active != 0 {
            let (mut s, mut e) = if self.cs <= self.mark {
                (self.cs, self.mark)
            } else {
                (self.mark, self.cs)
            };
            if self.region_active == 2 {
                s = self.findbol_at(s);
                e = self.findeol_at(e);
            } else if self.invicmdmode() {
                e += 1;
            }
            paint((s + prelen) as i64, (e + prelen) as i64, "standout");
        }
        for rh in &self.region_highlights {
            let off = if rh.predisplay { 0 } else { prelen as i64 };
            paint(rh.start + off, rh.end + off, &rh.attr);
        }
        v
    }

    /// Moves below the display, leaving it as it is, so other output can
    /// follow (zsh's trashzle).
    pub fn trash(&mut self) {
        if zle_active() && !self.trashed {
            self.trashed = true;
            let n = self.refresh.nlnct;
            let vln = self.refresh.vln;
            write_tty("\r");
            if n > vln {
                write_tty(&"\n".repeat(n - vln));
            }
            self.refresh.vln = n;
            self.refresh.vcs = 0;
            flush_tty();
            self.resetneeded = true;
        }
    }

    pub fn clearscreen(&mut self) {
        write_tty("\x1b[H\x1b[J");
        self.resetneeded = true;
    }

    pub fn redisplay(&mut self) {
        let vln = self.refresh.vln;
        if vln > 0 {
            write_tty(&format!("\x1b[{}A", vln));
        }
        write_tty("\r");
        self.refresh.vln = 0;
        self.refresh.vcs = 0;
        self.resetneeded = true;
    }
}

/// An SGR sequence for a zle_highlight style like `fg=red,bold`.
pub fn attr_to_sgr(attr: &str) -> String {
    let mut out = String::new();
    for part in attr.split(',') {
        let part = part.trim();
        let code = match part {
            "bold" => "1".to_string(),
            "faint" => "2".to_string(),
            "italic" => "3".to_string(),
            "underline" => "4".to_string(),
            "standout" | "reverse" => "7".to_string(),
            "none" | "" => continue,
            _ => {
                if let Some(c) = part.strip_prefix("fg=") {
                    colour(c, 30)
                } else if let Some(c) = part.strip_prefix("bg=") {
                    colour(c, 40)
                } else {
                    continue;
                }
            }
        };
        if !code.is_empty() {
            out.push_str(&format!("\x1b[{}m", code));
        }
    }
    out
}

fn colour(c: &str, base: u32) -> String {
    let names = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if let Some(i) = names.iter().position(|n| *n == c) {
        return (base + i as u32).to_string();
    }
    if c == "default" {
        return (base + 9).to_string();
    }
    if let Ok(n) = c.parse::<u32>() {
        return if n < 8 {
            (base + n).to_string()
        } else {
            format!("{};5;{}", base + 8, n)
        };
    }
    if let Some(hex) = c.strip_prefix('#') {
        if hex.len() == 6 {
            if let Ok(v) = u32::from_str_radix(hex, 16) {
                return format!("{};2;{};{};{}", base + 8, v >> 16, (v >> 8) & 255, v & 255);
            }
        }
    }
    String::new()
}

/// $region_highlight as zsh shows it.
pub fn get_region_highlight(z: &Zle) -> Vec<String> {
    z.region_highlights
        .iter()
        .map(|rh| {
            let mut s = format!(
                "{}{} {} {}",
                if rh.predisplay { "P" } else { "" },
                rh.start,
                rh.end,
                rh.attr
            );
            if let Some(m) = &rh.memo {
                s.push_str(&format!(" memo={}", m));
            }
            s
        })
        .collect()
}

/// Sets $region_highlight from its elements: `[P]start end attr [memo=x]`.
pub fn set_region_highlight(z: &mut Zle, items: &[String]) {
    z.region_highlights.clear();
    for item in items {
        let mut rest = item.trim_start();
        let predisplay = rest.starts_with('P');
        if predisplay {
            rest = &rest[1..];
        }
        let mut it = rest.split_whitespace();
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(start), Ok(end)) = (a.parse::<i64>(), b.parse::<i64>()) else {
            continue;
        };
        let attr = it.next().unwrap_or("").to_string();
        let memo = it
            .next()
            .and_then(|m| m.strip_prefix("memo="))
            .map(|m| m.split(',').next().unwrap_or("").to_string());
        z.region_highlights.push(RegionHighlight {
            start,
            end,
            attr,
            memo,
            predisplay,
        });
    }
}
