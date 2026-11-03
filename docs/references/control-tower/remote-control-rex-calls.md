# The Rex calls of the remote control

The remote control (`control-tower --remote`) runs in a floating Rex layer. Before it exits, it closes its own layer. This page records the two Rex API calls that it uses for that, as the installed Rex describes them.

The names and parameters come from `rex api describe session.view` and `rex api describe session.close_layer` on 2026-11-03. The sample below is a shortened `rex api call session.view` result from a real session on that day.

## How the remote finds and closes its layer

1. Read `REX_SESSION` (the session id) and `REX_BLOCK` (the block id of the remote). When one of them is missing, the remote skips the close.
2. Run `rex api call session.view '{"session_id":"<REX_SESSION>"}'`.
3. Find the layer whose `blocks` hold `REX_BLOCK`.
4. When that layer has the kind `tiled`, skip the close. Rex closes only a floating layer.
5. Run `rex api call session.close_layer '{"session_id":"<REX_SESSION>","layer_id":"<layer>"}'`.

Each call also gets `--autostart=false` and `--timeout=3s`, as every other control-tower call to `rex` does. A failure of any step is ignored, and the remote exits with code 0.

## `session.view`

Gets the windows of the session, their layers and their blocks in one snapshot.

| Input field | Type | Note |
| --- | --- | --- |
| `session_id` | string | Required. |

The result has `revision`, `session_id`, `label`, `active_window_id` and `windows`. A window has `window_id`, `label`, `focused_block_id` and `layers`, and sometimes `zoomed_block_id`. A layer has these fields, in this order:

| Layer field | Note |
| --- | --- |
| `layer_id` | The id of the layer, for example `layer:<uuid>`. |
| `kind` | `tiled` for the layer that fills the window, `floating` for a floating layer. Rex tells callers to treat an unknown kind as `floating`. |
| `bounds` | `[x, y, w, h]` as fractions of the window. |
| `layout` | A tree of `split` nodes. A leaf node holds a `block_id`. |
| `blocks` | The blocks of the layer. Each block has `block_id`, `label`, `creator_name`, `flavor`, `host_id` and sometimes `owner`. |

A block id is in the `layout` and in the `blocks` of the same layer. The key `focused_block_id` of a window is not the key `block_id`. The search reads each layer from its `layer_id` to the next `layer_id` or `window_id`.

Sample (ids and labels shortened; window 2 has a floating layer that holds the remote):

```json
{"revision":215,"session_id":"session:01a1","label":"harold","active_window_id":"window:01a2","windows":[{"window_id":"window:01a1","label":"pod","focused_block_id":"block:01b1","layers":[{"layer_id":"layer:01c1","kind":"tiled","bounds":[0,0,1,1],"layout":{"rect":[0,0,1,1],"split":{"split_id":"split:01d1","direction":"horizontal","ratio":0.4,"before":{"rect":[0,0,0.4,1],"block_id":"block:01b1"},"after":{"rect":[0.4,0,0.6,1],"block_id":"block:01b2"}}},"blocks":[{"block_id":"block:01b1","label":"files","creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","host_id":"9cc91d55","owner":"client:01e1"},{"block_id":"block:01b2","label":"pnem","creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","host_id":"9cc91d55","owner":"client:01e1"}]}]},{"window_id":"window:01a2","label":"keymap","focused_block_id":"block:01f9","layers":[{"layer_id":"layer:01c2","kind":"tiled","bounds":[0,0,1,1],"layout":{"rect":[0,0,1,1],"block_id":"block:01b3"},"blocks":[{"block_id":"block:01b3","label":"agent","creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","host_id":"9cc91d55","owner":"client:01e1"}]},{"layer_id":"layer:01c3","kind":"floating","bounds":[0.25,0.3,0.5,0.4],"layout":{"rect":[0.25,0.3,0.5,0.4],"block_id":"block:01f9"},"blocks":[{"block_id":"block:01f9","label":"remote","creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","host_id":"9cc91d55","owner":"client:01e1"}]}]}]}
```

The tiled layers, the `layout` tree and the block fields are as Rex returned them. The real session had no floating layer on that day, so the floating layer `layer:01c3` follows the schema of `session.view`.

## `session.close_layer`

Closes a floating layer and every block in it.

| Input field | Type | Note |
| --- | --- | --- |
| `session_id` | string | Required. |
| `layer_id` | string | Required. It must name a floating layer. |

The result is `{"revision": <n>}`. Rex refuses a `tiled` layer with `bad_request`, an unknown session with `session_not_found` and an unknown layer with `not_found`. The call returns once the blocks have closed, so the remote can end before the call returns. When the layer had focus, focus goes back to the layer that had it before.
