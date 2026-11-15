use crate::params::{
    Param, Value, DEFAULT_TIES, EXPORT, FLOAT_E, FLOAT_F, INTEGER, LEFT, READONLY, RIGHT_B,
    RIGHT_Z, UNIQUE, UNSET,
};
use crate::types::{Shell, OPT_ALLEXPORT};
use std::collections::{BTreeMap, HashSet};
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
            let mut p = Param::scalar(value);
            p.attrs = EXPORT;
            shell.params.insert(key.to_string(), p);
        }
    }

    let set_default = |shell: &mut Shell, name: &str, value: String, attrs: u32| {
        shell.params.entry(name.to_string()).or_insert_with(|| {
            let mut p = Param::scalar(value);
            p.attrs = attrs;
            p
        });
    };
    let set = |shell: &mut Shell, name: &str, value: String, attrs: u32| {
        let mut p = Param::scalar(value);
        p.attrs = attrs;
        shell.params.insert(name.to_string(), p);
    };
    set_default(
        &mut shell,
        "PATH",
        "/usr/local/bin:/usr/bin:/bin".to_string(),
        EXPORT,
    );
    set_default(&mut shell, "PS2", "> ".to_string(), 0);
    set_default(&mut shell, "HISTSIZE", "1000".to_string(), 0);
    set_default(&mut shell, "SAVEHIST", "1000".to_string(), 0);
    set_default(&mut shell, "PS4", "+ ".to_string(), 0);
    // zsh frameworks such as oh-my-zsh refuse to load without
    // ZSH_VERSION. The suffix keeps per-version files (for example
    // ~/.zcompdump-host-$ZSH_VERSION) apart from real zsh's.
    set(&mut shell, "ZSH_VERSION", "5.9-meowsh".to_string(), 0);
    set(
        &mut shell,
        "MEOWSH_VERSION",
        env!("CARGO_PKG_VERSION").to_string(),
        0,
    );
    set(&mut shell, "HOST", hostname_full(), 0);
    // POSIX: IFS is reset at startup rather than inherited.
    set(&mut shell, "IFS", " \t\n".to_string(), 0);
    let ppid = unsafe { libc::getppid() };
    set(&mut shell, "PPID", ppid.to_string(), 0);
    let shlvl = shell
        .scalar("SHLVL")
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(0)
        .max(0)
        + 1;
    set(&mut shell, "SHLVL", shlvl.to_string(), EXPORT);
    if let Some(pwd) = initial_pwd(shell.scalar("PWD")) {
        set(&mut shell, "PWD", pwd, EXPORT);
    }

    if let Some(home) = shell.scalar("HOME").filter(|h| !h.is_empty()) {
        shell.history_file = PathBuf::from(home)
            .join(".meowsh_history")
            .to_string_lossy()
            .to_string();
    }

    START.get_or_init(std::time::Instant::now);
    crate::options::init(&mut shell, crate::options::Emulation::Zsh);
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
        .assoc_entry("langinfo")
        .insert("CODESET".to_string(), codeset.to_string());
    for (scalar, array) in DEFAULT_TIES {
        if shell.has_param(scalar) || matches!(*scalar, "PATH" | "FPATH") {
            shell.tie(scalar, array, ":");
        }
    }
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
        s.params
            .iter()
            .filter(|(_, p)| p.attrs & EXPORT != 0)
            .filter_map(|(k, p)| match &p.value {
                Value::Scalar(v) => Some((k.clone(), v.clone())),
                _ => None,
            })
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

/// The parameter `name` refers to, following namerefs.
pub fn resolve_ref(name: &str) -> String {
    with_shell(|s| s.resolve_name(name))
}

// Resolves a nameref target that carries a subscript: (name, sub).
fn split_target(t: &str) -> (String, Option<String>) {
    match t.find('[') {
        Some(k) if t.ends_with(']') => {
            (t[..k].to_string(), Some(t[k + 1..t.len() - 1].to_string()))
        }
        _ => (t.to_string(), None),
    }
}

