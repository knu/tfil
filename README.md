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

### Presets

```console
% tfil --preset=codex codex
% tfil --preset=claude claude --resume
```

- `codex` enables `--codex-mouse-ui`, `--critty-notify`, and `--link-file-paths`.  It uses automatic filtering in tmux and herdr-core, relays allowed pointer-shape updates through tmux, and manages passthrough in a foreground tmux pane for image support.
- `claude` enables `--strip-ink-fake-cursor`.

Outside tmux and herdr-core, the Codex preset leaves terminal-string filtering disabled unless you pass `--filter-terminal-strings` explicitly.  See [Automatic terminal string filtering](#automatic-terminal-string-filtering) for details.

### Output filters

- `--link-file-paths` — Turn absolute references such as `(/path/to/file.rs:497)` into OSC 8 links.  The visible path and line number stay the same.  The destination uses `file:///path/to/file.rs#497`, which iTerm2 can pass to Semantic History for line navigation.  [iTerm2 escape codes](https://iterm2.com/documentation-escape-codes.html)

- `--strip-cursor-shape` — Drop DECSCUSR (`CSI Pn SP q`) so child programs cannot change the terminal's cursor shape.
- `--strip-ink-fake-cursor` — Strip [Ink](https://github.com/vadimdemedes/ink)'s fake-cursor sequences (`\x1b[7m{grapheme}\x1b[27m` and friends), and suppress `\x1b[?25l` so the terminal's native cursor shows through.  Useful with Ink-based TUIs such as Claude Code, Gemini CLI, or ccmanager.
- `--strip-osc-titles` — Drop OSC 0/1/2 sequences (icon name and window title).  Other OSCs (4 = palette, 8 = hyperlink, 52 = clipboard, ...) are passed through.  Both ST (`ESC \`) and BEL terminators are recognized.
- `--strip-osc-unfocused=CODES` — Drop comma-separated OSC codes from child output until a focus-in report arrives, and after each focus-out report.  For example, `--strip-osc-unfocused=22` suppresses child pointer-shape updates while unfocused.  This works on plain OSCs before any tmux wrapping by tfil.  Automatic filtering also checks OSCs inside tmux passthrough.  It requires a child that enables focus reporting and a terminal that sends focus reports.  In tmux, enable `focus-events` too.  Reports are forwarded to the child as usual.  Focus filtering is best-effort and depends on when the report arrives.
- `--tmux-osc-passthrough=CODES` — Wrap the OSC sequences with the given comma-separated codes in a tmux `DCS tmux; ... ST` passthrough so they reach the outer terminal instead of being swallowed by tmux.  Requires tmux 3.3+ with `allow-passthrough on` set.
- `--tmux-image-passthrough` — Wrap bare Kitty graphics and iTerm2 inline image sequences for one tmux layer.  Use it when running a nested tmux client, whose output needs to pass through the outer tmux pane.  Enable `allow-passthrough on` in that pane.
- `--filter-terminal-strings[=MODE]` — Filter child control strings.  The default mode when the flag has no value is `strict`.  Set `--filter-terminal-strings=auto` to allow selected images and status updates in tmux and herdr-core.
- `--tmux-managed-passthrough` — Set `allow-passthrough all` on the current tmux pane for the lifetime of tfil.  A watchdog restores the pane's previous value or inheritance when tfil exits.  Pair it with `--filter-terminal-strings` when running a child whose output needs filtering.

### Terminal string filtering

`--filter-terminal-strings` and `--filter-terminal-strings=strict` work with or without tmux and apply the same filters regardless of focus.

- Drops all DCS strings, including tmux passthrough, and all APC, PM, SOS, and legacy window-name strings.
- Passes plain OSC 0, 1, 2, 7, 8, 10, 11, 12, and 133.  Drops the remaining child OSCs, including pointer shape (22), clipboard (52 and 5522), and notifications (9 and 777).

The filter recognizes both ESC-prefixed and C1 forms.  To drop only those five plain OSCs while unfocused, use `--strip-osc-unfocused=22,52,5522,9,777`.  Add `--strip-osc-titles` to drop ESC-prefixed OSC 0, 1, and 2 as well.

With `--codex-mouse-ui`, tfil sends its own pointer updates when focus is confirmed.  The child must enable terminal focus reporting.  In tmux, enable `focus-events` and add `--tmux-osc-passthrough=22` to forward these pointer updates.  The terminal string filter runs before OSC wrapping, and the combined flags wrap OSCs only in tmux.  Add `--notify-on-exit` to send tfil's own exit notification.

### Automatic terminal string filtering

```console
% tfil --preset=codex codex
```

- **tmux**.  The Codex preset sets `allow-passthrough all` on a foreground pane before Codex starts and restores the previous setting on exit.  If setup fails, tfil reports the error and starts Codex with the available passthrough setting.  Pass `--tmux-managed-passthrough` explicitly to require successful setup.  Codex needs fullscreen mode and a compatible tmux client to enable its image support.  tfil inspects the contents of each tmux passthrough and rebuilds the permitted sequences.  It checks Kitty image IDs against Codex's range for the pane.
- **herdr-core**.  tfil passes validated Kitty graphics and OSC 7501 directly to the core.  The managed option has no effect here.  An explicit `--filter-terminal-strings=auto` uses the strict filter outside both multiplexers.
- **Focus**.  tfil allows OSC 22 (pointer shapes) and OSC 52/5522 (clipboard) only after a focus-in report and until the next focus-out report, including inside tmux passthrough.  The child must request terminal focus reports.  Enable `focus-events` in tmux.  Use `--strip-osc-unfocused` to add codes.  tfil bases its decision on input reports, which can arrive late.  Clipboard APIs and commands such as `tmux load-buffer` operate outside this output filter.
- **Allowed output**.  Plain titles, tmux window names, and bells go to the multiplexer.  tfil allows valid OSC 9;4 progress, OSC 7501 status, and inline Kitty image commands regardless of focus.  The tfil mouse UI wraps its own pointer updates in tmux as needed.  `--tmux-osc-passthrough=22` can also wrap allowed pointer updates from the child.  tfil blocks child desktop notifications, other passthrough content, and Kitty file or shared-memory transfers.

### Using tmux passthrough with untrusted output

`allow-passthrough all` lets an invisible pane send terminal controls to the outer terminal.  The focus filter above handles selected plain OSCs from the child before tfil wraps them.  It leaves child-supplied DCS passthrough and tfil-generated controls outside its scope.  Focus reports can also arrive after the focus has changed.

If a pane displays untrusted output, keep `allow-passthrough off` for that pane unless a separate filter controls its output.  Use `--tmux-managed-passthrough` with `--filter-terminal-strings` for strict filtering while passthrough is enabled.

- Drop child-supplied DCS passthrough in every focus state.  Wrap only sequences the filter has validated.  Start with no other controls forwarded to the outer terminal.  Normal text, colors, and cursor movement can still be handled by tmux.
- Block OSC 52 and 5522 clipboard reads and writes from the child in every focus state.  In tmux, `set-clipboard external` also blocks the regular OSC 52 path from applications while retaining copies initiated by tmux.  [tmux clipboard documentation](https://github.com/tmux/tmux/wiki/Clipboard#changing-set-clipboard) · [Ghostty clipboard protocols](https://ghostty.org/docs/install/release-notes/1-4-0#kitty-clipboard-protocol)
- Keep child-supplied desktop notifications such as OSC 9 and 777 out of passthrough.  For background completion notifications, use `tfil --notify-on-exit`.  It sends a fixed notification with the exit status.  Leave titles, palette changes, working-directory updates, and graphics out of the outer-terminal allowlist until a specific use needs them.  [Ghostty notification settings](https://ghostty.org/docs/config/reference#desktop-notifications)
- If a TUI needs OSC 22 pointer updates, allow only the required pointer values, and gate those updates on focus.  Use the focus filter for display behavior.  Enforce clipboard and passthrough restrictions in every focus state.

### Desktop notification on exit

`--notify-on-exit` sends an OSC 777 notification after the child exits and terminal cleanup finishes.  tfil uses a fixed title and message, with only the numeric exit status taken from the child.  It sends the notification only to terminal stdout and returns the child's exit status as usual.

```console
% tfil --notify-on-exit -- make test
```

Inside tmux, tfil wraps its own notification in a DCS passthrough.  Use `allow-passthrough on` for visible panes or `allow-passthrough all` if notifications must also reach the terminal from hidden panes.  The outer terminal must support OSC 777 and have desktop notifications enabled.  [Ghostty notification settings](https://ghostty.org/docs/config/reference#desktop-notifications)

### Interactive enhancements

`--codex-mouse-ui` makes Codex CLI's `›`-marked numbered menus, including approval prompts and question forms, mouse-driven.  When Codex starts its interactive TUI and enables bracketed paste, `tfil` begins maintaining a screen model of the output and enables SGR any-motion mouse reporting.  Non-interactive commands do not emit mouse-reporting sequences.  Hovering over a numbered option steers Codex's own selection there with arrow keys, so the selection marker follows the mouse, and the mouse pointer takes a hand shape via OSC 22 on supporting terminals.  Clicking sends Enter to confirm the selection.

Collapsed `? N question(s)` rows under `Queued follow-up inputs` also show a hand pointer.  Clicking sends the shortcut shown in the hint: Shift+Left for `shift + ←`, `shift+←`, or `⇧←`, or Alt+Up for `⌥+↑` or `alt+↑`.  Other bindings are left to keyboard navigation.  Rows containing only Astra's star decoration are excluded from menu continuation detection.

The `Managed worktrees` browser also supports hovering and clicking unnumbered entries, including a single search result.  Detection requires the browser's heading, description, and standard Enter/Escape hint to remain visible; wrapped or ambiguous layouts fall back to keyboard navigation.

The onboarding sign-in picker, which marks its numbered options with an ASCII `>`, also supports hovering and clicking an option or its description.  Detection requires the picker's intro text and `Press enter to continue` hint to remain visible; wrapped or ambiguous layouts fall back to keyboard navigation.

Mouse events the menu logic does not consume are forwarded only when the child has enabled a mouse protocol of its own, using the encoding requested by the child.  This preserves Codex's native text selection, right-click copying, and scrolling outside detected menus.  Modified mouse motion is forwarded without steering menus.  When a menu click is consumed, its drag and release events are consumed as well.

```console
% tfil --codex-mouse-ui codex
```

To relay the OSC 22 pointer-shape updates through tmux, combine the mouse UI with `--tmux-osc-passthrough=22`:

```console
% tfil --codex-mouse-ui --tmux-osc-passthrough=22 codex
```

Some terminals manage the pointer shape themselves while mouse tracking is active, as tmux does with `mouse on`, and ignore OSC 22 in that state.

### Critty integration

`--critty-notify` helps [Critty](https://github.com/crittylabs/critty), a terminal code review tool, locate Codex running in tmux or Herdr.  Codex's daemon mode, enabled by default since [0.157.0](https://github.com/openai/codex/releases/tag/rust-v0.157.0), can prevent Critty from identifying the terminal directly.  tfil detects markers in Codex's output and notifies Critty of the terminal to use for the review.

```console
% tfil --critty-notify codex
```

Critty was formerly called TCrit.  `--tcrit-notify` remains an alias, and both options also notify `tcrit` of its older markers, so existing wrappers keep working with either version.

### Wrapper scripts

`--create-wrapper=PATH` writes a small shell script that runs the command named after its basename through `tfil` with the given options, instead of running a command.  The recommended setup for Claude Code and Codex CLI is:

```console
% tfil --create-wrapper=~/bin/claude --preset=claude
% tfil --create-wrapper=~/bin/codex --preset=codex
```

Typing `claude` or `codex` then transparently runs the real command under `tfil` with the recommended enhancements.  Generated Codex scripts keep `--preset=codex` to apply its defaults at launch.  `--create-wrapper` is repeatable, so brace expansion creates several wrappers with the same options at once:

```console
% tfil --create-wrapper=~/bin/{claude,gemini} --strip-ink-fake-cursor
```

The generated script calls `tfil --wrap="$0" [OPTIONS] -- "$@"`.  At run time `--wrap` takes the command name from the wrapper's basename and resolves it in `PATH` after the wrapper itself (identified by device/inode), skipping later aliases of itself and other `tfil` wrappers.  Commands before the wrapper are never selected, so wrappers that follow the same rule can be chained in either order.  If the wrapper is absent from `PATH` or no eligible command follows it, the wrapper exits with status 127.  Existing generated scripts continue to work after updating `tfil`.  Because the script does not embed a command name, renaming or symlinking a wrapper retargets it: `ln -s claude ~/bin/gemini` yields a `gemini` wrapper with the same options.

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
