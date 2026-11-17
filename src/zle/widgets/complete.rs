//! Completion widgets, after zsh's zle_tricky.c.

use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("magic-space", magicspace),
    ("expand-history", expandhistory),
];

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
    let bol = z.findbol();
    if z.viinsbegin > bol {
        z.viinsbegin = bol;
    }
    true
}

pub fn magicspace(z: &mut Zle, args: &[String]) -> i32 {
    fixmagicspace(z);
    let bang = '!';
    let bangq = (0..z.line.len()).find(|&i| {
        z.line[i] == bang && z.line.get(i + 1) == Some(&'"') && (i == 0 || z.line[i - 1] == '\\')
    });
    let ret = super::misc::selfinsert(z, args);
    if ret == 0 && bangq.is_none_or(|b| b + 2 > z.cs) {
        doexpandhist(z);
    }
    ret
}

pub fn expandhistory(z: &mut Zle, _args: &[String]) -> i32 {
    i32::from(!doexpandhist(z))
}

pub fn completecall(_z: &mut Zle, _builtin: &str, _func: &str, _args: &[String]) -> i32 {
    1
}

pub fn iremovesuffix(_z: &mut Zle, _c: Option<char>, _keep: bool) {}

pub fn invalidatelist(_z: &mut Zle) {}

pub fn listlist(_z: &mut Zle, _items: &[String]) {}

pub fn in_completion() -> bool {
    false
}

/// Makes a following space count as typed (magic-space after expansion).
pub fn fixmagicspace(z: &mut Zle) {
    z.lastchar = b' ' as i32;
    z.lastchar_wide = Some(' ');
    z.lastchar_wide_valid = true;
}
