# tfil

A PTY proxy with configurable terminal output filters.

`tfil` runs a command inside a pseudo-terminal and rewrites its output stream on the way back to your terminal.  Use it to clean up specific escape-sequence quirks emitted by interactive TUIs without modifying the program itself.

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

With no filter flags, `tfil` is a transparent PTY proxy.  Each flag enables one filter on the output stream.  Hyphenated arguments after `<COMMAND>` are passed through to the child without needing `--`:

```console
% tfil --strip-ink-fake-cursor claude --resume
```

### Filters

- `--strip-cursor-shape` — Drop DECSCUSR (`CSI Pn SP q`) so child programs cannot change the terminal's cursor shape.
- `--strip-ink-fake-cursor` — Strip [Ink](https://github.com/vadimdemedes/ink)'s fake-cursor sequences (`\x1b[7m{grapheme}\x1b[27m` and friends), and suppress `\x1b[?25l` so the terminal's native cursor shows through.  Useful with Ink-based TUIs such as Claude Code, Gemini CLI, or ccmanager.
- `--strip-osc-titles` — Drop OSC 0/1/2 sequences (icon name and window title).  Other OSCs (4 = palette, 8 = hyperlink, 52 = clipboard, ...) are passed through.  Both ST (`ESC \`) and BEL terminators are recognized.

## Composition

When stacking with other PTY wrappers (such as [claude-chill](https://github.com/davidbeesley/claude-chill)), put `tfil` on the outside so its filters see the original byte stream before any re-rendering layer normalizes it:

```console
% tfil --strip-ink-fake-cursor claude-chill claude
```

## Author

Copyright (c) 2026 Akinori Musha.

Licensed under the MIT license.  See `LICENSE` for details.

Visit the [GitHub Repository](https://github.com/knu/tfil) for the latest information.
