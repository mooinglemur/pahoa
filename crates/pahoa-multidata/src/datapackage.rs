//! Item and location name tables, and where they come from.
//!
//! The server needs four pure-data maps per game, exactly the set
//! `WebHostLib/customserver.py:277-303` hands to a room process: item and
//! location name↔id, the name groups, and the hint blacklist. No world code
//! runs to produce them.
//!
//! **Three of the four come from the seed.** A freshly generated `.archipelago`
//! embeds a full package for every game in it (`Main.py:315-320`), including the
//! name groups, so a custom apworld this build has never heard of still resolves
//! its own names.
//!
//! **The fourth, `hint_blacklist`, is serialized nowhere** — it is Python class
//! data the reference reads out of its installed worlds — so it is compiled into
//! this binary instead. See [`crate::hint_blacklist`] for what that trades away.
//!
//! The one case the seed cannot cover is a package WebHost has *stripped* to
//! `{version, checksum}` on upload (`WebHostLib/upload.py:56-78`). That game is
//! reported unresolved and its names degrade to `Unknown item (ID:n)`; the room
//! still hosts, because refusing to start over cosmetic names would be worse
//! than the names being ugly.

use crate::error::{Path, Result};
use crate::extract::Extract;
use pahoa_pickle::PyObj;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One game's data. Field names match Archipelago's `GamesPackage`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GamePackage {
    #[serde(default)]
    pub item_name_to_id: BTreeMap<String, i64>,
    #[serde(default)]
    pub location_name_to_id: BTreeMap<String, i64>,
    /// Absent on pre-0.3.9 seeds, which is why clients must tolerate its
    /// absence in `RoomInfo.datapackage_checksums`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    /// Server-side only: stripped from what is sent to clients and exposed via
    /// the `_read_item_name_groups_*` data storage key instead.
    #[serde(default)]
    pub item_name_groups: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub location_name_groups: BTreeMap<String, Vec<String>>,
    /// Never present in multidata. Filled from [`crate::hint_blacklist`] during
    /// the merge, and anything a seed happens to carry here is discarded.
    #[serde(default)]
    pub hint_blacklist: HashSet<String>,
}

impl GamePackage {
    /// Reverse maps, built once at load. `!missing`, `!checked` and every log
    /// line resolve ids to names, so this cannot be a linear scan.
    fn build_reverse(&self) -> (HashMap<i64, String>, HashMap<i64, String>) {
        (
            self.item_name_to_id
                .iter()
                .map(|(n, &i)| (i, n.clone()))
                .collect(),
            self.location_name_to_id
                .iter()
                .map(|(n, &i)| (i, n.clone()))
                .collect(),
        )
    }

    fn from_py(v: &PyObj, path: &Path) -> Result<Self> {
        fn name_map(v: Option<&PyObj>, path: &Path) -> Result<BTreeMap<String, i64>> {
            let Some(v) = v else {
                return Ok(BTreeMap::new());
            };
            v.dict_(path)?
                .iter()
                .map(|(k, val)| {
                    let name = k.str_(path)?.to_string();
                    let id = val.int(&path.key(&name))?;
                    Ok((name, id))
                })
                .collect()
        }

        fn group_map(v: Option<&PyObj>, path: &Path) -> Result<BTreeMap<String, Vec<String>>> {
            let Some(v) = v else {
                return Ok(BTreeMap::new());
            };
            v.dict_(path)?
                .iter()
                .map(|(k, val)| {
                    let name = k.str_(path)?.to_string();
                    let members = val
                        .seq(&path.key(&name))?
                        .iter()
                        .map(|m| Ok(m.str_(&path.key(&name))?.to_string()))
                        .collect::<Result<Vec<_>>>()?;
                    Ok((name, members))
                })
                .collect()
        }

        Ok(Self {
            item_name_to_id: name_map(v.opt("item_name_to_id"), &path.key("item_name_to_id"))?,
            location_name_to_id: name_map(
                v.opt("location_name_to_id"),
                &path.key("location_name_to_id"),
            )?,
            checksum: match v.opt("checksum") {
                Some(c) => Some(c.str_(&path.key("checksum"))?.to_string()),
                None => None,
            },
            item_name_groups: group_map(v.opt("item_name_groups"), &path.key("item_name_groups"))?,
            location_name_groups: group_map(
                v.opt("location_name_groups"),
                &path.key("location_name_groups"),
            )?,
            // Multidata never carries this.
            hint_blacklist: HashSet::new(),
        })
    }

