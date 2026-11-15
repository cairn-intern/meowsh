//! Command execution.

use crate::expand::{
    expand_assign_value, expand_heredoc, expand_pattern, expand_prompt_string, expand_string,
    expand_words, ExpError, ExpResult,
};
use crate::jobs::{add_job, reap_jobs, status_to_exit, wait_foreground, TTY_FD};
use crate::lexer::assignment_split;
use crate::parser::{ParseError, Parser};
use crate::shell::{
    array_append, array_set, array_set_subscript, build_env, find_command, is_array, opt_set,
    pop_scope, push_scope, var_get, var_get_opt, var_set, with_shell,
};
use crate::sys::{close, dup_high, err_line, pipe_cloexec, read_all, temp_file_with};
use crate::types::{
    AndOrOp, Assign, AssignValue, CaseItem, Flow, FuncDef, JobState, ListItem, Node, Redir,
    RedirOp, SimpleCmd, Var, OPT_ERREXIT, OPT_NOCLOBBER, OPT_NOEXEC, OPT_PIPEFAIL, OPT_VERBOSE,
    OPT_XTRACE, VAR_READONLY,
};
use std::ffi::CString;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;

const MAX_FUNC_DEPTH: usize = 1000;

// ---- shared state helpers -------------------------------------------------

pub fn last_status() -> i32 {
    with_shell(|s| s.last_status)
}

pub fn flow() -> Flow {
    with_shell(|s| s.flow)
}

pub fn set_flow(f: Flow) {
    with_shell(|s| s.flow = f);
}

fn flow_pending() -> bool {
    flow() != Flow::None
}

fn interactive() -> bool {
    with_shell(|s| s.interactive)
}

fn suppress_errexit(delta: isize) {
    with_shell(|s| {
        s.errexit_suppressed = (s.errexit_suppressed as isize + delta).max(0) as usize;
    });
}

fn check_errexit(status: i32) {
    if status == 0 {
        return;
    }
    with_shell(|s| {
        if s.opts & OPT_ERREXIT != 0 && s.errexit_suppressed == 0 && s.flow == Flow::None {
            s.flow = Flow::Exit(status);
        }
    });
}

// An expansion error aborts the current command line in an interactive
// shell and exits a non-interactive one.
fn expansion_failed(e: ExpError) -> i32 {
    with_shell(|s| {
        if s.flow == Flow::None {
            s.flow = if s.interactive {
                Flow::Interrupt
            } else {
                Flow::Exit(e.status)
            };
        }
    });
    e.status
}

pub fn strerror(errno: i32) -> String {
    unsafe {
        let p = libc::strerror(errno);
        if p.is_null() {
            return format!("error {}", errno);
        }
        std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

// ---- process creation -----------------------------------------------------

/// How a forked child relates to job control.
#[derive(Clone, Copy)]
pub enum ForkKind {
    /// A foreground job (or member of one); 0 starts a new process group.
    Foreground(i32),
    /// An asynchronous list.
    Background,
    /// Stays in the shell's process group (command substitution).
    Inline,
}

/// Forks. In the child, signal dispositions and shell state are reset for
/// a subshell; returns 0 there and the pid in the parent (-1 on failure).
pub fn fork_process(kind: ForkKind) -> i32 {
    let _ = std::io::stdout().flush();
    let jc = with_shell(|s| s.job_control);
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        err_line(&format!("meowsh: fork: {}", strerror(last_errno())));
        return -1;
    }
    if pid == 0 {
        if jc {
            match kind {
                ForkKind::Foreground(pgid) => unsafe {
                    libc::setpgid(0, pgid);
                    let fd = TTY_FD.load(Ordering::SeqCst);
                    if fd >= 0 {
                        libc::tcsetpgrp(fd, libc::getpgrp());
                    }
                },
                ForkKind::Background => unsafe {
                    libc::setpgid(0, 0);
                },
                ForkKind::Inline => {}
            }
        }
        crate::trap::reset_in_child(matches!(kind, ForkKind::Background) && !jc);
        with_shell(|s| {
            s.in_subshell = true;
            s.interactive = false;
            s.job_control = false;
            s.jobs.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        TTY_FD.store(-1, Ordering::SeqCst);
    } else if jc {
        match kind {
            ForkKind::Foreground(pgid) => unsafe {
                libc::setpgid(pid, if pgid == 0 { pid } else { pgid });
            },
            ForkKind::Background => unsafe {
                libc::setpgid(pid, pid);
            },
            ForkKind::Inline => {}
        }
    }
    pid
}

/// Leaves a forked child: runs its EXIT trap and exits without running
/// the parent's destructors or flushing its buffers twice.
pub fn exit_child(status: i32) -> ! {
    let status = match flow() {
        Flow::Exit(n) => n,
        _ => status,
    };
    crate::trap::run_exit_trap();
    let status = match flow() {
        Flow::Exit(n) => n,
        _ => status,
    };
    let _ = std::io::stdout().flush();
    unsafe { libc::_exit(status & 0xff) }
}

/// Exits the shell: runs the EXIT trap and terminates the process.
pub fn exit_shell(status: i32) -> ! {
    if with_shell(|s| s.in_subshell) {
        exit_child(status);
    }
    with_shell(|s| s.flow = Flow::None);
    if interactive() {
        crate::builtins::run_hooks("zshexit", &[]);
        with_shell(|s| s.flow = Flow::None);
    }
    crate::trap::run_exit_trap();
    let status = match flow() {
        Flow::Exit(n) => n,
        _ => status,
    };
    let _ = std::io::stdout().flush();
    std::process::exit(status & 0xff)
}

/// Replaces the process with an external command. Never returns.
pub fn exec_external(words: &[String], env: &[(String, String)]) -> ! {
    let name = &words[0];
    let path = if name.contains('/') {
        Some(name.clone())
    } else {
        find_command(name)
    };
    let Some(path) = path else {
        err_line(&format!("meowsh: {}: command not found", name));
        unsafe { libc::_exit(127) }
    };
    let to_c = |s: &str| CString::new(s.as_bytes()).ok();
    let (Some(c_path), Some(argv)) = (
        to_c(&path),
        words.iter().map(|w| to_c(w)).collect::<Option<Vec<_>>>(),
    ) else {
        err_line(&format!("meowsh: {}: invalid argument", name));
        unsafe { libc::_exit(126) }
    };
    let envp: Vec<CString> = env
        .iter()
        .filter_map(|(k, v)| CString::new(format!("{}={}", k, v)).ok())
        .collect();
    let mut argv_p: Vec<*const libc::c_char> = argv.iter().map(|c| c.as_ptr()).collect();
    argv_p.push(std::ptr::null());
    let mut envp_p: Vec<*const libc::c_char> = envp.iter().map(|c| c.as_ptr()).collect();
    envp_p.push(std::ptr::null());
    let _ = std::io::stdout().flush();
    unsafe {
        libc::execve(c_path.as_ptr(), argv_p.as_ptr(), envp_p.as_ptr());
    }
    let e = last_errno();
    if e == libc::ENOEXEC {
        // No interpreter line: run the file as a shell script.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(c_exe) = to_c(&exe.to_string_lossy()) {
                let mut args: Vec<*const libc::c_char> = vec![c_exe.as_ptr(), c_path.as_ptr()];
                args.extend(argv.iter().skip(1).map(|c| c.as_ptr()));
                args.push(std::ptr::null());
                unsafe {
                    libc::execve(c_exe.as_ptr(), args.as_ptr(), envp_p.as_ptr());
                }
            }
        }
    }
    err_line(&format!("meowsh: {}: {}", name, strerror(e)));
    unsafe { libc::_exit(if e == libc::ENOENT { 127 } else { 126 }) }
}

// ---- redirections ---------------------------------------------------------

enum Target {
    Opened(i32),
    Dup(i32),
    Close,
}

fn open_file(path: &str, flags: i32) -> Result<i32, String> {
    let c = CString::new(path).map_err(|_| format!("{}: invalid file name", path))?;
    let fd = unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC, 0o666 as libc::c_uint) };
    if fd < 0 {
        return Err(format!("{}: {}", path, strerror(last_errno())));
    }
    Ok(fd)
}

