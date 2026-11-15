//! Builtin commands. Each takes the full argument vector (the name in
//! `words[0]`) and returns an exit status. Output goes straight to the
//! file descriptors so redirections apply.

use crate::exec::{call_function, last_status, run_string, set_flow, strerror};
use crate::jobs::{builtin_bg, builtin_fg, builtin_jobs, builtin_wait, kill_targets};
use crate::lexer::is_valid_name;
use crate::shell::{
    array_set, assoc_declare, builtin_err, find_command, shell_err, var_get, var_get_opt, var_set,
    with_shell,
};
use crate::sys::{err_line, out, read_byte};
use crate::trap::{set_trap, signal_name, signal_number, SIGNALS};
use crate::types::{Flow, FuncDef, Node, ZStyle, VAR_READONLY};
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
    "disable",
    "enable",
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
    "print",
    "zle",
    "zcompile",
    "zrecompile",
    "builtin",
    "unfunction",
    "whence",
    "which",
    "where",
    "integer",
    "float",
    "functions",
    "is-at-least",
    "zparseopts",
    "fc",
    "colors",
    "bashcompinit",
    "complete",
    "compctl",
];

const KEYWORDS: &[&str] = &[
    "!",
    "{",
    "}",
    "case",
    "do",
    "done",
    "elif",
    "else",
    "esac",
    "fi",
    "for",
    "function",
    "if",
    "in",
    "then",
    "until",
    "while",
    "[[",
    "]]",
    "repeat",
    "select",
    "coproc",
    "foreach",
    "end",
    "time",
    "nocorrect",
    "declare",
    "export",
    "float",
    "integer",
    "local",
    "readonly",
    "typeset",
];

pub fn builtin_names() -> impl Iterator<Item = &'static str> {
    SPECIAL.iter().chain(REGULAR.iter()).copied()
}

pub fn is_special_builtin(name: &str) -> bool {
    SPECIAL.contains(&name) && !builtin_disabled(name)
}

pub fn is_builtin(name: &str) -> bool {
    (SPECIAL.contains(&name) || REGULAR.contains(&name)) && !builtin_disabled(name)
}

// `disable`: (kind, name) pairs, kind `b` builtin, `r` reserved word,
// `a` alias, `f` function. Kept outside the shell state so the parser
// can ask without the shell lock.
static DISABLED: std::sync::RwLock<Vec<(char, String)>> = std::sync::RwLock::new(Vec::new());

pub fn is_disabled(kind: char, name: &str) -> bool {
    DISABLED
        .read()
        .map(|d| d.iter().any(|(k, n)| *k == kind && n == name))
        .unwrap_or(false)
}

// A disabled declaration builtin still runs through its reserved word.
fn builtin_disabled(name: &str) -> bool {
    !crate::parser::is_decl_word(name) && is_disabled('b', name)
}

/// `enable` / `disable [-afmr] [name ...]`.
fn builtin_enable(words: &[String]) -> i32 {
    let enable = words[0] == "enable";
    let mut kind = 'b';
    let mut pattern = false;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        if a == "--" {
            i += 1;
            break;
        }
        for c in flags.chars() {
            match c {
                'a' | 'f' | 'r' => kind = c,
                'm' => pattern = true,
                'p' | 's' => {}
                _ => return usage(&words[0], &format!("bad option: -{}", c)),
            }
        }
        i += 1;
    }
    let known: Vec<String> = match kind {
        'b' => SPECIAL
            .iter()
            .chain(REGULAR.iter())
            .map(|s| s.to_string())
            .collect(),
        'r' => KEYWORDS.iter().map(|s| s.to_string()).collect(),
        'a' => with_shell(|s| {
            s.assoc(crate::types::ALIASES)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default()
        }),
        _ => with_shell(|s| s.functions.keys().cloned().collect()),
    };
    let names = &words[i..];
    if names.is_empty() {
        let mut list: Vec<&String> = known
            .iter()
            .filter(|n| is_disabled(kind, n) != enable)
            .collect();
        list.sort();
        list.dedup();
        out(&list.iter().map(|n| format!("{}\n", n)).collect::<String>());
        return 0;
    }
    let mut status = 0;
    for name in names {
        let targets: Vec<String> = if pattern {
            known
                .iter()
                .filter(|n| crate::pattern::matches(name, n))
                .cloned()
                .collect()
        } else if known.contains(name) {
            vec![name.clone()]
        } else {
            builtin_err(&words[0], &format!("no such hash table element: {}", name));
            status = 1;
            continue;
        };
        if let Ok(mut d) = DISABLED.write() {
            for t in targets {
                d.retain(|(k, n)| !(*k == kind && *n == t));
                if !enable {
                    d.push((kind, t));
                }
            }
        }
    }
    status
}

