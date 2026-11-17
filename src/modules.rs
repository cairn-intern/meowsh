//! zsh's module bookkeeping and the `zmodload` builtin (Src/module.c).
//!
//! meowsh links every module's code in, so a module here is a static
//! description: its name and its features (`b:` builtins, `c:`/`C:`
//! conditions, `f:` math functions, `p:` parameters, in the order of the
//! module's tables). "Loading" a module turns its features on. The shell
//! asks `feature_enabled` before running a module builtin, reading a
//! module parameter, or calling a module condition or math function, and
//! `autoload_feature` when the feature is off but may be autoloaded.

use crate::shell::{builtin_err, shell_err};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};

struct ModDesc {
    name: &'static str,
    // None: the module does not support features (zsh/main).
    features: Option<&'static [&'static str]>,
}

const PARAMETER_FEATURES: &[&str] = &[
    "p:aliases",
    "p:builtins",
    "p:commands",
    "p:dirstack",
    "p:dis_aliases",
    "p:dis_builtins",
    "p:dis_functions",
    "p:dis_functions_source",
    "p:dis_galiases",
    "p:dis_patchars",
    "p:dis_reswords",
    "p:dis_saliases",
    "p:funcfiletrace",
    "p:funcsourcetrace",
    "p:funcstack",
    "p:functions",
    "p:functions_source",
    "p:functrace",
    "p:galiases",
    "p:history",
    "p:historywords",
    "p:jobdirs",
    "p:jobstates",
    "p:jobtexts",
    "p:modules",
    "p:nameddirs",
    "p:options",
    "p:parameters",
    "p:patchars",
    "p:reswords",
    "p:saliases",
    "p:userdirs",
    "p:usergroups",
];

const MATHFUNC_FEATURES: &[&str] = &[
    "f:abs",
    "f:acos",
    "f:acosh",
    "f:asin",
    "f:asinh",
    "f:atan",
    "f:atanh",
    "f:cbrt",
    "f:ceil",
    "f:copysign",
    "f:cos",
    "f:cosh",
    "f:erf",
    "f:erfc",
    "f:exp",
    "f:expm1",
    "f:fabs",
    "f:float",
    "f:floor",
    "f:fmod",
    "f:gamma",
    "f:hypot",
    "f:ilogb",
    "f:int",
    "f:isinf",
    "f:isnan",
    "f:j0",
    "f:j1",
    "f:jn",
    "f:ldexp",
    "f:lgamma",
    "f:log",
    "f:log10",
    "f:log1p",
    "f:log2",
    "f:logb",
    "f:nextafter",
    "f:rand48",
    "f:rint",
    "f:scalb",
    "f:signgam",
    "f:sin",
    "f:sinh",
    "f:sqrt",
    "f:tan",
    "f:tanh",
    "f:y0",
    "f:y1",
    "f:yn",
];

// The modules meowsh provides. Any other name fails to load, as in a zsh
// built without it.
const DESCS: &[ModDesc] = &[
    ModDesc {
        name: "zsh/main",
        features: None,
    },
    ModDesc {
        name: "zsh/complete",
        features: Some(&[
            "b:compadd",
            "b:compset",
            "c:after",
            "c:between",
            "c:prefix",
            "c:suffix",
        ]),
    },
    ModDesc {
        name: "zsh/complist",
        features: Some(&[]),
    },
    ModDesc {
        name: "zsh/computil",
        features: Some(&[
            "b:comparguments",
            "b:compdescribe",
            "b:compfiles",
            "b:compgroups",
            "b:compquote",
            "b:comptags",
            "b:comptry",
            "b:compvalues",
        ]),
    },
    ModDesc {
        name: "zsh/datetime",
        features: Some(&[
            "b:strftime",
            "p:EPOCHSECONDS",
            "p:EPOCHREALTIME",
            "p:epochtime",
        ]),
    },
    ModDesc {
        name: "zsh/langinfo",
        features: Some(&["p:langinfo"]),
    },
    ModDesc {
        name: "zsh/mathfunc",
        features: Some(MATHFUNC_FEATURES),
    },
    // Colour approximation: refresh maps colours itself once it is loaded.
    ModDesc {
        name: "zsh/nearcolor",
        features: Some(&[]),
    },
    ModDesc {
        name: "zsh/param/private",
        features: Some(&["b:private"]),
    },
    ModDesc {
        name: "zsh/parameter",
        features: Some(PARAMETER_FEATURES),
    },
    ModDesc {
        name: "zsh/random",
        features: Some(&["f:zrand_float", "f:zrand_int", "p:SRANDOM"]),
    },
    ModDesc {
        name: "zsh/regex",
        features: Some(&["C:regex-match"]),
    },
    ModDesc {
        name: "zsh/rlimits",
        features: Some(&["b:limit", "b:ulimit", "b:unlimit"]),
    },
    ModDesc {
        name: "zsh/system",
        features: Some(&[
            "b:syserror",
            "b:sysread",
            "b:syswrite",
            "b:sysopen",
            "b:sysseek",
            "b:zsystem",
            "f:systell",
            "p:errnos",
            "p:sysparams",
        ]),
    },
    ModDesc {
        name: "zsh/zle",
        features: Some(&["b:bindkey", "b:vared", "b:zle"]),
    },
    ModDesc {
        name: "zsh/zleparameter",
        features: Some(&["p:keymaps", "p:widgets"]),
    },
    ModDesc {
        name: "zsh/zpty",
        features: Some(&["b:zpty"]),
    },
    ModDesc {
        name: "zsh/zselect",
        features: Some(&["b:zselect"]),
    },
    ModDesc {
        name: "zsh/zutil",
        features: Some(&["b:zformat", "b:zparseopts", "b:zregexparse", "b:zstyle"]),
    },
];

// What zsh's generated bltinmods.list registers at startup for the
// load=yes modules (their .mdd `autofeatures` and `moddeps`).
const STARTUP: &[(&str, &[&str], &[&str])] = &[
    ("zsh/rlimits", &["b:limit", "b:ulimit", "b:unlimit"], &[]),
    ("zsh/parameter", PARAMETER_FEATURES, &[]),
    (
        "zsh/random",
        &["p:SRANDOM", "f:zrand_float", "f:zrand_int"],
        &[],
    ),
    (
        "zsh/zutil",
        &["b:zformat", "b:zstyle", "b:zregexparse", "b:zparseopts"],
        &["zsh/complete"],
    ),
    (
        "zsh/complete",
        &[
            "b:compadd",
            "b:compset",
            "c:prefix",
            "c:suffix",
            "c:between",
            "c:after",
        ],
        &["zsh/zle"],
    ),
    ("zsh/complist", &[], &["zsh/complete", "zsh/zle"]),
    (
        "zsh/computil",
        &[
            "b:compdescribe",
            "b:comparguments",
            "b:compvalues",
            "b:compquote",
            "b:comptags",
            "b:comptry",
            "b:compfiles",
            "b:compgroups",
        ],
        &["zsh/complete", "zsh/zle"],
    ),
    ("zsh/zle", &["b:bindkey", "b:vared", "b:zle"], &[]),
    (
        "zsh/zleparameter",
        &["p:widgets", "p:keymaps"],
        &["zsh/zle"],
    ),
];

// zsh -f has zsh/main loaded; Test/ztst.zsh also loads zsh/parameter.
const LOADED_AT_STARTUP: &[&str] = &["zsh/main", "zsh/parameter"];

const FEAT_IGNORE: u32 = 0x01;
const FEAT_INFIX: u32 = 0x02;
const FEAT_AUTOALL: u32 = 0x04;
const FEAT_REMOVE: u32 = 0x08;
const FEAT_CHECKAUTO: u32 = 0x10;

