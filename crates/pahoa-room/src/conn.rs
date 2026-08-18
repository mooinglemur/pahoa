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
    // Priority follows the reference *dict's* order rather than the client's
    // tag order: it iterates `_non_game_messages` and breaks on the first tag
    // the client carries, so `["Tracker", "HintGame"]` is "hinting".
    for (tag, verb) in [
        ("HintGame", "hinting"),
        ("Tracker", "tracking"),
        ("TextOnly", "viewing"),
    ] {
        if tags.iter().any(|t| t == tag) {
            return Some(verb);
        }
    }
    None
}

/// Render tags the way Python renders a list of strings.
///
/// The join and leave announcements interpolate `client.tags` directly
/// (`MultiServer.py:975`, `:1005`), so the wire text carries a Python list
/// repr — `['Tracker', 'DeathLink']`, single-quoted, comma-space separated.
/// Clients display this verbatim, which makes it observable formatting rather
/// than an internal detail.
pub fn python_list_repr(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&python_str_repr(item));
    }
    out.push(']');
    out
}

/// `repr()` of one string: single quotes, unless the value contains a single
/// quote and no double quote, which is when Python switches.
fn python_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            // Python escapes the C0 range and DEL as \xNN; anything printable
            // above ASCII it leaves alone.
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// How much of the room's feed a connection wants.
///
/// See `docs/scoped-feed.md`. The distinction that matters: `NoText` is an
/// *audience* filter — it decides who a message goes to — while this is a
/// *content* filter, deciding which subset of a feed one connection receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeedPolicy {
    /// Everything, as every client has always received it.
    #[default]
    Full,
    /// Only what concerns this connection's own slot: its own item traffic,
    /// its own hints, its own joins and parts. Chat, countdowns and the
    /// room-wide milestones still arrive in full.
    Scoped,
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
    /// How much of the feed this connection receives.
    ///
    /// **Sticky, and deliberately not derived from `tags`.** `ConnectUpdate`
    /// calls [`Client::apply_tags`], which *replaces* the tag vector — and
    /// trackers send `ConnectUpdate` routinely, to add `DeathLink` for
    /// instance. A policy living in the tags would therefore be wiped
    /// mid-session and the connection would silently fall back to the full
    /// firehose, with no error anywhere. So the listener sets this once, at
    /// accept time, and nothing the client sends can lower it.
    pub feed: FeedPolicy,
}

impl Client {
    pub fn new(id: ConnId) -> Self {
        Self::with_feed(id, FeedPolicy::Full)
    }

    /// A connection whose feed policy comes from the port it arrived on.
    pub fn with_feed(id: ConnId, feed: FeedPolicy) -> Self {
        Self {
            feed,
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

    /// Whether this connection receives only what concerns its own slot.
    pub fn scoped(&self) -> bool {
        self.feed == FeedPolicy::Scoped
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

    /// The reference scans its own table and breaks on the first hit, so the
    /// client's tag order does not decide the verb.
    #[test]
    fn verb_priority_is_the_servers_not_the_clients() {
        assert_eq!(
            non_game_verb(&tags(&["Tracker", "HintGame"])),
            Some("hinting")
        );
        assert_eq!(
            non_game_verb(&tags(&["TextOnly", "Tracker"])),
            Some("tracking")
        );
    }

    #[test]
    fn tags_render_as_a_python_list() {
        assert_eq!(python_list_repr(&[]), "[]");
        assert_eq!(python_list_repr(&tags(&["AP"])), "['AP']");
        assert_eq!(
            python_list_repr(&tags(&["Tracker", "Axolotl", "DeathLink"])),
            "['Tracker', 'Axolotl', 'DeathLink']"
        );
    }

    /// Tags come from the client, so the repr has to survive hostile ones the
    /// same way Python's does rather than producing something unquotable.
    #[test]
    fn quoting_follows_pythons_repr_rules() {
        // A single quote inside flips the outer quote to double, as Python's.
        assert_eq!(python_list_repr(&tags(&["it's"])), "[\"it's\"]");
        // Unless a double quote is present too, and then it escapes instead.
        assert_eq!(python_list_repr(&tags(&["it's \"x\""])), r#"['it\'s "x"']"#);
        assert_eq!(python_list_repr(&tags(&["a\\b"])), r"['a\\b']");
        assert_eq!(python_list_repr(&tags(&["a\nb"])), r"['a\nb']");
        assert_eq!(python_list_repr(&tags(&["a\u{7}b"])), r"['a\x07b']");
        // Printable non-ASCII passes through, as it does in Python 3.
        assert_eq!(python_list_repr(&tags(&["héllo"])), "['héllo']");
    }
}
