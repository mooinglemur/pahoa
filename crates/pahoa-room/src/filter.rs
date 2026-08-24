//! Per-slot send and receive filters.
//!
//! Two problems, one mechanism. A client that crashes on a particular message
//! needs that message not to reach it; a room drowning in DeathLinks needs
//! fewer of them to go out. Both are "drop some of this traffic for this slot",
//! and the only difference is *how much* — which is why every rule carries a
//! probability rather than DeathLink getting a special case.
//!
//! **`p` is the share dropped, not the share kept**: `p: 0.25` drops one
//! message in four. See [`Rule::probability`], and mind the direction whenever
//! writing about it — this is the one number here that reads plausibly
//! backwards.
//!
//! # What may be filtered, and what may not
//!
//! **Only advisory traffic.** `send_new_items` advances a slot's `send_index`
//! as it sends, so a dropped `ReceivedItems` leaves the server believing a
//! client holds items it never received, and the client cannot tell — the same
//! reasoning that makes the outbound budget close a connection rather than skip
//! a frame (see `pahoa_net::budget`). `Connected`, `ReceivedItems`,
//! `LocationInfo` and `RoomUpdate` are therefore not addressable here at all,
//! and [`Rule::from_json`] refuses them by name rather than silently ignoring
//! a rule that looks like it should work.
//!
//! If a client genuinely cannot survive one of those, no filter can save it
//! without desynchronizing it, and the honest answer is that the client is
//! broken.
//!
//! # Why the stored form is JSON
//!
//! The rule vocabulary is expected to grow — the tag list alone is open-ended,
//! `TrapLink` being the second entry and not the last. Storing rules as JSON,
//! the way the datastore's own values already are, means the save format
//! changed once to gain filters and need not change again to gain a matcher.
//! The cost is that validation lives at the API boundary rather than in the
//! decoder, which is where [`Rule::from_json`] earns its place.
//!
//! # Matching
//!
//! A filter is a **map from matcher to probability**, not an ordered list, and
//! **the most specific match wins**: a rule naming a `tag` or a `subtype` beats
//! one that names only a kind.
//!
//! Ordered rules were the first design and they made `PATCH` unanswerable. With
//! first-match-wins, patching an exemption onto a blanket rule silently does
//! nothing if it lands after it, and prepending is just as arbitrary in the
//! other direction — so every possible placement is a guess about intent, and
//! the wrong guess is a filter that looks configured and is dead. Keying on the
//! matcher removes the question: a rule either replaces one with the same
//! matcher or is a new entry, and the result is the same set either way.
//!
//! That also makes `PATCH` idempotent, which matters for a reconcile loop:
//! re-applying the same rule must not grow the filter every pass.
//!
//! The trade is that rules can no longer be hand-ordered. Nothing today's
//! matchers can express needs it — a qualified rule always wants to beat an
//! unqualified one, which is exactly what specificity gives. A future matcher
//! with a real predicate would have no obvious specificity and would need an
//! explicit tiebreak; that is a field to add, not a format to replace.

use pahoa_proto::ServerPacket;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};

/// Every drop, broken out by the slot it happened to and what it was.
///
/// **The only tally there is.** The two room-wide totals below are sums of this
/// table rather than counters of their own, so "which kind is being dropped"
/// and "how much is being dropped" cannot disagree — a drop path that forgot to
/// attribute itself would be missing from both rather than showing up as a
/// discrepancy nobody notices. Walking it costs a scrape, once a tick.
///
/// Read-locked to increment and write-locked only to introduce a pair that has
/// never been seen, which is what keeps the sparse-by-default property from
/// costing anything: a slot that has never had a `bounce` dropped has no entry,
/// and no series.
static DROPS: LazyLock<RwLock<HashMap<DropKey, AtomicU64>>> = LazyLock::new(RwLock::default);

/// One row of the drop table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DropKey {
    /// `(team, slot)`, not a slot number: a filter belongs to a participant,
    /// and slot numbers repeat across teams.
    pub key: crate::SlotKey,
    pub direction: Direction,
    pub kind: Kind,
}

/// Every observed (slot, direction, kind) with its count.
///
/// Observed only: a pair that has never been dropped is absent rather than
/// zero, because a gap and a zero say different things on a dashboard.
pub fn drops_by_slot() -> Vec<(DropKey, u64)> {
    DROPS
        .read()
        .expect("not poisoned")
        .iter()
        .map(|(key, count)| (*key, count.load(Ordering::Relaxed)))
        .collect()
}

/// Messages dropped because a slot's filter matched what it **sent**.
///
/// Counted **once per message**, because that is where the decision is: a slot
/// sends one `Say` and the room drops it once, before anyone was going to
/// receive it.
pub fn dropped_from_slot() -> u64 {
    total(Direction::FromSlot)
}

