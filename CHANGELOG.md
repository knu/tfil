# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
While on `0.x.y`, the minor version bumps for new features and the patch
version bumps for fixes.  Once `1.0.0` ships, the project will revisit and
likely adopt strict [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.6.0] - 2026-11-25

### Added

- Make absolute `path:line` references clickable in the Codex preset and with `--link-file-paths`.  Existing links stay intact.  iTerm2 Semantic History can open the file at the referenced line.
- Add `--tmux-image-passthrough` to relay Kitty and iTerm2 images through an outer tmux pane.  Set `allow-passthrough on` in that pane.

## [0.5.1] - 2026-11-23

### Fixed

- Enable automatic terminal-string filtering for the Codex preset in tmux and herdr-core, and relay focused pointer-shape updates through tmux.  In a foreground tmux pane, the preset temporarily enables passthrough for image support.  Codex still starts if passthrough setup fails.
- Keep `--preset=codex` in generated wrappers so they select these defaults at launch.

## [0.5.0] - 2026-11-23

### Added

- Add `--preset=codex` for Codex mouse integration and Critty terminal marker notifications, and `--preset=claude` to remove Claude Code's fake cursor.
- Add `--filter-terminal-strings=auto` for tmux and herdr-core.  Preserve supported images and terminal status updates while gating pointer and clipboard controls on focus.
- Add `--tmux-managed-passthrough` to enable passthrough for the current pane while tfil runs and restore the previous setting on exit.
- Add focus-aware OSC filtering and optional notifications that report the child process's exit status.

### Changed

- Switch back to upstream vt100.

## [0.4.3] - 2026-10-15

### Fixed

- Restore mouse selection on Codex's sign-in picker.  Hover or click an option or its description to select it, then click to confirm.

## [0.4.2] - 2026-10-12

### Fixed

- Restore queued Codex question clicks when the shortcut hint uses the symbolic Shift+Left label `⇧←`.

## [0.4.1] - 2026-10-11

### Added

- Add Critty terminal marker notifications for the upcoming Critty release, formerly TCrit.  `--critty-notify` reports both `CRITTY-` and older `TCRIT-` markers, and `--tcrit-notify` remains an alias, so existing Codex wrappers keep working.

## [0.4.0] - 2026-09-27

### Added

- Add TCrit terminal marker notifications.  TCrit users should regenerate their Codex wrapper with `--tcrit-notify`.

## [0.3.0] - 2026-09-27

### Fixed

- Fix mouse compatibility with Codex 0.157
- Prevent backward PATH lookup in wrappers
- Fix wide-character resize crashes

## [0.2.9] - 2026-09-13

### Added

- Support queued Codex question clicks
- Support Codex worktree list mouse selection

## [0.2.8] - 2026-09-08

### Fixed

- Prevent mouse reports leaking after Escape

## [0.2.7] - 2026-09-06

### Fixed

- Fix Escape swallowing the next input character

## [0.2.6] - 2026-09-06

### Changed

- Streamline Ink fake cursor recognition
- Serialize session state through bounded queues
- Bypass coordination for streams without mouse UI
- Batch queued terminal output in bounded writes

### Fixed

- Protect terminal controls through shutdown
- fix: Preserve worker errors across queue disconnects
- fix: Cap deferred terminal controls at 1 KiB

## [0.2.5] - 2026-09-06

### Fixed

- Fix launching commands with relative paths
- Prevent terminal controls from splitting sequences

## [0.2.4] - 2026-09-02

### Fixed

- Prevent panic on one-column terminal resize

## [0.2.3] - 2026-08-29

### Changed

- Bypass PTY for noninteractive wrappers

## [0.2.2] - 2026-08-29

### Changed

- Recommend Claude and Codex wrapper setups

### Fixed

- Fix terminal output from `codex completion`

## [0.2.1] - 2026-08-06

### Added

- Add --create-wrapper to generate wrapper scripts

### Changed

- Document current terminal proxy features

## [0.2.0] - 2026-08-05

### Added

- Add --tmux-osc-passthrough filter
- Add --codex-mouse-ui for mouse-driven Codex menus

## [0.1.5] - 2026-05-13

### Fixed

- Recognize compound SGR openers as fake cursor starts

## [0.1.4] - 2026-05-06

### Changed

- Treat TFIL_DEBUG_DUMP as --debug-dump

## [0.1.3] - 2026-05-06

### Fixed

- Recognize fake cursors split across line breaks

## [0.1.2] - 2026-05-06

### Fixed

- Recognize compound SGR resets as fake cursor terminators

## [0.1.1] - 2026-05-06

### Added

- Add initial tfil implementation
