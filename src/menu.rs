//! zsh's menu selection (`zstyle ':completion:*' menu select`, which
//! oh-my-zsh sets): Tab inserts what the matches share and lists them in
//! a grid below the line; a second Tab starts selecting. Tab, Shift-Tab
//! and the arrows move the selection, which is inserted as it moves.
//! Enter keeps the selection, Ctrl-G puts back the word as typed, and any
//! other edit leaves the menu with the line as it stands.
//!
//! The grid is drawn as the line's hint, so the editor clears its rows
//! on every redraw.

use rustyline::completion::Pair;
use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, RepeatCount};
use std::sync::Mutex;

struct Menu {
    // Byte offset of the word being completed.
    start: usize,
    // The word as typed before the menu opened.
    typed: String,
    cands: Vec<Pair>,
    sel: Option<usize>,
    // First grid row shown when the grid is taller than the screen.
    top: usize,
    // The line and cursor as the menu left them; anything else means the
    // user edited the line and the menu is gone.
    line: String,
    pos: usize,
    // The grid as last drawn, so highlight_hint keeps its colors.
    drawn: String,
}

static MENU: Mutex<Option<Menu>> = Mutex::new(None);

// Up/Down on an empty line step through history one entry at a time
// (the editor's prefix search finds nothing for an empty prefix). Holds
// the line such a step left, so the next Up or Down keeps stepping; a
// pending step records the line it lands on.
static BROWSE: Mutex<(Option<String>, bool)> = Mutex::new((None, false));

/// A key the menu handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Enter,
    Cancel,
}

/// Binds a key to the menu.
pub struct MenuKey(pub Key);

impl ConditionalEventHandler for MenuKey {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, ctx: &EventContext) -> Option<Cmd> {
        on_key(self.0, ctx.line(), ctx.pos())
    }
}

/// Forgets any menu, for a new line.
pub fn reset() {
    if let Ok(mut m) = MENU.lock() {
        *m = None;
    }
    if let Ok(mut b) = BROWSE.lock() {
        *b = (None, false);
    }
}

// Up/Down outside the menu: oh-my-zsh's up-line-or-beginning-search,
// which searches history for lines starting with what is typed, or steps
// to the previous or next line when nothing is.
fn history_key(up: bool, line: &str, pos: usize) -> Cmd {
    let Ok(mut b) = BROWSE.lock() else {
        return Cmd::Noop;
    };
    let stepping = b.0.as_deref() == Some(line);
    if line.get(..pos).is_some_and(str::is_empty) || stepping {
        b.1 = true;
        return if up {
            Cmd::PreviousHistory
        } else {
            Cmd::NextHistory
        };
    }
    b.0 = None;
    if up {
        Cmd::HistorySearchBackward
    } else {
        Cmd::HistorySearchForward
    }
}

/// The grid to draw below `line`, or None when no menu is open there.
pub fn hint(line: &str, pos: usize) -> Option<String> {
    if let Ok(mut b) = BROWSE.lock() {
        if b.1 {
            *b = (Some(line.to_string()), false);
        }
    }
    let mut guard = MENU.lock().ok()?;
    if !guard
        .as_ref()
        .is_some_and(|m| m.line == line && m.pos == pos)
    {
        *guard = None;
        return None;
    }
    let m = guard.as_mut()?;
    let (cols, rows) = crate::shell::terminal_size()
        .map(|(c, r)| (c as usize, r as usize))
        .unwrap_or((80, 24));
    m.drawn = render(m, cols, rows);
    Some(m.drawn.clone())
}

/// Whether `hint` is the grid, which carries its own colors.
pub fn is_drawn(hint: &str) -> bool {
    MENU.lock()
        .ok()
        .is_some_and(|g| g.as_ref().is_some_and(|m| m.drawn == hint))
}

