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

## Development

The source code is written in Rust and located in the `src/` directory. Run the checks with:

```bash
cargo fmt --check
RUSTFLAGS="-Dwarnings" cargo clippy --all-targets
cargo test
```
