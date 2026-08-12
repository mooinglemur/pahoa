//! The location table: which item sits at which location, for every slot.
//!
//! This is by far the largest structure in a multidata — 17,630 entries in a
//! 75-slot seed, and the plan sizes for ~400k at 2000 slots — and it is read on
//! every location check. The layout copies what Archipelago converged on in
//! Cython (`_speedups.pyx`): one flat array sorted by `(sender, location)` plus
//! a per-sender index, giving cache-friendly scans and O(log n) lookups with no
//! per-slot allocation. Unlike the Cython version this needs no `unsafe`.

use crate::error::{Error, Path, Result};
use crate::extract::Extract;
use pahoa_pickle::PyObj;

/// 32 bytes, 8-byte aligned. At 400k entries that is ~12.8 MB contiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct LocationEntry {
    pub location: i64,
    pub item: i64,
    /// The slot whose world contains this location.
    pub sender: u32,
    /// The slot that receives the item.
    pub receiver: u32,
    pub flags: u32,
    _pad: u32,
}

impl LocationEntry {
    pub fn new(sender: u32, location: i64, item: i64, receiver: u32, flags: u32) -> Self {
        Self {
            location,
            item,
            sender,
            receiver,
            flags,
            _pad: 0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LocationStore {
    entries: Box<[LocationEntry]>,
    /// `(start, count)` into `entries`, indexed by slot id. Index 0 is unused:
    /// slot 0 is the server itself and owns no locations.
    index: Box<[(u32, u32)]>,
}

impl LocationStore {
    /// Build from `{slot: {location: (item, receiver, flags)}}`.
    pub fn from_py(v: &PyObj, path: &Path) -> Result<Self> {
        let per_slot = v.dict_(path)?;

        let total: usize = per_slot
            .iter()
            .filter_map(|(_, m)| m.as_dict())
            .map(<[_]>::len)
            .sum();
        let mut entries = Vec::with_capacity(total);
        let mut max_slot = 0u32;

        for (slot_key, locs) in per_slot {
            let slot_path = path.index(format_args!("{slot_key:?}"));
            let sender = slot_key.u32_(&slot_path)?;
            if sender == 0 {
                return Err(Error::Locations(
                    "slot 0 is the server and cannot own locations".into(),
                ));
            }
            max_slot = max_slot.max(sender);

            for (loc_key, triple) in locs.dict_(&slot_path)? {
                let loc_path = slot_path.index(format_args!("{loc_key:?}"));
                let location = loc_key.int(&loc_path)?;
                let t = triple.tuple_n(&loc_path, 3)?;
                entries.push(LocationEntry::new(
                    sender,
                    location,
                    t[0].int(&loc_path.index(0))?,
                    t[1].u32_(&loc_path.index(1))?,
                    t[2].u32_(&loc_path.index(2))?,
                ));
            }
        }

        Ok(Self::from_entries(entries, max_slot))
    }

    /// Sorts and indexes a set of entries. Public so tests and the future
    /// fixture generator can build a store without going through pickle.
    pub fn from_entries(mut entries: Vec<LocationEntry>, max_slot: u32) -> Self {
        entries.sort_unstable_by_key(|e| (e.sender, e.location));

        let mut index = vec![(0u32, 0u32); max_slot as usize + 1];
        let mut i = 0usize;
        while i < entries.len() {
            let sender = entries[i].sender;
            let start = i;
            while i < entries.len() && entries[i].sender == sender {
                i += 1;
            }
            index[sender as usize] = (start as u32, (i - start) as u32);
        }

        Self {
            entries: entries.into_boxed_slice(),
            index: index.into_boxed_slice(),
        }
    }

    /// Every location belonging to `slot`, ascending by location id.
    pub fn for_slot(&self, slot: u32) -> &[LocationEntry] {
        match self.index.get(slot as usize) {
            Some(&(start, count)) => &self.entries[start as usize..(start + count) as usize],
            None => &[],
        }
    }

    /// The item at `location` in `slot`'s world.
    pub fn get(&self, slot: u32, location: i64) -> Option<&LocationEntry> {
        let slice = self.for_slot(slot);
        slice
            .binary_search_by_key(&location, |e| e.location)
            .ok()
            .map(|i| &slice[i])
    }

    pub fn contains(&self, slot: u32, location: i64) -> bool {
        self.get(slot, location).is_some()
    }

    pub fn count_for(&self, slot: u32) -> usize {
        self.for_slot(slot).len()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Highest slot id with locations. Slot ids are 1-based.
    pub fn max_slot(&self) -> u32 {
        self.index.len().saturating_sub(1) as u32
    }

    /// All entries, sorted by `(sender, location)`. Used by `!collect`, which
    /// scans for every item destined for a given slot.
    pub fn all(&self) -> &[LocationEntry] {
        &self.entries
    }

    /// The reference server validates the table on load and refuses to host an
    /// inconsistent one rather than failing mid-game (`NetUtils.py:449-506`).
    pub fn validate(&self) -> Result<()> {
        if self.entries.is_empty() {
            return Err(Error::Locations("no locations at all".into()));
        }
        // Slot ids must be contiguous from 1: a gap means the multidata and the
        // slot table disagree, and every downstream index would be off.
        for slot in 1..=self.max_slot() {
            if self.index[slot as usize].1 == 0 {
                return Err(Error::Locations(format!(
                    "slot {slot} has no locations, so slot ids are not contiguous"
                )));
            }
        }
        // Duplicate locations within a slot would make `get` ambiguous.
        for slot in 1..=self.max_slot() {
            let s = self.for_slot(slot);
            if let Some(w) = s.windows(2).find(|w| w[0].location == w[1].location) {
                return Err(Error::Locations(format!(
                    "slot {slot} lists location {} more than once",
                    w[0].location
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(spec: &[(u32, i64, i64, u32, u32)]) -> LocationStore {
        let max = spec.iter().map(|s| s.0).max().unwrap_or(0);
        let entries = spec
            .iter()
            .map(|&(s, l, i, r, f)| LocationEntry::new(s, l, i, r, f))
            .collect();
        LocationStore::from_entries(entries, max)
    }

    #[test]
    fn entry_is_thirty_two_bytes() {
        // The layout is the point; a silent change here would cost memory
        // bandwidth on the hottest structure in the server.
        assert_eq!(std::mem::size_of::<LocationEntry>(), 32);
        assert_eq!(std::mem::align_of::<LocationEntry>(), 8);
    }

    #[test]
    fn groups_and_sorts_by_slot_then_location() {
        let s = store(&[
            (2, 30, 1, 1, 0),
            (1, 20, 2, 2, 0),
            (1, 10, 3, 3, 0),
            (2, 5, 4, 4, 0),
        ]);
        assert_eq!(
            s.for_slot(1).iter().map(|e| e.location).collect::<Vec<_>>(),
            [10, 20]
        );
        assert_eq!(
            s.for_slot(2).iter().map(|e| e.location).collect::<Vec<_>>(),
            [5, 30]
        );
    }

    #[test]
    fn looks_up_by_slot_and_location() {
        let s = store(&[(1, 10, 111, 2, 0b001), (1, 20, 222, 3, 0b100)]);
        let e = s.get(1, 20).unwrap();
        assert_eq!((e.item, e.receiver, e.flags), (222, 3, 0b100));
        assert!(s.get(1, 15).is_none(), "unknown location");
        assert!(s.get(9, 10).is_none(), "unknown slot");
    }

    #[test]
    fn unknown_slots_yield_empty_rather_than_panicking() {
        // Clients can and do send location ids for slots that are not theirs;
        // the server drops them silently rather than erroring.
        let s = store(&[(1, 10, 1, 1, 0)]);
        assert!(s.for_slot(0).is_empty());
        assert!(s.for_slot(500).is_empty());
        assert_eq!(s.count_for(500), 0);
    }

    #[test]
    fn validate_rejects_slot_id_gaps() {
        // Slot 2 missing entirely: ids are not contiguous.
        let s = store(&[(1, 10, 1, 1, 0), (3, 10, 1, 1, 0)]);
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("slot 2"), "{err}");
    }

    #[test]
    fn validate_rejects_duplicate_locations() {
        let s = store(&[(1, 10, 1, 1, 0), (1, 10, 2, 2, 0)]);
        let err = s.validate().unwrap_err().to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn validate_accepts_a_well_formed_table() {
        let s = store(&[(1, 10, 1, 2, 0), (1, 11, 2, 2, 0), (2, 10, 3, 1, 0)]);
        s.validate().unwrap();
    }

    #[test]
    fn binary_search_finds_every_entry_in_a_large_slot() {
        let entries: Vec<_> = (0..10_000)
            .map(|i| LocationEntry::new(1, i * 7, i, 1, 0))
            .collect();
        let s = LocationStore::from_entries(entries, 1);
        for i in 0..10_000i64 {
            assert_eq!(s.get(1, i * 7).map(|e| e.item), Some(i));
            assert!(s.get(1, i * 7 + 1).is_none());
        }
    }
}