/// Messages dropped because a slot's filter matched what it would **receive**.
///
/// Counted **once per recipient connection**, which is a different denominator
/// from [`dropped_from_slot`] above and deliberately so: the test runs inside
/// the shard's per-recipient loop, so one chat line filtered for forty slots is
/// forty — and eighty if each of them also has a tracker attached. That is the
/// number worth watching, because it is what the filter actually spared those
/// clients, and it is the same convention `pahoa_frames_out_total` uses, which
/// is what makes "what share of this slot's traffic is being filtered" a
/// meaningful ratio of the two.
///
/// The asymmetry is not an accident of implementation. It is the same one
/// `pahoa_packets_out_total` and `pahoa_frames_out_total` draw: what the room
/// decided, and what fan-out made of it.
pub fn dropped_to_slot() -> u64 {
    total(Direction::ToSlot)
}

fn total(direction: Direction) -> u64 {
    DROPS
        .read()
        .expect("not poisoned")
        .iter()
        .filter(|(key, _)| key.direction == direction)
        .map(|(_, count)| count.load(Ordering::Relaxed))
        .sum()
}

/// Count one drop against the slot it happened to.
fn record_drop(key: DropKey) {
    if let Some(count) = DROPS.read().expect("not poisoned").get(&key) {
        count.fetch_add(1, Ordering::Relaxed);
        return;
    }
    DROPS
        .write()
        .expect("not poisoned")
        .entry(key)
        .or_default()
        .fetch_add(1, Ordering::Relaxed);
}

/// Which way a message is travelling.
///
/// **Named for the slot at both ends rather than for `in` and `out`**, because
/// those are relative and nobody remembers to what. Every reader of a rule has
/// a different default: the server author thinks "inbound" means arriving at
/// pahoa, the organizer writing the rule thinks about what a *player* is
/// sending, and the two are opposites. A rule read backwards is a filter that
/// silently does nothing, written by hand, usually while a room is on fire.
///
/// `FromSlot` and `ToSlot` cannot be read backwards from either chair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// What this slot **sends**: client to room. Where a chatty DeathLink
    /// source is thinned, and where an out-of-spec packet is dropped before the
    /// room relays it.
    FromSlot,
    /// What this slot **receives**: room to client. Where a message that
    /// crashes one client is kept away from it.
    ToSlot,
}

impl Direction {
    pub fn as_text(self) -> &'static str {
        match self {
            Self::FromSlot => "from_slot",
            Self::ToSlot => "to_slot",
        }
    }

    pub fn from_text(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "from_slot" => Some(Self::FromSlot),
            "to_slot" => Some(Self::ToSlot),
            _ => None,
        }
    }
}

/// What a message is, coarsely enough for a rule to name it.
///
/// Deliberately **not** every `ServerPacket` and `ClientPacket` variant: this is
/// the closed set of things it is safe to drop, and the absences are the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A `Bounce` inbound, or a `Bounced` outbound. The relay in both
    /// directions, and where DeathLink, TrapLink and their successors live.
    Bounce,
    /// Chat and every other `PrintJSON`. Narrowed further by `subtype`.
    PrintJson,
    /// A datastore write, inbound.
    Set,
    /// A datastore push or reply, outbound. The `SetReply` that a `SetNotify`
    /// subscription produces is the one that reaches a client unasked, and the
    /// one whose values can overflow a strongly-typed client's integer.
    SetReply,
    /// `Retrieved`, outbound — the answer to the slot's own `Get`.
    Retrieved,
    /// A `StatusUpdate`, inbound.
    StatusUpdate,
    /// A `Say`, inbound: this slot is **muted**.
    ///
    /// The mirror of a `print_json`/`Chat` rule, and worth keeping straight:
    /// muting a slot stops what it says reaching anyone, while
    /// `{"direction":"to_slot","kind":"print_json","subtype":"Chat"}` stops
    /// *other people's* chat reaching it. One is a gag, the other is earplugs.
    ///
    /// **It also disables that slot's `!` commands**, which is not obvious and
    /// is not a bug. Every `Say` is chat first and a command second — the room
    /// broadcasts the raw line before looking at whether it starts with `!` —
    /// so there is no point downstream where the two are still separable
    /// without reimplementing the command parser inside the filter. A muted
    /// slot cannot `!hint` or `!release`. If those need to survive a mute, this
    /// wants a narrowing field rather than a different drop point.
    Say,
}

