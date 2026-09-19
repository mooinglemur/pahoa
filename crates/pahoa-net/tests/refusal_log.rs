//! What the server says when it refuses a client.
//!
//! Archipelago logs none of this, and pahoa inherited the silence. That is
//! survivable for a server an organizer runs in a terminal beside the client
//! that is misbehaving, and it is not survivable for one running in a pod whose
//! bug reports arrive thirdhand as "it disconnects". Two things were invisible:
//!
//! - the `InvalidPacket` the room sent, which went to the client and nowhere
//!   else; and
//! - the sentence behind [`CloseReason::ProtocolError`], the room's own
//!   account of which argument of which command it could not survive, which
//!   the dispatcher flattened into the static `"protocol error"` before anyone
//!   could read it.
//!
//! The seam is the same one the keepalive work needed, for the same reason: the
//! refusal is decided where the room is borrowed mutably, and the slot that
//! earned it can only be read once that borrow ends. So the sink collects and
//! the actor logs, exactly as it does for membership updates.
//!
//! Driven through the real actor loop rather than a `Dispatcher` in isolation,
//! because "the sink collected it" and "an operator can read it" are different
//! claims and only the second one is worth anything.

use pahoa_multidata::{LocationStore, MultiData, NetworkSlot, SlotType, Version};
use pahoa_net::actor::{ActorMsg, SaveConfig, run_with_saves};
use pahoa_net::budget::Budget;
use pahoa_net::shard::Shards;
use pahoa_proto::decode;
use pahoa_room::{ConnId, Recorder, Room, RoomOptions};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

// --- capturing the log ----------------------------------------------------

/// Every event the thread emitted, rendered as `field=value` pairs.
///
/// Hand-rolled rather than `tracing-subscriber` with a capturing writer: this
/// is forty lines against a dev-dependency and a parse of formatted output, and
/// asserting on *fields* is the point: `slot` being absent is the claim in two
/// of these tests, and a substring search over rendered text cannot tell an
/// absent field from one that happened not to print.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    /// Lines whose `message` is exactly `msg`.
    ///
    /// `message` is always the first field and records unquoted, so a prefix
    /// match is an exact one.
    fn with_message(&self, msg: &str) -> Vec<String> {
        let prefix = format!("message={msg} ");
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// The one line whose `message` is `msg`, or a panic naming what was there.
    fn only(&self, msg: &str) -> String {
        let found = self.with_message(msg);
        assert_eq!(
            found.len(),
            1,
            "want exactly one {msg:?} line, got {found:#?}; the whole log was {:#?}",
            self.0.lock().unwrap()
        );
        found.into_iter().next().unwrap()
    }
}

struct Collector(Log);

impl tracing::Subscriber for Collector {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        self.0.0.lock().unwrap().push(line);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// `record_debug` is the only one worth writing: every other `record_*` on
/// `Visit` defaults to it.
struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

// --- the room -------------------------------------------------------------

fn room() -> Room {
    let mut slot_info = BTreeMap::new();
    slot_info.insert(
        1,
        NetworkSlot {
            name: "P1".to_string(),
            game: "Archipelago".to_string(),
            slot_type: SlotType::Player,
            group_members: Vec::new(),
        },
    );
    let mut connect_names = HashMap::new();
    connect_names.insert("P1".to_string(), (0, 1));
    let data = Arc::new(MultiData {
        seed_name: "refusals".to_string(),
        generator_version: Version::new(0, 6, 2),
        minimum_server_version: Version::new(0, 1, 6),
        minimum_client_versions: HashMap::new(),
        slot_info,
        connect_names,
        locations: LocationStore::default(),
        precollected_items: HashMap::new(),
        precollected_hints: HashMap::new(),
        er_hint_data: HashMap::new(),
        spheres: Vec::new(),
        race_mode: false,
        slot_data: HashMap::new(),
        server_options: None,
        embedded_datapackage: BTreeMap::new(),
    });
    let (names, _) = data.resolve_datapackage();
    Room::new(data, Arc::new(names), RoomOptions::default(), 0.0)
}

const CONN: ConnId = ConnId(1);

/// A `Connect` frame for `P1`, with `password` and `game` under the caller's
/// control so the refusal paths either side of authentication are reachable.
fn connect(extra: &str) -> String {
    format!(
        r#"[{{"cmd":"Connect","name":"P1","uuid":"u","items_handling":0,
             "version":{{"class":"Version","major":0,"minor":6,"build":8}},{extra}}}]"#
    )
}

