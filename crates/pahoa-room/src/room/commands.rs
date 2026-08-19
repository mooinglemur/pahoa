//! The `!` command processor.
//!
//! Ports `MultiServer.CommandProcessor` and its `ClientMessageProcessor`
//! subclass (`MultiServer.py:1325-1836`). Two things about the shape are worth
//! knowing before reading the handlers:
//!
//! - **Every `Say` is chat first and a command second.** The reference
//!   broadcasts the raw line to the room *before* looking at whether it starts
//!   with `!`, so `!hint Lamp` appears in chat exactly as typed. The single
//!   exception is `!admin`, which is suppressed so a password never reaches the
//!   room (`MultiServer.py:1420-1424`).
//! - **Arguments are parsed two different ways.** Most commands get
//!   shell-style word splitting; the ones marked `@mark_raw` in the reference
//!   get the entire remainder of the line, unsplit and unquoted, because item
//!   and location names contain spaces, quotes and apostrophes.
//!
//! Command *output* goes only to the caller, as `PrintJSON` with type
//! `CommandResult`, and is dropped entirely for `NoText` clients
//! (`MultiServer.py:456-460`).

use super::*;
use crate::fuzzy;

/// Lines one command may print before it gives up and asks for a filter.
///
/// A deliberate divergence: `!missing` on a fresh 2000-location slot would
/// otherwise emit 2000 `PrintJSON` packets, and the reference has no cap at
/// all. Bounded output is worth a truncation notice at this scale.
const MAX_LIST_LINES: usize = 500;

/// Name, argument spec, and help text — the listing `!help` prints.
///
/// The reference generates this by reflecting over each handler's signature
/// (`MultiServer.py:1359-1381`); reproducing the format by hand keeps the
/// output familiar without dragging a macro in for one string.
const HELP: &[(&str, &str, &str)] = &[
    ("help", "", "Returns the help listing"),
    ("license", "", "Returns the licensing information"),
    (
        "options",
        "",
        "List all current options. Warning: lists password.",
    ),
    (
        "admin",
        "[command] ",
        "Allow remote administration of the multiworld server\n    \
         Usage: \"!admin login <password>\" in order to log in to the remote interface.",
    ),
    (
        "players",
        "",
        "Get information about connected and missing players.",
    ),
    (
        "status",
        "[tag] ",
        "Get status information about your team.\n    \
         Optionally mention a Tag name and get information on who has that Tag.\n    \
         For example: DeathLink or EnergyLink.",
    ),
    (
        "release",
        "",
        "Sends remaining items in your world to their recipients.",
    ),
    ("collect", "", "Send your remaining items to yourself"),
    ("countdown", "seconds=10 ", "Start a countdown in seconds"),
    (
        "remaining",
        "",
        "List remaining items in your game, but not their location or recipient",
    ),
    (
        "missing",
        "[filter_text] ",
        "List all missing location checks from the server's perspective.\n    \
         Can be given text, which will be used as filter.",
    ),
    (
        "checked",
        "[filter_text] ",
        "List all done location checks from the server's perspective.\n    \
         Can be given text, which will be used as filter.",
    ),
    (
        "alias",
        "[alias_name] ",
        "Set your alias to the passed name.",
    ),
    (
        "getitem",
        "item_name ",
        "Cheat in an item, if it is enabled on this server",
    ),
    (
        "hint",
        "[item_name] ",
        "Use !hint {item_name},\n    \
         for example !hint Lamp to get a spoiler peek for that item.\n    \
         If hint costs are on, this will only give you one new result,\n    \
         you can rerun the command to get more in that case.",
    ),
    (
        "hint_location",
        "[location] ",
        "Use !hint_location {location_name},\n    \
         for example !hint_location atomic-bomb to get a spoiler peek for that location.",
    ),
];

/// Commands that take the raw remainder of the line as their one argument.
///
/// `@mark_raw` in the reference. Everything else has its arguments split into
/// words (`MultiServer.py:1346-1353`).
fn takes_raw_argument(name: &str) -> bool {
    matches!(
        name,
        "admin" | "missing" | "checked" | "alias" | "getitem" | "hint" | "hint_location"
    )
}

impl Room {
    // --- entry point -----------------------------------------------------

    pub(super) fn handle_say(&mut self, conn: ConnId, args: cmd::Say, out: &mut dyn EffectSink) {
        if !is_printable(&args.text) {
            self.bad_arguments(conn, "Say", "Say".into(), out);
            return;
        }
        self.process_message(conn, &args.text, out);
    }

    /// `ClientMessageProcessor.__call__`: broadcast, then dispatch.
    fn process_message(&mut self, conn: ConnId, raw: &str, out: &mut dyn EffectSink) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let key = (client.team, client.slot);

        // `!admin` is echoed by its own handler, with the password masked.
        if !raw.starts_with("!admin") {
            self.broadcast_chat(key, raw, raw, out);
        }

        if raw.is_empty() {
            return;
        }
        // Only the *first* word decides whether this is a command, and it is
        // taken from the shell-split form even for raw commands.
        let Some(first) = shell_split(raw).into_iter().next() else {
            return;
        };
        let Some(name) = first.strip_prefix('!') else {
            // Plain chat. The reference's `default` for a client is a no-op:
            // the line was already broadcast above.
            return;
        };
        let name = name.to_lowercase();

