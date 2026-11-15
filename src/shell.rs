use crate::types::{
    Shell, Var, OPT_ALLEXPORT, OPT_ERREXIT, OPT_HASHALL, OPT_INTERACTIVE, OPT_MONITOR, OPT_NOEXEC,
    OPT_NOGLOB, OPT_NOUNSET, OPT_VERBOSE, OPT_XTRACE, VAR_EXPORT,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct ShellState {
    pub shell: Mutex<Shell>,
}

impl Default for ShellState {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellState {
    pub fn new() -> Self {
        ShellState {
            shell: Mutex::new(Shell::new()),
        }
    }
}

use std::sync::LazyLock;

pub static SHELL: LazyLock<ShellState> = LazyLock::new(ShellState::new);

/// Runs `f` with the global shell state locked. Recovers from a poisoned
/// lock so a panic in one builtin does not wedge the shell.
pub fn with_shell<R>(f: impl FnOnce(&mut Shell) -> R) -> R {
    let mut guard = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

pub fn shell_init() {
    let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
    shell.shell_pid = std::process::id() as i32;
    shell.shell_pgid = unsafe { libc::getpgrp() };
    shell.last_status = 0;
    *shell.next_job_id.lock().unwrap() = 1;

    // Every environment variable becomes an exported shell variable.
    for (key, value) in env::vars_os() {
        let (Some(key), Some(value)) = (key.to_str(), value.to_str()) else {
            continue;
        };
        if crate::lexer::is_valid_name(key) {
            shell.vars.insert(
                key.to_string(),
                Var {
                    value: value.to_string(),
                    flags: VAR_EXPORT,
                },
            );
        }
    }

    let set_default = |shell: &mut Shell, name: &str, value: String, flags: u32| {
        shell
            .vars
            .entry(name.to_string())
            .or_insert(Var { value, flags });
    };
    set_default(
        &mut shell,
        "PATH",
        "/usr/local/bin:/usr/bin:/bin".to_string(),
        VAR_EXPORT,
    );
    set_default(&mut shell, "PS2", "> ".to_string(), 0);
    set_default(&mut shell, "HISTSIZE", "1000".to_string(), 0);
    set_default(&mut shell, "SAVEHIST", "1000".to_string(), 0);
    // zsh frameworks such as oh-my-zsh refuse to load without
    // ZSH_VERSION. The suffix keeps per-version files (for example
    // ~/.zcompdump-host-$ZSH_VERSION) apart from real zsh's.
    for (name, value) in [
        ("ZSH_VERSION", "5.9-meowsh".to_string()),
        ("MEOWSH_VERSION", env!("CARGO_PKG_VERSION").to_string()),
        ("HOST", hostname_full()),
    ] {
        shell.vars.insert(name.to_string(), Var { value, flags: 0 });
    }
    set_default(&mut shell, "PS4", "+ ".to_string(), 0);
    // POSIX: IFS is reset at startup rather than inherited.
    shell.vars.insert(
        "IFS".to_string(),
        Var {
            value: " \t\n".to_string(),
            flags: 0,
        },
    );
    let ppid = unsafe { libc::getppid() };
    shell.vars.insert(
        "PPID".to_string(),
        Var {
            value: ppid.to_string(),
            flags: 0,
        },
    );
    let shlvl = shell
        .vars
        .get("SHLVL")
        .and_then(|v| v.value.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .max(0)
        + 1;
    shell.vars.insert(
        "SHLVL".to_string(),
        Var {
            value: shlvl.to_string(),
            flags: VAR_EXPORT,
        },
    );
    let pwd = initial_pwd(shell.vars.get("PWD").map(|v| v.value.as_str()));
    if let Some(pwd) = pwd {
        shell.vars.insert(
            "PWD".to_string(),
            Var {
                value: pwd,
                flags: VAR_EXPORT,
            },
        );
    }

    if let Some(home) = shell.vars.get("HOME") {
        if !home.value.is_empty() {
            shell.history_file = PathBuf::from(&home.value)
                .join(".meowsh_history")
                .to_string_lossy()
                .to_string();
        }
    }

    START.get_or_init(std::time::Instant::now);
    // zsh/langinfo: the character set of the locale.
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default()
        .to_lowercase();
    let codeset = if locale.contains("utf-8") || locale.contains("utf8") {
        "UTF-8"
    } else {
        "US-ASCII"
    };
    shell
        .assoc_arrays
        .entry("langinfo".to_string())
        .or_default()
        .insert("CODESET".to_string(), codeset.to_string());
    seed_tied_array(&mut shell, "path", "PATH");
    seed_tied_array(&mut shell, "fpath", "FPATH");
}

// Keeps an inherited $PWD only if it is absolute and names the current
// directory; otherwise uses the physical working directory.
fn initial_pwd(inherited: Option<&str>) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let cwd = env::current_dir().ok()?;
    if let Some(p) = inherited {
        if p.starts_with('/') {
            if let (Ok(a), Ok(b)) = (std::fs::metadata(p), std::fs::metadata(&cwd)) {
                if a.dev() == b.dev() && a.ino() == b.ino() {
                    return Some(p.to_string());
                }
            }
        }
    }
    Some(cwd.to_string_lossy().into_owned())
}

pub fn opt_set(flag: u32) -> bool {
    with_shell(|s| s.opts & flag != 0)
}

pub fn alias_get(name: &str) -> Option<String> {
    with_shell(|s| s.alias(name, false).cloned())
}

pub fn galias_get(name: &str) -> Option<String> {
    with_shell(|s| s.alias(name, true).cloned())
}

/// Value of a parameter, or None if it is unset.
pub fn var_get_opt(name: &str) -> Option<String> {
    if var_exists(name) {
        Some(var_get(name))
    } else {
        None
    }
}

/// Exported variables for a child process, with `overrides` applied.
pub fn build_env(overrides: &[(String, String)]) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> = with_shell(|s| {
        s.vars
            .iter()
            .filter(|(_, v)| v.flags & VAR_EXPORT != 0)
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    });
    for (k, v) in overrides {
        map.insert(k.clone(), v.clone());
    }
    map.into_iter().collect()
}

static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static RANDOM_STATE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn terminal_size() -> Option<(u16, u16)> {
    let tty = crate::jobs::TTY_FD.load(std::sync::atomic::Ordering::SeqCst);
    for fd in [tty, 2, 0, 1] {
        if fd < 0 {
            continue;
        }
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
            return Some((ws.ws_col, ws.ws_row));
        }
    }
    None
}

/// Parameters computed on each read, as in zsh. A user assignment to
/// RANDOM reseeds it; the others ignore assignments.
fn dynamic_var(name: &str) -> Option<String> {
    use std::sync::atomic::Ordering;
    match name {
        "RANDOM" => {
            let mut x = RANDOM_STATE.load(Ordering::Relaxed);
            if x == 0 {
                let t = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(1);
                x = t ^ ((std::process::id() as u64) << 32) | 1;
            }
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            RANDOM_STATE.store(x, Ordering::Relaxed);
            Some(((x >> 33) % 32768).to_string())
        }
        "SECONDS" => Some(
            START
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_secs()
                .to_string(),
        ),
        "EPOCHSECONDS" | "EPOCHREALTIME" => {
            let d = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            Some(if name == "EPOCHSECONDS" {
                d.as_secs().to_string()
            } else {
                format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
            })
        }
        "COLUMNS" => terminal_size().map(|(c, _)| c.to_string()),
        "LINES" => terminal_size().map(|(_, r)| r.to_string()),
        _ => None,
    }
}

/// Reseeds `$RANDOM` after `RANDOM=n`.
pub fn seed_random(seed: u64) {
    RANDOM_STATE.store(
        seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1,
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub fn var_get(name: &str) -> String {
    if let Some(v) = dynamic_var(name) {
        return v;
    }
    let shell = SHELL.shell.lock().unwrap();
    match name {
        "$" => return format!("{}", shell.shell_pid),
        "?" => return format!("{}", shell.last_status),
        "!" => return format!("{}", shell.last_bg_pid),
        "0" => return shell.argv0.clone(),
        "#" => return format!("{}", shell.pos_params.len()),
        "@" | "*" => return shell.pos_params.join(" "),
        "-" => {
            let mut flags = String::new();
            if shell.opts & OPT_ALLEXPORT != 0 {
                flags.push('a');
            }
            if shell.opts & OPT_ERREXIT != 0 {
                flags.push('e');
            }
            if shell.opts & OPT_NOGLOB != 0 {
                flags.push('f');
            }
            if shell.opts & OPT_HASHALL != 0 {
                flags.push('h');
            }
            if shell.opts & OPT_INTERACTIVE != 0 {
                flags.push('i');
            }
            if shell.opts & OPT_MONITOR != 0 {
                flags.push('m');
            }
            if shell.opts & OPT_NOEXEC != 0 {
                flags.push('n');
            }
            if shell.opts & OPT_NOUNSET != 0 {
                flags.push('u');
            }
            if shell.opts & OPT_VERBOSE != 0 {
                flags.push('v');
            }
            if shell.opts & OPT_XTRACE != 0 {
                flags.push('x');
            }
            return flags;
        }
        _ => {}
    }

    if name.len() > 1 && name.starts_with('-') {
        return var_get(&name[1..]);
    }

    // Numeric positional parameter lookup ($1, $2, ...).
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(idx) = name.parse::<usize>() {
            if idx == 0 {
                return shell.argv0.clone();
            }
            if let Some(p) = shell.pos_params.get(idx - 1) {
                return p.clone();
            }
            return String::new();
        }
    }

    if let Some(arr) = shell.arrays.get(name) {
        return arr.join(" ");
    }
    if let Some(assoc) = shell.assoc_arrays.get(name) {
        return assoc.values().cloned().collect::<Vec<_>>().join(" ");
    }
    if let Some(v) = shell.vars.get(name) {
        return v.value.clone();
    }
    String::new()
}

pub fn var_exists(name: &str) -> bool {
    if matches!(
        name,
        "RANDOM" | "SECONDS" | "EPOCHSECONDS" | "EPOCHREALTIME"
    ) || (matches!(name, "COLUMNS" | "LINES") && terminal_size().is_some())
    {
        return true;
    }
    let shell = SHELL.shell.lock().unwrap();
    if matches!(name, "?" | "$" | "!" | "0" | "#" | "@" | "*" | "-") {
        return true;
    }
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(idx) = name.parse::<usize>() {
            return idx == 0 || idx <= shell.pos_params.len();
        }
    }
    shell.vars.contains_key(name)
        || shell.arrays.contains_key(name)
        || shell.assoc_arrays.contains_key(name)
}

pub fn var_get_if_exists(name: &str) -> Option<i64> {
    let shell = SHELL.shell.lock().unwrap();
    if let Some(v) = shell.vars.get(name) {
        if let Ok(n) = v.value.parse::<i64>() {
            return Some(n);
        }
    }
    None
}

pub fn var_set(name: &str, value: &str, export: bool) -> bool {
    if name == "RANDOM" {
        seed_random(value.trim().parse().unwrap_or(0));
    }
    let mut shell = SHELL.shell.lock().unwrap();
    if shell
        .vars
        .get(name)
        .map(|v| v.flags & crate::types::VAR_READONLY != 0)
        .unwrap_or(false)
    {
        eprintln!("meowsh: {}: readonly variable", name);
        return false;
    }
    // Setting a scalar with the same name as an existing array clears the
    // array — they share a namespace.
    shell.arrays.remove(name);
    shell.assoc_arrays.remove(name);
    let mut flags = shell.vars.get(name).map(|v| v.flags).unwrap_or(0);
    if export || shell.opts & OPT_ALLEXPORT != 0 {
        flags |= VAR_EXPORT;
    }
    shell.vars.insert(
        name.to_string(),
        Var {
            value: value.to_string(),
            flags,
        },
    );

    if let Some(array_name) = scalar_to_tied_array(name) {
        let items = split_colon_list(value);
        shell.arrays.insert(array_name.to_string(), items);
    }
    true
}

pub fn array_set(name: &str, items: Vec<String>) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if shell.assoc_arrays.contains_key(name) {
        let assoc = assoc_from_items(items);
        shell.assoc_arrays.insert(name.to_string(), assoc);
        return true;
    }
    let items = maybe_unique_items(&shell, name, items);
    sync_tied_scalar(&mut shell, name, &items);
    shell.arrays.insert(name.to_string(), items);
    true
}

