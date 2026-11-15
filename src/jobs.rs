use crate::shell::{builtin_err, with_shell};
use crate::sys::{err_line, out};
use crate::types::{Job, JobState, ProcState, Process};
use std::sync::atomic::{AtomicI32, Ordering};

/// Descriptor for the controlling terminal while job control is on.
pub static TTY_FD: AtomicI32 = AtomicI32::new(-1);

pub fn status_to_exit(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else if libc::WIFSTOPPED(status) {
        128 + libc::WSTOPSIG(status)
    } else {
        0
    }
}

fn job_control() -> bool {
    with_shell(|s| s.job_control)
}

fn give_terminal_to(pgid: i32) {
    let fd = TTY_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::tcsetpgrp(fd, pgid);
        }
    }
}

fn reclaim_terminal() {
    let pgid = with_shell(|s| s.shell_pgid);
    give_terminal_to(pgid);
}

/// Adds a job and returns its number.
pub fn add_job(pgid: i32, pids: &[i32], state: JobState, text: &str) -> i32 {
    with_shell(|s| {
        let mut next = s.next_job_id.lock().unwrap_or_else(|e| e.into_inner());
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.is_empty() {
            *next = 1;
        }
        let id = *next;
        *next += 1;
        let proc_state = match state {
            JobState::Stopped => ProcState::Stopped,
            JobState::Running => ProcState::Running,
            JobState::Done => ProcState::Done,
        };
        jobs.push(Job {
            id,
            pgid,
            procs: pids
                .iter()
                .map(|&pid| Process {
                    pid,
                    status: 0,
                    state: proc_state,
                    cmd: text.to_string(),
                })
                .collect(),
            state,
            notified: false,
            foreground: false,
            cmd_text: text.to_string(),
        });
        id
    })
}

pub fn job_update_status(pid: i32, status: i32) {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in jobs.iter_mut() {
            if let Some(proc) = job.procs.iter_mut().find(|p| p.pid == pid) {
                if libc::WIFSTOPPED(status) {
                    proc.state = ProcState::Stopped;
                    proc.status = status;
                } else if libc::WIFCONTINUED(status) {
                    proc.state = ProcState::Running;
                } else {
                    proc.state = ProcState::Done;
                    proc.status = status;
                }
                job.state = if job.procs.iter().all(|p| p.state == ProcState::Done) {
                    JobState::Done
                } else if job.procs.iter().any(|p| p.state == ProcState::Stopped) {
                    JobState::Stopped
                } else {
                    JobState::Running
                };
                job.notified = false;
                return;
            }
        }
    })
}

/// Collects status changes of background children without blocking.
/// With `notify`, finished jobs are reported and forgotten.
pub fn reap_jobs(notify: bool) {
    loop {
        let mut status = 0;
        let pid = unsafe {
            libc::waitpid(
                -1,
                &mut status,
                libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
            )
        };
        if pid <= 0 {
            break;
        }
        job_update_status(pid, status);
    }
    if notify {
        let done: Vec<(i32, String)> = with_shell(|s| {
            let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let done = jobs
                .iter()
                .filter(|j| j.state == JobState::Done)
                .map(|j| (j.id, j.cmd_text.clone()))
                .collect();
            jobs.retain(|j| j.state != JobState::Done);
            done
        });
        for (id, text) in done {
            err_line(&format!("[{}]+  Done\t\t{}", id, text));
        }
    }
}

fn waitpid_retry(pid: i32, flags: i32) -> Option<i32> {
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, flags) };
        if r == pid {
            return Some(status);
        }
        if r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return None;
    }
}

/// Waits for a foreground pipeline. Returns one exit code per process.
/// A stopped pipeline becomes a background job.
pub fn wait_foreground(pids: &[i32], pgid: i32, text: &str) -> Vec<i32> {
    let jc = job_control();
    let flags = if jc { libc::WUNTRACED } else { 0 };
    let mut codes = vec![0; pids.len()];
    let mut stopped_at = None;
    for (k, &pid) in pids.iter().enumerate() {
        match waitpid_retry(pid, flags) {
            Some(st) => {
                codes[k] = status_to_exit(st);
                if libc::WIFSTOPPED(st) {
                    stopped_at = Some(k);
                    break;
                }
                if libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGINT {
                    with_shell(|s| {
                        if s.interactive {
                            s.flow = crate::types::Flow::Interrupt;
                        }
                    });
                }
            }
            None => codes[k] = 127,
        }
    }
    if jc {
        reclaim_terminal();
    }
    if let Some(k) = stopped_at {
        let id = add_job(pgid, pids, JobState::Stopped, text);
        with_shell(|s| {
            let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
                for (i, p) in job.procs.iter_mut().enumerate() {
                    if i < k {
                        p.state = ProcState::Done;
                        p.status = codes[i] << 8;
                    }
                }
                job.notified = true;
            }
        });
        err_line(&format!("\n[{}]+  Stopped\t\t{}", id, text));
        let stopped_code = codes[k];
        for c in codes.iter_mut().skip(k + 1) {
            *c = stopped_code;
        }
    }
    codes
}

