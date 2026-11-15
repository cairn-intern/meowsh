use std::collections::{BTreeMap, HashMap, HashSet};
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

pub const VAR_EXPORT: u32 = 1 << 0;
pub const VAR_READONLY: u32 = 1 << 1;
pub const VAR_SPECIAL: u32 = 1 << 2;

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
pub struct Var {
    pub value: String,
    pub flags: u32,
}

#[derive(Clone)]
pub struct ZStyle {
    pub pattern: String,
    pub matcher: Option<glob::Pattern>,
    pub style: String,
    pub values: Vec<String>,
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
    pub vars: HashMap<String, Var>,
    pub aliases: HashMap<String, String>,
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
    pub lineno: i32,
    pub loop_depth: usize,
    pub func_depth: usize,
    pub source_depth: usize,
    // Nonzero while running a context in which `set -e` is ignored
    // (conditions, non-final AND-OR elements, negated pipelines).
    pub errexit_suppressed: usize,
    pub flow: Flow,
    // Exit status of the last command substitution in the current simple
    // command; becomes `$?` when the command has no command name.
    pub last_cmdsub_status: Option<i32>,
    // Character offset inside the current `getopts` argument.
    pub getopts_pos: usize,
    // Stack of local-variable scopes. Each entry maps a variable name to
    // its prior global value (None = the variable was unset before being
    // declared local). Pushed on function entry, popped on return.
    pub local_scopes: Vec<HashMap<String, Option<Var>>>,
    // Zsh-style named options (setopt/unsetopt). Keys are normalized to
    // lowercase with underscores stripped. Most are stubs for Phase 1;
    // the subset that maps to a legacy OPT_* flag also toggles that flag.
    pub named_opts: HashMap<String, bool>,
    // zstyle entries in declaration order. Later matching entries override
    // earlier ones for query operations.
    pub zstyles: Vec<ZStyle>,
    // Indexed arrays (zsh-style, 1-based at the user level). Stored 0-based.
    // A name that exists in `arrays` shadows any same-named scalar in `vars`.
    pub arrays: HashMap<String, Vec<String>>,
    // Associative arrays declared with `typeset -A`.
    pub assoc_arrays: HashMap<String, BTreeMap<String, String>>,
    // Arrays declared with `typeset -U` keep first occurrences only.
    pub unique_arrays: HashSet<String>,
    // Directory stack used by pushd/popd/dirs.
    pub dir_stack: Vec<String>,
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
            vars: HashMap::new(),
            aliases: HashMap::new(),
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
            lineno: 0,
            loop_depth: 0,
            func_depth: 0,
            source_depth: 0,
            errexit_suppressed: 0,
            flow: Flow::None,
            last_cmdsub_status: None,
            getopts_pos: 0,
            local_scopes: Vec::new(),
            named_opts: HashMap::new(),
            zstyles: Vec::new(),
            arrays: HashMap::new(),
            assoc_arrays: HashMap::new(),
            unique_arrays: HashSet::new(),
            dir_stack: Vec::new(),
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RedirOp {
    Read,      // <
    Write,     // >
    Clobber,   // >|
    Append,    // >>
    ReadWrite, // <>
    DupIn,     // <&
    DupOut,    // >&
    HereDoc,   // << and <<-
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
    pub assigns: Vec<Assign>,
    pub words: Vec<String>,
    pub redirs: Vec<Redir>,
}

#[derive(Clone, Debug)]
pub struct CaseItem {
    pub patterns: Vec<String>,
    pub body: Option<Node>,
    // Terminated by `;&` instead of `;;`.
    pub fallthrough: bool,
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
    For {
        var: String,
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
    },
    DBracket(Vec<String>),
    Redirected {
        node: Box<Node>,
        redirs: Vec<Redir>,
    },
    Empty,
}
