# tfil

A PTY proxy for filtering terminal sequences and enhancing interactive TUIs.

`tfil` runs a command inside a pseudo-terminal and sits between the child process and your terminal.  It can clean up or relay selected escape sequences and add targeted input handling without modifying the child program.

## Installation

```console
% cargo install tfil --locked
```

Or install a prebuilt binary from GitHub Releases:

```console
% curl --proto '=https' --tlsv1.2 -LsSf https://github.com/knu/tfil/releases/latest/download/tfil-installer.sh | sh
```

If you use `cargo-binstall`:

```console
% cargo binstall tfil
```

## Usage

```console
% tfil [OPTIONS] <COMMAND> [ARGS]...
```

With no behavior options, `tfil` is a transparent PTY proxy.  Options can be combined, and hyphenated arguments after `<COMMAND>` are passed through to the child:

```console
% tfil --strip-ink-fake-cursor claude --resume
```

### Output filters

- `--strip-cursor-shape` — Drop DECSCUSR (`CSI Pn SP q`) so child programs cannot change the terminal's cursor shape.
- `--strip-ink-fake-cursor` — Strip [Ink](https://github.com/vadimdemedes/ink)'s fake-cursor sequences (`\x1b[7m{grapheme}\x1b[27m` and friends), and suppress `\x1b[?25l` so the terminal's native cursor shows through.  Useful with Ink-based TUIs such as Claude Code, Gemini CLI, or ccmanager.
- `--strip-osc-titles` — Drop OSC 0/1/2 sequences (icon name and window title).  Other OSCs (4 = palette, 8 = hyperlink, 52 = clipboard, ...) are passed through.  Both ST (`ESC \`) and BEL terminators are recognized.
- `--tmux-osc-passthrough=CODES` — Wrap the OSC sequences with the given comma-separated codes in a tmux `DCS tmux; ... ST` passthrough so they reach the outer terminal instead of being swallowed by tmux.  Requires tmux 3.3+ with `allow-passthrough on` set.

### Interactive enhancements

`--codex-mouse-ui` makes Codex CLI's `›`-marked numbered menus, including approval prompts and question forms, mouse-driven.  `tfil` maintains a screen model of the output and enables SGR any-motion mouse reporting.  Hovering over a numbered option steers Codex's own selection there with arrow keys, so the selection marker follows the mouse, and the mouse pointer takes a hand shape via OSC 22 on supporting terminals.  Clicking sends Enter to confirm the selection.

Mouse events the menu logic does not consume are forwarded only when the child has enabled a mouse protocol of its own, using the encoding requested by the child.

```console
% tfil --codex-mouse-ui codex
```

To relay the OSC 22 pointer-shape updates through tmux, combine the mouse UI with `--tmux-osc-passthrough=22`:

```console
% tfil --codex-mouse-ui --tmux-osc-passthrough=22 codex
```

Some terminals manage the pointer shape themselves while mouse tracking is active, as tmux does with `mouse on`, and ignore OSC 22 in that state.

### Wrapper scripts

`--create-wrapper=PATH` writes a small shell script that runs the command named after its basename through `tfil` with the given options, instead of running a command.  The recommended setup for Claude Code and Codex CLI is:

```console
% tfil --create-wrapper=~/bin/claude --strip-ink-fake-cursor
% tfil --create-wrapper=~/bin/codex --codex-mouse-ui --tmux-osc-passthrough=22
```

Typing `claude` or `codex` then transparently runs the real command under `tfil` with the recommended enhancements.  The option is repeatable, so brace expansion creates several wrappers with the same options at once:

```console
% tfil --create-wrapper=~/bin/{claude,gemini} --strip-ink-fake-cursor
```

The generated script calls `tfil --wrap="$0" [OPTIONS] -- "$@"`.  At run time `--wrap` takes the command name from the wrapper's basename and resolves it in `PATH`, skipping the wrapper itself (compared by device/inode) and any other `tfil` wrapper, so no infinite recursion occurs regardless of `PATH` order; when the real command is not found, the wrapper exits with status 127.  Because the script does not embed a command name, renaming or symlinking a wrapper retargets it: `ln -s claude ~/bin/gemini` yields a `gemini` wrapper with the same options.

Positional arguments given after `--` with `--create-wrapper` are embedded as fixed leading arguments.  An existing file is only overwritten when it is a `tfil`-generated wrapper (recognized by its `# tfil-wrapper` marker line) or `--force` is given.  After writing, `tfil` warns when a wrapper is unreachable via `PATH` or shadowed by another executable.

### Debugging

`--debug-dump=FILE` appends the unfiltered PTY output stream to a file.  Set `TFIL_DEBUG_DUMP` to use a default file without passing the option; an explicit `--debug-dump` takes precedence.

## Composition

When stacking with other PTY wrappers (such as [claude-chill](https://github.com/davidbeesley/claude-chill)), put `tfil` on the outside so its filters see the original byte stream before any re-rendering layer normalizes it:

```console
% tfil --strip-ink-fake-cursor claude-chill claude
```

## Author

Copyright (c) 2026 Akinori Musha.

Licensed under the MIT license.  See `LICENSE` for details.

Visit the [GitHub Repository](https://github.com/knu/tfil) for the latest information.