        if takes_raw_argument(&name) {
            // Everything after the first whitespace run, verbatim.
            let argument = raw.split_once(char::is_whitespace).map(|(_, rest)| rest);
            self.run_raw_command(conn, &name, argument.unwrap_or(""), out);
        } else {
            let words: Vec<String> = shell_split(raw).into_iter().skip(1).collect();
            self.run_word_command(conn, &name, &words, out);
        }
    }

    fn run_word_command(
        &mut self,
        conn: ConnId,
        name: &str,
        args: &[String],
        out: &mut dyn EffectSink,
    ) {
        let first = args.first().map(String::as_str).unwrap_or("");
        match name {
            "help" => self.cmd_help(conn, out),
            "license" => self.cmd_license(conn, out),
            "options" => self.cmd_options(conn, out),
            "players" => self.cmd_players(conn, out),
            "status" => self.cmd_status(conn, first, out),
            "release" => self.cmd_release(conn, out),
            "collect" => self.cmd_collect(conn, out),
            "countdown" => self.cmd_countdown(conn, first, out),
            "remaining" => self.cmd_remaining(conn, out),
            _ => self.unknown_command(conn, name, out),
        }
    }

    fn run_raw_command(
        &mut self,
        conn: ConnId,
        name: &str,
        argument: &str,
        out: &mut dyn EffectSink,
    ) {
        match name {
            "admin" => self.cmd_admin(conn, argument, out),
            "missing" => self.cmd_location_list(conn, argument, true, out),
            "checked" => self.cmd_location_list(conn, argument, false, out),
            "alias" => self.cmd_alias(conn, argument, out),
            "getitem" => self.cmd_getitem(conn, argument, out),
            "hint" => self.cmd_hint(conn, argument, false, out),
            "hint_location" => self.cmd_hint(conn, argument, true, out),
            _ => self.unknown_command(conn, name, out),
        }
    }

    fn unknown_command(&self, conn: ConnId, name: &str, out: &mut dyn EffectSink) {
        let known: Vec<&str> = HELP.iter().map(|(c, _, _)| *c).collect();
        self.notify(
            conn,
            format!(
                "Could not find command {name}. Known commands: {}",
                known.join(", ")
            ),
            out,
        );
    }

    // --- output ----------------------------------------------------------

    /// `notify_client`: one line, to the caller only, skipped for `NoText`.
    ///
    /// Logged, and [`Room::notify_multiple`] deliberately is not — which is the
    /// reference's split too (`MultiServer.py:460` against `:463-468`, where
    /// `notify_client_multiple` has no logging line). It reads as an oversight
    /// and is not one: `!missing` on a fresh slot answers with hundreds of
    /// lines, and an operator watching a room does not want each of them.
    pub(super) fn notify(&self, conn: ConnId, text: String, out: &mut dyn EffectSink) {
        if let Some(client) = self.clients.get(&conn)
            && client.auth
            && !client.no_text
        {
            tracing::info!(
                slot = client.slot,
                team = client.team,
                player = %self.slot_alias((client.team, client.slot)),
                %text,
                "notice"
            );
        }
        self.notify_multiple(conn, vec![text], out);
    }

    /// `notify_client_multiple`: several lines in one batch, so a long listing
    /// is one frame rather than hundreds.
    pub(super) fn notify_multiple(
        &self,
        conn: ConnId,
        texts: Vec<String>,
        out: &mut dyn EffectSink,
    ) {
        self.notify_typed(conn, texts, PrintJsonType::CommandResult, out);
    }

    /// As [`Room::notify_multiple`], for the `/` command set.
    ///
    /// Its replies carry `AdminCommandResult` rather than `CommandResult`,
    /// because the two processors have different `output` methods in the
    /// reference (`MultiServer.py:1432` against `:2227-2230`). Clients render
    /// them differently, so the distinction is visible rather than cosmetic —
    /// and `!admin`'s *own* replies, the login and usage lines, stay
    /// `CommandResult`: they come from the client-side processor.
    ///
    /// **Not logged, and this is the one place pahoa is quieter than the
    /// reference on purpose.** `ServerCommandProcessor.output` goes through
    /// `notify_client`, so the reference logs every `/` reply. One of those
    /// replies is `/options`, which prints the real `server_password` — safe to
    /// show the administrator who just typed it, and not safe to write into a
    /// log that gets shipped and indexed. The administrative *action* is
    /// already on the record either way: `cmd_admin` masks and broadcasts the
    /// command before running it, and that broadcast is logged as chat.
    pub(super) fn notify_admin(&self, conn: ConnId, texts: Vec<String>, out: &mut dyn EffectSink) {
        self.notify_typed(conn, texts, PrintJsonType::AdminCommandResult, out);
    }

    fn notify_typed(
        &self,
        conn: ConnId,
        texts: Vec<String>,
        print_type: PrintJsonType,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        if !client.auth || client.no_text || texts.is_empty() {
            return;
        }
        let msgs: Vec<ServerPacket> = texts
            .into_iter()
            .map(|text| {
                ServerPacket::PrintJSON(PrintJson {
                    data: vec![JsonMessagePart::text(text)],
                    print_type: Some(print_type),
                    ..Default::default()
                })
            })
            .collect();
        out.send(conn, &msgs);
    }

    /// The chat line every `Say` produces, whether or not it is a command.
    fn broadcast_chat(&self, key: SlotKey, display: &str, message: &str, out: &mut dyn EffectSink) {
        let text = format!("{}: {display}", self.slot_alias(key));
        // Logged from `text`, which is built from `display`. That matters for
        // `!admin`: its caller masks the password *before* calling here and
        // passes the masked form, so what reaches the log is what reached the
        // room. Anything that later logged the pre-masked line would undo the
        // masking, in a place nobody would think to look for it.
        tracing::info!(slot = key.1, team = key.0, %text, "chat");
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::Chat),
                team: Some(key.0),
                slot: Some(key.1),
                message: Some(message.to_string()),
                ..Default::default()
            })],
        );
    }

    /// A room-wide `CommandResult`, for the commands that answer everybody.
    pub(crate) fn broadcast_result(&self, text: String, out: &mut dyn EffectSink) {
        tracing::info!(%text, "notice");
        out.broadcast(
            Recipients::AllText,
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(text)],
                print_type: Some(PrintJsonType::CommandResult),
                ..Default::default()
            })],
        );
    }

    // --- informational ---------------------------------------------------

    fn cmd_help(&self, conn: ConnId, out: &mut dyn EffectSink) {
        let mut s = String::new();
        for (command, args, doc) in HELP {
            s.push_str(&format!("!{command} {args}\n    {doc}\n"));
        }
        self.notify(conn, s, out);
    }

    fn cmd_license(&self, conn: ConnId, out: &mut dyn EffectSink) {
        self.notify(conn, LICENSE.to_string(), out);
    }

    /// The reference masks the server password with 4-16 random asterisks so
    /// its *length* does not leak either (`MultiServer.py:1410-1417`). The
    /// count comes from the room's PRNG rather than a fresh one, which means
    /// it is reproducible for a seed — the point is only that it is not the
    /// real length.
    fn cmd_options(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let masked = {
            let n = 4 + self.rng.randbelow(13) as usize;
            "*".repeat(n)
        };
        let o = &self.options;
        let texts = vec![
            "Current options:".to_string(),
            format!("Option hint_cost is set to {}", o.hint_cost),
            format!(
                "Option location_check_points is set to {}",
                o.location_check_points
            ),
            format!("Option server_password is set to {masked}"),
            format!("Option password is set to {}", opt_str(&o.password)),
            format!("Option release_mode is set to {}", o.release_mode.as_text()),
            format!(
                "Option remaining_mode is set to {}",
                o.remaining_mode.as_text()
            ),
            format!("Option collect_mode is set to {}", o.collect_mode.as_text()),
            format!(
                "Option countdown_mode is set to {}",
                o.countdown_mode.as_text()
            ),
            format!("Option item_cheat is set to {}", py_bool(o.item_cheat)),
            format!("Option compatibility is set to {}", o.compatibility),
        ];
        self.notify_multiple(conn, texts, out);
    }

    /// `get_players_string` (`MultiServer.py:1855-1873`).
    ///
    /// Broadcast to the room when the seed is small enough for that to be
    /// polite, private otherwise — the reference's own concession to scale.
    fn cmd_players(&self, conn: ConnId, out: &mut dyn EffectSink) {
        let text = self.players_string();
        if self.data.slot_info.len() < 10 {
            self.broadcast_result(text, out);
        } else {
            self.notify(conn, text, out);
        }
    }

    fn players_string(&self) -> String {
        let connected: HashSet<SlotKey> = self
            .clients
            .values()
            .filter(|c| c.auth)
            .map(|c| (c.team, c.slot))
            .collect();

        let mut text = String::new();
        let mut total = 0;
        let mut current_team: i64 = -1;
        // `slot_info` is a BTreeMap, so this is the sorted (team, slot) walk
        // the reference does over its own key list.
        for (slot, info) in &self.data.slot_info {
            if info.slot_type != SlotType::Player {
                continue;
            }
            total += 1;
            let key = (0, *slot);
            if current_team != key.0 as i64 {
                text.push_str(&format!(":: Team #{}: ", key.0 + 1));
                current_team = key.0 as i64;
            }
            // Aliases are deliberately absent here: this is a roll call of seed
            // names, which is what a player needs to type into `!hint`.
            if connected.contains(&key) {
                text.push_str(&format!("{} ", self.slot_name(key)));
            } else {
                text.push_str(&format!("({}) ", self.slot_name(key)));
            }
        }
        // Python drops the trailing space with `text[:-1]`, which on an empty
        // string is still empty.
        let trimmed = text.strip_suffix(' ').unwrap_or(&text);
        format!("{} players of {total} connected {trimmed}", connected.len())
    }

    /// `get_status_string` (`MultiServer.py:1876-1891`).
    fn cmd_status(&self, conn: ConnId, tag: &str, out: &mut dyn EffectSink) {
        let team = self.clients.get(&conn).map(|c| c.team).unwrap_or(0);
        let mut text = format!("Player Status on team {team}:");
        for slot in self.data.slot_info.keys() {
            let key = (team, *slot);
            let conns: Vec<&Client> = self
                .by_slot
                .get(&key)
                .into_iter()
                .flatten()
                .filter_map(|c| self.clients.get(c))
                .collect();
            let connected = conns.len();
            let tagged = conns
                .iter()
                .filter(|c| c.tags.iter().any(|t| t == tag))
                .count();

            let tag_text = if connected > 0 && !tag.is_empty() {
                format!(" {tagged} of which are tagged {tag}")
            } else {
                String::new()
            };
            let status_text = match self.status(key) {
                ClientStatus::Goal => " and has finished.",
                ClientStatus::Ready => " and is ready.",
                _ => ".",
            };
            text.push_str(&format!(
                "\n{} has {connected} connection{}{tag_text}{status_text} ({}/{})",
                self.slot_alias(key),
                if connected == 1 { "" } else { "s" },
                self.checked_count(key),
                self.data.locations.count_for(*slot),
            ));
        }
        self.notify(conn, text, out);
    }

    /// `!missing` and `!checked`, which differ only in which list they walk.
    fn cmd_location_list(
        &self,
        conn: ConnId,
        filter: &str,
        missing: bool,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let key = (client.team, client.slot);
        let (label, noun) = if missing {
            ("Missing", "missing")
        } else {
            ("Checked", "done")
        };

        let locations = if missing {
            self.missing_locations(key)
        } else {
            self.checked_locations(key)
        };
        if locations.is_empty() {
            self.notify(conn, format!("No {noun} location checks found."), out);
            return;
        }

        let game = self.slot_game(key.1);
        let names = self.datapackage.get(&game);
        let mut display: Vec<String> = locations
            .iter()
            .map(|id| {
                names
                    .map(|n| n.location_name(*id))
                    .unwrap_or_else(|| format!("Unknown location (ID:{id})"))
            })
            .collect();

        if !filter.is_empty() {
            let groups = names.map(|n| &n.package.location_name_groups);
            match groups.and_then(|g| g.get(filter)) {
                // An exact group name selects that group's members...
                Some(members) => display.retain(|name| members.contains(name)),
                // ...anything else is a substring match.
                None => display.retain(|name| name.contains(filter)),
            }
        }

        let shown = display.len();
        let truncated = shown > MAX_LIST_LINES;
        display.truncate(MAX_LIST_LINES);

        let mut texts: Vec<String> = display
            .into_iter()
            .map(|name| format!("{label}: {name}"))
            .collect();
        if filter.is_empty() {
            texts.push(format!("Found {} {noun} location checks", locations.len()));
        } else {
            texts.push(format!(
                "Found {} {noun} location checks, displaying {shown} of them.",
                locations.len()
            ));
        }
        if truncated {
            texts.push(format!(
                "Output was capped at {MAX_LIST_LINES} lines; narrow it with \
                 !{} <filter text or location group>.",
                if missing { "missing" } else { "checked" }
            ));
        }
        self.notify_multiple(conn, texts, out);
    }

    // --- release, collect, countdown, remaining ---------------------------
    //
    // These four are gated by a `Permission`, and the reference tests those
    // modes two different ways. `!release` and `!collect` use a **substring**
    // check — `"enabled" in release_mode` — which is also true for
    // `auto-enabled`; `!remaining` and `!countdown` use **equality**, so
    // `auto-enabled` matches neither `enabled` nor `disabled` and falls through
    // to the goal-gated branch. The bits in `Permission` capture the substring
    // form, so the equality cases match on the variant instead. It looks like an
    // inconsistency in the reference because it is one, and copying it is the
    // point.

    fn cmd_release(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let Some(key) = self.slot_of(conn) else {
            return;
        };
        // An administrator's one-off grant beats the mode entirely.
        if self.release_allowed(key) || self.options.release_mode.allows_manual() {
            self.release_player(key, out);
            out.mark_dirty();
            return;
        }
        if self.options.release_mode == Permission::Disabled {
            self.notify(
                conn,
                "Sorry, client item releasing has been disabled on this server. \
                 You can ask the server admin for a /release"
                    .to_string(),
                out,
            );
            return;
        }
        // auto or goal: allowed once the player has finished.
        if self.status(key) == ClientStatus::Goal {
            self.release_player(key, out);
            out.mark_dirty();
        } else {
            self.notify(
                conn,
                "Sorry, client item releasing requires you to have beaten the game on this \
                 server. You can ask the server admin for a /release"
                    .to_string(),
                out,
            );
        }
    }

    fn cmd_collect(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let Some(key) = self.slot_of(conn) else {
            return;
        };
        if self.options.collect_mode.allows_manual() {
            self.collect_player(key, out);
            out.mark_dirty();
            return;
        }
        if self.options.collect_mode == Permission::Disabled {
            self.notify(
                conn,
                "Sorry, client collecting has been disabled on this server. You can ask the \
                 server admin for a /collect"
                    .to_string(),
                out,
            );
            return;
        }
        if self.status(key) == ClientStatus::Goal {
            self.collect_player(key, out);
            out.mark_dirty();
        } else {
            self.notify(
                conn,
                "Sorry, client collecting requires you to have beaten the game on this server. \
                 You can ask the server admin for a /collect"
                    .to_string(),
                out,
            );
        }
    }

    /// `!countdown`, which needs a clock the room does not own.
    ///
    /// The time comes from `start_time` plus however far the transport says the
    /// room has got; tests drive it directly.
    fn cmd_countdown(&mut self, conn: ConnId, seconds: &str, out: &mut dyn EffectSink) {
        let disabled = self.options.countdown_mode == Permission::Disabled
            // `auto` turns itself off once the room is too big for a countdown
            // to mean anything (`MultiServer.py:1552-1553`).
            || (self.options.countdown_mode == Permission::Auto
                && self.data.slot_info.len() >= 30);
        if disabled {
            self.notify(
                conn,
                "Sorry, client countdowns have been disabled on this server. You can ask the \
                 server admin for a /countdown"
                    .to_string(),
                out,
            );
            return;
        }

        // Unparseable falls back to ten; the reference does the same rather
        // than complaining (`MultiServer.py:1556-1558`).
        let timer: i64 = seconds.parse().unwrap_or(10);
        if timer > 60 * 60 {
            // The reference raises here, and its handler prints the resulting
            // Python traceback to the client. A plain refusal instead: a
            // traceback is an information leak and tells the player nothing.
            self.notify(conn, format!("{timer} is invalid. Maximum is 1 hour."), out);
            return;
        }

        let now = self.clock;
        self.start_countdown(timer, now, out);
    }

    fn cmd_remaining(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let Some(key) = self.slot_of(conn) else {
            return;
        };
        match self.options.remaining_mode {
            Permission::Enabled => self.report_remaining(conn, key, out),
            Permission::Disabled => self.notify(
                conn,
                "Sorry, !remaining has been disabled on this server.".to_string(),
                out,
            ),
            // goal, auto, and — because the reference compares strings —
            // auto-enabled too.
            _ => {
                if self.status(key) == ClientStatus::Goal {
                    self.report_remaining(conn, key, out);
                } else {
                    self.notify(
                        conn,
                        "Sorry, !remaining requires you to have beaten the game on this server"
                            .to_string(),
                        out,
                    );
                }
            }
        }
    }

    /// What is still sitting in this slot's world, by item name only — no
    /// location and no recipient, so it spoils the inventory and nothing else.
    fn report_remaining(&self, conn: ConnId, key: SlotKey, out: &mut dyn EffectSink) {
        let checked = self.location_checks.get(&key);
        // Sorted by `(receiving player, item id)`, matching
        // `_LocationStore.get_remaining` — the order is a mild spoiler in
        // itself, so it must not leak the location order.
        let mut rest: Vec<(u32, i64)> = self
            .data
            .locations
            .for_slot(key.1)
            .iter()
            .filter(|e| checked.is_none_or(|c| !c.contains(&e.location)))
            .map(|e| (e.receiver, e.item))
            .collect();
        rest.sort_unstable();

        if rest.is_empty() {
            self.notify(conn, "No remaining items found.".to_string(), out);
            return;
        }
        let names: Vec<String> = rest
            .iter()
            .map(|(receiver, item)| {
                let game = self.slot_game(*receiver);
                self.datapackage
                    .get(&game)
                    .map(|n| n.item_name(*item))
                    .unwrap_or_else(|| format!("Unknown item (ID:{item})"))
            })
            .collect();
        self.notify(conn, format!("Remaining items: {}", names.join(", ")), out);
    }

    fn slot_of(&self, conn: ConnId) -> Option<SlotKey> {
        self.clients.get(&conn).map(|c| (c.team, c.slot))
    }

    // --- alias -----------------------------------------------------------

    fn cmd_alias(&mut self, conn: ConnId, name: &str, out: &mut dyn EffectSink) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let key = (client.team, client.slot);

        // Python takes the first 16 *characters* and then strips, so a name
        // padded with spaces can end up shorter than 16.
        let trimmed: String = name.chars().take(16).collect();
        let trimmed = trimmed.trim().to_string();

        if !trimmed.is_empty() {
            self.name_aliases.insert(key, trimmed.clone());
            self.notify(conn, format!("Hello, {trimmed}"), out);
        } else if self.name_aliases.remove(&key).is_some() {
            self.notify(conn, "Removed Alias".to_string(), out);
        } else {
            // No alias to remove: the reference returns False and says nothing.
            return;
        }

        // Aliases are part of `NetworkPlayer`, so everyone needs the new list.
        out.broadcast(
            Recipients::All,
            &[ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                players: Some(self.players_package()),
                ..Default::default()
            }))],
        );
        out.mark_dirty();
    }

    // --- cheats ----------------------------------------------------------

    fn cmd_getitem(&mut self, conn: ConnId, item_name: &str, out: &mut dyn EffectSink) {
        if !self.options.item_cheat {
            self.notify(conn, "Cheating is disabled.".to_string(), out);
            return;
        }
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let key = (client.team, client.slot);

        match self.grant_item(key, item_name, out) {
            Ok(_) => out.mark_dirty(),
            // A fuzzy match that found nothing at all says nothing, matching the
            // reference; one that found something too different explains itself.
            Err(message) if message.is_empty() => {}
            Err(message) => self.notify(conn, message, out),
        }
    }

    /// Put an item into a slot's queue, as the cheat console does.
    ///
    /// Shared with the admin API, which aims it at a slot rather than at the
    /// caller. Returns the item's resolved name, or the message explaining why
    /// nothing happened — empty when the reference server would say nothing.
    pub(super) fn grant_item(
        &mut self,
        key: SlotKey,
        item_name: &str,
        out: &mut dyn EffectSink,
    ) -> Result<String, String> {
        let (team, slot) = key;
        let game = self.slot_game(slot);
        let Some(names) = self.datapackage.get(&game) else {
            return Err(format!("No item names are known for {game}."));
        };

        let candidates: Vec<&str> = names
            .package
            .item_name_to_id
            .keys()
            .map(String::as_str)
            .collect();
        let Some(matched) = fuzzy::intended(item_name, &candidates) else {
            return Err(String::new());
        };
        let name = match &matched {
            fuzzy::Match::Accepted { name, .. } => name.clone(),
            fuzzy::Match::Rejected { message, .. } => return Err(message.clone()),
        };
        let id = names.item_id(&name).expect("matched an existing name");

        // The cheat sentinel: location -1, sender is the receiving slot itself
        // (`MultiServer.py:1672`). Queued on *both* streams directly rather
        // than through the group-expanding path, which is what the reference
        // does — a cheated item is not an item link event.
        let item = NetworkItem {
            item: id,
            location: -1,
            player: slot,
            flags: 0,
        };
        for remote in [false, true] {
            Arc::make_mut(self.received_items.entry((team, slot, remote)).or_default()).push(item);
        }

        out.broadcast(
            // Attributable to the receiving slot, so a scoped feed hears about
            // items granted to it and not about everyone else's.
            Recipients::AllTextAbout((team, slot)),
            &[ServerPacket::PrintJSON(PrintJson {
                data: vec![JsonMessagePart::text(format!(
                    "Cheat console: sending \"{name}\" to {}",
                    self.slot_alias((team, slot))
                ))],
                print_type: Some(PrintJsonType::ItemCheat),
                receiving: Some(slot),
                item: Some(item),
                team: Some(team),
                ..Default::default()
            })],
        );

        self.send_new_items(&HashSet::from([slot]), out);
        Ok(name)
    }

    // --- admin -----------------------------------------------------------

    /// The remote-administration shell (`MultiServer.py:1445-1491`).
    ///
    /// Three layers, in this order, and the order is the point: the echo is
    /// masked and broadcast *first*, so what a client typed reaches the room
    /// whether or not the command is accepted, exists, or was even allowed to
    /// run. Then the room-level refusal when no server password is configured.
    /// Then login state, and only then dispatch into the `/` command set —
    /// which is [`Room::server_command`].
    fn cmd_admin(&mut self, conn: ConnId, command: &str, out: &mut dyn EffectSink) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let key = (client.team, client.slot);

        // Mask before echoing, whether or not the password was correct — the
        // room must not learn it from a failed attempt either.
        //
        // The `/option server_password` arm masks a command pahoa will never
        // implement, and it is **not** dead code: masking happens before the
        // refusal below, so what a client types is echoed to the room either
        // way. Deleting the arm because the command does not exist is exactly
        // backwards — an unimplemented setter is still a string someone types a
        // real password into.
        let lower = command.to_lowercase();
        let masked = if lower.starts_with("login") {
            let n = 4 + self.rng.randbelow(13) as usize;
            format!("!admin login {}", "*".repeat(n))
        } else if lower.starts_with("/option server_password") {
            let n = 4 + self.rng.randbelow(13) as usize;
            format!("!admin /option server_password {}", "*".repeat(n))
        } else {
            format!("!admin {command}")
        };
        self.broadcast_chat(key, &masked, &masked, out);

        let Some(server_password) = self.options.server_password.clone() else {
            self.notify(
                conn,
                "Sorry, Remote administration is disabled".to_string(),
                out,
            );
            return;
        };

        let authenticated = self.admin_conn == Some(conn);

        if command.is_empty() {
            let usage = if authenticated {
                vec![
                    "Usage: !admin [Server command].".to_string(),
                    "Use !admin /help for help.".to_string(),
                    "Use !admin logout to log out of the current session.".to_string(),
                ]
            } else {
                vec!["Usage: !admin login [password]".to_string()]
            };
            self.notify_multiple(conn, usage, out);
            return;
        }

        if let Some(supplied) = command.strip_prefix("login ") {
            // Constant-time, like every other password comparison here. The
            // reference compares the whole `f"login {password}"` string, which
            // is the same test; splitting the prefix off first is what lets the
            // secret go through `ct_eq` rather than through `==`.
            if crate::secret::ct_eq(supplied.as_bytes(), server_password.as_bytes()) {
                // Replaces whoever held it, matching the reference's single
                // `commandprocessor.client` slot. Worth telling the displaced
                // administrator, which the reference does not do — silently
                // losing the session is the sort of thing that reads as a bug.
                if let Some(previous) = self.admin_conn.replace(conn)
                    && previous != conn
                {
                    self.notify(
                        previous,
                        "Another client has logged in for remote administration; \
                         your session has ended."
                            .to_string(),
                        out,
                    );
                }
                self.notify(
                    conn,
                    "Login successful. You can now issue server side commands.".to_string(),
                    out,
                );
            } else {
                self.notify(conn, "Password incorrect.".to_string(), out);
            }
            return;
        }

        if !authenticated {
            self.notify(
                conn,
                "You must first login using !admin login [password]".to_string(),
                out,
            );
            return;
        }

        if command == "logout" {
            self.admin_conn = None;
            self.notify(
                conn,
                "Logout successful. You can no longer issue server side commands.".to_string(),
                out,
            );
            return;
        }

        self.server_command(conn, command, out);
    }

    // --- hints -----------------------------------------------------------

    /// `get_hints` (`MultiServer.py:1690-1817`), behind `!hint` and
    /// `!hint_location`.
    fn cmd_hint(
        &mut self,
        conn: ConnId,
        input: &str,
        for_location: bool,
        out: &mut dyn EffectSink,
    ) {
        let Some(client) = self.clients.get(&conn) else {
            return;
        };
        let (team, slot) = (client.team, client.slot);
        let key = (team, slot);
        let points_available = self.slot_points(key);
        let cost = self
            .options
            .hint_cost_for(self.data.locations.count_for(slot));

        // Bare `!hint` re-shows what you already know and quotes the price.
        if input.is_empty() {
            let owned: Vec<Hint> = self.hints.get(key).to_vec();
            self.notify_hints(team, owned, false, false, Some(&[slot]), out);
            self.notify(
                conn,
                format!("A hint costs {cost} points. You have {points_available} points."),
                out,
            );
            return;
        }

        let game = self.slot_game(slot);
        let Some(names) = self.datapackage.get(&game) else {
            self.notify(
                conn,
                "Can't look up item/location for unknown game. Hint for ID instead.".to_string(),
                out,
            );
            return;
        };

        // What was asked for, and whether it is hintable at all.
        let (candidates, subject) = if input.chars().all(|c| c.is_numeric()) {
            // An id, used directly. An id the game does not know still
            // resolves — to the "Unknown item (ID:…)" placeholder — which the
            // blacklist will never contain, so it falls through to a lookup
            // that finds nothing.
            let Ok(id) = input.parse::<i64>() else {
                self.notify(conn, format!("{input} is not a usable id."), out);
                return;
            };
            let name = if for_location {
                names.location_name(id)
            } else {
                names.item_name(id)
            };
            if !names.is_hintable(&name) {
                self.notify(
                    conn,
                    format!("Sorry, \"{name}\" is marked as non-hintable."),
                    out,
                );
                return;
            }
            (self.collect_hints_by_id(key, id, for_location), name)
        } else {
            let pool = if for_location {
                names.location_and_group_names()
            } else {
                names.item_and_group_names()
            };
            let Some(matched) = fuzzy::intended(input, &pool) else {
                self.notify(
                    conn,
                    "Can't look up item/location for unknown game. Hint for ID instead."
                        .to_string(),
                    out,
                );
                return;
            };
            let name = match &matched {
                fuzzy::Match::Accepted { name, .. } => name.clone(),
                fuzzy::Match::Rejected { message, .. } => {
                    self.notify(conn, message.clone(), out);
                    return;
                }
            };
            if !names.is_hintable(&name) {
                self.notify(
                    conn,
                    format!("Sorry, \"{name}\" is marked as non-hintable."),
                    out,
                );
                return;
            }
            (self.collect_hints_by_name(key, &name, for_location), name)
        };

        if candidates.is_empty() {
            let text = if points_available >= cost {
                let kind = if for_location { "location" } else { "item" };
                format!(
                    "Nothing found for recognized {kind} name \"{subject}\". \
                     {} appears to not exist in this multiworld.",
                    if for_location { "Location" } else { "Item" }
                )
            } else {
                format!(
                    "You can't afford the hint. You have {points_available} points and \
                     need at least {cost}."
                )
            };
            self.notify(conn, text, out);
            return;
        }

        self.pay_for_hints(Some(conn), key, candidates, points_available, cost, out);
    }

    /// Split candidates into already-known and new, charge for the new ones,
    /// and announce whatever the player ended up with.
    /// `conn` is `None` for an administrator's hint: the economy applies exactly
    /// as it would to the player, but there is no caller to explain it to. The
    /// count of hints actually granted comes back for the admin API's response.
    pub(super) fn pay_for_hints(
        &mut self,
        conn: Option<ConnId>,
        key: SlotKey,
        candidates: Vec<Hint>,
        points_available: i64,
        cost: i64,
        out: &mut dyn EffectSink,
    ) -> usize {
        let team = key.0;
        let (known, fresh): (Vec<Hint>, Vec<Hint>) = candidates
            .into_iter()
            .partition(|h| self.hints.contains(key, &h.identity()));

        if fresh.is_empty() {
            if !known.is_empty() {
                self.notify_hints(team, known, false, false, None, out);
                if let Some(conn) = conn {
                    self.notify(
                        conn,
                        "Hint was previously used, no points deducted.".to_string(),
                        out,
                    );
                }
            }
            return 0;
        }

        let (found, unfound): (Vec<Hint>, Vec<Hint>) = fresh.into_iter().partition(|h| h.found);
        let budget = crate::hints::budget(cost, points_available, !unfound.is_empty());

        // Split the borrow: the sphere lookup reads the multidata while the
        // shuffle advances the room's PRNG.
        let (granted, remaining) = {
            let Self { rng, data, .. } = self;
            let spheres = |player: u32, location: i64| sphere_of(data, player, location);
            crate::hints::choose(rng, unfound, &spheres, budget)
        };

        let granted_count = granted.len();
        *self.hints_used.entry(key).or_insert(0) += granted.len() as i64;

        // Already-found hints are free, so they ride along with whatever was
        // paid for, and the ones the player already had are re-announced.
        let mut announce = found;
        announce.extend(known);
        announce.extend(granted);
        // The follow-up message turns on whether the player got *anything*,
        // not on whether they paid (`MultiServer.py:1792-1804`).
        let got_something = !announce.is_empty();
        self.notify_hints(team, announce, false, false, None, out);

        if !remaining.is_empty() {
            let now = self.slot_points(key);
            // Floored division, like Python's `//`: with a negative balance the
            // truncating form would report "you can't afford any more" as
            // "rerun for more".
            let broke = cost != 0 && now.div_euclid(cost) == 0;
            let text = if got_something && broke {
                format!(
                    "There may be more hintables, however, you cannot afford to pay for \
                     any more.  You have {now} and need at least {cost}."
                )
            } else if got_something {
                "There may be more hintables, you can rerun the command to find more.".to_string()
            } else {
                format!(
                    "You can't afford the hint. You have {now} points and need at least \
                     {cost}."
                )
            };
            if let Some(conn) = conn {
                self.notify(conn, text, out);
            }
        }
        out.mark_dirty();
        granted_count
    }

    fn collect_hints_by_id(&self, key: SlotKey, id: i64, for_location: bool) -> Vec<Hint> {
        if for_location {
            self.collect_location_hints(key, key.1, id, Some(HintStatus::Unspecified))
        } else {
            self.collect_item_hints(key, id)
        }
    }

    /// Resolve an accepted name, which may be a single item/location or a
    /// group standing for many (`MultiServer.py:1736-1751`).
    pub(super) fn collect_hints_by_name(
        &self,
        key: SlotKey,
        name: &str,
        for_location: bool,
    ) -> Vec<Hint> {
        let game = self.slot_game(key.1);
        let Some(names) = self.datapackage.get(&game) else {
            return Vec::new();
        };

        if !for_location {
            if let Some(members) = names.package.item_name_groups.get(name) {
                return members
                    .iter()
                    .filter_map(|m| names.item_id(m))
                    .flat_map(|id| self.collect_item_hints(key, id))
                    .collect();
            }
            if let Some(id) = names.item_id(name) {
                return self.collect_item_hints(key, id);
            }
        }
        if let Some(members) = names.package.location_name_groups.get(name) {
            return members
                .iter()
                .filter_map(|m| names.location_id(m))
                .flat_map(|id| {
                    self.collect_location_hints(key, key.1, id, Some(HintStatus::Unspecified))
                })
                .collect();
        }
        match names.location_id(name) {
            Some(id) => self.collect_location_hints(key, key.1, id, Some(HintStatus::Unspecified)),
            None => Vec::new(),
        }
    }

    fn collect_item_hints(&self, key: SlotKey, item: i64) -> Vec<Hint> {
        crate::hints::collect_for_item(
            &self.data,
            &self.hints,
            key,
            key.1,
            item,
            None,
            &|s, loc| {
                self.location_checks
                    .get(&(key.0, s))
                    .is_some_and(|c| c.contains(&loc))
            },
        )
    }
}

