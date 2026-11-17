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
        // Aliases apply to text parsed after they are defined, so the
        // uses go through eval (zsh parses a -c string whole).
        "alias -g G='| grep b'; eval \"printf 'a\\nb\\n' G\"; alias ll='ls -l'; \
         echo ${aliases[ll]} ${#galiases}; aliases[hi]='echo hello'; eval hi",
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
    // The running fc is the current event, not listed (as zsh does).
    let (_, out) = interactive("", "echo a\nfc -ln -2\n");
    assert_eq!(out, "a\necho a\n");
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

// ---- whence, match flags, extended glob, namerefs, array items ------------

#[test]
fn whence_family_formats() {
    let (_, out, _) = sh(
        "alias ll='ls -l'; f() { :; }; whence ll; whence -v ll f echo if; whence -c echo if; \
         whence -w ll f echo if sh nope-xyz; type -w echo",
    );
    assert_eq!(
        out,
        "ls -l\nll is an alias for ls -l\nf is a shell function from meowsh\necho is a shell builtin\n\
         if is a reserved word\necho: shell built-in command\nif: shell reserved word\n\
         ll: alias\nf: function\necho: builtin\nif: reserved\nsh: command\nnope-xyz: none\necho: builtin\n"
    );
}

#[test]
fn substitution_match_flags() {
    let (_, out, _) = sh(
        "s='where is the white windmill'; print ${(S)s#h*e}; print ${(S)s%%h*e}; \
         print ${(BES)s##white}; print ${(NS)s##white}; print ${(MSI:2:)s#w*e}; \
         a=(arthur boldly claws); print ${a:/[aeiou]*/X}; print ${(S)a/[aeiou]*/Y}; \
         x=abcabc; print ${x//b/X} ${x#*b} ${x%%b*}; set2=old; print ${set2::=new} $set2; \
         m=(one two three); n=(two); print ${m:|n} -- ${m:*n}",
    );
    assert_eq!(
        out,
        "wre is the white windmill\nwhere is the w windmill\n14 19\n5\nwhite\nX boldly claws\n\
         Yrthur bYldly clYws\naXcaXc cabc a\nnew new\none three -- two\n"
    );
}

#[test]
fn extended_glob_operators() {
    let (_, out, _) = sh(
        "setopt extendedglob; [[ aaab == a#b ]] && echo 1; [[ b == a##b ]] || echo 2; \
         [[ foo == ^bar ]] && echo 3; [[ foo.c == *.c~f* ]] || echo 4; [[ FOO == (#i)foo ]] && echo 5; \
         [[ abab == (ab)## ]] && echo 6; print \"q~^#\"",
    );
    assert_eq!(out, "1\n2\n3\n4\n5\n6\nq~^#\n");
}

#[test]
fn padding_and_display_flags() {
    let (_, out, _) = sh(
        "foo=(resulting words uproariously); print ${(l.5..X.r.5..Y.)foo}; \
         print ${(l.6..X..A.r.6..Y..B.)foo}; w=(why in); print ${(r.5..!..?.)w}; \
         e='$(print a b)'; print -l ${(e)e}; psvar=(dog); print ${(%):-%1v}; \
         arr=(ab cd e); print ${(c)#arr} ${(w)#arr}; v=$'a\\x7fb'; print ${(V)v}; \
         q=\"a b'c\"; print -r ${(qqqq)q}",
    );
    assert_eq!(
        out,
        "Xresulting XXXwordsYY proariousl\nXAresultingB XXXAwordsBYY uproariously\n\
         why?! in?!!\na\nb\ndog\n7 3\na^?b\n$'a b\\'c'\n"
    );
}

#[test]
fn namerefs() {
    let (status, out, err) = sh(
        "var=value; typeset -n ptr=var; echo $ptr ${ptr[2,3]} ${!ptr}; ptr=new; echo $var; \
         arr=(a b); typeset -n ar=arr; ar+=(c); echo ${ar[@]} $#ar; typeset -p ptr; \
         typeset -n p2; p2=var; echo $p2; unset ptr; echo ${+var}; \
         typeset -n l1=l2; typeset -n l2=l1",
    );
    assert_eq!(
        out,
        "value al var\nnew\na b c 3\ntypeset -n ptr=var\nnew\n0\n"
    );
    assert!(err.contains("invalid self reference"), "stderr: {}", err);
    assert_eq!(status, 1);
}

