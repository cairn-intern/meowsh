# meowsh

A POSIX-compliant shell implementation.

## Features

- POSIX compliance (WIP)
- Built-in commands (alias, jobs, etc.)
- Line editing support
- Job control
- Variable expansion
- Redirections

## Building

To build `meowsh`, run:

```bash
make
```

## Usage

After building, you can start the shell with:

```bash
./meowsh                    # interactive when stdin and stderr are terminals
./meowsh script.sh a b      # run a script with positional parameters
./meowsh -c 'echo $1' x y   # run a command string ($0 is x, $1 is y)
./meowsh -s a b < script.sh # read commands from stdin with parameters
```

meowsh behaves like zsh: unquoted parameters are not word-split, a glob
that matches nothing is an error, and so on. Started as `sh` (or `ksh`,
`bash`), or with `--emulate sh` as the first argument, it uses sh
behavior and option letters instead, as zsh does; `emulate sh` switches
inside a script.

Options: `-i` (force interactive), `-l`/`--login`, `-s`, `-c`, `-o name`,
and zsh's option letters (`-e` errexit, `-x` xtrace, `-f` no startup
files, ...); all of zsh's named options are recognized by `setopt`,
`set -o` and `$options`. Startup files are read in zsh order:
`~/.zshenv`, `~/.zprofile` (login), `~/.zshrc` (interactive), `~/.zlogin`
(interactive login).

## zsh startup files

meowsh reads an existing zsh configuration, including oh-my-zsh, its
plugins and themes, and the hooks that tools such as starship and direnv
generate. It sets `$ZSH_VERSION` (to `5.9-meowsh`, so per-version files
stay apart from real zsh's) and `$MEOWSH_VERSION`. Supported zsh syntax
and builtins include:

- process substitution `<(...)` and `>(...)`, here-strings `<<<`, `&>`,
  `&|`, `{ ... } always { ... }`, `{ cmd }` without a closing `;`
- `(( ... ))`, `for (( ; ; ))`, `for x (list)`, `for k v (list)`,
  anonymous functions `() { ... } args`, `function a b { ... }`
- floating-point arithmetic and `zsh/mathfunc` functions (`int`, `rint`,
  `floor`, `sqrt`, ...)
- parameter expansion: nested `${${x}[2]}`, flags such as `(s:,:)`,
  `(j: :)`, `(f)`, `(@)`, `(k)`, `(kv)`, `(o)`, `(u)`, `(q)`, `(Q)`, `(U)`,
  `(%)`, `(P)`, `(t)`, `(l:n:)`; modifiers `:h`, `:t`, `:r`, `:e`, `:a`,
  `:A`, `:l`, `:u`, `:q`, `:s/a/b/`; `${a[2,-1]}`, `${^a}`, `${=a}`,
  `${a:#pat}`, `${a:^b}`; `$name[sub]`, `$+name`, `$#name` without braces
- subscript flags `(i)`, `(I)`, `(r)`, `(R)`, `(e)`, `(w)` and `^pat`
- patterns and globs with `(a|b)` groups and `<n-m>` ranges, `**/`, and
  glob qualifiers such as `(N)`, `(.)`, `(/)`, `(@)`, `(m-1)`, `(D)`
- `[[ x == (a|b)* ]]` and `[[ -v name[key] ]]`
- hooks: `precmd`, `preexec`, `chpwd`, `zshexit`, their `*_functions`
  arrays, and `add-zsh-hook`
- `typeset` and `declare`, `local`, `export`, `readonly`, `integer`,
  `float` with zsh's attributes (`-i`, `-E`/`-F`, `-L`/`-R`/`-Z`, `-l`/`-u`,
  `-r`, `-x`, `-U`, `-T` tied pairs, `-g`, `-m`, `-p`), array values
  (`typeset a=(x y)`), and `functions` to print definitions
- `repeat`, `select`, `coproc` (`print -p`, `read -p`, `>&p`), `foreach`,
  short and brace forms (`if [[ x ]] cmd`, `for x in a b; cmd`,
  `while c { }`, `case w { }`), `;|`, `|&`, `noglob`, `-`, `exec -a`/`-l`/`-c`
- `autoload` from `$fpath`, `print`, `emulate`, `is-at-least`,
  `zparseopts`, `fc -l`, `colors`, `unfunction`, `builtin`, `read -k`/`-q`,
  global aliases (`alias -g`) and the `$aliases`/`$galiases` arrays
- function-scoped EXIT traps, `LOCAL_OPTIONS`/`LOCAL_TRAPS` (also set by
  `emulate -L`), `AUTO_CD`, `$0` as the function or sourced file name
- `$RANDOM`, `$SECONDS`, `$EPOCHSECONDS`, `$EPOCHREALTIME`, `$COLUMNS`,
  `$LINES`, `$pipestatus`, `$HOST`, `$commands`, `$langinfo`
- prompts: `$PROMPT`/`$PS1` and `$PROMPT2`/`$PS2` get parameter and
  command substitution first, then zsh `%` escapes including `%N<..<`
  truncation

There is no zle: `zle`, `bindkey`, `compinit`, `compdef` and `zcompile`
are accepted and do nothing, so zle plugins such as zsh-autosuggestions
load but stay inactive (meowsh shows its own history suggestions), and
zsh completion scripts (`#compdef`) are not run. `RPROMPT` is not shown.
Other zsh options are recorded by `setopt` but have no effect. Up and Down
search history for lines starting with the typed text. History is kept in
`~/.meowsh_history`; `$HISTFILE` is not used, so zsh's history file is
never rewritten in a different format.

## Using meowsh as your shell

Install it, then point your terminal at it before changing your login
shell, so a broken build cannot lock you out:

```bash
sudo make install                     # installs /usr/local/bin/meowsh
# Terminal only, for example foot: shell=/usr/local/bin/meowsh in foot.ini
# Login shell:
echo /usr/local/bin/meowsh | sudo tee -a /etc/shells
chsh -s /usr/local/bin/meowsh
```

## Development

The source code is written in Rust and located in the `src/` directory. Run the checks with:

```bash
cargo fmt --check
RUSTFLAGS="-Dwarnings" cargo clippy --all-targets
cargo test
```

To measure zsh compatibility, run zsh's own test suite (`Test/*.ztst` in a
zsh source checkout) against a release build:

```bash
cargo build --release
python3 scripts/ztst.py --zsh-src ../zsh            # summary per file
python3 scripts/ztst.py --zsh-src ../zsh -v D04parameter  # show failures
```
