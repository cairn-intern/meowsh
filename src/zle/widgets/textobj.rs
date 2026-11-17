//! Port of zsh's textobjects.c.

use crate::zle::widgets::word::wordclass;
use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("select-a-word", selectword),
    ("select-in-word", selectword),
    ("select-a-blank-word", selectword),
    ("select-in-blank-word", selectword),
    ("select-a-shell-word", selectargument),
    ("select-in-shell-word", selectargument),
];

fn iblank(c: char) -> bool {
    c.is_whitespace() && c != '\n'
}

fn blankwordclass(x: char) -> i32 {
    if iblank(x) {
        0
    } else {
        1
    }
}

// zsh's IS_THINGY: bindk is the widget name or its dotted form.
fn is_thingy(z: &Zle, name: &str) -> bool {
    z.bindk
        .as_deref()
        .is_some_and(|b| b == name || b.strip_prefix('.') == Some(name))
}

pub fn selectword(z: &mut Zle, _args: &[String]) -> i32 {
    let mut n = z.zmod.mult;
    let mut all = is_thingy(z, "select-a-word") || is_thingy(z, "select-a-blank-word");
    let viclass: fn(char) -> i32 =
        if is_thingy(z, "select-a-word") || is_thingy(z, "select-in-word") {
            wordclass
        } else {
            blankwordclass
        };
    let line = z.line.clone();
    let ll = line.len();
    // The line as zsh sees it, with its terminating NUL.
    let at = |i: usize| line.get(i).copied().unwrap_or('\0');
    let mut cs = z.cs;
    let mut mark = z.mark;
    let mut sclass = viclass(at(cs));
    let mut doblanks = all && sclass != 0;

    if z.region_active == 0 || cs == mark {
        // Back to the first character of the same class, stopping at the
        // start of the line.
        mark = cs;
        while mark != 0 {
            let pos = mark - 1;
            if line[pos] == '\n' || viclass(line[pos]) != sclass {
                break;
            }
            mark = pos;
        }
        // Forward over characters of the same class.
        while cs < ll {
            cs += 1;
            let mut pos = cs;
            // Single newlines within blanks are included.
            if all && sclass == 0 && pos < ll && line[pos] == '\n' {
                pos += 1;
            }
            if at(pos) == '\n' || viclass(at(pos)) != sclass {
                break;
            }
        }
        if all {
            let nclass = viclass(at(cs));
            // If the start or the new position is blank, advance over a
            // new block of characters of a common type.
            if nclass == 0 || sclass == 0 {
                while cs < ll {
                    cs += 1;
                    if at(cs) == '\n' || viclass(at(cs)) != nclass {
                        break;
                    }
                }
                if n < 2 {
                    doblanks = false;
                }
            }
        }
    } else {
        // In visual mode, advance one character so repeated invocations
        // select subsequent words.
        if cs > mark {
            if cs < ll {
                cs += 1;
            }
        } else {
            cs = cs.saturating_sub(1);
        }
        if cs < mark {
            // The cursor is before the mark: move it back.
            while n > 0 {
                n -= 1;
                let mut pos = cs;
                // First over blanks.
                if all && (viclass(at(pos)) == 0 || at(pos) == '\n') {
                    all = false;
                    while pos != 0 {
                        pos -= 1;
                        if line[pos] == '\n' {
                            break;
                        }
                        cs = pos;
                        if viclass(line[pos]) != 0 {
                            break;
                        }
                    }
                } else if cs != 0 && at(cs) == '\n' {
                    // The "in" widgets pass over one newline.
                    pos -= 1;
                    if line[pos] != '\n' {
                        cs = pos;
                    }
                }
                pos = cs;
                sclass = viclass(at(cs));
                // Now back over non-blanks.
                while at(pos) != '\n' && viclass(at(pos)) == sclass {
                    cs = pos;
                    if pos == 0 {
                        cs = 0;
                        break;
                    }
                    pos -= 1;
                }
                // Blanks again, but only if there were none the first time.
                if all && cs != 0 {
                    pos = cs - 1;
                    if viclass(line[pos]) == 0 {
                        while pos != 0 {
                            pos -= 1;
                            if line[pos] == '\n' || viclass(line[pos]) != 0 {
                                break;
                            }
                            cs = pos;
                        }
                    }
                }
            }
            z.cs = cs;
            z.mark = mark;
            return 0;
        }
        n += 1;
        doblanks = false;
    }
    let mut region_active = i32::from(z.region_active != 0); // character-wise

    // Each numeric argument advances over a further block of one class.
    loop {
        n -= 1;
        if n <= 0 {
            break;
        }
        if cs < ll && line[cs] == '\n' {
            cs += 1;
        }
        sclass = viclass(at(cs));
        while cs < ll {
            cs += 1;
            if at(cs) == '\n' || viclass(at(cs)) != sclass {
                break;
            }
        }
        // The "a" widgets advance an extra block if either is blanks.
        if all {
            if cs < ll && line[cs] == '\n' {
                cs += 1;
            }
            if sclass == 0 || viclass(at(cs)) == 0 {
                sclass = viclass(at(cs));
                if n == 1 && sclass == 0 {
                    doblanks = false;
                }
                while cs < ll {
                    cs += 1;
                    if at(cs) == '\n' || viclass(at(cs)) != sclass {
                        break;
                    }
                }
            }
        }
    }

    // Blanks removed at neither end: remove some at the start.
    if doblanks {
        let mut pos = mark;
        while pos != 0 {
            pos -= 1;
            // Leave blanks at the start of the line (indentation).
            if line[pos] == '\n' {
                break;
            }
            if !iblank(line[pos]) {
                mark = pos + 1;
                break;
            }
        }
    }
    // vi operators don't include the cursor position; in insert or emacs
    // mode neither does the region, but in vi visual mode it does.
    if z.virangeflag == 0 {
        if !z.invicmdmode() {
            region_active = 1;
        } else if cs != 0 && cs > mark {
            cs -= 1;
        }
    }
    z.cs = cs;
    z.mark = mark;
    z.region_active = region_active;
    0
}