#[test]
fn key_value_array_items_and_slices() {
    let (_, out, _) = sh(
        "typeset -A h=([one]=eins [two]=zwei); h+=([three]=drei); for k in ${(ok)h}; print $k $h[$k]; \
         a=(first [3]=third fourth); print -l \"${a[@]}\"; \
         c=(one two three four five); c[2,4]=(umm er); print ${#c} $c; c[2,3]=(); print $c; \
         s=abcdef; s[2,3]=XY; print $s",
    );
    assert_eq!(
        out,
        "one eins\nthree drei\ntwo zwei\nfirst\n\nthird\nfourth\n4 one umm er five\none five\naXYdef\n"
    );
}

// ---- glob/split toggles, KSH_ARRAYS, captures, assignment forms -----------

#[test]
fn glob_and_split_toggles_and_brace_order() {
    let (_, out, err) = sh("cd $HOME && touch b1 b2; s='b*'; print $s ${~s} $~s; \
         setopt shwordsplit; w='x y'; print -l ${==w}; unsetopt shwordsplit; \
         a=(p q); print IF{${^a},T}E; print X{\"${a}\",T}Y");
    assert_eq!(
        out, "b* b1 b2 b1 b2\nx y\nIFpE IFTE IFqE IFTE\nXp qY XTY\n",
        "stderr: {}",
        err
    );
}

#[test]
fn ksh_arrays_and_zero_subscript() {
    let (_, out, _) = sh(
        "setopt ksharrays; k=(x y z); print $k ${k[0]} ${k[1]} \"${k[0,1]}\" ${k[(i)y]}; \
         k[0]=X; print ${k[@]}; unsetopt ksharrays; \
         z=(one two); print X${z[0]}X; setopt kshzerosubscript; print ${z[0]}",
    );
    assert_eq!(out, "x x y x y 1\nX y z\nXX\none\n");
}

#[test]
fn backreferences_and_match_counts() {
    let (_, out, err) = sh("setopt extendedglob; s='look for a match'; \
         [[ $s == (#b)(*)(match) ]] && print $match[1]x $match[2] $mbegin[2] $mend[2]; \
         t='this is'; print ${t//(#m)s/<$MATCH$MBEGIN>}; \
         f=aleftkept; print ${f//(#b)(*)left/${match/a/A}}; \
         n=1_2_3_4; print ${n#(*_)(#c2)}; \
         fn() { setopt localoptions noextendedglob; }; fn; [[ -o extendedglob ]] && print still");
    assert_eq!(
        out, "look for a x match 12 16\nthi<s4> i<s7>\nAkept\n3_4\nstill\n",
        "stderr: {}",
        err
    );
}

#[test]
fn subscripted_and_positional_assignment_forms() {
    let (_, out, err) = sh(
        "typeset -Z3 z; print ${z::=15}; a=(a b c d); print ${a[2]::=X} $a; \
         () { print ${2::=two} $2 } one; s=abc; s[2]=Z; print $s; \
         x=sprodj; x[-10]=scrumf; print $x; typeset -i i=1; i=(3 4); typeset -p i; \
         set a b c d e f g h i j k; print $11 $10; v=abc; print $(( #v )) $(( ##a )); \
         set 1 2 3 4 5 6 7 8 9 10; print ${##} ${##1}; echo $(( 010 ))",
    );
    assert_eq!(
        out,
        "015\nX a X c d\ntwo two\naZc\nscrumfsprodj\ntypeset -a i=( 3 4 )\nk j\n97 97\n2 0\n10\n",
        "stderr: {}",
        err
    );
}