/// `get_sphere` (`MultiServer.py:762-771`).
///
/// A free function so the hint shuffle can borrow the PRNG and the sphere
/// table at the same time.
fn sphere_of(data: &MultiData, player: u32, location: i64) -> usize {
    if data.spheres.is_empty() {
        // Python returns -1 here; the ordering only ever compares spheres
        // against each other, so a single shared value is equivalent.
        return 0;
    }
    data.spheres
        .iter()
        .position(|s| s.get(&player).is_some_and(|set| set.contains(&location)))
        // The reference raises for a location outside every sphere. Sorting it
        // last is friendlier and cannot corrupt anything.
        .unwrap_or(data.spheres.len())
}

/// Python's `str.isprintable`, approximately.
///
/// Rejects control characters and any whitespace other than a plain space,
/// which is what keeps a chat line from carrying newlines or terminal escapes.
/// Python also rejects the format, surrogate, private-use and unassigned
/// categories; matching that needs a Unicode-category table, and the gap only
/// covers things like zero-width joiners that no client sends today. Worth
/// revisiting alongside the other anti-griefing work.
pub(super) fn is_printable(s: &str) -> bool {
    s.chars()
        .all(|c| !c.is_control() && (c == ' ' || !c.is_whitespace()))
}

/// `shlex.split(raw, comments=False)`, with Python's own fallback.
///
/// POSIX mode: single quotes are literal, double quotes allow backslash
/// escapes, and a bare backslash escapes the next character. An unterminated
/// quote raises in Python and the caller falls back to plain whitespace
/// splitting (`MultiServer.py:1340-1343`) — reproduced by returning that here.
fn shell_split(raw: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut has_word = false;
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            '\'' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return raw.split_whitespace().map(str::to_string).collect(),
                    }
                }
            }
            '"' => {
                has_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // Only these four are escapes inside double quotes;
                            // any other backslash stays literal.
                            Some(e @ ('"' | '\\' | '$' | '`')) => current.push(e),
                            Some(other) => {
                                current.push('\\');
                                current.push(other);
                            }
                            None => {
                                return raw.split_whitespace().map(str::to_string).collect();
                            }
                        },
                        Some(c) => current.push(c),
                        None => return raw.split_whitespace().map(str::to_string).collect(),
                    }
                }
            }
            '\\' => {
                has_word = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c => {
                has_word = true;
                current.push(c);
            }
        }
    }
    if has_word {
        words.push(current);
    }
    words
}