impl Kind {
    pub fn as_text(self) -> &'static str {
        match self {
            Self::Bounce => "bounce",
            Self::PrintJson => "print_json",
            Self::Set => "set",
            Self::SetReply => "set_reply",
            Self::Retrieved => "retrieved",
            Self::StatusUpdate => "status_update",
            Self::Say => "say",
        }
    }

    pub fn from_text(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "bounce" => Self::Bounce,
            "print_json" => Self::PrintJson,
            "set" => Self::Set,
            "set_reply" => Self::SetReply,
            "retrieved" => Self::Retrieved,
            "status_update" => Self::StatusUpdate,
            "say" => Self::Say,
            _ => return None,
        })
    }

    /// Every kind, so a refusal can list the alternatives without repeating the
    /// table.
    pub const ALL: [Self; 7] = [
        Self::Bounce,
        Self::PrintJson,
        Self::Set,
        Self::SetReply,
        Self::Retrieved,
        Self::StatusUpdate,
        Self::Say,
    ];

    /// Whether this kind can travel this way at all.
    ///
    /// A rule that names an impossible pairing — an outbound `Set`, say — would
    /// simply never fire, which looks identical to a filter that is not working.
    /// Refusing it at the boundary turns a silent no-op into an error message.
    pub fn travels(self, direction: Direction) -> bool {
        match self {
            Self::Bounce => true,
            // `say` is what a slot sends; the room relays it as a `print_json`,
            // which is the other kind and the other direction.
            Self::Set | Self::StatusUpdate | Self::Say => direction == Direction::FromSlot,
            Self::PrintJson | Self::SetReply | Self::Retrieved => direction == Direction::ToSlot,
        }
    }
}

/// Names that are *recognized and refused*, so that asking to filter them is an
/// error rather than a rule that quietly never matches.
///
/// See the module docs: dropping any of these desynchronizes a client.
const REFUSED: &[(&str, &str)] = &[
    (
        "received_items",
        "dropping an item delivery desynchronizes the slot: the room advances \
         its send index as it sends, so the client would never learn what it \
         missed",
    ),
    (
        "connected",
        "a client that never receives its Connect reply cannot play at all",
    ),
    (
        "location_info",
        "scout results answer a request the client is waiting on",
    ),
    (
        "room_update",
        "room updates carry checked locations and permissions the client acts on",
    ),
];

/// One rule: what it matches, and how often it fires.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub direction: Direction,
    pub kind: Kind,
    /// A `Bounce` tag, matched case-insensitively. `None` matches any tag.
    pub tag: Option<String>,
    /// A `PrintJSON` type, matched case-insensitively against the wire spelling
    /// (`Chat`, `ItemSend`, …). `None` matches any.
    pub subtype: Option<String>,
    /// **The probability a matching message is _dropped_**, `0.0..=1.0`.
    ///
    /// Spelled `p` on the wire, and the direction it runs is the thing to be
    /// unambiguous about: `p: 0.25` **drops one message in four and delivers
    /// three**. It is not the share that survives. The first draft of the
    /// README got this backwards — "thin to a quarter", which reads as keeping
    /// a quarter — and a filter dropping three times what was intended still
    /// looks like it is working, so nothing would have caught it.
    ///
    /// **A plain filter is `1.0`**, which is what an absent field means, so the
    /// common case needs no probability at all. Anything below 1 turns the same
    /// rule into a thinning valve — which is all "scale DeathLinks down" ever
    /// was, and why it is a property of every rule rather than a feature of one.
    /// `0.0` is the other end: a rule that never fires, which is how a more
    /// specific matcher exempts itself from a blanket one.
    pub probability: f64,
}

impl Rule {
    /// Read one rule from its stored form.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "a rule must be a JSON object".to_string())?;

        let direction = Self::text(object, "direction")?;
        let direction = Direction::from_text(&direction)
            .ok_or_else(|| format!("unknown direction {direction:?}, known: from_slot, to_slot"))?;

        let kind = Self::text(object, "kind")?;
        let kind = Kind::from_text(&kind).ok_or_else(|| Self::explain_kind(&kind))?;
        if !kind.travels(direction) {
            return Err(format!(
                "a {} cannot travel {}; it is {}",
                kind.as_text(),
                direction.as_text(),
                if kind.travels(Direction::FromSlot) {
                    "something a slot sends"
                } else {
                    "something a slot receives"
                }
            ));
        }

