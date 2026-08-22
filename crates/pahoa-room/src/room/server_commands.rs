//! The `/` command set, reached through `!admin`.
//!
//! Ports `ServerCommandProcessor` (`MultiServer.py:2222-2560`). In the
//! reference this is the *console* processor — the one an operator drives from
//! the terminal the server was launched in — and `!admin` merely forwards into
//! it once a client has logged in. Pahoa has no console, so this exists only
//! for `!admin`, and everything an operator would otherwise reach here is on
//! the HTTP admin API instead.
//!
//! That split explains what is implemented. `/option` is here because it is the
//! one thing with no HTTP equivalent and no reason to gain one: changing a
//! room's rules mid-game is an *organizer's* act, done from inside the game,
//! and an organizer has a chat window rather than a bearer token. Commands that
//! act on a player — release, collect, kick, send — are deliberately not here;
//! they exist on the admin API, which authenticates properly and does not put
//! the operation in the room's chat log.
//!
//! ## What a setter may change
//!
//! **Exactly the options the save is authoritative for.** `save::encode_options`
//! persists every field of [`RoomOptions`](crate::RoomOptions) except the three
//! secrets, and [`Room::restore`](super::Room) takes them from the snapshot, so
//! a change made here survives a restart with nothing further to write.
//!
//! The passwords are excluded for the same reason read the other way round: the
//! save deliberately carries no secret, so a live change to one would revert at
//! the next restart, in every deployment rather than only an orchestrated one.
//! `/option password` and `/option server_password` are therefore refused
//! explicitly rather than reported as unknown — they *are* recognized, and
//! saying so is the difference between a decision and a gap.
//! [`Room::cmd_admin`](super::Room) states the rule the two halves come from.
//!
//! ## Telling clients
//!
//! `hint_cost`, `location_check_points` and the permission modes all went out in
//! `RoomInfo` at connect, so a client holds a stale copy until pushed a
//! `RoomUpdate` (`MultiServer.py:2543-2551`). The fan-out differs by option and
//! getting it wrong is silent, so see [`Push`].

use super::commands::{opt_str, py_bool};
use super::*;

/// Options `/option` will set, and how to read their values.
///
/// Mirrors `Context.simple_options` (`MultiServer.py:220-229`) minus the two
/// passwords, which [`Room::server_cmd_option`] refuses by name so the message
/// can say why.
const SETTABLE: &[(&str, Kind)] = &[
    ("hint_cost", Kind::Int),
    ("location_check_points", Kind::Int),
    ("release_mode", Kind::Mode),
    ("remaining_mode", Kind::Mode),
    ("collect_mode", Kind::Mode),
    ("countdown_mode", Kind::Mode),
    ("item_cheat", Kind::Bool),
    ("compatibility", Kind::Int),
];

/// Options that exist, are recognized, and are still refused. See the module
/// documentation.
const REFUSED: &[&str] = &["password", "server_password"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Mode,
    Bool,
}

impl Kind {
    /// The spelling `/option` prints when it lists what it accepts, matching
    /// the reference's `f"{option}: {option_type}"` shape.
    fn as_text(self) -> &'static str {
        match self {
            Self::Int => "int",
            Self::Mode => "str",
            Self::Bool => "bool",
        }
    }
}

/// What a successful `/option` has to tell connected clients.
///
/// The three shapes are not interchangeable and the wrong one fails quietly —
/// clients simply keep believing the old value — so this is an enum rather than
/// a condition at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Push {
    /// One room-wide broadcast carrying the whole `permissions` map.
    Permissions,
    /// **Per slot**, carrying the new value and that slot's recomputed
    /// `hint_points`.
    ///
    /// Room-wide would be wrong: `hint_cost` is a percentage of a slot's
    /// location count (`get_hint_cost`), so one broadcast either omits the
    /// points or sends one slot's number to every slot.
    PerSlot,
    /// Nothing. `countdown_mode` and `item_cheat` have no client-side
    /// representation — neither appears in `RoomInfo` — and `compatibility`
    /// governs how the server reads packets rather than anything a client
    /// displays.
    Nothing,
}

impl Room {
    /// Dispatch one line from an authenticated `!admin` session.
    ///
    /// A line that does not begin with `/` is not an error: the reference's
    /// `default` broadcasts it to the room as the server speaking
    /// (`MultiServer.py:2232-2233`), which is how an organizer makes an
    /// announcement that does not look like it came from their own slot.
    pub(crate) fn server_command(&mut self, conn: ConnId, raw: &str, out: &mut dyn EffectSink) {
        let mut words = raw.split_whitespace();
        let Some(first) = words.next() else {
            return;
        };
        let Some(name) = first.strip_prefix('/') else {
            // The same helper the admin API announces through, so the two
            // cannot disagree about what a server message looks like.
            self.broadcast_server_chat(raw, out);
            return;
        };

        let args: Vec<&str> = words.collect();
        match name.to_ascii_lowercase().as_str() {
            "option" => self.server_cmd_option(conn, &args, out),
            "options" => self.server_cmd_options(conn, out),
            "help" => self.server_cmd_help(conn, out),
            other => {
                self.admin_out(
                    conn,
                    format!("Unknown command {other}. Use /help for a list of commands."),
                    out,
                );
            }
        }
    }

