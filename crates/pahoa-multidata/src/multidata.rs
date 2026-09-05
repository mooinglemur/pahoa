//! The `.archipelago` container and its typed contents.
//!
//! Wire format: one format-version byte, then zlib-compressed pickle
//! (`Main.py:357-361`, read back at `MultiServer.py:488-493`).
//!
//! Two fields are deliberately left as raw [`PyObj`]: `slot_data` and
//! `server_options`. `slot_data` is opaque per-world state forwarded to clients
//! verbatim, and it can contain integers wider than `i64` (a real seed carries
//! one exceeding `u64`), so it must not be coerced through a typed model on the
//! way through. Encoding it to JSON is the wire layer's problem, not this
//! crate's.

use crate::datapackage::{DataPackage, GamePackage, MergeReport};
use crate::error::{Error, Path, Result};
use crate::extract::Extract;
use crate::locations::LocationStore;
use crate::types::{Hint, NetworkSlot, SlotType, Version};
use pahoa_pickle::{Allowlist, PyObj};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;

/// Highest container format this server understands (`MultiServer.py:490-491`).
pub const MAX_FORMAT_VERSION: u8 = 3;

/// The Archipelago version this server reports and validates seeds against.
///
/// **Here rather than in `pahoa-room`, because this is the crate that checks
/// it.** `validate` refuses a seed demanding a newer server, and a caller that
/// links only the parser previously had to transcribe this constant from
/// another crate — or, more honestly, skip the check entirely rather than rest
/// it on a number copied across a repository boundary. It is a fact about what
/// the parser will accept, so it belongs beside the parser.
///
/// `pahoa_room::SERVER_VERSION` is derived from this one rather than written
/// out again, so the wire and the load-time check cannot disagree.
pub const SERVER_VERSION: Version = Version::new(0, 6, 7);

/// Most decompressed pickle this will inflate before refusing the file.
///
/// **Without a ceiling here, `read_to_end` on a zlib stream is a decompression
/// bomb.** zlib reaches about 1032:1, so the only bound was whatever the caller
/// was willing to hand over — an orchestrator accepting 256 MiB uploads was
/// implicitly offering 264 GiB of inflate to one HTTP request.
///
/// 64 MiB against a sixteen-seed corpus whose largest member inflates to
/// **6.94 MiB** (a synthetic 2000-slot seed; the largest *real* one is 5.66
/// MiB), and whose compression ratios run 2.29:1 to 4.55:1. That is roughly
/// nine times the biggest thing anyone has, which leaves room for seeds several
/// times larger than this server is designed for while turning an unbounded
/// allocation into a bounded one.
///
/// A caller may well cap harder — an orchestrator that knows its own upload
/// limits should — but a constant here is the one every caller gets for free,
/// and a standalone `pahoa` has no other.
pub const MAX_PICKLE_BYTES: u64 = 64 * 1024 * 1024;

/// Most items a seed may hand out before the room ever starts.
///
/// The field the known attack uses, and the one whose cost **outlives the
/// parse**: `precollected_items` survives typing as `HashMap<u32, Vec<i64>>`
/// and then becomes items handed to a slot at connect and held for the room's
/// lifetime. A file carrying 11,422,785 of them cost the reference server about
/// a gigabyte per room, indefinitely, rather than only during load.
///
/// Bytes are the wrong unit for that, which is why this is counted separately
/// from [`MAX_PICKLE_BYTES`] and from the reader's object budget. The corpus
/// maximum is **2,975** across all slots, so 100,000 is thirty-three times
/// anything real — and deliberately the same number the orchestrator upstream
/// of this uses, so the two cannot disagree about what a room may be asked to
/// load.
pub const MAX_PRECOLLECTED_ITEMS: usize = 100_000;

/// Minimum client version the reference server enforces (`MultiServer.py:51`).
pub const MIN_CLIENT_VERSION: Version = Version::new(0, 5, 0);

