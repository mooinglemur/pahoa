//! Per-connection state.
//!
//! A slot may have several connections at once — that is how co-op works — so
//! connections and slots are separate concepts throughout.

use pahoa_proto::{ItemsHandling, Version};

/// Opaque handle for one client connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

impl std::fmt::Display for ConnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn{}", self.0)
    }
}

/// Tags that mean "this client is not playing a game".
///
/// Any of them, combined with an absent `game`, skips game and version
/// validation and blocks location checks and goal completion
/// (`MultiServer.py:956`, `:1886-1892`, `:1917`).
pub const NON_GAME_TAGS: [&str; 3] = ["HintGame", "Tracker", "TextOnly"];

/// The join/leave verb each non-game tag produces (`MultiServer.py:956`).
pub fn non_game_verb(tags: &[String]) -> Option<&'static str> {
    for tag in tags {
        match tag.as_str() {
            "HintGame" => return Some("hinting"),
            "Tracker" => return Some("tracking"),
            "TextOnly" => return Some("viewing"),
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct Client {
    pub id: ConnId,
    /// Set once `Connect` succeeds. A re-`Connect` to the same slot leaves this
    /// true so the join message is not printed twice.
    pub auth: bool,
    pub team: u32,
    pub slot: u32,
    pub version: Version,
    pub tags: Vec<String>,
    pub items_handling: ItemsHandling,
    /// How many items this connection has already been sent, counting start
    /// inventory (`MultiServer.py:1934`).
    pub send_index: usize,
    /// Trackers and text clients may not check locations or claim the goal.
    pub no_locations: bool,
    /// Suppresses text broadcasts for bandwidth.
    pub no_text: bool,
}

impl Client {
    pub fn new(id: ConnId) -> Self {
        Self {
            id,
            auth: false,
            team: 0,
            slot: 0,
            // `no_version` in Python (`MultiServer.py:54`).
            version: Version::new(0, 0, 0),
            tags: Vec::new(),
            items_handling: ItemsHandling::new(0).expect("0 is always valid"),
            send_index: 0,
            no_locations: false,
            no_text: false,
        }
    }

    /// Recompute the tag-derived flags.
    ///
    /// The `PopTracker` clause is a real compatibility hack: clients older than
    /// 0.5.1 predate the `NoText` tag, so the server infers it for them to save
    /// traffic (`MultiServer.py:1919`).
    pub fn apply_tags(&mut self, tags: Vec<String>) {
        self.no_locations = tags.iter().any(|t| NON_GAME_TAGS.contains(&t.as_str()));
        self.no_text = tags.iter().any(|t| t == "NoText")
            || (tags.iter().any(|t| t == "PopTracker") && self.version < Version::new(0, 5, 1));
        self.tags = tags;
    }

    /// Whether game and version checks are skipped for this connection.
    ///
    /// Only when the client both omits `game` *and* carries a non-game tag
    /// (`MultiServer.py:1886`).
    pub fn ignores_game(game: &Option<String>, tags: &[String]) -> bool {
        let no_game = game.as_deref().is_none_or(str::is_empty);
        no_game && tags.iter().any(|t| NON_GAME_TAGS.contains(&t.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn non_game_tags_block_locations_and_goal() {
        for tag in NON_GAME_TAGS {
            let mut c = Client::new(ConnId(1));
            c.apply_tags(tags(&[tag]));
            assert!(c.no_locations, "{tag} should block location checks");
        }

        let mut playing = Client::new(ConnId(1));
        playing.apply_tags(tags(&["AP", "DeathLink"]));
        assert!(!playing.no_locations);
    }

    #[test]
    fn no_text_is_explicit_or_inferred_for_old_poptracker() {
        let mut explicit = Client::new(ConnId(1));
        explicit.apply_tags(tags(&["NoText"]));
        assert!(explicit.no_text);

        // PopTracker older than 0.5.1 predates the tag, so it is inferred.
        let mut old = Client::new(ConnId(2));
        old.version = Version::new(0, 5, 0);
        old.apply_tags(tags(&["PopTracker"]));
        assert!(old.no_text, "old PopTracker should be treated as NoText");

        let mut new = Client::new(ConnId(3));
        new.version = Version::new(0, 5, 1);
        new.apply_tags(tags(&["PopTracker"]));
        assert!(!new.no_text, "0.5.1 and later send the tag themselves");
    }

    #[test]
    fn game_is_ignored_only_when_absent_and_tagged() {
        assert!(Client::ignores_game(&None, &tags(&["Tracker"])));
        assert!(Client::ignores_game(
            &Some(String::new()),
            &tags(&["TextOnly"])
        ));

        // A named game is validated even for a tracker.
        assert!(!Client::ignores_game(
            &Some("Timespinner".into()),
            &tags(&["Tracker"])
        ));
        // No tag means the game is required.
        assert!(!Client::ignores_game(&None, &tags(&["AP"])));
    }

    #[test]
    fn join_verbs_match_the_reference() {
        assert_eq!(non_game_verb(&tags(&["Tracker"])), Some("tracking"));
        assert_eq!(non_game_verb(&tags(&["TextOnly"])), Some("viewing"));
        assert_eq!(non_game_verb(&tags(&["HintGame"])), Some("hinting"));
        assert_eq!(non_game_verb(&tags(&["AP"])), None);
    }
}