fn on_key(key: Key, line: &str, pos: usize) -> Option<Cmd> {
    let mut guard = MENU.lock().ok()?;
    let active = guard
        .as_ref()
        .is_some_and(|m| m.line == line && m.pos == pos);
    if !active {
        *guard = None;
    }
    let selecting = active && guard.as_ref().is_some_and(|m| m.sel.is_some());
    match key {
        Key::Tab | Key::BackTab if !active => open(&mut guard, line, pos, key),
        Key::Enter if selecting => {
            // Keep the selection; the line is not run.
            *guard = None;
            Some(Cmd::Repaint)
        }
        Key::Cancel if active => {
            let m = guard.take()?;
            Some(replace(m.start, &m.typed))
        }
        Key::Tab | Key::BackTab => {
            let step = if key == Key::Tab {
                Step::Next
            } else {
                Step::Prev
            };
            Some(select(guard.as_mut()?, step, line))
        }
        Key::Up | Key::Down | Key::Left | Key::Right if selecting => {
            let step = match key {
                Key::Up => Step::Up,
                Key::Down => Step::Down,
                Key::Left => Step::Left,
                _ => Step::Right,
            };
            Some(select(guard.as_mut()?, step, line))
        }
        _ => {
            // Not the menu's key: the menu closes and the key does what
            // it does without one.
            *guard = None;
            match key {
                Key::Up => Some(history_key(true, line, pos)),
                Key::Down => Some(history_key(false, line, pos)),
                _ => None,
            }
        }
    }
}

// Completes the word before the cursor: one match is inserted, several
// open the menu with what they share inserted.
fn open(guard: &mut Option<Menu>, line: &str, pos: usize, key: Key) -> Option<Cmd> {
    let (start, cands) = crate::lineedit::candidates(line, pos);
    let typed = line.get(start..pos)?.to_string();
    match cands.len() {
        0 => Some(Cmd::Noop),
        1 => Some(replace(start, &cands[0].replacement)),
        _ => {
            let shared = common_prefix(cands.iter().map(|c| c.replacement.as_str()));
            // Fuzzy matches need not start with the word; keep it then.
            let inserted = if shared.len() > typed.len() && shared.starts_with(&typed) {
                shared
            } else {
                typed.clone()
            };
            let mut m = Menu {
                start,
                typed: typed.clone(),
                cands,
                sel: None,
                top: 0,
                line: line.to_string(),
                pos,
                drawn: String::new(),
            };
            let cmd = if key == Key::BackTab {
                // Shift-Tab selects at once, from the end, as zsh's
                // reverse-menu-complete does.
                select(&mut m, Step::Prev, line)
            } else if inserted != typed {
                m.line = format!("{}{}{}", &line[..start], inserted, &line[pos..]);
                m.pos = start + inserted.len();
                replace(start, &inserted)
            } else {
                Cmd::Repaint
            };
            *guard = Some(m);
            Some(cmd)
        }
    }
}

// Moves the selection and puts it in place of the word.
fn select(m: &mut Menu, step: Step, line: &str) -> Cmd {
    let (cols, _) = crate::shell::terminal_size()
        .map(|(c, r)| (c as usize, r as usize))
        .unwrap_or((80, 24));
    let g = grid(&widths(&m.cands), cols);
    let i = move_selection(m.sel, m.cands.len(), g.rows, step);
    m.sel = Some(i);
    let text = &m.cands[i].replacement;
    let cmd = replace(m.start, text);
    m.line = format!("{}{}{}", &line[..m.start], text, &line[m.pos..]);
    m.pos = m.start + text.len();
    cmd
}

// The replacement the next Cmd::Complete makes: the start of the word,
// which ends at the cursor, and what takes its place.
static PENDING: Mutex<Option<(usize, String)>> = Mutex::new(None);

// Puts `new` in place of the word from `start` to the cursor, leaving the
// cursor after it. Commands that edit by a count get the key's repeat
// count from the editor, and Cmd::Replace leaves the cursor where it was,
// so this goes through completion with a single match instead.
fn replace(start: usize, new: &str) -> Cmd {
    if let Ok(mut p) = PENDING.lock() {
        *p = Some((start, new.to_string()));
    }
    Cmd::Complete
}

/// The replacement the menu asked for, as the one completion match.
pub fn take_pending() -> Option<(usize, Vec<Pair>)> {
    let (start, text) = PENDING.lock().ok()?.take()?;
    Some((
        start,
        vec![Pair {
            display: text.clone(),
            replacement: text,
        }],
    ))
}

fn common_prefix<'a>(mut words: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut len = first.len();
    for w in words {
        len = first
            .char_indices()
            .zip(w.chars())
            .take_while(|((_, a), b)| a == b)
            .last()
            .map_or(0, |((i, a), _)| i + a.len_utf8())
            .min(len);
    }
    first[..len].to_string()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Next,
    Prev,
    Up,
    Down,
    Left,
    Right,
}

/// The grid's shape: matches fill each column top to bottom, as zsh
/// lists them without LIST_ROWS_FIRST.
#[derive(Debug, PartialEq, Eq)]
struct Grid {
    cols: usize,
    rows: usize,
    // Width of a column, two blanks after the widest match included.
    width: usize,
}