        let optional = |name: &str| -> Result<Option<String>, String> {
            match object.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                Some(Value::String(_)) => Err(format!("{name:?} must not be empty")),
                Some(_) => Err(format!("{name:?} must be a string")),
            }
        };
        let tag = optional("tag")?;
        let subtype = optional("subtype")?;

        if tag.is_some() && kind != Kind::Bounce {
            return Err("\"tag\" only applies to a bounce".to_string());
        }
        if subtype.is_some() && kind != Kind::PrintJson {
            return Err("\"subtype\" only applies to print_json".to_string());
        }

        let probability = match object.get("p") {
            None | Some(Value::Null) => 1.0,
            Some(v) => {
                let p = v
                    .as_f64()
                    .ok_or_else(|| "\"p\" must be a number".to_string())?;
                if !(0.0..=1.0).contains(&p) {
                    return Err(format!(
                        "\"p\" is the share of matching messages to drop and must be \
                         between 0 and 1, got {p}"
                    ));
                }
                p
            }
        };

        Ok(Self {
            direction,
            kind,
            tag,
            subtype,
            probability,
        })
    }

    /// Back to the stored form, omitting what was defaulted so a round trip
    /// through the API does not grow fields nobody wrote.
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("direction".into(), self.direction.as_text().into());
        object.insert("kind".into(), self.kind.as_text().into());
        if let Some(tag) = &self.tag {
            object.insert("tag".into(), tag.clone().into());
        }
        if let Some(subtype) = &self.subtype {
            object.insert("subtype".into(), subtype.clone().into());
        }
        if self.probability != 1.0 {
            object.insert("p".into(), self.probability.into());
        }
        Value::Object(object)
    }

    fn text(object: &Map<String, Value>, name: &str) -> Result<String, String> {
        object
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{name:?} is required and must be a string"))
    }

    /// A refused name gets its reason; an unknown one gets the menu.
    fn explain_kind(name: &str) -> String {
        let lowered = name.to_ascii_lowercase();
        if let Some((_, why)) = REFUSED.iter().find(|(n, _)| *n == lowered) {
            return format!("{name:?} cannot be filtered: {why}");
        }
        let known: Vec<&str> = Kind::ALL.iter().map(|k| k.as_text()).collect();
        format!("unknown kind {name:?}, known: {}", known.join(", "))
    }

    /// How narrowly this rule matches, for resolving overlaps: a rule naming a
    /// tag or a subtype is more specific than one naming only a kind.
    fn specificity(&self) -> u8 {
        u8::from(self.tag.is_some() || self.subtype.is_some())
    }

    /// The matcher, which is this rule's **identity**.
    ///
    /// Two rules with the same matcher are one rule with different settings, so
    /// a `PATCH` replaces rather than appends and a `DELETE` needs only this
    /// much of a rule to name it.
    pub fn matcher(&self) -> (Direction, Kind, Option<&str>, Option<&str>) {
        (
            self.direction,
            self.kind,
            self.tag.as_deref(),
            self.subtype.as_deref(),
        )
    }
    /// Whether this rule's matcher covers a message. Does **not** consult the
    /// probability — see [`Filter::fires`].
    /// `labels` is what the message offers to narrow on — **all** of a bounce's
    /// tags, not just the first. A `Bounce` routinely carries `["AP",
    /// "DeathLink"]`, so matching only the leading tag would miss the rule an
    /// operator actually wrote, silently and in the direction that looks like
    /// the filter is broken.
    fn matches(&self, direction: Direction, kind: Kind, labels: &[&str]) -> bool {
        if self.direction != direction || self.kind != kind {
            return false;
        }
        // `tag` and `subtype` occupy the same slot: at most one is set, because
        // each belongs to a different kind.
        match (&self.tag, &self.subtype) {
            (Some(want), _) | (_, Some(want)) => {
                labels.iter().any(|got| got.eq_ignore_ascii_case(want))
            }
            _ => true,
        }
    }
}

/// A slot's rules, or the room's defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Filter {
    pub rules: Vec<Rule>,
}

/// How many rules one filter may carry.
///
/// Every rule is tested against every message the filter covers, on the room
/// actor for inbound and on a shard for outbound, so this is a bound on work
/// done per message rather than a storage limit.
pub const MAX_RULES: usize = 64;

