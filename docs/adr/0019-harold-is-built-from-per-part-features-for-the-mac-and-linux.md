# 19. Harold is built from per-part features, for the Mac and for Linux

Date: 2026-10-26
Status: Accepted (per-part-features). Amends 0008.

## Context

The user wants Harold on Omarchy, where there is no Messages app and no Apple GPU. Some parts of Harold need a Mac: iMessage and calls. The voice is a TTS server on each machine, not a feature.

## Decision

- Each platform part is one Cargo feature: `imessage` and `calls`.
- `mac-defaults` is the default set and the Mac package. `linux-defaults` is the set for the Linux package.
- A macOS-only feature built for another target stops at compile time.
- A setting that needs a missing feature is refused at start, with the setting named.
- The screen lock check and the service manager follow the operating system, not a feature.

## Consequences

- 0008's "iMessage only" now means "iMessage is the only text channel". A build without `imessage`, such as the Linux package, reaches the phone through the web app only.
- Every change must pass `scripts/check_features.sh`: `linux-defaults`, no feature and each feature alone. The Mac job, `make release` and the pod's merge checks run it.
- A Linux Harold does not share replies with the Macs.
- On Linux, `HAROLD_LAUNCHD=1` will mean that the systemd user manager started Harold. The name stays. The Linux build item records this.
