//! Builtin commands. Each takes the full argument vector (the name in
//! `words[0]`) and returns an exit status. Output goes straight to the
//! file descriptors so redirections apply.

use crate::exec::{call_function, last_status, run_string, set_flow, strerror};
use crate::jobs::{builtin_bg, builtin_fg, builtin_jobs, builtin_wait, kill_targets};
use crate::lexer::is_valid_name;
use crate::shell::{
    array_declare, array_make_unique, array_set, assoc_declare, declare_local, find_command,
    in_function_scope, var_get, var_get_opt, var_set, with_shell,
};
use crate::sys::{err_line, out, read_byte};
use crate::trap::{set_trap, signal_name, signal_number, SIGNALS};
use crate::types::{
    Flow, FuncDef, Node, Var, ZStyle, OPT_ALLEXPORT, OPT_ERREXIT, OPT_HASHALL, OPT_MONITOR,
    OPT_NOCLOBBER, OPT_NOEXEC, OPT_NOGLOB, OPT_NOUNSET, OPT_PIPEFAIL, OPT_VERBOSE, OPT_XTRACE,
    VAR_EXPORT, VAR_READONLY,
};
use std::sync::Arc;

const SPECIAL: &[&str] = &[
    "break", ":", "continue", ".", "eval", "exec", "exit", "export", "readonly", "return", "set",
    "shift", "times", "trap", "unset",
];

const REGULAR: &[&str] = &[
    "alias",
    "autoload",
    "bg",
    "cd",
    "command",
    "declare",
    "dirs",
    "echo",
    "false",
    "fg",
    "getopts",
    "hash",
    "history",
    "jobs",
    "kill",
    "local",
    "popd",
    "pushd",
    "pwd",
    "read",
    "setopt",
    "source",
    "test",
    "[",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unsetopt",
    "wait",
    "zstyle",
    "add-zsh-hook",
    "bindkey",
    "compinit",
    "compdef",
    "compaudit",
    "zmodload",
    "emulate",
];

const KEYWORDS: &[&str] = &[
    "!", "{", "}", "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if",
    "in", "then", "until", "while", "[[", "]]",
];

pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    SPECIAL.iter().chain(REGULAR.iter()).copied()
}

pub fn is_special_builtin(name: &str) -> bool {
    SPECIAL.contains(&name)
}

pub fn is_builtin(name: &str) -> bool {
    SPECIAL.contains(&name) || REGULAR.contains(&name)
}

pub fn run(name: &str, words: &[String]) -> i32 {
    match name {
        ":" | "true" => 0,
        "false" => 1,
        "break" => builtin_break(words, true),
        "continue" => builtin_break(words, false),
        "." | "source" => builtin_source(words),
        "eval" => run_string(&words[1..].join(" ")),
        "exit" => builtin_exit(words),
        "export" => builtin_export(words),
        "readonly" => builtin_readonly(words),
        "return" => builtin_return(words),
        "set" => builtin_set(words),
        "shift" => builtin_shift(words),
        "times" => builtin_times(),
        "trap" => builtin_trap(words),
        "unset" => builtin_unset(words),
        "alias" => builtin_alias(words),
        "unalias" => builtin_unalias(words),
        "autoload" => builtin_autoload(words),
        "bg" => builtin_bg(&words[1..]),
        "fg" => builtin_fg(&words[1..]),
        "jobs" => builtin_jobs(&words[1..]),
        "wait" => builtin_wait(&words[1..]),
        "cd" => builtin_cd(words),
        "pushd" => builtin_pushd(words),
        "popd" => builtin_popd(),
        "dirs" => {
            print_dirs();
            0
        }
        "command" => builtin_command(words),
        "echo" => builtin_echo(words),
        "getopts" => builtin_getopts(words),
        "hash" => builtin_hash(words),
        "history" => builtin_history(),
        "kill" => builtin_kill(words),
        "local" => builtin_local(words),
        "typeset" | "declare" => builtin_typeset(words),
        "pwd" => builtin_pwd(words),
        "read" => builtin_read(words),
        "setopt" => builtin_setopt(words, true),
        "unsetopt" => builtin_setopt(words, false),
        "test" | "[" => crate::testexpr::builtin_test(words),
        "type" => builtin_type(words),
        "ulimit" => builtin_ulimit(words),
        "umask" => builtin_umask(words),
        "zstyle" => builtin_zstyle(&words[1..]),
        // Accepted for zsh startup-file compatibility; no effect.
        "add-zsh-hook" | "bindkey" | "compinit" | "compdef" | "compaudit" | "zmodload"
        | "emulate" => 0,
        _ => {
            err_line(&format!("meowsh: {}: not a builtin", name));
            1
        }
    }
}

/// Quotes a value so the shell reads it back unchanged.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn usage(name: &str, msg: &str) -> i32 {
    err_line(&format!("meowsh: {}: {}", name, msg));
    2
}

// ---- control flow ---------------------------------------------------------

fn builtin_break(words: &[String], is_break: bool) -> i32 {
    let name = &words[0];
    let n = match words.get(1) {
        None => 1,
        Some(a) => match a.parse::<usize>() {
            Ok(n) if n >= 1 => n,
            _ => {
                err_line(&format!("meowsh: {}: {}: loop count out of range", name, a));
                return 1;
            }
        },
    };
    let depth = with_shell(|s| s.loop_depth);
    if depth == 0 {
        err_line(&format!(
            "meowsh: {}: only meaningful in a `for', `while', or `until' loop",
            name
        ));
        return 0;
    }
    let n = n.min(depth);
    set_flow(if is_break {
        Flow::Break(n)
    } else {
        Flow::Continue(n)
    });
    0
}

fn builtin_exit(words: &[String]) -> i32 {
    let code = match words.get(1) {
        None => last_status(),
        Some(a) => match a.trim().parse::<i64>() {
            Ok(n) => (n & 0xff) as i32,
            Err(_) => {
                err_line(&format!("meowsh: exit: {}: numeric argument required", a));
                2
            }
        },
    };
    set_flow(Flow::Exit(code));
    code
}

fn builtin_return(words: &[String]) -> i32 {
    let (funcs, sources) = with_shell(|s| (s.func_depth, s.source_depth));
    if funcs == 0 && sources == 0 {
        err_line("meowsh: return: can only `return' from a function or sourced script");
        return 1;
    }
    let code = match words.get(1) {
        None => last_status(),
        Some(a) => match a.trim().parse::<i64>() {
            Ok(n) => (n & 0xff) as i32,
            Err(_) => {
                err_line(&format!("meowsh: return: {}: numeric argument required", a));
                2
            }
        },
    };
    set_flow(Flow::Return(code));
    code
}

fn is_zsh_completion_script(content: &str) -> bool {
    content
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("#compdef") || trimmed.starts_with("#autoload")
        })
        .unwrap_or(false)
}

