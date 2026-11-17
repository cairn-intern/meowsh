//! Port of zsh's zle_move.c.

use super::{hist, vi};
use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("backward-char", backwardchar),
    ("beginning-of-line", beginningofline),
    ("beginning-of-line-hist", beginningoflinehist),
    ("deactivate-region", deactivateregion),
    ("end-of-line", endofline),
    ("end-of-line-hist", endoflinehist),
    ("exchange-point-and-mark", exchangepointandmark),
    ("forward-char", forwardchar),
    ("set-mark-command", setmarkcommand),
    ("vi-backward-char", vibackwardchar),
    ("vi-beginning-of-line", vibeginningofline),
    ("vi-end-of-line", viendofline),
    ("vi-find-next-char", vifindnextchar),
    ("vi-find-next-char-skip", vifindnextcharskip),
    ("vi-find-prev-char", vifindprevchar),
    ("vi-find-prev-char-skip", vifindprevcharskip),
    ("vi-first-non-blank", vifirstnonblank),
    ("vi-forward-char", viforwardchar),
    ("vi-goto-column", vigotocolumn),
    ("vi-goto-mark", vigotomark),
    ("vi-goto-mark-line", vigotomarkline),
    ("vi-match-bracket", vimatchbracket),
    ("vi-repeat-find", virepeatfind),
    ("vi-rev-repeat-find", virevrepeatfind),
    ("vi-set-mark", visetmark),
    ("visual-line-mode", visuallinemode),
    ("visual-mode", visualmode),
];

fn iblank(c: char) -> bool {
    c != '\n' && c.is_whitespace()
}

pub fn beginningofline(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = endofline(z, args);
        z.zmod.mult = n;
        return ret;
    }
    for _ in 0..n {
        if z.cs == 0 {
            return 0;
        }
        let pos = z.cs - 1;
        if z.line[pos] == '\n' {
            z.cs = pos;
            if z.cs == 0 {
                return 0;
            }
        }
        while z.cs > 0 && z.line[z.cs - 1] != '\n' {
            z.cs -= 1;
        }
    }
    0
}

pub fn endofline(z: &mut Zle, args: &[String]) -> i32 {
    let n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = beginningofline(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let ll = z.line.len();
    for _ in 0..n {
        if z.cs >= ll {
            z.cs = ll;
            return 0;
        }
        z.cs += usize::from(z.invicmdmode());
        if z.cs == ll {
            break;
        }
        if z.line[z.cs] == '\n' {
            z.cs += 1;
            if z.cs == ll {
                return 0;
            }
        }
        while z.cs != ll && z.line[z.cs] != '\n' {
            z.cs += 1;
        }
    }
    0
}

pub fn beginningoflinehist(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = endoflinehist(z, args);
        z.zmod.mult = n;
        return ret;
    }
    while n != 0 {
        if z.cs == 0 {
            break;
        }
        let pos = z.cs - 1;
        if z.line[pos] == '\n' {
            z.cs = pos;
            if pos == 0 {
                break;
            }
        }
        while z.cs > 0 && z.line[z.cs - 1] != '\n' {
            z.cs -= 1;
        }
        n -= 1;
    }
    if n != 0 {
        let m = z.zmod.mult;
        z.zmod.mult = n;
        let ret = hist::uphistory(z, args);
        z.zmod.mult = m;
        z.cs = 0;
        return ret;
    }
    0
}

pub fn endoflinehist(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = beginningoflinehist(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let ll = z.line.len();
    while n != 0 {
        if z.cs >= ll {
            z.cs = ll;
            break;
        }
        z.cs += usize::from(z.invicmdmode());
        if z.cs == ll {
            break;
        }
        if z.line[z.cs] == '\n' {
            z.cs += 1;
            if z.cs == ll {
                break;
            }
        }
        while z.cs != ll && z.line[z.cs] != '\n' {
            z.cs += 1;
        }
        n -= 1;
    }
    if n != 0 {
        let m = z.zmod.mult;
        z.zmod.mult = n;
        let ret = hist::downhistory(z, args);
        z.zmod.mult = m;
        return ret;
    }
    0
}

pub fn forwardchar(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = backwardchar(z, args);
        z.zmod.mult = n;
        return ret;
    }
    while z.cs < z.line.len() && n > 0 {
        n -= 1;
        z.cs += 1;
    }
    0
}

pub fn backwardchar(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = forwardchar(z, args);
        z.zmod.mult = n;
        return ret;
    }
    while z.cs > 0 && n > 0 {
        n -= 1;
        z.cs -= 1;
    }
    0
}

pub fn setmarkcommand(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.mult < 0 {
        z.region_active = 0;
        return 0;
    }
    z.mark = z.cs;
    z.region_active = 1;
    0
}

