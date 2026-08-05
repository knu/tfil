# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
While on `0.x.y`, the minor version bumps for new features and the patch
version bumps for fixes.  Once `1.0.0` ships, the project will revisit and
likely adopt strict [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [Unreleased]

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

