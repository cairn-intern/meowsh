use meowsh::builtins::source_file;
use meowsh::exec::{exit_shell, flow, run_string, set_flow, strerror};
use meowsh::jobs::TTY_FD;
use meowsh::lineedit::{run_fd, run_interactive};
use meowsh::options::{self, Emulation};
use meowsh::shell::{shell_init, with_shell};
use meowsh::types::Flow;
use std::env;
use std::path::Path;
use std::sync::atomic::Ordering;

#[derive(Debug, Default, PartialEq)]
struct Args {
    command: Option<String>,
    script: Option<String>,
    arg0: Option<String>,
    positional: Vec<String>,
    interactive: Option<bool>,
    login: bool,
    // (option, value) pairs from -e, +x, -o name, ...
    options: Vec<(&'static str, bool)>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        login: args.first().map(|a| a.starts_with('-')).unwrap_or(false),
        ..Args::default()
    };
    let mut command_mode = false;
    let mut stdin_mode = false;
    let mut i = 1;
    // `--emulate mode` is handled before parsing (it decides the letters).
    if args.get(1).map(|a| a.as_str()) == Some("--emulate") {
        i = 3;
    }
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        if a == "--login" {
            out.login = true;
            i += 1;
            continue;
        }
        if !(a.starts_with('-') || a.starts_with('+')) || a.len() < 2 {
            break;
        }
        let on = a.starts_with('-');
        for c in a[1..].chars() {
            match c {
                'c' => command_mode = true,
                'i' => out.interactive = Some(on),
                'l' => out.login = true,
                's' => stdin_mode = true,
                'o' => {
                    i += 1;
                    let name = args.get(i).ok_or("-o: option name required")?;
                    let (opt, v) = options::resolve(name)
                        .ok_or_else(|| format!("no such option: {}", name))?;
                    out.options.push((opt, if on { v } else { !v }));
                }
                _ => match options::letter(c) {
                    Some((opt, v)) => out.options.push((opt, if on { v } else { !v })),
                    None => return Err(format!("-{}: invalid option", c)),
                },
            }
        }
        i += 1;
    }
    if command_mode {
        let cmd = args.get(i).ok_or("-c: option requires an argument")?;
        out.command = Some(cmd.clone());
        i += 1;
        if let Some(a0) = args.get(i) {
            out.arg0 = Some(a0.clone());
            i += 1;
        }
    } else if !stdin_mode && i < args.len() {
        out.script = Some(args[i].clone());
        out.arg0 = Some(args[i].clone());
        i += 1;
    }
    out.positional = args[i.min(args.len())..].to_vec();
    Ok(out)
}

fn main() {
    shell_init();
    let argv: Vec<String> = env::args().collect();
    // As zsh does: emulate the shell named by argv[0] (sh, ksh, ...) or
    // by `--emulate mode`; otherwise zsh.
    let emu = match (argv.get(1).map(|a| a.as_str()), argv.get(2)) {
        (Some("--emulate"), Some(m)) => Emulation::from_name(m).unwrap_or(Emulation::Zsh),
        _ => argv
            .first()
            .and_then(|a0| Emulation::from_name(a0))
            .filter(|e| *e != Emulation::Csh)
            .unwrap_or(Emulation::Zsh),
    };
    with_shell(|s| options::init(s, emu));
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("meowsh: {}", e);
            std::process::exit(2);
        }
    };

    let tty_in = unsafe { libc::isatty(0) == 1 };
    let tty_err = unsafe { libc::isatty(2) == 1 };
    let interactive = args
        .interactive
        .unwrap_or(args.command.is_none() && args.script.is_none() && tty_in && tty_err);

    with_shell(|s| {
        s.interactive = interactive;
        s.login_shell = args.login;
        s.argv0 = args.arg0.clone().unwrap_or_else(|| {
            argv.first()
                .cloned()
                .unwrap_or_else(|| "meowsh".to_string())
        });
        s.pos_params = args.positional.clone();
        if let Some(script) = &args.script {
            s.script_name = script.clone();
        }
        for (opt, on) in &args.options {
            options::set_in(s, opt, *on);
        }
        options::set_in(s, "interactive", interactive);
        options::set_in(s, "login", args.login);
        options::set_in(
            s,
            "shinstdin",
            args.command.is_none() && args.script.is_none(),
        );
    });

    if interactive && tty_in {
        init_job_control();
    }
    meowsh::trap::init_shell_signals();

    if options::is_set("rcs") {
        source_startup_files(args.login, interactive);
    }

    let status = if let Some(cmd) = &args.command {
        run_string(cmd)
    } else if let Some(path) = &args.script {
        match open_script(path) {
            Ok(fd) => run_fd(fd, false),
            Err(e) => {
                eprintln!("meowsh: {}: {}", path, strerror(e));
                exit_shell(if e == libc::ENOENT { 127 } else { 126 });
            }
        }
    } else if interactive {
        run_interactive()
    } else {
        run_fd(0, false)
    };
    let status = match flow() {
        Flow::Exit(n) => n,
        Flow::None => status,
        _ => with_shell(|s| s.last_status),
    };
    exit_shell(status);
}