#[test]
fn flag_order_and_double_quoted_joining() {
    let (_, out, _) = sh("arr=(she sells z shells); (IFS=+; print ${(s.s.)arr}); \
         x=b:a:b; print ${(us.:.)x} ${(os.:.j.-.)x}; \
         foo=(a6 a117 a17 b-2 b-11); print ${(n)foo}; print ${(-)foo}; \
         a=(et Swann avec); print \"${a#?}\" \"${${a}[2,4]}\" \"${${(@)a}[1,2]#?}\" \"${(o)a}\"");
    assert_eq!(
        out,
        "he+ ell +z+ hell\nb a a b b\na6 a17 a117 b-2 b-11\na6 a17 a117 b-11 b-2\n\
         t Swann avec t S t Swann et Swann avec\n"
    );
}

#[test]
fn errors_name_their_context_and_line() {
    let (_, out, err) = sh(
        "eval 'nosuchcmd-xyz'\nf() {\n  :\n  nosuchcmd-abc\n}\nf\n() { cd /nonexistent-dir-xyz }\n\
         print $(( 3,,4 ))\nprint after",
    );
    assert_eq!(out, "");
    assert_eq!(
        err,
        "(eval):1: command not found: nosuchcmd-xyz\nf:2: command not found: nosuchcmd-abc\n\
         (anon):cd: no such file or directory: /nonexistent-dir-xyz\n\
         meowsh:8: bad math expression: operand expected at `,4 '\n"
    );
}

#[test]
fn redirect_both_streams_to_a_file() {
    let (_, out, _) = sh("cd $HOME && { echo out; echo err >&2 } >& f.txt; cat f.txt");
    assert_eq!(out, "out\nerr\n");
}

// ---- (z) splitting, subscript searches, enable/disable, traps ------------

#[test]
fn z_flag_splits_like_the_lexer() {
    let (_, out, _) = sh("foo='<five> {six} )ten( more'; print -rl -- ${(z)foo}; \
         s='for (( i = 1 ; i < 3 ; i++ ))'; print -rl -- ${(z)s}; \
         l=$'a # c\\nb'; print -rl -- ${(Z+c+)l} -- ${(Z+C+)l}");
    assert_eq!(
        out,
        "<\nfive\n>\n{six}\n)\nten( more\nfor\n((\ni = 1 ;\ni < 3 ;\ni++ \n))\n\
         a\n# c\n;\nb\n--\na\n;\nb\n"
    );
}

#[test]
fn subscript_search_flags_and_multibyte() {
    let (_, out, err) = sh("s='Twinkle, twinkle, little *'; x=','; \
         print ${s[(r)$x,(R)$x]} $s[(i)winkle] $s[(I)winkle]; \
         a=ténébreux; print ${a[(r)én,(r)éb]} ${a[(rb:3:)é,-1]} ${a[(rn:2:)é,-1]}; \
         b=(sping spang spong); print ${b[(ib.2.)sp?ng]} ${b[(n:2:i)sp*]}; \
         w='foo bar baz'; print ${w[(w)2]} ${w[(wi)baz]}; hähä=3; print $hähä");
    assert_eq!(
        out, ", twinkle, 2 11\nénéb ébreux ébreux\n2 2\nbar 9\n3\n",
        "stderr: {}",
        err
    );
}

#[test]
fn enable_disable_and_closing_braces() {
    let (_, out, _) = sh(
        "f() { print f }; disable -f f; f 2>/dev/null || print disabled; enable -f f; f; \
         disable -r typeset; eval 'g() { typeset v=`echo a b=c`; print $v $b }'; g; \
         { A=0 }; print $A",
    );
    assert_eq!(out, "disabled\nf\na c\n0\n");
}

#[test]
fn readonly_listing_in_sh_mode_does_not_hang() {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .args([
            "--emulate",
            "sh",
            "-c",
            "PATH=/bin; export PATH; readonly PATH; export -p PATH; typeset -p PATH; \
             readonly -p; echo ${path-unset}",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&o.stdout),
        "export -r PATH=/bin\nreadonly PATH=/bin\nunset\n"
    );
}

