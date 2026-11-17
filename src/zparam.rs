//! zsh/parameter's special parameters ($builtins, $commands, $parameters,
//! ...). Each is computed when read and kept as an ordinary read-only
//! parameter, so subscripts, flags and `$+` see it like any other. A
//! parameter the user made with the same name hides it.

use crate::params::{Param, Value, READONLY, SPECIAL};
use crate::shell::with_shell;
use std::collections::BTreeMap;

const NAMES: &[&str] = &[
    "builtins",
    "commands",
    "parameters",
    "reswords",
    "nameddirs",
    "userdirs",
    "modules",
    "history",
    "historywords",
    "widgets",
    "keymaps",
];

/// Brings special parameter `name` up to date, if it is one.
pub fn ensure(name: &str) {
    if !NAMES.contains(&name) {
        return;
    }
    let shadowed = with_shell(|s| {
        s.param(name)
            .is_some_and(|p| p.attrs & SPECIAL == 0 || p.level > 0)
    });
    if shadowed {
        return;
    }
    let value = match name {
        "builtins" => assoc(
            crate::builtins::builtin_names()
                .filter(|b| crate::builtins::is_builtin(b))
                .map(|b| (b.to_string(), "defined".to_string())),
        ),
        "commands" => assoc(
            crate::shell::all_commands()
                .into_iter()
                .filter_map(|c| crate::shell::find_command(&c).map(|p| (c, p))),
        ),
        "parameters" => {
            let names: Vec<String> = with_shell(|s| s.params.keys().cloned().collect());
            assoc(
                names
                    .into_iter()
                    .filter(|n| n != "parameters")
                    .map(|n| {
                        let t = crate::expand::param_type(&n);
                        (n, t)
                    })
                    .filter(|(_, t)| !t.is_empty()),
            )
        }
        "reswords" => Value::Array(crate::builtins::reserved_words()),
        "nameddirs" => {
            let m = crate::shell::NAMED_DIRS
                .lock()
                .map(|m| m.clone())
                .unwrap_or_default();
            Value::Assoc(m)
        }
        "userdirs" => assoc(user_dirs().into_iter()),
        "modules" => assoc(
            [
                "zsh/complete",
                "zsh/complist",
                "zsh/computil",
                "zsh/main",
                "zsh/parameter",
                "zsh/zle",
                "zsh/zpty",
                "zsh/zutil",
            ]
            .iter()
            .map(|m| (m.to_string(), "loaded".to_string())),
        ),
        "history" => {
            let h = with_shell(|s| s.history.lock().map(|h| h.clone()).unwrap_or_default());
            assoc(
                h.into_iter()
                    .enumerate()
                    .map(|(i, l)| ((i + 1).to_string(), l)),
            )
        }
        "historywords" => {
            let h = with_shell(|s| s.history.lock().map(|h| h.clone()).unwrap_or_default());
            Value::Array(
                h.iter()
                    .rev()
                    .flat_map(|l| crate::expand::split_shell_words(l).into_iter().rev())
                    .collect(),
            )
        }
        "widgets" => assoc(crate::zle::thingy::names().into_iter().map(|n| {
            let kind = crate::zle::thingy::describe(&n);
            (n, kind)
        })),
        _ => Value::Array(crate::zle::keymap::with_keymaps(|t| t.names())),
    };
    with_shell(|s| {
        let mut p = Param::with(value);
        p.attrs |= READONLY | SPECIAL;
        s.params.insert(name.to_string(), p);
    });
}

fn assoc(it: impl Iterator<Item = (String, String)>) -> Value {
    Value::Assoc(it.collect::<BTreeMap<_, _>>())
}

fn user_dirs() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let pw = libc::getpwent();
            if pw.is_null() {
                break;
            }
            let name = std::ffi::CStr::from_ptr((*pw).pw_name)
                .to_string_lossy()
                .into_owned();
            let dir = std::ffi::CStr::from_ptr((*pw).pw_dir)
                .to_string_lossy()
                .into_owned();
            out.push((name, dir));
        }
        libc::endpwent();
    }
    out
}
