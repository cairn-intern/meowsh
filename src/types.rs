use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

pub const OPT_ALLEXPORT: u32 = 1 << 0;
pub const OPT_ERREXIT: u32 = 1 << 1;
pub const OPT_NOGLOB: u32 = 1 << 2;
pub const OPT_HASHALL: u32 = 1 << 3;
pub const OPT_INTERACTIVE: u32 = 1 << 4;
pub const OPT_MONITOR: u32 = 1 << 5;
pub const OPT_NOEXEC: u32 = 1 << 6;
pub const OPT_NOUNSET: u32 = 1 << 7;
pub const OPT_VERBOSE: u32 = 1 << 8;
pub const OPT_XTRACE: u32 = 1 << 9;
pub const OPT_NOCLOBBER: u32 = 1 << 10;
pub const OPT_PIPEFAIL: u32 = 1 << 11;

pub const VAR_EXPORT: u32 = crate::params::EXPORT;
pub const VAR_READONLY: u32 = crate::params::READONLY;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum JobState {
    Running,
    Stopped,
    Done,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ProcState {
    Running,
    Stopped,
    Done,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: i32,
    pub pgid: i32,
    pub procs: Vec<Process>,
    pub state: JobState,
    pub notified: bool,
    pub foreground: bool,
    pub cmd_text: String,
}

#[derive(Clone, Debug)]
pub struct Process {
    pub pid: i32,
    pub status: i32,
    pub state: ProcState,
    pub cmd: String,
}

#[derive(Clone)]
pub struct FuncDef {
    pub body: Arc<Node>,
    // Where it was defined (file or shell name), for `whence -v`.
    pub origin: String,
    // Line of the definition: error messages count lines from it.
    pub line: usize,
}

/// Pending non-local control flow. Set by `break`, `continue`, `return`,
/// `exit`, errexit, and SIGINT; every list and loop stops executing while
/// one is pending, and the construct that owns it clears it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Flow {
    None,
    Break(usize),
    Continue(usize),
    Return(i32),
    Exit(i32),
    Interrupt,
}