// Err(None) means the message was already printed by the expander.
fn open_redir(r: &Redir) -> Result<Target, Option<String>> {
    if r.op == RedirOp::HereDoc {
        let body = match &r.heredoc {
            Some(h) if h.expand => expand_heredoc(&h.body).map_err(|_| None)?,
            Some(h) => h.body.clone(),
            None => String::new(),
        };
        let fd =
            temp_file_with(body.as_bytes()).map_err(|e| Some(format!("here-document: {}", e)))?;
        return Ok(Target::Opened(fd));
    }
    let target = expand_string(&r.target).map_err(|_| None)?;
    if r.op == RedirOp::HereStr {
        let fd = temp_file_with(format!("{}\n", target).as_bytes())
            .map_err(|e| Some(format!("here-string: {}", e)))?;
        return Ok(Target::Opened(fd));
    }
    let file = |flags| open_file(&target, flags).map(Target::Opened).map_err(Some);
    match r.op {
        RedirOp::DupIn | RedirOp::DupOut => {
            if target == "-" {
                return Ok(Target::Close);
            }
            match target.parse::<i32>() {
                Ok(n) if n >= 0 => {
                    if unsafe { libc::fcntl(n, libc::F_GETFD) } < 0 {
                        return Err(Some(format!("{}: {}", n, strerror(libc::EBADF))));
                    }
                    Ok(Target::Dup(n))
                }
                _ => Err(Some(format!("{}: ambiguous redirect", target))),
            }
        }
        RedirOp::Read => file(libc::O_RDONLY),
        RedirOp::Write if opt_set(OPT_NOCLOBBER) => match std::fs::metadata(&target) {
            Ok(m) if m.is_file() => {
                Err(Some(format!("{}: cannot overwrite existing file", target)))
            }
            Ok(_) => file(libc::O_WRONLY),
            Err(_) => file(libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL),
        },
        RedirOp::Write | RedirOp::Clobber => file(libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC),
        RedirOp::Append => file(libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND),
        RedirOp::ReadWrite => file(libc::O_RDWR | libc::O_CREAT),
        RedirOp::HereDoc | RedirOp::HereStr => unreachable!(),
    }
}

/// Saved descriptors: (fd, copy), where copy is -1 if fd was closed.
pub type SavedFds = Vec<(i32, i32)>;

/// A redirection failed; the message has been printed.
#[derive(Debug)]
pub struct RedirError;

/// Applies redirections. With `save`, the previous descriptors are kept
/// so `restore_redirs` can undo them.
pub fn apply_redirs(redirs: &[Redir], save: bool) -> Result<SavedFds, RedirError> {
    let mut saved = Vec::new();
    for r in redirs {
        let fd = r.fd;
        if save {
            saved.push((fd, dup_high(fd)));
        }
        let target = match open_redir(r) {
            Ok(t) => t,
            Err(msg) => {
                if let Some(m) = msg {
                    err_line(&format!("meowsh: redirection: {}", m));
                }
                restore_redirs(saved);
                return Err(RedirError);
            }
        };
        let ok = match target {
            Target::Opened(newfd) => {
                let ok = newfd == fd || unsafe { libc::dup2(newfd, fd) } >= 0;
                if newfd != fd {
                    close(newfd);
                } else {
                    unsafe {
                        libc::fcntl(fd, libc::F_SETFD, 0);
                    }
                }
                ok
            }
            Target::Dup(src) => src == fd || unsafe { libc::dup2(src, fd) } >= 0,
            Target::Close => {
                close(fd);
                true
            }
        };
        if !ok {
            err_line(&format!("meowsh: {}: {}", fd, strerror(last_errno())));
            restore_redirs(saved);
            return Err(RedirError);
        }
    }
    Ok(saved)
}

pub fn restore_redirs(saved: SavedFds) {
    for (fd, copy) in saved.into_iter().rev() {
        if copy >= 0 {
            unsafe {
                libc::dup2(copy, fd);
            }
            close(copy);
        } else {
            close(fd);
        }
    }
}

// ---- assignments ----------------------------------------------------------

enum Value {
    Scalar(String),
    Array(Vec<String>),
}

struct Expanded {
    name: String,
    subscript: Option<String>,
    append: bool,
    value: Value,
}