const PRINTMOD_LIST: u32 = 0x01;
const PRINTMOD_EXIST: u32 = 0x02;
const PRINTMOD_ALIAS: u32 = 0x04;
const PRINTMOD_DEPS: u32 = 0x08;
const PRINTMOD_FEATURES: u32 = 0x10;
const PRINTMOD_LISTALL: u32 = 0x20;
const PRINTMOD_AUTO: u32 = 0x40;

#[derive(Default)]
struct Module {
    alias: Option<String>,
    loaded: bool,
    // Loading its dependencies (circular dependency guard).
    busy: bool,
    deps: Option<Vec<String>>,
    // Features to autoload, in lexical order.
    autoloads: Option<Vec<String>>,
}

#[derive(Clone)]
struct AutoBin {
    module: String,
    autoall: bool,
}

#[derive(Clone)]
struct AutoCond {
    infix: bool,
    name: String,
    module: String,
    autoall: bool,
}

#[derive(Clone)]
struct AutoMath {
    name: String,
    module: String,
    autoall: bool,
}

enum Msg {
    Out(String),
    // zwarn (None) or zwarnnam (Some(command name)).
    Warn(Option<String>, String),
}

#[derive(PartialEq)]
enum ParamState {
    Absent,
    Local,
    Global,
}

struct FeatEnable {
    // The feature, maybe with a leading `+` or `-`.
    s: String,
    // `s` (without its sign) is a pattern (zmodload -m).
    pat: bool,
}

impl FeatEnable {
    fn matches(&self, feature: &str) -> bool {
        feat_match(strip_pm(&self.s), feature, self.pat)
    }
}

struct State {
    modules: BTreeMap<String, Module>,
    // Per DESCS entry, whether each feature is on.
    enables: Vec<Vec<bool>>,
    // Autoload stubs, as zsh keeps them in its builtin, condition,
    // parameter and math function tables. Conditions and math functions
    // are lists with the newest first, as zsh's are.
    autobins: BTreeMap<String, AutoBin>,
    autoconds: Vec<AutoCond>,
    autoparams: BTreeMap<String, String>,
    automath: Vec<AutoMath>,
    // Builtins outside modules among the current command's arguments,
    // computed before the state is borrowed.
    core_builtins: HashSet<String>,
    msgs: Vec<Msg>,
    pending_array: Option<(String, Vec<String>)>,
    newly_loaded: Vec<String>,
    // Modules zmodload has been given by name. Until then a module's
    // features stay usable, so scripts written for meowsh that never load
    // zsh/datetime and friends keep working.
    managed: HashSet<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::new());
}

// Nothing called while the state is borrowed may call back into this
// module: messages and parameter assignments wait until it is released.
fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

fn flush(msgs: Vec<Msg>) {
    for m in msgs {
        match m {
            Msg::Out(s) => {
                let _ = crate::sys::out(&s);
            }
            Msg::Warn(None, s) => shell_err(&s),
            Msg::Warn(Some(n), s) => builtin_err(&n, &s),
        }
    }
}

fn desc_idx(name: &str) -> Option<usize> {
    DESCS.iter().position(|d| d.name == name)
}

fn strip_pm(s: &str) -> &str {
    s.strip_prefix(['+', '-']).unwrap_or(s)
}

// (sense, feature) for a `+feature`/`-feature`/`feature` argument.
fn strip_sense(s: &str) -> (bool, &str) {
    match s.strip_prefix('-') {
        Some(r) => (false, r),
        None => (true, s.strip_prefix('+').unwrap_or(s)),
    }
}

fn feat_match(arg: &str, feature: &str, pat: bool) -> bool {
    if pat {
        crate::pattern::Pattern::new(arg).matches(feature)
    } else {
        arg == feature
    }
}

fn quote(s: &str) -> String {
    crate::builtins::shell_quote(s)
}

// A module name may hold only identifier characters and slashes.
fn modname_ok(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '/')
}

/// True if `feature` (e.g. `b:strftime`) belongs to one of the modules
/// meowsh provides.
pub fn is_module_feature(feature: &str) -> bool {
    DESCS
        .iter()
        .any(|d| d.features.is_some_and(|f| f.contains(&feature)))
}

/// True if `feature` (e.g. `b:strftime`, `p:EPOCHSECONDS`) is provided
/// right now: its module is loaded and the feature is enabled.
pub fn feature_enabled(feature: &str) -> bool {
    with_state(|st| st.feature_on(feature))
}

/// Whether `feature` can be used: enabled, or in a module that zmodload
/// has not been told about (meowsh builds every module in).
pub fn feature_available(feature: &str) -> bool {
    with_state(|st| {
        st.feature_on(feature)
            || DESCS
                .iter()
                .find(|d| d.features.is_some_and(|f| f.contains(&feature)))
                .is_none_or(|d| !st.managed.contains(d.name))
    })
}

/// Autoloads `feature` (`b:name`, `p:name`, `f:name`, `c:name` or
/// `C:name`) if an autoload is registered for it. None: there is none.
/// Some(Err(msg)): the feature is still not defined; print a non-empty
/// msg with shell_err (zsh's zerr). An empty msg means print nothing:
/// for conditions the caller reports its usual "unknown condition", for
/// a builtin whose module failed to load the command just fails.
/// Must not be called with the shell lock held.
/// Whether `name` has a parameter autoload waiting (zmodload -ap).
pub fn has_autoparam(name: &str) -> bool {
    with_state(|st| st.autoparams.contains_key(name))
}

pub fn autoload_feature(feature: &str) -> Option<Result<(), String>> {
    let (kind, name) = match feature.as_bytes() {
        [k, b':', ..] if k.is_ascii() => (*k as char, &feature[2..]),
        _ => return None,
    };
    let (r, msgs) = with_state(|st| {
        let r = st.autoload(kind, name);
        (r, std::mem::take(&mut st.msgs))
    });
    flush(msgs);
    r
}

/// Modules that finished loading since the last call, for module setup
/// code (e.g. zsh/complist) to run.
pub fn take_newly_loaded() -> Vec<String> {
    with_state(|st| std::mem::take(&mut st.newly_loaded))
}

/// The entries of zsh/parameter's `$modules`: name to `loaded`,
/// `autoloaded` or `alias:target`.
pub fn modules_param() -> BTreeMap<String, String> {
    with_state(|st| {
        let mut out = BTreeMap::new();
        for (name, m) in &st.modules {
            if let Some(a) = &m.alias {
                out.insert(name.clone(), format!("alias:{}", a));
            } else if m.loaded {
                out.insert(name.clone(), "loaded".to_string());
            } else if m.autoloads.as_ref().is_some_and(|a| !a.is_empty()) {
                out.insert(name.clone(), "autoloaded".to_string());
            }
        }
        let stubs = st
            .autobins
            .values()
            .map(|b| &b.module)
            .chain(st.autoconds.iter().map(|c| &c.module))
            .chain(st.autoparams.values())
            .chain(st.automath.iter().map(|f| &f.module));
        for m in stubs {
            out.entry(m.clone())
                .or_insert_with(|| "autoloaded".to_string());
        }
        out
    })
}

// Builtins not provided by a module among `args` (bare names or `b:`
// features), which autoloads can neither replace nor remove.
fn core_builtin_names(args: &[String]) -> HashSet<String> {
    let mut set = HashSet::new();
    for a in args {
        let a = strip_pm(a);
        for c in [a, a.strip_prefix("b:").unwrap_or(a)] {
            if !is_module_feature(&format!("b:{}", c)) && crate::builtins::is_builtin(c) {
                set.insert(c.to_string());
            }
        }
    }
    set
}

struct Ops {
    set: HashSet<char>,
    p_arg: Option<String>,
}

impl Ops {
    fn is(&self, c: char) -> bool {
        self.set.contains(&c)
    }
}