pub struct Shell {
    pub opts: u32,
    pub emulation: crate::options::Emulation,
    pub last_status: i32,
    pub shell_pid: i32,
    pub shell_pgid: i32,
    pub last_bg_pid: i32,
    pub argv0: String,
    pub interactive: bool,
    pub login_shell: bool,
    // True when the shell owns the terminal and runs jobs in their own
    // process groups.
    pub job_control: bool,
    // True in forked subshells (command substitution, pipelines, `( )`).
    pub in_subshell: bool,
    // All parameters: scalars, arrays and associative arrays.
    pub params: HashMap<String, crate::params::Param>,
    pub pos_params: Vec<String>,
    pub jobs: Mutex<Vec<Job>>,
    pub history: Mutex<Vec<String>>,
    pub next_job_id: Mutex<i32>,
    pub ps1: String,
    pub ps2: String,
    pub cur_prompt: String,
    pub history_file: String,
    // Trap actions keyed by signal number; 0 is the EXIT condition.
    pub traps: HashMap<i32, String>,
    pub functions: HashMap<String, FuncDef>,
    pub loop_depth: usize,
    pub func_depth: usize,
    pub source_depth: usize,
    // Nonzero while running a context in which `set -e` is ignored
    // (conditions, non-final AND-OR elements, negated pipelines).
    pub errexit_suppressed: usize,
    // The same for ERR_RETURN, which starts afresh in each function.
    pub errreturn_suppressed: usize,
    pub errreturn_stack: Vec<usize>,
    pub flow: Flow,
    // The pending Flow::Exit comes from an error (expansion, assignment,
    // redirection) rather than `exit`; `eval` stops it, as in zsh.
    pub error_exit: bool,
    // Exit status of the last command substitution in the current simple
    // command; becomes `$?` when the command has no command name.
    pub last_cmdsub_status: Option<i32>,
    // Character offset inside the current `getopts` argument.
    pub getopts_pos: usize,
    // Loop depth of the parent when this subshell forked: `break` here
    // cannot reach those loops.
    pub subshell_loop_depth: usize,
    // Stack of local scopes. Each maps a parameter name to its state
    // before it was made local (None = unset). Pushed on function entry,
    // popped on return.
    pub local_scopes: Vec<HashMap<String, Option<crate::params::Param>>>,
    // Private parameters (zsh/param/private) set aside while a function
    // their owner calls runs: (owner depth, name, parameter).
    pub private_stash: Vec<(usize, String, crate::params::Param)>,
    // Zsh-style named options (setopt/unsetopt). Keys are normalized to
    // lowercase with underscores stripped. Most are stubs for Phase 1;
    // the subset that maps to a legacy OPT_* flag also toggles that flag.
    pub named_opts: HashMap<String, bool>,
    // Directory stack used by pushd/popd/dirs.
    pub dir_stack: Vec<String>,
    // Coprocess pipes: (write end to its input, read end of its output).
    pub coproc: Option<(i32, i32)>,
    // The file `.`/`source` is reading, and the script's name ($0 at
    // startup).
    pub source_file: Option<String>,
    pub script_name: String,
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            opts: 0,
            last_status: 0,
            shell_pid: 0,
            shell_pgid: 0,
            last_bg_pid: 0,
            argv0: String::new(),
            interactive: false,
            login_shell: false,
            job_control: false,
            in_subshell: false,
            params: HashMap::from([
                (
                    ALIASES.to_string(),
                    crate::params::Param::with(crate::params::Value::Assoc(BTreeMap::new())),
                ),
                (
                    GALIASES.to_string(),
                    crate::params::Param::with(crate::params::Value::Assoc(BTreeMap::new())),
                ),
            ]),
            pos_params: Vec::new(),
            jobs: Mutex::new(Vec::new()),
            history: Mutex::new(Vec::new()),
            next_job_id: Mutex::new(1),
            ps1: String::new(),
            ps2: String::from("> "),
            cur_prompt: String::new(),
            history_file: String::new(),
            traps: HashMap::new(),
            functions: HashMap::new(),

            loop_depth: 0,
            func_depth: 0,
            source_depth: 0,
            errexit_suppressed: 0,
            errreturn_suppressed: 0,
            errreturn_stack: Vec::new(),
            flow: Flow::None,
            error_exit: false,
            emulation: crate::options::Emulation::Zsh,
            last_cmdsub_status: None,
            getopts_pos: 0,
            subshell_loop_depth: 0,
            local_scopes: Vec::new(),
            private_stash: Vec::new(),
            named_opts: HashMap::new(),
            dir_stack: Vec::new(),
            coproc: None,
            source_file: None,
            script_name: "zsh".to_string(),
        }
    }

    /// The alias table (`global`: zsh `alias -g`). As in zsh, aliases
    /// live in the associative arrays `$aliases` and `$galiases`.
    /// Name to report as a function's origin: the file being sourced or
    /// run, else the shell's name.
    pub fn function_origin(&self) -> String {
        self.source_file
            .clone()
            .unwrap_or_else(|| self.script_name.clone())
    }

    pub fn alias_map(&mut self, global: bool) -> &mut BTreeMap<String, String> {
        self.assoc_entry(if global { GALIASES } else { ALIASES })
    }

    pub fn alias(&self, name: &str, global: bool) -> Option<&String> {
        if crate::builtins::is_disabled('a', name) {
            return None;
        }
        self.assoc(if global { GALIASES } else { ALIASES })
            .and_then(|m| m.get(name))
    }
}

pub const ALIASES: &str = "aliases";
pub const GALIASES: &str = "galiases";
pub const SALIASES: &str = "saliases";

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RedirOp {
    Read,          // <
    Write,         // >
    Clobber,       // >|
    Append,        // >>
    AppendClobber, // >>| and >>!: appends even with NO_CLOBBER
    ReadWrite,     // <>
    DupIn,         // <&
    DupOut,        // >&
    HereDoc,       // << and <<-
    HereStr,       // <<< word
}

