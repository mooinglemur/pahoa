//! Item and location name tables, and the two places they come from.
//!
//! The server needs four pure-data maps per game, exactly the set
//! `WebHostLib/customserver.py:277-303` hands to a room process: item and
//! location name↔id, the name groups, and the hint blacklist. No world code
//! runs to produce them.
//!
//! There are two sources, and neither suffices alone:
//!
//! - **The multidata's embedded package.** A freshly generated `.archipelago`
//!   embeds a full package for every game in the seed (`Main.py:315-320`), so
//!   this covers custom apworlds the server has never heard of. But WebHost
//!   *strips* it to `{version, checksum}` on upload (`WebHostLib/upload.py:56-78`),
//!   old seeds may lack checksums, and the hint blacklist is never serialised
//!   anywhere.
//! - **An offline JSON snapshot**, exported from an Archipelago checkout by
//!   `tools/export-datapackage.py`. This is the only source of `hint_blacklist`.
//!
//! So: snapshot as the base layer, multidata overlaid on top, and the multidata
//! wins — that is what lets a seed using a custom apworld work against a
//! snapshot that predates it.

use crate::error::{Error, Path, Result};
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
    /// Never present in multidata; only ever from the snapshot.
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
}

/// The merged data package for a room.
#[derive(Debug, Clone, Default)]
pub struct DataPackage {
    games: BTreeMap<String, GameNames>,
}