// Waits for every unfinished process of a job brought to the foreground.
fn wait_job_foreground(id: i32) -> i32 {
    let pids: Vec<i32> = with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .find(|j| j.id == id)
            .map(|j| {
                j.procs
                    .iter()
                    .filter(|p| p.state != ProcState::Done)
                    .map(|p| p.pid)
                    .collect()
            })
            .unwrap_or_default()
    });
    let mut stopped = false;
    for pid in pids {
        if let Some(st) = waitpid_retry(pid, libc::WUNTRACED) {
            job_update_status(pid, st);
            if libc::WIFSTOPPED(st) {
                stopped = true;
                break;
            }
        }
    }
    reclaim_terminal();
    let (status, text) = job_result(id);
    if stopped {
        err_line(&format!("\n[{}]+  Stopped\t\t{}", id, text));
    } else {
        remove_job(id);
    }
    status
}

fn job_result(id: i32) -> (i32, String) {
    with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .find(|j| j.id == id)
            .map(|j| {
                let st = j
                    .procs
                    .last()
                    .map(|p| status_to_exit(p.status))
                    .unwrap_or(0);
                (st, j.cmd_text.clone())
            })
            .unwrap_or((0, String::new()))
    })
}

fn remove_job(id: i32) {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.retain(|j| j.id != id);
    })
}

fn parse_job_id(args: &[String]) -> Result<Option<i32>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    if args.len() > 1 {
        return Err("too many arguments".to_string());
    }

    let spec = args[0].strip_prefix('%').unwrap_or(&args[0]);
    if spec.is_empty() || spec == "+" || spec == "%" {
        return Ok(None);
    }

    spec.parse::<i32>()
        .map(Some)
        .map_err(|_| format!("no such job: {}", args[0]))
}

fn find_job_index(jobs: &[Job], requested_id: Option<i32>) -> Option<usize> {
    match requested_id {
        Some(id) => jobs
            .iter()
            .position(|job| job.id == id && job.state != JobState::Done),
        None => jobs.iter().rposition(|job| job.state != JobState::Done),
    }
}

fn no_current_job(command: &str, requested_id: Option<i32>) -> i32 {
    match requested_id {
        Some(id) => err_line(&format!("meowsh: {}: no such job: %{}", command, id)),
        None => err_line(&format!("meowsh: {}: no current job", command)),
    }
    1
}

// Resolves a job spec to (id, pgid, text).
fn select_job(command: &str, args: &[String]) -> Result<(i32, i32, String), i32> {
    reap_jobs(false);
    let requested = match parse_job_id(args) {
        Ok(id) => id,
        Err(e) => {
            builtin_err(command, &e.to_string());
            return Err(1);
        }
    };
    let found = with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        find_job_index(&jobs, requested)
            .map(|i| (jobs[i].id, jobs[i].pgid, jobs[i].cmd_text.clone()))
    });
    found.ok_or_else(|| no_current_job(command, requested))
}

fn set_job_running(id: i32) {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.iter_mut().find(|j| j.id == id) {
            job.state = JobState::Running;
            for p in job.procs.iter_mut() {
                if p.state == ProcState::Stopped {
                    p.state = ProcState::Running;
                }
            }
        }
    })
}

pub fn builtin_jobs(args: &[String]) -> i32 {
    reap_jobs(false);
    let pids_only = args.iter().any(|a| a == "-p");
    let long = args.iter().any(|a| a == "-l");
    let jobs: Vec<Job> = with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let snapshot = jobs.clone();
        jobs.retain(|j| j.state != JobState::Done);
        snapshot
    });
    for job in jobs {
        if pids_only {
            out(&format!("{}\n", job.pgid));
            continue;
        }
        let state = match job.state {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
            JobState::Done => "Done",
        };
        if long {
            out(&format!(
                "[{}]+ {} {}\t\t{}\n",
                job.id, job.pgid, state, job.cmd_text
            ));
        } else {
            out(&format!("[{}]+  {}\t\t{}\n", job.id, state, job.cmd_text));
        }
    }
    0
}

pub fn builtin_fg(args: &[String]) -> i32 {
    if !job_control() {
        builtin_err("fg", "no job control");
        return 1;
    }
    let (id, pgid, text) = match select_job("fg", args) {
        Ok(j) => j,
        Err(st) => return st,
    };
    out(&format!("{}\n", text));
    set_job_running(id);
    give_terminal_to(pgid);
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
    wait_job_foreground(id)
}

pub fn builtin_bg(args: &[String]) -> i32 {
    if !job_control() {
        builtin_err("bg", "no job control");
        return 1;
    }
    let (id, pgid, text) = match select_job("bg", args) {
        Ok(j) => j,
        Err(st) => return st,
    };
    set_job_running(id);
    unsafe {
        libc::kill(-pgid, libc::SIGCONT);
    }
    out(&format!("[{}]+ {} &\n", id, text));
    0
}