// zsh's generic builtin option parsing for optstr "AFRILP:abcfdilmpsue".
fn parse_ops(words: &[String]) -> Result<(Ops, usize), String> {
    const OPTS: &str = "AFRILabcfdilmpsue";
    let mut ops = Ops {
        set: HashSet::new(),
        p_arg: None,
    };
    let mut i = 1;
    while let Some(word) = words.get(i) {
        let Some(mut arg) = word.strip_prefix('-') else {
            break;
        };
        if arg.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        if let Some(r) = arg.strip_prefix('-') {
            arg = r;
        }
        i += 1;
        if arg.is_empty() {
            break;
        }
        for (k, c) in arg.char_indices() {
            if c == 'P' {
                ops.set.insert('P');
                let rest = &arg[k + 1..];
                if !rest.is_empty() {
                    ops.p_arg = Some(rest.to_string());
                } else if let Some(next) = words.get(i) {
                    ops.p_arg = Some(next.clone());
                    i += 1;
                } else {
                    return Err("argument expected: -P".to_string());
                }
                break;
            } else if OPTS.contains(c) {
                ops.set.insert(c);
            } else {
                return Err(format!("bad option: -{}", c));
            }
        }
    }
    Ok((ops, i))
}

/// The zmodload builtin; words[0] is "zmodload".
pub fn builtin_zmodload(words: &[String]) -> i32 {
    let nam = "zmodload";
    let (ops, first) = match parse_ops(words) {
        Ok(x) => x,
        Err(msg) => {
            builtin_err(nam, &msg);
            return 1;
        }
    };
    let args: Vec<String> = words[first..].to_vec();
    let core = core_builtin_names(&args);
    let (mut ret, msgs, pending) = with_state(|st| {
        st.managed
            .extend(args.iter().filter(|a| desc_idx(a).is_some()).cloned());
        st.core_builtins = core;
        let r = st.bin_zmodload(nam, &args, &ops);
        (r, std::mem::take(&mut st.msgs), st.pending_array.take())
    });
    flush(msgs);
    if let Some((name, items)) = pending {
        if !crate::shell::array_set(&name, items) {
            ret = 1;
        }
    }
    ret
}

impl State {
    fn new() -> State {
        let mut st = State {
            modules: BTreeMap::new(),
            enables: DESCS
                .iter()
                .map(|d| vec![false; d.features.map_or(0, |f| f.len())])
                .collect(),
            autobins: BTreeMap::new(),
            autoconds: Vec::new(),
            autoparams: BTreeMap::new(),
            automath: Vec::new(),
            core_builtins: HashSet::new(),
            msgs: Vec::new(),
            pending_array: None,
            newly_loaded: Vec::new(),
            managed: HashSet::new(),
        };
        for name in LOADED_AT_STARTUP {
            st.modules.insert(
                name.to_string(),
                Module {
                    loaded: true,
                    ..Module::default()
                },
            );
            if let Some(i) = desc_idx(name) {
                st.enables[i].iter_mut().for_each(|e| *e = true);
            }
        }
        for (module, features, deps) in STARTUP {
            for d in deps.iter() {
                st.add_dep(module, d);
            }
            let loaded = LOADED_AT_STARTUP.contains(module);
            for f in features.iter() {
                let key = st.find_or_create(module);
                st.insert_autoload(&key, f);
                // A loaded module's features replaced their stubs.
                if !loaded {
                    st.add_stub(f, module);
                }
            }
        }
        st
    }

    // Registers an autoload stub without the checks zmodload makes.
    fn add_stub(&mut self, feature: &str, module: &str) {
        let (kind, name) = (feature.as_bytes()[0], feature[2..].to_string());
        let module = module.to_string();
        match kind {
            b'b' => {
                self.autobins.insert(
                    name,
                    AutoBin {
                        module,
                        autoall: false,
                    },
                );
            }
            b'c' | b'C' => self.autoconds.insert(
                0,
                AutoCond {
                    infix: kind == b'C',
                    name,
                    module,
                    autoall: false,
                },
            ),
            b'f' => self.automath.insert(
                0,
                AutoMath {
                    name,
                    module,
                    autoall: false,
                },
            ),
            _ => {
                self.autoparams.insert(name, module);
            }
        }
    }

    fn out(&mut self, s: String) {
        self.msgs.push(Msg::Out(s));
    }

    fn zwarn(&mut self, s: String) {
        self.msgs.push(Msg::Warn(None, s));
    }

    fn zwarnnam(&mut self, nam: Option<&str>, s: String) {
        self.msgs.push(Msg::Warn(nam.map(|n| n.to_string()), s));
    }

    fn feature_on(&self, feature: &str) -> bool {
        DESCS.iter().enumerate().any(|(i, d)| {
            d.features
                .and_then(|f| f.iter().position(|x| *x == feature))
                .is_some_and(|j| self.enables[i][j])
        })
    }

    fn builtin_defined(&self, name: &str) -> bool {
        let f = format!("b:{}", name);
        if is_module_feature(&f) {
            self.feature_on(&f)
        } else {
            self.core_builtins.contains(name)
        }
    }

    fn cond_defined(&self, infix: bool, name: &str) -> bool {
        self.feature_on(&format!("{}:{}", if infix { 'C' } else { 'c' }, name))
    }

    // The parameter zsh would find under `name`, other than an autoload
    // stub. A shell special meowsh caches (SPECIAL) is the module's own.
    fn param_state(&self, name: &str) -> ParamState {
        use crate::params::{SPECIAL, UNSET};
        let shell = crate::shell::with_shell(|s| s.param(name).map(|p| (p.level, p.attrs)));
        match shell {
            Some((level, _)) if level > 0 => return ParamState::Local,
            Some((_, attrs)) if attrs & (SPECIAL | UNSET) == 0 => return ParamState::Global,
            _ => {}
        }
        if self.feature_on(&format!("p:{}", name)) {
            ParamState::Global
        } else {
            ParamState::Absent
        }
    }

    // find_module: (the module's key if it exists, the name reached).
    // With `aliasp`, follows aliases to the module they name.
    fn find_module(&self, name: &str, aliasp: bool) -> (Option<String>, String) {
        let mut name = name.to_string();
        loop {
            match self.modules.get(&name) {
                Some(m) => {
                    if aliasp {
                        if let Some(a) = &m.alias {
                            name = a.clone();
                            continue;
                        }
                    }
                    return (Some(name.clone()), name);
                }
                None => return (None, name),
            }
        }
    }

    // find_module with FINDMOD_ALIASP|FINDMOD_CREATE.
    fn find_or_create(&mut self, name: &str) -> String {
        let (found, last) = self.find_module(name, true);
        if let Some(k) = found {
            return k;
        }
        self.modules.insert(last.clone(), Module::default());
        last
    }

    fn is_loaded(&self, key: &str) -> bool {
        self.modules.get(key).is_some_and(|m| m.loaded)
    }

    fn insert_autoload(&mut self, key: &str, feature: &str) {
        let Some(m) = self.modules.get_mut(key) else {
            return;
        };
        let list = m.autoloads.get_or_insert_with(Vec::new);
        if let Err(pos) = list.binary_search_by(|x| x.as_str().cmp(feature)) {
            list.insert(pos, feature.to_string());
        }
    }

    fn add_dep(&mut self, name: &str, from: &str) {
        let key = self.find_or_create(name);
        if let Some(m) = self.modules.get_mut(&key) {
            let deps = m.deps.get_or_insert_with(Vec::new);
            if !deps.iter().any(|d| d == from) {
                deps.push(from.to_string());
            }
        }
    }