pub fn array_append(name: &str, items: Vec<String>) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if let Some(existing) = shell.assoc_arrays.get(name).cloned() {
        let mut assoc = existing;
        assoc.extend(assoc_from_items(items));
        shell.assoc_arrays.insert(name.to_string(), assoc);
        return true;
    }
    let mut combined = shell.arrays.get(name).cloned().unwrap_or_default();
    combined.extend(items);
    let combined = maybe_unique_items(&shell, name, combined);
    sync_tied_scalar(&mut shell, name, &combined);
    shell.arrays.insert(name.to_string(), combined);
    true
}

pub fn array_set_subscript(name: &str, subscript: &str, value: String, append: bool) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if let Some(assoc) = shell.assoc_arrays.get_mut(name) {
        let new_value = if append {
            let mut current = assoc.get(subscript).cloned().unwrap_or_default();
            current.push_str(&value);
            current
        } else {
            value
        };
        assoc.insert(subscript.to_string(), new_value);
        return true;
    }

    let Ok(index) = subscript.parse::<isize>() else {
        return false;
    };
    if index == 0 {
        return false;
    }
    let mut items = shell.arrays.get(name).cloned().unwrap_or_default();
    let idx = if index > 0 {
        (index - 1) as usize
    } else {
        let len = items.len() as isize;
        let resolved = len + index;
        if resolved < 0 {
            return false;
        }
        resolved as usize
    };
    if items.len() <= idx {
        items.resize(idx + 1, String::new());
    }
    if append {
        items[idx].push_str(&value);
    } else {
        items[idx] = value;
    }
    let items = maybe_unique_items(&shell, name, items);
    sync_tied_scalar(&mut shell, name, &items);
    shell.arrays.insert(name.to_string(), items);
    true
}