pub fn var_get(name: &str) -> String {
    if let Some(v) = dynamic_var(name) {
        return v;
    }
    let resolved = resolve_ref(name);
    if resolved != name {
        let (base, sub) = split_target(&resolved);
        return match sub {
            Some(sub) => with_shell(|s| match s.param(&base).map(|p| p.value.clone()) {
                Some(Value::Assoc(m)) => m.get(&sub).cloned().unwrap_or_default(),
                Some(Value::Array(a)) => sub
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| a.get(i).cloned())
                    .unwrap_or_default(),
                Some(Value::Scalar(v)) => v,
                None => String::new(),
            }),
            None => var_get(&base),
        };
    }
    let shell = SHELL.shell.lock().unwrap();
    match name {
        "$" => return format!("{}", shell.shell_pid),
        "?" => return format!("{}", shell.last_status),
        "!" => return format!("{}", shell.last_bg_pid),
        "0" => return shell.argv0.clone(),
        "#" => return format!("{}", shell.pos_params.len()),
        "@" | "*" => return shell.pos_params.join(" "),
        "-" => return crate::options::flags_string(&shell),
        _ => {}
    }

    if name.len() > 1 && name.starts_with('-') {
        drop(shell);
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
    shell.param(name).map(|p| p.text()).unwrap_or_default()
}

pub fn var_exists(name: &str) -> bool {
    let resolved = resolve_ref(name);
    if resolved != name {
        return var_exists(&split_target(&resolved).0);
    }
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
    shell.param(name).is_some_and(|p| p.is_set())
}

pub fn var_get_if_exists(name: &str) -> Option<i64> {
    let shell = SHELL.shell.lock().unwrap();
    shell.scalar(name).and_then(|v| v.parse::<i64>().ok())
}

fn readonly_error(name: &str) {
    shell_err(&format!("read-only variable: {}", name));
}

/// Converts a value for a numeric parameter at assignment: integers
/// and floats evaluate it arithmetically and keep the number. Other
/// attributes (case, justification, base) apply when the value is read.
/// Done without the shell lock held, since arithmetic reads variables.
fn convert_for(attrs: u32, value: &str) -> Result<String, String> {
    if attrs & INTEGER != 0 {
        return Ok(crate::arith::eval(value)?.to_string());
    }
    if attrs & (FLOAT_E | FLOAT_F) != 0 {
        let f = crate::arith::eval_num(value)?.as_float_pub();
        return Ok(format!("{:?}", f));
    }
    Ok(value.to_string())
}

pub fn format_int(n: i64, base: u32) -> String {
    if base == 0 || base == 10 {
        return n.to_string();
    }
    if crate::options::quick("cbases")
        && (base == 16 || (base == 8 && crate::options::quick("octalzeroes")))
    {
        let digits = if base == 16 {
            format!("0x{:X}", n.unsigned_abs())
        } else {
            format!("0{:o}", n.unsigned_abs())
        };
        return format!("{}{}", if n < 0 { "-" } else { "" }, digits);
    }
    let neg = n < 0;
    let mut u = n.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        digits.push(
            std::char::from_digit((u % base as u64) as u32, base)
                .unwrap()
                .to_ascii_uppercase(),
        );
        u /= base as u64;
        if u == 0 {
            break;
        }
    }
    digits.reverse();
    format!(
        "{}{}#{}",
        if neg { "-" } else { "" },
        base,
        digits.into_iter().collect::<String>()
    )
}

pub fn format_float(f: f64, attrs: u32, digits: u32) -> String {
    let d = if digits == 0 { 10 } else { digits as usize };
    if attrs & FLOAT_F != 0 {
        format!("{:.*}", d, f)
    } else {
        // C's %.*e: two-digit exponent with a sign.
        let s = format!("{:.*e}", d.saturating_sub(1), f);
        match s.split_once('e') {
            Some((m, e)) => {
                let (sign, digits) = match e.strip_prefix('-') {
                    Some(d) => ('-', d),
                    None => ('+', e),
                };
                format!("{}e{}{:0>2}", m, sign, digits)
            }
            None => s,
        }
    }
}