fn find_sourced(file: &str) -> Option<String> {
    if file.contains('/') {
        return Some(file.to_string());
    }
    for dir in var_get("PATH").split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(file);
        if p.is_file() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    if std::path::Path::new(file).is_file() {
        return Some(file.to_string());
    }
    None
}

/// Runs a file in the current shell, as `.` does.
pub fn source_file(path: &str, args: Option<&[String]>) -> i32 {
    let content = match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            err_line(&format!(
                "meowsh: {}: {}",
                path,
                strerror(e.raw_os_error().unwrap_or(0))
            ));
            return 1;
        }
    };
    if is_zsh_completion_script(&content) {
        return 0;
    }
    let saved = args.map(|a| with_shell(|s| std::mem::replace(&mut s.pos_params, a.to_vec())));
    with_shell(|s| s.source_depth += 1);
    let mut st = run_string(&content);
    with_shell(|s| {
        s.source_depth -= 1;
        if let Flow::Return(n) = s.flow {
            st = n;
            s.flow = Flow::None;
        }
        if let Some(p) = saved {
            s.pos_params = p;
        }
    });
    st
}

fn builtin_source(words: &[String]) -> i32 {
    let Some(file) = words.get(1) else {
        return usage(&words[0], "filename argument required");
    };
    let Some(path) = find_sourced(file) else {
        err_line(&format!("meowsh: {}: {}: not found", words[0], file));
        return 1;
    };
    let args = if words.len() > 2 {
        Some(&words[2..])
    } else {
        None
    };
    source_file(&path, args)
}

// ---- variables ------------------------------------------------------------

fn split_assignment(arg: &str) -> (&str, Option<&str>, bool) {
    match arg.find('=') {
        Some(i) => {
            let (name, append) = match arg[..i].strip_suffix('+') {
                Some(n) => (n, true),
                None => (&arg[..i], false),
            };
            (name, Some(&arg[i + 1..]), append)
        }
        None => (arg, None, false),
    }
}

fn list_vars(prefix: &str, flag: u32) {
    let mut vars: Vec<(String, String)> = with_shell(|s| {
        s.vars
            .iter()
            .filter(|(_, v)| flag == 0 || v.flags & flag != 0)
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    });
    vars.sort();
    let mut text = String::new();
    for (k, v) in vars {
        if prefix.is_empty() {
            text.push_str(&format!("{}={}\n", k, shell_quote(&v)));
        } else {
            text.push_str(&format!("{} {}={}\n", prefix, k, shell_quote(&v)));
        }
    }
    out(&text);
}

fn set_flag(name: &str, flag: u32, on: bool) {
    with_shell(|s| {
        if on {
            s.vars
                .entry(name.to_string())
                .or_insert(Var {
                    value: String::new(),
                    flags: 0,
                })
                .flags |= flag;
        } else if let Some(v) = s.vars.get_mut(name) {
            v.flags &= !flag;
        }
    });
}

fn builtin_export(words: &[String]) -> i32 {
    let mut unexport = false;
    let mut names = Vec::new();
    for a in &words[1..] {
        match a.as_str() {
            "-p" | "--" => {}
            "-n" => unexport = true,
            _ => names.push(a),
        }
    }
    if names.is_empty() {
        list_vars("export", VAR_EXPORT);
        return 0;
    }
    let mut status = 0;
    for a in names {
        let (name, value, append) = split_assignment(a);
        if !is_valid_name(name) {
            err_line(&format!("meowsh: export: `{}': not a valid identifier", a));
            status = 1;
            continue;
        }
        if let Some(v) = value {
            let v = if append {
                format!("{}{}", var_get(name), v)
            } else {
                v.to_string()
            };
            if !var_set(name, &v, !unexport) {
                status = 1;
                continue;
            }
        }
        set_flag(name, VAR_EXPORT, !unexport);
    }
    status
}

fn builtin_readonly(words: &[String]) -> i32 {
    let names: Vec<&String> = words[1..]
        .iter()
        .filter(|a| a.as_str() != "-p" && a.as_str() != "--")
        .collect();
    if names.is_empty() {
        list_vars("readonly", VAR_READONLY);
        return 0;
    }
    let mut status = 0;
    for a in names {
        let (name, value, _) = split_assignment(a);
        if !is_valid_name(name) {
            err_line(&format!(
                "meowsh: readonly: `{}': not a valid identifier",
                a
            ));
            status = 1;
            continue;
        }
        if let Some(v) = value {
            if !var_set(name, v, false) {
                status = 1;
                continue;
            }
        }
        set_flag(name, VAR_READONLY, true);
    }
    status
}

fn builtin_unset(words: &[String]) -> i32 {
    let mut functions = false;
    let mut status = 0;
    for a in &words[1..] {
        match a.as_str() {
            "-f" => {
                functions = true;
                continue;
            }
            "-v" => {
                functions = false;
                continue;
            }
            _ => {}
        }
        if functions {
            with_shell(|s| s.functions.remove(a.as_str()));
            continue;
        }
        let readonly = with_shell(|s| {
            s.vars
                .get(a.as_str())
                .map(|v| v.flags & VAR_READONLY != 0)
                .unwrap_or(false)
        });
        if readonly {
            err_line(&format!(
                "meowsh: unset: {}: cannot unset: readonly variable",
                a
            ));
            status = 1;
            continue;
        }
        with_shell(|s| {
            s.vars.remove(a.as_str());
            s.arrays.remove(a.as_str());
            s.assoc_arrays.remove(a.as_str());
        });
    }
    status
}

pub fn option_letter(c: char) -> Option<u32> {
    Some(match c {
        'a' => OPT_ALLEXPORT,
        'e' => OPT_ERREXIT,
        'f' => OPT_NOGLOB,
        'h' => OPT_HASHALL,
        'm' => OPT_MONITOR,
        'n' => OPT_NOEXEC,
        'u' => OPT_NOUNSET,
        'v' => OPT_VERBOSE,
        'x' => OPT_XTRACE,
        'C' => OPT_NOCLOBBER,
        _ => return None,
    })
}

pub const NAMED_OPTIONS: &[(&str, u32)] = &[
    ("allexport", OPT_ALLEXPORT),
    ("errexit", OPT_ERREXIT),
    ("hashall", OPT_HASHALL),
    ("monitor", OPT_MONITOR),
    ("noclobber", OPT_NOCLOBBER),
    ("noexec", OPT_NOEXEC),
    ("noglob", OPT_NOGLOB),
    ("nounset", OPT_NOUNSET),
    ("pipefail", OPT_PIPEFAIL),
    ("verbose", OPT_VERBOSE),
    ("xtrace", OPT_XTRACE),
];