#[test]
fn fd_variables_clobber_forms_and_read_options() {
    let (_, out, _) = sh(
        "cd $HOME && exec {fd}>out; print to fd >&$fd; exec {fd}>&-; cat out; \
         set -o noclobber; print a >!o2; print b >>|o2; cat o2; \
         print -n x | read -e; print; print -n y | { read -t 1 v; print $v }",
    );
    assert_eq!(out, "to fd\na\nb\nx\n\ny\n");
}

#[test]
fn shebang_fallback_and_prefix_path() {
    let (_, out, err) = sh(
        "cd $HOME && print -n '#!sh\\necho slashless' >t1 && chmod +x t1 && \
         PATH=$PATH:$HOME t1",
    );
    assert_eq!(out, "slashless\n", "stderr: {}", err);
}

#[test]
fn zerr_and_debug_traps() {
    let (_, out, _) = sh(
        "TRAPZERR() { print zerr }; false; unfunction TRAPZERR; false; print ok\n\
         trap 'print dbg $LINENO' DEBUG; :; trap - DEBUG\nprint $LINENO",
    );
    assert_eq!(out, "zerr\nok\ndbg 2\ndbg 2\n3\n");
}

// ---- functions: anonymous, math, autoload, handlers ----------------------

#[test]
fn anonymous_functions_with_glob_args_and_redirections() {
    let (_, out, err) = sh(
        "cd $HOME && v=outside; function { local v=inside; print $v $* } a b; print $v; \
         touch yes no; () { echo $1 } (y|z)*; function { sed s/a/b/ } <<<abc",
    );
    assert_eq!(out, "inside a b\noutside\nyes\nbbc\n", "stderr: {}", err);
}

#[test]
fn not_found_handler_math_functions_and_argv() {
    let (_, out, err) = sh(
        "command_not_found_handler() { print handled $1 ${argv[2,-1]}; return 3 }; \
         nosuchcmd-q x y; print $?; \
         functions -M m1; m1() { (( $# * 10 )) }; print $(( m1(1,2) )); \
         f() { (( $#1 )) }; functions -Ms slen 1 1 f; print $(( slen(a, b) ))",
    );
    assert_eq!(
        out, "handled nosuchcmd-q x y\n3\n20\n4\n",
        "stderr: {}",
        err
    );
}

#[test]
fn eval_parses_whole_string_and_function_names_expand() {
    let (_, out, _) = sh(
        "eval 'alias zz=\"print aliased\"; zz' 2>/dev/null; eval zz; \
         function g$$ () { print named }; g$$; d() { a b() { print $0 } }; functions d; \
         e(){}; e; print empty ok",
    );
    assert_eq!(
        out,
        "aliased\nnamed\nd () {\n\tfunction a b {\n\t\tprint $0\n\t}\n}\nempty ok\n"
    );
}

#[test]
fn autoload_paths_and_function_copies() {
    let (_, out, err) = sh(
        "cd $HOME && mkdir -p ad && print 'print loaded from dir' >ad/af; \
         autoload $PWD/ad/af; af; print 'print via X' >ad/ax; ax() { autoload -X $PWD/ad }; ax; \
         h() { print h }; functions -c h h2; unfunction h; h2",
    );
    assert_eq!(out, "loaded from dir\nvia X\nh\n", "stderr: {}", err);
}

#[test]
fn output_files_equals_and_glob_exclusions() {
    let (_, out, err) = sh("cd $HOME && cat =(print in a file); print =cat(:t); \
         mkdir -p g/d1 g/d2 g/d3; touch g/f; setopt extendedglob; \
         print g/*~*/d2; print g/*~*/d3(/*|(#e))(/); print g/*(/:t); \
         (FUNCNEST=1; r() { r }; r) 2>/dev/null; print $?");
    assert_eq!(
        out, "in a file\ncat\ng/d1 g/d3 g/f\ng/d1 g/d2\nd1 d2 d3\n1\n",
        "stderr: {}",
        err
    );
}

// ---- right prompt ---------------------------------------------------------

