//! The native save format.
//!
//! # Why not `.apsave`
//!
//! Archipelago persists a pickled dict (`MultiServer.py:656-683`). Reading that
//! back would need a pickle *writer* and bit-exact serialization of CPython's
//! Mersenne Twister state — the riskiest half of `pahoa-pickle` — bought for an
//! interop nobody has asked for. The plan cut it deliberately. What is kept is
//! the *contents*: this stores everything `get_save` does, plus one thing it
//! does not (see [`Snapshot::allow_releases`]), and nothing it treats as
//! transient.
//!
//! # Shape
//!
//! A snapshot is taken on the actor and encoded off it, so the two halves have
//! very different budgets:
//!
//! - [`Room::snapshot`](crate::Room::snapshot) must be O(slots). Every large
//!   collection in the room lives behind an `Arc` and is mutated through
//!   `Arc::make_mut`, so taking one is a refcount bump per slot and nothing is
//!   copied until the next write to a slot that a save is still holding.
//! - [`Snapshot::encode`] is pure CPU on a background thread and may take its
//!   time. It sorts, so the same state always produces the same bytes — which
//!   is what makes "restore round-trips" a testable claim rather than a hope.
//!
//! # Encoding
//!
//! Little-endian header, LEB128 varints throughout the body, zigzag for signed
//! values. Two size levers do most of the work:
//!
//! - a slot's checked locations are sorted and **delta-encoded**, so ids that
//!   are eight bytes apiece in memory cost one or two on disk;
//! - hint entrance strings go in a **table** and are referenced by index. Every
//!   hint is held twice — once by the finder, once by the receiver — so even a
//!   seed with unique entrances pays for each string once instead of twice.
//!
//! Neither is speculative: both are measured in `tests/save_scale.rs`, which is
//! also what decides whether journaled saves are worth building at all.

use crate::options::RoomOptions;
use crate::room::{QueueKey, SlotKey};
use pahoa_multidata::{ClientStatus, Hint, HintStatus};
// The item queues hold the protocol's `NetworkItem`, not the multidata's
// identically shaped one; they are distinct types and the room uses this one.
use pahoa_proto::{NetworkItem, Permission};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Bytes every save starts with, so a wrong file is refused rather than parsed.
const MAGIC: &[u8; 8] = b"PAHOASAV";

/// Bumped when the body layout changes incompatibly. A save from a *newer*
/// version is refused; the reference does the same (`MultiServer.py:688-689`).
///
/// **2 added `locked_slots`**, appended after the timers. Reading is gated on
/// the version rather than on whether bytes remain, and that is deliberate: the
/// body's length and CRC are both checked before any field is parsed, so
/// "ran out of bytes" would be a sound test here — but it would also make every
/// future trailing field silently optional, and a save that lost its tail to a
/// bug rather than to truncation would then load with defaults instead of
/// failing. Gating on the version keeps "this field is absent" a fact about the
/// format rather than an inference from the data.
///
/// The cost is that a rolled-back server refuses a newer save outright. That is
/// the intended trade for a field carrying access control: a lock that quietly
/// stopped holding after a downgrade is worse than a room that will not start.
pub const FORMAT_VERSION: u8 = 2;

/// The first version carrying `locked_slots`.
const VERSION_LOCKED_SLOTS: u8 = 2;

const ENCODING_RAW: u8 = 0;
const ENCODING_ZLIB: u8 = 1;

/// Header is magic, version, encoding, body length, CRC of the body.
const HEADER_LEN: usize = 8 + 1 + 1 + 4 + 4;

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("not a pahoa save file")]
    BadMagic,
    #[error("save format version {found} is newer than this server understands ({FORMAT_VERSION})")]
    TooNew { found: u8 },
    #[error("unknown body encoding {0}")]
    UnknownEncoding(u8),
    #[error("save is truncated: wanted {wanted} more bytes at offset {at}, {left} remain")]
    Truncated {
        at: usize,
        wanted: usize,
        left: usize,
    },
    #[error("save is corrupt: body checksum mismatch")]
    Checksum,
    #[error("save is corrupt: {0}")]
    Malformed(&'static str),
    #[error("save decompression failed: {0}")]
    Inflate(String),
    #[error("save is for seed {found:?}, this room is seed {expected:?}")]
    WrongSeed { expected: String, found: String },
}

