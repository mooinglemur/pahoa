# The scoped feed, and why it is a second port

`--filtered-port` (HANDOFF P5). **Implemented.** This is the design and the reasoning behind it;
what shipped follows it exactly, and the measurement at the end is from a real room.

A second listening port on which a client receives only the messages relevant to its own slot.
APX reportedly had something like this, but it was never publicly available, so there is no
reference implementation to port — this is a design task rather than a translation.

## Why a port rather than a path or a tag

The property that decides it: **it needs no client changes and no protocol extension.** An
unmodified client pointed at the scoped port transparently gets a quieter feed.

That rules out both alternatives. A `Connect` tag requires client support, and the clients that
most need this are exactly the ones nobody is going to modify — they cannot handle the `PrintJSON`
firehose *and* have no mechanism for choosing a URL path either. A path on the one port has the
same problem for the same reason.

So the port is the interface, and the server applies the policy on the client's behalf.

## What the filter keeps

The filter is easy to over-apply, and most of these are load-bearing:

- `RoomInfo`, `Connected`, `DataPackage`
- the slot's own `ReceivedItems`, and its own `RoomUpdate` / `checked_locations`
- `Retrieved` / `SetReply` for its own subscriptions
- **`Bounced` — DeathLink depends on it**
- **chat, in full.** Every `Say` from every slot reaches the scoped port unfiltered. This is settled
  rather than provisional and is not configurable: chat is what makes a room a room, it is
  low-volume next to the item feed it is being separated from, and a port that quietly swallowed it
  would read as broken rather than as quiet. The filter is about the `ItemSend` firehose, and chat
  is not part of that firehose
- `CommandResult` and countdowns, on the same reasoning
- hints where it is the finding or the receiving player
- responses to its own `!` commands
- goal / release / collect announcements, kept **room-wide**. A release already sends the releasing
  player's items down the scoped feed, so the recipient learns what they got either way, and the
  one-line announcement is cheap

What it drops is the bulk of the firehose, and only that: `ItemSend` between other slots,
`Join`/`Part` spam for other slots, and other players' hints. **A message a human typed is never
dropped.**

## `MyText` is a content filter, not an audience filter

This is the distinction the implementation turns on. `NoText` is an *audience* filter: `Recipients`
says **who**, the shard expands it against the membership it owns, and every recipient of a
broadcast therefore gets byte-identical bytes — which is exactly what makes encode-once,
share-to-6000 work.

A scoped connection on slot 42 needs a different **subset** of the ~140 `PrintJSON` packets inside
one feed frame. That cannot be a `Recipients` variant however it is spelled. It is the one place the
existing broadcast model genuinely does not stretch.

## It is a route, not a filter

`register_location_checks` already holds `(sender, receiver)` for every item as it builds the feed,
so it can append each message to per-slot buffers for those two slots as it goes, then flush the
non-empty ones. No filtering pass, no second traversal, no re-encoding.

Each `ItemSend` lands in at most two buffers, so scoped encoding costs roughly 2× the full feed's
message count — and only for slots that actually have a scoped listener, which the room can test
against the `by_slot` index it already maintains. With nobody on the scoped port it is one `if` and
no work at all.

The performance shape inverts on this port: scoped feeds are per-connection unique, so
compress-once-share-to-all does not apply. That is fine, because the volume per client is orders of
magnitude smaller. The full-feed port keeps the shared-frame path. Measure both.

Of the eleven `Recipients::AllText` emit sites, exactly **one** — the `ItemSend` chunking — needs the
routing treatment. The other ten are single messages that already carry a `slot` field, so they are
one-line audience decisions.

## The trap: `MyText` must not live in `tags`

`ConnectUpdate` calls `apply_tags`, which **replaces** the tag vector
([room.rs:492](../crates/pahoa-room/src/room.rs#L492)). Trackers send `ConnectUpdate` routinely — to
add `DeathLink`, for instance — so a server-applied tag would be silently wiped mid-session and the
client would fall back to the full firehose with no error anywhere.

The port-derived policy therefore has to be a **separate sticky field** on `Client`, set at accept
time. A client-sent `MyText` tag may still *raise* the policy for anyone opting in explicitly; the
port sets a floor that `ConnectUpdate` cannot lower.

## Both ports serve the same HTTP surface

When both listeners are active they are the same server, not a primary and a satellite: the HTTP
surface — `/healthz`, `/api/v1/room`, `/admin/v1/**`, the tracker — is served identically on both,
and TLS terminates identically on both. Only the WebSocket feed differs.

That is a constraint on the HTTP work (P4) rather than on this: the router has to be a thing a
listener is given, not a thing wired into one listener.

Both ports share one `TlsAcceptor`, one `CertResolver` and one reload timer, so a renewal cannot
land on one port and not the other. `--allow-plaintext` and the `426` refusal apply to both. They
also share one connection-id counter: a `ConnId` names a connection to the room, and two listeners
minting the same one would be two clients the actor could not tell apart.

## What it measures

A 75-slot seed, one client watching slot 2 on each port, while slot 1 releases its whole world:

| | `ItemSend` received | room-wide lines |
|---|---|---|
| full port | 130 | all |
| scoped port | 4 | all |

The four are the items that actually involved slot 2. Repeating it with a release of a slot that
sends slot 2 nothing gives 94 against 0, while chat, the countdown and the release announcement
still arrive on both — the filter drops firehose and never anything a human typed.
