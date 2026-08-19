# The tracker API

`GET /api/tracker` and `GET /api/static_tracker` (HANDOFF P15). These mirror the reference
WebHost's endpoints of the same names, field for field, so that a tracker page written against
`archipelago.gg` works against a pahoa room with only its base URL changed.

The difference is where the data comes from. WebHost reads a database that a room periodically
saves into; pahoa *is* the room, so the same document is assembled from live state. That is an
advantage the reference cannot have, and the [future direction](#the-live-tracker) below is about
spending it properly rather than pretending a poll is live.

## Compatibility first

The initial implementation is a faithful mirror, including the parts that are only the way they
are because of how Flask serializes Python. Deviating anywhere makes the endpoint a *different*
API that merely resembles the reference, which is the one thing it must not be.

Three details that are easy to get wrong, all confirmed against a live 185-slot room:

- **`NetworkItem` and `Hint` serialize as arrays, not objects.** They are `NamedTuple`s falling
  through Flask's encoder, so an item is `[item, location, player, flags]` and a hint is
  `[receiving_player, finding_player, location, item, found, entrance, item_flags, status]`.
  pahoa's `NetworkItem` has a custom `Serialize` that emits a *map*, because that is what the
  WebSocket protocol wants — so the tracker needs its own serialization and cannot reuse the wire
  type. pahoa's `Hint` field order already matches the reference exactly.
- **Timestamps are RFC 1123**, not RFC 3339: `"Mon, 17 Aug 2026 18:22:09 GMT"`, and `null` when no
  connection has been made. This differs from `/admin/v1/status`, which is RFC 3339 — the tracker
  matches the reference and the admin API matches its own contract.
- **`static_tracker.datapackage` is only a checksum manifest**, `{game: {checksum, version}}`, not
  the packages themselves. 26 KB for 99 games. A tracker page fetches the real data separately and
  caches it by checksum.

**Which slots appear where.** The reference walks two different sets and the difference is
invisible until a seed has a spectator or an item-link group in it: `get_all_players()` — players
only — feeds every per-player array, while `get_all_slots()` feeds `hints` alone. A spectator has no
progress to report and a group has no client behind it. pahoa mirrors that split; see
`MultiData::player_slots` against `MultiData::connectable_slots` for the same distinction on the
room's own surfaces.

`player_items_received` comes from the **remote** item queue — `(team, player, True)` in the
reference — not the combined one.

`activity_timers` and `connection_timers` are **saved**, at whole-second resolution, which is all
RFC 1123 can carry. An async routinely outlives the process serving it, and a room that restarted
reporting "never connected" for every slot would lose the one signal that distinguishes an
abandoned slot from an active one. The reference persists them for the same reason
(`MultiServer.py:667-670`). A slot that has genuinely never acted still reports `null` rather than
a zero that would render as 1970.

## Who may read it

**The tracker is gated behind the admin token whenever one is configured** — not only for
`race_mode` seeds.

The reference restricts race rooms because its tracker links are handed out publicly. pahoa's
exposure is different and, left open, worse in one specific way: the endpoints sit on a public port
with no authentication, so an **anonymous port scan can iterate rooms and read the participant list
out of each one**.

The risk that creates is *identification*, not the names themselves. Running a room without a
password is common — the reference makes it the default and most groups never change it — and what
protects those rooms is that a scanner cannot tell which one is worth attacking. There is no index
from a port number to whose game it is. An open tracker supplies exactly that index: sweep the port
range, read the slot lists, find the one containing a high-visibility player, and now a stream's
multiworld is a known address with an open door. Griefing it costs one connection.

So the gate is not protecting slot names as private data. It is preserving the property that a room
on a public port is anonymous until its operator chooses otherwise, which is the property an
unpassworded room has been silently relying on all along.

So the rule is about deployment rather than seed:

- **No admin token configured** — a standalone pahoa — and the tracker is open. This is the case the
  CORS headers exist for, and it stays browser-fetchable.
- **A token configured**, which is what an orchestrated room has, and the tracker requires it like
  the rest of the admin surface. An orchestrator that proxies the tracker server-side holds the
  token already and is unaffected.
- **`--open-tracker`** restores the open behavior for an operator who wants both an admin API and a
  public tracker.

`race_mode` is parsed and available, and deliberately does **not** enter into this: gating on the
seed would leave the ordinary case open to the scan, and the ordinary case — an unpassworded room
that expects to go unnoticed — is the one with the most to lose from being findable.

## CORS

Both endpoints send `Access-Control-Allow-Origin: *`, as the reference does
(`WebHostLib/api/__init__.py:15-16`, and confirmed on the live endpoint).

One intended deployment is that an orchestrator serves the tracker's static assets and its
JavaScript fetches from the room, which is cross-origin: a different port alone is enough to make
it so. Since these are plain `GET`s with no custom headers they are *simple requests*, so there is
no preflight and no `OPTIONS` handler to write — the one response header is the whole of it.

Two constraints that follow, and are worth not tripping over later:

- **A gated tracker is not browser-fetchable.** Sending `Authorization` makes the request
  non-simple, which needs a preflight pahoa does not answer. That is the trade accepted above: an
  orchestrated room's tracker is fetched server-side by something holding the token, and only the
  open cases — standalone, or `--open-tracker` — are reachable from a page.
- **`*` and credentials are mutually exclusive.** If cookies were ever needed the wildcard would be
  rejected by the browser, and pahoa would have to echo the specific `Origin` and add
  `Access-Control-Allow-Credentials`. Nothing here needs credentials.

Unrelated to CORS but fatal in the same place: an `https://` page cannot fetch an `http://` URL at
all. The room must be serving TLS with a browser-trusted certificate, so cert-manager rather than
the self-signed pair used in development.

## Caching

`/api/tracker` is **60 seconds**, `/api/static_tracker` is **300** — the same windows the reference
memoizes with.

This is not premature: the live document measured **2.7 MB** for a 185-slot room, dominated by
`player_items_received` and `player_checks_done`, and assembling it walks every slot on the actor.
Without a cache, every open tracker tab becomes steady background work on the single task that owns
room state, which is the one thread in pahoa that must not become a bottleneck.

The staleness is bounded and is exactly what `archipelago.gg` already gives, so no existing tracker
page can tell the difference. The cache holds the *rendered* JSON, so repeat requests cost a memcpy
rather than a serialization.

## The live tracker

The polling API above is for compatibility. The direction worth building toward is a **WebSocket
path a tracker connects to, which sends the current state once and then streams deltas** for as
long as it stays connected.

That is the thing pahoa can offer and a database-backed WebHost structurally cannot: the room
already knows the moment a location is checked, because it is the thing that processed it. A
tracker that holds a connection would see a check land in the same tick the sending client did,
with no polling interval and no 2.7 MB re-render — the delta for one check is a few dozen bytes.

Shapes worth carrying over when it is built:

- The scoped feed (`docs/scoped-feed.md`) already established the machinery for *routing* messages
  to interested connections as state changes, rather than filtering afterwards. A tracker
  subscription is the same problem with a different audience, and should reuse that rather than
  grow a parallel one.
- The initial snapshot should be the same document `/api/tracker` returns, so a client has one
  parser rather than two and can fall back to polling where the socket is unavailable.
- Deltas want to be additive and idempotent — "these locations are now checked", "these items were
  received" — so a client that misses one and reconnects can re-request the snapshot and be
  correct, rather than needing an ordered log.

Until then the polling endpoints are the contract, and they are the fallback afterwards.
