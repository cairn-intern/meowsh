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

Options: `-i` (force interactive), `-l`/`--login`, `-s`, `-c`, any `set`
option letter (`-a`, `-e`, `-f`, `-h`, `-m`, `-n`, `-u`, `-v`, `-x`, `-C`),
and `-o name` (for example `-o pipefail`). Startup files are read in zsh
order: `~/.zshenv`, `~/.zprofile` (login), `~/.zshrc` (interactive),
`~/.zlogin` (interactive login).

## zsh startup files

meowsh reads an existing zsh configuration and supports what common
configs and tool hooks (starship, direnv) generate:

- process substitution `<(...)` and `>(...)`
- `(( ... ))`, `for (( ; ; ))`, floating-point arithmetic and the
  `zsh/mathfunc` functions (`int`, `rint`, `floor`, `sqrt`, ...)
- hooks: `precmd`, `preexec`, `chpwd`, `zshexit`, their `*_functions`
  arrays, and `add-zsh-hook`
- `autoload` of functions from `$fpath`
- subscript flags `${a[(i)pat]}`, `(I)`, `(r)`, `(R)`, `(e)`
- `[[ x == (a|b)* ]]` grouped patterns and `[[ -v name[key] ]]`
- `print`, `{ cmd }` without a closing `;`
- `$RANDOM`, `$SECONDS`, `$EPOCHSECONDS`, `$EPOCHREALTIME`, `$COLUMNS`,
  `$LINES`, `$pipestatus`
- prompts: `$PROMPT`/`$PS1` and `$PROMPT2`/`$PS2` get parameter and
  command substitution first, then zsh `%` escapes

Lines that source `oh-my-zsh.sh` are skipped, and zsh completion scripts
(`#compdef`) are not run. `zle`, `bindkey`, `compinit` and `compdef` are
accepted and do nothing; `RPROMPT` is not shown. Up and Down search
history for lines starting with the typed text. History is kept in
`~/.meowsh_history`.

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