/// The one team a multiworld has.
///
/// Not a magic zero: see [`MultiData::teams`] for why there is exactly one and
/// what would have to change for there to be more. Every `(team, slot)` key in
/// this server carries a team because Archipelago's does; this is the value it
/// carries.
pub const ONLY_TEAM: u32 = 0;

/// Generators older than this kept a much lower client floor
/// (`MultiServer.py:509-514`).
const LEGACY_GENERATOR_CUTOFF: Version = Version::new(0, 6, 2);
const LEGACY_MIN_CLIENT_VERSION: Version = Version::new(0, 1, 6);

#[derive(Debug, Clone)]
pub struct MultiData {
    pub seed_name: String,
    /// Version of Archipelago that generated the seed.
    pub generator_version: Version,
    /// Minimum server version the seed demands.
    pub minimum_server_version: Version,
    /// Per-slot client version floors, already combined with the global floor.
    pub minimum_client_versions: HashMap<u32, Version>,
    pub slot_info: BTreeMap<u32, NetworkSlot>,
    /// Slot name -> `(team, slot)`. This is the whole of Archipelago's slot
    /// identity today: an unauthenticated exact string match.
    pub connect_names: HashMap<String, (u32, u32)>,
    pub locations: LocationStore,
    /// Items granted before play begins, per slot.
    pub precollected_items: HashMap<u32, Vec<i64>>,
    pub precollected_hints: HashMap<u32, Vec<Hint>>,
    /// Entrance names for hint text, `{slot: {location: entrance}}`.
    pub er_hint_data: HashMap<u32, HashMap<i64, String>>,
    /// Progression spheres, used to bias hint ordering toward earlier ones.
    pub spheres: Vec<HashMap<u32, HashSet<i64>>>,
    pub race_mode: bool,
    /// Opaque per-slot data, forwarded to clients untouched.
    pub slot_data: HashMap<u32, PyObj>,
    /// Server options baked in at generation, applied only when the operator
    /// opts in (`--use_embedded_options`; WebHost always does).
    pub server_options: Option<PyObj>,
    /// Data package embedded in the seed, before merging with a snapshot.
    pub embedded_datapackage: BTreeMap<String, GamePackage>,
}

