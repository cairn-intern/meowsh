//! Internal widgets, one module per zsh source file.

pub mod complete;
pub mod core;
pub mod deltochar;
pub mod hist;
pub mod misc;
pub mod move_;
pub mod textobj;
pub mod vi;
pub mod word;

use super::WidgetFn;

/// The function implementing internal widget `name`, if there is one.
pub fn lookup(name: &str) -> Option<WidgetFn> {
    for table in [
        core::WIDGETS,
        misc::WIDGETS,
        move_::WIDGETS,
        word::WIDGETS,
        vi::WIDGETS,
        hist::WIDGETS,
        textobj::WIDGETS,
        complete::WIDGETS,
    ] {
        if let Some((_, f)) = table.iter().find(|(n, _)| *n == name) {
            return Some(*f);
        }
    }
    None
}
