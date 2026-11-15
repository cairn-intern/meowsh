//! zsh features that common startup files rely on: what `starship init
//! zsh`, `direnv hook zsh` and oh-my-zsh style configs emit.

use std::process::{Command, Stdio};
use tempfile::TempDir;

fn sh(script: &str) -> (i32, String, String) {
    let home = TempDir::new().unwrap();
    sh_home(script, &home)
}

fn sh_home(script: &str, home: &TempDir) -> (i32, String, String) {
    let o = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .args(["-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("run meowsh");
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
        String::from_utf8_lossy(&o.stderr).into_owned(),
    )
}

// Runs `input` through an interactive shell reading a pipe, so prompts
// and hooks run, with `zshrc` as the startup file.
fn interactive(zshrc: &str, input: &str) -> (i32, String) {
    use std::io::Write;
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    (
        o.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    )
}

#[test]
fn process_substitution() {
    let (_, out, err) = sh("source <(echo 'echo sourced'); cat <(echo a) <(echo b); \
         while read l; do echo \"got $l\"; done < <(printf 'x\\ny\\n'); \
         echo z > >(cat); wait");
    assert_eq!(out, "sourced\na\nb\ngot x\ngot y\nz\n", "stderr: {}", err);
}

#[test]
fn process_substitution_does_not_leak_descriptors() {
    let (_, out, _) = sh(
        "before=$(ls /proc/$$/fd | wc -l); cat <(echo a) <(echo b) >/dev/null; \
         after=$(ls /proc/$$/fd | wc -l); [ \"$before\" = \"$after\" ] && echo same",
    );
    assert_eq!(out, "same\n");
}

#[test]
fn brace_group_closes_without_semicolon() {
    let (_, out, _) = sh("f() { echo in f }; f; { echo grp }; echo }");
    assert_eq!(out, "in f\ngrp\n}\n");
}

#[test]
fn arithmetic_command_and_c_style_for() {
    let (_, out, _) = sh(
        "(( 2 > 1 )); echo $?; (( 0 )); echo $?; x=0; (( x++ )); echo $? $x; \
         for ((i = 0; i < 3; i++)); do echo $i; done; ((echo sub); echo b)",
    );
    assert_eq!(out, "0\n1\n1 1\n0\n1\n2\nsub\nb\n");
}

#[test]
fn float_arithmetic_and_math_functions() {
    let (_, out, _) = sh("echo $((1.5 * 2)) $((10 / 4.)) $((7 / 2)); \
         t=$(( int(rint(EPOCHREALTIME * 1000)) )); [ ${#t} -ge 13 ] && echo ms");
    assert_eq!(out, "3. 2.5 3\nms\n");
}

#[test]
fn subscript_search_flags() {
    let (_, out, _) = sh(
        "a=(x foo bar foo); echo ${a[(i)foo]} ${a[(I)foo]} ${a[(I)nope]} ${a[(i)nope]} \
         ${a[(r)b*]}; typeset -ag hooks; \
         if (( ! ${hooks[(I)h]} )); then hooks=(h $hooks); fi; echo ${hooks[@]}",
    );
    assert_eq!(out, "2 4 0 5 bar\nh\n");
}

#[test]
fn dbracket_grouped_patterns_and_subscript_v() {
    let (_, out, _) = sh("v=5.9; [[ $v == ([1-4]*) ]] && echo old || echo new; \
         [[ main.rs == *.(rs|toml) ]] && echo grp; \
         typeset -A w; [[ -v w[k] ]] || echo unset; w[k]=1; [[ -v w[k] ]] && echo set");
    assert_eq!(out, "new\ngrp\nunset\nset\n");
}

#[test]
fn dynamic_parameters() {
    let (_, out, _) = sh(
        "RANDOM=7; a=$RANDOM; RANDOM=7; [ $a = $RANDOM ] && echo seeded; \
         [ $EPOCHSECONDS -gt 0 ] && echo epoch; true | false | (exit 3); \
         echo ${pipestatus[@]}",
    );
    assert_eq!(out, "seeded\nepoch\n0 1 3\n");
}

#[test]
fn print_builtin() {
    let (_, out, _) = sh("print -n 'a\\tb'; print; print -l x y; print -r 'raw\\n'");
    assert_eq!(out, "a\tb\nx\ny\nraw\\n\n");
}

#[test]
fn autoload_reads_body_from_fpath_and_keeps_builtins() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("fn");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("greet"), "echo \"hello $1\"\n").unwrap();
    let script = format!(
        "fpath=({} $fpath); autoload -Uz greet add-zsh-hook; greet world; greet again; \
         type add-zsh-hook",
        dir.display()
    );
    let (_, out, _) = sh_home(&script, &home);
    assert_eq!(
        out,
        "hello world\nhello again\nadd-zsh-hook is a shell builtin\n"
    );
}

#[test]
fn hooks_run_with_saved_status() {
    let zshrc = r#"
p1() { echo "p1 st=$? ps=${pipestatus[*]}"; false; }
p2() { echo "p2 st=$?"; }
precmd_functions+=(p1)
autoload -Uz add-zsh-hook
add-zsh-hook precmd p2
add-zsh-hook preexec px
px() { echo "preexec [$1]"; }
add-zsh-hook chpwd cw
cw() { echo "chpwd $PWD"; }
zshexit() { echo bye; }
"#;
    let (status, out) = interactive(zshrc, "false | true\ncd /\nexit 3\n");
    assert_eq!(status, 3);
    assert_eq!(
        out,
        "p1 st=0 ps=0\np2 st=0\npreexec [false | true]\n\
         p1 st=0 ps=1 0\np2 st=0\npreexec [cd /]\nchpwd /\n\
         p1 st=0 ps=0\np2 st=0\npreexec [exit 3]\nbye\n"
    );
}

#[test]
fn prompt_substitutes_before_percent_escapes() {
    let zshrc = "setopt promptsubst\nPROMPT='$(printf \"%%{<%%}x\")%# '\n";
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .arg("-i")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    let expected = if unsafe { libc::geteuid() } == 0 {
        "<x# "
    } else {
        "<x% "
    };
    assert!(err.starts_with(expected), "prompt was {:?}", err);
}