type Result<T> = std::result::Result<T, SaveError>;

/// Everything a room needs to come back exactly as it went down.
///
/// Cheap to take and safe to hand to another thread: the bulky fields are the
/// room's own `Arc`s, shared rather than copied.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Checked against the loaded multidata on restore. The reference compares
    /// `connect_names` for this (`MultiServer.py:686-687`); the seed name is
    /// the same guarantee in one field.
    pub seed_name: String,
    pub options: RoomOptions,
    /// CPython MT19937 state: 624 words plus the index.
    pub rng_state: Vec<u32>,

    pub location_checks: Vec<(SlotKey, Arc<HashSet<i64>>)>,
    pub received_items: Vec<(QueueKey, Arc<Vec<NetworkItem>>)>,
    pub hints: Vec<(SlotKey, Arc<Vec<Hint>>)>,
    pub hints_used: Vec<(SlotKey, i64)>,
    pub name_aliases: Vec<(SlotKey, String)>,
    pub client_game_state: Vec<(SlotKey, ClientStatus)>,
    pub group_collected: Vec<(u32, Vec<u32>)>,
    /// One-off release permissions granted by an administrator.
    ///
    /// The reference holds these in `allow_forfeits` and does **not** save
    /// them, so a restart silently revokes a permission an admin granted. That
    /// looks like an oversight rather than a decision, and persisting it costs
    /// a few bytes.
    pub allow_releases: Vec<SlotKey>,
    pub stored_data: Vec<(String, Arc<Value>)>,

    /// When each slot last checked a new location, and when it last connected,
    /// as **whole unix seconds**.
    ///
    /// Saved because an async routinely outlives the process serving it: a room
    /// that restarts and reported "never connected" for everyone would lose the
    /// one thing a tracker uses to tell an abandoned slot from an active one.
    /// The reference persists these for the same reason
    /// (`MultiServer.py:667-670`).
    ///
    /// Second resolution rather than the float the room holds: RFC 1123, which
    /// is what the tracker renders, has no room for anything finer.
    pub activity_at: Vec<(SlotKey, u64)>,
    pub connected_at: Vec<(SlotKey, u64)>,

    /// Slots an administrator has barred from connecting.
    ///
    /// **Saved because a lock that a restart lifted would be worse than no
    /// lock at all.** The reason to bar a slot — a griefer, a mistaken entry, a
    /// player asked to stop until an organizer sorts something out — outlives
    /// any one process, and a room that quietly re-admits them on its next
    /// deploy fails in exactly the moment it was set up for. Added in format
    /// version 2; see [`FORMAT_VERSION`].
    pub locked_slots: Vec<SlotKey>,
}

impl Snapshot {
    /// Serialize, optionally deflating the body.
    ///
    /// Off-actor work: sorts every collection so the output is a function of
    /// the state alone.
    pub fn encode(&self, compress: bool) -> Vec<u8> {
        let body = self.encode_body();

        let mut crc = flate2::Crc::new();
        crc.update(&body);
        let checksum = crc.sum();
        let raw_len = body.len() as u32;

        let (encoding, body) = if compress {
            (ENCODING_ZLIB, deflate(&body))
        } else {
            (ENCODING_RAW, body)
        };

        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        out.push(encoding);
        out.extend_from_slice(&raw_len.to_le_bytes());
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err(SaveError::BadMagic);
        }
        let version = bytes[8];
        if version > FORMAT_VERSION {
            return Err(SaveError::TooNew { found: version });
        }
        let encoding = bytes[9];
        let raw_len = u32::from_le_bytes(bytes[10..14].try_into().unwrap()) as usize;
        let checksum = u32::from_le_bytes(bytes[14..18].try_into().unwrap());

