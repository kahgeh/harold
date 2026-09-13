# Isolated install acceptance

Tested on the current Mac using a temporary custom prefix, separate loopback ports,
no configured agent providers, a nonexistent fixture Messages database, and synthetic
channel settings. No notification RPC was sent.

```text
Fresh install: ready PID 95894 on 127.0.0.1:55271; managed store/env and no login plist verified.
Normal install: same database inode, marker, and local config retained; ready PID 76336.
Manual stop/start: stopped status failed readiness; explicit start restored readiness.
Port change: restart refreshed managed endpoint; installed hook startup resolved 127.0.0.1:55569 without notification RPC.
Fresh reinstall: new database, retained settings, old database+marker archived, external store untouched; ready PID 77845.
Final revised installer: Cargo-reported artifacts installed; exact installed executable owns PID 10008 and readiness passed.
Release privacy probes: both modes redact invalid credential sentinel; no success output.
Real unrelated-listener test: installer failed before bundle replacement and left the listener alive.
Cleanup: stopped the exact test service, verified launchd job absent and port closed,
then removed all generated test installation/backups. Production installation untouched.
```