fn expand_assigns(assigns: &[Assign]) -> ExpResult<Vec<Expanded>> {
    let mut out = Vec::new();
    for a in assigns {
        let value = match &a.value {
            AssignValue::Scalar(v) => Value::Scalar(expand_assign_value(v)?),
            AssignValue::Array(items) => Value::Array(expand_words(items)?),
        };
        out.push(Expanded {
            name: a.name.clone(),
            subscript: a.subscript.clone(),
            append: a.append,
            value,
        });
    }
    Ok(out)
}

fn is_readonly(name: &str) -> bool {
    with_shell(|s| {
        s.vars
            .get(name)
            .map(|v| v.flags & VAR_READONLY != 0)
            .unwrap_or(false)
    })
}

fn apply_assign(a: &Expanded, export: bool) -> Result<(), i32> {
    if is_readonly(&a.name) {
        err_line(&format!("meowsh: {}: readonly variable", a.name));
        return Err(1);
    }
    let ok = match (&a.subscript, &a.value) {
        (Some(sub), Value::Scalar(v)) => {
            let is_assoc = with_shell(|s| s.assoc_arrays.contains_key(&a.name));
            let key = if is_assoc {
                expand_string(sub).map_err(|e| e.status)?
            } else {
                let idx = expand_string(sub).map_err(|e| e.status)?;
                match crate::arith::eval(&idx) {
                    Ok(n) => n.to_string(),
                    Err(e) => {
                        err_line(&format!("meowsh: {}: {}", idx, e));
                        return Err(1);
                    }
                }
            };
            array_set_subscript(&a.name, &key, v.clone(), a.append)
        }
        (Some(_), Value::Array(_)) => {
            err_line(&format!(
                "meowsh: {}: cannot assign list to array member",
                a.name
            ));
            false
        }
        (None, Value::Array(items)) => {
            if a.append {
                array_append(&a.name, items.clone())
            } else {
                array_set(&a.name, items.clone())
            }
        }
        (None, Value::Scalar(v)) => {
            if a.append && is_array(&a.name) {
                array_append(&a.name, vec![v.clone()])
            } else if a.append {
                let cur = var_get(&a.name);
                var_set(&a.name, &format!("{}{}", cur, v), export)
            } else {
                var_set(&a.name, v, export)
            }
        }
    };
    if ok {
        Ok(())
    } else {
        Err(1)
    }
}

fn assign_text(a: &Expanded) -> String {
    let sub = a
        .subscript
        .as_ref()
        .map(|s| format!("[{}]", s))
        .unwrap_or_default();
    let op = if a.append { "+=" } else { "=" };
    match &a.value {
        Value::Scalar(v) => format!("{}{}{}{}", a.name, sub, op, v),
        Value::Array(items) => format!("{}{}{}({})", a.name, sub, op, items.join(" ")),
    }
}

// Variables assigned for the duration of one builtin or function call.
struct TempVars(Vec<(String, Option<Var>)>);

impl TempVars {
    fn set(assigns: &[Expanded]) -> Result<TempVars, i32> {
        let mut saved = TempVars(Vec::new());
        for a in assigns {
            let Value::Scalar(v) = &a.value else { continue };
            if a.subscript.is_some() {
                continue;
            }
            if is_readonly(&a.name) {
                err_line(&format!("meowsh: {}: readonly variable", a.name));
                saved.restore();
                return Err(1);
            }
            let prev = with_shell(|s| s.vars.get(&a.name).cloned());
            saved.0.push((a.name.clone(), prev));
            var_set(&a.name, v, true);
        }
        Ok(saved)
    }

    fn restore(self) {
        for (name, prev) in self.0.into_iter().rev() {
            match prev {
                Some(var) => {
                    var_set(&name, &var.value, false);
                    with_shell(|s| s.vars.insert(name, var));
                }
                None => {
                    with_shell(|s| s.vars.remove(&name));
                }
            }
        }
    }
}

fn xtrace(parts: &[String]) {
    if !opt_set(OPT_XTRACE) || parts.is_empty() {
        return;
    }
    let ps4 = var_get_opt("PS4").unwrap_or_else(|| "+ ".to_string());
    let ps4 = expand_prompt_string(&ps4).unwrap_or(ps4);
    err_line(&format!("{}{}", ps4, parts.join(" ")));
}

// Declaration utilities take assignment arguments, which expand like
// assignments (no field splitting or globbing).
fn expand_command_words(raw: &[String]) -> ExpResult<Vec<String>> {
    let Some(first) = raw.first() else {
        return Ok(Vec::new());
    };
    if !matches!(
        first.as_str(),
        "export" | "readonly" | "local" | "typeset" | "declare"
    ) {
        return expand_words(raw);
    }
    let mut out = expand_words(&raw[..1])?;
    for w in &raw[1..] {
        match assignment_split(w) {
            Some((name, sub, append, value)) => {
                let sub = sub.map(|s| format!("[{}]", s)).unwrap_or_default();
                let op = if append { "+=" } else { "=" };
                out.push(format!(
                    "{}{}{}{}",
                    name,
                    sub,
                    op,
                    expand_assign_value(&value)?
                ));
            }
            None => out.extend(expand_words(std::slice::from_ref(w))?),
        }
    }
    Ok(out)
}

// ---- node execution -------------------------------------------------------

pub fn exec_node(node: &Node) -> i32 {
    let mark = procsub_mark();
    let st = exec_node_inner(node);
    procsub_close_from(mark);
    st
}