#[derive(Clone, Debug)]
pub struct HereDoc {
    pub id: usize,
    pub body: String,
    // False when any part of the delimiter was quoted.
    pub expand: bool,
}

#[derive(Clone, Debug)]
pub struct Redir {
    pub fd: i32,
    pub op: RedirOp,
    // Raw (unexpanded) target word; for here-documents, the delimiter.
    pub target: String,
    pub heredoc: Option<HereDoc>,
    // zsh `{name}>file`: the descriptor is chosen by the shell (10 or
    // above) and stored in the parameter.
    pub var: Option<String>,
}

#[derive(Clone, Debug)]
pub enum AssignValue {
    Scalar(String),
    Array(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct Assign {
    pub name: String,
    // `name[sub]=value`
    pub subscript: Option<String>,
    // `name+=value`
    pub append: bool,
    pub value: AssignValue,
}

#[derive(Clone, Debug, Default)]
pub struct SimpleCmd {
    // Input line where it starts, for $LINENO and error messages.
    pub line: usize,
    pub assigns: Vec<Assign>,
    pub words: Vec<String>,
    pub redirs: Vec<Redir>,
    // Array values given to declaration commands, `typeset a=(x y)`: the
    // index of the `a=` word in `words` and the raw items.
    pub decl_arrays: Vec<(usize, Vec<String>)>,
}

#[derive(Clone, Debug)]
pub struct CaseItem {
    pub patterns: Vec<String>,
    pub body: Option<Node>,
    // Terminated by `;&` instead of `;;`.
    pub fallthrough: bool,
    // Terminated by `;|`: keep testing the following patterns.
    pub cont: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AndOrOp {
    And,
    Or,
}

#[derive(Clone, Debug)]
pub struct ListItem {
    pub node: Node,
    pub background: bool,
}

#[derive(Clone, Debug)]
pub enum Node {
    Simple(SimpleCmd),
    Pipeline {
        cmds: Vec<Node>,
        bang: bool,
    },
    AndOr {
        first: Box<Node>,
        rest: Vec<(AndOrOp, Node)>,
    },
    List(Vec<ListItem>),
    Subshell(Box<Node>),
    Brace(Box<Node>),
    If {
        branches: Vec<(Node, Node)>,
        else_body: Option<Box<Node>>,
    },
    Loop {
        cond: Box<Node>,
        body: Box<Node>,
        until: bool,
    },
    // `(( expr ))`
    // `(( expr ))` and the input line it is on.
    Arith(String, usize),
    // `repeat count body`
    Repeat {
        count: String,
        body: Box<Node>,
    },
    // `select var [in words]; do body; done`
    Select {
        var: String,
        words: Option<Vec<String>>,
        body: Box<Node>,
    },
    // `coproc cmd`
    Coproc(Box<Node>),
    // `time pipeline`: report its CPU and elapsed time.
    Time(Box<Node>),
    // Run in the background with `&|` / `&!`: no job entry, no notice.
    Disowned(Box<Node>),
    // zsh `{ try } always { cleanup }`
    Always {
        body: Box<Node>,
        always: Box<Node>,
    },
    // zsh anonymous function `() { body } args`, run immediately.
    AnonFunc {
        body: std::sync::Arc<Node>,
        args: Vec<String>,
        line: usize,
        // `function -T { ... }`: traced.
        traced: bool,
    },
    // `for (( init; cond; step ))`
    ForArith {
        init: String,
        cond: String,
        step: String,
        body: Box<Node>,
    },
    For {
        vars: Vec<String>,
        words: Option<Vec<String>>,
        body: Box<Node>,
    },
    Case {
        word: String,
        items: Vec<CaseItem>,
    },
    FuncDef {
        name: String,
        body: Box<Node>,
        line: usize,
        // `function -T name { ... }`: traced.
        traced: bool,
    },
    DBracket(Vec<String>, usize),
    Redirected {
        node: Box<Node>,
        redirs: Vec<Redir>,
    },
    Empty,
}