fn set_opt(flag: u32, on: bool) {
    with_shell(|s| {
        if on {
            s.opts |= flag;
        } else {
            s.opts &= !flag;
        }
    });
}

fn builtin_set(words: &[String]) -> i32 {
    let args = &words[1..];
    if args.is_empty() {
        list_vars("", 0);
        return 0;
    }
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            with_shell(|s| s.pos_params = args[i + 1..].to_vec());
            return 0;
        }
        if a == "-" {
            set_opt(OPT_XTRACE | OPT_VERBOSE, false);
            if i + 1 < args.len() {
                with_shell(|s| s.pos_params = args[i + 1..].to_vec());
            }
            return 0;
        }
        if (a.starts_with('-') || a.starts_with('+')) && a.len() > 1 {
            let on = a.starts_with('-');
            for c in a[1..].chars() {
                if c == 'o' {
                    match args.get(i + 1) {
                        Some(name) => {
                            i += 1;
                            match NAMED_OPTIONS.iter().find(|(n, _)| n == name) {
                                Some((_, f)) => set_opt(*f, on),
                                None if matches!(name.as_str(), "vi" | "emacs" | "ignoreeof") => {}
                                None => {
                                    return usage("set", &format!("{}: invalid option name", name))
                                }
                            }
                        }
                        None => {
                            let opts = with_shell(|s| s.opts);
                            let mut text = String::new();
                            for (n, f) in NAMED_OPTIONS {
                                if on {
                                    let state = if opts & f != 0 { "on" } else { "off" };
                                    text.push_str(&format!("{:<15} {}\n", n, state));
                                } else {
                                    let sign = if opts & f != 0 { '-' } else { '+' };
                                    text.push_str(&format!("set {}o {}\n", sign, n));
                                }
                            }
                            out(&text);
                        }
                    }
                    continue;
                }
                match option_letter(c) {
                    Some(f) => set_opt(f, on),
                    None => {
                        return usage(
                            "set",
                            &format!("{}{}: invalid option", if on { '-' } else { '+' }, c),
                        )
                    }
                }
            }
            i += 1;
            continue;
        }
        with_shell(|s| s.pos_params = args[i..].to_vec());
        return 0;
    }
    0
}

fn builtin_shift(words: &[String]) -> i32 {
    let n = match words.get(1) {
        None => 1,
        Some(a) => match a.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                err_line(&format!("meowsh: shift: {}: numeric argument required", a));
                return 1;
            }
        },
    };
    with_shell(|s| {
        if n > s.pos_params.len() {
            err_line("meowsh: shift: can't shift that many");
            return 1;
        }
        s.pos_params.drain(..n);
        0
    })
}

fn builtin_local(words: &[String]) -> i32 {
    if !in_function_scope() {
        err_line("meowsh: local: can only be used in a function");
        return 1;
    }
    for arg in &words[1..] {
        if arg.starts_with('-') {
            continue;
        }
        let (name, value, _) = split_assignment(arg);
        if !is_valid_name(name) {
            err_line(&format!("meowsh: local: `{}': not a valid identifier", arg));
            return 1;
        }
        declare_local(name);
        // Bare `local foo` masks any global with an empty value for the
        // duration of the function.
        if !var_set(name, value.unwrap_or(""), false) {
            return 1;
        }
    }
    0
}

