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

// ---- oh-my-zsh ------------------------------------------------------------

#[test]
fn zsh_version_and_emulate() {
    let (_, out, _) = sh("[ -n \"$ZSH_VERSION\" ] && echo set; emulate; is-at-least 5.0 && echo ok; is-at-least 9.0 || echo old");
    assert_eq!(out, "set\nzsh\nok\nold\n");
}

#[test]
fn nested_expansion_flags_and_modifiers() {
    let (_, out, err) = sh("v='git version 2.43.0'; echo ${${(As: :)v}[3]}; \
         p=/a/b/c.tar.gz; echo ${p:t} ${p:h} ${p:r} ${p:e} ${p:h:t}; \
         a=(b a c a); echo ${(j:,:)a} ${(o)a} ${(u)a} ${#a} ${a[2,-1]}; \
         codes=(1 2); x=(\"<\"${^codes}\">\"); echo ${x[@]}; \
         s=hello; echo ${s[2]} ${s[2,3]} $#s; \
         typeset -A m; m=(k1 v1 k2 v2); echo ${(k)m} ${(kv)m} ${(k)m[(r)v2]}; \
         l='l1\nl2'; arr=(\"${(@f)$(printf \"$l\")}\"); echo ${#arr} $arr[2]; \
         echo $+commands[sh] $+commands[no-such-cmd-xyz]; \
         o='a b'; set -- ${=o}; echo $#; \
         h=(x.zsh y.sh); echo ${h:#*.sh} ${(M)h:#*.sh}; \
         k=(1 2); w=(a b); echo ${k:^w}");
    assert_eq!(
        out,
        "2.43.0\nc.tar.gz /a/b /a/b/c.tar gz b\nb,a,c,a a a b c b a c 4 a c a\n<1> <2>\n\
         e el 5\nk1 k2 k1 v1 k2 v2 k2\n2 l2\n1 0\n2\nx.zsh y.sh\n1 a 2 b\n",
        "stderr: {}",
        err
    );
}

#[test]
fn global_aliases_and_the_aliases_array() {
    let (_, out, _) = sh(
        "alias -g G='| grep b'\nprintf 'a\\nb\\n' G\nalias ll='ls -l'\n\
         echo ${aliases[ll]} ${#galiases}\naliases[hi]='echo hello'\nhi",
    );
    assert_eq!(out, "b\nls -l 1\nhello\n");
}

#[test]
fn short_loops_multiple_names_and_anonymous_functions() {
    let (_, out, _) = sh(
        "for k v (a 1 b 2); do echo $k=$v; done; for x (p q) echo $x; \
         () { echo anon $1 } arg; function f1 f2 { echo $0 }; f1; f2",
    );
    assert_eq!(out, "a=1\nb=2\np\nq\nanon arg\nf1\nf2\n");
}

#[test]
fn always_here_string_and_redirect_both() {
    let home = TempDir::new().unwrap();
    let (_, out, _) = sh_home(
        "cd; f() { { return 3 } always { echo cleanup } }; f; echo $?; \
         read x <<< 'from string'; echo $x; \
         { echo out; echo err >&2 } &> both; cat both; \
         (echo bg > bgout) &|; while [ ! -s bgout ]; do :; done; cat bgout; jobs | wc -l",
        &home,
    );
    assert_eq!(out, "cleanup\n3\nfrom string\nout\nerr\nbg\n0\n");
}

#[test]
fn case_patterns_with_groups() {
    let (_, out, _) = sh(
        "for v in freebsd13 plugin::load x; do case $v in \
         (darwin|freebsd)*) echo bsd;; plugin::(disable|load)) echo plug;; (x) echo posix;; esac; done",
    );
    assert_eq!(out, "bsd\nplug\nposix\n");
}

#[test]
fn glob_groups_ranges_and_qualifiers() {
    let home = TempDir::new().unwrap();
    for f in ["a.rs", "b.toml", "c.txt", "f1", "f2", "f10"] {
        std::fs::write(home.path().join(f), "").unwrap();
    }
    std::fs::create_dir(home.path().join("sub")).unwrap();
    let script = format!(
        "cd {}; echo *.(rs|toml); echo f<1-2>; echo *(/); echo none*(N) end; echo f<->(.)",
        home.path().display()
    );
    let (_, out, _) = sh_home(&script, &home);
    assert_eq!(out, "a.rs b.toml\nf1 f2\nsub\nend\nf1 f10 f2\n");
}

#[test]
fn zparseopts_and_fc() {
    let (_, out, _) = sh(
        "f() { local -a opts; local clear stamp; zparseopts -D -E -a opts c=clear t:=stamp P; \
         echo \"c=$clear t=${stamp[*]} o=${opts[*]} rest=$*\"; }; f -c -t fmt x -P y",
    );
    assert_eq!(out, "c=-c t=-t fmt o=-P rest=x y\n");
    let (_, out) = interactive("", "echo a\nfc -ln -2\n");
    assert_eq!(out, "a\necho a\nfc -ln -2\n");
}

#[test]
fn function_exit_trap_and_local_traps() {
    let (_, out, _) = sh("f() { trap 'echo fexit' EXIT; echo in; }; f; echo after; \
         g() { emulate -L zsh; trap 'echo int' INT; setopt autocd; }; g; trap; echo end");
    assert_eq!(out, "in\nfexit\nafter\nend\n");
}

