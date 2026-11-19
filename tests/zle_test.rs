//! meowsh's own line editor (ZLE), driven through a pseudo-terminal.

use std::process::{Command, Stdio};
use tempfile::TempDir;

// Starts an interactive meowsh using ZLE with `zshrc`, types `input`
// and returns everything it wrote.
fn on_terminal(zshrc: &str, input: &str) -> String {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let (mut master, mut slave) = (0, 0);
    let mut ws = libc::winsize {
        ws_row: 10,
        ws_col: 60,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws as *mut _ as _,
        )
    };
    assert_eq!(ok, 0, "openpty");
    let tty = |fd: i32| unsafe { Stdio::from_raw_fd(libc::dup(fd)) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .env("TERM", "xterm")
        .env("MEOWSH_ZLE", "1")
        .arg("-i")
        .stdin(tty(slave))
        .stdout(tty(slave))
        .stderr(tty(slave))
        .spawn()
        .unwrap();
    unsafe { libc::close(slave) };
    let mut m = unsafe { std::fs::File::from_raw_fd(master) };
    std::thread::sleep(std::time::Duration::from_millis(500));
    m.write_all(input.as_bytes()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let _ = child.kill();
    let _ = child.wait();
    let mut out = Vec::new();
    unsafe {
        let flags = libc::fcntl(master, libc::F_GETFL);
        libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut buf = [0u8; 4096];
    while let Ok(n) = m.read(&mut buf) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

const REPORT: &str = "KEYTIMEOUT=1\n\
report() { print -r \"[B=$BUFFER C=$CURSOR]\"; BUFFER=; zle accept-line }\n\
zle -N report\nbindkey '^X' report\nbindkey -a '^X' report\n";

#[test]
fn vi_change_command_edits_the_line() {
    let out = on_terminal(&format!("bindkey -v\n{}", REPORT), "goox\x1bcld\x18");
    assert!(out.contains("[B=good C=4]"), "output: {:?}", out);
}

#[test]
fn widgets_see_and_set_the_buffer() {
    let rc = format!(
        "{}up() {{ LBUFFER=${{LBUFFER:u}} }}\nzle -N up\nbindkey '^U' up\n",
        REPORT
    );
    let out = on_terminal(&rc, "ab cd\x02\x02\x15\x18");
    assert!(out.contains("[B=AB cd C=3]"), "output: {:?}", out);
}

#[test]
fn send_string_bindings_are_read_as_input() {
    let rc = format!("bindkey -s '^Y' 'foo'\n{}", REPORT);
    let out = on_terminal(&rc, "\x19\x18");
    assert!(out.contains("[B=foo C=3]"), "output: {:?}", out);
}
