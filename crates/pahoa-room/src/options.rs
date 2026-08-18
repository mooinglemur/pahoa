//! Runtime-adjustable room settings.
//!
//! Defaults follow the standalone server (`settings.py:525-632`); WebHost
//! overrides several of these at construction (`customserver.py:74-76`), and
//! `/option` may change any of them while the room is live.

use pahoa_proto::Permission;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct RoomOptions {
    /// Required by every client on `Connect`. `None` means no password.
    ///
    /// Mutually exclusive with [`slot_passwords`](Self::slot_passwords): a room
    /// has one password, a password per slot, or none at all.
    pub password: Option<String>,
    /// Per-slot passwords, keyed by slot number. A slot absent from the map has
    /// none, and an empty map means the mode is unused.
    ///
    /// Checked *after* the slot name is resolved, unlike [`password`](Self::password),
    /// which can be checked before anything about the client is known.
    pub slot_passwords: BTreeMap<u32, String>,
    /// Enables `!admin login`. `None` disables remote administration entirely.
    pub server_password: Option<String>,
    /// Hint price as a *percentage* of a slot's total locations, not an
    /// absolute cost. Zero makes hints free.
    pub hint_cost: u32,
    pub location_check_points: u32,
    pub release_mode: Permission,
    pub collect_mode: Permission,
    pub remaining_mode: Permission,
    pub countdown_mode: Permission,
    pub item_cheat: bool,
    /// 0 = exact version match required, 1 = strict, 2 = permissive.
    pub compatibility: u8,
    /// Server tags advertised in `RoomInfo`.
    pub tags: Vec<String>,
}

impl Default for RoomOptions {
    fn default() -> Self {
        Self {
            password: None,
            slot_passwords: BTreeMap::new(),
            server_password: None,
            hint_cost: 10,
            location_check_points: 1,
            release_mode: Permission::Auto,
            collect_mode: Permission::Auto,
            remaining_mode: Permission::Goal,
            countdown_mode: Permission::Enabled,
            item_cheat: true,
            compatibility: 2,
            tags: vec!["AP".to_string()],
        }
    }
}

impl RoomOptions {
    /// The absolute point cost of one hint for a slot with `total_locations`.
    ///
    /// `max(1, int(hint_cost * 0.01 * total))`, or 0 when hints are free
    /// (`MultiServer.py:729-732`). The `max(1, …)` matters: without it a small
    /// world would round down to a free hint.
    pub fn hint_cost_for(&self, total_locations: usize) -> i64 {
        if self.hint_cost == 0 {
            return 0;
        }
        let raw = (self.hint_cost as f64 * 0.01 * total_locations as f64) as i64;
        raw.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_cost_is_a_percentage_with_a_floor_of_one() {
        let o = RoomOptions {
            hint_cost: 10,
            ..Default::default()
        };
        assert_eq!(o.hint_cost_for(1000), 100);
        assert_eq!(o.hint_cost_for(50), 5);
        // Truncates toward zero, then floors at 1 so hints are never free by
        // accident in a small world.
        assert_eq!(o.hint_cost_for(5), 1);
        assert_eq!(o.hint_cost_for(1), 1);
    }

    #[test]
    fn zero_hint_cost_means_free() {
        let o = RoomOptions {
            hint_cost: 0,
            ..Default::default()
        };
        assert_eq!(o.hint_cost_for(1000), 0);
        assert_eq!(o.hint_cost_for(1), 0);
    }
}