impl Filter {
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let list = match value {
            Value::Array(list) => list,
            Value::Object(object) => object
                .get("rules")
                .and_then(Value::as_array)
                .ok_or_else(|| "expected {\"rules\": [...]}".to_string())?,
            _ => return Err("expected a list of rules".to_string()),
        };
        if list.len() > MAX_RULES {
            return Err(format!("at most {MAX_RULES} rules, got {}", list.len()));
        }
        let rules = list
            .iter()
            .enumerate()
            .map(|(i, v)| Rule::from_json(v).map_err(|e| format!("rule {i}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { rules })
    }

    pub fn to_json(&self) -> Value {
        Value::Array(self.rules.iter().map(Rule::to_json).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether a message should be dropped.
    ///
    /// **The most specific matching rule decides**, so a blanket rule and an
    /// exemption for one tag can coexist in either written order. `roll`
    /// supplies the randomness so the caller owns the generator — deliberately, because the room's own PRNG is
    /// the *hint* PRNG: it is saved, and it is pinned byte for byte against a
    /// real `MultiServer.Context` by `hint_vectors.jsonl`. Drawing sampling
    /// numbers from it would move hint selection and break that comparison.
    pub fn drops(
        &self,
        key: crate::SlotKey,
        direction: Direction,
        kind: Kind,
        labels: &[&str],
        roll: &mut impl FnMut() -> f64,
    ) -> bool {
        let winner = self
            .rules
            .iter()
            .filter(|rule| rule.matches(direction, kind, labels))
            .max_by_key(|rule| rule.specificity());
        let dropped = match winner {
            // `1.0` short-circuits, so an ordinary filter never touches the
            // generator and a room with no sampling rules is deterministic.
            Some(rule) => rule.probability >= 1.0 || roll() < rule.probability,
            None => false,
        };
        if dropped {
            // Counted here rather than at the two call sites, because this is
            // the one place both directions agree on what "dropped" means — and
            // a filter that is quietly discarding more than an operator
            // expected is the failure worth being able to see. It is also why
            // `key` is a parameter: attributing at the decision keeps the
            // breakdown and the totals the same numbers.
            record_drop(DropKey {
                key,
                direction,
                kind,
            });
        }
        dropped
    }

    /// Merge rules in, replacing any with the same matcher.
    ///
    /// What `PATCH` does. Idempotent by construction: applying the same rule
    /// twice leaves one entry, so a reconcile loop re-asserting its intent does
    /// not grow the filter.
    pub fn merge(&mut self, incoming: Vec<Rule>) -> Result<(), String> {
        for rule in incoming {
            match self
                .rules
                .iter_mut()
                .find(|existing| existing.matcher() == rule.matcher())
            {
                Some(existing) => *existing = rule,
                None => self.rules.push(rule),
            }
        }
        if self.rules.len() > MAX_RULES {
            return Err(format!(
                "at most {MAX_RULES} rules, and this would make {}",
                self.rules.len()
            ));
        }
        Ok(())
    }

    /// Drop rules whose matcher appears in `matchers`. Returns how many went,
    /// so a `DELETE` can report whether it did anything.
    pub fn remove(&mut self, matchers: &[Rule]) -> usize {
        let before = self.rules.len();
        self.rules
            .retain(|rule| !matchers.iter().any(|m| m.matcher() == rule.matcher()));
        before - self.rules.len()
    }
}

/// What a filter rule can name an outbound frame, if anything.
///
/// `None` means the frame is not addressable and will be delivered — which is
/// the safe direction, and covers everything carrying progression as well as
/// anything a rule has no vocabulary for.
///
/// **A frame carries several packets and is compressed once for everybody**, so
/// the decision has to be whole-frame: there is no way to drop one packet out
/// of a shared buffer without re-encoding it per recipient, which is the cost
/// the shards exist to avoid. A batch therefore only becomes filterable when
/// every packet in it says the same thing; a mixed frame is delivered intact.
/// In practice the batches that matter are homogeneous — a run of hint
/// notifications, a run of chat — so this is not the limitation it sounds like.
pub fn outbound_tag(msgs: &[ServerPacket]) -> Option<(Kind, Vec<String>)> {
    let mut tag: Option<(Kind, Vec<String>)> = None;
    for msg in msgs {
        let this = one_outbound_tag(msg)?;
        match &tag {
            None => tag = Some(this),
            Some(first) if *first == this => {}
            Some(_) => return None,
        }
    }
    tag
}

fn one_outbound_tag(msg: &ServerPacket) -> Option<(Kind, Vec<String>)> {
    match msg {
        ServerPacket::PrintJSON(p) => Some((
            Kind::PrintJson,
            p.print_type
                .map(|t| vec![t.as_text().to_string()])
                .unwrap_or_default(),
        )),
        // `Retrieved`, `SetReply` and `Bounced` share one variant because the
        // reference builds them by overwriting `cmd` in the client's own
        // request, so the name lives in the map rather than in the type.
        ServerPacket::Echo(map) => match map.get("cmd").and_then(Value::as_str)? {
            "Bounced" => Some((
                Kind::Bounce,
                map.get("tags")
                    .and_then(Value::as_array)
                    .map(|tags| {
                        tags.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            )),
            "SetReply" => Some((Kind::SetReply, Vec::new())),
            "Retrieved" => Some((Kind::Retrieved, Vec::new())),
            _ => None,
        },
        // Everything else is progression, a handshake, or a reply a client is
        // blocking on. See the module docs.
        _ => None,
    }
}

/// A small generator for sampling decisions.
///
/// **Not** `pahoa_pyrandom::PyRandom`. That exists to reproduce CPython's
/// Mersenne Twister for hint selection, costs 624 words of state, and is saved;
/// none of which a filter wants. Nothing differential depends on these numbers,
/// so the requirement is only that they be cheap and well spread.
///
/// Not saved, either: a filter's job is statistical rather than reproducible,
/// and a restart reseeding changes nothing an operator can observe.
#[derive(Debug)]
pub struct Sampler(u64);

impl Sampler {
    pub fn new(seed: u64) -> Self {
        // Any nonzero state; xorshift is stuck at zero.
        Self(seed | 1)
    }

    /// A float in `[0, 1)`.
    ///
    /// Named `roll` rather than `next` so it is not mistaken for an iterator;
    /// a `Sampler` has no end.
    pub fn roll(&mut self) -> f64 {
        // xorshift64*, which is ample for deciding whether to drop a DeathLink.
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        // The top 53 bits, so every representable float in the range is reachable.
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D)) >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Whose drops these are. The counters are process-wide, so a test that
    /// reads them back picks a slot number nothing else uses rather than
    /// sharing this one.
    const SLOT: crate::SlotKey = (0, 1);

    fn rule(v: serde_json::Value) -> Result<Rule, String> {
        Rule::from_json(&v)
    }

    fn always() -> impl FnMut() -> f64 {
        || 0.0
    }

    #[test]
    fn a_bare_rule_is_a_plain_filter() {
        let r = rule(json!({"direction": "from_slot", "kind": "bounce"})).unwrap();
        assert_eq!(r.probability, 1.0, "an absent p means always");
        assert!(r.tag.is_none(), "and matches every tag");
    }

    /// **Every drop lands in the table, whatever it was.**
    ///
    /// The room-wide totals are sums of this table, so a drop that failed to
    /// attribute itself would not show up as a discrepancy between the two —
    /// it would be missing from both, which is the silent version. What can be
    /// checked is that no reachable (direction, kind) pairing escapes
    /// attribution, and that each lands under its own labels rather than being
    /// piled onto one.
    #[test]
    fn every_kind_and_direction_is_attributed() {
        // A slot number no other test in this crate touches, because the table
        // is process-wide.
        const MINE: crate::SlotKey = (0, 41);

        let mut expected = Vec::new();
        for direction in [Direction::FromSlot, Direction::ToSlot] {
            for kind in Kind::ALL {
                if !kind.travels(direction) {
                    continue;
                }
                let filter = Filter::from_json(&json!([{
                    "direction": direction.as_text(),
                    "kind": kind.as_text(),
                }]))
                .unwrap();
                assert!(
                    filter.drops(MINE, direction, kind, &[], &mut always()),
                    "{} {} should have been dropped",
                    direction.as_text(),
                    kind.as_text()
                );
                expected.push(DropKey {
                    key: MINE,
                    direction,
                    kind,
                });
            }
        }
        // A bounce both ways, three kinds a slot only sends, three it only
        // receives. A new kind has to move this number, which is the point.
        assert_eq!(expected.len(), 8, "the reachable pairings");

        let mine: Vec<_> = drops_by_slot()
            .into_iter()
            .filter(|(row, _)| row.key == MINE)
            .collect();
        for key in expected {
            let count = mine.iter().find(|(k, _)| *k == key).map(|(_, n)| *n);
            assert_eq!(
                count,
                Some(1),
                "{} {} was not attributed on its own",
                key.direction.as_text(),
                key.kind.as_text()
            );
        }
    }

    /// **`p` is the share dropped, not the share kept.**
    ///
    /// Pinned as behavior because it was documented backwards — "thin to a
    /// quarter", which reads as keeping a quarter — and nothing at runtime
    /// would have caught it: a filter dropping three times what an operator
    /// intended still looks like it is working. Prose can drift back; this
    /// cannot.
    #[test]
    fn p_is_the_share_dropped_not_the_share_kept() {
        let filter =
            Filter::from_json(&json!([{"direction": "from_slot", "kind": "bounce", "p": 0.25}]))
                .unwrap();

        // A quarter of the draws fall below 0.25, and those are the ones that
        // go. Sampled rather than reasoned about, so an inverted comparison
        // fails here rather than in a live room.
        let mut sampler = Sampler::new(0xFACE_FEED);
        let mut roll = || sampler.roll();
        let dropped = (0..10_000)
            .filter(|_| filter.drops(SLOT, Direction::FromSlot, Kind::Bounce, &[], &mut roll))
            .count();

        assert!(
            (2200..2800).contains(&dropped),
            "p = 0.25 must drop about a quarter and deliver about three quarters; \
             dropped {dropped} of 10000"
        );
    }

    /// The two ends, which are the ones written by hand most often.
    #[test]
    fn p_of_one_always_drops_and_p_of_zero_never_does() {
        let at = |p| {
            Filter::from_json(&json!([{"direction": "from_slot", "kind": "bounce", "p": p}]))
                .unwrap()
        };
        let mut sampler = Sampler::new(1);
        let mut roll = || sampler.roll();

        assert!(at(1.0).drops(SLOT, Direction::FromSlot, Kind::Bounce, &[], &mut roll));
        for _ in 0..100 {
            assert!(
                !at(0.0).drops(SLOT, Direction::FromSlot, Kind::Bounce, &[], &mut roll),
                "p = 0 must never fire, which is how an exemption is written"
            );
        }
    }

    #[test]
    fn a_tagged_rule_matches_only_that_tag_and_only_that_direction() {
        let filter = Filter::from_json(&json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink"}
        ]))
        .unwrap();

        assert!(filter.drops(
            SLOT,
            Direction::FromSlot,
            Kind::Bounce,
            &["DeathLink"],
            &mut always()
        ));
        // Case-insensitively, because tags are conventions rather than a schema.
        assert!(filter.drops(
            SLOT,
            Direction::FromSlot,
            Kind::Bounce,
            &["deathlink"],
            &mut always()
        ));
        assert!(!filter.drops(
            SLOT,
            Direction::FromSlot,
            Kind::Bounce,
            &["TrapLink"],
            &mut always()
        ));
        // And the same tag going the other way is a different rule: thinning
        // what a slot sends says nothing about what it receives.
        assert!(!filter.drops(
            SLOT,
            Direction::ToSlot,
            Kind::Bounce,
            &["DeathLink"],
            &mut always()
        ));
    }

    /// The generalization the feature is built around: thinning is an ordinary
    /// rule with a probability, so a tag nobody has invented yet needs no code.
    #[test]
    fn probability_applies_to_any_rule_not_just_deathlink() {
        for tag in ["DeathLink", "TrapLink", "SomethingNobodyHasWrittenYet"] {
            let filter = Filter::from_json(&json!([
                {"direction": "from_slot", "kind": "bounce", "tag": tag, "p": 0.5}
            ]))
            .unwrap();
            let mut low = || 0.25;
            let mut high = || 0.75;
            assert!(filter.drops(SLOT, Direction::FromSlot, Kind::Bounce, &[tag], &mut low));
            assert!(!filter.drops(SLOT, Direction::FromSlot, Kind::Bounce, &[tag], &mut high));
        }
    }

    /// A plain filter must not consume randomness, so a room with no sampling
    /// rules behaves identically every run.
    #[test]
    fn a_certain_rule_never_touches_the_generator() {
        let filter =
            Filter::from_json(&json!([{"direction": "from_slot", "kind": "bounce"}])).unwrap();
        let mut rolled = false;
        let mut roll = || {
            rolled = true;
            0.0
        };
        assert!(filter.drops(SLOT, Direction::FromSlot, Kind::Bounce, &[], &mut roll));
        assert!(!rolled, "p = 1 should short-circuit");
    }

    /// **Every tag, not the leading one.** A `Bounce` routinely carries
    /// `["AP", "DeathLink"]`, so a rule naming `DeathLink` has to see past the
    /// first entry — matching only the head would miss the rule an operator
    /// actually wrote, silently, and in the direction that reads as "the filter
    /// does not work".
    #[test]
    fn a_tag_rule_matches_anywhere_in_the_tag_list() {
        let filter = Filter::from_json(&json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink"}
        ]))
        .unwrap();

        assert!(filter.drops(
            SLOT,
            Direction::FromSlot,
            Kind::Bounce,
            &["AP", "DeathLink"],
            &mut always()
        ));
        assert!(!filter.drops(
            SLOT,
            Direction::FromSlot,
            Kind::Bounce,
            &["AP", "TrapLink"],
            &mut always()
        ));
        // A bounce with no tags matches only an untagged rule.
        assert!(!filter.drops(SLOT, Direction::FromSlot, Kind::Bounce, &[], &mut always()));
    }

    /// **The most specific rule wins, whichever order it is written in.**
    ///
    /// This is what makes `PATCH` answerable at all: an exemption merged onto a
    /// blanket rule works without the caller reasoning about position, and the
    /// same two rules behave identically read either way round. Under the
    /// ordered model this test's second case silently did nothing.
    #[test]
    fn the_most_specific_rule_wins_in_either_order() {
        let blanket = json!({"direction": "from_slot", "kind": "bounce", "p": 1.0});
        let exemption =
            json!({"direction": "from_slot", "kind": "bounce", "tag": "TrapLink", "p": 0.0});

        for order in [json!([blanket, exemption]), json!([exemption, blanket])] {
            let filter = Filter::from_json(&order).unwrap();
            assert!(
                !filter.drops(
                    SLOT,
                    Direction::FromSlot,
                    Kind::Bounce,
                    &["TrapLink"],
                    &mut always()
                ),
                "the tagged rule must win regardless of position: {order}"
            );
            assert!(
                filter.drops(
                    SLOT,
                    Direction::FromSlot,
                    Kind::Bounce,
                    &["DeathLink"],
                    &mut always()
                ),
                "and the blanket rule still covers everything else: {order}"
            );
        }
    }

    /// `PATCH` semantics: the same matcher replaces, a new one appends, and
    /// applying the same rule twice leaves one entry — which is what lets a
    /// reconcile loop re-assert its intent without growing the filter.
    #[test]
    fn merging_upserts_on_the_matcher_and_is_idempotent() {
        let rules = |v| Filter::from_json(&v).unwrap().rules;
        let mut filter = Filter::from_json(&json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink", "p": 0.5}
        ]))
        .unwrap();