/// Drive `frames` through a real actor and return what it logged.
///
/// `authed` runs a `Connect` against the room directly first, so the packets
/// under test arrive from a client with a slot. Doing it here rather than
/// through the actor keeps the handshake's own traffic out of the log being
/// asserted on.
async fn run(authed: bool, frames: &[String]) -> Log {
    let mut room = room();
    let mut pre = Recorder::default();
    room.on_connect(CONN, &mut pre);
    if authed {
        for packet in decode(&connect(r#""password":null,"game":"Archipelago""#)).unwrap() {
            room.handle(CONN, packet, &mut pre);
        }
        assert!(
            room.client(CONN).is_some_and(|c| c.auth),
            "the fixture Connect should have authenticated"
        );
    }

    let log = Log::default();
    let guard = tracing::subscriber::set_default(Collector(log.clone()));

    let shards = Shards::spawn(1, 16, 0, Budget::new(1 << 20, 1 << 16));
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    for frame in frames {
        tx.send(ActorMsg::Packets {
            conn: CONN,
            packets: decode(frame).expect("the frames here are all well-formed JSON"),
            bytes: frame.len(),
        })
        .await
        .unwrap();
    }
    // Dropping the sender is what ends the loop: every message is already
    // queued, so the actor drains them and returns.
    drop(tx);
    run_with_saves(&mut room, shards, rx, SaveConfig::default()).await;

    drop(guard);
    log
}

const ANSWERED: &str = "refused a client's arguments";
const FATAL: &str = "dropping a connection over a packet the reference would raise on";

// --- the tests ------------------------------------------------------------

#[tokio::test]
async fn an_invalid_packet_is_logged_against_the_slot_that_earned_it() {
    // `"text" not in args or type(args["text"]) is not str` (`MultiServer.py:2176`):
    // answered, socket kept.
    let log = run(true, &[r#"[{"cmd":"Say","text":7}]"#.to_string()]).await;

    let line = log.only(ANSWERED);
    assert!(line.contains("team=0"), "{line}");
    assert!(line.contains("slot=1"), "{line}");
    // The inbound command, which the packet's own `original_cmd` need not
    // match: `Get` is answered as `Retrieve`.
    assert!(line.contains(r#"cmd="Say""#), "{line}");
    assert!(line.contains(r#"text="Say""#), "{line}");
}

#[tokio::test]
async fn the_reply_text_is_kept_beside_the_command_that_caused_it() {
    // The reference answers a malformed `Get` with the text `Retrieve`
    // (`MultiServer.py:2246`), the name of nothing any client sends, and a
    // dead end for anyone who greps for it. The line carries both, so the
    // wording a player quotes from their client still leads to the command.
    let log = run(true, &[r#"[{"cmd":"Get","keys":"a"}]"#.to_string()]).await;

    let line = log.only(ANSWERED);
    assert!(line.contains(r#"cmd="Get""#), "{line}");
    assert!(line.contains(r#"text="Retrieve""#), "{line}");
}

#[tokio::test]
async fn dropping_a_socket_records_why_the_room_gave_up() {
    // A non-string key reaches `args["key"].startswith(...)` and raises, so the
    // socket goes. What the room knew and could not say was *which* part of the
    // packet did it.
    let log = run(true, &[r#"[{"cmd":"Get","keys":[7]}]"#.to_string()]).await;

    let line = log.only(FATAL);
    assert!(line.contains("slot=1"), "{line}");
    assert!(line.contains(r#"cmd="Get""#), "{line}");
    assert!(
        line.contains("keys must be strings"),
        "want the room's own reason, got {line}"
    );
}

#[tokio::test]
async fn a_refusal_before_authentication_has_no_slot_to_name() {
    // `game` absent is the reference's one `Connect` precondition
    // (`MultiServer.py:1904-1907`), answered rather than closed on, and it
    // happens before there is a slot. Half of all refusals look like this, so
    // the line has to stay readable without one.
    let log = run(false, &[connect(r#""password":null"#)]).await;

    let line = log.only(ANSWERED);
    assert!(line.contains(r#"cmd="Connect""#), "{line}");
    assert!(
        !line.contains("slot="),
        "an unauthenticated connection has no slot: {line}"
    );
    assert!(!line.contains("team="), "{line}");
}

#[tokio::test]
async fn a_refused_connect_never_writes_the_password_it_carried() {
    // The failure mode `DecodeFailed` already avoids by discarding serde's
    // message, which quotes the offending value. A `Connect` is refused with
    // the password still in hand, and these logs are what an organizer pastes
    // into a bug report.
    let log = run(false, &[connect(r#""password":"hunter2""#)]).await;

    log.only(ANSWERED);
    let everything = log.0.lock().unwrap().join("\n");
    assert!(
        !everything.contains("hunter2"),
        "a password reached the log:\n{everything}"
    );
}

#[tokio::test]
async fn a_packet_the_room_accepts_is_not_logged() {
    // The counterweight to the rest of this file. A refusal log that also fires
    // for ordinary traffic is noise, and on a busy room noise is indistinguish-
    // able from silence.
    let log = run(true, &[r#"[{"cmd":"Say","text":"hello"}]"#.to_string()]).await;

    assert!(log.with_message(ANSWERED).is_empty(), "{:?}", log.0);
    assert!(log.with_message(FATAL).is_empty(), "{:?}", log.0);
}