/// zsh's -L/-R/-Z display (getstrvalue in Src/params.c). `width` is the
/// field width; numeric parameters keep a leading `-`, `0x` or `base#`
/// before zero padding.
pub fn justify(v: &str, attrs: u32, width: usize) -> String {
    let chars: Vec<char> = v.chars().collect();
    let fwidth = if width > 0 { width } else { chars.len() };
    if attrs & LEFT != 0 {
        let mut t = 0;
        if attrs & RIGHT_Z != 0 {
            while chars.get(t) == Some(&'0') {
                t += 1;
            }
        } else {
            while chars.get(t).is_some_and(|c| *c == ' ' || *c == '\t') {
                t += 1;
            }
        }
        let mut out: String = chars[t..].iter().take(fwidth).collect();
        let n = out.chars().count();
        out.extend(std::iter::repeat_n(' ', fwidth - n));
        return out;
    }
    if chars.len() >= fwidth {
        return chars[chars.len() - fwidth..].iter().collect();
    }
    let numeric = attrs & (INTEGER | FLOAT_E | FLOAT_F) != 0;
    let mut zero = true;
    let mut pref = 0;
    if attrs & RIGHT_Z != 0 {
        let mut t = chars
            .iter()
            .take_while(|c| **c == ' ' || **c == '\t')
            .count();
        if numeric && chars.get(t) == Some(&'-') {
            t += 1;
        }
        if attrs & INTEGER != 0 {
            if crate::options::quick("cbases")
                && chars.get(t) == Some(&'0')
                && chars.get(t + 1) == Some(&'x')
            {
                t += 2;
            } else if let Some(k) = chars[t..].iter().position(|c| *c == '#') {
                t += k + 1;
            }
        }
        pref = t;
        if t >= chars.len() || (!numeric && !chars[t].is_ascii_digit()) {
            zero = false;
        }
    }
    let fill = if attrs & RIGHT_B != 0 || !zero {
        ' '
    } else {
        '0'
    };
    let mut out: String = chars[..pref].iter().collect();
    out.extend(std::iter::repeat_n(fill, fwidth - chars.len()));
    out.extend(&chars[pref..]);
    out
}

/// Points nameref `name` at `target`, rejecting bad names and loops.
pub fn set_ref_target(name: &str, target: &str) -> bool {
    let (base, sub) = split_target(target);
    let valid = !target.is_empty()
        && (crate::lexer::is_valid_name(&base) || base.chars().all(|c| c.is_ascii_digit()))
        && sub
            .as_ref()
            .is_none_or(|_| target.ends_with(']') && target.matches('[').count() == 1);
    if !target.is_empty() && !valid {
        shell_err(&format!("invalid variable name: {}", target));
        return false;
    }
    let looping = with_shell(|s| {
        let mut cur = base.clone();
        for _ in 0..64 {
            if cur == name {
                return true;
            }
            match s.param(&cur) {
                Some(p) if p.attrs & crate::params::NAMEREF != 0 => match &p.value {
                    Value::Scalar(t) if !t.is_empty() => cur = split_target(t).0,
                    _ => return false,
                },
                _ => return false,
            }
        }
        true
    });
    if looping {
        shell_err(&format!("{}: invalid self reference", name));
        return false;
    }
    with_shell(|s| {
        if let Some(p) = s.param_mut(name) {
            p.value = Value::Scalar(target.to_string());
            p.attrs &= !UNSET;
        }
    });
    true
}

pub fn var_set(name: &str, value: &str, export: bool) -> bool {
    // A nameref without a target yet: the value becomes its target.
    let placeholder = with_shell(|s| {
        s.param(name).is_some_and(|p| {
            p.attrs & crate::params::NAMEREF != 0
                && matches!(&p.value, Value::Scalar(t) if t.is_empty())
        })
    });
    if placeholder {
        return set_ref_target(name, value);
    }
    let resolved = resolve_ref(name);
    if resolved != name {
        let (base, sub) = split_target(&resolved);
        return match sub {
            Some(sub) => array_set_subscript(&base, &sub, value.to_string(), false),
            None => var_set(&base, value, export),
        };
    }
    if name == "RANDOM" {
        seed_random(value.trim().parse().unwrap_or(0));
    }
    let attrs = with_shell(|s| s.param(name).map(|p| p.attrs).unwrap_or(0));
    if attrs & READONLY != 0 {
        readonly_error(name);
        return false;
    }
    let value = match convert_for(attrs, value) {
        Ok(v) => v,
        Err(e) => {
            shell_err(&format!("{}: {}", name, e));
            return false;
        }
    };
    let mut shell = SHELL.shell.lock().unwrap();
    let allexport = shell.opts & OPT_ALLEXPORT != 0;
    let p = shell
        .params
        .entry(name.to_string())
        .or_insert_with(|| Param::scalar(""));
    if export || allexport {
        p.attrs |= EXPORT;
    }
    p.attrs &= !UNSET;
    if p.width == 0 && p.attrs & (LEFT | RIGHT_B | RIGHT_Z) != 0 {
        p.width = value.chars().count();
    }
    p.value = Value::Scalar(value);
    shell.sync_tie(name);
    true
}