pub fn array_make_unique(name: &str) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    shell.unique_arrays.insert(name.to_string());
    if let Some(items) = shell.arrays.get(name).cloned() {
        let items = unique_items(items);
        sync_tied_scalar(&mut shell, name, &items);
        shell.arrays.insert(name.to_string(), items);
    }
    true
}

pub fn array_declare(name: &str) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    shell.assoc_arrays.remove(name);
    shell.arrays.entry(name.to_string()).or_default();
    let items = shell.arrays.get(name).cloned().unwrap_or_default();
    sync_tied_scalar(&mut shell, name, &items);
    true
}

pub fn assoc_declare(name: &str) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    shell.arrays.remove(name);
    shell.assoc_arrays.entry(name.to_string()).or_default();
    true
}

pub fn assoc_set(name: &str, key: &str, value: &str) {
    let mut shell = SHELL.shell.lock().unwrap();
    shell
        .assoc_arrays
        .entry(name.to_string())
        .or_default()
        .insert(key.to_string(), value.to_string());
}

pub fn assoc_get(name: &str, key: &str) -> Option<String> {
    SHELL
        .shell
        .lock()
        .unwrap()
        .assoc_arrays
        .get(name)
        .and_then(|assoc| assoc.get(key).cloned())
}

pub fn assoc_values(name: &str) -> Option<Vec<String>> {
    SHELL
        .shell
        .lock()
        .unwrap()
        .assoc_arrays
        .get(name)
        .map(|assoc| assoc.values().cloned().collect())
}