    /// One line of `/` command output, which is `AdminCommandResult` rather
    /// than the `CommandResult` a `!` command replies with.
    fn admin_out(&self, conn: ConnId, text: String, out: &mut dyn EffectSink) {
        self.notify_admin(conn, vec![text], out);
    }

    fn server_cmd_help(&self, conn: ConnId, out: &mut dyn EffectSink) {
        let mut lines = vec![
            "/help".to_string(),
            "    Returns the help listing".to_string(),
            "/options".to_string(),
            "    List all current options".to_string(),
            "/option <name> <value>".to_string(),
            "    Set an option for the server".to_string(),
        ];
        // Said plainly, because the gap is deliberate and an administrator who
        // reaches for `/release` should learn where it went rather than
        // conclude the shell is broken.
        lines.push(
            "Anything not starting with / is announced to the room as the server. \
             Commands that act on a player — release, collect, send, kick — are on \
             the HTTP admin API rather than here."
                .to_string(),
        );
        self.notify_admin(conn, lines, out);
    }

    /// `_cmd_options` (`MultiServer.py:1409-1416`).
    ///
    /// Reached with the `/` marker rather than `!`, which in the reference is
    /// exactly the condition under which the real `server_password` is printed
    /// instead of asterisks. Kept: whoever is reading this typed that password
    /// a moment ago to get here, so masking it would protect nothing, and the
    /// output goes to that one connection.
    fn server_cmd_options(&mut self, conn: ConnId, out: &mut dyn EffectSink) {
        let o = &self.options;
        let texts = vec![
            "Current options:".to_string(),
            format!("Option hint_cost is set to {}", o.hint_cost),
            format!(
                "Option location_check_points is set to {}",
                o.location_check_points
            ),
            format!(
                "Option server_password is set to {}",
                opt_str(&o.server_password)
            ),
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
        self.notify_admin(conn, texts, out);
    }

    /// `_cmd_option` (`MultiServer.py:2513-2551`).
    fn server_cmd_option(&mut self, conn: ConnId, args: &[&str], out: &mut dyn EffectSink) {
        let (Some(name), Some(value)) = (args.first(), args.get(1)) else {
            // The reference reaches Python's own arity error here and prints the
            // traceback. A usage line carries the same information.
            self.admin_out(conn, "Usage: /option <name> <value>".to_string(), out);
            return;
        };
        match self.apply_option(name, value, "!admin", out) {
            Ok(line) => self.admin_out(conn, line, out),
            Err(lines) => self.notify_admin(conn, lines, out),
        }
    }

    /// Set one option, with no caller to reply to.
    ///
    /// Split out of [`Room::server_cmd_option`] so the HTTP admin API reaches
    /// **exactly** this — the same names, the same value parsing, the same
    /// refusals, the same `RoomUpdate` fan-out and the same journal records.
    /// Two implementations of "what may be set and to what" would drift, and
    /// the failure would be silent: a room whose rules disagree with what the
    /// surface that set them believes.
    ///
    /// `Ok` carries the confirmation line, `Err` the refusal — more than one
    /// line where the reason needs explaining.
    pub(super) fn apply_option(
        &mut self,
        name: &str,
        value: &str,
        source: &'static str,
        out: &mut dyn EffectSink,
    ) -> Result<String, Vec<String>> {
        let name = name.to_ascii_lowercase();

        if REFUSED.contains(&name.as_str()) {
            return Err(vec![
                format!("Option {name} cannot be set while the room is running."),
                "Passwords are never written to the save, so a change here would \
                 revert the next time the room restarts. Set it where the room is \
                 configured instead."
                    .to_string(),
            ]);
        }

        let Some((_, kind)) = SETTABLE.iter().find(|(option, _)| *option == name) else {
            let known: Vec<String> = SETTABLE
                .iter()
                .map(|(option, kind)| format!("{option}: {}", kind.as_text()))
                .collect();
            return Err(vec![format!(
                "Unrecognized option '{name}', known: {}",
                known.join(", ")
            )]);
        };

        let push = match kind {
            Kind::Int => self.set_int_option(&name, value).map_err(|m| vec![m])?,
            Kind::Mode => self.set_mode_option(&name, value).map_err(|m| vec![m])?,
            Kind::Bool => {
                // `bool("off") is True` in Python, so the reference spells the
                // falsey words out rather than casting (`MultiServer.py:2522`).
                let on = !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "off" | "0" | "false" | "none" | "null" | "no"
                );
                self.options.item_cheat = on;
                Push::Nothing
            }
        };

        out.mark_dirty();
        // An explicit audit record, with the option and its new value as
        // fields. The chat echo already shows that *someone ran a command*;
        // this is the one that survives as data — and it is safe to log in a
        // way `/options` is not, because the only options reachable here are
        // the ones with no secret in them.
        tracing::info!(
            option = %name,
            value = %self.option_as_text(&name),
            source,
            "room option changed"
        );
        out.journal_event(crate::effect::JournalEvent::option_changed(
            self.clock,
            &name,
            &self.option_as_text(&name),
        ));
        // The whole option set again, so a reader never has to replay every
        // change from the start to know what the room's rules were at a moment.
        out.journal_event(crate::effect::JournalEvent::options(
            self.clock,
            &self.options,
        ));
        self.push_option_update(push, &name, out);
        Ok(format!(
            "Set option {name} to {}",
            self.option_as_text(&name)
        ))
    }

