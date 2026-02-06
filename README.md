# shell.ai

A tiny interactive shell written in Rust.

## Features

- Builtins: `echo`, `pwd`, `type`, `cd`, `exit`
- External commands from your `PATH`
- Optional prompt that shows the current directory name

## Requirements

- Rust toolchain (via `rustup`)
- Unix-like OS (uses Unix permission bits to detect executables)

## Run

```bash
cargo run
```

## Usage

At the prompt (`$ `), type a command and press Enter.

Builtins:

- `echo <text>`: print text
- `pwd`: print current working directory
- `type <name>`: show whether `<name>` is a builtin or the resolved path in `PATH`
- `cd [path|~]`: change directory; with no args or `~` goes to `$HOME`
- `exit`: exit the shell

External commands:

- If the command name exists in `PATH` at shell startup, it is executed via `std::process::Command`.

## Prompt

Enable showing the current directory name in the prompt:

```bash
ENABLE_CUR_DIR_DISPLAY=true cargo run
```

Example prompt:

```
[project] $ 
```

## Notes / Limitations

- Argument parsing is whitespace-based (no quotes, escaping, pipes, or redirects).
- External command availability is determined from a startup cache of executables in `PATH`.
