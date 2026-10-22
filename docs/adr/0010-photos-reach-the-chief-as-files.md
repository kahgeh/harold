# 10. Photos reach the chief as files

Date: 2026-10-23
Status: Accepted (web-app-photo). Builds on 0007.

## Context

The user wants to attach a photo or screenshot to a web app reply so the chief can look at it. The chief is a Claude Code session in a tmux pane, and Harold reaches it only by typing a line. Claude Code reads an image from a file path with its Read tool. Datastar actions send signals as JSON or a form, so they cannot carry a raw image body, and multipart parsing would add a crate.

## Decision

- The phone shrinks the photo to JPEG (longest side 2,048 px) and uploads it with `fetch` to `POST /replies/photo`. Harold checks its first bytes (JPEG or PNG), limits it to 5 MB, and stores it as `<data folder>/app-photos/<uuid>.<ext>`, folder 0700, file 0600. It never decodes the image and adds no crate.
- The reply names the upload by id. The stored `InboundMessageReceived` payload and the thread row carry the file name, and the line typed to the chief ends with `📷 <absolute path>`.
- Photos live as long as their thread row (30 days, the user's choice); an upload no reply names is deleted after 1 hour. The hourly prune deletes the files.
- The route answers the id as plain text, the one exception to answering through Datastar, because Datastar cannot send a raw body.

## Consequences

- A photo is a message to the chief: its content goes to a hosted model through the chief, as the user's words do.
- Photos sit unencrypted on the Mac's disk until pruned.
- Every Mac's Harold reads replies from its own `chat.db`, but a web app photo is stored on the Mac that serves the web app; the path typed is local to that Mac.
- The hostname-free path in the typed line ties the chief to the same Mac as Harold's data folder.