impl MultiData {
    /// Decode a `.archipelago` file: format byte, zlib, pickle, then typing.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let format = *raw.first().ok_or(Error::Empty)?;
        if format > MAX_FORMAT_VERSION {
            return Err(Error::UnsupportedFormat(format));
        }
        // **Bounded, and the bound is checked by overshooting it by one byte.**
        // `take(N)` alone stops at N and reports success, which would hand a
        // truncated pickle to the reader and produce a confusing parse error
        // instead of an honest "this file is too big". Asking for one more byte
        // than allowed distinguishes "exactly at the limit" from "longer than
        // the limit" without inflating the rest of a hostile stream.
        let mut pickle = Vec::new();
        flate2::read::ZlibDecoder::new(&raw[1..])
            .take(MAX_PICKLE_BYTES + 1)
            .read_to_end(&mut pickle)?;
        if pickle.len() as u64 > MAX_PICKLE_BYTES {
            return Err(Error::PickleTooLarge {
                limit: MAX_PICKLE_BYTES,
            });
        }
        let obj = pahoa_pickle::from_slice(&pickle, &Allowlist::archipelago())?;
        Self::from_py(&obj)
    }

    pub fn from_py(v: &PyObj) -> Result<Self> {
        let root = Path::root();
        v.dict_(&root)?;

        let seed_name = v
            .at(&root, "seed_name")?
            .str_(&root.key("seed_name"))?
            .to_string();

        let generator_version = Version::from_py(v.at(&root, "version")?, &root.key("version"))?;

        let min_versions = v.at(&root, "minimum_versions")?;
        let mv_path = root.key("minimum_versions");
        let minimum_server_version =
            Version::from_py(min_versions.at(&mv_path, "server")?, &mv_path.key("server"))?;

        // A seed that needs a newer server is refused outright rather than
        // hosted incorrectly (`MultiServer.py:502-505`). The caller decides
        // what to do; we only record it here and check in `validate`.

        // Old generators keep the old client floor so existing seeds stay
        // playable with older clients.
        let floor = if generator_version < LEGACY_GENERATOR_CUTOFF {
            LEGACY_MIN_CLIENT_VERSION
        } else {
            MIN_CLIENT_VERSION
        };
        let mut minimum_client_versions = HashMap::new();
        if let Some(clients) = min_versions.opt("clients") {
            let cp = mv_path.key("clients");
            for (slot, ver) in clients.dict_(&cp)? {
                let slot = slot.u32_(&cp)?;
                let v = Version::from_py(ver, &cp.index(slot))?;
                minimum_client_versions.insert(slot, v.max(floor));
            }
        }

        let si_path = root.key("slot_info");
        let slot_info: BTreeMap<u32, NetworkSlot> = v
            .at(&root, "slot_info")?
            .dict_(&si_path)?
            .iter()
            .map(|(k, val)| {
                let slot = k.u32_(&si_path)?;
                Ok((slot, NetworkSlot::from_py(val, &si_path.index(slot))?))
            })
            .collect::<Result<_>>()?;

        let cn_path = root.key("connect_names");
        let connect_names = v
            .at(&root, "connect_names")?
            .dict_(&cn_path)?
            .iter()
            .map(|(k, val)| {
                let name = k.str_(&cn_path)?.to_string();
                let pair = val.tuple_n(&cn_path.key(&name), 2)?;
                let team = pair[0].u32_(&cn_path.key(&name).index(0))?;
                let slot = pair[1].u32_(&cn_path.key(&name).index(1))?;
                Ok((name, (team, slot)))
            })
            .collect::<Result<_>>()?;

        let locations = LocationStore::from_py(v.at(&root, "locations")?, &root.key("locations"))?;

        let precollected_items =
            int_list_map(v.opt("precollected_items"), &root.key("precollected_items"))?;
        // **In `from_py` rather than in `validate`**, because `validate` is a
        // policy check a caller opts into and this is a refusal that has to
        // hold for anyone who types a seed at all — including a tool that only
        // ever inspects one. The tree is already built by here, so this does
        // not save the parse; what it bounds is the room afterwards, which is
        // where this field's cost actually lives.
        let granted: usize = precollected_items.values().map(Vec::len).sum();
        if granted > MAX_PRECOLLECTED_ITEMS {
            return Err(Error::TooManyPrecollectedItems {
                found: granted,
                limit: MAX_PRECOLLECTED_ITEMS,
            });
        }

        let ph_path = root.key("precollected_hints");
        let precollected_hints = match v.opt("precollected_hints") {
            None => HashMap::new(),
            Some(d) => d
                .dict_(&ph_path)?
                .iter()
                .map(|(k, val)| {
                    let slot = k.u32_(&ph_path)?;
                    let hints = val
                        .seq(&ph_path.index(slot))?
                        .iter()
                        .enumerate()
                        .map(|(i, h)| Hint::from_py(h, &ph_path.index(slot).index(i)))
                        .collect::<Result<Vec<_>>>()?;
                    Ok((slot, hints))
                })
                .collect::<Result<_>>()?,
        };

        let eh_path = root.key("er_hint_data");
        let er_hint_data = match v.opt("er_hint_data") {
            None => HashMap::new(),
            Some(d) => d
                .dict_(&eh_path)?
                .iter()
                .map(|(k, val)| {
                    let slot = k.u32_(&eh_path)?;
                    let inner = val
                        .dict_(&eh_path.index(slot))?
                        .iter()
                        .map(|(loc, name)| {
                            let p = eh_path.index(slot);
                            // Some worlds emit a null entrance name; a real seed
                            // has 2 among 1290 entries. Python never type-checks
                            // this and downstream only ever tests `if entrance:`,
                            // so None and "" behave identically. Normalize here
                            // rather than carrying an Option no caller can act on.
                            let name = match name {
                                PyObj::None => String::new(),
                                other => other.str_(&p)?.to_string(),
                            };
                            Ok((loc.int(&p)?, name))
                        })
                        .collect::<Result<_>>()?;
                    Ok((slot, inner))
                })
                .collect::<Result<_>>()?,
        };

        let sp_path = root.key("spheres");
        let spheres = match v.opt("spheres") {
            None => Vec::new(),
            Some(list) => list
                .seq(&sp_path)?
                .iter()
                .enumerate()
                .map(|(i, sphere)| {
                    let p = sp_path.index(i);
                    sphere
                        .dict_(&p)?
                        .iter()
                        .map(|(slot, locs)| {
                            let slot = slot.u32_(&p)?;
                            let set = locs
                                .seq(&p.index(slot))?
                                .iter()
                                .map(|l| l.int(&p.index(slot)))
                                .collect::<Result<HashSet<_>>>()?;
                            Ok((slot, set))
                        })
                        .collect::<Result<_>>()
                })
                .collect::<Result<_>>()?,
        };

        let race_mode = match v.opt("race_mode") {
            Some(r) => r.int(&root.key("race_mode"))? != 0,
            None => false,
        };

        let sd_path = root.key("slot_data");
        let slot_data = match v.opt("slot_data") {
            None => HashMap::new(),
            Some(d) => d
                .dict_(&sd_path)?
                .iter()
                .map(|(k, val)| Ok((k.u32_(&sd_path)?, val.clone())))
                .collect::<Result<_>>()?,
        };

        let embedded_datapackage = match v.opt("datapackage") {
            None => BTreeMap::new(),
            Some(d) => DataPackage::embedded_from_py(d, &root.key("datapackage"))?,
        };

        Ok(Self {
            seed_name,
            generator_version,
            minimum_server_version,
            minimum_client_versions,
            slot_info,
            connect_names,
            locations,
            precollected_items,
            precollected_hints,
            er_hint_data,
            spheres,
            race_mode,
            slot_data,
            server_options: v.opt("server_options").cloned(),
            embedded_datapackage,
        })
    }

    /// Every game named by a slot, plus `"Archipelago"`.
    ///
    /// The pseudo-game is always present because the server itself grants items
    /// from it (`MultiServer.py:922-923`).
    pub fn games(&self) -> HashSet<String> {
        let mut games: HashSet<String> = self.slot_info.values().map(|s| s.game.clone()).collect();
        games.insert("Archipelago".to_string());
        games
    }

    /// Name tables for the games this seed uses.
    ///
    /// Everything comes from the seed's own embedded package except the hint
    /// blacklist, which is serialized nowhere and is compiled into the binary.
    pub fn resolve_datapackage(&self) -> (DataPackage, MergeReport) {
        DataPackage::merge(&self.embedded_datapackage, &self.games())
    }

    /// Slots that have **progress to report** — checks, a goal, a completion
    /// percentage.
    ///
    /// Players only. A spectator has nothing to report here and would be a
    /// `0/0` row; a group is not a participant at all.
    ///
    /// This is not the same question as "who may connect" — see
    /// [`Self::connectable_slots`]. The two came apart the moment a spectator
    /// appeared, and using one where the other belongs is how a spectator goes
    /// missing from a roster or shows up as an idle player.
    pub fn player_slots(&self) -> impl Iterator<Item = (&u32, &NetworkSlot)> {
        self.slot_info
            .iter()
            .filter(|(_, s)| s.slot_type == SlotType::Player)
    }

    /// Every team in this seed, ascending. **Always exactly one, team 0.**
    ///
    /// Archipelago's data model is team-aware throughout — `(team, slot)` keys
    /// everything the server owns, the wire carries a `team` field, and
    /// `MultiServer.py` threads a team through hints, item queues and status —
    /// but **nothing can produce a second one**. Generation writes
    /// `{name: (0, player)}` unconditionally (`Main.py:337`), and the server
    /// seeds `self.clients = {0: {}}` at load and never grows it
    /// (`MultiServer.py:521`), so a seed naming any other team raises inside
    /// `ctx.clients[team][slot]` on the connect that used the name.
    ///
    /// pahoa serves what the reference serves, so this is one team, and
    /// [`Self::validate`] refuses a seed that says otherwise rather than
    /// half-serving it. What this accessor buys is that the limit is written
    /// down **once**: callers walk teams instead of writing `0`, so the day
    /// upstream can generate a second one, this and the validation move and the
    /// walks already work.
    pub fn teams(&self) -> impl Iterator<Item = u32> + Clone + use<> {
        std::iter::once(ONLY_TEAM)
    }

    /// Every `(team, slot)` a client may connect as, ascending.
    ///
    /// The roster question. One team today, so this is `connectable_slots` with
    /// a team on it — which is the point: a surface written against this keeps
    /// working when there is more than one, where a surface walking slots alone
    /// would show half the participants and look right.
    pub fn team_slots(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.teams()
            .flat_map(move |team| self.connectable_slots().map(move |(slot, _)| (team, *slot)))
    }

    /// Slots a client may **connect as** — players and spectators, groups
    /// excluded.
    ///
    /// The roster question: who needs a password, who appears on a room page,
    /// whose connections are counted. A spectator comes from someone's yaml,
    /// has a name in `connect_names`, connects, and watches the whole
    /// multiworld; it is a participant that happens to play nothing. A group is
    /// an item-link construct with no client behind it.
    ///
    /// Matches `WebHostLib/upload.py`'s rule, which keeps everything but groups.
    pub fn connectable_slots(&self) -> impl Iterator<Item = (&u32, &NetworkSlot)> {
        self.slot_info
            .iter()
            .filter(|(_, s)| s.slot_type != SlotType::Group)
    }

    /// The client version floor for a slot, including the global minimum.
    pub fn min_client_version(&self, slot: u32) -> Version {
        let floor = if self.generator_version < LEGACY_GENERATOR_CUTOFF {
            LEGACY_MIN_CLIENT_VERSION
        } else {
            MIN_CLIENT_VERSION
        };
        self.minimum_client_versions
            .get(&slot)
            .copied()
            .unwrap_or(floor)
    }

    /// Consistency checks the reference server performs at load time.
    pub fn validate(&self, server_version: Version) -> Result<()> {
        if self.minimum_server_version > server_version {
            return Err(Error::Locations(format!(
                "seed requires a server of at least {} but this is {server_version}",
                self.minimum_server_version
            )));
        }
        self.locations.validate()?;

        // Every connectable name must name a slot that exists, or a client
        // could authenticate into a slot with no world behind it.
        for (name, (team, slot)) in &self.connect_names {
            if !self.slot_info.contains_key(slot) {
                return Err(Error::Locations(format!(
                    "connect_names[{name:?}] points at slot {slot}, which has no slot_info"
                )));
            }
            // Refused rather than half-served. See `Self::teams`: the reference
            // cannot run this seed either, but it fails at the connect that
            // used the name, with a traceback, after the room is already up.
            // Saying so at load is the same limit stated where it can be acted
            // on.
            if *team != ONLY_TEAM {
                return Err(Error::Locations(format!(
                    "connect_names[{name:?}] is on team {team}, and this server serves \
                     one team, as the reference does"
                )));
            }
        }
        // Group members must exist too.
        for (slot, info) in &self.slot_info {
            for m in &info.group_members {
                if !self.slot_info.contains_key(m) {
                    return Err(Error::Locations(format!(
                        "slot {slot} lists group member {m}, which has no slot_info"
                    )));
                }
            }
        }
        Ok(())
    }
}

fn int_list_map(v: Option<&PyObj>, path: &Path) -> Result<HashMap<u32, Vec<i64>>> {
    let Some(d) = v else {
        return Ok(HashMap::new());
    };
    d.dict_(path)?
        .iter()
        .map(|(k, val)| {
            let slot = k.u32_(path)?;
            let items = val
                .seq(&path.index(slot))?
                .iter()
                .map(|i| i.int(&path.index(slot)))
                .collect::<Result<Vec<_>>>()?;
            Ok((slot, items))
        })
        .collect()
}