fn exec_node_inner(node: &Node) -> i32 {
    if flow_pending() {
        return last_status();
    }
    if opt_set(OPT_NOEXEC) && !interactive() {
        return 0;
    }
    let status = match node {
        Node::Simple(cmd) => {
            let st = exec_simple(cmd, false);
            set_pipestatus(&[st]);
            check_errexit(st);
            st
        }
        Node::Pipeline { cmds, bang } => exec_pipeline(cmds, *bang),
        Node::AndOr { first, rest } => exec_and_or(first, rest),
        Node::List(items) => exec_list(items),
        Node::Subshell(body) => {
            let st = exec_subshell(body);
            check_errexit(st);
            st
        }
        Node::Brace(body) => exec_node(body),
        Node::If {
            branches,
            else_body,
        } => exec_if(branches, else_body.as_deref()),
        Node::Loop { cond, body, until } => exec_loop(cond, body, *until),
        Node::For { vars, words, body } => exec_for(vars, words.as_deref(), body),
        Node::Disowned(inner) => exec_node(inner),
        Node::Always { body, always } => {
            let st = exec_node(body);
            // The cleanup runs even after break, return or an error; a
            // pending flow resumes afterwards unless the cleanup sets one.
            let pending = flow();
            set_flow(Flow::None);
            exec_node(always);
            if flow() == Flow::None {
                set_flow(pending);
            }
            st
        }
        Node::AnonFunc { body, args } => match expand_words(args) {
            Ok(mut words) => {
                words.insert(0, "(anon)".to_string());
                call_function(body, &words)
            }
            Err(e) => expansion_failed(e),
        },
        Node::Arith(expr) => {
            let st = arith_status(expr);
            check_errexit(st);
            st
        }
        Node::ForArith {
            init,
            cond,
            step,
            body,
        } => exec_for_arith(init, cond, step, body),
        Node::Case { word, items } => exec_case(word, items),
        Node::FuncDef { name, body } => {
            with_shell(|s| {
                s.functions.insert(
                    name.clone(),
                    FuncDef {
                        body: Arc::new((**body).clone()),
                    },
                )
            });
            0
        }
        Node::DBracket(words) => {
            let st = crate::testexpr::eval_dbracket(words);
            check_errexit(st);
            st
        }
        Node::Redirected { node, redirs } => match apply_redirs(redirs, true) {
            Ok(saved) => {
                let st = exec_node(node);
                restore_redirs(saved);
                st
            }
            Err(RedirError) => {
                check_errexit(1);
                1
            }
        },
        Node::Empty => 0,
    };
    with_shell(|s| s.last_status = status);
    status
}

// Runs a node in a forked child that exits afterwards; a simple command
// replaces the child instead of forking again.
fn exec_in_child(node: &Node) -> ! {
    let st = match node {
        Node::Simple(cmd) => exec_simple(cmd, true),
        _ => exec_node(node),
    };
    exit_child(st)
}

fn exec_simple(cmd: &SimpleCmd, in_child: bool) -> i32 {
    with_shell(|s| s.last_cmdsub_status = None);
    let words = match expand_command_words(&cmd.words) {
        Ok(w) => w,
        Err(e) => return expansion_failed(e),
    };
    let assigns = match expand_assigns(&cmd.assigns) {
        Ok(a) => a,
        Err(e) => return expansion_failed(e),
    };
    if flow_pending() {
        return last_status();
    }

    if words.is_empty() {
        xtrace(&assigns.iter().map(assign_text).collect::<Vec<_>>());
        for a in &assigns {
            if let Err(st) = apply_assign(a, false) {
                if !interactive() {
                    set_flow(Flow::Exit(st));
                }
                return st;
            }
        }
        match apply_redirs(&cmd.redirs, true) {
            Ok(saved) => restore_redirs(saved),
            Err(RedirError) => return 1,
        }
        return with_shell(|s| s.last_cmdsub_status).unwrap_or(0);
    }

    let mut trace: Vec<String> = assigns.iter().map(assign_text).collect();
    trace.extend(words.iter().cloned());
    xtrace(&trace);
    run_command(words, &assigns, &cmd.redirs, in_child)
}

fn run_command(
    mut words: Vec<String>,
    assigns: &[Expanded],
    redirs: &[Redir],
    in_child: bool,
) -> i32 {
    // `command [-p] name ...` skips function lookup.
    let mut skip_functions = false;
    if words[0] == "command" {
        let mut i = 1;
        while i < words.len() && words[i].starts_with('-') && words[i] != "-" {
            match words[i].as_str() {
                "-p" => i += 1,
                "--" => {
                    i += 1;
                    break;
                }
                _ => break,
            }
        }
        if i < words.len() && !matches!(words[i].as_str(), "-v" | "-V") {
            words.drain(..i);
            skip_functions = true;
        } else if i >= words.len() {
            return 0;
        }
    }
    // zsh/bash `builtin name ...` runs the builtin even if a function or
    // alias shadows it.
    let mut builtin_only = false;
    if words[0] == "builtin" {
        words.remove(0);
        if words.is_empty() {
            return 0;
        }
        if !crate::builtins::is_builtin(&words[0]) {
            err_line(&format!(
                "meowsh: builtin: {}: not a shell builtin",
                words[0]
            ));
            return 1;
        }
        builtin_only = true;
        skip_functions = true;
    }
    let name = words[0].clone();

    if name == "exec" && !skip_functions {
        return exec_builtin(&words[1..], assigns, redirs);
    }

    if crate::builtins::is_special_builtin(&name) {
        for a in assigns {
            if let Err(st) = apply_assign(a, false) {
                if !interactive() {
                    set_flow(Flow::Exit(st));
                }
                return st;
            }
        }
        let saved = match apply_redirs(redirs, true) {
            Ok(s) => s,
            Err(RedirError) => return 1,
        };
        let st = crate::builtins::run(&name, &words);
        restore_redirs(saved);
        return st;
    }

    let func = if skip_functions {
        None
    } else {
        with_shell(|s| s.functions.get(&name).map(|f| f.body.clone()))
    };
    if let Some(body) = func {
        let tmp = match TempVars::set(assigns) {
            Ok(t) => t,
            Err(st) => return st,
        };
        let st = match apply_redirs(redirs, true) {
            Ok(saved) => {
                let st = call_function(&body, &words);
                restore_redirs(saved);
                st
            }
            Err(RedirError) => 1,
        };
        tmp.restore();
        return st;
    }

    if builtin_only || crate::builtins::is_builtin(&name) {
        let tmp = match TempVars::set(assigns) {
            Ok(t) => t,
            Err(st) => return st,
        };
        let st = match apply_redirs(redirs, true) {
            Ok(saved) => {
                let st = crate::builtins::run(&name, &words);
                restore_redirs(saved);
                st
            }
            Err(RedirError) => 1,
        };
        tmp.restore();
        return st;
    }

    // zsh AUTO_CD: a directory name typed as a command changes to it.
    if words.len() == 1
        && interactive()
        && with_shell(|s| s.named_opts.get("autocd").copied().unwrap_or(false))
        && crate::shell::find_command(&name).is_none()
        && std::path::Path::new(&name).is_dir()
    {
        return crate::builtins::run("cd", &["cd".to_string(), name]);
    }

    let overrides: Vec<(String, String)> = assigns
        .iter()
        .filter(|a| a.subscript.is_none())
        .filter_map(|a| match &a.value {
            Value::Scalar(v) => Some((a.name.clone(), v.clone())),
            Value::Array(_) => None,
        })
        .collect();
    let env = build_env(&overrides);

    if in_child {
        if apply_redirs(redirs, false).is_err() {
            exit_child(1);
        }
        exec_external(&words, &env);
    }
    let pid = fork_process(ForkKind::Foreground(0));
    if pid == 0 {
        if apply_redirs(redirs, false).is_err() {
            exit_child(1);
        }
        exec_external(&words, &env);
    }
    if pid < 0 {
        return 1;
    }
    wait_foreground(&[pid], pid, &words.join(" "))[0]
}