fn grid(widths: &[usize], term_cols: usize) -> Grid {
    let n = widths.len().max(1);
    let width = widths.iter().copied().max().unwrap_or(0) + 2;
    // One column spare: a row that reaches the edge would wrap.
    let cols = (term_cols.saturating_sub(1) / width).clamp(1, n);
    let rows = n.div_ceil(cols);
    Grid {
        cols: n.div_ceil(rows),
        rows,
        width,
    }
}

// The index `step` moves the selection to among `n` matches in a grid of
// `rows` rows. With nothing selected yet, forward steps take the first
// match and backward ones the last.
fn move_selection(sel: Option<usize>, n: usize, rows: usize, step: Step) -> usize {
    let Some(i) = sel else {
        return match step {
            Step::Prev | Step::Up | Step::Left => n - 1,
            _ => 0,
        };
    };
    match step {
        Step::Next | Step::Down => (i + 1) % n,
        Step::Prev | Step::Up => (i + n - 1) % n,
        // Across the row, wrapping to its first column.
        Step::Right if i + rows < n => i + rows,
        Step::Right => i % rows,
        // Back across the row, wrapping to its last filled column.
        Step::Left if i >= rows => i - rows,
        Step::Left => {
            let row = i % rows;
            let last_col = (n - 1 - row) / rows;
            last_col * rows + row
        }
    }
}

