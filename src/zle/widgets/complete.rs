//! Completion widgets: the editor's way into the completion code in
//! `crate::zle::comp`, after zsh's zle_tricky.c.

use crate::zle::comp::tricky;
use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("accept-and-menu-complete", tricky::acceptandmenucomplete),
    ("complete-word", tricky::completeword),
    ("delete-char-or-list", tricky::deletecharorlist),
    ("end-of-list", tricky::endoflist),
    ("expand-cmd-path", tricky::expandcmdpath),
    ("expand-history", expandhistory),
    ("expand-or-complete", tricky::expandorcomplete),
    ("expand-or-complete-prefix", tricky::expandorcompleteprefix),
    ("expand-word", tricky::expandword),
    ("list-choices", tricky::listchoices),
    ("list-expand", tricky::listexpand),
    ("magic-space", magicspace),
    ("menu-complete", tricky::menucomplete),
    ("menu-expand-or-complete", tricky::menuexpandorcomplete),
    ("reverse-menu-complete", tricky::reversemenucomplete),
    ("run-help", tricky::processcmd),
    ("spell-word", tricky::spellword),
    ("which-command", tricky::processcmd),
];

/// History expansion of the whole line; true if it changed.
pub fn doexpandhist(z: &mut Zle) -> bool {
    tricky::doexpandhist(z)
}

pub fn magicspace(z: &mut Zle, args: &[String]) -> i32 {
    tricky::magicspace(z, args)
}

pub fn expandhistory(z: &mut Zle, args: &[String]) -> i32 {
    tricky::expandhistory(z, args)
}

/// Runs a `zle -C` widget: builtin completion widget `builtin` with
/// completion function `func`.
pub fn completecall(z: &mut Zle, builtin: &str, func: &str, args: &[String]) -> i32 {
    tricky::completecall(z, builtin, func, args)
}

pub fn iremovesuffix(z: &mut Zle, c: Option<char>, keep: bool) {
    tricky::iremovesuffix(z, c, keep);
}

pub fn invalidatelist(z: &mut Zle) {
    crate::zle::comp::result::invalidate_list(z);
}

pub fn listlist(z: &mut Zle, items: &[String]) {
    tricky::listlist(z, items);
}

/// Whether a completion widget's function is running (zsh's incompfunc).
pub fn in_completion() -> bool {
    with_active(|z| z.comp.incompfunc != 0).unwrap_or(false)
}

/// Makes a following space count as typed (magic-space after expansion).
pub fn fixmagicspace(z: &mut Zle) {
    tricky::fixmagicspace(z);
}