fn builtin_typeset(words: &[String]) -> i32 {
    let mut export = false;
    let mut readonly = false;
    let mut unique = false;
    let mut array = false;
    let mut associative = false;
    let mut print = false;
    let mut names = Vec::new();

    for arg in &words[1..] {
        if arg == "--" {
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 && !arg.contains('=') {
            for flag in arg.chars().skip(1) {
                match flag {
                    'a' => array = true,
                    'A' => associative = true,
                    'r' => readonly = true,
                    'U' => unique = true,
                    'x' => export = true,
                    'p' => print = true,
                    // Other zsh modifiers do not affect storage here.
                    _ => {}
                }
            }
            continue;
        }
        if arg.starts_with('+') && arg.len() > 1 && !arg.contains('=') {
            for flag in arg.chars().skip(1) {
                match flag {
                    'x' => export = false,
                    'r' => readonly = false,
                    'U' => unique = false,
                    _ => {}
                }
            }
            continue;
        }
        names.push(arg.clone());
    }

    if print || names.is_empty() {
        print_typeset_state(&names);
        return 0;
    }

    let local = in_function_scope();
    for spec in names {
        let (name, value, _) = split_assignment(&spec);
        if !is_valid_name(name) {
            err_line(&format!(
                "meowsh: typeset: `{}': not a valid identifier",
                spec
            ));
            return 1;
        }
        if local && !array && !associative && !unique {
            declare_local(name);
        }
        if array && !array_declare(name) {
            return 1;
        }
        if associative && !assoc_declare(name) {
            return 1;
        }
        if unique && !array_make_unique(name) {
            return 1;
        }
        if let Some(value) = value {
            if !var_set(name, value, export) {
                return 1;
            }
        } else if export {
            set_flag(name, VAR_EXPORT, true);
        }
        if readonly {
            set_flag(name, VAR_READONLY, true);
        }
    }
    0
}

fn print_typeset_state(names: &[String]) {
    let mut text = String::new();
    with_shell(|shell| {
        let fmt_assoc = |items: &std::collections::BTreeMap<String, String>| {
            items
                .iter()
                .flat_map(|(k, v)| [k.clone(), v.clone()])
                .collect::<Vec<_>>()
                .join(" ")
        };
        if names.is_empty() {
            let mut vars: Vec<_> = shell.vars.iter().collect();
            vars.sort_by(|a, b| a.0.cmp(b.0));
            for (name, var) in vars {
                text.push_str(&format!("typeset {}={}\n", name, shell_quote(&var.value)));
            }
            let mut arrays: Vec<_> = shell.arrays.iter().collect();
            arrays.sort_by(|a, b| a.0.cmp(b.0));
            for (name, items) in arrays {
                text.push_str(&format!("typeset -a {}=({})\n", name, items.join(" ")));
            }
            let mut assoc: Vec<_> = shell.assoc_arrays.iter().collect();
            assoc.sort_by(|a, b| a.0.cmp(b.0));
            for (name, items) in assoc {
                text.push_str(&format!("typeset -A {}=({})\n", name, fmt_assoc(items)));
            }
            return;
        }
        for name in names {
            if let Some(items) = shell.arrays.get(name) {
                text.push_str(&format!("typeset -a {}=({})\n", name, items.join(" ")));
            } else if let Some(items) = shell.assoc_arrays.get(name) {
                text.push_str(&format!("typeset -A {}=({})\n", name, fmt_assoc(items)));
            } else if let Some(var) = shell.vars.get(name) {
                text.push_str(&format!("typeset {}={}\n", name, shell_quote(&var.value)));
            }
        }
    });
    out(&text);
}

// ---- read -----------------------------------------------------------------

// Splits `chars` (character, escaped) into at most `n` fields by IFS; the
// last field takes the rest of the line. `n == 0` means no limit.
fn read_split(chars: &[(char, bool)], ifs: &str, n: usize) -> Vec<String> {
    let is_ws = |c: char| matches!(c, ' ' | '\t' | '\n') && ifs.contains(c);
    let is_delim = |&(c, esc): &(char, bool)| !esc && ifs.contains(c);
    let len = chars.len();
    let mut i = 0;
    while i < len && is_delim(&chars[i]) && is_ws(chars[i].0) {
        i += 1;
    }
    let mut out = Vec::new();
    while i < len && (n == 0 || out.len() + 1 < n) {
        let mut f = String::new();
        while i < len && !is_delim(&chars[i]) {
            f.push(chars[i].0);
            i += 1;
        }
        out.push(f);
        while i < len && is_delim(&chars[i]) && is_ws(chars[i].0) {
            i += 1;
        }
        if i < len && is_delim(&chars[i]) && !is_ws(chars[i].0) {
            i += 1;
            while i < len && is_delim(&chars[i]) && is_ws(chars[i].0) {
                i += 1;
            }
        }
    }
    if n != 0 && i < len {
        let mut end = len;
        while end > i && is_delim(&chars[end - 1]) && is_ws(chars[end - 1].0) {
            end -= 1;
        }
        out.push(chars[i..end].iter().map(|(c, _)| *c).collect());
    }
    while n != 0 && out.len() < n {
        out.push(String::new());
    }
    out
}

fn builtin_read(words: &[String]) -> i32 {
    let mut raw = false;
    let mut prompt = None;
    let mut delim = b'\n';
    let mut array: Option<String> = None;
    let mut i = 1;
    while i < words.len() && words[i].starts_with('-') && words[i].len() > 1 {
        let a = &words[i];
        if a == "--" {
            i += 1;
            break;
        }
        let mut chars = a[1..].chars();
        while let Some(c) = chars.next() {
            let mut take_arg = || -> Option<String> {
                let rest: String = chars.by_ref().collect();
                if !rest.is_empty() {
                    Some(rest)
                } else {
                    i += 1;
                    words.get(i).cloned()
                }
            };
            match c {
                'r' => raw = true,
                's' => {}
                'p' => prompt = take_arg(),
                'd' => delim = take_arg().and_then(|d| d.bytes().next()).unwrap_or(0),
                'a' => array = take_arg(),
                _ => return usage("read", &format!("-{}: invalid option", c)),
            }
        }
        i += 1;
    }
    let names: Vec<String> = words[i..].to_vec();
    for n in &names {
        if !is_valid_name(n) {
            err_line(&format!("meowsh: read: `{}': not a valid identifier", n));
            return 2;
        }
    }
    if let Some(p) = prompt {
        if unsafe { libc::isatty(0) } == 1 {
            let _ = crate::sys::write_fd(2, p.as_bytes());
        }
    }

    let mut bytes: Vec<u8> = Vec::new();
    let mut escaped: Vec<bool> = Vec::new();
    let mut got_delim = false;
    let mut pending_backslash = false;
    loop {
        match read_byte(0) {
            Ok(Some(b)) => {
                if pending_backslash {
                    pending_backslash = false;
                    if b == b'\n' {
                        continue;
                    }
                    bytes.push(b);
                    escaped.push(true);
                    continue;
                }
                if b == delim {
                    got_delim = true;
                    break;
                }
                if b == b'\\' && !raw {
                    pending_backslash = true;
                    continue;
                }
                bytes.push(b);
                escaped.push(false);
            }
            Ok(None) => break,
            Err(_) => return 128 + libc::SIGINT,
        }
    }

    let chars: Vec<(char, bool)> = match std::str::from_utf8(&bytes) {
        Ok(s) => {
            let mut out = Vec::new();
            let mut idx = 0;
            for c in s.chars() {
                out.push((c, escaped[idx]));
                idx += c.len_utf8();
            }
            out
        }
        Err(_) => String::from_utf8_lossy(&bytes)
            .chars()
            .map(|c| (c, false))
            .collect(),
    };
    let ifs = var_get_opt("IFS").unwrap_or_else(|| " \t\n".to_string());

    let ok = if let Some(arr) = array {
        array_set(&arr, read_split(&chars, &ifs, 0))
    } else if names.is_empty() {
        var_set(
            "REPLY",
            &chars.iter().map(|(c, _)| *c).collect::<String>(),
            false,
        )
    } else {
        let fields = read_split(&chars, &ifs, names.len());
        names.iter().zip(fields).all(|(n, v)| var_set(n, &v, false))
    };
    if !ok {
        return 2;
    }
    if got_delim {
        0
    } else {
        1
    }
}

// ---- getopts --------------------------------------------------------------

fn builtin_getopts(words: &[String]) -> i32 {
    if words.len() < 3 {
        return usage("getopts", "usage: getopts optstring name [arg ...]");
    }
    let optstring = &words[1];
    let name = &words[2];
    let args: Vec<String> = if words.len() > 3 {
        words[3..].to_vec()
    } else {
        with_shell(|s| s.pos_params.clone())
    };
    let silent = optstring.starts_with(':');
    let spec: Vec<char> = optstring.trim_start_matches(':').chars().collect();
    let mut optind: usize = var_get("OPTIND").trim().parse().unwrap_or(1).max(1);
    let mut pos = with_shell(|s| s.getopts_pos);

    let finish = |optind: usize, pos: usize| {
        var_set("OPTIND", &optind.to_string(), false);
        with_shell(|s| s.getopts_pos = pos);
    };

    if optind > args.len() {
        var_set(name, "?", false);
        finish(optind, 0);
        return 1;
    }
    let arg: Vec<char> = args[optind - 1].chars().collect();
    if pos == 0 || pos >= arg.len() {
        if args[optind - 1] == "--" {
            var_set(name, "?", false);
            finish(optind + 1, 0);
            return 1;
        }
        if arg.len() < 2 || arg[0] != '-' {
            var_set(name, "?", false);
            finish(optind, 0);
            return 1;
        }
        pos = 1;
    }
    let c = arg[pos];
    pos += 1;
    let at_end = pos >= arg.len();
    let idx = spec.iter().position(|&x| x == c && x != ':');
    match idx {
        None => {
            if silent {
                var_set("OPTARG", &c.to_string(), false);
            } else {
                err_line(&format!("meowsh: illegal option -- {}", c));
                with_shell(|s| s.vars.remove("OPTARG"));
            }
            var_set(name, "?", false);
        }
        Some(k) if spec.get(k + 1) == Some(&':') => {
            let optarg = if !at_end {
                let v: String = arg[pos..].iter().collect();
                optind += 1;
                pos = 0;
                Some(v)
            } else if optind < args.len() {
                let v = args[optind].clone();
                optind += 2;
                pos = 0;
                Some(v)
            } else {
                None
            };
            match optarg {
                Some(v) => {
                    var_set("OPTARG", &v, false);
                    var_set(name, &c.to_string(), false);
                }
                None => {
                    if silent {
                        var_set("OPTARG", &c.to_string(), false);
                        var_set(name, ":", false);
                    } else {
                        err_line(&format!("meowsh: option requires an argument -- {}", c));
                        with_shell(|s| s.vars.remove("OPTARG"));
                        var_set(name, "?", false);
                    }
                    optind += 1;
                    pos = 0;
                }
            }
            finish(optind, pos);
            return 0;
        }
        Some(_) => {
            with_shell(|s| s.vars.remove("OPTARG"));
            var_set(name, &c.to_string(), false);
        }
    }
    if at_end {
        optind += 1;
        pos = 0;
    }
    finish(optind, pos);
    0
}

// ---- directories ----------------------------------------------------------

fn normalize_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    format!("/{}", parts.join("/"))
}