        let body = match encoding {
            ENCODING_RAW => bytes[HEADER_LEN..].to_vec(),
            ENCODING_ZLIB => inflate(&bytes[HEADER_LEN..], raw_len)?,
            other => return Err(SaveError::UnknownEncoding(other)),
        };
        if body.len() != raw_len {
            return Err(SaveError::Truncated {
                at: HEADER_LEN,
                wanted: raw_len,
                left: body.len(),
            });
        }
        let mut crc = flate2::Crc::new();
        crc.update(&body);
        if crc.sum() != checksum {
            return Err(SaveError::Checksum);
        }

        Self::decode_body(&mut Reader::new(&body), version)
    }

    fn encode_body(&self) -> Vec<u8> {
        let mut w = Writer::default();

        w.str(&self.seed_name);
        encode_options(&mut w, &self.options);

        w.uvar(self.rng_state.len() as u64);
        for word in &self.rng_state {
            w.u32(*word);
        }

        // --- checked locations ----------------------------------------------
        // Sorted and delta-encoded. A slot's locations cluster in one id range,
        // so most deltas fit in a single byte.
        let mut checks: Vec<_> = self.location_checks.iter().collect();
        checks.sort_unstable_by_key(|(key, _)| *key);
        w.uvar(checks.len() as u64);
        for (key, locations) in checks {
            w.key(*key);
            let mut sorted: Vec<i64> = locations.iter().copied().collect();
            sorted.sort_unstable();
            w.uvar(sorted.len() as u64);
            let mut previous = 0i64;
            for location in sorted {
                w.ivar(location - previous);
                previous = location;
            }
        }

        // --- item queues -----------------------------------------------------
        let mut queues: Vec<_> = self.received_items.iter().collect();
        queues.sort_unstable_by_key(|(key, _)| *key);
        w.uvar(queues.len() as u64);
        for ((team, slot, remote), items) in queues {
            w.uvar(*team as u64);
            w.uvar(*slot as u64);
            w.byte(u8::from(*remote));
            w.uvar(items.len() as u64);
            for item in items.iter() {
                w.ivar(item.item);
                w.ivar(item.location);
                w.uvar(item.player as u64);
                w.uvar(item.flags as u64);
            }
        }

        // --- hints ------------------------------------------------------------
        let mut hints: Vec<_> = self.hints.iter().collect();
        hints.sort_unstable_by_key(|(key, _)| *key);

        // Entrances first, so the reader can resolve indexes as it goes.
        let mut entrances: Vec<&str> = Vec::new();
        let mut entrance_index: HashMap<&str, u32> = HashMap::new();
        for (_, list) in &hints {
            for hint in list.iter() {
                entrance_index.entry(&hint.entrance).or_insert_with(|| {
                    entrances.push(&hint.entrance);
                    (entrances.len() - 1) as u32
                });
            }
        }
        w.uvar(entrances.len() as u64);
        for entrance in &entrances {
            w.str(entrance);
        }

        w.uvar(hints.len() as u64);
        for (key, list) in hints {
            w.key(*key);
            w.uvar(list.len() as u64);
            for hint in list.iter() {
                w.uvar(hint.receiving_player as u64);
                w.uvar(hint.finding_player as u64);
                w.ivar(hint.location);
                w.ivar(hint.item);
                w.byte(u8::from(hint.found));
                w.uvar(entrance_index[hint.entrance.as_str()] as u64);
                w.uvar(hint.item_flags as u64);
                w.tag(hint.status as i64);
            }
        }

        // --- the small maps ---------------------------------------------------
        let mut used: Vec<_> = self.hints_used.iter().collect();
        used.sort_unstable_by_key(|(key, _)| *key);
        w.uvar(used.len() as u64);
        for (key, count) in used {
            w.key(*key);
            w.ivar(*count);
        }

        let mut aliases: Vec<_> = self.name_aliases.iter().collect();
        aliases.sort_unstable_by_key(|(key, _)| *key);
        w.uvar(aliases.len() as u64);
        for (key, alias) in aliases {
            w.key(*key);
            w.str(alias);
        }

        let mut states: Vec<_> = self.client_game_state.iter().collect();
        states.sort_unstable_by_key(|(key, _)| *key);
        w.uvar(states.len() as u64);
        for (key, status) in states {
            w.key(*key);
            w.tag(*status as i64);
        }

        let mut collected: Vec<_> = self.group_collected.iter().collect();
        collected.sort_unstable_by_key(|(group, _)| *group);
        w.uvar(collected.len() as u64);
        for (group, members) in collected {
            w.uvar(*group as u64);
            let mut members = members.clone();
            members.sort_unstable();
            w.uvar(members.len() as u64);
            for member in members {
                w.uvar(member as u64);
            }
        }

        let mut releases = self.allow_releases.clone();
        releases.sort_unstable();
        w.uvar(releases.len() as u64);
        for key in releases {
            w.key(key);
        }

        // Values keep their own key order — `preserve_order` is load-bearing,
        // because a client sees that order echoed back in `Retrieved`.
        let mut stored: Vec<_> = self.stored_data.iter().collect();
        stored.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        w.uvar(stored.len() as u64);
        for (key, value) in stored {
            w.str(key);
            let json = serde_json::to_vec(&**value).expect("a stored value is already JSON");
            w.bytes(&json);
        }

        // Last, so every field that was already here keeps its position.
        for timers in [&self.activity_at, &self.connected_at] {
            let mut timers: Vec<_> = timers.iter().collect();
            timers.sort_unstable_by_key(|(key, _)| *key);
            w.uvar(timers.len() as u64);
            for (key, at) in timers {
                w.key(*key);
                w.uvar(*at);
            }
        }

        // Version 2. Sorted for a stable encoding, as everywhere else here — a
        // save whose bytes change when nothing changed defeats the dirty check.
        let mut locked = self.locked_slots.clone();
        locked.sort_unstable();
        w.uvar(locked.len() as u64);
        for key in locked {
            w.key(key);
        }

        w.into_inner()
    }

    fn decode_body(r: &mut Reader<'_>, version: u8) -> Result<Self> {
        let seed_name = r.str()?;
        let options = decode_options(r)?;

        let words = r.count()?;
        let mut rng_state = Vec::with_capacity(words.min(1024));
        for _ in 0..words {
            rng_state.push(r.u32()?);
        }

        let mut location_checks = Vec::new();
        for _ in 0..r.count()? {
            let key = r.key()?;
            let n = r.count()?;
            let mut set = HashSet::with_capacity(n);
            let mut previous = 0i64;
            for _ in 0..n {
                previous = previous
                    .checked_add(r.ivar()?)
                    .ok_or(SaveError::Malformed("location id overflowed"))?;
                set.insert(previous);
            }
            location_checks.push((key, Arc::new(set)));
        }

        let mut received_items = Vec::new();
        for _ in 0..r.count()? {
            let team = r.u32var()?;
            let slot = r.u32var()?;
            let remote = r.byte()? != 0;
            let n = r.count()?;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(NetworkItem {
                    item: r.ivar()?,
                    location: r.ivar()?,
                    player: r.u32var()?,
                    flags: r.u32var()?,
                });
            }
            received_items.push(((team, slot, remote), Arc::new(items)));
        }

        let mut entrances = Vec::new();
        for _ in 0..r.count()? {
            entrances.push(r.str()?);
        }

        let mut hints = Vec::new();
        for _ in 0..r.count()? {
            let key = r.key()?;
            let n = r.count()?;
            let mut list = Vec::with_capacity(n);
            for _ in 0..n {
                let receiving_player = r.u32var()?;
                let finding_player = r.u32var()?;
                let location = r.ivar()?;
                let item = r.ivar()?;
                let found = r.byte()? != 0;
                let entrance = entrances
                    .get(r.u32var()? as usize)
                    .cloned()
                    .ok_or(SaveError::Malformed("hint names an unknown entrance"))?;
                let item_flags = r.u32var()?;
                let status = HintStatus::from_wire(r.tag()?)
                    .ok_or(SaveError::Malformed("unknown hint status"))?;
                list.push(Hint {
                    receiving_player,
                    finding_player,
                    location,
                    item,
                    found,
                    entrance,
                    item_flags,
                    status,
                });
            }
            hints.push((key, Arc::new(list)));
        }

        let mut hints_used = Vec::new();
        for _ in 0..r.count()? {
            hints_used.push((r.key()?, r.ivar()?));
        }

        let mut name_aliases = Vec::new();
        for _ in 0..r.count()? {
            name_aliases.push((r.key()?, r.str()?));
        }

        let mut client_game_state = Vec::new();
        for _ in 0..r.count()? {
            let key = r.key()?;
            let status = ClientStatus::from_wire(r.tag()?)
                .ok_or(SaveError::Malformed("unknown client status"))?;
            client_game_state.push((key, status));
        }

        let mut group_collected = Vec::new();
        for _ in 0..r.count()? {
            let group = r.u32var()?;
            let n = r.count()?;
            let mut members = Vec::with_capacity(n);
            for _ in 0..n {
                members.push(r.u32var()?);
            }
            group_collected.push((group, members));
        }

        let mut allow_releases = Vec::new();
        for _ in 0..r.count()? {
            allow_releases.push(r.key()?);
        }

        let mut stored_data = Vec::new();
        for _ in 0..r.count()? {
            let key = r.str()?;
            let raw = r.bytes()?;
            let value: Value = serde_json::from_slice(raw)
                .map_err(|_| SaveError::Malformed("stored value is not JSON"))?;
            stored_data.push((key, Arc::new(value)));
        }

        let mut timers = [Vec::new(), Vec::new()];
        for slot in &mut timers {
            for _ in 0..r.count()? {
                slot.push((r.key()?, r.uvar()?));
            }
        }
        let [activity_at, connected_at] = timers;

        // Absent before version 2, and read on the version rather than on
        // whether bytes remain. See [`FORMAT_VERSION`].
        let mut locked_slots = Vec::new();
        if version >= VERSION_LOCKED_SLOTS {
            for _ in 0..r.count()? {
                locked_slots.push(r.key()?);
            }
        }

        Ok(Self {
            seed_name,
            options,
            rng_state,
            location_checks,
            received_items,
            hints,
            hints_used,
            name_aliases,
            client_game_state,
            group_collected,
            allow_releases,
            stored_data,
            activity_at,
            connected_at,
            locked_slots,
        })
    }
}