/// What the merge did, so operators can see whether a snapshot was actually
/// needed and whether anything is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Games taken from the multidata's embedded package.
    pub from_multidata: Vec<String>,
    /// Games that fell back to the snapshot.
    pub from_snapshot: Vec<String>,
    /// Games with no usable package anywhere. Item and location names will
    /// render as `Unknown item (ID:n)` for these.
    pub unresolved: Vec<String>,
    /// Games resolved from the multidata but with no hint blacklist available,
    /// because that field exists only in the snapshot.
    pub missing_hint_blacklist: Vec<String>,
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

    /// Merge a snapshot with a multidata's embedded package.
    ///
    /// `needed` is the set of games actually present in the seed; anything else
    /// in the snapshot is dropped rather than carried around, which matters at
    /// 2000 slots where the snapshot may describe hundreds of unused games.
    pub fn merge(
        snapshot: &BTreeMap<String, GamePackage>,
        embedded: &BTreeMap<String, GamePackage>,
        needed: &HashSet<String>,
    ) -> (Self, MergeReport) {
        let mut games = BTreeMap::new();
        let mut report = MergeReport::default();

        for game in needed {
            let snap = snapshot.get(game);
            let emb = embedded.get(game).filter(|p| !p.is_stub());

            let merged = match (emb, snap) {
                // Embedded wins: it is authoritative for this seed and covers
                // custom apworlds the snapshot has never seen. The blacklist is
                // the one field it can never supply, so graft it from the
                // snapshot when available.
                (Some(e), snap) => {
                    report.from_multidata.push(game.clone());
                    let mut p = e.clone();
                    match snap {
                        Some(s) => p.hint_blacklist = s.hint_blacklist.clone(),
                        None => report.missing_hint_blacklist.push(game.clone()),
                    }
                    p
                }
                (None, Some(s)) => {
                    report.from_snapshot.push(game.clone());
                    s.clone()
                }
                (None, None) => {
                    report.unresolved.push(game.clone());
                    GamePackage::default()
                }
            };
            games.insert(game.clone(), GameNames::new(merged));
        }

        report.from_multidata.sort();
        report.from_snapshot.sort();
        report.unresolved.sort();
        report.missing_hint_blacklist.sort();
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

    /// Load a snapshot produced by `tools/export-datapackage.py`.
    pub fn load_snapshot(json: &str) -> Result<BTreeMap<String, GamePackage>> {
        #[derive(Deserialize)]
        struct Snapshot {
            games: BTreeMap<String, GamePackage>,
        }
        let s: Snapshot = serde_json::from_str(json).map_err(|e| Error::Snapshot(e.to_string()))?;
        Ok(s.games)
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
    fn multidata_wins_over_the_snapshot() {
        // A custom apworld the snapshot describes differently (or staler).
        let snapshot = BTreeMap::from([("G".into(), pkg(&[("Old", 1)], &[]))]);
        let embedded = BTreeMap::from([("G".into(), pkg(&[("New", 2)], &[]))]);
        let (dp, report) = DataPackage::merge(&snapshot, &embedded, &needed(&["G"]));

        assert_eq!(dp.get("G").unwrap().item_id("New"), Some(2));
        assert_eq!(dp.get("G").unwrap().item_id("Old"), None);
        assert_eq!(report.from_multidata, ["G"]);
        assert!(report.from_snapshot.is_empty());
    }

    #[test]
    fn hint_blacklist_is_grafted_from_the_snapshot_onto_embedded_data() {
        // The blacklist exists nowhere in multidata, so an embedded package
        // must still pick it up or `!hint` silently stops refusing names.
        let snapshot = BTreeMap::from([("G".into(), pkg(&[("Old", 1)], &["Secret"]))]);
        let embedded = BTreeMap::from([("G".into(), pkg(&[("New", 2)], &[]))]);
        let (dp, report) = DataPackage::merge(&snapshot, &embedded, &needed(&["G"]));

        let g = dp.get("G").unwrap();
        assert!(!g.is_hintable("Secret"));
        assert!(g.is_hintable("New"));
        assert!(report.missing_hint_blacklist.is_empty());
    }

    #[test]
    fn reports_when_no_hint_blacklist_is_available() {
        // Custom apworld, no snapshot entry: usable, but the blacklist is gone
        // and an operator should be told rather than left guessing.
        let (dp, report) = DataPackage::merge(
            &BTreeMap::new(),
            &BTreeMap::from([("Custom".into(), pkg(&[("A", 1)], &[]))]),
            &needed(&["Custom"]),
        );
        assert_eq!(dp.get("Custom").unwrap().item_id("A"), Some(1));
        assert_eq!(report.missing_hint_blacklist, ["Custom"]);
    }

    #[test]
    fn webhost_stripped_stubs_fall_back_to_the_snapshot() {
        // Upload replaces the package with {version, checksum}; treating that
        // as authoritative would erase every name in the game.
        let stub = GamePackage {
            checksum: Some("abc".into()),
            ..Default::default()
        };
        let snapshot = BTreeMap::from([("G".into(), pkg(&[("Real", 7)], &[]))]);
        let embedded = BTreeMap::from([("G".into(), stub)]);
        let (dp, report) = DataPackage::merge(&snapshot, &embedded, &needed(&["G"]));

        assert_eq!(dp.get("G").unwrap().item_id("Real"), Some(7));
        assert_eq!(report.from_snapshot, ["G"]);
    }

    #[test]
    fn unresolved_games_still_load_with_unknown_name_fallbacks() {
        // A missing package must degrade, not fail: the room still hosts.
        let (dp, report) =
            DataPackage::merge(&BTreeMap::new(), &BTreeMap::new(), &needed(&["Mystery"]));
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
    fn drops_snapshot_games_the_seed_does_not_use() {
        let snapshot = BTreeMap::from([
            ("Used".into(), pkg(&[("A", 1)], &[])),
            ("Unused".into(), pkg(&[("B", 2)], &[])),
        ]);
        let (dp, _) = DataPackage::merge(&snapshot, &BTreeMap::new(), &needed(&["Used"]));
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
        let (dp, _) = DataPackage::merge(&BTreeMap::new(), &embedded, &needed(&["W", "N"]));

        let sums = dp.checksums();
        assert_eq!(sums.get("W"), Some(&"aaa"));
        assert!(!sums.contains_key("N"));
    }

    #[test]
    fn loads_a_snapshot_from_json() {
        let json = r#"{"games":{"G":{
            "item_name_to_id":{"Sword":1},
            "location_name_to_id":{"Chest":10},
            "checksum":"deadbeef",
            "hint_blacklist":["Sword"]
        }}}"#;
        let snap = DataPackage::load_snapshot(json).unwrap();
        let g = &snap["G"];
        assert_eq!(g.item_name_to_id["Sword"], 1);
        assert_eq!(g.checksum.as_deref(), Some("deadbeef"));
        assert!(g.hint_blacklist.contains("Sword"));
    }

    #[test]
    fn resolves_names_in_both_directions() {
        let (dp, _) = DataPackage::merge(
            &BTreeMap::from([("G".into(), pkg(&[("Sword", 5)], &[]))]),
            &BTreeMap::new(),
            &needed(&["G"]),
        );
        let g = dp.get("G").unwrap();
        assert_eq!(g.item_id("Sword"), Some(5));
        assert_eq!(g.item_name(5), "Sword");
    }
}