// What a match shows: control characters in file names would move the
// cursor, so they show as `?`, as ls does.
fn shown(p: &Pair) -> String {
    p.display
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn widths(cands: &[Pair]) -> Vec<usize> {
    cands
        .iter()
        .map(|c| crate::display_width(&shown(c)))
        .collect()
}

// The color ls gives directories, from $LS_COLORS.
fn dir_color() -> String {
    crate::shell::var_get_opt("LS_COLORS")
        .and_then(|v| {
            v.split(':')
                .find_map(|e| e.strip_prefix("di=").map(str::to_string))
        })
        .filter(|c| c.bytes().all(|b| b.is_ascii_digit() || b == b';'))
        .unwrap_or_else(|| "01;34".to_string())
}

// The grid below the line: a newline, then its rows. A grid taller than
// the screen shows the rows around the selection and a line saying which.
fn render(m: &mut Menu, term_cols: usize, term_rows: usize) -> String {
    let w = widths(&m.cands);
    let g = grid(&w, term_cols);
    let n = m.cands.len();
    // Room for the prompt and the line saying which rows show.
    let room = term_rows.saturating_sub(4).max(1);
    let shown_rows = g.rows.min(room);
    if let Some(sel) = m.sel {
        let row = sel % g.rows;
        if row < m.top {
            m.top = row;
        } else if row >= m.top + shown_rows {
            m.top = row + 1 - shown_rows;
        }
    }
    m.top = m.top.min(g.rows - shown_rows);
    let dir = dir_color();
    let mut out = String::new();
    for r in m.top..m.top + shown_rows {
        out.push('\n');
        let mut line = String::new();
        for c in 0..g.cols {
            let i = c * g.rows + r;
            if i >= n {
                break;
            }
            let text = shown(&m.cands[i]);
            // The selection is highlighted across its column.
            let pad = " ".repeat(g.width - 2 - w[i]);
            if m.sel == Some(i) {
                line.push_str(&format!("\x1b[7m{}{}\x1b[0m", text, pad));
            } else if m.cands[i].display.ends_with('/') {
                line.push_str(&format!("\x1b[{}m{}\x1b[0m{}", dir, text, pad));
            } else {
                line.push_str(&text);
                line.push_str(&pad);
            }
            line.push_str("  ");
        }
        out.push_str(line.trim_end_matches(' '));
    }
    if shown_rows < g.rows {
        out.push_str(&format!(
            "\n\x1b[7mrows {} to {} of {}\x1b[0m",
            m.top + 1,
            m.top + shown_rows,
            g.rows
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;

    fn pairs(names: &[&str]) -> Vec<Pair> {
        names
            .iter()
            .map(|n| Pair {
                display: n.to_string(),
                replacement: n.to_string(),
            })
            .collect()
    }

    fn menu(names: &[&str], sel: Option<usize>) -> Menu {
        Menu {
            start: 0,
            typed: String::new(),
            cands: pairs(names),
            sel,
            top: 0,
            line: String::new(),
            pos: 0,
            drawn: String::new(),
        }
    }

    fn plain(s: &str) -> String {
        crate::strip_ansi(s)
    }

    #[test]
    fn grid_fills_columns_that_fit_the_terminal() {
        // Width 3 + 2 = 5; 19 usable columns hold 3 of them.
        assert_eq!(
            grid(&[3; 7], 20),
            Grid {
                cols: 3,
                rows: 3,
                width: 5
            }
        );
        // Never more columns than matches, never fewer than one.
        assert_eq!(grid(&[3; 2], 80).cols, 2);
        assert_eq!(grid(&[100], 20).cols, 1);
        // 7 matches in 4 columns need 2 rows, which 4 columns fill.
        assert_eq!(grid(&[3; 7], 21).rows, 2);
        assert_eq!(grid(&[3; 7], 21).cols, 4);
    }

    #[test]
    fn selection_moves_through_the_grid() {
        // 7 matches, 3 rows: columns hold 0-2, 3-5, 6.
        let mv = |sel, step| move_selection(Some(sel), 7, 3, step);
        assert_eq!(move_selection(None, 7, 3, Step::Next), 0);
        assert_eq!(move_selection(None, 7, 3, Step::Prev), 6);
        assert_eq!(mv(6, Step::Next), 0);
        assert_eq!(mv(0, Step::Up), 6);
        assert_eq!(mv(2, Step::Down), 3);
        assert_eq!(mv(1, Step::Right), 4);
        assert_eq!(mv(4, Step::Right), 1);
        assert_eq!(mv(0, Step::Right), 3);
        assert_eq!(mv(3, Step::Right), 6);
        assert_eq!(mv(6, Step::Right), 0);
        assert_eq!(mv(4, Step::Left), 1);
        assert_eq!(mv(0, Step::Left), 6);
        assert_eq!(mv(1, Step::Left), 4);
    }

    #[test]
    fn common_prefix_stops_at_the_first_difference() {
        let p = |w: &[&str]| common_prefix(w.iter().copied());
        assert_eq!(p(&["src/main.rs", "src/menu.rs"]), "src/m");
        assert_eq!(p(&["abc", "xyz"]), "");
        assert_eq!(p(&["abc", "ab"]), "ab");
        assert_eq!(p(&["héllo", "hélp"]), "hél");
    }

    #[test]
    fn render_lists_columns_and_highlights_the_selection() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = menu(&["aa", "bb", "cc", "dir/"], Some(1));
        // Width 4 + 2 = 6; 13 usable columns hold 2: rows of 2.
        let out = render(&mut m, 14, 24);
        assert_eq!(plain(&out), "\naa    cc\nbb    dir/");
        assert!(out.contains("\x1b[7mbb  \x1b[0m"), "{:?}", out);
    }

    #[test]
    fn render_scrolls_to_keep_the_selection_shown() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let names: Vec<String> = (0..10).map(|i| format!("f{}", i)).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        // One column of 10; 7 terminal rows leave room for 3.
        let mut m = menu(&refs, Some(8));
        let out = plain(&render(&mut m, 5, 7));
        assert_eq!(out, "\nf6\nf7\nf8\nrows 7 to 9 of 10");
        m.sel = Some(0);
        let out = plain(&render(&mut m, 5, 7));
        assert_eq!(out, "\nf0\nf1\nf2\nrows 1 to 3 of 10");
    }

    #[test]
    fn control_characters_in_names_show_as_question_marks() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut m = menu(&["a\x1b[2Jb", "c\nd"], None);
        assert_eq!(plain(&render(&mut m, 80, 24)), "\na?[2Jb  c?d");
    }

    // Drives the menu as the editor would: applies each command to the
    // line and asks for the hint afterwards.
    fn apply(line: &mut String, pos: &mut usize, cmd: Option<Cmd>) {
        if cmd == Some(Cmd::Complete) {
            let (start, m) = take_pending().expect("a pending replacement");
            line.replace_range(start..*pos, &m[0].replacement);
            *pos = start + m[0].replacement.len();
        }
        let _ = hint(line, *pos);
    }

    #[test]
    fn tab_lists_then_selects_and_enter_keeps_it() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for f in ["menu_alpha", "menu_beta", "other"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        reset();
        let mut line = format!("ls {}/m", dir.path().display());
        let mut pos = line.len();
        let base = format!("ls {}/", dir.path().display());

        // First Tab: what the matches share, and the list.
        let cmd = on_key(Key::Tab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert_eq!(line, format!("{}menu_", base));
        let grid = hint(&line, pos).expect("menu open");
        assert_eq!(plain(&grid), "\nmenu_alpha  menu_beta");

        // Second Tab selects the first match, the next one the second.
        let cmd = on_key(Key::Tab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert_eq!(line, format!("{}menu_alpha", base));
        let cmd = on_key(Key::Right, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert_eq!(line, format!("{}menu_beta", base));

        // Enter keeps it and closes the menu without running the line.
        assert_eq!(on_key(Key::Enter, &line, pos), Some(Cmd::Repaint));
        assert_eq!(hint(&line, pos), None);
    }

    #[test]
    fn cancel_restores_the_word_and_edits_close_the_menu() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for f in ["menu_alpha", "menu_beta"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        reset();
        let typed = format!("ls {}/m", dir.path().display());
        let mut line = typed.clone();
        let mut pos = line.len();
        for _ in 0..2 {
            let cmd = on_key(Key::Tab, &line, pos);
            apply(&mut line, &mut pos, cmd);
        }
        assert!(line.ends_with("menu_alpha"));
        let cmd = on_key(Key::Cancel, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert_eq!(line, typed);
        assert_eq!(hint(&line, pos), None);

        // Typing after the list closes it; arrows then search history.
        let cmd = on_key(Key::Tab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert!(hint(&line, pos).is_some());
        line.push('x');
        pos += 1;
        assert_eq!(hint(&line, pos), None);
        assert_eq!(
            on_key(Key::Up, &line, pos),
            Some(Cmd::HistorySearchBackward)
        );
    }

    #[test]
    fn arrows_before_selecting_keep_their_usual_meaning() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for f in ["menu_alpha", "menu_beta"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        reset();
        let mut line = format!("ls {}/m", dir.path().display());
        let mut pos = line.len();
        let cmd = on_key(Key::Tab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert_eq!(
            on_key(Key::Down, &line, pos),
            Some(Cmd::HistorySearchForward)
        );
        assert_eq!(hint(&line, pos), None);
        // Enter with only a list runs the line as usual.
        assert_eq!(on_key(Key::Enter, &line, pos), None);
    }

    #[test]
    fn shift_tab_selects_from_the_end_at_once() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        for f in ["menu_alpha", "menu_beta"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        reset();
        let mut line = format!("ls {}/m", dir.path().display());
        let mut pos = line.len();
        let cmd = on_key(Key::BackTab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert!(line.ends_with("/menu_beta"), "{}", line);
        let cmd = on_key(Key::BackTab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert!(line.ends_with("/menu_alpha"), "{}", line);
    }

    #[test]
    fn replacing_a_word_goes_through_completion() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A count-based edit would get the key's repeat count of 1 and
        // remove one character of the word, not all of it.
        assert_eq!(replace(3, "main.rs"), Cmd::Complete);
        let (start, m) = take_pending().unwrap();
        assert_eq!((start, m[0].replacement.as_str()), (3, "main.rs"));
        assert!(take_pending().is_none());
    }

    #[test]
    fn up_on_an_empty_line_steps_through_history() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset();
        // The editor's prefix search finds nothing for an empty prefix.
        assert_eq!(on_key(Key::Up, "", 0), Some(Cmd::PreviousHistory));
        // The line it lands on keeps the next Up and Down stepping.
        let _ = hint("echo second", 11);
        assert_eq!(
            on_key(Key::Up, "echo second", 11),
            Some(Cmd::PreviousHistory)
        );
        let _ = hint("echo first", 10);
        assert_eq!(on_key(Key::Down, "echo first", 10), Some(Cmd::NextHistory));
        // Typed text searches by prefix.
        let _ = hint("echo second", 11);
        assert_eq!(on_key(Key::Up, "ec", 2), Some(Cmd::HistorySearchBackward));
        let _ = hint("echo second", 2);
        assert_eq!(
            on_key(Key::Up, "echo second", 2),
            Some(Cmd::HistorySearchBackward)
        );
    }

    #[test]
    fn a_single_match_is_inserted_without_a_menu() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only_one"), "").unwrap();
        reset();
        let mut line = format!("ls {}/o", dir.path().display());
        let mut pos = line.len();
        let cmd = on_key(Key::Tab, &line, pos);
        apply(&mut line, &mut pos, cmd);
        assert!(line.ends_with("/only_one"), "{}", line);
        assert_eq!(hint(&line, pos), None);
    }
}
