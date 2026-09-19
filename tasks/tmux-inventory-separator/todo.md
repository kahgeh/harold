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
- [x] Publish v0.1.3 and verify downloaded binary against real isolated tmux.

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

## Published release verification

- Released [v0.1.3](https://github.com/kahgeh/harold/releases/tag/v0.1.3)
  from `8adf13bede681ea2acd88fc79bcdce9be1785e3a`.
- [GitHub run 35439923083](https://github.com/kahgeh/harold/actions/runs/35439923083)
  passed 388 Rust tests, 35 Python tests, the explicit live test with tmux 3.7c,
  native macOS ARM64 build, package smoke checks, and asset upload.
  Promoted to latest only after the archive and checksum were available.
- Public `curl .../main/scripts/bootstrap.sh | sh` installation passed checksum
  and readiness checks in an isolated prefix. The downloaded daemon discovered
  a real tmux 3.5a pane and preserved the exact Unicode session name and working
  directory containing spaces and a literal backslash037 sequence.
- No inventory degradation was reported. The first acceptance harness incorrectly
  required an explicit initial healthy row; source confirms initial healthy state
  emits no health event. Corrected the harness and reran successfully.
- Temporary launchd service, listener, tmux server, installation, and compiled
  tmux 3.5a source directory removed.
  Production installation and local scan-interval settings were not changed.
- Final completion reviewer independently verified release assets, hosted tests,
  public installer acceptance, and cleanup; approved with no findings.
- Reporting-machine acceptance remains unverified; issue #3 remains open.

Evidence: `/tmp/harold-tmux-release-success.log`,
`/tmp/harold-tmux-public-acceptance.py`,
`/tmp/harold-tmux-public-acceptance.log` (local temporary logs).
