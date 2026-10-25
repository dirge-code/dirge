# Panel feed

dirge can subscribe to an external **panel feed**: a local HTTP
producer that streams Server-Sent Events. Each event opens, updates or
closes a panel in the left side panel, or posts a one-line
notification. dirge can answer the producer through a small reply
endpoint.

The feed is **off by default**. Nothing is contacted unless a
`panel_feed` block is configured.

## Configuration

In `~/.config/dirge/config.json` (or the project `.dirge/config.json`):

```json
{ "panel_feed": { "discovery_dir": "my-producer" } }
```

or, with a fixed endpoint:

```json
{ "panel_feed": { "url": "http://127.0.0.1:7777/feed", "token_file": "/run/user/1000/feed.token" } }
```

| key             | meaning                                                                                   |
|-----------------|-------------------------------------------------------------------------------------------|
| `enabled`       | `false` switches the feed off while keeping the rest of the block. Default: on when a source is set. |
| `discovery_dir` | Directory holding a discovery file named `dirge.json`. A relative path is taken under `$XDG_RUNTIME_DIR` (else the system temp dir). |
| `url`           | Explicit base URL. Takes precedence over `discovery_dir`.                                 |
| `token_file`    | File holding the token for `url` (surrounding whitespace is trimmed).                     |

The source is re-read on every (re)connect, so a producer that restarts
on a new port with a new token is picked up without restarting dirge.

### Discovery file

`<discovery_dir>/dirge.json`:

```json
{ "url": "http://127.0.0.1:43127/prefix", "token": "0123abcd...", "port": 43127, "pid": 4242 }
```

Only `url` (must be `http://` or `https://`; a trailing `/` is dropped)
and `token` (optional) are read; other keys are ignored.

Every file that carries a token (the discovery file or `token_file`)
must be a regular file, not a symlink, **owned by the current user**
and with **mode 0600** (no group or other bits). Anything else is
refused and retried later. The token is never logged; error messages
and debug output redact it.

## Transport

With base URL `<url>` and token `<token>` (percent-encoded):

| request                              | purpose                                     |
|--------------------------------------|---------------------------------------------|
| `GET  <url>/events?token=<token>`    | the event stream (`Accept: text/event-stream`) |
| `POST <url>/reply?token=<token>`     | one JSON reply (`Content-Type: application/json`) |

When there is no token, the `token` parameter is omitted. dirge never
sends an `Origin` header and never routes feed requests through a
proxy; producers should refuse any request that carries an `Origin`
and any request whose token does not match (e.g. `401`).

### Stream

Standard Server-Sent Events. dirge ignores the `event:` type: every
`data:` payload is one JSON object naming its own `op`. `id:` is
accepted and ignored, `retry: <ms>` sets the reconnect base delay
(clamped to 100 ms .. 60 s), and comment lines (`: ping`) serve as
heartbeats.

A stream silent for 60 s (no event, no comment) counts as dead.
Producers should send a heartbeat comment more often than that, e.g.
every 15 s.

Example frame:

```
id: 12
event: panel
data: {"op":"ui/notify","message":"build finished","level":"info"}

```

### Reconnection

On any failure (producer not running, refused connection, non-2xx
answer, stream end, parse error, idle timeout) dirge waits and
reconnects, forever, until it exits. The delay is exponential backoff
from the base delay (1 s, or the last `retry:`) capped at 30 s, with
jitter: each wait is uniformly between half and all of the current
step. A connection that delivered at least one event resets the
backoff.

When a connection ends, every panel it showed is closed, so no stale
state lingers. Producers should therefore replay the current state of
every live panel (their latest `ui/show-panel`) to each new
connection.

## Ops

Unknown ops, non-object payloads and ops missing a required field are
ignored; the stream continues. Field names contain a `/` where shown.

### `ui/show-panel`

Create panel `panel/id`, or replace its title and body.

```json
{ "op": "ui/show-panel",
  "panel/id": "builds",
  "title": "Builds",
  "lines": [ {"text": "Builds", "face": "title"},
             {"text": "", "face": "plain"},
             {"text": "api   passing", "face": "success"},
             "a bare string is a plain line" ] }
```

- `panel/id` (or `id`): required, non-blank.
- `title`: optional. Otherwise `doc["doc/title"]` (or `doc["title"]`)
  when a `doc` object is present, else the id.
- `lines`: array of strings or `{text, face}` objects. A `text`
  containing newlines becomes several rows with the same face. If
  `lines` is absent, a string `text` field is used as the body.