    /// True when this looks like a WebHost-stripped stub rather than real data.
    fn is_stub(&self) -> bool {
        self.item_name_to_id.is_empty() && self.location_name_to_id.is_empty()
    }
}

/// Name lookups for one game, with ids resolved in both directions.
#[derive(Debug, Clone, Default)]
pub struct GameNames {
    pub package: GamePackage,
    item_by_id: HashMap<i64, String>,
    location_by_id: HashMap<i64, String>,
}

impl GameNames {
    fn new(package: GamePackage) -> Self {
        let (item_by_id, location_by_id) = package.build_reverse();
        Self {
            package,
            item_by_id,
            location_by_id,
        }
    }

    /// Matches Archipelago's `KeyedDefaultDict` fallback text exactly
    /// (`MultiServer.py:327-330`), because it is player-visible in chat.
    pub fn item_name(&self, id: i64) -> String {
        self.item_by_id
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("Unknown item (ID:{id})"))
    }

    pub fn location_name(&self, id: i64) -> String {
        self.location_by_id
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("Unknown location (ID:{id})"))
    }

    pub fn item_id(&self, name: &str) -> Option<i64> {
        self.package.item_name_to_id.get(name).copied()
    }

    pub fn location_id(&self, name: &str) -> Option<i64> {
        self.package.location_name_to_id.get(name).copied()
    }

    pub fn is_hintable(&self, name: &str) -> bool {
        !self.package.hint_blacklist.contains(name)
    }

    /// Everything `!hint` will match a name against: items plus item groups
    /// (`MultiServer.py:359-360`).
    ///
    /// Sorted, and deduplicated where a group shares an item's name. Python
    /// builds a `set` here, so ties in the fuzzy ranking that follows resolve
    /// in whatever order CPython's hashing produced — not reproducible even
    /// between its own runs. A deterministic order costs nothing (both sources
    /// are already sorted maps, so this is a merge, not a sort) and makes the
    /// outcome testable.
    pub fn item_and_group_names(&self) -> Vec<&str> {
        union_sorted(
            self.package.item_name_to_id.keys(),
            self.package.item_name_groups.keys(),
        )
    }

    /// The same for `!hint_location`.
    pub fn location_and_group_names(&self) -> Vec<&str> {
        union_sorted(
            self.package.location_name_to_id.keys(),
            self.package.location_name_groups.keys(),
        )
    }
}

/// Merge two already-sorted key sequences into one sorted, deduplicated list.
fn union_sorted<'a>(
    a: impl Iterator<Item = &'a String>,
    b: impl Iterator<Item = &'a String>,
) -> Vec<&'a str> {
    let mut a = a.peekable();
    let mut b = b.peekable();
    let mut out = Vec::new();
    loop {
        let next = match (a.peek(), b.peek()) {
            (None, None) => return out,
            (Some(_), None) => a.next(),
            (None, Some(_)) => b.next(),
            (Some(x), Some(y)) => match x.cmp(y) {
                std::cmp::Ordering::Less => a.next(),
                std::cmp::Ordering::Greater => b.next(),
                // Present in both: take one and drop the other.
                std::cmp::Ordering::Equal => {
                    b.next();
                    a.next()
                }
            },
        };
        out.push(next.expect("peeked").as_str());
    }
}

/// The merged data package for a room.
#[derive(Debug, Clone, Default)]
pub struct DataPackage {
    games: BTreeMap<String, GameNames>,
}

/// What the merge found, so an operator can see whether anything is missing.
///
/// There is no "missing hint blacklist" any more: the table is compiled in, so
/// every game gets one, and a game absent from it gets an empty set rather than
/// an unknown one — the same answer the reference gives a world that sets none.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Games taken from the multidata's embedded package.
    pub from_multidata: Vec<String>,
    /// Games whose package was absent or stripped. Item and location names
    /// render as `Unknown item (ID:n)` for these.
    pub unresolved: Vec<String>,
}

impl DataPackage {
    pub fn get(&self, game: &str) -> Option<&GameNames> {
        self.games.get(game)
    }

    pub fn games(&self) -> impl Iterator<Item = (&String, &GameNames)> {
        self.games.iter()
    }

