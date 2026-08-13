//! The `GetDataPackage` reply, rendered once at startup.
//!
//! # Why this is cached rather than built per request
//!
//! Every other reply the room produces is bounded by something a client did —
//! a slot's location count, a 140-packet chunk, one datastorage value. This one
//! is bounded by the *seed*: it carries every item and location name for every
//! game in the multiworld, which is **1.1 MiB on a 35-game seed** and grows
//! with the game count rather than the slot count.
//!
//! Building it per request meant cloning every name table and serializing the
//! result on the actor — measured at **5.5 ms** for 35 games. The actor owns
//! `Room` and awaits only its mailbox, so that is 5.5 ms in which no other
//! client's packet is handled. Two things make that untenable rather than
//! merely wasteful:
//!
//! - `GetDataPackage` is one of the two packets accepted **before
//!   authentication** (`MultiServer.py:1963`), so anyone who can open a socket
//!   can ask, repeatedly. At 5.5 ms a single connection saturates the actor at
//!   about 180 requests a second — a remote stall of the whole room from one
//!   socket, needing no credentials and no unusual traffic.
//! - Every real client asks once on connecting whenever its cached checksums
//!   miss, so a 6000-client reconnect storm — the scenario the design exists to
//!   survive — would spend half a minute of pure actor time on it.
//!
//! Rendered once and shared, the common reply costs a refcount bump, and the
//! bytes are identical because they come from the same serializer.

use pahoa_multidata::DataPackage as NameTables;
use pahoa_proto::server::{DataPackageContents, GameData};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub struct DataPackageCache {
    /// Every game — what a client gets when it omits `games`, and what it asks
    /// for by name often enough to be worth answering without assembly.
    full: Arc<RawValue>,
    /// One rendered game object each, so a subset can be assembled by
    /// concatenation instead of by touching the name tables again.
    by_game: BTreeMap<String, Box<str>>,
}

impl DataPackageCache {
    pub fn build(names: &NameTables) -> Self {
        let mut by_game = BTreeMap::new();
        let mut all = BTreeMap::new();
        for (game, tables) in names.games() {
            let data = GameData {
                item_name_to_id: tables.package.item_name_to_id.clone(),
                location_name_to_id: tables.package.location_name_to_id.clone(),
                checksum: tables.package.checksum.clone(),
            };
            by_game.insert(
                game.clone(),
                serde_json::to_string(&data)
                    .expect("game data always serializes")
                    .into_boxed_str(),
            );
            all.insert(game.clone(), data);
        }
        Self {
            full: DataPackageContents { games: all }.render(),
            by_game,
        }
    }

    /// The reply for a set of game names.
    ///
    /// Names the room does not have are dropped rather than reported, which is
    /// the reference's behavior too — it filters its package by membership
    /// (`MultiServer.py:1944-1946`), so an unknown game is simply absent.
    pub fn select(&self, wanted: &[&str]) -> Arc<RawValue> {
        let chosen: BTreeSet<&str> = wanted
            .iter()
            .copied()
            .filter(|g| self.by_game.contains_key(*g))
            .collect();

        // Deduplicated first, so a request naming everything — or naming a game
        // twice — still recognizes itself as the whole package and shares it.
        if chosen.len() == self.by_game.len() {
            return Arc::clone(&self.full);
        }

        let size: usize = chosen
            .iter()
            .map(|g| self.by_game[*g].len() + g.len() + 4)
            .sum();
        let mut json = String::with_capacity(size + 16);
        json.push_str("{\"games\":{");
        for (i, game) in chosen.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            // Serialized rather than quoted by hand: game names come from the
            // seed and are not guaranteed to be free of characters that need
            // escaping.
            json.push_str(&serde_json::to_string(game).expect("a string always serializes"));
            json.push(':');
            json.push_str(&self.by_game[*game]);
        }
        json.push_str("}}");

        Arc::from(RawValue::from_string(json).expect("assembled from rendered fragments"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pahoa_multidata::GamePackage;

    fn package(item: (&str, i64), location: (&str, i64), checksum: Option<&str>) -> GamePackage {
        GamePackage {
            item_name_to_id: BTreeMap::from([(item.0.to_string(), item.1)]),
            location_name_to_id: BTreeMap::from([(location.0.to_string(), location.1)]),
            checksum: checksum.map(str::to_string),
            ..Default::default()
        }
    }

    fn tables() -> NameTables {
        let embedded = BTreeMap::from([
            (
                "Archipelago".to_string(),
                package(("Nothing", 0), ("Cheat Console", -1), Some("aaa")),
            ),
            (
                "Timespinner".to_string(),
                package(("Blade", 1), ("Chest", 10), Some("bbb")),
            ),
            (
                // A name needing escaping, since seeds decide these.
                "Quote\"Game".to_string(),
                package(("Item", 2), ("Place", 20), None),
            ),
        ]);
        let needed = embedded.keys().cloned().collect();
        NameTables::merge(&BTreeMap::new(), &embedded, &needed).0
    }

    /// The assembled path must produce exactly what the typed path would, or
    /// the byte-exact protocol vectors stop describing what goes on the wire.
    #[test]
    fn an_assembled_subset_matches_a_typed_render() {
        let names = tables();
        let cache = DataPackageCache::build(&names);

        for subset in [
            vec!["Timespinner"],
            vec!["Archipelago", "Timespinner"],
            vec!["Quote\"Game"],
            vec![],
        ] {
            let mut games = BTreeMap::new();
            for game in &subset {
                let tables = names.get(game).expect("present");
                games.insert(
                    (*game).to_string(),
                    GameData {
                        item_name_to_id: tables.package.item_name_to_id.clone(),
                        location_name_to_id: tables.package.location_name_to_id.clone(),
                        checksum: tables.package.checksum.clone(),
                    },
                );
            }
            let expected = DataPackageContents { games }.render();
            assert_eq!(
                cache.select(&subset).get(),
                expected.get(),
                "subset {subset:?}"
            );
        }
    }

    #[test]
    fn asking_for_everything_shares_the_cached_bytes() {
        let names = tables();
        let cache = DataPackageCache::build(&names);
        let all = ["Archipelago", "Timespinner", "Quote\"Game"];

        assert!(
            Arc::ptr_eq(&cache.select(&all), &cache.full),
            "naming every game should share the cached package, not rebuild it"
        );
        // Repeats must not fool the count into thinking a subset is everything.
        assert!(Arc::ptr_eq(
            &cache.select(&["Archipelago", "Archipelago", "Timespinner", "Quote\"Game"]),
            &cache.full
        ));
        assert!(!Arc::ptr_eq(
            &cache.select(&["Archipelago", "Archipelago"]),
            &cache.full
        ));
    }

    #[test]
    fn an_unknown_game_is_dropped_rather_than_erroring() {
        let names = tables();
        let cache = DataPackageCache::build(&names);
        let rendered = cache
            .select(&["Timespinner", "Not A Game"])
            .get()
            .to_string();
        assert!(rendered.contains("Timespinner"), "{rendered}");
        assert!(!rendered.contains("Not A Game"), "{rendered}");
    }
}