pub fn selectargument(z: &mut Zle, _args: &[String]) -> i32 {
    let n = z.zmod.mult;
    if n < 1 || 2 * n > z.ll() as i64 + 1 {
        return 1;
    }
    let n = n as usize;

    // From emacs mode, enable the region.
    if !z.invicmdmode() {
        z.region_active = 1;
        z.mark = z.cs;
    }

    let mut wstarts: Vec<isize> = vec![0; n];
    let mut wcur = 0;
    let mut wend: isize = 0;
    let pre: String = if !z.isfirstln {
        crate::zle::prebuffer()
    } else {
        String::new()
    };
    let off = pre.chars().count();
    let mut text = pre;
    text.extend(z.line.iter());
    // zsh measures word ends from the start of the edit buffer but the
    // cursor from the start of the prebuffer.
    let metacs = ((z.cs + off) as isize - 1).max(0);
    let mut lx = crate::lexer::Lexer::new(&text, true);
    lx.comments = crate::options::is_set("interactivecomments");
    loop {
        wstarts[wcur] = wend;
        wcur = (wcur + 1) % n;
        match lx.next_token() {
            Ok(crate::lexer::Tok::Eof) | Err(_) => break,
            Ok(_) => {}
        }
        wend = lx.pos() as isize - off as isize;
        if wend > metacs {
            break;
        }
    }

    let ll = z.ll() as isize;
    z.cs = wend.clamp(0, ll) as usize;
    z.mark = wstarts[wcur].clamp(0, ll) as usize;

    if is_thingy(z, "select-in-shell-word") {
        let at = |z: &Zle, i: usize| z.line.get(i).copied().unwrap_or('\0');
        let (mut matches, mut ematches) = ("`'\"", "`'\"");
        let mut end = z.cs;
        // Leave out initial blanks...
        while z.mark < z.cs && iblank(z.line[z.mark]) {
            z.mark += 1;
        }
        // ...and a matching pair of quotes.
        let mut start = z.mark;
        if at(z, start) == '$' {
            matches = "'({";
            ematches = "')}";
            start += 1;
        }
        let c = at(z, start);
        if let Some(found) = matches.chars().position(|m| m == c) {
            end = end.saturating_sub(1);
            if ematches.chars().nth(found) == Some(at(z, end)) {
                z.cs = end;
                z.mark = start + 1;
            }
        }
    }

    // vi operators don't include the cursor position.
    if z.virangeflag == 0 && z.invicmdmode() && z.cs != 0 {
        z.cs -= 1;
    }
    0
}