    fn printmodulenode(&self, name: &str, flags: u32) -> String {
        let Some(m) = self.modules.get(name) else {
            return String::new();
        };
        let list = flags & PRINTMOD_LIST != 0;
        let mut line = String::new();
        if flags & PRINTMOD_DEPS != 0 {
            let Some(deps) = &m.deps else {
                return String::new();
            };
            if list {
                line.push_str("zmodload -d ");
                if name.starts_with('-') {
                    line.push_str("-- ");
                }
                line.push_str(&quote(name));
            } else {
                line.push_str(name);
                line.push(':');
            }
            for d in deps {
                line.push(' ');
                line.push_str(&if list { quote(d) } else { d.clone() });
            }
        } else if flags & PRINTMOD_EXIST != 0 {
            let mut target = m;
            if let Some(a) = &m.alias {
                if flags & PRINTMOD_ALIAS == 0 {
                    return String::new();
                }
                match self.find_module(a, true).0 {
                    Some(k) => target = &self.modules[&k],
                    None => return String::new(),
                }
            }
            if !target.loaded {
                return String::new();
            }
            line.push_str(name);
        } else if let Some(a) = &m.alias {
            if list {
                line.push_str("zmodload -A ");
                if name.starts_with('-') {
                    line.push_str("-- ");
                }
                line.push_str(&quote(name));
                line.push('=');
                line.push_str(&quote(a));
            } else {
                line.push_str(name);
                line.push_str(" -> ");
                line.push_str(a);
            }
        } else if m.loaded || flags & PRINTMOD_AUTO != 0 {
            if list {
                let auto = flags & PRINTMOD_AUTO != 0;
                let mut feats: Option<(&[&str], &[bool])> = None;
                if auto {
                    if m.autoloads.as_ref().is_none_or(|a| a.is_empty()) {
                        return String::new();
                    }
                } else if flags & PRINTMOD_FEATURES != 0 {
                    match desc_idx(name).and_then(|i| DESCS[i].features.map(|f| (i, f))) {
                        Some((i, f)) if !f.is_empty() => {
                            feats = Some((f, self.enables[i].as_slice()))
                        }
                        _ => return String::new(),
                    }
                }
                line.push_str("zmodload ");
                if auto {
                    line.push_str("-Fa ");
                } else if feats.is_some() {
                    line.push_str("-F ");
                }
                if name.starts_with('-') {
                    line.push_str("-- ");
                }
                line.push_str(&quote(name));
                if auto {
                    for a in m.autoloads.iter().flatten() {
                        line.push(' ');
                        line.push_str(&quote(a));
                    }
                } else if let Some((f, e)) = feats {
                    for (feature, on) in f.iter().zip(e) {
                        if flags & PRINTMOD_LISTALL != 0 {
                            line.push_str(if *on { " +" } else { " -" });
                        } else if !on {
                            continue;
                        } else {
                            line.push(' ');
                        }
                        line.push_str(&quote(feature));
                    }
                }
            } else {
                line.push_str(name);
            }
        } else {
            return String::new();
        }
        line.push('\n');
        line
    }

    // Scans the module table in name order, skipping aliases or keeping
    // only aliases.
    fn scan_modules(&mut self, aliases: Option<bool>, flags: u32) {
        let mut out = String::new();
        for (name, m) in &self.modules {
            if aliases.is_some_and(|a| a != m.alias.is_some()) {
                continue;
            }
            out.push_str(&self.printmodulenode(name, flags));
        }
        self.out(out);
    }

    fn bin_zmodload(&mut self, nam: &str, args: &[String], ops: &Ops) -> i32 {
        let n = Some(nam);
        let ops_bcpf = ops.is('b') || ops.is('c') || ops.is('p') || ops.is('f');
        let ops_au = ops.is('a') || ops.is('u');
        if ops_bcpf && !ops_au {
            self.zwarnnam(
                n,
                "-b, -c, -f, and -p must be combined with -a or -u".into(),
            );
            return 1;
        }
        if ops.is('F') && (ops_bcpf || ops.is('u')) {
            self.zwarnnam(n, "-b, -c, -f, -p and -u cannot be combined with -F".into());
            return 1;
        }
        if ops.is('A') || ops.is('R') {
            if ops_bcpf || ops_au || ops.is('d') || (ops.is('R') && ops.is('e')) {
                self.zwarnnam(n, "invalid flags combined with -A or -R".into());
                return 1;
            }
            if !ops.is('e') {
                return self.bin_zmodload_alias(nam, args, ops);
            }
        }
        if ops.is('d') && ops.is('a') {
            self.zwarnnam(n, "-d cannot be combined with -a".into());
            return 1;
        }
        if ops.is('u') && args.is_empty() {
            self.zwarnnam(n, "what do you want to unload?".into());
            return 1;
        }
        if ops.is('e')
            && (ops.is('I')
                || ops.is('L')
                || (ops.is('a') && !ops.is('F'))
                || ops.is('d')
                || ops.is('i')
                || ops.is('u'))
        {
            self.zwarnnam(n, "-e cannot be combined with other options".into());
            return 1;
        }
        for c in ['l', 'P'] {
            if ops.is(c) && !ops.is('F') {
                self.zwarnnam(n, format!("-{} is only allowed with -F", c));
                return 1;
            }
        }
        if ops.is('F') {
            self.bin_zmodload_features(nam, args, ops)
        } else if ops.is('e') {
            self.bin_zmodload_exist(args, ops)
        } else if ops.is('d') {
            self.bin_zmodload_dep(args, ops)
        } else {
            let autoopts = ['b', 'c', 'p', 'f'].iter().filter(|c| ops.is(**c)).count();
            // zmodload -a is equivalent to zmodload -ab.
            if autoopts > 0 || ops.is('a') {
                if autoopts > 1 {
                    self.zwarnnam(n, "use only one of -b, -c, or -p".into());
                    1
                } else {
                    self.bin_zmodload_auto(nam, args, ops)
                }
            } else {
                self.bin_zmodload_load(nam, args, ops)
            }
        }
    }

    fn bin_zmodload_alias(&mut self, nam: &str, args: &[String], ops: &Ops) -> i32 {
        let n = Some(nam);
        let listflag = if ops.is('L') { PRINTMOD_LIST } else { 0 };
        if args.is_empty() {
            if ops.is('R') {
                self.zwarnnam(n, "no module alias to remove".into());
                return 1;
            }
            self.scan_modules(Some(true), listflag);
            return 0;
        }
        for arg in args {
            let (name, aliasname) = match arg.split_once('=') {
                Some((a, b)) => (a, Some(b)),
                None => (arg.as_str(), None),
            };
            if !modname_ok(name) {
                self.zwarnnam(n, format!("invalid module name `{}'", name));
                return 1;
            }
            if ops.is('R') {
                if aliasname.is_some() {
                    self.zwarnnam(n, format!("bad syntax for removing module alias: {}", name));
                    return 1;
                }
                match self.modules.get(name) {
                    Some(m) if m.alias.is_some() => {
                        self.modules.remove(name);
                    }
                    Some(_) => {
                        self.zwarnnam(n, format!("module is not an alias: {}", name));
                        return 1;
                    }
                    None => {
                        self.zwarnnam(n, format!("no such module alias: {}", name));
                        return 1;
                    }
                }
            } else if let Some(aliasname) = aliasname {
                if !modname_ok(aliasname) {
                    self.zwarnnam(n, format!("invalid module name `{}'", aliasname));
                    return 1;
                }
                let mut mname = aliasname.to_string();
                loop {
                    if mname == name {
                        self.zwarnnam(n, format!("module alias would refer to itself: {}", name));
                        return 1;
                    }
                    match self.modules.get(&mname).and_then(|m| m.alias.clone()) {
                        Some(a) => mname = a,
                        None => break,
                    }
                }
                match self.modules.get_mut(name) {
                    Some(m) if m.alias.is_some() => m.alias = Some(aliasname.to_string()),
                    Some(_) => {
                        self.zwarnnam(n, format!("module is not an alias: {}", name));
                        return 1;
                    }
                    None => {
                        self.modules.insert(
                            name.to_string(),
                            Module {
                                alias: Some(aliasname.to_string()),
                                ..Module::default()
                            },
                        );
                    }
                }
            } else {
                match self.modules.get(name) {
                    Some(m) if m.alias.is_some() => {
                        let s = self.printmodulenode(name, listflag);
                        self.out(s);
                    }
                    Some(_) => {
                        self.zwarnnam(n, format!("module is not an alias: {}", name));
                        return 1;
                    }
                    None => {
                        self.zwarnnam(n, format!("no such module alias: {}", name));
                        return 1;
                    }
                }
            }
        }
        0
    }