    pub fn len(&self) -> usize {
        self.games.len()
    }

    pub fn is_empty(&self) -> bool {
        self.games.is_empty()
    }

    /// Per-game checksums for `RoomInfo.datapackage_checksums`. Games without a
    /// checksum are omitted, matching `MultiServer.py:934-935`.
    pub fn checksums(&self) -> BTreeMap<&str, &str> {
        self.games
            .iter()
            .filter_map(|(g, n)| n.package.checksum.as_deref().map(|c| (g.as_str(), c)))
            .collect()
    }

    /// Build the name tables for the games a seed actually uses.
    ///
    /// `needed` is the set of games present in the seed. Everything comes from
    /// the multidata's own embedded package except the hint blacklist, which is
    /// serialized nowhere and comes from the table compiled into this binary —
    /// see [`crate::hint_blacklist`].
    pub fn merge(
        embedded: &BTreeMap<String, GamePackage>,
        needed: &HashSet<String>,
    ) -> (Self, MergeReport) {
        let mut games = BTreeMap::new();
        let mut report = MergeReport::default();

        for game in needed {
            let mut merged = match embedded.get(game).filter(|p| !p.is_stub()) {
                Some(e) => {
                    report.from_multidata.push(game.clone());
                    e.clone()
                }
                // A seed whose package was stripped, which WebHost does on
                // upload. Names degrade to "Unknown item (ID:n)" rather than
                // the room refusing to start, and the caller says so.
                None => {
                    report.unresolved.push(game.clone());
                    GamePackage::default()
                }
            };
            // Grafted for every game, resolved or not: a game with no entry
            // gets an empty set, which is what the reference gives a world that
            // sets no `hint_blacklist`.
            merged.hint_blacklist = crate::hint_blacklist::for_game(game)
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            games.insert(game.clone(), GameNames::new(merged));
        }

        // Every game can carry Archipelago's own items and locations — the
        // cheat console, `Nothing` — so their ids resolve in any game's
        // context. Only the id->name direction is merged, exactly as the
        // reference does (`MultiServer.py:364-368`): `item_name_to_id` stays
        // the game's own, which is what `!hint` matches names against.
        if let Some(ap) = games.get("Archipelago") {
            let (items, locations) = (ap.item_by_id.clone(), ap.location_by_id.clone());
            for (name, game) in games.iter_mut() {
                if name == "Archipelago" {
                    continue;
                }
                game.item_by_id
                    .extend(items.iter().map(|(k, v)| (*k, v.clone())));
                game.location_by_id
                    .extend(locations.iter().map(|(k, v)| (*k, v.clone())));
            }
        }

        report.from_multidata.sort();
        report.unresolved.sort();
        (Self { games }, report)
    }