- If the first line equals the title with face `title`/`heading`, it
  (and one following blank line) is dropped, so the title is not
  painted twice.
- Any other fields (such as a structured `doc`) are ignored.

### `ui/close-panel`

```json
{ "op": "ui/close-panel", "panel/id": "builds" }
```

### `ui/focus-tab`

Create or retitle an accumulating (log-style) panel and focus it.

```json
{ "op": "ui/focus-tab", "panel/id": "log", "title": "Activity" }
```

### `ui/append-tab`

Append one line to a log-style panel, creating it when absent.

```json
{ "op": "ui/append-tab", "panel/id": "log", "line": {"text": "step 3 done", "face": "muted"} }
```

`line` may also be a bare string, or the line can be given as top-level
`text` and `face`.

### `ui/notify`

```json
{ "op": "ui/notify", "message": "deploy finished", "level": "warn" }
```

`message` (or `text`) is required; `level` is `info` (default), `warn`
or `error`. Terminal escape sequences are stripped.

### Faces

| face                                         | painted as |
|----------------------------------------------|------------|
| `title`, `heading`, `link`, `hunk`, `info`, `accent` | accent |
| `success`, `added`, `ok`                     | success    |
| `warn`, `warning`                            | warning    |
| `error`, `removed`                           | error      |
| `muted`, `dim`, `comment`                    | dimmed     |
| `plain`, `code`, anything else, absent       | normal     |

All producer text is sanitised (control and escape sequences removed)
and bounded in length and count by the panel layer.

## Replies

dirge POSTs one JSON object per request to `<url>/reply`. Any 2xx
(typically `204 No Content`) is success.

```json
{"action": "focus", "target": "<item id>"}
{"action": "unfocus"}
{"action": "next-tab"}
{"action": "prev-tab"}
{"action": "refresh"}
```

- `focus`: focus the item `target` (an id the producer showed).
- `unfocus`: leave the focused view.
- `next-tab` / `prev-tab`: move between the producer's views.
- `refresh`: ask the producer to repaint everything it shows.

Producers should accept and ignore actions they do not know.

From the TUI, `/panel next`, `/panel prev`, `/panel refresh`,
`/panel unfocus` and `/panel focus <id>` send these replies, and the
global keys Alt+. (next), Alt+, (previous) and Alt+/ (refresh) do the
same without leaving the prompt (rebindable as `panel_next_tab`,
`panel_prev_tab` and `panel_refresh`). A reply that fails (no feed
running, the producer unreachable or answering non-2xx) is shown as a
notification in the chat area.

## Swarm grid

The left side panel shows external panels as compact boxes. `/swarm`
(or Alt+S, rebindable as `toggle_swarm`) opens the swarm grid instead:
every external panel painted at full size, one cell per panel, over
the chat and both side panels, followed by one cell per in-flight
subagent. The input strip and status line stay below it. `/swarm on`
and `/swarm off` open or close it explicitly.

A subagent cell is titled with its agent profile (or `subagent`) and
its short id. Its body is the `[AGENTS]` preview line (`↳ elapsed · N tools ·
<last tool call>`) followed by the newest lines of the
subagent's chat tab, so the grid shows what every subagent is doing
without switching tabs. The cell disappears when the subagent
finishes; its chat tab keeps the transcript.

The grid repaints the latest state the producer sent; it keeps no
history of earlier frames. A panel the producer focused (`FocusTab`) is
painted first and marked `●`; an accumulating panel (`AppendTab`) shows
its newest lines. Each cell's title carries its number (`2/5`), and the
selected cell is marked `▸` and drawn in the accent colour. When not all
cells fit, the grid shows the page that holds the selected cell.

While the grid is open, keys drive it rather than the prompt:

| Key | Action |
|-----|--------|
| Tab / Shift+Tab | reply `next-tab` / `prev-tab` |
| r | reply `refresh` |
| Enter | panel: reply `focus` with the selected panel's id as `target`; subagent: close the grid and switch to its chat tab |
| m | subagent: close the grid and start `/msg <id> ` in the editor |
| u | reply `unfocus` |
| Arrows, h/j/k/l, 1-9, Home/End | move the selection (local, no reply) |
| Esc, q, Alt+S | close the grid |

These are the same replies `/panel` sends, over the same channel. Other
global keys (scrolling, Ctrl+L, Alt+. / Alt+, / Alt+/) keep working,
and Ctrl+C still interrupts a running agent. A permission prompt or
question from the agent takes the keys while it is shown.