    fn bin_zmodload_exist(&mut self, args: &[String], ops: &Ops) -> i32 {
        if args.is_empty() {
            let flags = if ops.is('A') {
                PRINTMOD_EXIST | PRINTMOD_ALIAS
            } else {
                PRINTMOD_EXIST
            };
            self.scan_modules(None, flags);
            return 0;
        }
        for a in args {
            match self.find_module(a, true).0 {
                Some(k) if self.is_loaded(&k) => {}
                _ => return 1,
            }
        }
        0
    }

    fn bin_zmodload_dep(&mut self, args: &[String], ops: &Ops) -> i32 {
        if ops.is('u') {
            // Dependencies cannot pertain to aliases.
            let Some(key) = self.find_module(&args[0], true).0 else {
                return 0;
            };
            let Some(m) = self.modules.get_mut(&key) else {
                return 0;
            };
            if args.len() > 1 {
                if let Some(deps) = m.deps.as_mut() {
                    for a in &args[1..] {
                        if let Some(p) = deps.iter().position(|d| d == a) {
                            deps.remove(p);
                        }
                    }
                }
                if m.deps.as_ref().is_some_and(|d| d.is_empty()) {
                    m.deps = None;
                }
            } else {
                m.deps = None;
            }
            if m.deps.is_none() && !m.loaded {
                self.modules.remove(&key);
            }
            0
        } else if args.len() < 2 {
            let flags = if ops.is('L') {
                PRINTMOD_DEPS | PRINTMOD_LIST
            } else {
                PRINTMOD_DEPS
            };
            match args.first() {
                Some(a) => {
                    let s = self.printmodulenode(a, flags);
                    self.out(s);
                }
                None => self.scan_modules(None, flags),
            }
            0
        } else {
            for d in &args[1..] {
                self.add_dep(&args[0], d);
            }
            0
        }
    }

    fn bin_zmodload_auto(&mut self, nam: &str, args: &[String], ops: &Ops) -> i32 {
        let list = ops.is('L');
        let fchar = if ops.is('c') {
            if args.is_empty() {
                let mut out = String::new();
                for c in &self.autoconds {
                    if list {
                        out.push_str(&format!(
                            "zmodload -ac{} {} {}\n",
                            if c.infix { "I" } else { "" },
                            c.module,
                            c.name
                        ));
                    } else {
                        out.push_str(&format!(
                            "{}{} ({})\n",
                            if c.infix { "infix " } else { "post " },
                            c.name,
                            c.module
                        ));
                    }
                }
                self.out(out);
                return 0;
            }
            if ops.is('I') {
                'C'
            } else {
                'c'
            }
        } else if ops.is('p') {
            if args.is_empty() {
                let mut out = String::new();
                for (name, module) in &self.autoparams {
                    // A local parameter hides the stub.
                    let local =
                        crate::shell::with_shell(|s| s.param(name).is_some_and(|p| p.level > 0));
                    if local {
                        continue;
                    }
                    if list {
                        out.push_str(&format!("zmodload -ap {} {}\n", module, name));
                    } else {
                        out.push_str(&format!("{} ({})\n", name, module));
                    }
                }
                self.out(out);
                return 0;
            }
            'p'
        } else if ops.is('f') {
            if args.is_empty() {
                let mut out = String::new();
                for f in &self.automath {
                    if list {
                        out.push_str(&format!("zmodload -af {} {}\n", f.module, f.name));
                    } else {
                        out.push_str(&format!("{} ({})\n", f.name, f.module));
                    }
                }
                self.out(out);
                return 0;
            }
            'f'
        } else {
            if args.is_empty() {
                let mut out = String::new();
                for (name, b) in &self.autobins {
                    if list {
                        out.push_str("zmodload -ab ");
                        if b.module.starts_with('-') {
                            out.push_str("-- ");
                        }
                        out.push_str(&quote(&b.module));
                        if *name != b.module {
                            out.push(' ');
                            out.push_str(&quote(name));
                        }
                    } else {
                        out.push_str(name);
                        if *name != b.module {
                            out.push_str(&format!(" ({})", b.module));
                        }
                    }
                    out.push('\n');
                }
                self.out(out);
                return 0;
            }
            'b'
        };
        let mut flags = FEAT_AUTOALL;
        if ops.is('i') {
            flags |= FEAT_IGNORE;
        }
        if ops.is('u') {
            flags |= FEAT_REMOVE;
            self.autofeatures(Some(nam), None, args, Some(fchar), flags)
        } else {
            let features = if args.len() > 1 { &args[1..] } else { args };
            self.autofeatures(
                Some(nam),
                Some(args[0].as_str()),
                features,
                Some(fchar),
                flags,
            )
        }
    }

    fn unload_module(&mut self, key: &str) -> i32 {
        let key = match self.modules.get(key).and_then(|m| m.alias.clone()) {
            Some(a) => match self.find_module(&a, true).0 {
                Some(k) => k,
                None => return 1,
            },
            None => key.to_string(),
        };
        if self.is_loaded(&key) {
            self.do_cleanup_module(&key);
        }
        let Some(m) = self.modules.get_mut(&key) else {
            return 1;
        };
        m.loaded = false;
        match m.autoloads.clone().filter(|a| !a.is_empty()) {
            // Restore the autoloads so the module loads again when needed.
            Some(al) => {
                self.autofeatures(Some("zsh"), Some(key.as_str()), &al, None, FEAT_IGNORE);
            }
            None => {
                if m.deps.is_none() {
                    self.modules.remove(&key);
                }
            }
        }
        0
    }

    fn unload_named_module(&mut self, modname: &str, nam: &str, silent: bool) -> i32 {
        let (found, mname) = self.find_module(modname, true);
        let Some(key) = found else {
            if !silent {
                self.zwarnnam(Some(nam), format!("no such module {}", modname));
                return 1;
            }
            return 0;
        };
        let in_use = self
            .modules
            .values()
            .any(|dm| dm.loaded && dm.deps.as_ref().is_some_and(|d| d.contains(&mname)));
        if in_use {
            self.zwarnnam(
                Some(nam),
                format!(
                    "module {} is in use by another module and cannot be unloaded",
                    mname
                ),
            );
            return 1;
        }
        i32::from(self.unload_module(&key) != 0)
    }

    fn bin_zmodload_load(&mut self, nam: &str, args: &[String], ops: &Ops) -> i32 {
        let mut ret = 0;
        if ops.is('u') {
            for a in args {
                if self.unload_named_module(a, nam, ops.is('i')) != 0 {
                    ret = 1;
                }
            }
            ret
        } else if args.is_empty() {
            let flags = if ops.is('L') { PRINTMOD_LIST } else { 0 };
            self.scan_modules(Some(false), flags);
            0
        } else {
            for a in args {
                let r = self.require_module(a, None, ops.is('s'));
                if r != 0 && ret != 1 {
                    ret = r;
                }
            }
            ret
        }
    }