    fn set_int_option(&mut self, name: &str, value: &str) -> Result<Push, String> {
        // The reference takes Python's `int`, which accepts negatives and then
        // stores them. A negative hint cost makes hints *pay*, and a negative
        // compatibility level means nothing at all, so this rejects rather than
        // reproducing it — the one place the parsing deliberately narrows.
        let Ok(parsed) = value.parse::<u32>() else {
            return Err(format!(
                "Could not read '{value}' as a whole number for {name}."
            ));
        };
        match name {
            "hint_cost" => {
                self.options.hint_cost = parsed;
                Ok(Push::PerSlot)
            }
            "location_check_points" => {
                self.options.location_check_points = parsed;
                Ok(Push::PerSlot)
            }
            "compatibility" => {
                let level = u8::try_from(parsed)
                    .map_err(|_| format!("compatibility is 0, 1 or 2, not {parsed}."))?;
                self.options.compatibility = level;
                Ok(Push::Nothing)
            }
            _ => unreachable!("every Kind::Int option is handled"),
        }
    }

    fn set_mode_option(&mut self, name: &str, value: &str) -> Result<Push, String> {
        let lower = value.to_ascii_lowercase();
        // Accepts both spellings of `auto_enabled`. The reference's valid set
        // holds the underscore, but pahoa *prints* the hyphen — `!options` and
        // `--release-mode` both do — and rejecting the spelling the room just
        // showed you would be an unforced trap.
        let normalized = lower.replace('-', "_");

        let valid: &[&str] = match name {
            // Countdown has no `goal` or `auto_enabled` (`MultiServer.py:2527`).
            "countdown_mode" => &["enabled", "disabled", "auto"],
            "remaining_mode" => &["goal", "enabled", "disabled"],
            _ => &["goal", "enabled", "disabled", "auto", "auto_enabled"],
        };
        if !valid.contains(&normalized.as_str()) {
            return Err(format!(
                "Unrecognized {name} value '{value}', known: {}",
                valid.join(", ")
            ));
        }

        let permission = Permission::from_text(&normalized);
        match name {
            "release_mode" => self.options.release_mode = permission,
            "collect_mode" => self.options.collect_mode = permission,
            "remaining_mode" => self.options.remaining_mode = permission,
            "countdown_mode" => {
                self.options.countdown_mode = permission;
                return Ok(Push::Nothing);
            }
            _ => unreachable!("every Kind::Mode option is handled"),
        }
        Ok(Push::Permissions)
    }

    /// How a set option reads back, for the confirmation line.
    fn option_as_text(&self, name: &str) -> String {
        let o = &self.options;
        match name {
            "hint_cost" => o.hint_cost.to_string(),
            "location_check_points" => o.location_check_points.to_string(),
            "compatibility" => o.compatibility.to_string(),
            "release_mode" => o.release_mode.as_text().to_string(),
            "collect_mode" => o.collect_mode.as_text().to_string(),
            "remaining_mode" => o.remaining_mode.as_text().to_string(),
            "countdown_mode" => o.countdown_mode.as_text().to_string(),
            "item_cheat" => py_bool(o.item_cheat).to_string(),
            _ => String::new(),
        }
    }

    /// Tell connected clients what changed. See [`Push`].
    fn push_option_update(&mut self, push: Push, name: &str, out: &mut dyn EffectSink) {
        match push {
            Push::Nothing => {}
            Push::Permissions => {
                // Every connection, not only the text ones: this is state a
                // tracker needs as much as a player does.
                out.broadcast(
                    Recipients::All,
                    &[ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                        permissions: Some(self.permissions()),
                        ..Default::default()
                    }))],
                );
            }
            Push::PerSlot => {
                // Only slots someone is actually connected to, as the reference
                // walks `ctx.clients`. A slot nobody is playing learns the new
                // value in `RoomInfo` whenever it does connect.
                let keys: Vec<SlotKey> = self
                    .by_slot
                    .iter()
                    .filter(|(_, conns)| !conns.is_empty())
                    .map(|(key, _)| *key)
                    .collect();
                for key in keys {
                    let mut update = RoomUpdate {
                        hint_points: Some(self.slot_points(key)),
                        ..Default::default()
                    };
                    if name == "hint_cost" {
                        update.hint_cost = Some(self.options.hint_cost);
                    } else {
                        update.location_check_points = Some(self.options.location_check_points);
                    }
                    out.broadcast(
                        Recipients::Slot(key),
                        &[ServerPacket::RoomUpdate(Box::new(update))],
                    );
                }
            }
        }
    }
}