/// Everything about a room's configuration **except its secrets**.
///
/// Passwords are deliberately absent. They used to be the first two fields
/// here, and because [`Room::restore`](crate::Room::restore) assigns the
/// decoded options wholesale, a saved password silently replaced the configured
/// one on every restart — so rotating a password appeared to work and then
/// reverted, and the configured value was never authoritative. The environment
/// is the only source now, re-read on every start, which is also what lets a
/// live rotation survive a restart.
fn encode_options(w: &mut Writer, o: &RoomOptions) {
    w.uvar(o.hint_cost as u64);
    w.uvar(o.location_check_points as u64);
    for mode in [
        o.release_mode,
        o.collect_mode,
        o.remaining_mode,
        o.countdown_mode,
    ] {
        w.tag(mode as i64);
    }
    w.byte(u8::from(o.item_cheat));
    w.byte(o.compatibility);
    w.uvar(o.tags.len() as u64);
    for tag in &o.tags {
        w.str(tag);
    }
}

fn decode_options(r: &mut Reader<'_>) -> Result<RoomOptions> {
    let hint_cost = r.u32var()?;
    let location_check_points = r.u32var()?;
    let mut modes = [Permission::Disabled; 4];
    for mode in &mut modes {
        *mode = Permission::from_wire(r.tag()?)
            .ok_or(SaveError::Malformed("unknown permission mode"))?;
    }
    let item_cheat = r.byte()? != 0;
    let compatibility = r.byte()?;
    let mut tags = Vec::new();
    for _ in 0..r.count()? {
        tags.push(r.str()?);
    }
    Ok(RoomOptions {
        // Never restored, so the configured values survive. See
        // `encode_options`.
        password: None,
        server_password: None,
        slot_passwords: None,
        hint_cost,
        location_check_points,
        release_mode: modes[0],
        collect_mode: modes[1],
        remaining_mode: modes[2],
        countdown_mode: modes[3],
        item_cheat,
        compatibility,
        tags,
    })
}