/// Stores items into array `name`, honoring -U and readonly.
pub fn array_set(name: &str, items: Vec<String>) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if shell.assoc(name).is_some() {
        let assoc = assoc_from_items(items);
        shell.store(name, Value::Assoc(assoc));
        return true;
    }
    // A scalar given an array value becomes a plain array: its scalar
    // attributes (-i, -Z3, -u, -x...) go.
    if let Some(p) = shell.param_mut(name) {
        if matches!(p.value, Value::Scalar(_)) && p.tie.is_none() {
            p.attrs &= crate::params::SPECIAL | crate::params::HIDE | crate::params::DECLARED;
            p.width = 0;
            p.base = 0;
        }
    }
    let items = maybe_unique_items(&shell, name, items);
    shell.store(name, Value::Array(items));
    true
}

/// An item of an array assignment: a plain value or zsh's `[key]=value`
/// (the bool: `[key]+=value`).
#[derive(Clone, Debug)]
pub enum ArrItem {
    Plain(String),
    Key(String, String, bool),
}

pub fn plain_items(items: &[ArrItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| match i {
            ArrItem::Plain(v) => v.clone(),
            ArrItem::Key(k, v, _) => format!("[{}]={}", k, v),
        })
        .collect()
}

/// `name=(items)` / `name+=(items)` with `[key]=value` items: for an
/// associative array each sets a key (plain items pair up); for a normal
/// array `[n]=v` sets element n and later plain items follow it.
pub fn array_assign_items(name: &str, items: Vec<ArrItem>, append: bool) -> bool {
    if !items.iter().any(|i| matches!(i, ArrItem::Key(..))) {
        let plain = plain_items(&items);
        return if append {
            array_append(name, plain)
        } else {
            array_set(name, plain)
        };
    }
    let target = split_target(&resolve_ref(name)).0;
    let is_assoc = with_shell(|s| s.assoc(&target).is_some());
    if is_assoc {
        if items.iter().any(|i| matches!(i, ArrItem::Plain(_))) {
            shell_err("bad [key]=value syntax for associative array");
            return false;
        }
        let mut map = if append {
            with_shell(|s| s.assoc(&target).cloned()).unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        for i in items {
            if let ArrItem::Key(k, v, add) = i {
                let v = if add {
                    format!("{}{}", map.get(&k).cloned().unwrap_or_default(), v)
                } else {
                    v
                };
                map.insert(k, v);
            }
        }
        with_shell(|s| s.store(&target, Value::Assoc(map)));
        return true;
    }
    let mut arr = if append {
        array_get(&target).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut idx = arr.len();
    for i in items {
        match i {
            ArrItem::Plain(v) => {
                if arr.len() <= idx {
                    arr.resize(idx + 1, String::new());
                }
                arr[idx] = v;
                idx += 1;
            }
            ArrItem::Key(k, v, add) => {
                let mut n = crate::arith::eval(&k).unwrap_or(0);
                if n >= 0 && crate::options::quick("ksharrays") {
                    n += 1;
                }
                if n < 1 {
                    shell_err(&format!("{}: assignment to invalid subscript range", name));
                    return false;
                }
                let k = (n - 1) as usize;
                if arr.len() <= k {
                    arr.resize(k + 1, String::new());
                }
                if add {
                    arr[k].push_str(&v);
                } else {
                    arr[k] = v;
                }
                idx = k + 1;
            }
        }
    }
    array_set(&target, arr)
}

/// `name[a,b]=(items)`: replaces elements a through b (1-based,
/// negatives from the end) with `items`. `name[n]=(items)` replaces one.
pub fn array_splice(name: &str, sub: &str, items: Vec<String>) -> bool {
    let target = split_target(&resolve_ref(name)).0;
    let (a, b) = match sub.split_once(',') {
        Some((a, b)) => (crate::arith::eval(a), crate::arith::eval(b)),
        None => (crate::arith::eval(sub), crate::arith::eval(sub)),
    };
    let (Ok(mut a), Ok(mut b)) = (a, b) else {
        shell_err(&format!("{}: bad subscript", name));
        return false;
    };
    if crate::options::quick("ksharrays") {
        a += i64::from(a >= 0);
        b += i64::from(b >= 0);
    }
    let scalar = with_shell(|s| s.scalar(&target).map(|v| v.to_string()));
    let mut arr: Vec<String> = match &scalar {
        Some(v) => v.chars().map(|c| c.to_string()).collect(),
        None => array_get(&target).unwrap_or_default(),
    };
    let len = arr.len() as i64;
    let norm = |x: i64| if x < 0 { (len + x + 1).max(0) } else { x };
    let start = (norm(a).max(1) - 1) as usize;
    let end = (norm(b).max(start as i64)) as usize;
    if arr.len() < start {
        arr.resize(start, String::new());
    }
    let end = end.min(arr.len());
    arr.splice(start..end, items);
    match scalar {
        Some(_) => var_set(&target, &arr.concat(), false),
        None => array_set(&target, arr),
    }
}

pub fn array_append(name: &str, items: Vec<String>) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if let Some(existing) = shell.assoc(name).cloned() {
        let mut assoc = existing;
        assoc.extend(assoc_from_items(items));
        shell.store(name, Value::Assoc(assoc));
        return true;
    }
    let mut combined = match shell.param(name).map(|p| &p.value) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Scalar(s)) => vec![s.clone()],
        _ => Vec::new(),
    };
    combined.extend(items);
    let combined = maybe_unique_items(&shell, name, combined);
    shell.store(name, Value::Array(combined));
    true
}

