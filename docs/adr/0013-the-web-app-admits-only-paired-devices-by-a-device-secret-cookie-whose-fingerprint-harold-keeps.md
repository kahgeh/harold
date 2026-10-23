# 0013. The web app admits only paired devices, by a device-secret cookie whose fingerprint Harold keeps

Date: 2026-10-23
Status: Accepted (web-app-lockdown)

## Context

The web app admits any request that carries the configured Tailscale login, so any device signed in to that account passes. Harold has no way to tell the user's phone from another device on the account. The installed iPhone app keeps its cookies apart from Safari's, so a secret cannot be handed over by a link opened in Safari.

## Decision

A device is admitted only when it holds a device secret: a 32-byte random value set as the cookie `harold_device` (`HttpOnly; Secure; SameSite=Lax`, 400 days). Harold keeps only the SHA-256 fingerprint of each secret, with a name and times, in `web-app/devices.json` (mode 0600), and looks a request's cookie up by its fingerprint right after the login check. A device is paired inside the app: the user types a pairing code they chose, a fixed code of at least 6 characters kept as `HAROLD_PAIRING_CODE` in `config/pairing.env` (mode 0600, outside the repository, carried across installs by the installer, read at each attempt and never shown or logged), and `POST /pair` exchanges it for the cookie. Five wrong tries in all lock pairing for good (kept in `web-app/pairing-lock.json` with a salted fingerprint of the code), until the code is changed or `harold app pair --unlock` is run; a successful pairing resets the count. The code is compared in constant time. `harold app devices` and `harold app unpair` manage the list. A short, explicit list of static files is open so the pair screen can load. `[web_app] require_pairing = false` switches the check off and logs a WARN at every start-up.

## Consequences

- A device that was never paired, even one signed in to the user's Tailscale account, gets the pair screen or `401`.
- Reading Harold's files does not reveal any device secret; a copied cookie does, and is cut off by unpairing. The pairing code itself sits in `pairing.env` (mode 0600), readable by anyone who can run commands as the user, who could pair a device anyway.
- A fixed code can be guessed over time, so five wrong tries in all lock pairing until the user changes the code or unlocks it; a 6-digit code leaves a guesser about 1 chance in 200,000 before the lock.
- Anyone who can run commands as the user on the Mac can pair a device of their own; this stays as accepted in ADR 0002.
- A paired phone whose device is removed gets `401` and reloads to the pair screen.
- Tailscale's own device identity, access rules, rate limits and a refusal counter remain later steps.