    fn bin_zmodload_features(&mut self, nam: &str, args: &[String], ops: &Ops) -> i32 {
        let n = Some(nam);
        let (modname, args) = match args.split_first() {
            Some((m, rest)) => (m.clone(), rest),
            None if ops.is('L') => {
                if ops.is('P') {
                    self.zwarnnam(n, "-P is only allowed with a module name".into());
                    return 1;
                }
                let mut flags = PRINTMOD_LIST | PRINTMOD_FEATURES;
                if ops.is('l') {
                    flags |= PRINTMOD_LISTALL;
                }
                if ops.is('a') {
                    flags |= PRINTMOD_AUTO;
                }
                self.scan_modules(Some(false), flags);
                return 0;
            }
            None => {
                self.zwarnnam(n, "-F requires a module name".into());
                return 1;
            }
        };
        let pat = ops.is('m');
        let (l, big_l, e) = (ops.is('l'), ops.is('L'), ops.is('e'));
        if l || big_l || e {
            let param = ops.p_arg.clone();
            let found = self.find_module(&modname, true).0;
            if ops.is('a') {
                let Some(key) = found else {
                    return 1;
                };
                let Some(al) = self.modules[&key].autoloads.clone() else {
                    return 1;
                };
                if e {
                    for fp in args {
                        let (sense, fstr) = strip_sense(fp);
                        if al.iter().any(|a| a == fstr) != sense {
                            return 1;
                        }
                    }
                    return 0;
                }
                if let Some(p) = param {
                    self.pending_array = Some((p, al));
                    return 0;
                }
                let mut out = String::new();
                if big_l {
                    out.push_str(&format!(
                        "zmodload -aF {}{}",
                        key,
                        if al.is_empty() { '\n' } else { ' ' }
                    ));
                }
                for (i, a) in al.iter().enumerate() {
                    out.push_str(a);
                    out.push(if big_l && i + 1 < al.len() { ' ' } else { '\n' });
                }
                self.out(out);
                return 0;
            }
            let key = match found {
                Some(k) if self.is_loaded(&k) => k,
                _ => {
                    if !e {
                        self.zwarnnam(n, format!("module `{}' is not yet loaded", modname));
                    }
                    return 1;
                }
            };
            let Some((idx, features)) =
                desc_idx(&key).and_then(|i| DESCS[i].features.map(|f| (i, f)))
            else {
                if !e {
                    self.zwarnnam(n, format!("module `{}' does not support features", key));
                }
                return 1;
            };
            let enables = self.enables[idx].clone();
            for arg in args {
                let (on, a) = if let Some(r) = arg.strip_prefix('-') {
                    (Some(false), r)
                } else if let Some(r) = arg.strip_prefix('+') {
                    (Some(true), r)
                } else {
                    (None, arg.as_str())
                };
                let mut hit = false;
                for (j, f) in features.iter().enumerate() {
                    if feat_match(a, f, pat) {
                        // For -e, check the given state, if any.
                        if e && on.is_some_and(|o| o != enables[j]) {
                            return 1;
                        }
                        hit = true;
                        if !pat {
                            break;
                        }
                    }
                }
                if !hit {
                    if !e {
                        let what = if pat {
                            "has no feature matching"
                        } else {
                            "has no such feature"
                        };
                        self.zwarnnam(n, format!("module `{}' {}: `{}'", modname, what, arg));
                    }
                    return 1;
                }
            }
            if e {
                return 0;
            }
            let mut items = Vec::new();
            let mut out = String::new();
            if param.is_none() && big_l {
                out.push_str(&format!("zmodload -F {} ", key));
            }
            for (j, f) in features.iter().enumerate() {
                // zsh compares a plain argument here with its +/- sign
                // still on, so a signed argument selects nothing.
                if !args.is_empty()
                    && !args.iter().any(|arg| {
                        if pat {
                            feat_match(strip_pm(arg), f, true)
                        } else {
                            arg == f
                        }
                    })
                {
                    continue;
                }
                let onoff = if big_l && !l {
                    if !enables[j] {
                        continue;
                    }
                    ""
                } else if enables[j] {
                    "+"
                } else {
                    "-"
                };
                if param.is_some() {
                    items.push(format!("{}{}", onoff, f));
                } else {
                    out.push_str(onoff);
                    out.push_str(f);
                    out.push(if big_l && j + 1 < features.len() {
                        ' '
                    } else {
                        '\n'
                    });
                }
            }
            match param {
                Some(p) => self.pending_array = Some((p, items)),
                None => self.out(out),
            }
            return 0;
        } else if ops.is('P') {
            self.zwarnnam(n, "-P can only be used with -l or -L".into());
            return 1;
        } else if ops.is('a') {
            if pat {
                self.zwarnnam(n, "-m cannot be used with -a".into());
                return 1;
            }
            // Marking a feature for autoload is separate from enabling
            // it, so zmodload -aF always acts as with -i.
            return self.autofeatures(n, Some(modname.as_str()), args, None, FEAT_IGNORE);
        }
        let features = args
            .iter()
            .map(|s| FeatEnable { s: s.clone(), pat })
            .collect();
        self.require_module(&modname, Some(features), ops.is('s'))
    }

    // Sets the features of loaded module `key`: all of them when
    // `enablesarr` is None. 0 for success, 1 for failure, 2 if some
    // features could not be set.
    fn do_module_features(
        &mut self,
        key: &str,
        mut enablesarr: Option<Vec<FeatEnable>>,
        flags: u32,
    ) -> i32 {
        let mut ret = 0;
        let ignore = flags & FEAT_IGNORE != 0;
        let Some(idx) = desc_idx(key) else {
            return 0;
        };
        let Some(features) = DESCS[idx].features else {
            if enablesarr.is_some() {
                if !ignore {
                    self.zwarn(format!("module `{}' does not support features", key));
                }
                return 1;
            }
            return 0;
        };
        if flags & FEAT_CHECKAUTO != 0 {
            let autoloads = self
                .modules
                .get(key)
                .and_then(|m| m.autoloads.clone())
                .unwrap_or_default();
            for al in autoloads {
                if features.contains(&al.as_str()) {
                    continue;
                }
                if !ignore {
                    self.zwarn(format!(
                        "module `{}' has no such feature: `{}': autoload cancelled",
                        key, al
                    ));
                }
                self.autofeatures(
                    None,
                    Some(key),
                    std::slice::from_ref(&al),
                    None,
                    FEAT_IGNORE | FEAT_REMOVE,
                );
                // It cannot be enabled after all.
                if let Some(arr) = enablesarr.as_mut() {
                    if let Some(k) = arr.iter().position(|fe| fe.matches(&al)) {
                        arr.remove(k);
                        ret = 1;
                    }
                }
            }
        }
        let mut new = self.enables[idx].clone();
        match &enablesarr {
            Some(arr) => {
                for fe in arr {
                    let (on, esp) = strip_sense(&fe.s);
                    let mut found = false;
                    for (j, f) in features.iter().enumerate() {
                        if feat_match(esp, f, fe.pat) {
                            new[j] = on;
                            found = true;
                            if !fe.pat {
                                break;
                            }
                        }
                    }
                    if !found {
                        if !ignore {
                            let what = if fe.pat {
                                "has no feature matching"
                            } else {
                                "has no such feature"
                            };
                            self.zwarn(format!("module `{}' {}: `{}'", key, what, esp));
                        }
                        return 1;
                    }
                }
            }
            None => new.iter_mut().for_each(|e| *e = true),
        }
        if self.setfeatureenables(idx, &new) {
            return 2;
        }
        ret
    }