pub fn assoc_keys(name: &str) -> Option<Vec<String>> {
    SHELL
        .shell
        .lock()
        .unwrap()
        .assoc_arrays
        .get(name)
        .map(|assoc| assoc.keys().cloned().collect())
}

fn prepare_array_mutation(shell: &mut Shell, name: &str) -> bool {
    if let Some(var) = shell.vars.get_mut(name) {
        if var.flags & crate::types::VAR_READONLY != 0 {
            eprintln!("meowsh: {}: readonly variable", name);
            return false;
        }
        var.value.clear();
    }
    true
}

fn seed_tied_array(shell: &mut Shell, array_name: &str, scalar_name: &str) {
    if shell.arrays.contains_key(array_name) {
        return;
    }
    if let Some(var) = shell.vars.get(scalar_name) {
        shell
            .arrays
            .insert(array_name.to_string(), split_colon_list(&var.value));
    }
}

fn scalar_to_tied_array(name: &str) -> Option<&'static str> {
    match name {
        "PATH" => Some("path"),
        "FPATH" => Some("fpath"),
        _ => None,
    }
}

fn tied_array_to_scalar(name: &str) -> Option<&'static str> {
    match name {
        "path" => Some("PATH"),
        "fpath" => Some("FPATH"),
        _ => None,
    }
}