#[test]
fn auto_cd_in_interactive_shell() {
    let (_, out) = interactive("setopt auto_cd\n", "/\npwd\n");
    assert_eq!(out, "/\n");
}

#[test]
fn read_keys_and_yes_no() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .args([
            "-c",
            "read -q && echo yes; read -k 1 c; echo \"[$c]\"; read -k 2; echo \"[$REPLY]\"",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"yZab").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "yes\n[Z]\n[ab]\n");
}

#[test]
fn colors_and_prompt_truncation() {
    let (_, out, _) = sh("colors; print -n $fg[red]x$reset_color | od -An -c | tr -s ' '; print -P '%5<..<abcdefgh%<<'");
    assert_eq!(out, " 033 [ 3 1 m x 033 [ 0 0 m\n..fgh\n");
}

#[test]
fn eval_catches_expansion_errors_but_not_exit() {
    let (status, out, _) = sh(
        "eval 'echo ${(Z)x}; echo same-eval'; echo \"st=$? after\"; \
         echo ${unset_v?gone}; echo not-reached",
    );
    assert_eq!(out, "st=1 after\n");
    assert_eq!(status, 1);
    let (status, out, _) = sh("eval 'exit 4'; echo not-reached");
    assert_eq!((status, out.as_str()), (4, ""));
}

// ---- zsh defaults, options, typeset, grammar ------------------------------

#[test]
fn zsh_defaults_and_sh_emulation() {
    let (_, out, _) = sh("x='a b'; for w in $x; do echo \"[$w]\"; done; for w in $(echo c d); do echo \"[$w]\"; done");
    assert_eq!(out, "[a b]\n[c]\n[d]\n");
    let (status, out, err) = sh("echo nomatch-xyz-*; echo after");
    assert_eq!((status, out.as_str()), (1, ""));
    assert!(err.contains("no matches found: nomatch-xyz-*"));
    let (_, out, _) = sh("emulate sh; x='a b'; set -- $x; echo $#; echo nomatch-xyz-*");
    assert_eq!(out, "2\nnomatch-xyz-*\n");
    let (_, out, _) = sh("setopt shwordsplit; x='a b'; set -- $x; echo $#; unsetopt sh_word_split; set -- $x; echo $#");
    assert_eq!(out, "2\n1\n");
    let (_, out, _) =
        sh("echo ${options[nomatch]} ${options[shwordsplit]}; [[ $- == *f* ]] || echo rcs");
    assert_eq!(out, "on off\nrcs\n");
}

#[test]
fn typeset_attributes_and_printing() {
    let (_, out, err) = sh(
        "f() { local s=x; local -a a=(1 2); typeset -p s a; typeset +m s a; \
         integer i=3.7; integer -i2 i; echo $i; typeset -p i; \
         float -F2 g=3.14159; echo $g; typeset -R5 -u v=foo; echo \"[$v]\"; typeset -p v; \
         typeset -Z4 z=7; echo $z; export e=1; typeset -p e; }; f; \
         typeset -T TIED tied=(a b); echo $TIED; tied+=(c); echo $TIED; typeset -p tied",
    );
    assert_eq!(
        out,
        "typeset s=x\ntypeset -a a=( 1 2 )\nlocal s\narray local a\n2#11\ntypeset -i2 i=3\n\
         3.14\n[  FOO]\ntypeset -R5 -u v=foo\n0007\nexport e=1\na:b\na:b:c\n\
         typeset -aT TIED tied=( a b c )\n",
        "stderr: {}",
        err
    );
}

#[test]
fn functions_prints_definitions() {
    let (_, out, _) =
        sh("f() { if a; then b; fi; for x in 1 2; do echo $x; done; c && d | e; }; functions f");
    assert_eq!(
        out,
        "f () {\n\tif a\n\tthen\n\t\tb\n\tfi\n\tfor x in 1 2\n\tdo\n\t\techo $x\n\tdone\n\tc && d | e\n}\n"
    );
}

#[test]
fn zsh_loop_forms_and_coprocess() {
    let (_, out, err) = sh(
        "repeat 2 echo r; for x in a b; print $x; if [[ 1 == 1 ]] print short; \
         if (false) { print no } else { print yes }; while (( n++ < 2 )) { print w$n }; \
         case b { (a) echo A;; (b) echo B;; }; \
         case x in (x) echo 1 ;| (*) echo 2 ;; esac; \
         foreach v (p q)\nprint $v\nend\n\
         coproc { read l; print got $l; }; print -p hi; read -p r; echo $r",
    );
    assert_eq!(
        out, "r\nr\na\nb\nshort\nyes\nw1\nw2\nB\n1\n2\np\nq\ngot hi\n",
        "stderr: {}",
        err
    );
}

#[test]
fn precommand_modifiers_and_exec_options() {
    let (_, out, _) = sh(
        "noglob echo *; command -v sh >/dev/null && echo quiet; exec -a renamed sh -c 'echo $0'",
    );
    assert_eq!(out, "*\nquiet\nrenamed\n");
}