fn physical_cwd() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

fn chdir(path: &str) -> Result<(), i32> {
    let c = std::ffi::CString::new(path).map_err(|_| libc::EINVAL)?;
    if unsafe { libc::chdir(c.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    Ok(())
}

fn builtin_cd(words: &[String]) -> i32 {
    let mut physical = false;
    let mut i = 1;
    while i < words.len() && words[i].starts_with('-') && words[i].len() > 1 {
        match words[i].as_str() {
            "-L" => physical = false,
            "-P" => physical = true,
            "--" => {
                i += 1;
                break;
            }
            o => return usage("cd", &format!("{}: invalid option", o)),
        }
        i += 1;
    }
    let mut print = false;
    let target = match words.get(i).map(|s| s.as_str()) {
        None => match var_get_opt("HOME").filter(|h| !h.is_empty()) {
            Some(h) => h,
            None => {
                err_line("meowsh: cd: HOME not set");
                return 1;
            }
        },
        Some("-") => match var_get_opt("OLDPWD").filter(|h| !h.is_empty()) {
            Some(d) => {
                print = true;
                d
            }
            None => {
                err_line("meowsh: cd: OLDPWD not set");
                return 1;
            }
        },
        Some(d) => d.to_string(),
    };
    let mut dir = target.clone();
    let relative_name = !target.starts_with('/')
        && target != "."
        && target != ".."
        && !target.starts_with("./")
        && !target.starts_with("../");
    if relative_name {
        if let Some(cdpath) = var_get_opt("CDPATH") {
            for p in cdpath.split(':') {
                let cand = if p.is_empty() {
                    format!("./{}", target)
                } else {
                    format!("{}/{}", p.trim_end_matches('/'), target)
                };
                if std::path::Path::new(&cand).is_dir() {
                    if !p.is_empty() {
                        print = true;
                    }
                    dir = cand;
                    break;
                }
            }
        }
    }
    let old = var_get_opt("PWD")
        .filter(|p| p.starts_with('/'))
        .or_else(physical_cwd)
        .unwrap_or_default();
    let new = if physical {
        if let Err(e) = chdir(&dir) {
            err_line(&format!("meowsh: cd: {}: {}", target, strerror(e)));
            return 1;
        }
        physical_cwd().unwrap_or(dir)
    } else {
        let logical = if dir.starts_with('/') {
            normalize_path(&dir)
        } else {
            normalize_path(&format!("{}/{}", old, dir))
        };
        if let Err(e) = chdir(&logical) {
            err_line(&format!("meowsh: cd: {}: {}", target, strerror(e)));
            return 1;
        }
        logical
    };
    var_set("OLDPWD", &old, false);
    var_set("PWD", &new, false);
    if print {
        out(&format!("{}\n", new));
    }
    0
}

fn builtin_pwd(words: &[String]) -> i32 {
    let physical = words.iter().skip(1).any(|a| a == "-P");
    let pwd = var_get_opt("PWD").filter(|p| p.starts_with('/'));
    let dir = match (physical, pwd) {
        (false, Some(p)) => p,
        _ => match physical_cwd() {
            Some(p) => p,
            None => {
                err_line("meowsh: pwd: cannot determine current directory");
                return 1;
            }
        },
    };
    if out(&format!("{}\n", dir)) {
        0
    } else {
        1
    }
}

fn builtin_pushd(words: &[String]) -> i32 {
    let mut popped = None;
    let target = match words.get(1) {
        Some(t) => t.clone(),
        None => match with_shell(|s| s.dir_stack.pop()) {
            Some(dir) => {
                popped = Some(dir.clone());
                dir
            }
            None => {
                err_line("meowsh: pushd: directory stack empty");
                return 1;
            }
        },
    };
    let old = var_get("PWD");
    let status = builtin_cd(&["cd".to_string(), target]);
    if status == 0 {
        with_shell(|s| s.dir_stack.push(old));
        print_dirs();
    } else if let Some(dir) = popped {
        with_shell(|s| s.dir_stack.push(dir));
    }
    status
}

fn builtin_popd() -> i32 {
    let Some(target) = with_shell(|s| s.dir_stack.pop()) else {
        err_line("meowsh: popd: directory stack empty");
        return 1;
    };
    let status = builtin_cd(&["cd".to_string(), target.clone()]);
    if status == 0 {
        print_dirs();
    } else {
        with_shell(|s| s.dir_stack.push(target));
    }
    status
}

fn print_dirs() {
    let mut dirs = vec![var_get("PWD")];
    dirs.extend(with_shell(|s| s.dir_stack.clone()).into_iter().rev());
    out(&format!("{}\n", dirs.join(" ")));
}

// ---- output ---------------------------------------------------------------

fn builtin_echo(words: &[String]) -> i32 {
    let mut args = &words[1..];
    let mut newline = true;
    let mut escapes = false;
    while let Some(a) = args.first() {
        let flags = a.strip_prefix('-').unwrap_or("");
        if !flags.is_empty() && flags.chars().all(|c| matches!(c, 'n' | 'e' | 'E')) {
            for c in flags.chars() {
                match c {
                    'n' => newline = false,
                    'e' => escapes = true,
                    _ => escapes = false,
                }
            }
            args = &args[1..];
        } else {
            break;
        }
    }
    let mut text = String::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        if escapes {
            if let Some(cut) = a.find("\\c") {
                text.push_str(&crate::expand::decode_escapes(&a[..cut]));
                newline = false;
                break;
            }
            text.push_str(&crate::expand::decode_escapes(a));
        } else {
            text.push_str(a);
        }
    }
    if newline {
        text.push('\n');
    }
    if out(&text) {
        0
    } else {
        1
    }
}

fn builtin_history() -> i32 {
    let lines = with_shell(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).clone());
    let mut text = String::new();
    for (i, line) in lines.iter().enumerate() {
        text.push_str(&format!("{:5}  {}\n", i + 1, line));
    }
    out(&text);
    0
}