pub fn run(name: &str, words: &[String]) -> i32 {
    match name {
        ":" | "true" => 0,
        "false" => 1,
        "break" => builtin_break(words, true),
        "continue" => builtin_break(words, false),
        "." | "source" => builtin_source(words),
        "eval" => builtin_eval(words),
        "exit" => builtin_exit(words),
        "export" | "readonly" => decl_from_words(words),
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
        "local" | "typeset" | "declare" | "integer" | "float" => decl_from_words(words),
        "functions" => builtin_functions(words),
        "pwd" => builtin_pwd(words),
        "read" => builtin_read(words),
        "setopt" => builtin_setopt(words, true),
        "unsetopt" => builtin_setopt(words, false),
        "test" | "[" => crate::testexpr::builtin_test(words),
        "type" | "whence" | "which" | "where" => builtin_whence(words),
        "enable" | "disable" => builtin_enable(words),
        "ulimit" => builtin_ulimit(words),
        "umask" => builtin_umask(words),
        "zstyle" => builtin_zstyle(&words[1..]),
        "add-zsh-hook" => builtin_add_zsh_hook(words),
        "print" => builtin_print(words),
        // Accepted for zsh startup-file compatibility; no effect. There is
        // no zle, so widgets defined with `zle -N` never run.
        "emulate" => builtin_emulate(words),
        "unfunction" => {
            builtin_unset(&[&["unset".to_string(), "-f".to_string()], &words[1..]].concat())
        }
        "is-at-least" => builtin_is_at_least(words),
        "zparseopts" => builtin_zparseopts(words),
        "fc" => builtin_fc(words),
        "colors" => builtin_colors(),
        "bindkey" | "compinit" | "compdef" | "compaudit" | "zmodload" | "zle" | "zcompile"
        | "zrecompile" | "bashcompinit" | "complete" | "compctl" => 0,
        _ => {
            builtin_err(name, "not a builtin");
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
    builtin_err(name, msg);
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
                builtin_err(name, &format!("{}: loop count out of range", a));
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

// zsh: an error inside eval ends the eval with status 1 instead of
// leaving the shell.
fn builtin_eval(words: &[String]) -> i32 {
    let st = crate::exec::run_string_named(&words[1..].join(" "), Some("(eval)"));
    with_shell(|s| {
        if s.error_exit {
            s.error_exit = false;
            if matches!(s.flow, Flow::Exit(_)) {
                s.flow = Flow::None;
                return 1;
            }
        }
        st
    })
}

fn builtin_exit(words: &[String]) -> i32 {
    let code = match words.get(1) {
        None => last_status(),
        Some(a) => match a.trim().parse::<i64>() {
            Ok(n) => (n & 0xff) as i32,
            Err(_) => {
                builtin_err("exit", &format!("{}: numeric argument required", a));
                2
            }
        },
    };
    with_shell(|s| s.error_exit = false);
    set_flow(Flow::Exit(code));
    code
}

fn builtin_return(words: &[String]) -> i32 {
    let (funcs, sources) = with_shell(|s| (s.func_depth, s.source_depth));
    if funcs == 0 && sources == 0 {
        builtin_err(
            "return",
            "can only `return' from a function or sourced script",
        );
        return 1;
    }
    let code = match words.get(1) {
        None => last_status(),
        Some(a) => match a.trim().parse::<i64>() {
            Ok(n) => (n & 0xff) as i32,
            Err(_) => {
                builtin_err("return", &format!("{}: numeric argument required", a));
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
    // zsh: $0 is the sourced file while it runs.
    let (saved_argv0, saved_source) = with_shell(|s| {
        s.source_depth += 1;
        (
            std::mem::replace(&mut s.argv0, path.to_string()),
            s.source_file.replace(path.to_string()),
        )
    });
    let mut st = crate::exec::run_string_named(&content, Some(path));
    with_shell(|s| {
        s.source_depth -= 1;
        s.argv0 = saved_argv0;
        s.source_file = saved_source;
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
        builtin_err(&words[0], &format!("{}: not found", file));
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
        s.params
            .iter()
            .filter(|(_, p)| (flag == 0 || p.attrs & flag != 0) && !p.is_array() && !p.is_assoc())
            .map(|(k, p)| (k.clone(), p.text()))
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

fn builtin_unset(words: &[String]) -> i32 {
    let mut functions = false;
    let mut no_deref = false;
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
            // -n: unset a nameref itself, not what it refers to.
            "-n" => {
                no_deref = true;
                continue;
            }
            _ => {}
        }
        if functions {
            with_shell(|s| s.functions.remove(a.as_str()));
            crate::trap::function_removed(a);
            continue;
        }
        let target = if no_deref {
            a.clone()
        } else {
            with_shell(|s| s.resolve_name(a))
        };
        let a = &target;
        let readonly = with_shell(|s| {
            s.param(a.as_str())
                .is_some_and(|p| p.attrs & VAR_READONLY != 0)
        });
        if readonly {
            builtin_err("unset", &format!("read-only variable: {}", a));
            status = 1;
            continue;
        }
        with_shell(|s| s.remove_param(a.as_str()));
    }
    status
}

// `set -o` / `set +o` listings.
fn list_options(as_commands: bool) {
    let mut text = String::new();
    for name in crate::options::names() {
        let on = crate::options::is_set(name);
        if as_commands {
            text.push_str(&format!("set {}o {}\n", if on { '-' } else { '+' }, name));
        } else {
            text.push_str(&format!("{:<22}{}\n", name, if on { "on" } else { "off" }));
        }
    }
    out(&text);
}

fn builtin_set(words: &[String]) -> i32 {
    let args = &words[1..];
    if args.is_empty() {
        list_vars("", 0);
        return 0;
    }
    let mut i = 0;
    let mut array: Option<(String, bool)> = None;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            i += 1;
            break;
        }
        if a == "-" {
            crate::options::set("xtrace", false);
            crate::options::set("verbose", false);
            i += 1;
            break;
        }
        if !((a.starts_with('-') || a.starts_with('+')) && a.len() > 1) {
            break;
        }
        let on = a.starts_with('-');
        for c in a[1..].chars() {
            match c {
                'o' => match args.get(i + 1) {
                    Some(name) => {
                        i += 1;
                        if !crate::options::set(name, on) {
                            return usage("set", &format!("no such option: {}", name));
                        }
                    }
                    None => list_options(!on),
                },
                // zsh: set -A name values (replace) / +A (overwrite start).
                'A' => {
                    i += 1;
                    match args.get(i) {
                        Some(name) => array = Some((name.clone(), on)),
                        None => {
                            list_vars("", 0);
                            return 0;
                        }
                    }
                }
                's' if crate::options::letter('s').is_none() => {}
                c => match crate::options::letter(c) {
                    Some((name, v)) => {
                        crate::options::set(name, if on { v } else { !v });
                    }
                    None => {
                        return usage(
                            "set",
                            &format!("{}{}: invalid option", if on { '-' } else { '+' }, c),
                        )
                    }
                },
            }
        }
        i += 1;
    }
    let rest = args[i.min(args.len())..].to_vec();
    match array {
        Some((name, replace)) => {
            let items = if replace {
                rest
            } else {
                let mut old = with_shell(|s| s.array(&name).cloned()).unwrap_or_default();
                for (k, v) in rest.into_iter().enumerate() {
                    if k < old.len() {
                        old[k] = v;
                    } else {
                        old.push(v);
                    }
                }
                old
            };
            if !array_set(&name, items) {
                return 1;
            }
        }
        None if i < args.len() || args.last().map(|a| a == "--").unwrap_or(false) => {
            with_shell(|s| s.pos_params = rest);
        }
        None => {}
    }
    0
}

fn builtin_shift(words: &[String]) -> i32 {
    let n = match words.get(1) {
        None => 1,
        Some(a) => match a.parse::<usize>() {
            Ok(n) => n,
            Err(_) => {
                builtin_err("shift", &format!("{}: numeric argument required", a));
                return 1;
            }
        },
    };
    with_shell(|s| {
        if n > s.pos_params.len() {
            builtin_err("shift", "can't shift that many");
            return 1;
        }
        s.pos_params.drain(..n);
        0
    })
}

// Declaration commands called with plain words (through `builtin`,
// `command` or `eval`): assignments become typed arguments.
fn decl_from_words(words: &[String]) -> i32 {
    let mut args = vec![crate::typeset::DeclArg::Word(words[0].clone())];
    let mut options_done = false;
    for w in &words[1..] {
        if !options_done && (w.starts_with('-') || w.starts_with('+')) {
            if w == "--" {
                options_done = true;
            }
            args.push(crate::typeset::DeclArg::Word(w.clone()));
            continue;
        }
        options_done = true;
        let (name, value, append) = split_assignment(w);
        match value {
            Some(v) => args.push(crate::typeset::DeclArg::Assign {
                name: name.to_string(),
                append,
                value: Some(v.to_string()),
            }),
            None => args.push(crate::typeset::DeclArg::Word(w.clone())),
        }
    }
    crate::typeset::typeset(&words[0], &args)
}

/// `functions` and `typeset -f`: print function definitions (all, or the
/// named ones; names only with `+`; patterns with -m; -x N indents with
/// N spaces).
pub fn functions_cmd(
    _cmd: &str,
    minus: &[char],
    plus: &[char],
    rest: &[crate::typeset::DeclArg],
) -> i32 {
    let names: Vec<String> = rest
        .iter()
        .map(|a| match a {
            crate::typeset::DeclArg::Word(w) => w.clone(),
            crate::typeset::DeclArg::Assign { name, .. }
            | crate::typeset::DeclArg::Array { name, .. } => name.clone(),
        })
        .collect();
    functions_listing(&names, minus.contains(&'m'), !plus.is_empty(), None)
}

fn functions_listing(
    names: &[String],
    patterns: bool,
    names_only: bool,
    spaces: Option<usize>,
) -> i32 {
    let mut all: Vec<(String, Arc<Node>)> = with_shell(|s| {
        s.functions
            .iter()
            .map(|(k, f)| (k.clone(), f.body.clone()))
            .collect()
    });
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let mut status = 0;
    let mut text = String::new();
    let mut emit = |name: &str, body: &Node| {
        if names_only {
            text.push_str(name);
            text.push('\n');
        } else {
            text.push_str(&crate::deparse::function_text(name, body, spaces));
        }
    };
    if names.is_empty() {
        for (n, b) in &all {
            emit(n, b);
        }
    } else {
        for want in names {
            let mut found = false;
            for (n, b) in &all {
                if (patterns && crate::pattern::matches(want, n)) || (!patterns && n == want) {
                    emit(n, b);
                    found = true;
                }
            }
            if !found && !patterns {
                status = 1;
            }
        }
    }
    out(&text);
    status
}

// `functions -M [-s] name [min [max [shellfn]]]` defines a math
// function, `functions +M name` removes one, `functions -M` lists them.
fn math_functions(words: &[String]) -> i32 {
    use crate::arith::{MathFunc, MATH_FUNCS};
    let remove = words[1].starts_with('+');
    let string = words[1].contains('s');
    let args = &words[2..];
    let Ok(mut table) = MATH_FUNCS.write() else {
        return 1;
    };
    if remove {
        table.retain(|m| !args.contains(&m.name));
        return 0;
    }
    let Some(name) = args.first() else {
        let mut text = String::new();
        for m in table.iter() {
            text.push_str(&format!(
                "functions -M{} {} {} {}{}\n",
                if m.string { "s" } else { "" },
                m.name,
                m.min,
                m.max,
                if m.shellfn != m.name {
                    format!(" {}", m.shellfn)
                } else {
                    String::new()
                }
            ));
        }
        out(&text);
        return 0;
    };
    let num = |k: usize, d: i64| args.get(k).and_then(|a| a.parse::<i64>().ok()).unwrap_or(d);
    let min = num(1, 0);
    let max = if args.len() > 2 {
        num(2, -1)
    } else if args.len() > 1 {
        min
    } else {
        -1
    };
    table.retain(|m| m.name != *name);
    table.push(MathFunc {
        name: name.clone(),
        min,
        max,
        shellfn: args.get(3).cloned().unwrap_or_else(|| name.clone()),
        string,
    });
    0
}

// `functions -c old new`: copies a function, loading it first if it is
// autoloaded.
fn copy_function(words: &[String]) -> i32 {
    let (Some(old), Some(new)) = (words.get(2), words.get(3)) else {
        return usage("functions", "-c: requires two arguments");
    };
    if is_autoload_stub(old) && !autoload_now(old, None, true) {
        shell_err(&format!("{}: function definition file not found", old));
        return 1;
    }
    let Some(f) = with_shell(|s| s.functions.get(old).cloned()) else {
        builtin_err("functions", &format!("no such function: {}", old));
        return 1;
    };
    with_shell(|s| s.functions.insert(new.clone(), f));
    0
}

fn builtin_functions(words: &[String]) -> i32 {
    if words.get(1).is_some_and(|a| a == "-c") {
        return copy_function(words);
    }
    if words
        .get(1)
        .is_some_and(|a| a.len() > 1 && (a.starts_with("-M") || a.starts_with("+M")))
    {
        return math_functions(words);
    }
    let (mut patterns, mut names_only, mut spaces) = (false, false, None);
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "+" {
            names_only = true;
        } else if a == "--" {
            i += 1;
            break;
        } else if let Some(f) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            let mut chars = f.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    'm' => patterns = true,
                    'x' => {
                        let rest: String = chars.by_ref().collect();
                        let n = if rest.is_empty() {
                            i += 1;
                            words.get(i).cloned().unwrap_or_default()
                        } else {
                            rest
                        };
                        match n.parse() {
                            Ok(v) => spaces = Some(v),
                            Err(_) => {
                                return usage("functions", &format!("invalid indentation: {}", n))
                            }
                        }
                    }
                    _ => {}
                }
            }
        } else if a.starts_with('+') && a.len() > 1 {
            names_only = true;
        } else {
            break;
        }
        i += 1;
    }
    functions_listing(&words[i.min(words.len())..], patterns, names_only, spaces)
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
    let mut fd = 0;
    let mut keys: Option<usize> = None;
    let mut yes_no = false;
    let mut raw = false;
    let mut prompt = None;
    let mut delim = b'\n';
    let mut array: Option<String> = None;
    // -e echoes the input instead of assigning it, -E does both.
    let (mut echo, mut echo_only) = (false, false);
    let mut timeout: Option<f64> = None;
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
                'd' => delim = take_arg().and_then(|d| d.bytes().next()).unwrap_or(0),
                'a' | 'A' => array = take_arg(),
                // zsh: -k [n] reads n characters, -q one y/n answer.
                'k' => {
                    let rest: String = chars.by_ref().collect();
                    keys = Some(if !rest.is_empty() {
                        rest.parse().unwrap_or(1)
                    } else if let Some(n) = words.get(i + 1).and_then(|w| w.parse::<usize>().ok()) {
                        i += 1;
                        n
                    } else {
                        1
                    });
                }
                'q' => {
                    keys = Some(1);
                    yes_no = true;
                }
                'u' => match take_arg().and_then(|n| n.parse().ok()) {
                    Some(n) => fd = n,
                    None => return usage("read", "-u: bad file descriptor"),
                },
                'p' => match with_shell(|s| s.coproc) {
                    Some((_, r)) => fd = r,
                    None => return usage("read", "-p: no coprocess"),
                },
                'E' => echo = true,
                'e' => {
                    echo = true;
                    echo_only = true;
                }
                // -t [secs]: fail if no input arrives in time.
                't' => {
                    let rest: String = chars.by_ref().collect();
                    timeout = Some(if !rest.is_empty() {
                        rest.parse().unwrap_or(0.0)
                    } else if let Some(n) = words.get(i + 1).and_then(|w| w.parse::<f64>().ok()) {
                        i += 1;
                        n
                    } else {
                        0.0
                    });
                }
                _ => return usage("read", &format!("-{}: invalid option", c)),
            }
        }
        i += 1;
    }
    let mut names: Vec<String> = words[i..].to_vec();
    // zsh: `read 'name?prompt'` prompts on a terminal.
    if let Some(first) = names.first_mut() {
        if let Some((n, p)) = first.split_once('?') {
            prompt = Some(p.to_string());
            *first = n.to_string();
        }
    }
    for n in &names {
        if !is_valid_name(n) {
            builtin_err("read", &format!("`{}': not a valid identifier", n));
            return 2;
        }
    }
    if let Some(p) = prompt {
        if unsafe { libc::isatty(0) } == 1 {
            let _ = crate::sys::write_fd(2, p.as_bytes());
        }
    }

    if let Some(n) = keys {
        let text = match read_chars(fd, n) {
            Some(t) => t,
            None => return 1,
        };
        let target = names
            .first()
            .cloned()
            .unwrap_or_else(|| "REPLY".to_string());
        if yes_no {
            let yes = matches!(text.as_str(), "y" | "Y");
            var_set(&target, if yes { "y" } else { "n" }, false);
            return if yes { 0 } else { 1 };
        }
        return if var_set(&target, &text, false) { 0 } else { 2 };
    }

    if let Some(t) = timeout {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = (t * 1000.0).round().clamp(0.0, i32::MAX as f64) as i32;
        if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
            return 1;
        }
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut escaped: Vec<bool> = Vec::new();
    let mut got_delim = false;
    let mut pending_backslash = false;
    loop {
        match read_byte(fd) {
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
    if echo {
        let line: String = chars.iter().map(|(c, _)| *c).collect();
        out(&format!("{}\n", line));
        if echo_only {
            return if got_delim || !bytes.is_empty() { 0 } else { 1 };
        }
    }

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

// Reads `n` characters from standard input, from a terminal without
// waiting for Enter. None at end of input.
fn read_chars(fd: i32, n: usize) -> Option<String> {
    let tty = unsafe { libc::isatty(fd) } == 1;
    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if tty && unsafe { libc::tcgetattr(fd, &mut saved) } == 0 {
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) };
    }
    let mut bytes = Vec::new();
    let mut count = 0;
    while count < n {
        match read_byte(fd) {
            Ok(Some(b)) => {
                bytes.push(b);
                // Count a character once its UTF-8 sequence is complete.
                if std::str::from_utf8(&bytes).is_ok() {
                    count = String::from_utf8_lossy(&bytes).chars().count();
                }
            }
            _ => break,
        }
    }
    if tty {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
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
                with_shell(|s| s.remove_param("OPTARG"));
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
                        with_shell(|s| s.remove_param("OPTARG"));
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
            with_shell(|s| s.remove_param("OPTARG"));
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
                builtin_err("cd", "HOME not set");
                return 1;
            }
        },
        Some("-") => match var_get_opt("OLDPWD").filter(|h| !h.is_empty()) {
            Some(d) => {
                print = true;
                d
            }
            None => {
                builtin_err("cd", "OLDPWD not set");
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
            builtin_err("cd", &crate::exec::file_error(&target, e));
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
            builtin_err("cd", &crate::exec::file_error(&target, e));
            return 1;
        }
        logical
    };
    var_set("OLDPWD", &old, false);
    var_set("PWD", &new, false);
    if print {
        out(&format!("{}\n", new));
    }
    run_hooks("chpwd", &[]);
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
                builtin_err("pwd", "cannot determine current directory");
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
                builtin_err("pushd", "directory stack empty");
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
        builtin_err("popd", "directory stack empty");
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
    let mut args = &words[1..];
    let mut global = false;
    // -g global (zsh), -s suffix (accepted, treated as global), -L/-p list.
    while let Some(a) = args.first() {
        match a.as_str() {
            "-g" | "-s" => global = true,
            "-p" | "-L" | "-r" | "+g" => {}
            "--" => {
                args = &args[1..];
                break;
            }
            _ => break,
        }
        args = &args[1..];
    }
    let table = |s: &mut crate::types::Shell| s.alias_map(global).clone();
    if args.is_empty() {
        let mut all: Vec<(String, String)> = with_shell(|s| table(s).into_iter().collect());
        all.sort();
        let flag = if global { "-g " } else { "" };
        let text: String = all
            .iter()
            .map(|(k, v)| format!("alias {}{}={}\n", flag, k, shell_quote(v)))
            .collect();
        out(&text);
        return 0;
    }
    let mut status = 0;
    for a in args {
        match a.find('=') {
            Some(i) => {
                let (name, value) = (a[..i].to_string(), a[i + 1..].to_string());
                with_shell(|s| s.alias_map(global).insert(name, value));
            }
            None => match with_shell(|s| table(s).get(a.as_str()).cloned()) {
                Some(v) => {
                    out(&format!("alias {}={}\n", a, shell_quote(&v)));
                }
                None => {
                    builtin_err("alias", &format!("{}: not found", a));
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
            with_shell(|s| {
                s.alias_map(false).clear();
                s.alias_map(true).clear();
            });
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        let removed = with_shell(|s| {
            s.alias_map(false).remove(a.as_str()).is_some()
                | s.alias_map(true).remove(a.as_str()).is_some()
        });
        if !removed {
            builtin_err("unalias", &format!("{}: not found", a));
            status = 1;
        }
    }
    status
}

// ---- command lookup -------------------------------------------------------

fn describe_command(name: &str, verbose: bool) -> Option<String> {
    if let Some(v) = with_shell(|s| s.alias(name, false).cloned()) {
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

#[derive(Default)]
struct Whence {
    verbose: bool,   // -v
    csh: bool,       // -c
    word: bool,      // -w
    funcs: bool,     // -f
    all: bool,       // -a
    path_only: bool, // -p
    pattern: bool,   // -m
    symlinks: u8,    // -s 1, -S 2
    indent: Option<usize>,
}

// Resolves symlinks for -s (final target) or -S (each step).
fn whence_links(path: &str, steps: bool) -> String {
    let mut out = path.to_string();
    let mut cur = std::path::PathBuf::from(path);
    let mut seen = std::collections::HashSet::new();
    while let Ok(target) = std::fs::read_link(&cur) {
        if !seen.insert(cur.clone()) {
            return path.to_string();
        }
        let next = cur.parent().map(|p| p.join(&target)).unwrap_or(target);
        let next = std::fs::canonicalize(next.parent().unwrap_or(&next))
            .map(|p| p.join(next.file_name().unwrap_or_default()))
            .unwrap_or(next);
        if steps {
            out.push_str(&format!(" -> {}", next.display()));
        }
        cur = next;
    }
    if !steps {
        if let Ok(real) = std::fs::canonicalize(path) {
            if real.to_string_lossy() != path {
                out.push_str(&format!(" -> {}", real.display()));
            }
        }
    }
    out
}

fn path_matches(name: &str, all: bool) -> Vec<String> {
    if name.contains('/') {
        let p = std::path::Path::new(name);
        return if p.is_file() {
            vec![name.to_string()]
        } else {
            Vec::new()
        };
    }
    let mut found = Vec::new();
    for dir in var_get("PATH").split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(name);
        if p.is_file() && crate::shell::find_command(&p.to_string_lossy()).is_some() {
            found.push(p.to_string_lossy().into_owned());
            if !all {
                break;
            }
        }
    }
    found
}

// One name for whence and its relatives. Returns false if not found.
fn whence_one(name: &str, o: &Whence, out_text: &mut String) -> bool {
    let mut found = false;
    let mut line = |s: String| {
        out_text.push_str(&s);
        out_text.push('\n');
    };
    if !o.path_only {
        if let Some(v) = with_shell(|s| s.alias(name, false).cloned()) {
            line(if o.word {
                format!("{}: alias", name)
            } else if o.csh {
                format!("{}: aliased to {}", name, v)
            } else if o.verbose {
                format!("{} is an alias for {}", name, v)
            } else {
                v
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if let Some(v) = with_shell(|s| s.alias(name, true).cloned()) {
            line(if o.word {
                format!("{}: global alias", name)
            } else if o.csh {
                format!("{}: globally aliased to {}", name, v)
            } else if o.verbose {
                format!("{} is a global alias for {}", name, v)
            } else {
                v
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if KEYWORDS.contains(&name) {
            line(if o.word {
                format!("{}: reserved", name)
            } else if o.csh {
                format!("{}: shell reserved word", name)
            } else if o.verbose {
                format!("{} is a reserved word", name)
            } else {
                name.to_string()
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if let Some(f) = with_shell(|s| s.functions.get(name).cloned()) {
            let def = || crate::deparse::function_text(name, &f.body, o.indent);
            line(if o.word {
                format!("{}: function", name)
            } else if o.csh || o.funcs {
                def().trim_end().to_string()
            } else if o.verbose {
                format!("{} is a shell function from {}", name, f.origin)
            } else {
                name.to_string()
            });
            found = true;
            if !o.all {
                return true;
            }
        }
        if is_builtin(name) {
            line(if o.word {
                format!("{}: builtin", name)
            } else if o.csh {
                format!("{}: shell built-in command", name)
            } else if o.verbose {
                format!("{} is a shell builtin", name)
            } else {
                name.to_string()
            });
            found = true;
            if !o.all {
                return true;
            }
        }
    }
    for p in path_matches(name, o.all) {
        let shown = match o.symlinks {
            1 => whence_links(&p, false),
            2 => whence_links(&p, true),
            _ => p.clone(),
        };
        line(if o.word {
            format!("{}: command", name)
        } else if o.verbose && !o.csh {
            format!("{} is {}", name, shown)
        } else {
            shown
        });
        found = true;
    }
    if !found {
        if o.word {
            line(format!("{}: none", name));
        } else if o.verbose || o.csh {
            line(format!("{} not found", name));
        }
    }
    found
}

/// whence, and which (-c), where (-ca), type (-v).
fn builtin_whence(words: &[String]) -> i32 {
    let cmd = words[0].as_str();
    let mut o = Whence::default();
    match cmd {
        "which" => o.csh = true,
        "where" => {
            o.csh = true;
            o.all = true;
        }
        "type" => o.verbose = true,
        _ => {}
    }
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            i += 1;
            break;
        }
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        let mut chars = flags.chars();
        while let Some(c) = chars.next() {
            match c {
                'v' => o.verbose = true,
                'c' => o.csh = true,
                'w' => o.word = true,
                'f' => o.funcs = true,
                'a' => o.all = true,
                'p' => o.path_only = true,
                'm' => o.pattern = true,
                's' => o.symlinks = 1,
                'S' => o.symlinks = 2,
                'x' => {
                    let rest: String = chars.by_ref().collect();
                    let n = if rest.is_empty() {
                        i += 1;
                        words.get(i).cloned().unwrap_or_default()
                    } else {
                        rest
                    };
                    match n.parse() {
                        Ok(v) => o.indent = Some(v),
                        Err(_) => return usage(cmd, &format!("bad argument to -x: {}", n)),
                    }
                }
                _ => return usage(cmd, &format!("bad option: -{}", c)),
            }
        }
        i += 1;
    }
    let mut status = 0;
    let mut text = String::new();
    for name in &words[i.min(words.len())..] {
        if o.pattern {
            let mut names: Vec<String> = crate::shell::all_commands()
                .into_iter()
                .chain(with_shell(|s| {
                    s.functions.keys().cloned().collect::<Vec<_>>()
                }))
                .chain(builtin_names().map(|b| b.to_string()))
                .filter(|n| crate::pattern::matches(name, n))
                .collect();
            names.sort();
            names.dedup();
            if names.is_empty() {
                status = 1;
            }
            for n in names {
                whence_one(&n, &o, &mut text);
            }
            continue;
        }
        if !whence_one(name, &o, &mut text) {
            status = 1;
        }
    }
    out(&text);
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
    // command -V is whence -v.
    if verbose {
        let mut args = vec!["whence".to_string(), "-v".to_string()];
        args.extend(words[i..].iter().cloned());
        return builtin_whence(&args);
    }
    let mut status = 0;
    for name in &words[i..] {
        match describe_command(name, verbose) {
            Some(d) => {
                out(&format!("{}\n", d));
            }
            None => {
                if verbose {
                    builtin_err("command", &format!("{}: not found", name));
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
            builtin_err("hash", &format!("{}: not found", name));
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
                        builtin_err("kill", &format!("({}) - {}", pid, strerror(e)));
                        status = 1;
                    }
                }
            }
            Err(e) => {
                builtin_err("kill", &e.to_string());
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
        let mut text = String::new();
        for (sig, a) in &traps {
            // A TRAPxxx function is shown as its definition.
            let fname = format!("TRAP{}", signal_name(*sig));
            let func = (a.split(' ').next() == Some(fname.as_str()))
                .then(|| with_shell(|s| s.functions.get(&fname).map(|f| f.body.clone())))
                .flatten();
            match func {
                Some(body) => text.push_str(&crate::deparse::function_text(&fname, &body, None)),
                None => text.push_str(&format!(
                    "trap -- {} {}\n",
                    shell_quote(a),
                    signal_name(*sig)
                )),
            }
        }
        out(&text);
        return 0;
    }
    // `trap -` alone removes every trap.
    if args.len() == 1 && args[0] == "-" {
        let sigs: Vec<i32> = with_shell(|s| s.traps.keys().copied().collect());
        for sig in sigs {
            set_trap(sig, None);
        }
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
                builtin_err("trap", &format!("{}: cannot trap", c));
                status = 1;
            }
            Some(sig) => set_trap(sig, action.clone()),
            None => {
                builtin_err("trap", &format!("undefined signal: {}", c));
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
        builtin_err("ulimit", &format!("{}: cannot get limit", what));
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
                builtin_err("ulimit", &format!("{}: {}", what, strerror(e)));
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
// autoload [-UXzk] [+X] name...: marks functions to be loaded from the
// first file of that name in $fpath when first called. As in zsh, the
// file's contents are the function body. `+X` loads immediately.
// Names that are meowsh builtins are left alone so the builtin runs.
// The stub an autoloaded function has until it is loaded: it loads
// the definition (from `dir` when given, `-d` also from $fpath) and runs
// it, as zsh's `autoload -X` does.
fn autoload_stub(dir: Option<&str>, default_too: bool) -> String {
    let mut stub = String::from("builtin autoload -X");
    if default_too {
        stub.push_str(" -d");
    }
    if let Some(d) = dir {
        stub.push(' ');
        stub.push_str(&shell_quote(d));
    }
    stub
}

fn is_autoload_stub(name: &str) -> bool {
    with_shell(|s| {
        s.functions.get(name).is_some_and(|f| {
            crate::deparse::Deparser::new(None)
                .body_text(&f.body)
                .trim_start_matches(['{', ' ', '\n', '\t'])
                .starts_with("builtin autoload -X")
        })
    })
}

fn define_stub(name: &str, dir: Option<&str>, default_too: bool) {
    let stub = autoload_stub(dir, default_too);
    let mut p = crate::parser::Parser::without_aliases(&stub, true);
    if let Ok(Some(body)) = p.parse_complete_command() {
        with_shell(|s| {
            s.functions.insert(
                name.to_string(),
                FuncDef {
                    body: Arc::new(body),
                    origin: s.argv0.clone(),
                    line: 0,
                },
            )
        });
    }
}

// The directory of $fpath holding `name`, made absolute.
fn fpath_dir(name: &str) -> Option<String> {
    let fpath = with_shell(|s| s.array("fpath").cloned()).unwrap_or_default();
    fpath.iter().find_map(|d| {
        let p = std::path::Path::new(d).join(name);
        p.is_file().then(|| {
            let dir = std::path::Path::new(d);
            let abs = if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(dir)
            };
            abs.to_string_lossy().into_owned()
        })
    })
}

fn builtin_autoload(words: &[String]) -> i32 {
    let (mut load_now, mut exec_now, mut default_too) = (false, false, false);
    let (mut resolve, mut strict) = (false, false);
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "+X" {
            load_now = true;
        } else if let Some(f) = a.strip_prefix('-').filter(|f| !f.is_empty()) {
            for c in f.chars() {
                match c {
                    'X' => exec_now = true,
                    'd' => default_too = true,
                    'r' => resolve = true,
                    'R' => {
                        resolve = true;
                        strict = true;
                    }
                    _ => {}
                }
            }
        } else if !a.starts_with('+') {
            break;
        }
        i += 1;
    }
    let args = &words[i..];
    if exec_now {
        return autoload_exec(args.first().map(|s| s.as_str()), default_too);
    }
    let mut status = 0;
    for arg in args {
        // `autoload /dir/name`: name loads from dir.
        let (name, dir) = match arg.rsplit_once('/') {
            Some((d, n)) if arg.starts_with('/') => (n.to_string(), Some(d.to_string())),
            _ => (arg.clone(), None),
        };
        if is_builtin(&name) {
            continue;
        }
        if load_now {
            if !autoload_now(&name, dir.as_deref(), default_too || dir.is_none()) {
                shell_err(&format!("{}: function definition file not found", name));
                status = 1;
            }
            continue;
        }
        let defined = with_shell(|s| s.functions.contains_key(&name));
        if defined && !is_autoload_stub(&name) {
            continue;
        }
        if dir.is_some() {
            define_stub(&name, dir.as_deref(), default_too);
            continue;
        }
        if defined {
            // A bare autoload keeps a stub with a path.
            continue;
        }
        if resolve {
            match fpath_dir(&name) {
                Some(d) => define_stub(&name, Some(&d), false),
                None if strict => {
                    shell_err(&format!("{}: function definition file not found", name));
                    status = 1;
                }
                None => define_stub(&name, None, false),
            }
            continue;
        }
        define_stub(&name, None, false);
    }
    status
}

// `autoload -X [dir]` in a function: loads the running function's
// definition, replacing it, and runs it with the same arguments.
fn autoload_exec(dir: Option<&str>, default_too: bool) -> i32 {
    let (name, args) = with_shell(|s| (s.argv0.clone(), s.pos_params.clone()));
    if !autoload_now(&name, dir, default_too || dir.is_none()) {
        // Reported where the function was called.
        let top = crate::shell::with_err_ctx(|e| e.stack.pop());
        shell_err(&format!("{}: function definition file not found", name));
        if let Some(t) = top {
            crate::shell::with_err_ctx(|e| e.stack.push(t));
        }
        return 1;
    }
    let Some((body, line)) =
        with_shell(|s| s.functions.get(&name).map(|f| (f.body.clone(), f.line)))
    else {
        return 1;
    };
    // Functions it defines come from its file.
    let origin = with_shell(|s| s.functions.get(&name).map(|f| f.origin.clone()));
    let saved = with_shell(|s| std::mem::replace(&mut s.source_file, origin));
    let mut words = vec![name];
    words.extend(args);
    let st = call_function(&body, &words, line);
    with_shell(|s| s.source_file = saved);
    st
}

// Loads `name`'s definition from `dir` (or $fpath when `fpath_too`).
// A file holding just a definition of `name` gives that definition's
// body; otherwise the whole file is the body.
fn autoload_now(name: &str, dir: Option<&str>, fpath_too: bool) -> bool {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(d) = dir {
        candidates.push(std::path::Path::new(d).join(name));
    }
    if fpath_too {
        let fpath = with_shell(|s| s.array("fpath").cloned()).unwrap_or_default();
        candidates.extend(fpath.iter().map(|d| std::path::Path::new(d).join(name)));
    }
    let Some((path, content)) = candidates.into_iter().find_map(|p| {
        std::fs::read_to_string(&p)
            .ok()
            .map(|c| (p.to_string_lossy().into_owned(), c))
    }) else {
        return false;
    };
    let mut parser = crate::parser::Parser::new(&content, true);
    let nodes = match parser.parse_program() {
        Ok(nodes) => nodes,
        Err(e) => {
            let msg = match e {
                crate::parser::ParseError::Syntax(m) => m,
                crate::parser::ParseError::Incomplete => "unexpected end of file".to_string(),
            };
            builtin_err(name, &msg);
            return false;
        }
    };
    let body = match nodes.as_slice() {
        [Node::FuncDef { name: n, body, .. }] if n == name => (**body).clone(),
        _ => Node::List(
            nodes
                .into_iter()
                .map(|node| crate::types::ListItem {
                    node,
                    background: false,
                })
                .collect(),
        ),
    };
    let origin = if path.starts_with('/') {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path.trim_start_matches("./"))
            .to_string_lossy()
            .into_owned()
    };
    with_shell(|s| {
        s.functions.insert(
            name.to_string(),
            FuncDef {
                body: Arc::new(body),
                origin,
                line: 0,
            },
        )
    });
    true
}

fn builtin_setopt(words: &[String], value: bool) -> i32 {
    let name = &words[0];
    let args = &words[1..];
    if args.is_empty() {
        // Options that differ from the defaults, as zsh lists them: the
        // name for setopt; with a `no` prefix for unsetopt.
        let emu = with_shell(|s| s.emulation);
        let mut text = String::new();
        // setopt lists options changed from their defaults, unsetopt the
        // rest; either way an option that is off shows as `noname` when it
        // is on by default.
        for o in crate::options::names() {
            let on = crate::options::is_set(o);
            let default = crate::options::default_in(o, emu);
            if (on != default) != value {
                continue;
            }
            if on != default {
                if on {
                    text.push_str(&format!("{}\n", o));
                } else {
                    text.push_str(&format!("no{}\n", o));
                }
            } else if on {
                text.push_str(&format!("no{}\n", o));
            } else {
                text.push_str(&format!("{}\n", o));
            }
        }
        out(&text);
        return 0;
    }
    let mut status = 0;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "-o" || arg == "+o" {
            let on = if arg == "-o" { value } else { !value };
            match args.get(i + 1) {
                Some(o) => {
                    if !crate::options::set(o, on) {
                        builtin_err(name, &format!("no such option: {}", o));
                        status = 1;
                    }
                }
                None => return usage(name, "-o requires an argument"),
            }
            i += 2;
            continue;
        }
        if (arg.starts_with('-') || arg.starts_with('+')) && arg.len() > 1 {
            // Letters: -X sets the option, +X unsets it (inverted for unsetopt).
            let on = arg.starts_with('-') == value;
            for c in arg[1..].chars() {
                match crate::options::letter(c) {
                    Some((o, v)) => {
                        crate::options::set(o, if on { v } else { !v });
                    }
                    None => {
                        builtin_err(name, &format!("bad option: -{}", c));
                        status = 1;
                    }
                }
            }
        } else if !crate::options::set(arg, value) {
            builtin_err(name, &format!("no such option: {}", arg));
            status = 1;
        }
        i += 1;
    }
    status
}

fn toggle_named_opt(name: &str, value: bool) {
    crate::options::set(name, value);
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
const HOOKS: &[&str] = &[
    "chpwd",
    "precmd",
    "preexec",
    "periodic",
    "zshaddhistory",
    "zshexit",
];

/// Runs zsh hook `hook`: the function of that name, then each function
/// named in `${hook}_functions`. Each sees the `$?` and `$pipestatus`
/// from before the hooks, and both are restored afterwards.
pub fn run_hooks(hook: &str, args: &[String]) {
    let mut names = vec![hook.to_string()];
    let list = format!("{}_functions", hook);
    names.extend(with_shell(|s| s.array(&list).cloned()).unwrap_or_default());
    let saved = with_shell(|s| (s.last_status, s.array("pipestatus").cloned()));
    let restore = || {
        with_shell(|s| {
            s.last_status = saved.0;
            match &saved.1 {
                Some(p) => s.store("pipestatus", crate::params::Value::Array(p.clone())),
                None => {
                    s.remove_param("pipestatus");
                }
            };
        })
    };
    for name in names {
        let Some((body, line)) =
            with_shell(|s| s.functions.get(&name).map(|f| (f.body.clone(), f.line)))
        else {
            continue;
        };
        restore();
        let mut words = vec![name];
        words.extend(args.iter().cloned());
        call_function(&body, &words, line);
        if matches!(crate::exec::flow(), Flow::Exit(_)) {
            return;
        }
        set_flow(Flow::None);
    }
    restore();
}

// fc -l [-n] [-r] [-d|-f|-E|-i|-t fmt] [first [last]]: lists history.
// Other forms (editing, -p/-P/-R/-W/-A history files) are accepted and
// do nothing.
fn builtin_fc(words: &[String]) -> i32 {
    let (mut list, mut numbers, mut reverse) = (false, true, false);
    let mut range = Vec::new();
    let mut i = 1;
    while let Some(a) = words.get(i) {
        if a == "--" {
            range.extend(words[i + 1..].iter().cloned());
            break;
        }
        match a
            .strip_prefix('-')
            .filter(|f| !f.is_empty() && !f.starts_with(|c: char| c.is_ascii_digit()))
        {
            Some(flags) => {
                for c in flags.chars() {
                    match c {
                        'l' => list = true,
                        'n' => numbers = false,
                        'r' => reverse = true,
                        't' => i += 1,
                        _ => {}
                    }
                }
            }
            None if a.is_empty() => {}
            None => range.push(a.clone()),
        }
        i += 1;
    }
    if !list {
        return 0;
    }
    let lines = with_shell(|s| s.history.lock().unwrap_or_else(|e| e.into_inner()).clone());
    let n = lines.len() as i64;
    let pos = |spec: &str, default: i64| -> i64 {
        match spec.parse::<i64>() {
            Ok(k) if k < 0 => (n + k + 1).max(1),
            Ok(k) => k.max(1),
            Err(_) => lines
                .iter()
                .rposition(|l| l.starts_with(spec))
                .map(|k| k as i64 + 1)
                .unwrap_or(default),
        }
    };
    let first = range.first().map(|f| pos(f, 1)).unwrap_or((n - 15).max(1));
    let last = range.get(1).map(|l| pos(l, n)).unwrap_or(n);
    let mut picked: Vec<(i64, &String)> = (first..=last.min(n))
        .filter_map(|k| lines.get((k - 1) as usize).map(|l| (k, l)))
        .collect();
    if reverse {
        picked.reverse();
    }
    let text: String = picked
        .iter()
        .map(|(k, l)| {
            if numbers {
                format!("{:5}  {}\n", k, l)
            } else {
                format!("{}\n", l)
            }
        })
        .collect();
    out(&text);
    0
}

fn drop_unknown_escapes(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&n) = chars.peek() {
                if !n.is_alphanumeric() {
                    out.push(n);
                    chars.next();
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

// zparseopts [-D] [-E] [-F] [-K] [-M] [-a array] [-A assoc] [--] spec...
// Parses options from the positional parameters. A spec is
// name[+][:|::][=array]; matched options are stored as `-name` (and the
// argument) in the spec's array or the -a array, and as keys in -A.
fn builtin_zparseopts(words: &[String]) -> i32 {
    struct Spec {
        name: String,
        arg: u8, // 0 none, 1 required, 2 optional (attached only)
        append: bool,
        array: Option<String>,
    }
    let (mut delete, mut extract, mut fail_unknown, mut keep) = (false, false, false, false);
    let (mut def_array, mut assoc) = (None::<String>, None::<String>);
    let mut i = 1;
    while let Some(a) = words.get(i) {
        match a.as_str() {
            "-D" => delete = true,
            "-E" => extract = true,
            "-F" => fail_unknown = true,
            "-K" => keep = true,
            "-M" => {}
            "-a" => {
                i += 1;
                def_array = words.get(i).cloned();
            }
            "-A" => {
                i += 1;
                assoc = words.get(i).cloned();
            }
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
        i += 1;
    }
    let mut specs = Vec::new();
    for raw in &words[i.min(words.len())..] {
        let (body, array) = match raw.split_once('=') {
            Some((b, a)) => (b, Some(a.to_string())),
            None => (raw.as_str(), None),
        };
        let mut name = body.to_string();
        let mut arg = 0;
        if let Some(n) = name.strip_suffix("::") {
            name = n.to_string();
            arg = 2;
        } else if let Some(n) = name.strip_suffix(":-").or_else(|| name.strip_suffix(':')) {
            name = n.to_string();
            arg = 1;
        }
        let append = name.ends_with('+') && name.len() > 1;
        if append {
            name.pop();
        }
        specs.push(Spec {
            name,
            arg,
            append,
            array,
        });
    }
    let params = with_shell(|s| s.pos_params.clone());
    let mut results: Vec<(usize, Vec<String>)> = Vec::new();
    let mut assoc_out: Vec<(String, String)> = Vec::new();
    let mut kept = Vec::new();
    let mut k = 0;
    while k < params.len() {
        let p = &params[k];
        if p == "-" || p == "--" {
            if !delete {
                kept.push(p.clone());
            }
            kept.extend(params[k + 1..].iter().cloned());
            break;
        }
        let Some(opt) = p.strip_prefix('-').filter(|o| !o.is_empty()) else {
            kept.push(p.clone());
            if !extract {
                kept.extend(params[k + 1..].iter().cloned());
                break;
            }
            k += 1;
            continue;
        };
        // Longest spec name that the option starts with.
        let found = specs
            .iter()
            .enumerate()
            .filter(|(_, s)| opt == s.name || (s.arg > 0 && opt.starts_with(&s.name)))
            .max_by_key(|(_, s)| s.name.len());
        let Some((idx, spec)) = found else {
            if fail_unknown {
                builtin_err("zparseopts", &format!("bad option: {}", p));
                return 1;
            }
            kept.push(p.clone());
            if !extract {
                kept.extend(params[k + 1..].iter().cloned());
                break;
            }
            k += 1;
            continue;
        };
        let mut vals = vec![format!("-{}", spec.name)];
        let attached = &opt[spec.name.len()..];
        let mut value = None;
        if spec.arg > 0 {
            if !attached.is_empty() {
                value = Some(attached.to_string());
            } else if spec.arg == 1 {
                k += 1;
                match params.get(k) {
                    Some(v) => value = Some(v.clone()),
                    None => {
                        err_line(&format!(
                            "meowsh: zparseopts: missing argument for option: {}",
                            p
                        ));
                        return 1;
                    }
                }
            }
        }
        if let Some(v) = &value {
            vals.push(v.clone());
        }
        assoc_out.push((format!("-{}", spec.name), value.unwrap_or_default()));
        match results.iter_mut().find(|(j, _)| *j == idx) {
            Some((_, existing)) if spec.append => existing.extend(vals),
            Some((_, existing)) => *existing = vals,
            None => results.push((idx, vals)),
        }
        k += 1;
    }
    let mut default_items = Vec::new();
    for (idx, vals) in &results {
        match &specs[*idx].array {
            Some(arr) => {
                array_set(arr, vals.clone());
            }
            None => default_items.extend(vals.iter().cloned()),
        }
    }
    if let Some(arr) = def_array {
        if !(keep && default_items.is_empty()) {
            array_set(&arr, default_items);
        }
    }
    if let Some(name) = assoc {
        assoc_declare(&name);
        for (key, v) in assoc_out {
            crate::shell::assoc_set(&name, &key, &v);
        }
    }
    if delete {
        with_shell(|s| s.pos_params = kept);
    }
    0
}

// is-at-least min [version]: true if version (default $ZSH_VERSION) is
// at least min, comparing the numeric parts of each `.`/`-` component.
fn builtin_is_at_least(words: &[String]) -> i32 {
    let Some(min) = words.get(1) else {
        return usage("is-at-least", "usage: is-at-least min [version]");
    };
    let have = words
        .get(2)
        .cloned()
        .unwrap_or_else(|| var_get("ZSH_VERSION"));
    let parts = |v: &str| -> Vec<u64> {
        v.split(['.', '-'])
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parts(&have), parts(min));
    for k in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(k).copied().unwrap_or(0),
            b.get(k).copied().unwrap_or(0),
        );
        if x != y {
            return if x > y { 0 } else { 1 };
        }
    }
    0
}

// zsh's `colors` function: $fg, $bg, $fg_bold, $fg_no_bold, $bg_bold,
// $bg_no_bold and $color tables, and $reset_color.
fn builtin_colors() -> i32 {
    const NAMES: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let mut tables: Vec<(&str, Vec<(String, String)>)> = Vec::new();
    let mk = |f: &dyn Fn(usize) -> String| -> Vec<(String, String)> {
        NAMES
            .iter()
            .enumerate()
            .flat_map(|(k, n)| {
                let v = f(k);
                [(n.to_string(), v.clone()), (n.to_uppercase(), v)]
            })
            .collect()
    };
    tables.push(("fg", mk(&|k| format!("\x1b[{}m", 30 + k))));
    tables.push(("bg", mk(&|k| format!("\x1b[{}m", 40 + k))));
    tables.push(("fg_bold", mk(&|k| format!("\x1b[01;{}m", 30 + k))));
    tables.push(("fg_no_bold", mk(&|k| format!("\x1b[22;{}m", 30 + k))));
    tables.push(("bg_bold", mk(&|k| format!("\x1b[01;{}m", 40 + k))));
    tables.push(("bg_no_bold", mk(&|k| format!("\x1b[22;{}m", 40 + k))));
    let mut color: Vec<(String, String)> = Vec::new();
    for (k, n) in NAMES.iter().enumerate() {
        color.push((n.to_string(), (30 + k).to_string()));
        color.push((format!("bg-{}", n), (40 + k).to_string()));
    }
    for (n, c) in [
        ("reset", 0),
        ("bold", 1),
        ("faint", 2),
        ("italic", 3),
        ("underline", 4),
        ("blink", 5),
        ("reverse", 7),
        ("normal", 22),
    ] {
        color.push((n.to_string(), c.to_string()));
    }
    tables.push(("color", color));
    for (name, entries) in tables {
        assoc_declare(name);
        for (k, v) in entries {
            crate::shell::assoc_set(name, &k, &v);
        }
    }
    var_set("reset_color", "\x1b[00m", false);
    var_set("bold_color", "\x1b[01m", false);
    0
}

// emulate: meowsh always behaves as zsh here. With no arguments it
// reports the mode; `-c cmd` runs cmd; option changes are ignored.
// emulate [-lLR] [zsh|sh|ksh|csh [-c arg]]: with no mode, prints the
// current one. -R resets every option, -L makes the change local to the
// calling function, -c runs `arg` under the emulation and then restores
// the options.
fn builtin_emulate(words: &[String]) -> i32 {
    let (mut reset_all, mut local) = (false, false);
    let mut mode = None;
    let mut cmd = None;
    let mut i = 1;
    while let Some(a) = words.get(i) {
        match a.as_str() {
            "-c" => {
                i += 1;
                cmd = words.get(i).cloned();
                if cmd.is_none() {
                    return usage("emulate", "-c requires an argument");
                }
            }
            a if a.starts_with('-') && a.len() > 1 => {
                for c in a[1..].chars() {
                    match c {
                        'R' => reset_all = true,
                        'L' => local = true,
                        'l' => {}
                        _ => return usage("emulate", &format!("bad option: -{}", c)),
                    }
                }
            }
            m => match crate::options::Emulation::from_name(m) {
                Some(e) if mode.is_none() => mode = Some(e),
                _ => return usage("emulate", &format!("unknown emulation: {}", m)),
            },
        }
        i += 1;
    }
    let Some(mode) = mode else {
        out(&format!("{}\n", with_shell(|s| s.emulation.name())));
        return 0;
    };
    if let Some(cmd) = cmd {
        let saved = with_shell(|s| {
            (
                s.opts,
                s.named_opts.clone(),
                s.emulation,
                s.assoc("options").cloned(),
            )
        });
        with_shell(|s| crate::options::emulate_in(s, mode, reset_all));
        let st = run_string(&cmd);
        with_shell(|s| {
            crate::options::restore_in(s, &saved.1);
            s.opts = saved.0;
            s.emulation = saved.2;
            if let Some(o) = saved.3 {
                s.store("options", crate::params::Value::Assoc(o));
            }
        });
        return st;
    }
    with_shell(|s| crate::options::emulate_in(s, mode, reset_all));
    if local {
        for o in ["localoptions", "localtraps", "localpatterns"] {
            toggle_named_opt(o, true);
        }
    }
    0
}

// add-zsh-hook [-d] hook function
fn builtin_add_zsh_hook(words: &[String]) -> i32 {
    let mut args: Vec<&str> = words[1..].iter().map(|s| s.as_str()).collect();
    let mut delete = false;
    while let Some(a) = args.first() {
        match *a {
            "-d" => delete = true,
            "-D" | "-U" | "-z" | "-k" => {}
            _ => break,
        }
        args.remove(0);
    }
    let [hook, func] = args[..] else {
        return usage("add-zsh-hook", "usage: add-zsh-hook [-d] hook function");
    };
    if !HOOKS.contains(&hook) {
        return usage("add-zsh-hook", &format!("{}: unknown hook", hook));
    }
    let list = format!("{}_functions", hook);
    with_shell(|s| {
        let mut fns = s.array(&list).cloned().unwrap_or_default();
        fns.retain(|f| f != func);
        if !delete {
            fns.push(func.to_string());
        }
        s.store(&list, crate::params::Value::Array(fns));
    });
    0
}

// zsh `print`: echo-style escapes unless -r; -n no newline, -l one
// argument per line, -N NUL-separated, -P prompt escapes, -u fd.
fn builtin_print(words: &[String]) -> i32 {
    let mut i = 1;
    let (mut raw, mut newline, mut lines, mut nul, mut prompt) = (false, true, false, false, false);
    let (mut sort, mut icase) = (None::<bool>, false);
    let mut fd = 1;
    while let Some(a) = words.get(i) {
        if a == "-" || a == "--" {
            i += 1;
            break;
        }
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        if !flags
            .chars()
            .all(|c| "rRnlNPeEupoOisz0123456789".contains(c))
        {
            break;
        }
        let mut chars = flags.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                'r' | 'R' => raw = true,
                'E' => raw = true,
                'e' => raw = false,
                'n' => newline = false,
                'l' => lines = true,
                'N' => nul = true,
                'P' => prompt = true,
                // -p: to the coprocess.
                'p' => match with_shell(|s| s.coproc) {
                    Some((w, _)) => fd = w,
                    None => return usage("print", "-p: no coprocess"),
                },
                'o' => sort = Some(false),
                'O' => sort = Some(true),
                'i' => icase = true,
                'u' => {
                    let rest: String = chars.by_ref().collect();
                    let spec = if rest.is_empty() {
                        i += 1;
                        words.get(i).cloned().unwrap_or_default()
                    } else {
                        rest
                    };
                    match spec.parse() {
                        Ok(n) => fd = n,
                        Err(_) => return usage("print", &format!("{}: bad file number", spec)),
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    let sep = if nul {
        "\0"
    } else if lines {
        "\n"
    } else {
        " "
    };
    let mut cut = false;
    let mut parts = Vec::new();
    for a in &words[i.min(words.len())..] {
        let mut t = a.clone();
        if !raw {
            if let Some(k) = t.find("\\c") {
                t.truncate(k);
                cut = true;
            }
            t = crate::expand::decode_escapes(&t);
        }
        if prompt {
            // Backslashes that quoted prompt characters (as `${x:q}`
            // leaves them) are dropped before prompt expansion.
            if !raw {
                t = drop_unknown_escapes(&t);
            }
            t = crate::shell::expand_prompt(&t);
        }
        parts.push(t);
        if cut {
            break;
        }
    }
    if let Some(desc) = sort {
        if icase {
            parts.sort_by_key(|p| p.to_lowercase());
        } else {
            parts.sort();
        }
        if desc {
            parts.reverse();
        }
    }
    let mut text = parts.join(sep);
    if newline && !cut {
        text.push(if nul { '\0' } else { '\n' });
    }
    match crate::sys::write_fd(fd, text.as_bytes()) {
        Ok(()) => 0,
        Err(e) => {
            builtin_err("print", &format!("write error: {}", e));
            1
        }
    }
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