fn split_colon_list(value: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(':').map(|s| s.to_string()).collect()
    }
}

fn sync_tied_scalar(shell: &mut Shell, name: &str, items: &[String]) {
    if let Some(scalar_name) = tied_array_to_scalar(name) {
        let value = items.join(":");
        let flags = shell
            .vars
            .get(scalar_name)
            .map(|v| v.flags)
            .unwrap_or(VAR_EXPORT);
        shell
            .vars
            .insert(scalar_name.to_string(), Var { value, flags });
    }
}

fn maybe_unique_items(shell: &Shell, name: &str, items: Vec<String>) -> Vec<String> {
    if shell.unique_arrays.contains(name) {
        unique_items(items)
    } else {
        items
    }
}

fn unique_items(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn assoc_from_items(items: Vec<String>) -> BTreeMap<String, String> {
    let mut assoc = BTreeMap::new();
    let mut iter = items.into_iter();
    while let Some(key) = iter.next() {
        let value = iter.next().unwrap_or_default();
        assoc.insert(key, value);
    }
    assoc
}

pub fn array_get(name: &str) -> Option<Vec<String>> {
    SHELL.shell.lock().unwrap().arrays.get(name).cloned()
}

pub fn is_array(name: &str) -> bool {
    let shell = SHELL.shell.lock().unwrap();
    shell.arrays.contains_key(name) || shell.assoc_arrays.contains_key(name)
}

pub fn push_scope() {
    let mut shell = SHELL.shell.lock().unwrap();
    shell.local_scopes.push(HashMap::new());
}

pub fn pop_scope() {
    let mut shell = SHELL.shell.lock().unwrap();
    if let Some(scope) = shell.local_scopes.pop() {
        for (name, prev) in scope {
            match prev {
                Some(var) => {
                    shell.vars.insert(name, var);
                }
                None => {
                    shell.vars.remove(&name);
                }
            }
        }
    }
}

pub fn in_function_scope() -> bool {
    !SHELL.shell.lock().unwrap().local_scopes.is_empty()
}

// Snapshot the current value of `name` into the innermost scope so it will
// be restored on scope exit. Returns true if a scope existed (i.e. we are
// inside a function), false otherwise. The first call for a given name
// wins; subsequent calls in the same scope are no-ops to preserve the
// original prior value.
pub fn declare_local(name: &str) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    let prev = shell.vars.get(name).cloned();
    if let Some(scope) = shell.local_scopes.last_mut() {
        scope.entry(name.to_string()).or_insert(prev);
        true
    } else {
        false
    }
}

pub fn shorten_path(p: &str) -> String {
    let home = var_get("HOME");
    let home = home.trim_end_matches('/');
    if !home.is_empty() {
        if let Some(rest) = p.strip_prefix(home) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{}", rest);
            }
        }
    }
    p.to_string()
}