// ---- aliases --------------------------------------------------------------

fn builtin_alias(words: &[String]) -> i32 {
    let args = &words[1..];
    if args.is_empty() || (args.len() == 1 && args[0] == "-p") {
        let mut all: Vec<(String, String)> = with_shell(|s| {
            s.aliases
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        });
        all.sort();
        let text: String = all
            .iter()
            .map(|(k, v)| format!("alias {}={}\n", k, shell_quote(v)))
            .collect();
        out(&text);
        return 0;
    }
    let mut status = 0;
    for a in args {
        match a.find('=') {
            Some(i) => {
                let (name, value) = (&a[..i], &a[i + 1..]);
                with_shell(|s| s.aliases.insert(name.to_string(), value.to_string()));
            }
            None => match with_shell(|s| s.aliases.get(a.as_str()).cloned()) {
                Some(v) => {
                    out(&format!("alias {}={}\n", a, shell_quote(&v)));
                }
                None => {
                    err_line(&format!("meowsh: alias: {}: not found", a));
                    status = 1;
                }
            },
        }
    }
    status
}

fn builtin_unalias(words: &[String]) -> i32 {
    let mut status = 0;
    for a in &words[1..] {
        if a == "-a" {
            with_shell(|s| s.aliases.clear());
            continue;
        }
        if with_shell(|s| s.aliases.remove(a.as_str())).is_none() {
            err_line(&format!("meowsh: unalias: {}: not found", a));
            status = 1;
        }
    }
    status
}

// ---- command lookup -------------------------------------------------------

fn describe_command(name: &str, verbose: bool) -> Option<String> {
    if let Some(v) = with_shell(|s| s.aliases.get(name).cloned()) {
        return Some(if verbose {
            format!("{} is an alias for {}", name, v)
        } else {
            format!("alias {}={}", name, shell_quote(&v))
        });
    }
    if KEYWORDS.contains(&name) {
        return Some(if verbose {
            format!("{} is a shell keyword", name)
        } else {
            name.to_string()
        });
    }
    if with_shell(|s| s.functions.contains_key(name)) {
        return Some(if verbose {
            format!("{} is a function", name)
        } else {
            name.to_string()
        });
    }
    if is_special_builtin(name) {
        return Some(if verbose {
            format!("{} is a special shell builtin", name)
        } else {
            name.to_string()
        });
    }
    if is_builtin(name) {
        return Some(if verbose {
            format!("{} is a shell builtin", name)
        } else {
            name.to_string()
        });
    }
    find_command(name).map(|p| {
        if verbose {
            format!("{} is {}", name, p)
        } else {
            p
        }
    })
}

fn builtin_type(words: &[String]) -> i32 {
    let mut status = 0;
    for name in &words[1..] {
        match describe_command(name, true) {
            Some(d) => {
                out(&format!("{}\n", d));
            }
            None => {
                err_line(&format!("meowsh: type: {}: not found", name));
                status = 1;
            }
        }
    }
    status
}

// Only `command -v` / `-V` reach here; plain `command` is handled by the
// executor.
fn builtin_command(words: &[String]) -> i32 {
    let mut verbose = None;
    let mut i = 1;
    while i < words.len() && words[i].starts_with('-') {
        match words[i].as_str() {
            "-v" => verbose = Some(false),
            "-V" => verbose = Some(true),
            "-p" => {}
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
        i += 1;
    }
    let Some(verbose) = verbose else {
        return 0;
    };
    let mut status = 0;
    for name in &words[i..] {
        match describe_command(name, verbose) {
            Some(d) => {
                out(&format!("{}\n", d));
            }
            None => {
                if verbose {
                    err_line(&format!("meowsh: command: {}: not found", name));
                }
                status = 1;
            }
        }
    }
    status
}

fn builtin_hash(words: &[String]) -> i32 {
    let mut status = 0;
    for name in &words[1..] {
        if name.starts_with('-') {
            continue;
        }
        if find_command(name).is_none() && !is_builtin(name) {
            err_line(&format!("meowsh: hash: {}: not found", name));
            status = 1;
        }
    }
    status
}

// ---- processes and signals ------------------------------------------------

fn builtin_kill(words: &[String]) -> i32 {
    let args = &words[1..];
    if args.is_empty() {
        return usage(
            "kill",
            "usage: kill [-s sigspec | -signum] pid | jobspec ...",
        );
    }
    if args[0] == "-l" || args[0] == "-L" {
        if args.len() == 1 {
            let names: Vec<&str> = SIGNALS.iter().map(|(n, _)| *n).collect();
            out(&format!("{}\n", names.join(" ")));
            return 0;
        }
        let mut status = 0;
        for a in &args[1..] {
            match a.parse::<i32>() {
                Ok(n) => {
                    let n = if n > 128 { n - 128 } else { n };
                    out(&format!("{}\n", signal_name(n)));
                }
                Err(_) => match signal_number(a) {
                    Some(n) => {
                        out(&format!("{}\n", n));
                    }
                    None => {
                        err_line(&format!(
                            "meowsh: kill: {}: invalid signal specification",
                            a
                        ));
                        status = 1;
                    }
                },
            }
        }
        return status;
    }
    let mut sig = libc::SIGTERM;
    let mut i = 0;
    if args[0] == "-s" || args[0] == "-n" {
        let Some(spec) = args.get(1) else {
            return usage("kill", "option requires an argument");
        };
        match signal_number(spec) {
            Some(s) => sig = s,
            None => return usage("kill", &format!("{}: invalid signal specification", spec)),
        }
        i = 2;
    } else if args[0].starts_with('-') && args[0] != "--" && args[0].len() > 1 {
        match signal_number(&args[0][1..]) {
            Some(s) => sig = s,
            None => {
                return usage(
                    "kill",
                    &format!("{}: invalid signal specification", &args[0][1..]),
                )
            }
        }
        i = 1;
    }
    if args.get(i).map(|s| s.as_str()) == Some("--") {
        i += 1;
    }
    let mut status = 0;
    for spec in &args[i..] {
        match kill_targets(spec) {
            Ok(pids) => {
                for pid in pids {
                    if unsafe { libc::kill(pid, sig) } != 0 {
                        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                        err_line(&format!("meowsh: kill: ({}) - {}", pid, strerror(e)));
                        status = 1;
                    }
                }
            }
            Err(e) => {
                err_line(&format!("meowsh: kill: {}", e));
                status = 1;
            }
        }
    }
    status
}

fn builtin_trap(words: &[String]) -> i32 {
    let mut args = &words[1..];
    if args.first().map(|s| s.as_str()) == Some("--") {
        args = &args[1..];
    }
    if args.is_empty() || args[0] == "-p" {
        let mut traps: Vec<(i32, String)> =
            with_shell(|s| s.traps.iter().map(|(k, v)| (*k, v.clone())).collect());
        traps.sort();
        let text: String = traps
            .iter()
            .map(|(sig, a)| format!("trap -- {} {}\n", shell_quote(a), signal_name(*sig)))
            .collect();
        out(&text);
        return 0;
    }
    if args[0] == "-l" {
        let text: String = SIGNALS
            .iter()
            .map(|(n, s)| format!("{}) SIG{}\n", s, n))
            .collect();
        out(&text);
        return 0;
    }
    let first_is_number = args[0].parse::<u32>().is_ok();
    let (action, conds): (Option<String>, &[String]) = if first_is_number || args.len() == 1 {
        (None, args)
    } else if args[0] == "-" {
        (None, &args[1..])
    } else {
        (Some(args[0].clone()), &args[1..])
    };
    let mut status = 0;
    for c in conds {
        match signal_number(c) {
            Some(sig) if sig == libc::SIGKILL || sig == libc::SIGSTOP => {
                err_line(&format!("meowsh: trap: {}: cannot trap", c));
                status = 1;
            }
            Some(sig) => set_trap(sig, action.clone()),
            None => {
                err_line(&format!(
                    "meowsh: trap: {}: invalid signal specification",
                    c
                ));
                status = 1;
            }
        }
    }
    status
}

fn fmt_time(tv: libc::timeval) -> String {
    let secs = tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    format!("{}m{:.3}s", (secs / 60.0) as i64, secs % 60.0)
}

fn builtin_times() -> i32 {
    let mut me: libc::rusage = unsafe { std::mem::zeroed() };
    let mut kids: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut me);
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut kids);
    }
    out(&format!(
        "{} {}\n{} {}\n",
        fmt_time(me.ru_utime),
        fmt_time(me.ru_stime),
        fmt_time(kids.ru_utime),
        fmt_time(kids.ru_stime)
    ));
    0
}