pub fn array_set_subscript(name: &str, subscript: &str, value: String, append: bool) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if let Some(assoc) = shell.assoc_mut(name) {
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

    let Ok(mut index) = subscript.parse::<isize>() else {
        return false;
    };
    if index >= 0 && crate::options::quick("ksharrays") {
        index += 1;
    }
    if index == 0 && crate::options::quick("kshzerosubscript") {
        index = 1;
    }
    if index == 0 {
        drop(shell);
        shell_err(&format!("{}: assignment to invalid subscript range", name));
        return false;
    }
    let mut items = shell.array(name).cloned().unwrap_or_default();
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
    shell.store(name, Value::Array(items));
    true
}

pub fn array_make_unique(name: &str) -> bool {
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    let p = shell
        .params
        .entry(name.to_string())
        .or_insert_with(|| Param::with(Value::Array(Vec::new())));
    p.attrs |= UNIQUE;
    if let Some(items) = shell.array(name).cloned() {
        shell.store(name, Value::Array(unique_items(items)));
    }
    true
}

pub fn array_declare(name: &str) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    let items = match shell.param(name).map(|p| &p.value) {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Scalar(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    };
    shell.store(name, Value::Array(items));
    true
}

pub fn assoc_declare(name: &str) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    if !prepare_array_mutation(&mut shell, name) {
        return false;
    }
    if shell.assoc(name).is_none() {
        shell.store(name, Value::Assoc(BTreeMap::new()));
    }
    true
}

pub fn assoc_set(name: &str, key: &str, value: &str) {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let mut shell = SHELL.shell.lock().unwrap();
    shell
        .assoc_entry(name)
        .insert(key.to_string(), value.to_string());
}

pub fn assoc_get(name: &str, key: &str) -> Option<String> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let shell = SHELL.shell.lock().unwrap();
    shell.assoc(name).and_then(|m| m.get(key).cloned())
}