pub fn build_prompt() -> String {
    let prompt_str = {
        let p = var_get("PROMPT");
        if !p.is_empty() {
            p
        } else {
            var_get("PS1")
        }
    };
    if prompt_str.is_empty() {
        let user = var_get("USER");
        let user = if user.is_empty() { "meow" } else { &user };
        let pwd = var_get("PWD");
        let pwd = if pwd.is_empty() { "?" } else { &pwd };
        let short_pwd = shorten_path(pwd);
        return format!("\x1b[32m{}\x1b[0m \x1b[34m{}\x1b[0m 𓃠  ", user, short_pwd);
    }
    render_prompt(&prompt_str)
}

/// Parameter, command and arithmetic expansion, then zsh `%` escapes, the
/// order zsh uses with PROMPT_SUBST. Text produced by a substitution may
/// itself contain `%` escapes, as starship's output does.
pub fn render_prompt(s: &str) -> String {
    let expanded = crate::expand::expand_prompt_string(s).unwrap_or_else(|_| s.to_string());
    expand_prompt(&expanded)
}

pub fn expand_prompt(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        // %N<str< ... %<< and %N>str> ... %>>: truncate the text up to
        // the closing marker to N characters, marking the cut with str.
        let mut digits = String::new();
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            digits.push(chars.next().unwrap());
        }
        if matches!(chars.peek(), Some('<' | '>')) {
            let dir = chars.next().unwrap();
            let mut marker = String::new();
            for ch in chars.by_ref() {
                if ch == dir {
                    break;
                }
                marker.push(ch);
            }
            if digits.is_empty() {
                // A bare %<< ends a truncation region; nothing to do here.
                continue;
            }
            let end_tag = format!("%{0}{0}", dir);
            let rest: String = chars.collect();
            let (region, after) = match rest.find(&end_tag) {
                Some(k) => (&rest[..k], &rest[k + end_tag.len()..]),
                None => (rest.as_str(), ""),
            };
            let text: Vec<char> = expand_prompt(region).chars().collect();
            let n: usize = digits.parse().unwrap_or(0);
            if text.len() <= n {
                out.extend(text);
            } else {
                let keep = n.saturating_sub(marker.chars().count());
                if dir == '<' {
                    out.push_str(&marker);
                    out.extend(&text[text.len() - keep..]);
                } else {
                    out.extend(&text[..keep]);
                    out.push_str(&marker);
                }
            }
            out.push_str(&expand_prompt(after));
            return out;
        }
        // Numeric arguments to other escapes (%3~ and so on) are ignored.
        match chars.next() {
            None => out.push('%'),
            Some('%') => out.push('%'),
            Some('n') => out.push_str(&var_get("USER")),
            Some('m') => out.push_str(&hostname_short()),
            Some('M') => out.push_str(&hostname_full()),
            Some('~') => out.push_str(&shorten_path(&var_get("PWD"))),
            Some('d') | Some('/') => out.push_str(&var_get("PWD")),
            Some('c') | Some('.') | Some('C') => out.push_str(&pwd_basename()),
            Some('#') => out.push(if is_root() { '#' } else { '%' }),
            Some('T') => out.push_str(&time_fmt("%H:%M")),
            Some('t') | Some('@') => out.push_str(&time_fmt("%I:%M%p")),
            Some('*') => out.push_str(&time_fmt("%H:%M:%S")),
            Some('D') => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let fmt = read_until_brace(&mut chars);
                    out.push_str(&time_fmt(&fmt));
                } else {
                    out.push_str(&time_fmt("%y-%m-%d"));
                }
            }
            Some('w') => out.push_str(&time_fmt("%a %d")),
            Some('W') => out.push_str(&time_fmt("%m/%d/%y")),
            Some('!') | Some('h') => out.push_str(&history_count()),
            Some('?') => out.push_str(&format!("{}", SHELL.shell.lock().unwrap().last_status)),
            Some('j') => out.push_str(&jobs_count()),
            Some('B') => out.push_str("\x1b[1m"),
            Some('b') => out.push_str("\x1b[22m"),
            Some('U') => out.push_str("\x1b[4m"),
            Some('u') => out.push_str("\x1b[24m"),
            Some('S') => out.push_str("\x1b[7m"),
            Some('s') => out.push_str("\x1b[27m"),
            Some('F') => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let name = read_until_brace(&mut chars);
                    out.push_str(&color_fg(&name));
                } else {
                    out.push_str("\x1b[39m");
                }
            }
            Some('f') => out.push_str("\x1b[39m"),
            Some('K') => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    let name = read_until_brace(&mut chars);
                    out.push_str(&color_bg(&name));
                } else {
                    out.push_str("\x1b[49m");
                }
            }
            Some('k') => out.push_str("\x1b[49m"),
            // %{ raw %} — content is emitted verbatim. We don't track
            // visual width, so the markers are effectively no-ops.
            Some('{') => loop {
                match chars.next() {
                    None => break,
                    Some('%') => {
                        if chars.peek() == Some(&'}') {
                            chars.next();
                            break;
                        }
                        out.push('%');
                    }
                    Some(ch) => out.push(ch),
                }
            },
            Some(other) => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn read_until_brace(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(&ch) = chars.peek() {
        chars.next();
        if ch == '}' {
            break;
        }
        s.push(ch);
    }
    s
}