// Runs an interactive meowsh on a 60-column pseudo-terminal, types
// `input`, and returns everything it drew.
fn on_terminal(zshrc: &str, input: &str) -> String {
    use std::io::{Read, Write};
    use std::os::fd::FromRawFd;
    let home = TempDir::new().unwrap();
    std::fs::write(home.path().join(".zshrc"), zshrc).unwrap();
    let (mut master, mut slave) = (0, 0);
    let ws = libc::winsize {
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
            std::ptr::null(),
            &ws,
        )
    };
    assert_eq!(ok, 0, "openpty");
    let tty = |fd: i32| unsafe { Stdio::from_raw_fd(libc::dup(fd)) };
    let mut child = Command::new(env!("CARGO_BIN_EXE_meowsh"))
        .env("HOME", home.path())
        .env("TERM", "xterm")
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
    std::thread::sleep(std::time::Duration::from_millis(500));
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

#[test]
fn rprompt_is_drawn_at_the_right_edge() {
    let out = on_terminal("PROMPT='> '\nRPROMPT='[right]'\n", "echo hi");
    // 60 columns, 7 wide, one column of indent: column 53 (1-based).
    assert!(out.contains("\x1b[53G[right]"), "output: {:?}", out);
}

#[test]
fn read_k_honours_its_timeout() {
    // The harness for zsh's ZLE tests waits this way between keys; it
    // used to block until the writer went away.
    let (_, out, _) = sh("typeset -F SECONDS; s=$SECONDS; \
         { sleep 2 >/dev/null 2>&1 & } | read -t 0.05 -u 0 -k 1; r=$?; \
         print $r $(( SECONDS - s < 1 ))");
    assert_eq!(out, "1 1\n");
}

#[test]
fn rc_expand_and_glob_subst_flags_combine() {
    let (_, out, err) = sh("a=(x .); print -r ${^~a:/.}");
    assert_eq!((out.as_str(), err.as_str()), ("x\n", ""));
}

#[test]
fn quoted_nested_split_keeps_empty_fields() {
    let (_, out, _) = sh("x=:a:b; print -l \"${(@)${(@s.:.)x}[1,2]}\"");
    assert_eq!(out, "\na\n");
}

#[test]
fn arithmetic_output_base_may_come_from_a_parameter() {
    let (_, out, _) = sh("f='##8'; print $(( [$f] 8#7 + 1 ))");
    assert_eq!(out, "10\n");
}

#[test]
fn rc_expand_param_drops_words_with_empty_arrays() {
    let (_, out, _) = sh("setopt rcexpandparam; a=(); f=( \"$a[@]\" x$a ); b=(1 2); \
         g=( \"x$b[@]y\" ); print ${#f} $g");
    assert_eq!(out, "0 x1y x2y\n");
}

#[test]
fn process_substitution_is_literal_inside_braces() {
    let (_, out, err) = sh("setopt extendedglob; s='<LC><DI><RC>'; \
         print ${(N)s#*(#b)<LC><(??)><RC>} $match[1]; cat <(print ok)");
    assert_eq!((out.as_str(), err.as_str()), ("12 DI\nok\n", ""));
}

#[test]
fn zsh_parameter_module_specials() {
    let (_, out, _) = sh(
        "x=1; print $+builtins[:] $+builtins[nosuch] ${parameters[x]} \
         $+commands[sh]",
    );
    assert_eq!(out, "1 0 scalar 1\n");
}

#[test]
fn zmodload_e_reports_missing_modules() {
    let (_, out, _) = sh("zmodload -e zsh/compctl; a=$?; zmodload -e zsh/main; print $a $?");
    assert_eq!(out, "1 0\n");
}

#[test]
fn argv_assignments_edit_the_positional_parameters() {
    let (_, out, _) = sh("f() { argv+=(h:$1); argv[1]=x; print -r -- $# $@ }; f a b");
    assert_eq!(out, "3 x b h:a\n");
}

#[test]
fn set_a_takes_every_later_word_as_a_value() {
    let (_, out, _) = sh("o=(); set -A a \"$o[@]\" -J y; print -r -- $#a $a");
    assert_eq!(out, "2 -J y\n");
}