pub fn exchangepointandmark(z: &mut Zle, _args: &[String]) -> i32 {
    if z.zmod.mult == 0 {
        z.region_active = 1;
        return 0;
    }
    std::mem::swap(&mut z.mark, &mut z.cs);
    if z.cs > z.line.len() {
        z.cs = z.line.len();
    }
    if z.zmod.mult > 0 {
        z.region_active = 1;
    }
    0
}

pub fn visualmode(z: &mut Zle, _args: &[String]) -> i32 {
    if z.virangeflag != 0 {
        z.prefixflag = true;
        z.zmod.flags &= !MOD_LINE;
        z.zmod.flags |= MOD_CHAR;
        return 0;
    }
    match z.region_active {
        1 => z.region_active = 0,
        0 => {
            z.mark = z.cs;
            z.region_active = 1;
        }
        2 => z.region_active = 1,
        _ => {}
    }
    0
}

pub fn visuallinemode(z: &mut Zle, _args: &[String]) -> i32 {
    if z.virangeflag != 0 {
        z.prefixflag = true;
        z.zmod.flags &= !MOD_CHAR;
        z.zmod.flags |= MOD_LINE;
        return 0;
    }
    match z.region_active {
        2 => z.region_active = 0,
        0 => {
            z.mark = z.cs;
            z.region_active = 2;
        }
        1 => z.region_active = 2,
        _ => {}
    }
    0
}

pub fn deactivateregion(z: &mut Zle, _args: &[String]) -> i32 {
    z.region_active = 0;
    0
}

pub fn vigotocolumn(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    let (x, y) = z.findline();

    if n >= 0 {
        if n != 0 {
            n -= 1;
        }
        z.cs = x;
        while z.cs < y && n > 0 {
            n -= 1;
            z.cs += 1;
        }
    } else {
        z.cs = y;
        n = -n;
        while z.cs > x && n > 0 {
            n -= 1;
            z.cs -= 1;
        }
    }
    0
}

pub fn vimatchbracket(z: &mut Zle, _args: &[String]) -> i32 {
    let ocs = z.cs;
    let ll = z.line.len() as isize;

    if (z.cs == z.line.len() || z.line[z.cs] == '\n') && z.cs > 0 {
        z.cs -= 1;
    }
    if z.virangeflag != 0 {
        z.mark = z.cs;
    }
    let mut c = z.cs as isize;
    let (dir, me, oth): (isize, char, char) = loop {
        if c == ll || z.line[c as usize] == '\n' {
            z.cs = ocs;
            return 1;
        }
        match z.line[c as usize] {
            '{' => break (1, '{', '}'),
            '}' => break (-1, '}', '{'),
            '(' => break (1, '(', ')'),
            ')' => break (-1, ')', '('),
            '[' => break (1, '[', ']'),
            ']' => break (-1, ']', '['),
            _ => c += 1,
        }
    };
    if z.virangeflag != 0 && dir < 0 {
        // Include the starting position when going backwards.
        z.mark += 1;
    }
    let mut ct = 1;
    while c >= 0 && c < ll && ct != 0 {
        c += dir;
        if c < 0 || c >= ll {
            break;
        }
        let ch = z.line[c as usize];
        if ch == oth {
            ct -= 1;
        } else if ch == me {
            ct += 1;
        }
    }
    if c < 0 || c >= ll {
        z.cs = ocs;
        return 1;
    } else if dir > 0 && z.virangeflag != 0 {
        c += 1;
    }
    z.cs = c as usize;
    0
}

pub fn viforwardchar(z: &mut Zle, args: &[String]) -> i32 {
    let mut lim = z.findeol() as isize;
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = vibackwardchar(z, args);
        z.zmod.mult = n;
        return ret;
    }
    if z.invicmdmode() && z.virangeflag == 0 {
        lim -= 1;
    }
    if z.cs as isize >= lim {
        return 1;
    }
    while n > 0 && (z.cs as isize) < lim {
        n -= 1;
        z.cs += 1;
    }
    0
}

pub fn vibackwardchar(z: &mut Zle, args: &[String]) -> i32 {
    let mut n = z.zmod.mult;

    if n < 0 {
        z.zmod.mult = -n;
        let ret = viforwardchar(z, args);
        z.zmod.mult = n;
        return ret;
    }
    if z.cs == z.findbol() {
        return 1;
    }
    while n > 0 && z.cs > 0 {
        n -= 1;
        z.cs -= 1;
        if z.line[z.cs] == '\n' {
            z.cs += 1;
            break;
        }
    }
    0
}

pub fn viendofline(z: &mut Zle, _args: &[String]) -> i32 {
    let oldcs = z.cs;
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
    z.cs -= 1;
    z.lastcol = 1 << 30;
    0
}

pub fn vibeginningofline(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = z.findbol();
    0
}

fn vifindstart(z: &mut Zle, dir: i32, tailadd: i32, args: &[String]) -> i32 {
    z.vfindchar = vi::vigetkey(z);
    if z.vfindchar.is_some() {
        z.vfinddir = dir;
        z.tailadd = tailadd;
        return vifindchar(z, false, args);
    }
    1
}