fn builtin_umask(words: &[String]) -> i32 {
    let mut symbolic = false;
    let mut args = Vec::new();
    for a in &words[1..] {
        if a == "-S" {
            symbolic = true;
        } else {
            args.push(a.clone());
        }
    }
    let current = unsafe {
        let m = libc::umask(0);
        libc::umask(m);
        m as u32
    };
    let Some(spec) = args.first() else {
        if symbolic {
            let perms = |shift: u32| {
                let bits = !(current >> shift) & 7;
                let mut s = String::new();
                if bits & 4 != 0 {
                    s.push('r');
                }
                if bits & 2 != 0 {
                    s.push('w');
                }
                if bits & 1 != 0 {
                    s.push('x');
                }
                s
            };
            out(&format!("u={},g={},o={}\n", perms(6), perms(3), perms(0)));
        } else {
            out(&format!("{:04o}\n", current));
        }
        return 0;
    };
    if spec.chars().all(|c| c.is_digit(8)) {
        match u32::from_str_radix(spec, 8) {
            Ok(m) if m <= 0o777 => {
                unsafe {
                    libc::umask(m as libc::mode_t);
                }
                return 0;
            }
            _ => return usage("umask", &format!("{}: octal number out of range", spec)),
        }
    }
    // Symbolic mode: [ugoa]*[+-=][rwx]*, comma separated, applied to the
    // permissions the mask allows.
    let mut allowed = !current & 0o777;
    for clause in spec.split(',') {
        let who_end = clause.find(['+', '-', '=']).unwrap_or(clause.len());
        let who = &clause[..who_end];
        if who_end >= clause.len() || !who.chars().all(|c| "ugoa".contains(c)) {
            return usage("umask", &format!("{}: invalid symbolic mode", spec));
        }
        let mut mask_who = 0;
        for c in who.chars() {
            mask_who |= match c {
                'u' => 0o700,
                'g' => 0o070,
                'o' => 0o007,
                _ => 0o777,
            };
        }
        if who.is_empty() {
            mask_who = 0o777;
        }
        let op = clause.as_bytes()[who_end] as char;
        let mut bits = 0;
        for c in clause[who_end + 1..].chars() {
            bits |= match c {
                'r' => 0o444,
                'w' => 0o222,
                'x' => 0o111,
                _ => return usage("umask", &format!("{}: invalid symbolic mode", spec)),
            };
        }
        bits &= mask_who;
        match op {
            '+' => allowed |= bits,
            '-' => allowed &= !bits,
            _ => allowed = (allowed & !mask_who) | bits,
        }
    }
    unsafe {
        libc::umask((!allowed & 0o777) as libc::mode_t);
    }
    0
}

fn builtin_ulimit(words: &[String]) -> i32 {
    let mut hard = false;
    let mut resource = (libc::RLIMIT_FSIZE, 512u64, "file size");
    let mut value = None;
    for a in &words[1..] {
        match a.as_str() {
            "-H" => hard = true,
            "-S" => hard = false,
            "-f" => resource = (libc::RLIMIT_FSIZE, 512, "file size"),
            "-n" => resource = (libc::RLIMIT_NOFILE, 1, "open files"),
            "-c" => resource = (libc::RLIMIT_CORE, 512, "core file size"),
            "-s" => resource = (libc::RLIMIT_STACK, 1024, "stack size"),
            "-t" => resource = (libc::RLIMIT_CPU, 1, "cpu time"),
            "-d" => resource = (libc::RLIMIT_DATA, 1024, "data seg size"),
            "-v" => resource = (libc::RLIMIT_AS, 1024, "virtual memory"),
            v if !v.starts_with('-') => value = Some(v.to_string()),
            o => return usage("ulimit", &format!("{}: invalid option", o)),
        }
    }
    let (res, unit, what) = resource;
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(res as _, &mut lim) } != 0 {
        err_line(&format!("meowsh: ulimit: {}: cannot get limit", what));
        return 1;
    }
    match value {
        None => {
            let cur = if hard { lim.rlim_max } else { lim.rlim_cur };
            if cur == libc::RLIM_INFINITY {
                out("unlimited\n");
            } else {
                out(&format!("{}\n", cur as u64 / unit));
            }
            0
        }
        Some(v) => {
            let new = if v == "unlimited" {
                libc::RLIM_INFINITY
            } else {
                match v.parse::<u64>() {
                    Ok(n) => (n.saturating_mul(unit)) as libc::rlim_t,
                    Err(_) => return usage("ulimit", &format!("{}: invalid number", v)),
                }
            };
            if hard {
                lim.rlim_max = new;
            } else {
                lim.rlim_cur = new;
            }
            if unsafe { libc::setrlimit(res as _, &lim) } != 0 {
                let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                err_line(&format!("meowsh: ulimit: {}: {}", what, strerror(e)));
                return 1;
            }
            0
        }
    }
}