/// How Python renders an optional option value in `!options`.
pub(super) fn opt_str(v: &Option<String>) -> String {
    match v {
        Some(s) => s.clone(),
        None => "None".to_string(),
    }
}

pub(super) fn py_bool(v: bool) -> &'static str {
    if v { "True" } else { "False" }
}

const LICENSE: &str = "\
pahoa is a reimplementation of the Archipelago MultiServer.
Archipelago is available under the MIT license at
https://github.com/ArchipelagoMW/Archipelago.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_split_handles_quotes_the_way_shlex_does() {
        assert_eq!(shell_split("!status DeathLink"), ["!status", "DeathLink"]);
        assert_eq!(
            shell_split("!status \"Death Link\""),
            ["!status", "Death Link"]
        );
        assert_eq!(
            shell_split("!status 'Death Link'"),
            ["!status", "Death Link"]
        );
        // Quotes vanish even mid-word, as in the shell.
        assert_eq!(shell_split("a\"b c\"d"), ["ab cd"]);
        assert_eq!(shell_split("a\\ b"), ["a b"]);
        assert_eq!(shell_split("   "), Vec::<String>::new());
        // Escapes inside double quotes.
        assert_eq!(shell_split(r#""a\"b""#), [r#"a"b"#]);
        assert_eq!(shell_split(r#""a\nb""#), [r"a\nb"]);
    }

    #[test]
    fn an_unterminated_quote_falls_back_to_whitespace_splitting() {
        // shlex raises here and the reference catches it, so a player typing an
        // apostrophe does not get an error page instead of a command.
        assert_eq!(
            shell_split("!hint Farmer's Hat"),
            ["!hint", "Farmer's", "Hat"]
        );
        assert_eq!(shell_split("!hint \"unclosed"), ["!hint", "\"unclosed"]);
    }

    #[test]
    fn raw_commands_are_the_ones_taking_names() {
        // Names contain spaces and quotes, so they must not be word-split.
        for cmd in [
            "hint",
            "hint_location",
            "getitem",
            "alias",
            "missing",
            "checked",
        ] {
            assert!(takes_raw_argument(cmd), "{cmd} should take raw text");
        }
        for cmd in ["help", "players", "status", "options", "license"] {
            assert!(!takes_raw_argument(cmd), "{cmd} should be word-split");
        }
    }

    #[test]
    fn printable_rejects_control_characters_and_newlines() {
        assert!(is_printable("hello world"));
        assert!(is_printable(""));
        assert!(is_printable("héllo ✓ 日本語"));
        assert!(!is_printable("two\nlines"));
        assert!(!is_printable("bell\x07"));
        assert!(!is_printable("escape\x1b[31m"));
        // A non-breaking space is whitespace but not a plain space.
        assert!(!is_printable("a\u{a0}b"));
    }
}
