# TODO

- [ ] When updating vt100, recheck the wide-character resize fix from
  <https://github.com/doy/vt100-rust/pull/41> and the screen resize behavior.
  The published atuin-vt100 dependency includes the fix.  Revisit the minimum
  two-column workaround in src/codex_mouse_ui.rs when updating the dependency.
  Keep the resize regression tests and coordinator panic cleanup.