fn exec_builtin(args: &[String], assigns: &[Expanded], redirs: &[Redir]) -> i32 {
    if args.is_empty() {
        for a in assigns {
            if let Err(st) = apply_assign(a, false) {
                return st;
            }
        }
        return match apply_redirs(redirs, false) {
            Ok(_) => 0,
            Err(RedirError) => {
                if !interactive() {
                    set_flow(Flow::Exit(1));
                }
                1
            }
        };
    }
    if apply_redirs(redirs, false).is_err() {
        return 1;
    }
    let overrides: Vec<(String, String)> = assigns
        .iter()
        .filter_map(|a| match &a.value {
            Value::Scalar(v) if a.subscript.is_none() => Some((a.name.clone(), v.clone())),
            _ => None,
        })
        .collect();
    let env = build_env(&overrides);
    exec_external(args, &env)
}

type Frame = (
    std::collections::HashMap<i32, String>,
    u32,
    std::collections::HashMap<String, bool>,
);

// zsh function scoping: an EXIT trap set inside a function runs when it
// returns; with LOCAL_TRAPS / LOCAL_OPTIONS (also set by `emulate -L`)
// traps / options go back to their values at entry.
fn restore_function_frame(entry: Frame, status: i32) {
    let (traps, opts, named) = entry;
    let (local_traps, local_opts, exit_action) = with_shell(|s| {
        let on = |k: &str| s.named_opts.get(k).copied().unwrap_or(false);
        let exit = s.traps.get(&0).cloned();
        let changed = exit != traps.get(&0).cloned();
        (
            on("localtraps"),
            on("localoptions"),
            exit.filter(|_| changed),
        )
    });
    if exit_action.is_some() || local_traps {
        let current = with_shell(|s| s.traps.clone());
        let mut sigs: Vec<i32> = current.keys().chain(traps.keys()).copied().collect();
        sigs.sort();
        sigs.dedup();
        for sig in sigs {
            let before = traps.get(&sig).cloned();
            if (local_traps || sig == 0) && current.get(&sig) != before.as_ref() {
                crate::trap::set_trap(sig, before);
            }
        }
    }
    if local_opts {
        with_shell(|s| {
            s.opts = opts;
            s.named_opts = named;
        });
    }
    if let Some(action) = exit_action {
        with_shell(|s| s.last_status = status);
        let pending = flow();
        set_flow(Flow::None);
        crate::trap::run_action(&action);
        if flow() == Flow::None || matches!(flow(), Flow::Return(_)) {
            set_flow(pending);
        }
    }
}

pub fn call_function(body: &Arc<Node>, words: &[String]) -> i32 {
    let depth = with_shell(|s| s.func_depth);
    if depth >= MAX_FUNC_DEPTH {
        err_line(&format!(
            "meowsh: {}: maximum function nesting level exceeded",
            words[0]
        ));
        return 1;
    }
    // zsh (FUNCTION_ARGZERO): $0 is the function name while it runs.
    let (saved_params, saved_loops, saved_argv0) = with_shell(|s| {
        s.func_depth += 1;
        let loops = std::mem::replace(&mut s.loop_depth, 0);
        (
            std::mem::replace(&mut s.pos_params, words[1..].to_vec()),
            loops,
            std::mem::replace(&mut s.argv0, words[0].clone()),
        )
    });
    let entry = with_shell(|s| (s.traps.clone(), s.opts, s.named_opts.clone()));
    push_scope();
    let mut st = exec_node(body);
    with_shell(|s| {
        if let Flow::Return(n) = s.flow {
            st = n;
            s.flow = Flow::None;
        }
    });
    restore_function_frame(entry, st);
    pop_scope();
    with_shell(|s| {
        s.pos_params = saved_params;
        s.loop_depth = saved_loops;
        s.argv0 = saved_argv0;
        s.func_depth -= 1;
        s.last_status = st;
    });
    st
}

fn describe(node: &Node) -> String {
    match node {
        Node::Simple(c) => c.words.join(" "),
        Node::Pipeline { cmds, bang } => {
            let s = cmds.iter().map(describe).collect::<Vec<_>>().join(" | ");
            if *bang {
                format!("! {}", s)
            } else {
                s
            }
        }
        Node::AndOr { first, rest } => {
            let mut s = describe(first);
            for (op, n) in rest {
                s.push_str(if *op == AndOrOp::And { " && " } else { " || " });
                s.push_str(&describe(n));
            }
            s
        }
        Node::Subshell(b) => format!("( {} )", describe(b)),
        Node::Brace(b) => format!("{{ {}; }}", describe(b)),
        Node::Redirected { node, .. } => describe(node),
        Node::List(items) => items
            .iter()
            .map(|i| describe(&i.node))
            .collect::<Vec<_>>()
            .join("; "),
        _ => "...".to_string(),
    }
}

fn exec_pipeline(cmds: &[Node], bang: bool) -> i32 {
    if bang {
        suppress_errexit(1);
    }
    let status = if cmds.len() == 1 {
        exec_node(&cmds[0])
    } else {
        run_pipeline(cmds)
    };
    if bang {
        suppress_errexit(-1);
        return if status == 0 { 1 } else { 0 };
    }
    check_errexit(status);
    status
}