    // Turns module DESCS[idx]'s features on or off as `new` says,
    // replacing autoload stubs. True on any failure.
    fn setfeatureenables(&mut self, idx: usize, new: &[bool]) -> bool {
        let desc = &DESCS[idx];
        let features = desc.features.unwrap_or(&[]);
        let mut err = false;
        for (j, f) in features.iter().enumerate() {
            let on = new[j];
            if on == self.enables[idx][j] {
                continue;
            }
            if on {
                let (kind, name) = (f.as_bytes()[0], &f[2..]);
                match kind {
                    b'b' => {
                        self.autobins.remove(name);
                    }
                    b'c' | b'C' => self
                        .autoconds
                        .retain(|c| !(c.infix == (kind == b'C') && c.name == name)),
                    b'f' => self.automath.retain(|m| m.name != name),
                    _ => {
                        if self.checkaddparam(name, false) != 0 {
                            self.zwarnnam(
                                Some(desc.name),
                                format!("error when adding parameter `{}'", name),
                            );
                            err = true;
                            continue;
                        }
                    }
                }
            }
            self.enables[idx][j] = on;
        }
        err
    }

    // Whether module parameter `nam` can be added: 0 yes (an autoload
    // stub in the way is removed), 1 no (reported), 2 no but -i keeps
    // quiet about it.
    fn checkaddparam(&mut self, nam: &str, opt_i: bool) -> i32 {
        match self.param_state(nam) {
            ParamState::Absent => {
                self.autoparams.remove(nam);
                0
            }
            ParamState::Global if opt_i => 2,
            st => {
                self.zwarn(format!(
                    "Can't add module parameter `{}': {}",
                    nam,
                    if st == ParamState::Local {
                        "local parameter exists"
                    } else {
                        "parameter already exists"
                    }
                ));
                1
            }
        }
    }

    fn do_boot_module(
        &mut self,
        key: &str,
        enablesarr: Option<Vec<FeatEnable>>,
        silent: bool,
    ) -> i32 {
        let flags = if silent {
            FEAT_IGNORE | FEAT_CHECKAUTO
        } else {
            FEAT_CHECKAUTO
        };
        self.do_module_features(key, enablesarr, flags)
    }

    fn do_cleanup_module(&mut self, key: &str) {
        if let Some(idx) = desc_idx(key) {
            let off = vec![false; self.enables[idx].len()];
            self.setfeatureenables(idx, &off);
        }
    }

    fn load_failed(&mut self, name: &str, silent: bool) {
        if !silent {
            self.zwarn(format!("failed to load module `{}': no such module", name));
        }
    }

    // 0 for success, 1 for complete failure, 2 if some features could
    // not be set.
    fn load_module(
        &mut self,
        name: &str,
        enablesarr: Option<Vec<FeatEnable>>,
        silent: bool,
    ) -> i32 {
        if !modname_ok(name) {
            if !silent {
                self.zwarn(format!("invalid module name `{}'", name));
            }
            return 1;
        }
        let (found, name) = self.find_module(name, true);
        let Some(key) = found else {
            if desc_idx(&name).is_none() {
                self.load_failed(&name, silent);
                return 1;
            }
            self.modules.insert(name.clone(), Module::default());
            let bootret = self.do_boot_module(&name, enablesarr, silent);
            if bootret == 1 {
                self.do_cleanup_module(&name);
                self.modules.remove(&name);
                return 1;
            }
            self.mark_loaded(&name);
            return bootret;
        };
        let Some(m) = self.modules.get_mut(&key) else {
            return 1;
        };
        if m.loaded {
            return 0;
        }
        if m.busy {
            self.zwarn(format!("circular dependencies for module ;{}", key));
            return 1;
        }
        m.busy = true;
        let deps = m.deps.clone().unwrap_or_default();
        for d in deps {
            if self.load_module(&d, None, silent) == 1 {
                if let Some(m) = self.modules.get_mut(&key) {
                    m.busy = false;
                }
                return 1;
            }
        }
        if let Some(m) = self.modules.get_mut(&key) {
            m.busy = false;
        }
        if desc_idx(&key).is_none() {
            self.load_failed(&key, silent);
            return 1;
        }
        let bootret = self.do_boot_module(&key, enablesarr, silent);
        if bootret == 1 {
            self.do_cleanup_module(&key);
            return 1;
        }
        self.mark_loaded(&key);
        bootret
    }

    fn mark_loaded(&mut self, key: &str) {
        if let Some(m) = self.modules.get_mut(key) {
            m.loaded = true;
        }
        self.newly_loaded.push(key.to_string());
    }

    // Loads `module` if needed and sets its features (all of them when
    // `features` is None).
    fn require_module(
        &mut self,
        module: &str,
        features: Option<Vec<FeatEnable>>,
        silent: bool,
    ) -> i32 {
        let (found, name) = self.find_module(module, true);
        match found {
            Some(k) if self.is_loaded(&k) => self.do_module_features(&k, features, 0),
            _ => self.load_module(&name, features, silent),
        }
    }

    // Makes module `modname` provide `prefix` + `feature`, or all its
    // features when `feature` is None.
    fn ensurefeature(&mut self, modname: &str, prefix: &str, feature: Option<&str>) -> i32 {
        let features = feature.map(|f| {
            vec![FeatEnable {
                s: format!("{}{}", prefix, f),
                pat: false,
            }]
        });
        self.require_module(modname, features, false)
    }

    fn autoload(&mut self, kind: char, name: &str) -> Option<Result<(), String>> {
        match kind {
            'b' => {
                let stub = self.autobins.get(name)?.clone();
                self.ensurefeature(&stub.module, "b:", (!stub.autoall).then_some(name));
                if self.builtin_defined(name) {
                    Some(Ok(()))
                } else if self.autobins.remove(name).is_some() {
                    // zsh finds the stub again and drops it, silently.
                    Some(Err(String::new()))
                } else {
                    Some(Err(format!(
                        "autoloading module {} failed to define builtin: {}",
                        stub.module, name
                    )))
                }
            }
            'p' => {
                let module = self.autoparams.get(name)?.clone();
                self.ensurefeature(&module, "p:", Some(name));
                if self.feature_on(&format!("p:{}", name)) {
                    Some(Ok(()))
                } else {
                    Some(Err(format!(
                        "autoloading module {} failed to define parameter: {}",
                        module, name
                    )))
                }
            }
            'f' => {
                let pos = self.automath.iter().position(|f| f.name == name)?;
                let stub = self.automath.remove(pos);
                self.ensurefeature(&stub.module, "f:", (!stub.autoall).then_some(name));
                if self.feature_on(&format!("f:{}", name)) {
                    Some(Ok(()))
                } else {
                    Some(Err(format!(
                        "autoloading module {} failed to define math function: {}",
                        stub.module, name
                    )))
                }
            }
            'c' | 'C' => {
                let infix = kind == 'C';
                let stub = self
                    .autoconds
                    .iter()
                    .find(|c| c.infix == infix && c.name == name)?
                    .clone();
                let prefix = if infix { "C:" } else { "c:" };
                self.ensurefeature(&stub.module, prefix, (!stub.autoall).then_some(name));
                if self.cond_defined(infix, name) {
                    Some(Ok(()))
                } else {
                    self.autoconds
                        .retain(|c| !(c.infix == infix && c.name == name));
                    Some(Err(String::new()))
                }
            }
            _ => None,
        }
    }