        let updated = rules(json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink", "p": 0.1}
        ]));
        filter.merge(updated.clone()).unwrap();
        assert_eq!(filter.rules.len(), 1, "same matcher must not duplicate");
        assert_eq!(filter.rules[0].probability, 0.1);

        filter.merge(updated).unwrap();
        assert_eq!(filter.rules.len(), 1, "and re-applying changes nothing");

        filter
            .merge(rules(json!([
                {"direction": "from_slot", "kind": "bounce", "tag": "TrapLink"}
            ])))
            .unwrap();
        assert_eq!(filter.rules.len(), 2, "a different matcher is a new entry");
    }

    /// A `DELETE` names a rule by its matcher, so a caller need not know the
    /// probability it is removing.
    #[test]
    fn removing_takes_the_matcher_not_the_whole_rule() {
        let rules = |v| Filter::from_json(&v).unwrap().rules;
        let mut filter = Filter::from_json(&json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink", "p": 0.5},
            {"direction": "from_slot", "kind": "bounce", "tag": "TrapLink"}
        ]))
        .unwrap();

        let target = rules(json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink", "p": 0.9}
        ]));
        assert_eq!(
            filter.remove(&target),
            1,
            "the probability is not the identity"
        );
        assert_eq!(filter.rules.len(), 1);
        assert_eq!(filter.remove(&target), 0, "and it says so the second time");
    }

    /// A merge cannot smuggle a filter past the bound the constructor enforces.
    #[test]
    fn merging_still_respects_the_bound() {
        let mut filter = Filter::default();
        let many: Vec<Rule> = (0..MAX_RULES + 1)
            .map(|i| {
                Rule::from_json(&json!({
                    "direction": "from_slot",
                    "kind": "bounce",
                    "tag": format!("tag{i}")
                }))
                .unwrap()
            })
            .collect();
        assert!(filter.merge(many).is_err());
    }

    /// **The absences are the feature.** Naming one of these is an error with a
    /// reason, not a rule that silently never fires.
    #[test]
    fn progression_traffic_is_refused_by_name() {
        for (name, _) in REFUSED {
            let e = rule(json!({"direction": "to_slot", "kind": name})).unwrap_err();
            assert!(
                e.contains("cannot be filtered"),
                "{name} should be refused with a reason, got {e}"
            );
        }
        // And an item delivery says why specifically, since it is the one an
        // operator is most likely to reach for.
        let e = rule(json!({"direction": "to_slot", "kind": "received_items"})).unwrap_err();
        assert!(e.contains("desynchronizes"), "{e}");
    }

    /// A rule that could never fire is a filter that looks broken. Refuse it.
    #[test]
    fn a_kind_that_cannot_travel_that_way_is_refused() {
        let e = rule(json!({"direction": "to_slot", "kind": "set"})).unwrap_err();
        assert!(e.contains("cannot travel to_slot"), "{e}");
        let e = rule(json!({"direction": "from_slot", "kind": "print_json"})).unwrap_err();
        assert!(e.contains("cannot travel from_slot"), "{e}");
    }

    #[test]
    fn a_qualifier_must_belong_to_its_kind() {
        let e = rule(json!({"direction": "to_slot", "kind": "print_json", "tag": "DeathLink"}))
            .unwrap_err();
        assert!(e.contains("only applies to a bounce"), "{e}");
        let e = rule(json!({"direction": "from_slot", "kind": "bounce", "subtype": "Chat"}))
            .unwrap_err();
        assert!(e.contains("only applies to print_json"), "{e}");
    }

    #[test]
    fn a_probability_outside_the_range_is_refused() {
        for p in [-0.1, 1.1, 2.0, -1.0] {
            assert!(
                rule(json!({"direction": "from_slot", "kind": "bounce", "p": p})).is_err(),
                "p = {p} should be refused"
            );
        }
        // JSON has no NaN — `json!(f64::NAN)` is `null` — so the range check
        // cannot see one from this direction. It is written to reject NaN
        // anyway, because `contains` says false for it and a rule that fires on
        // a comparison against NaN would be neither on nor off.
        assert!(rule(json!({"direction": "from_slot", "kind": "bounce", "p": null})).is_ok());
    }

    /// `null` and absent are the same thing, and both mean "always".
    #[test]
    fn a_null_probability_is_the_same_as_omitting_it() {
        let explicit =
            rule(json!({"direction": "from_slot", "kind": "bounce", "p": null})).unwrap();
        let omitted = rule(json!({"direction": "from_slot", "kind": "bounce"})).unwrap();
        assert_eq!(explicit, omitted);
        assert_eq!(explicit.probability, 1.0);
    }

    #[test]
    fn rules_round_trip_without_growing_fields() {
        let source = json!([
            {"direction": "from_slot", "kind": "bounce", "tag": "DeathLink", "p": 0.25},
            {"direction": "to_slot", "kind": "print_json", "subtype": "Chat"}
        ]);
        let filter = Filter::from_json(&source).unwrap();
        assert_eq!(filter.to_json(), source, "a defaulted p must not appear");
        assert_eq!(Filter::from_json(&filter.to_json()).unwrap(), filter);
    }

    #[test]
    fn a_filter_is_bounded() {
        let many: Vec<Value> = (0..MAX_RULES + 1)
            .map(|_| json!({"direction": "from_slot", "kind": "bounce"}))
            .collect();
        assert!(Filter::from_json(&Value::Array(many)).is_err());
    }

    #[test]
    fn the_sampler_stays_in_range_and_moves() {
        let mut sampler = Sampler::new(12345);
        let draws: Vec<f64> = (0..1000).map(|_| sampler.roll()).collect();
        assert!(draws.iter().all(|d| (0.0..1.0).contains(d)));
        let mean = draws.iter().sum::<f64>() / draws.len() as f64;
        assert!((0.4..0.6).contains(&mean), "mean was {mean}");
        // A zero seed must not lock the generator at zero.
        let mut zero = Sampler::new(0);
        assert!(zero.roll() > 0.0);
    }
}