/// zsh's `$pipestatus`: the exit status of each command of the last
/// pipeline.
fn set_pipestatus(codes: &[i32]) {
    let items = codes.iter().map(|c| c.to_string()).collect();
    with_shell(|s| s.arrays.insert("pipestatus".to_string(), items));
}

fn run_pipeline(cmds: &[Node]) -> i32 {
    let text = cmds.iter().map(describe).collect::<Vec<_>>().join(" | ");
    let mut pids = Vec::new();
    let mut pgid = 0;
    let mut prev_read = -1;
    for (k, cmd) in cmds.iter().enumerate() {
        let last = k + 1 == cmds.len();
        let (r, w) = if last {
            (-1, -1)
        } else {
            match pipe_cloexec() {
                Ok(p) => p,
                Err(e) => {
                    err_line(&format!("meowsh: pipe: {}", e));
                    break;
                }
            }
        };
        let pid = fork_process(ForkKind::Foreground(pgid));
        if pid == 0 {
            unsafe {
                if prev_read >= 0 {
                    libc::dup2(prev_read, 0);
                    libc::close(prev_read);
                }
                if w >= 0 {
                    libc::dup2(w, 1);
                    libc::close(w);
                    libc::close(r);
                }
            }
            exec_in_child(cmd);
        }
        close(prev_read);
        close(w);
        prev_read = r;
        if pid < 0 {
            close(r);
            prev_read = -1;
            break;
        }
        if pgid == 0 {
            pgid = pid;
        }
        pids.push(pid);
    }
    close(prev_read);
    if pids.is_empty() {
        return 1;
    }
    let codes = wait_foreground(&pids, pgid, &text);
    set_pipestatus(&codes);
    if opt_set(OPT_PIPEFAIL) {
        codes.iter().rev().find(|&&c| c != 0).copied().unwrap_or(0)
    } else {
        *codes.last().unwrap_or(&0)
    }
}

fn exec_subshell(body: &Node) -> i32 {
    let pid = fork_process(ForkKind::Foreground(0));
    if pid == 0 {
        exec_in_child(body);
    }
    if pid < 0 {
        return 1;
    }
    wait_foreground(&[pid], pid, &describe(body))[0]
}

fn exec_and_or(first: &Node, rest: &[(AndOrOp, Node)]) -> i32 {
    suppress_errexit(1);
    let mut status = exec_node(first);
    suppress_errexit(-1);
    for (k, (op, node)) in rest.iter().enumerate() {
        if flow_pending() {
            break;
        }
        let run = match op {
            AndOrOp::And => status == 0,
            AndOrOp::Or => status != 0,
        };
        if !run {
            continue;
        }
        let last = k + 1 == rest.len();
        if !last {
            suppress_errexit(1);
        }
        status = exec_node(node);
        if !last {
            suppress_errexit(-1);
        }
    }
    status
}

fn exec_list(items: &[ListItem]) -> i32 {
    let mut status = last_status();
    for item in items {
        if flow_pending() {
            break;
        }
        status = if item.background {
            exec_background(&item.node)
        } else {
            exec_node(&item.node)
        };
        with_shell(|s| s.last_status = status);
        crate::trap::run_pending();
    }
    status
}

fn exec_background(node: &Node) -> i32 {
    let (node, disown) = match node {
        Node::Disowned(inner) => (inner.as_ref(), true),
        n => (n, false),
    };
    let jc = with_shell(|s| s.job_control);
    let pid = fork_process(ForkKind::Background);
    if pid == 0 {
        if !jc {
            if let Ok(fd) = open_file("/dev/null", libc::O_RDONLY) {
                unsafe {
                    libc::dup2(fd, 0);
                }
                close(fd);
            }
        }
        exec_in_child(node);
    }
    if pid < 0 {
        return 1;
    }
    with_shell(|s| s.last_bg_pid = pid);
    if disown {
        return 0;
    }
    let id = add_job(pid, &[pid], JobState::Running, &describe(node));
    if interactive() {
        err_line(&format!("[{}] {}", id, pid));
    }
    0
}

fn exec_if(branches: &[(Node, Node)], else_body: Option<&Node>) -> i32 {
    for (cond, body) in branches {
        suppress_errexit(1);
        let st = exec_node(cond);
        suppress_errexit(-1);
        if flow_pending() {
            return st;
        }
        if st == 0 {
            return exec_node(body);
        }
    }
    match else_body {
        Some(b) => exec_node(b),
        None => 0,
    }
}

// After a loop body: Some(true) leaves the loop, Some(false) starts the
// next iteration, None continues normally.
fn loop_control() -> Option<bool> {
    with_shell(|s| match s.flow {
        Flow::None => None,
        Flow::Break(n) => {
            s.flow = if n > 1 {
                Flow::Break(n - 1)
            } else {
                Flow::None
            };
            Some(true)
        }
        Flow::Continue(n) => {
            if n > 1 {
                s.flow = Flow::Continue(n - 1);
                Some(true)
            } else {
                s.flow = Flow::None;
                Some(false)
            }
        }
        _ => Some(true),
    })
}