// --- primitives -----------------------------------------------------------

#[derive(Default)]
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn into_inner(self) -> Vec<u8> {
        self.out
    }

    fn byte(&mut self, b: u8) {
        self.out.push(b);
    }

    fn u32(&mut self, v: u32) {
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    /// LEB128.
    fn uvar(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.out.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.out.push(v as u8);
    }

    /// Zigzag then LEB128, so small negatives stay small.
    fn ivar(&mut self, v: i64) {
        self.uvar(((v << 1) ^ (v >> 63)) as u64);
    }

    fn bytes(&mut self, b: &[u8]) {
        self.uvar(b.len() as u64);
        self.out.extend_from_slice(b);
    }

    fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn key(&mut self, (team, slot): SlotKey) {
        self.uvar(team as u64);
        self.uvar(slot as u64);
    }

    /// An enum discriminant. Paired with [`Reader::tag`] so the two sides
    /// cannot drift apart on whether the value is zigzagged — discriminants are
    /// small and non-negative, and zigzagging them would only cost bytes.
    fn tag(&mut self, v: i64) {
        self.uvar(v as u64);
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.at < n {
            return Err(SaveError::Truncated {
                at: self.at,
                wanted: n,
                left: self.buf.len() - self.at,
            });
        }
        let out = &self.buf[self.at..self.at + n];
        self.at += n;
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn uvar(&mut self) -> Result<u64> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            out |= u64::from(b & 0x7f)
                .checked_shl(shift)
                .ok_or(SaveError::Malformed("varint too long"))?;
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(SaveError::Malformed("varint too long"))
    }

    fn ivar(&mut self) -> Result<i64> {
        let v = self.uvar()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    fn u32var(&mut self) -> Result<u32> {
        u32::try_from(self.uvar()?).map_err(|_| SaveError::Malformed("value does not fit in u32"))
    }

    /// A length that is about to drive an allocation, so it is checked against
    /// what is actually left rather than trusted. A corrupt count must not turn
    /// into a multi-gigabyte `with_capacity`.
    fn count(&mut self) -> Result<usize> {
        let n = self.uvar()? as usize;
        let left = self.buf.len() - self.at;
        if n > left {
            return Err(SaveError::Truncated {
                at: self.at,
                wanted: n,
                left,
            });
        }
        Ok(n)
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.count()?;
        self.take(n)
    }

    fn str(&mut self) -> Result<String> {
        let raw = self.bytes()?;
        String::from_utf8(raw.to_vec()).map_err(|_| SaveError::Malformed("string is not UTF-8"))
    }

    fn key(&mut self) -> Result<SlotKey> {
        Ok((self.u32var()?, self.u32var()?))
    }

    /// The counterpart of [`Writer::tag`].
    fn tag(&mut self) -> Result<i64> {
        Ok(self.uvar()? as i64)
    }
}

fn deflate(body: &[u8]) -> Vec<u8> {
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    // Level 1: this runs on a background thread but a save is on the room's
    // recovery-point path, and the body is already varint-packed, so the ratio
    // difference between level 1 and level 6 is small next to the time.
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(body.len() / 3), Compression::fast());
    encoder
        .write_all(body)
        .expect("writing to a Vec cannot fail");
    encoder.finish().expect("writing to a Vec cannot fail")
}