fn hostname_full() -> String {
    let mut buf = vec![0u8; 256];
    unsafe {
        if libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) == 0 {
            let cstr = std::ffi::CStr::from_ptr(buf.as_ptr() as *const libc::c_char);
            return cstr.to_string_lossy().into_owned();
        }
    }
    String::new()
}

fn hostname_short() -> String {
    let h = hostname_full();
    h.split('.').next().unwrap_or(&h).to_string()
}

fn pwd_basename() -> String {
    let pwd = var_get("PWD");
    if pwd.is_empty() {
        return String::new();
    }
    PathBuf::from(&pwd)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| pwd.clone())
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn time_fmt(fmt: &str) -> String {
    let cfmt = match std::ffi::CString::new(fmt) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let mut buf = vec![0u8; 128];
    unsafe {
        let mut t: libc::time_t = 0;
        libc::time(&mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return String::new();
        }
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            cfmt.as_ptr(),
            &tm,
        );
        if n == 0 {
            return String::new();
        }
        buf.truncate(n);
        String::from_utf8_lossy(&buf).into_owned()
    }
}

fn history_count() -> String {
    let shell = SHELL.shell.lock().unwrap();
    let n = shell.history.lock().unwrap().len();
    n.to_string()
}

fn jobs_count() -> String {
    let shell = SHELL.shell.lock().unwrap();
    let n = shell.jobs.lock().unwrap().len();
    n.to_string()
}

fn color_code(name: &str) -> Option<u16> {
    match name.to_ascii_lowercase().as_str() {
        "black" => Some(0),
        "red" => Some(1),
        "green" => Some(2),
        "yellow" => Some(3),
        "blue" => Some(4),
        "magenta" => Some(5),
        "cyan" => Some(6),
        "white" => Some(7),
        "default" => Some(9),
        _ => name.parse::<u16>().ok(),
    }
}

fn color_fg(name: &str) -> String {
    match color_code(name) {
        Some(9) => "\x1b[39m".to_string(),
        Some(n) if n < 8 => format!("\x1b[{}m", 30 + n),
        Some(n) => format!("\x1b[38;5;{}m", n),
        None => String::new(),
    }
}

fn color_bg(name: &str) -> String {
    match color_code(name) {
        Some(9) => "\x1b[49m".to_string(),
        Some(n) if n < 8 => format!("\x1b[{}m", 40 + n),
        Some(n) => format!("\x1b[48;5;{}m", n),
        None => String::new(),
    }
}

/// Names of all executables in $PATH, for zsh's `${(k)commands}`.
pub fn all_commands() -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for dir in var_get("PATH").split(':').filter(|d| !d.is_empty()) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if is_executable(&e.path()) {
                    names.insert(e.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    names.into_iter().collect()
}

pub fn find_command(name: &str) -> Option<String> {
    if name.contains('/') {
        let p = std::path::Path::new(name);
        if p.is_file() && is_executable(p) {
            return Some(name.to_string());
        }
        return None;
    }

    let path = var_get("PATH");
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let p = std::path::Path::new(dir).join(name);
        if p.is_file() && is_executable(&p) {
            return Some(p.to_string_lossy().to_string());
        }
    }
    None
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = path.metadata() {
            let mode = metadata.permissions().mode();
            return metadata.is_file() && (mode & 0o111 != 0);
        }
    }
    path.is_file()
}