fn exec_loop(cond: &Node, body: &Node, until: bool) -> i32 {
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    loop {
        suppress_errexit(1);
        let c = exec_node(cond);
        suppress_errexit(-1);
        match loop_control() {
            Some(true) => break,
            Some(false) => continue,
            None => {}
        }
        if (c == 0) == until {
            break;
        }
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

fn exec_for(vars: &[String], words: Option<&[String]>, body: &Node) -> i32 {
    let items = match words {
        Some(ws) => match expand_words(ws) {
            Ok(v) => v,
            Err(e) => return expansion_failed(e),
        },
        None => with_shell(|s| s.pos_params.clone()),
    };
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    for chunk in items.chunks(vars.len().max(1)) {
        let ok = vars
            .iter()
            .enumerate()
            .all(|(k, v)| var_set(v, chunk.get(k).map(|s| s.as_str()).unwrap_or(""), false));
        if !ok {
            status = 1;
            break;
        }
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

// 0 if the expression is non-zero, 1 if zero, 2 on error.
fn arith_status(expr: &str) -> i32 {
    match crate::expand::eval_arith(expr) {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(_) => 2,
    }
}

fn exec_for_arith(init: &str, cond: &str, step: &str, body: &Node) -> i32 {
    if !init.is_empty() && crate::expand::eval_arith(init).is_err() {
        return 1;
    }
    let mut status = 0;
    with_shell(|s| s.loop_depth += 1);
    loop {
        if !cond.is_empty() {
            match crate::expand::eval_arith(cond) {
                Ok(false) => break,
                Ok(true) => {}
                Err(_) => {
                    status = 1;
                    break;
                }
            }
        }
        status = exec_node(body);
        if loop_control() == Some(true) {
            break;
        }
        crate::trap::run_pending();
        if !step.is_empty() && crate::expand::eval_arith(step).is_err() {
            status = 1;
            break;
        }
    }
    with_shell(|s| s.loop_depth -= 1);
    status
}

fn exec_case(word: &str, items: &[CaseItem]) -> i32 {
    let w = match expand_string(word) {
        Ok(w) => w,
        Err(e) => return expansion_failed(e),
    };
    let mut status = 0;
    let mut falling = false;
    for item in items {
        if !falling {
            let mut matched = false;
            for p in &item.patterns {
                let pat = match expand_pattern(p) {
                    Ok(p) => p,
                    Err(e) => return expansion_failed(e),
                };
                if crate::pattern::matches(&pat, &w) {
                    matched = true;
                    break;
                }
            }
            if !matched {
                continue;
            }
        }
        status = match &item.body {
            Some(b) => exec_node(b),
            None => 0,
        };
        if !item.fallthrough || flow_pending() {
            break;
        }
        falling = true;
    }
    status
}

// ---- running source text --------------------------------------------------

/// Parses and runs `src` one complete command at a time, so aliases
/// defined by earlier commands apply to later ones. Used for `-c`,
/// `eval`, `.`, and trap actions.
pub fn run_string(src: &str) -> i32 {
    run_string_named(src, None)
}

/// Like `run_string`; syntax errors name `file` and the line.
pub fn run_string_named(src: &str, file: Option<&str>) -> i32 {
    let mut parser = Parser::new(src, true);
    let mut status = 0;
    loop {
        if flow_pending() {
            break;
        }
        match parser.parse_complete_command() {
            Ok(Some(node)) => {
                status = exec_node(&node);
                crate::trap::run_pending();
            }
            Ok(None) => break,
            Err(e) => {
                let msg = match e {
                    ParseError::Syntax(m) => m,
                    ParseError::Incomplete => "syntax error: unexpected end of file".to_string(),
                };
                match file {
                    Some(f) => err_line(&format!("meowsh: {}:{}: {}", f, parser.line(), msg)),
                    None => err_line(&format!("meowsh: {}", msg)),
                }
                status = 2;
                with_shell(|s| s.last_status = 2);
                break;
            }
        }
    }
    status
}

/// Runs a command line as typed interactively (used by tests).
pub fn execute_line(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if interactive() {
        history_add(line);
    }
    run_string(line);
}

pub fn history_add(line: &str) {
    with_shell(|s| {
        let mut h = s.history.lock().unwrap_or_else(|e| e.into_inner());
        h.push(line.to_string());
        let excess = h.len().saturating_sub(1000);
        if excess > 0 {
            h.drain(..excess);
        }
    });
}

/// Runs the output of `src` in a subshell and returns its standard output
/// and exit status.
pub fn command_subst(src: &str) -> (String, i32) {
    let (r, w) = match pipe_cloexec() {
        Ok(p) => p,
        Err(e) => {
            err_line(&format!("meowsh: pipe: {}", e));
            return (String::new(), 1);
        }
    };
    let pid = fork_process(ForkKind::Inline);
    if pid == 0 {
        unsafe {
            libc::close(r);
            libc::dup2(w, 1);
            libc::close(w);
        }
        let st = run_string(src);
        exit_child(st);
    }
    close(w);
    if pid < 0 {
        close(r);
        return (String::new(), 1);
    }
    let bytes = read_all(r);
    close(r);
    let status = loop {
        let mut st = 0;
        let rc = unsafe { libc::waitpid(pid, &mut st, 0) };
        if rc == pid {
            break status_to_exit(st);
        }
        if rc < 0 && last_errno() == libc::EINTR {
            continue;
        }
        break 127;
    };
    (String::from_utf8_lossy(&bytes).into_owned(), status)
}

// Descriptors handed out by process substitution, closed once the
// command that expanded them finishes.
static PROCSUB_FDS: std::sync::Mutex<Vec<i32>> = std::sync::Mutex::new(Vec::new());

fn procsub_mark() -> usize {
    PROCSUB_FDS.lock().unwrap_or_else(|e| e.into_inner()).len()
}

fn procsub_close_from(mark: usize) {
    let fds: Vec<i32> = {
        let mut v = PROCSUB_FDS.lock().unwrap_or_else(|e| e.into_inner());
        if v.len() <= mark {
            return;
        }
        v.drain(mark..).collect()
    };
    for fd in fds {
        close(fd);
    }
}

/// `<(src)` (`read` true) or `>(src)`: runs `src` in the background
/// connected to a pipe and returns a `/dev/fd` path for the other end.
pub fn process_subst(src: &str, read: bool) -> Option<String> {
    let (r, w) = match pipe_cloexec() {
        Ok(p) => p,
        Err(e) => {
            err_line(&format!("meowsh: pipe: {}", e));
            return None;
        }
    };
    let (mine, theirs) = if read { (r, w) } else { (w, r) };
    let pid = fork_process(ForkKind::Inline);
    if pid == 0 {
        unsafe {
            libc::close(mine);
            libc::dup2(theirs, if read { 1 } else { 0 });
            libc::close(theirs);
        }
        let st = run_string(src);
        exit_child(st);
    }
    close(theirs);
    if pid < 0 {
        close(mine);
        return None;
    }
    // The path must stay valid across exec, so the descriptor is
    // inheritable; it is closed in the shell when the command ends.
    let fd = dup_high(mine);
    close(mine);
    if fd < 0 {
        return None;
    }
    unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
    PROCSUB_FDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(fd);
    Some(format!("/dev/fd/{}", fd))
}

/// One line of input for `run_input`.
pub enum InputLine {
    Line(String),
    Eof,
    Interrupted,
}

/// Reads and runs commands until end of input. `next` supplies lines
/// (its argument is true when a command is incomplete and a continuation
/// prompt is due); `on_command` receives each complete command's text in
/// an interactive shell. Returns the shell's exit status.
pub fn run_input(mut next: impl FnMut(bool) -> InputLine, mut on_command: impl FnMut(&str)) -> i32 {
    let mut buf = String::new();
    loop {
        let is_interactive = interactive();
        if is_interactive && buf.is_empty() {
            reap_jobs(true);
        }
        let eof = match next(!buf.is_empty()) {
            InputLine::Line(l) => {
                if opt_set(OPT_VERBOSE) {
                    let _ = crate::sys::write_fd(2, l.as_bytes());
                }
                buf.push_str(&l);
                false
            }
            InputLine::Eof => true,
            InputLine::Interrupted => {
                buf.clear();
                with_shell(|s| s.last_status = 130);
                continue;
            }
        };
        loop {
            if buf.trim().is_empty() {
                buf.clear();
                break;
            }
            let mut parser = Parser::new(&buf, eof);
            match parser.parse_complete_command() {
                Ok(Some(node)) => {
                    let rest = parser.rest();
                    let text = if buf.ends_with(&rest) {
                        buf[..buf.len() - rest.len()].trim().to_string()
                    } else {
                        buf.trim().to_string()
                    };
                    buf = rest;
                    if is_interactive {
                        on_command(&text);
                        crate::builtins::run_hooks("preexec", &[text.clone(), text.clone(), text]);
                    }
                    exec_node(&node);
                    crate::trap::run_pending();
                    match flow() {
                        Flow::Exit(n) => return n,
                        Flow::None => {}
                        Flow::Interrupt => {
                            set_flow(Flow::None);
                            buf.clear();
                            break;
                        }
                        _ => set_flow(Flow::None),
                    }
                    if !is_interactive {
                        reap_jobs(false);
                    }
                }
                Ok(None) => {
                    buf.clear();
                    break;
                }
                Err(ParseError::Incomplete) => break,
                Err(ParseError::Syntax(msg)) => {
                    err_line(&format!("meowsh: {}", msg));
                    with_shell(|s| s.last_status = 2);
                    buf.clear();
                    if !is_interactive {
                        return 2;
                    }
                    break;
                }
            }
        }
        if eof {
            break;
        }
    }
    last_status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::SHELL;
    use crate::TEST_LOCK;

    #[test]
    fn test_execute_line_history() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = true;
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
        }

        execute_line(": ls -l");
        execute_line(": echo hello");

        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = false;
            let history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(history.len(), 2);
            assert_eq!(history[0], ": ls -l");
            assert_eq!(history[1], ": echo hello");
        }
    }

    #[test]
    fn test_execute_line_history_non_interactive() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = false;
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
        }

        execute_line(": ls -l");

        {
            let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            let history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(history.len(), 0);
        }
    }

    fn reset_shell() {
        let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.vars.clear();
        shell.arrays.clear();
        shell.assoc_arrays.clear();
        shell.unique_arrays.clear();
        shell.zstyles.clear();
        shell.functions.clear();
        shell.local_scopes.clear();
        shell.alias_map(false).clear();
        shell.pos_params.clear();
        shell.last_status = 0;
        shell.loop_depth = 0;
        shell.func_depth = 0;
        shell.flow = Flow::None;
        shell.interactive = true;
        shell.opts = 0;
    }

    fn var(name: &str) -> Option<String> {
        let shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
        shell.vars.get(name).map(|v| v.value.clone())
    }

    #[test]
    fn test_local_outside_function_acts_like_typeset() {
        // zsh accepts `local` at the top level of a sourced file.
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("local x=1");
        assert_eq!(last_status(), 0);
        assert_eq!(var("x").as_deref(), Some("1"));
    }

    #[test]
    fn test_local_does_not_leak_after_return() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("function f { local x=inner; }");
        execute_line("f");
        assert!(
            var("x").is_none(),
            "x should not exist after function returns"
        );
    }

    #[test]
    fn test_local_restores_prior_global() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function f { local x=inner; }");
        execute_line("f");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_bare_local_masks_with_empty_then_restores() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function f { local x; x=inside; }");
        execute_line("f");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_local_repeated_in_same_scope_keeps_original_snapshot() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function f { local x=first; local x=second; }");
        execute_line("f");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_nested_functions_have_independent_scopes() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("x=outer");
        execute_line("function inner { local x=inner_val; }");
        execute_line("function outer { local x=outer_val; inner; }");
        execute_line("outer");
        assert_eq!(var("x").as_deref(), Some("outer"));
    }

    #[test]
    fn test_local_creates_var_that_did_not_exist() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("function f { local newvar=hi; }");
        execute_line("f");
        assert!(var("newvar").is_none());
    }

    #[test]
    fn test_assignment_without_local_is_global() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("function f { y=set_inside; }");
        execute_line("f");
        assert_eq!(var("y").as_deref(), Some("set_inside"));
    }

    #[test]
    fn test_return_sets_function_status() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("f() { return 3; echo unreachable; }; f; r=$?");
        assert_eq!(var("r").as_deref(), Some("3"));
    }

    #[test]
    fn test_and_or_is_left_associative_at_runtime() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("false && x=a || x=b");
        assert_eq!(var("x").as_deref(), Some("b"));
    }

    #[test]
    fn test_bang_negates_status() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_shell();
        execute_line("! false; a=$?; ! true; b=$?");
        assert_eq!(var("a").as_deref(), Some("0"));
        assert_eq!(var("b").as_deref(), Some("1"));
    }

    #[test]
    fn test_execute_line_history_limit() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = true;
            let mut history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            history.clear();
            for i in 0..1000 {
                history.push(format!("cmd{}", i));
            }
        }

        execute_line(": new_cmd");

        {
            let mut shell = SHELL.shell.lock().unwrap_or_else(|e| e.into_inner());
            shell.interactive = false;
            let history = shell.history.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(history.len(), 1000);
            assert_eq!(history[999], ": new_cmd");
            assert_eq!(history[0], "cmd1");
        }
    }
}