#[test]
fn typeset_plus_fm_lists_matching_functions() {
    let (_, out, _) = sh("_q() {}; _x() {}; y() {}; print -r -- $(typeset +fm '_*')");
    assert_eq!(out, "_q _x\n");
}

#[test]
fn indirect_keys_and_values_of_an_association() {
    let (_, out, _) = sh("typeset -A h; h=(a 1 b 2); n=h; print -r -- ${(@Pkv)n} / ${(@Pk)n}");
    assert_eq!(out, "a 1 b 2 / a b\n");
}

#[test]
fn tilde_join_separator_is_pattern_text() {
    let (_, out, _) = sh("f=(/a/_x /a/_y /b/_z); a=(_x _z); print -r -- ${(M)f:#*/(${(~j.|.)a})}");
    assert_eq!(out, "/a/_x /b/_z\n");
}

#[test]
fn quoted_outer_expansion_joins_a_nested_at_list() {
    let (_, out, _) = sh("f() { print -rl -- \"x ${${(@q+)@}} y\" }; f a 'b c'");
    assert_eq!(out, "x a 'b c' y\n");
}

#[test]
fn zformat_formats_and_aligns() {
    let (_, out, _) = sh(
        "zformat -f r '<%d:%-5n>' d:file n:ab; zformat -a l . aa:x b:y; \
         print -r -- $r $l",
    );
    assert_eq!(out, "<file:   ab> aa.x b .y\n");
}

#[test]
fn close_brace_may_follow_a_quoted_word() {
    let (_, out, _) = sh("f () { echo -n 'a'}; f; { echo \"b\"}; { echo '}'}");
    assert_eq!(out, "ab\n}\n");
}

#[test]
fn a_lone_semicolon_is_an_empty_command() {
    let (st, out, _) = sh("; echo a; ; echo b; f () { echo c; ; }; f");
    assert_eq!((st, out.as_str()), (0, "a\nb\nc\n"));
}

#[test]
fn braces_expand_as_the_first_array_element() {
    let (_, out, _) = sh("a=({a,b} x); local -a c; c=( {1..2} ); print -r -- $a $c");
    assert_eq!(out, "a b x 1 2\n");
}

#[test]
fn dollar_bracket_arithmetic() {
    let (_, out, _) = sh("a=3; e=$[ $a - 1 ]; print -r -- $e $[2*3] \"$[4]\"");
    assert_eq!(out, "2 6 4\n");
}

#[test]
fn quoted_subscripts_may_contain_blanks() {
    let (_, out, _) = sh("p=1; x=abcd; print -r -- \"[$x[p + 1, -2]]\"");
    assert_eq!(out, "[bc]\n");
}

#[test]
fn glob_group_words_after_a_command_word() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo"), "").unwrap();
    let (_, out, _) = sh(&format!(
        "cd {}; print (foo|bar) x; x=(''); setopt nomatch; print -r -- $^x(N) end",
        dir.path().display()
    ));
    assert_eq!(out, "foo x\nend\n");
}

#[test]
fn loading_an_unavailable_module_fails() {
    let (st, _, err) = sh("zmodload zsh/pcre");
    assert_eq!(st, 1);
    assert!(err.contains("failed to load module `zsh/pcre'"), "{}", err);
}

#[test]
fn subscript_flag_p_splits_on_nul() {
    let (_, out, _) = sh("s=$'foo\\0bar'; print -r -- ${s[(pws:\\0:)2]}");
    assert_eq!(out, "bar\n");
}

#[test]
fn search_flags_as_assignment_range() {
    let (_, out, _) = sh("a=(x y z); a[(R)none,(r)none]=(n m); print -r -- $a");
    assert_eq!(out, "n m\n");
}

#[test]
fn module_builtins_work_until_zmodload_manages_the_module() {
    let (_, out, _) = sh("TZ=UTC0; strftime -s t %Y 0; print -r -- $t; \
         zmodload -F zsh/datetime -b:strftime; strftime %Y 0 2>/dev/null || print gone");
    assert_eq!(out, "1970\ngone\n");
}