// ---- zsh compatibility ----------------------------------------------------

// Best-effort autoload: for each function name, sources a file of that name
// from $fpath; names that are not found become no-op functions so later
// calls do not fail.
fn builtin_autoload(words: &[String]) -> i32 {
    let fpath = with_shell(|s| s.arrays.get("fpath").cloned()).unwrap_or_default();
    for arg in &words[1..] {
        if arg.starts_with('-') || arg.starts_with('+') {
            continue;
        }
        let file = fpath
            .iter()
            .map(|d| format!("{}/{}", d, arg))
            .find(|p| std::path::Path::new(p).is_file());
        match file {
            Some(path) => {
                source_file(&path, None);
            }
            None => {
                with_shell(|s| {
                    s.functions.entry(arg.clone()).or_insert(FuncDef {
                        body: Arc::new(Node::Empty),
                    });
                });
            }
        }
    }
    0
}

fn builtin_setopt(words: &[String], value: bool) -> i32 {
    let name = &words[0];
    let args = &words[1..];
    if args.is_empty() && value {
        let mut keys: Vec<String> = with_shell(|s| {
            s.named_opts
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| k.clone())
                .collect()
        });
        keys.sort();
        out(&keys.iter().map(|k| format!("{}\n", k)).collect::<String>());
        return 0;
    }
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-o" {
            match args.get(i + 1) {
                Some(o) => toggle_named_opt(o, value),
                None => return usage(name, "-o requires an argument"),
            }
            i += 2;
            continue;
        }
        if arg.starts_with('-') || arg.starts_with('+') {
            err_line(&format!("meowsh: {}: unsupported option: {}", name, arg));
        } else {
            toggle_named_opt(arg, value);
        }
        i += 1;
    }
    0
}

fn toggle_named_opt(name: &str, value: bool) {
    let normalized: String = name
        .to_ascii_lowercase()
        .chars()
        .filter(|c| *c != '_')
        .collect();
    let flag = NAMED_OPTIONS
        .iter()
        .find(|(n, _)| *n == normalized)
        .map(|(_, f)| *f);
    with_shell(|s| {
        s.named_opts.insert(normalized.clone(), value);
        if let Some(f) = flag {
            if value {
                s.opts |= f;
            } else {
                s.opts &= !f;
            }
        }
    });
}

fn builtin_zstyle(args: &[String]) -> i32 {
    if args.is_empty() {
        return 0;
    }
    match args[0].as_str() {
        "-L" => {
            print_zstyles(&args[1..]);
            0
        }
        "-s" => {
            if args.len() < 4 {
                return 1;
            }
            match zstyle_lookup(&args[1], &args[2]) {
                Some(values) => {
                    var_set(&args[3], &values.join(" "), false);
                    0
                }
                None => 1,
            }
        }
        "-a" => {
            if args.len() < 4 {
                return 1;
            }
            match zstyle_lookup(&args[1], &args[2]) {
                Some(values) if array_set(&args[3], values.clone()) => 0,
                _ => 1,
            }
        }
        "-b" => {
            if args.len() < 4 {
                return 1;
            }
            let value = zstyle_lookup(&args[1], &args[2])
                .map(|values| zstyle_truthy(&values))
                .unwrap_or(false);
            var_set(&args[3], if value { "yes" } else { "no" }, false);
            if value {
                0
            } else {
                1
            }
        }
        "-t" => {
            if args.len() < 3 {
                return 1;
            }
            match zstyle_lookup(&args[1], &args[2]) {
                Some(values) if zstyle_truthy(&values) => 0,
                _ => 1,
            }
        }
        _ => {
            if args.len() < 2 {
                return 1;
            }
            let entry = ZStyle {
                pattern: args[0].clone(),
                matcher: glob::Pattern::new(&args[0]).ok(),
                style: args[1].clone(),
                values: args[2..].to_vec(),
            };
            with_shell(|s| {
                match s
                    .zstyles
                    .iter_mut()
                    .find(|z| z.pattern == entry.pattern && z.style == entry.style)
                {
                    Some(existing) => *existing = entry,
                    None => s.zstyles.push(entry),
                }
            });
            0
        }
    }
}

fn zstyle_lookup(context: &str, style: &str) -> Option<Vec<String>> {
    with_shell(|s| {
        s.zstyles
            .iter()
            .rev()
            .find(|e| {
                e.style == style
                    && (e.pattern == context
                        || e.matcher
                            .as_ref()
                            .map(|p| p.matches(context))
                            .unwrap_or(false))
            })
            .map(|e| e.values.clone())
    })
}

fn zstyle_truthy(values: &[String]) -> bool {
    let first = values.first().map(|s| s.to_ascii_lowercase());
    matches!(first.as_deref(), Some("yes" | "true" | "on" | "1"))
}

fn print_zstyles(filters: &[String]) {
    let text: String = with_shell(|s| {
        s.zstyles
            .iter()
            .filter(|e| {
                filters.is_empty() || filters.iter().any(|f| f == &e.pattern || f == &e.style)
            })
            .map(|e| {
                if e.values.is_empty() {
                    format!("zstyle {} {}\n", e.pattern, e.style)
                } else {
                    format!("zstyle {} {} {}\n", e.pattern, e.style, e.values.join(" "))
                }
            })
            .collect()
    });
    out(&text);
}

/// Calls a shell function by name if one is defined (used for hooks).
pub fn call_if_defined(name: &str) -> Option<i32> {
    let body = with_shell(|s| s.functions.get(name).map(|f| f.body.clone()))?;
    Some(call_function(&body, &[name.to_string()]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<(char, bool)> {
        s.chars().map(|c| (c, false)).collect()
    }

    #[test]
    fn read_split_assigns_rest_to_last() {
        assert_eq!(
            read_split(&chars("  a b  c d  "), " \t\n", 2),
            vec!["a", "b  c d"]
        );
        assert_eq!(read_split(&chars("a:b:c"), ":", 2), vec!["a", "b:c"]);
        assert_eq!(read_split(&chars("a"), " ", 3), vec!["a", "", ""]);
        assert_eq!(read_split(&chars("x y z"), " ", 0), vec!["x", "y", "z"]);
    }

    #[test]
    fn quoting_round_trips() {
        assert_eq!(shell_quote("abc"), "abc");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(normalize_path("/a/b/../c/./d"), "/a/c/d");
        assert_eq!(normalize_path("/.."), "/");
    }
}