pub fn vifindnextchar(z: &mut Zle, args: &[String]) -> i32 {
    vifindstart(z, 1, 0, args)
}

pub fn vifindprevchar(z: &mut Zle, args: &[String]) -> i32 {
    vifindstart(z, -1, 0, args)
}

pub fn vifindnextcharskip(z: &mut Zle, args: &[String]) -> i32 {
    vifindstart(z, 1, -1, args)
}

pub fn vifindprevcharskip(z: &mut Zle, args: &[String]) -> i32 {
    vifindstart(z, -1, 1, args)
}

pub fn vifindchar(z: &mut Zle, repeat: bool, args: &[String]) -> i32 {
    let ocs = z.cs;
    let n = z.zmod.mult;
    let ll = z.line.len() as isize;

    if z.vfinddir == 0 {
        return 1;
    }
    if n < 0 {
        z.zmod.mult = -n;
        let ret = virevrepeatfind(z, args);
        z.zmod.mult = n;
        return ret;
    }
    let target = z.vfindchar;
    if repeat && z.tailadd != 0 {
        // Repeating t or T from just before the character: skip it.
        if z.vfinddir > 0 {
            if z.cs < z.line.len() && z.line.get(z.cs + 1).copied() == target {
                z.cs += 1;
            }
        } else if z.cs > 0 && Some(z.line[z.cs - 1]) == target {
            z.cs -= 1;
        }
    }
    let step: isize = if z.vfinddir > 0 { 1 } else { -1 };
    let mut c = z.cs as isize;
    for _ in 0..n {
        loop {
            c += step;
            if !(c >= 0
                && c < ll
                && Some(z.line[c as usize]) != target
                && z.line[c as usize] != '\n')
            {
                break;
            }
        }
        if c < 0 || c >= ll || z.line[c as usize] == '\n' {
            z.cs = ocs;
            return 1;
        }
    }
    z.cs = c as usize;
    if z.tailadd > 0 {
        z.cs += 1;
    } else if z.tailadd < 0 {
        z.cs = z.cs.saturating_sub(1);
    }
    if z.vfinddir == 1 && z.virangeflag != 0 {
        z.cs += 1;
    }
    0
}

pub fn virepeatfind(z: &mut Zle, args: &[String]) -> i32 {
    vifindchar(z, true, args)
}

pub fn virevrepeatfind(z: &mut Zle, args: &[String]) -> i32 {
    if z.zmod.mult < 0 {
        z.zmod.mult = -z.zmod.mult;
        let ret = vifindchar(z, true, args);
        z.zmod.mult = -z.zmod.mult;
        return ret;
    }
    z.tailadd = -z.tailadd;
    z.vfinddir = -z.vfinddir;
    let ret = vifindchar(z, true, args);
    z.vfinddir = -z.vfinddir;
    z.tailadd = -z.tailadd;
    ret
}

pub fn vifirstnonblank(z: &mut Zle, _args: &[String]) -> i32 {
    z.cs = z.findbol();
    while z.cs != z.line.len() && iblank(z.line[z.cs]) {
        z.cs += 1;
    }
    0
}

pub fn visetmark(z: &mut Zle, _args: &[String]) -> i32 {
    let Some(ch) = z.getfullchar(false).filter(|c| c.is_ascii_lowercase()) else {
        return 1;
    };
    let i = (ch as u8 - b'a') as usize;
    z.vimarkcs[i] = Some(z.cs);
    z.vimarkline[i] = z.histline;
    0
}

pub fn vigotomark(z: &mut Zle, _args: &[String]) -> i32 {
    let oldcs = z.cs;
    let oldline = z.histline;

    // A mark slot, or (for `.) where the cursor was after the last change.
    let (slot, markcs, markhist) = match z.getfullchar(false) {
        Some('\'') | Some('`') => (Some(26), z.vimarkcs[26], z.vimarkline[26]),
        Some('.') if z.ucur > 0 => {
            let ch = &z.changes[z.ucur - 1];
            (None, Some(ch.new_cs), ch.hist)
        }
        Some(c) if c.is_ascii_lowercase() => {
            let i = (c as u8 - b'a') as usize;
            (Some(i), z.vimarkcs[i], z.vimarkline[i])
        }
        _ => return 1,
    };
    // An unset mark (zsh's zero history line).
    let Some(markcs) = markcs else {
        return 1;
    };
    if z.histline != markhist && !hist::zle_goto_hist(z, markhist) {
        if let Some(i) = slot {
            z.vimarkcs[i] = None;
            z.vimarkline[i] = 0;
        }
        return 1;
    }
    z.cs = markcs;
    z.vimarkcs[26] = Some(oldcs);
    z.vimarkline[26] = oldline;
    if z.cs > z.line.len() {
        z.cs = z.line.len();
    }
    0
}

pub fn vigotomarkline(z: &mut Zle, args: &[String]) -> i32 {
    vigotomark(z, args);
    vifirstnonblank(z, &[])
}