    // Adds (or with FEAT_REMOVE, or a `-` prefix, removes) autoloads for
    // `features` of `module`. With `prefchar` the features are bare
    // names of that type. `module` is None only when removing; then the
    // module is the one that has the autoload.
    fn autofeatures(
        &mut self,
        cmdnam: Option<&str>,
        module: Option<&str>,
        features: &[String],
        prefchar: Option<char>,
        defflags: u32,
    ) -> i32 {
        let mut ret = 0;
        let defm = module.map(|m| self.find_or_create(m));
        let (modfeatures, modenables) = match &defm {
            Some(k) if self.is_loaded(k) => match desc_idx(k) {
                Some(i) => (DESCS[i].features, self.enables[i].clone()),
                None => (None, Vec::new()),
            },
            _ => (None, Vec::new()),
        };
        for f in features {
            let mut flags = defflags;
            let (mut add, fchar, fnam, feature) = match prefchar {
                Some(pc) => (true, pc, f.clone(), format!("{}:{}", pc, f)),
                None => {
                    let (add, rest) = strip_sense(f);
                    if rest.as_bytes().get(1) != Some(&b':') {
                        self.zwarnnam(
                            cmdnam,
                            format!("bad format for autoloadable feature: `{}'", rest),
                        );
                        ret = 1;
                        continue;
                    }
                    (
                        add,
                        rest.as_bytes()[0] as char,
                        rest[2..].to_string(),
                        rest.to_string(),
                    )
                }
            };
            if flags & FEAT_REMOVE != 0 {
                add = false;
            }
            let typnam = match fchar {
                'b' => "builtin",
                'C' => {
                    flags |= FEAT_INFIX;
                    "condition"
                }
                'c' => "condition",
                'f' => "math function",
                'p' => "parameter",
                _ => {
                    self.zwarnnam(
                        cmdnam,
                        format!("bad autoloadable feature type: `{}'", fchar),
                    );
                    ret = 1;
                    continue;
                }
            };
            if fnam.contains('/') {
                self.zwarnnam(cmdnam, format!("{}: `/' is invalid in a {}", fnam, typnam));
                ret = 1;
                continue;
            }
            let m = match &defm {
                Some(k) => k.clone(),
                None => {
                    let owner = self
                        .modules
                        .iter()
                        .find(|(_, m)| m.autoloads.as_ref().is_some_and(|a| a.contains(&feature)))
                        .map(|(k, _)| k.clone());
                    match owner {
                        Some(k) => k,
                        None => {
                            if flags & FEAT_IGNORE == 0 {
                                ret = 1;
                                self.zwarnnam(cmdnam, format!("{}: no such {}", fnam, typnam));
                            }
                            continue;
                        }
                    }
                }
            };
            let mut subret = 0;
            if add {
                // A loaded module must provide the feature; one it
                // provides already needs no autoload.
                if let Some(mf) = modfeatures {
                    match mf.iter().position(|x| *x == feature) {
                        None => {
                            self.zwarnnam(
                                cmdnam,
                                format!("module `{}' has no such feature: `{}'", m, feature),
                            );
                            ret = 1;
                            continue;
                        }
                        Some(j) if modenables[j] => continue,
                        Some(_) => {}
                    }
                }
                self.insert_autoload(&m, &feature);
            } else if let Some(al) = self.modules.get_mut(&m).and_then(|x| x.autoloads.as_mut()) {
                match al.iter().position(|x| *x == feature) {
                    Some(p) => {
                        al.remove(p);
                    }
                    // With -i (or zmodload -Fa), removing an autoload that
                    // is not there is not an error.
                    None => subret = if flags & FEAT_IGNORE != 0 { -2 } else { 2 },
                }
            }
            if subret == 0 {
                let module = module.unwrap_or_default();
                subret = match (fchar, add) {
                    ('b', true) => self.add_autobin(module, &fnam, flags),
                    ('b', false) => self.del_autobin(&fnam, flags),
                    ('c' | 'C', true) => self.add_autocond(module, &fnam, flags),
                    ('c' | 'C', false) => self.del_autocond(&fnam, flags),
                    ('f', true) => self.add_automathfunc(module, &fnam, flags),
                    ('f', false) => self.del_automathfunc(&fnam, flags),
                    (_, true) => self.add_autoparam(module, &fnam, flags),
                    (_, false) => self.del_autoparam(&fnam, flags),
                };
            }
            if subret != 0 {
                // -2: not an error, the function just was not run.
                if subret != -2 {
                    ret = 1;
                }
                match subret {
                    1 => self.zwarnnam(cmdnam, format!("failed to add {} `{}'", typnam, fnam)),
                    2 => self.zwarnnam(cmdnam, format!("{}: no such {}", fnam, typnam)),
                    3 => self.zwarnnam(cmdnam, format!("{}: {} is already defined", fnam, typnam)),
                    _ => {}
                }
            }
        }
        ret
    }

    fn add_autobin(&mut self, module: &str, bnam: &str, flags: u32) -> i32 {
        if self.builtin_defined(bnam) {
            return i32::from(flags & FEAT_IGNORE == 0);
        }
        self.autobins.insert(
            bnam.to_string(),
            AutoBin {
                module: module.to_string(),
                autoall: flags & FEAT_AUTOALL != 0,
            },
        );
        0
    }

    fn del_autobin(&mut self, bnam: &str, flags: u32) -> i32 {
        if self.autobins.remove(bnam).is_some() || flags & FEAT_IGNORE != 0 {
            0
        } else if self.builtin_defined(bnam) {
            3
        } else {
            2
        }
    }

    fn add_autocond(&mut self, module: &str, cnam: &str, flags: u32) -> i32 {
        let infix = flags & FEAT_INFIX != 0;
        if self.cond_defined(infix, cnam) {
            return i32::from(flags & FEAT_IGNORE == 0);
        }
        self.autoconds
            .retain(|c| !(c.infix == infix && c.name == cnam));
        self.autoconds.insert(
            0,
            AutoCond {
                infix,
                name: cnam.to_string(),
                module: module.to_string(),
                autoall: flags & FEAT_AUTOALL != 0,
            },
        );
        0
    }

    fn del_autocond(&mut self, cnam: &str, flags: u32) -> i32 {
        let infix = flags & FEAT_INFIX != 0;
        let pos = self
            .autoconds
            .iter()
            .position(|c| c.infix == infix && c.name == cnam);
        if let Some(p) = pos {
            self.autoconds.remove(p);
            0
        } else if flags & FEAT_IGNORE != 0 {
            0
        } else if self.cond_defined(infix, cnam) {
            3
        } else {
            2
        }
    }

    fn add_autoparam(&mut self, module: &str, pnam: &str, flags: u32) -> i32 {
        match self.checkaddparam(pnam, flags & FEAT_IGNORE != 0) {
            0 => {
                self.autoparams.insert(pnam.to_string(), module.to_string());
                0
            }
            // -i: there is such a parameter and we were told not to mind.
            2 => 0,
            // checkaddparam has said why.
            _ => -1,
        }
    }

    fn del_autoparam(&mut self, pnam: &str, flags: u32) -> i32 {
        let ignore = flags & FEAT_IGNORE != 0;
        // A local parameter hides the stub.
        let state = self.param_state(pnam);
        if state == ParamState::Local {
            return if ignore { 0 } else { 3 };
        }
        if self.autoparams.remove(pnam).is_some() || ignore {
            0
        } else if state == ParamState::Global {
            3
        } else {
            2
        }
    }

    fn add_automathfunc(&mut self, module: &str, fnam: &str, flags: u32) -> i32 {
        if self.feature_on(&format!("f:{}", fnam)) {
            return i32::from(flags & FEAT_IGNORE == 0);
        }
        self.automath.retain(|f| f.name != fnam);
        self.automath.insert(
            0,
            AutoMath {
                name: fnam.to_string(),
                module: module.to_string(),
                autoall: flags & FEAT_AUTOALL != 0,
            },
        );
        0
    }

    fn del_automathfunc(&mut self, fnam: &str, flags: u32) -> i32 {
        if let Some(p) = self.automath.iter().position(|f| f.name == fnam) {
            self.automath.remove(p);
            0
        } else if flags & FEAT_IGNORE != 0 {
            0
        } else if self.feature_on(&format!("f:{}", fnam)) {
            3
        } else {
            2
        }
    }
}