    /// Parse the multidata's `datapackage` field.
    pub fn embedded_from_py(v: &PyObj, path: &Path) -> Result<BTreeMap<String, GamePackage>> {
        v.dict_(path)?
            .iter()
            .map(|(k, val)| {
                let game = k.str_(path)?.to_string();
                let pkg = GamePackage::from_py(val, &path.key(&game))?;
                Ok((game, pkg))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(items: &[(&str, i64)], blacklist: &[&str]) -> GamePackage {
        GamePackage {
            item_name_to_id: items.iter().map(|(n, i)| (n.to_string(), *i)).collect(),
            hint_blacklist: blacklist.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn needed(games: &[&str]) -> HashSet<String> {
        games.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_embedded_package_is_the_source_of_names() {
        let embedded = BTreeMap::from([("G".into(), pkg(&[("New", 2)], &[]))]);
        let (dp, report) = DataPackage::merge(&embedded, &needed(&["G"]));

        assert_eq!(dp.get("G").unwrap().item_id("New"), Some(2));
        assert_eq!(report.from_multidata, ["G"]);
        assert!(report.unresolved.is_empty());
    }

    /// The blacklist is serialized nowhere, so it has to come from the table
    /// compiled in — and it has to reach a game whose names came from the seed.
    #[test]
    fn the_built_in_hint_blacklist_is_grafted_onto_embedded_data() {
        let embedded = BTreeMap::from([(
            "A Link to the Past".into(),
            pkg(&[("Triforce", 1), ("Bow", 2)], &[]),
        )]);
        let (dp, _) = DataPackage::merge(&embedded, &needed(&["A Link to the Past"]));

        let g = dp.get("A Link to the Past").unwrap();
        assert!(
            !g.is_hintable("Triforce"),
            "the built-in entry was not applied"
        );
        assert!(g.is_hintable("Bow"));
    }

    /// A package carrying its own `hint_blacklist` must not override the built-in
    /// one: multidata never contains this field, so anything that appears there
    /// is noise rather than data.
    #[test]
    fn an_embedded_blacklist_does_not_displace_the_built_in_one() {
        let embedded = BTreeMap::from([(
            "A Link to the Past".into(),
            pkg(&[("Triforce", 1), ("Bow", 2)], &["Bow"]),
        )]);
        let (dp, _) = DataPackage::merge(&embedded, &needed(&["A Link to the Past"]));

        let g = dp.get("A Link to the Past").unwrap();
        assert!(!g.is_hintable("Triforce"));
        assert!(g.is_hintable("Bow"), "a seed dictated the blacklist");
    }

    /// Absence means "hints everything", which is what the reference gives a
    /// world that sets no `hint_blacklist` — not an error and not a warning.
    #[test]
    fn a_game_with_no_built_in_entry_hints_everything() {
        let embedded = BTreeMap::from([("Balatro".into(), pkg(&[("Joker", 1)], &[]))]);
        let (dp, report) = DataPackage::merge(&embedded, &needed(&["Balatro"]));

        assert!(dp.get("Balatro").unwrap().is_hintable("Joker"));
        assert!(report.unresolved.is_empty());
    }

    /// Upload replaces the package with {version, checksum}. With no snapshot to
    /// fall back to, the game is unresolved and names degrade — the room still
    /// hosts, and the caller is told.
    #[test]
    fn a_webhost_stripped_stub_is_reported_as_unresolved() {
        let stub = GamePackage {
            checksum: Some("abc".into()),
            ..Default::default()
        };
        let embedded = BTreeMap::from([("G".into(), stub)]);
        let (dp, report) = DataPackage::merge(&embedded, &needed(&["G"]));

        assert_eq!(report.unresolved, ["G"]);
        assert_eq!(dp.get("G").unwrap().item_name(42), "Unknown item (ID:42)");
    }

    #[test]
    fn unresolved_games_still_load_with_unknown_name_fallbacks() {
        // A missing package must degrade, not fail: the room still hosts.
        let (dp, report) = DataPackage::merge(&BTreeMap::new(), &needed(&["Mystery"]));
        assert_eq!(report.unresolved, ["Mystery"]);
        assert_eq!(
            dp.get("Mystery").unwrap().item_name(42),
            "Unknown item (ID:42)"
        );
        assert_eq!(
            dp.get("Mystery").unwrap().location_name(7),
            "Unknown location (ID:7)"
        );
    }

    #[test]
    fn drops_embedded_games_the_seed_does_not_use() {
        let embedded = BTreeMap::from([
            ("Used".into(), pkg(&[("A", 1)], &[])),
            ("Unused".into(), pkg(&[("B", 2)], &[])),
        ]);
        let (dp, _) = DataPackage::merge(&embedded, &needed(&["Used"]));
        assert_eq!(dp.len(), 1);
        assert!(dp.get("Unused").is_none());
    }

    #[test]
    fn checksums_omit_games_that_have_none() {
        let with = GamePackage {
            checksum: Some("aaa".into()),
            ..pkg(&[("A", 1)], &[])
        };
        let without = pkg(&[("B", 2)], &[]);
        let embedded = BTreeMap::from([("W".into(), with), ("N".into(), without)]);
        let (dp, _) = DataPackage::merge(&embedded, &needed(&["W", "N"]));

        let sums = dp.checksums();
        assert_eq!(sums.get("W"), Some(&"aaa"));
        assert!(!sums.contains_key("N"));
    }

    #[test]
    fn resolves_names_in_both_directions() {
        let (dp, _) = DataPackage::merge(
            &BTreeMap::from([("G".into(), pkg(&[("Sword", 5)], &[]))]),
            &needed(&["G"]),
        );
        let g = dp.get("G").unwrap();
        assert_eq!(g.item_id("Sword"), Some(5));
        assert_eq!(g.item_name(5), "Sword");
    }
}