pub fn command_exists(name: &str) -> bool {
    let name = crate::strip_ansi(name);
    let name = name.as_str();
    {
        let shell = SHELL.shell.lock().unwrap();
        if shell.alias(name, false).is_some() || shell.functions.contains_key(name) {
            return true;
        }
    }

    if crate::builtins::is_builtin(name) {
        return true;
    }

    find_command(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TEST_LOCK;

    #[test]
    fn test_expand_prompt_literal_percent() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        assert_eq!(expand_prompt("%%"), "%");
    }

    #[test]
    fn test_expand_prompt_color_named() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%F{red}x%f");
        assert!(s.contains("\x1b[31m"), "missing red SGR in {:?}", s);
        assert!(s.contains('x'));
        assert!(
            s.contains("\x1b[39m"),
            "missing default fg reset in {:?}",
            s
        );
    }

    #[test]
    fn test_expand_prompt_color_numeric() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%F{208}orange%f");
        assert!(s.contains("\x1b[38;5;208m"));
    }

    #[test]
    fn test_expand_prompt_bold_underline() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%B%Utext%u%b");
        assert!(s.contains("\x1b[1m"));
        assert!(s.contains("\x1b[4m"));
        assert!(s.contains("\x1b[24m"));
        assert!(s.contains("\x1b[22m"));
    }

    #[test]
    fn test_expand_prompt_user_and_pwd() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        var_set("USER", "alice", false);
        var_set("HOME", "/home/alice", false);
        var_set("PWD", "/home/alice/projects", false);
        let s = expand_prompt("%n at %~");
        assert_eq!(s, "alice at ~/projects");
    }

    #[test]
    fn test_expand_prompt_passes_through_unknown() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%Z");
        assert_eq!(s, "%Z");
    }

    #[test]
    fn test_expand_prompt_raw_braces() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        let s = expand_prompt("%{\x1b[31m%}red%{\x1b[0m%}");
        assert_eq!(s, "\x1b[31mred\x1b[0m");
    }

    #[test]
    fn test_prompt_truncation() {
        assert_eq!(expand_prompt("%5<..<abcdefgh%<<!"), "..fgh!");
        assert_eq!(expand_prompt("%5>..>abcdefgh%>>!"), "abc..!");
        assert_eq!(expand_prompt("%9<..<short%<<"), "short");
    }

    #[test]
    fn test_prompt_expands_substitutions_before_percent_escapes() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        var_set("PS1", "", false);
        var_set("PROMPT", "$(printf '%%{X%%}a')%%", false);
        assert_eq!(build_prompt(), "Xa%");
    }

    #[test]
    fn test_build_prompt_uses_cat_fallback_without_ps1() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        shell_init();
        var_set("PS1", "", false);
        var_set("PROMPT", "", false);
        var_set("USER", "alice", false);
        var_set("HOME", "/home/alice", false);
        var_set("PWD", "/home/alice/projects", false);

        let s = build_prompt();
        assert!(s.contains("alice"));
        assert!(s.contains("~/projects"));
        assert!(s.contains("𓃠"));
    }

    #[test]
    fn test_diagnostic_commands() {
        let _lock = TEST_LOCK.lock().unwrap();
        shell_init();
        let path = var_get("PATH");
        println!("PATH in meowsh: {}", path);

        let ls_exists = command_exists("ls");
        println!("ls exists: {}", ls_exists);
        assert!(ls_exists, "ls should exist in PATH");

        // Try cargo if it exists in the outer environment
        if let Ok(cargo_path) = std::process::Command::new("which").arg("cargo").output() {
            if cargo_path.status.success() {
                let cargo_exists = command_exists("cargo");
                println!("cargo exists: {}", cargo_exists);
                assert!(
                    cargo_exists,
                    "cargo should exist in PATH if it exists in environment"
                );
            }
        }
    }
}
