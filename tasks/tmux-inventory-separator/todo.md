# Fix inventory framing (GitHub #3)

Evidence: tmux3.5a server_client_print applies octal/C-style escaping to output,
matching reported literal backslash037. Installed tmux3.6a returns raw separators
with locale, but replaces both unit separators and tabs with underscores when
locale is absent; -u preserves UTF-8 output without inherited locale.

Fix: use tab-separated seven-field rows and invoke tmux -u. Reject malformed or
extra fields instead of unescaping arbitrary path data. Preserve spaces, Unicode,
and literal backslashes. No schema/migration or scan-interval change.

- [x] Add regression parser tests and prove failure before the fix.
- [x] Update command framing and parser; add real isolated tmux round-trip test.
- [x] Full tests/Clippy and completion review.
- [ ] Publish v0.1.3 and verify downloaded binary against real isolated tmux.

Keep unrelated tasks/local-scan-intervals work untouched. No production restart or
provider hook changes; test tmux server and daemon must be isolated and cleaned.

## Local verification

- Parser regression first failed on old code, passed after tab framing change.
- 388 workspace Rust tests pass,3ignored; clean warnings-denied Clippy/fmt/diff.
- Real isolated round-trip test explicitly passes on tmux3.6a and tmux3.5a
  with absent, C, UTF-8 locales; preserves spaces, Unicode and literal backslashes.
- Exact3.5a command reproduces unit separator as literal backslash037; tab remains
  a byte. Built official source in a temporary directory only, checksum verified
  against historical Homebrew formula. No system package install.
- Official Homebrew/core tmux CI prerequisite audit approved; CI runs real test.
- Completion reviewer approved source/packaging with no findings.