pub fn assoc_values(name: &str) -> Option<Vec<String>> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let shell = SHELL.shell.lock().unwrap();
    shell.assoc(name).map(|m| m.values().cloned().collect())
}

pub fn assoc_keys(name: &str) -> Option<Vec<String>> {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let shell = SHELL.shell.lock().unwrap();
    shell.assoc(name).map(|m| m.keys().cloned().collect())
}

fn prepare_array_mutation(shell: &mut Shell, name: &str) -> bool {
    if shell.param(name).is_some_and(|p| p.attrs & READONLY != 0) {
        readonly_error(name);
        return false;
    }
    true
}

fn maybe_unique_items(shell: &Shell, name: &str, items: Vec<String>) -> Vec<String> {
    if shell.param(name).is_some_and(|p| p.attrs & UNIQUE != 0) {
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
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    SHELL.shell.lock().unwrap().array(name).cloned()
}

pub fn is_array(name: &str) -> bool {
    let resolved = split_target(&resolve_ref(name)).0;
    let name = resolved.as_str();
    let shell = SHELL.shell.lock().unwrap();
    shell
        .param(name)
        .is_some_and(|p| p.is_array() || p.is_assoc())
}

pub fn push_scope() {
    SHELL.shell.lock().unwrap().push_scope();
}

pub fn pop_scope() {
    SHELL.shell.lock().unwrap().pop_scope();
}

pub fn in_function_scope() -> bool {
    !SHELL.shell.lock().unwrap().local_scopes.is_empty()
}

/// Saves `name` in the innermost scope so it is restored when the
/// function returns. False outside a function. Only the first call per
/// scope records the prior state.
pub fn declare_local(name: &str) -> bool {
    SHELL.shell.lock().unwrap().make_local(name)
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
        // %Nv: element N of $psvar.
        if chars.peek() == Some(&'v') {
            chars.next();
            let n: usize = digits.parse().unwrap_or(1).max(1);
            let v = with_shell(|s| s.array("psvar").and_then(|a| a.get(n - 1).cloned()));
            out.push_str(&v.unwrap_or_default());
            continue;
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

/// Where errors say they happened, kept apart from the shell state so an
/// error can be reported while the shell is locked.
pub struct ErrCtx {
    // (name, line the numbering starts after) for each function,
    // `(eval)` or sourced file being run.
    pub stack: Vec<(String, usize)>,
    // Line of the command running, in its input.
    pub lineno: usize,
    // The script, or the shell's name for `-c` and standard input.
    pub name: String,
    pub interactive: bool,
}

static ERR_CTX: std::sync::Mutex<ErrCtx> = std::sync::Mutex::new(ErrCtx {
    stack: Vec::new(),
    lineno: 0,
    name: String::new(),
    interactive: false,
});

pub fn with_err_ctx<R>(f: impl FnOnce(&mut ErrCtx) -> R) -> R {
    let mut guard = ERR_CTX.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// zsh's prefix for an error message: `name:line: ` (or
/// `name:builtin:line: `), where name is the function, `(eval)`, sourced
/// file or script running. Interactive commands typed at the prompt get
/// just `name: ` / `builtin: `.
pub fn err_prefix(builtin: Option<&str>) -> String {
    with_err_ctx(|e| {
        if e.interactive && e.stack.is_empty() {
            return match builtin {
                Some(b) => format!("{}: ", b),
                None => format!("{}: ", e.name),
            };
        }
        let (name, base) = e
            .stack
            .last()
            .cloned()
            .unwrap_or_else(|| (e.name.clone(), 0));
        let line = e.lineno.saturating_sub(base);
        let line = if line > 0 {
            format!(":{}", line)
        } else {
            String::new()
        };
        match builtin {
            Some(b) => format!("{}:{}{}: ", name, b, line),
            None => format!("{}{}: ", name, line),
        }
    })
}

/// Prints a shell error in zsh's format.
pub fn shell_err(msg: &str) {
    crate::sys::err_line(&format!("{}{}", err_prefix(None), msg));
}

/// Prints an error from builtin `name` in zsh's format.
pub fn builtin_err(name: &str, msg: &str) {
    crate::sys::err_line(&format!("{}{}", err_prefix(Some(name)), msg));
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
