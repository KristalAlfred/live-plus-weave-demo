# greenroom

A guest-invite service for a live broadcast. It sits outside both
[open-weave](https://github.com/KristalAlfred/open-weave) and
[open-live](https://github.com/KristalAlfred/open-live) and is the glue that
makes a remote guest's camera show up in an open-live production on its own:

1. An operator creates an invite and gets a private link.
2. The guest opens the link. The page becomes an open-weave media node and
   takes their camera and microphone.
3. open-weave tells greenroom the node registered. greenroom declares a stream
   from that camera to a gateway node over open-weave's northbound API.
4. open-weave plans the hop, the page pushes WHIP to the gateway, and the
   gateway publishes an SRT output.
5. open-live's weave source provider discovers that output and materialises it
   as a source. The operator assigns it and takes the guest on air.

Nothing in open-weave or open-live changed for this. greenroom is the missing
writer: open-live's provider only reads weave, and weave only plans what
somebody declares.

```
  operator ──> greenroom ──────── POST /v1/streams ──────> weave northbound
                  │  ▲                                          │
   guest ─────────┘  └── POST node.registered ── weave controller
     │                                                          │
     │  register / heartbeat / desired (proxied by greenroom) ──>│ weave southbound
     │                                                          │
     └── SDP offer, straight to the gateway ──> Strom /whip ──> SRT ──> open-live
                                                                        (polls weave,
                                                                         creates a source)
```

## The guest holds no weave credential

open-weave's own browser node takes the shared southbound token in its URL
fragment. A guest is not an operator, so greenroom does not do that. The page
calls `/api/session/{invite}/…` on greenroom, which verifies the invite,
**pins the node id to the seat**, and forwards the call with the real token.
Consequences worth knowing:

- Southbound never needs to be reachable from a guest's browser, and
  `WEAVE_SOUTHBOUND_CORS_ORIGIN` is not needed.
- The page carries no protocol version and no capability list. greenroom builds
  the registration from `weave-core`, so it cannot drift from what the control
  plane accepts.
- A guest cannot register as another guest's seat, because the seat comes from
  the signed invite rather than from the request.

The WHIP offer still goes from the browser straight to the gateway node. That
is the media path, and greenroom is not in it.

## Invite links

A link is `/j/<seat>.<expiry>.<tag>`, where the tag is HMAC-SHA256 over
`<seat>.<expiry>` truncated to 128 bits. The seat is readable so an operator
can tell two links apart; the tag is what makes a guessed seat useless. The
token is in the path because greenroom is its intended audience — unlike the
weave token, which the reference page keeps in the fragment to stay out of
server logs.

The seat id is the whole identity. It is the weave node id, the weave stream
name and, through open-live's provider, the source document id
(`src-ext-weave-<seat>-0`). One seat, one stream, one source, stable across a
guest rejoining — which is why the id is held to `[a-z0-9-]`.

## Webhooks are a fast path, not a dependency

open-weave has no per-node webhook subscription: the controller takes **one**
receiver URL in `WEAVE_WEBHOOK_URL` and delivers every node's
`node.registered`, `node.online` and `node.offline` to it. So there is nothing
to subscribe to per guest — greenroom is simply running before anyone joins,
and filters deliveries by seat.

A delivery does not act directly. It nudges the reconcile loop, which reads
southbound `GET /v1/nodes` and northbound `GET /v1/streams` and decides for
itself. A duplicate, a stale, or a lost event all converge to the same place,
and the loop runs on its interval regardless. Registration through the session
proxy nudges it too, so a demo with no webhook configured still works — you
just wait up to one interval instead of ~10 ms.

## Running it

greenroom needs open-weave **cloned beside this repo**: it depends on
`weave-core` by path (`../open-weave/crates/core`) so the registration payload,
the stream definition and the webhook event are the control plane's own types
rather than copies that can drift.

Against a running open-weave bench and open-live, the defaults in the justfile
need no configuration:

```sh
cd ~/git/open-weave && just bench up          # weave: 29080 / 29081 / 29082
cd ~/git/open-live  && just up-weave          # open-live on 3000, its Strom on the bench network
cd ~/git/live-plus-weave-demo
just hook                                     # point weave's node webhooks here
just run                                      # http://localhost:8090
```

Open `http://localhost:8090`, create an invite, and open the link. The guest
page needs a secure context for the camera: `https`, or `http` on
`localhost`/`127.0.0.1`.

`just hook` **rebuilds** the bench image before recreating the weave services.
The bench image is only as new as the last `just bench up`, and a controller
predating open-weave's webhook commit accepts `WEAVE_WEBHOOK_URL` and emits
nothing. All the weave services share that one image, so they are recreated
together — a new controller serves desired hops an older adapter cannot decode.
`just unhook` sends the webhooks back to `just bench hook-sink`.

`just status` prints what the three systems say about each other.

### A guest without a second machine

```sh
cd check && pnpm install
node guest.mjs --name "Ada" --stay
```

Creates an invite, opens the link in Chromium with a fake camera, presses Join,
and prints the chain as it advances. `--stay` keeps the guest on air.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `GREENROOM_ADDR` | `127.0.0.1:8090` | Listen address. |
| `GREENROOM_PUBLIC_URL` | `http://localhost:8090` | Base URL join links are built from. Guests must reach it. |
| `GREENROOM_SIGNING_KEY` | generated | Signs invites. Generated at boot when unset, which invalidates links from an earlier run. |
| `GREENROOM_WEBHOOK_TOKEN` | none | Bearer the controller must present. Required unless `GREENROOM_AUTH_DISABLED=1`. |
| `GREENROOM_STATE` | `greenroom-state.json` | Seats are written here, so a restart keeps its invites. |
| `GREENROOM_ASSETS` | unset | Serve the pages from this directory instead of the embedded copies. |
| `GREENROOM_RECONCILE_SECS` | `3` | Reconcile interval. |
| `GREENROOM_INVITE_TTL_SECS` | `3600` | Default link lifetime. |
| `GREENROOM_TARGET_NODE` | `strom-node-1` | The gateway node a guest's camera is delivered to. |
| `GREENROOM_TARGET_NETWORK` | `docker-host` | Which of that node's data-plane addresses the signalling URL is built from. |
| `GREENROOM_TARGET_LATENCY_MS` | `200` | SRT latency on the gateway's output. |
| `WEAVE_NORTHBOUND_URL` / `_TOKEN` | `http://127.0.0.1:9080` | Where streams are declared. |
| `WEAVE_SOUTHBOUND_URL` / `_TOKEN` | `http://127.0.0.1:8081` | Where the session proxy forwards. |
| `OPEN_LIVE_URL` | `http://127.0.0.1:3000` | Read-only, to report whether the source landed. |
| `OPEN_LIVE_API_KEY` | unset | Only if open-live has `API_KEY` set. |

`GREENROOM_TARGET_NETWORK` decides whether the guest's browser can reach the
gateway at all. On the bench, `docker-host` is the alias whose signalling base
is `http://localhost:28080`; the `default` alias is `172.26.0.10:8080`, which a
browser on the Docker host cannot dial.

The operator page and its API are **unauthenticated**. Anyone who can reach
`GREENROOM_ADDR` can invite a guest and read every link. Put it behind
something before exposing it.

## The five steps

Both pages render one `chain` the gateway derives, so they cannot disagree:

| Step | Reads | Turns green when |
|---|---|---|
| `invited` | the seat store | always |
| `joined` | southbound `GET /v1/nodes` | the node is registered and heartbeating |
| `routed` | northbound `GET /v1/status` | weave has placed the stream onto a gateway node |
| `flowing` | the stream's status, plus the page's own hop report | weave reads media as moving end to end |
| `open-live` | open-live `GET /api/v1/sources` | the provider has created the source |

`flowing` needs a *consumer*: the gateway's SRT output only moves bytes once
something dials it, which means an activated open-live production with the
guest assigned. Until then the step sits amber showing the browser's own
outbound rate — the guest is sending, nothing is pulling. That is the honest
reading, not a fault.

The page's hop report is only trusted while its node is heartbeating, so a
guest who has left is not reported as still sending.

## Out of scope

**greenroom does not assign the source to a mixer input.** open-live reads a
production's source assignments only while building the Strom flow at
activation (`activateStromFlow`, `open-live/src/lib/flow-generator.ts:70-81`),
and its controller WebSocket accepts no message that adds an input —
`InboundMessageSchema` (`open-live/src/ws/controller.ts:138-201`) is CUT,
TRANSITION, TAKE, audio, PiP and effects. Adding a guest to a *running*
production therefore means deactivate and reactivate. Assigning before
activation does work, so a "assign new guests to the next free input" step is
possible — it just cannot help a production already on air, so greenroom stops
at making the source appear.

Also not here: a return feed to the guest (open-weave's browser node has the
`whep → device` half already), per-guest weave tokens, and any signature on
webhook bodies beyond the bearer.

## Sharp edges

- **H.264 only.** Strom's `whip_input` accepts H.264 video. The guest page pins
  the video transceiver's codec preferences to H.264 and fails the hop with a
  plain-language message if the browser has none, rather than connecting and
  sending nothing usable.
- **Camera and microphone both.** A video-only WHIP sender starves the
  gateway's flow. The page requires both and offers no video-only mode.
- **One session per WHIP endpoint.** A guest who reloads can be answered `503`
  for 10–20 s while the previous session is torn down. The page retries and
  shows "reconnecting" rather than an error.
- **A gateway node that has not provisioned yet answers `404`.** Expected: the
  page gets the planned URL before the adapter has built the flow, and retries.
  It shows up once in "Connection detail" on a normal join.
- **Empty inputs hold a production at `Paused`** in open-live. Do not assign a
  guest's source before the guest is actually sending.

## Layout

```
src/invite.rs      HMAC-signed links and seat ids
src/seat.rs        the seat store, persisted as one JSON file
src/weave.rs       northbound and southbound clients, built on weave-core types
src/live.rs        open-live, read-only
src/reconcile.rs   the one thing that changes anything
src/snapshot.rs    the five steps, derived once per tick
src/routes.rs      operator API, guest session proxy, webhook receiver
assets/            the two pages, embedded in the binary
check/guest.mjs    a driven guest with a fake camera
```