fn inflate(body: &[u8], expected: usize) -> Result<Vec<u8>> {
    use flate2::write::ZlibDecoder;
    use std::io::Write;

    let mut decoder = ZlibDecoder::new(Vec::with_capacity(expected));
    decoder
        .write_all(body)
        .map_err(|e| SaveError::Inflate(e.to_string()))?;
    decoder
        .finish()
        .map_err(|e| SaveError::Inflate(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Build a header around an arbitrary body, so a decode can be aimed at the
    /// body parser rather than being stopped by the checksum first.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut crc = flate2::Crc::new();
        crc.update(body);
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        out.push(ENCODING_RAW);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn a_huge_count_fails_instead_of_reserving_for_it() {
        // A save comes off a shared filesystem, so a length in it is a claim,
        // not a fact. `Reader::count` checks every length that drives an
        // allocation against what is actually left — without that, one corrupt
        // varint is a multi-gigabyte `with_capacity` and an OOM at startup.
        let mut w = Writer::default();
        w.uvar(u32::MAX as u64); // a seed name of four billion bytes
        let body = w.into_inner();
        assert!(matches!(
            Snapshot::decode(&framed(&body)),
            Err(SaveError::Truncated { .. })
        ));
    }

    #[test]
    fn varints_round_trip_at_the_edges() {
        let cases = [
            0i64,
            1,
            -1,
            127,
            128,
            -128,
            i64::MAX,
            i64::MIN,
            4_000_000_000,
            -4_000_000_000,
        ];
        let mut w = Writer::default();
        for v in cases {
            w.ivar(v);
        }
        let body = w.into_inner();
        let mut r = Reader::new(&body);
        for v in cases {
            assert_eq!(r.ivar().unwrap(), v);
        }
    }

    #[test]
    fn a_varint_that_never_terminates_is_rejected() {
        // Ten continuation bytes overruns 64 bits. Without the bound this loops
        // until the buffer runs out, which on a large save is a long time to
        // spend on garbage.
        let body = vec![0xff; 16];
        let mut r = Reader::new(&body);
        assert!(matches!(r.uvar(), Err(SaveError::Malformed(_))));
    }

    /// The save carries no secrets, so a room started with a password keeps it
    /// across a restore rather than having it replaced by what was on disk.
    /// This is the regression test for the bug that motivated dropping them:
    /// the saved value used to win, so a rotated password reverted on restart.
    #[test]
    fn restoring_does_not_disturb_the_configured_passwords() {
        let mut options = RoomOptions {
            password: Some("from-the-environment".to_string()),
            server_password: Some("admin-secret".to_string()),
            hint_cost: 42,
            ..Default::default()
        };
        options.slot_passwords = Some(BTreeMap::from([(3, "per-slot-secret".to_string())]));

        let mut w = Writer::default();
        encode_options(&mut w, &options);
        let body = w.into_inner();

        // Nothing secret reaches the encoding at all.
        for secret in ["from-the-environment", "admin-secret", "per-slot-secret"] {
            assert!(
                !body.windows(secret.len()).any(|w| w == secret.as_bytes()),
                "{secret:?} was written into the save"
            );
        }

        // And what comes back carries none of them, so a wholesale assignment
        // could only ever clear what was configured — which is why
        // `Room::restore` puts them back explicitly.
        let decoded = decode_options(&mut Reader::new(&body)).expect("decodes");
        assert_eq!(decoded.password, None);
        assert_eq!(decoded.server_password, None);
        assert!(decoded.slot_passwords.is_none());
        assert_eq!(
            decoded.hint_cost, 42,
            "the non-secret options still survive"
        );
    }
}