// Waits for one process, using a status already collected by `reap_jobs`
// when there is one.
fn wait_pid(pid: i32) -> i32 {
    let known = with_shell(|s| {
        let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.iter()
            .flat_map(|j| j.procs.iter())
            .find(|p| p.pid == pid)
            .map(|p| (p.state, p.status))
    });
    match known {
        Some((ProcState::Done, st)) => status_to_exit(st),
        Some(_) => match waitpid_retry(pid, 0) {
            Some(st) => {
                job_update_status(pid, st);
                status_to_exit(st)
            }
            None => 127,
        },
        None => match waitpid_retry(pid, 0) {
            Some(st) => status_to_exit(st),
            None => 127,
        },
    }
}

fn forget_done_jobs() {
    with_shell(|s| {
        let mut jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.retain(|j| j.state != JobState::Done);
    })
}

pub fn builtin_wait(args: &[String]) -> i32 {
    if args.is_empty() {
        let pids: Vec<i32> = with_shell(|s| {
            let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.iter()
                .flat_map(|j| j.procs.iter())
                .filter(|p| p.state != ProcState::Done)
                .map(|p| p.pid)
                .collect()
        });
        for pid in pids {
            wait_pid(pid);
        }
        forget_done_jobs();
        return 0;
    }
    let mut status = 0;
    for arg in args {
        if let Some(spec) = arg.strip_prefix('%') {
            let pids: Option<Vec<i32>> = spec.parse::<i32>().ok().and_then(|id| {
                with_shell(|s| {
                    let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
                    jobs.iter()
                        .find(|j| j.id == id)
                        .map(|j| j.procs.iter().map(|p| p.pid).collect())
                })
            });
            match pids {
                Some(pids) => {
                    for pid in pids {
                        status = wait_pid(pid);
                    }
                }
                None => {
                    builtin_err("wait", &format!("{}: no such job", arg));
                    status = 127;
                }
            }
        } else {
            match arg.parse::<i32>() {
                Ok(pid) if pid > 0 => status = wait_pid(pid),
                _ => {
                    err_line(&format!(
                        "meowsh: wait: `{}': not a pid or valid job spec",
                        arg
                    ));
                    status = 2;
                }
            }
        }
    }
    forget_done_jobs();
    status
}

/// Target process(es) for `kill`: a pid, or a job spec.
pub fn kill_targets(spec: &str) -> Result<Vec<i32>, String> {
    if let Some(job) = spec.strip_prefix('%') {
        let id = if job.is_empty() || job == "+" || job == "%" {
            None
        } else {
            Some(
                job.parse::<i32>()
                    .map_err(|_| format!("{}: no such job", spec))?,
            )
        };
        let jc = job_control();
        return with_shell(|s| {
            let jobs = s.jobs.lock().unwrap_or_else(|e| e.into_inner());
            let idx = find_job_index(&jobs, id).ok_or_else(|| format!("{}: no such job", spec))?;
            let job = &jobs[idx];
            Ok(if jc {
                vec![-job.pgid]
            } else {
                job.procs.iter().map(|p| p.pid).collect()
            })
        });
    }
    spec.parse::<i32>()
        .map(|p| vec![p])
        .map_err(|_| format!("{}: arguments must be process or job IDs", spec))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_job(id: i32, state: JobState) -> Job {
        Job {
            id,
            pgid: id,
            procs: vec![Process {
                pid: id,
                status: 0,
                state: ProcState::Running,
                cmd: format!("cmd{id}"),
            }],
            state,
            notified: false,
            foreground: false,
            cmd_text: format!("cmd{id}"),
        }
    }

    #[test]
    fn parse_job_id_accepts_default_and_percent_specs() {
        assert_eq!(parse_job_id(&[]).unwrap(), None);
        assert_eq!(parse_job_id(&["%".to_string()]).unwrap(), None);
        assert_eq!(parse_job_id(&["%+".to_string()]).unwrap(), None);
        assert_eq!(parse_job_id(&["%2".to_string()]).unwrap(), Some(2));
        assert_eq!(parse_job_id(&["3".to_string()]).unwrap(), Some(3));
        assert!(parse_job_id(&["%abc".to_string()]).is_err());
        assert!(parse_job_id(&["%1".to_string(), "%2".to_string()]).is_err());
    }

    #[test]
    fn status_to_exit_reports_stopped_signal_status() {
        let status = libc::W_STOPCODE(libc::SIGTSTP);

        assert_eq!(status_to_exit(status), 128 + libc::SIGTSTP);
    }

    #[test]
    fn find_job_index_defaults_to_latest_non_done_job() {
        let jobs = vec![
            test_job(1, JobState::Stopped),
            test_job(2, JobState::Done),
            test_job(3, JobState::Running),
        ];

        assert_eq!(find_job_index(&jobs, None), Some(2));
        assert_eq!(find_job_index(&jobs, Some(1)), Some(0));
        assert_eq!(find_job_index(&jobs, Some(2)), None);
        assert_eq!(find_job_index(&jobs, Some(99)), None);
    }
}