// Opens a script on a descriptor >= 10 so the script's own redirections
// of low descriptors cannot disturb reading it.
fn open_script(path: &str) -> Result<i32, i32> {
    let c = std::ffi::CString::new(path).map_err(|_| libc::EINVAL)?;
    let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    let high = meowsh::sys::dup_high(fd);
    meowsh::sys::close(fd);
    if high < 0 {
        return Err(errno());
    }
    Ok(high)
}

fn init_job_control() {
    unsafe {
        let fd = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 10);
        if fd < 0 {
            return;
        }
        // Wait until the shell is in the foreground before taking over.
        loop {
            let fg = libc::tcgetpgrp(fd);
            let me = libc::getpgrp();
            if fg < 0 || fg == me {
                break;
            }
            libc::kill(-me, libc::SIGTTIN);
        }
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        let pid = libc::getpid();
        libc::setpgid(pid, pid);
        let pgid = libc::getpgrp();
        if libc::tcsetpgrp(fd, pgid) != 0 {
            libc::close(fd);
            return;
        }
        TTY_FD.store(fd, Ordering::SeqCst);
        with_shell(|s| {
            s.job_control = true;
            s.shell_pgid = pgid;
            options::set_in(s, "monitor", true);
        });
    }
}

fn source_startup_files(login: bool, interactive: bool) {
    let home = meowsh::shell::var_get_opt("HOME").unwrap_or_default();
    if home.is_empty() {
        return;
    }
    let home_path = Path::new(&home);
    // zsh order: .zshenv always, .zprofile for login shells, .zshrc for
    // interactive shells, .zlogin for interactive login shells.
    source_startup(&home_path.join(".zshenv").to_string_lossy());
    if login {
        source_startup(&home_path.join(".zprofile").to_string_lossy());
    }
    if interactive {
        source_startup(&home_path.join(".zshrc").to_string_lossy());
    }
    if login && interactive {
        source_startup(&home_path.join(".zlogin").to_string_lossy());
    }
}

fn source_startup(path: &str) {
    if !Path::new(path).is_file() {
        return;
    }
    source_file(path, None);
    if !matches!(flow(), Flow::Exit(_)) {
        set_flow(Flow::None);
    }
    if let Flow::Exit(n) = flow() {
        exit_shell(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_args_interactive() {
        let a = parse_args(&v(&["meowsh", "-i"])).unwrap();
        assert_eq!(a.interactive, Some(true));
        assert_eq!(a.command, None);
    }

    #[test]
    fn test_parse_args_command_with_arg0_and_params() {
        let a = parse_args(&v(&["meowsh", "-c", "echo $0 $1", "name", "one"])).unwrap();
        assert_eq!(a.command.as_deref(), Some("echo $0 $1"));
        assert_eq!(a.arg0.as_deref(), Some("name"));
        assert_eq!(a.positional, v(&["one"]));
    }

    #[test]
    fn test_parse_args_both() {
        let a = parse_args(&v(&["meowsh", "-i", "-c", "ls"])).unwrap();
        assert_eq!(a.interactive, Some(true));
        assert_eq!(a.command.as_deref(), Some("ls"));
    }

    #[test]
    fn test_parse_args_script_file() {
        let a = parse_args(&v(&["meowsh", "-e", "script.sh", "x", "y"])).unwrap();
        assert_eq!(a.script.as_deref(), Some("script.sh"));
        assert_eq!(a.positional, v(&["x", "y"]));
        assert_eq!(a.options, vec![("errexit", true)]);
    }

    #[test]
    fn test_parse_args_stdin_mode() {
        let a = parse_args(&v(&["meowsh", "-s", "a", "b"])).unwrap();
        assert_eq!(a.script, None);
        assert_eq!(a.positional, v(&["a", "b"]));
    }

    #[test]
    fn test_parse_args_none() {
        assert_eq!(parse_args(&v(&["meowsh"])).unwrap(), Args::default());
    }

    #[test]
    fn test_parse_args_rejects_unknown_option() {
        assert!(parse_args(&v(&["meowsh", "-j"])).is_err());
        assert!(parse_args(&v(&["meowsh", "-c"])).is_err());
    }
}
