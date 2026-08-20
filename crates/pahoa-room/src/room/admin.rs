//! Commands issued from outside a connection.
//!
//! The chat commands in [`super::commands`] are all connection-scoped:
//! `!release` releases *the caller's* slot, and their replies go back to the
//! caller's socket. An administrator has neither — the target is supplied, and
//! the reply is a document rather than a message to a player.
//!
//! So this reuses the **primitives** those handlers sit on rather than the
//! handlers themselves. That is deliberate and not laziness in the other
//! direction: [`Room::notify`](super::Room) silently does nothing for a
//! `ConnId` that is not a registered, authenticated client, so driving the
//! chat handlers with a synthetic connection would produce commands that
//! report success and change nothing.
//!
//! Where an administrator's action should read to players exactly as the chat
//! one does, the announcement comes from the same primitive — `release_player`
//! broadcasts its own line — so the two cannot drift.

use super::*;

/// A command with its target supplied rather than inferred from a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminCommand {
    Status,
    Say {
        text: String,
    },
    Countdown {
        seconds: i64,
    },
    Release {
        slot: u32,
    },
    Collect {
        slot: u32,
    },
    SendItem {
        slot: u32,
        item: String,
    },
    Hint {
        slot: u32,
        item: String,
        /// Grant the hint regardless of what the slot can afford.
        force: bool,
    },
    Kick {
        slot: u32,
        reason: String,
    },
}

/// What happened, in the shape the admin API answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminOutcome {
    pub ok: bool,
    /// Rendered verbatim in an operator's console, so this is pahoa's own
    /// phrasing rather than a status code they have to interpret.
    pub output: Vec<String>,
    pub affected_slots: Vec<u32>,
}

impl AdminOutcome {
    fn ok(line: impl Into<String>, slots: Vec<u32>) -> Self {
        Self {
            ok: true,
            output: vec![line.into()],
            affected_slots: slots,
        }
    }

    fn lines(output: Vec<String>) -> Self {
        Self {
            ok: true,
            output,
            affected_slots: Vec::new(),
        }
    }

    /// A command that was understood but could not be carried out.
    fn refused(line: impl Into<String>) -> Self {
        Self {
            ok: false,
            output: vec![line.into()],
            affected_slots: Vec::new(),
        }
    }
}

impl Room {
    /// Run one administrative command.
    ///
    /// Every path is synchronous and runs on the actor, holding `&mut Room`, so
    /// there is no locking here and nothing to reconcile afterwards.
    pub fn admin(&mut self, command: AdminCommand, out: &mut dyn EffectSink) -> AdminOutcome {
        match command {
            AdminCommand::Status => AdminOutcome::lines(self.admin_status()),
            AdminCommand::Say { text } => self.admin_say(text, out),
            AdminCommand::Countdown { seconds } => self.admin_countdown(seconds, out),
            AdminCommand::Release { slot } => self.admin_release(slot, out),
            AdminCommand::Collect { slot } => self.admin_collect(slot, out),
            AdminCommand::SendItem { slot, item } => self.admin_send_item(slot, &item, out),
            AdminCommand::Hint { slot, item, force } => self.admin_hint(slot, &item, force, out),
            AdminCommand::Kick { slot, reason } => self.admin_kick(slot, &reason, out),
        }
    }

    /// The slot key for a slot number on team 0.
    ///
    /// Teams are not addressable over this API yet — nothing in the multiworlds
    /// pahoa serves uses more than one — so this is where that assumption
    /// lives, rather than spread across every handler.
    fn admin_key(&self, slot: u32) -> Option<SlotKey> {
        self.data.slot_info.contains_key(&slot).then_some((0, slot))
    }

    fn unknown_slot(slot: u32) -> AdminOutcome {
        AdminOutcome::refused(format!("There is no slot {slot} in this seed."))
    }

    /// The same per-slot rendering `!status` produces, without a caller to send
    /// it to.
    fn admin_status(&self) -> Vec<String> {
        let mut lines = Vec::with_capacity(self.data.slot_info.len() + 1);
        lines.push(format!(
            "{} of {} slots connected.",
            self.data
                .slot_info
                .keys()
                .filter(|slot| self.connections_for((0, **slot)) > 0)
                .count(),
            self.data.slot_info.len(),
        ));

        for slot in self.data.slot_info.keys() {
            let key = (0, *slot);
            let connected = self.connections_for(key);
            let status = match self.status(key) {
                ClientStatus::Goal => " and has finished.",
                ClientStatus::Ready => " and is ready.",
                _ => ".",
            };
            lines.push(format!(
                "{} has {connected} connection{}{status} ({}/{})",
                self.slot_alias(key),
                if connected == 1 { "" } else { "s" },
                self.checked_count(key),
                self.data.locations.count_for(*slot),
            ));
        }
        lines
    }

    fn admin_say(&mut self, text: String, out: &mut dyn EffectSink) -> AdminOutcome {
        // The same validator client chat goes through: control characters and
        // exotic whitespace would render unpredictably in every client.
        if !super::commands::is_printable(&text) {
            return AdminOutcome::refused(
                "That message contains characters clients cannot display.".to_string(),
            );
        }
        if text.is_empty() {
            return AdminOutcome::refused("Nothing to say.".to_string());
        }

        self.broadcast_server_chat(&text, out);
        AdminOutcome::ok(format!("Said to the room: {text}"), Vec::new())
    }

    fn admin_countdown(&mut self, seconds: i64, out: &mut dyn EffectSink) -> AdminOutcome {
        // The same bound `!countdown` applies. An administrator may run one in a
        // room where the mode denies it to players — that is what the mode is
        // for — but not an unbounded one.
        if !(0..=60 * 60).contains(&seconds) {
            return AdminOutcome::refused(format!(
                "{seconds} is invalid. A countdown runs between 0 and 3600 seconds."
            ));
        }
        let now = self.clock;
        self.start_countdown(seconds, now, out);
        AdminOutcome::ok(format!("Started a {seconds} second countdown."), Vec::new())
    }

    /// Unconditional, unlike `!release`: the mode gates *players*, and being
    /// able to release for someone who cannot is the point of an admin API.
    fn admin_release(&mut self, slot: u32, out: &mut dyn EffectSink) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        let before = self.checked_count(key);
        self.release_player(key, out);
        out.mark_dirty();

        let released = self.checked_count(key).saturating_sub(before);
        AdminOutcome::ok(
            format!(
                "Released {released} locations for {}.",
                self.slot_alias(key)
            ),
            vec![slot],
        )
    }

    fn admin_collect(&mut self, slot: u32, out: &mut dyn EffectSink) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        self.collect_player(key, out);
        out.mark_dirty();
        AdminOutcome::ok(
            format!("Collected for {}.", self.slot_alias(key)),
            vec![slot],
        )
    }

    /// The cheat console's effect, aimed at a slot rather than at the caller.
    fn admin_send_item(&mut self, slot: u32, item: &str, out: &mut dyn EffectSink) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        // Deliberately not gated on `item_cheat`: that option decides whether
        // *players* may help themselves, and an administrator granting an item
        // is the sanctioned path it points people at.
        match self.grant_item(key, item, out) {
            Ok(name) => {
                out.mark_dirty();
                AdminOutcome::ok(
                    format!("Sent \"{name}\" to {}.", self.slot_alias(key)),
                    vec![slot],
                )
            }
            Err(message) => AdminOutcome::refused(message),
        }
    }

    fn admin_hint(
        &mut self,
        slot: u32,
        item: &str,
        force: bool,
        out: &mut dyn EffectSink,
    ) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };

        let hints = self.collect_hints_by_name(key, item, false);
        if hints.is_empty() {
            return AdminOutcome::refused(format!(
                "Found no hintable item matching \"{item}\" for {}.",
                self.slot_alias(key)
            ));
        }

        if force {
            // Straight past the economy: no points are spent and `hints_used`
            // does not move, because an administrator granting a hint is not the
            // slot buying one.
            let count = hints.len();
            self.notify_hints(key.0, hints, true, true, None, out);
            out.mark_dirty();
            AdminOutcome::ok(
                format!("Granted {count} hint(s) to {}.", self.slot_alias(key)),
                vec![slot],
            )
        } else {
            // The slot's own economy, exactly as `!hint` would apply it —
            // an administrator asking without `force` is asking on the
            // player's behalf, not overriding them.
            let points = self.slot_points(key);
            let cost = self
                .options
                .hint_cost_for(self.data.locations.count_for(slot));
            let granted = self.pay_for_hints(None, key, hints, points, cost, out);
            out.mark_dirty();
            AdminOutcome::ok(
                format!(
                    "Hinted {granted} item(s) for {}, at their own cost.",
                    self.slot_alias(key)
                ),
                vec![slot],
            )
        }
    }

    /// Disconnect every connection a slot has open.
    ///
    /// A kick is a disconnect and not a ban: nothing stops an immediate
    /// reconnect, which the response says so an operator is not surprised by it.
    fn admin_kick(&mut self, slot: u32, reason: &str, out: &mut dyn EffectSink) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };

        let conns = self.resolve(&Recipients::Slot(key));
        if conns.is_empty() {
            return AdminOutcome::refused(format!("{} is not connected.", self.slot_alias(key)));
        }

        // Said first, and as a message rather than a close reason: the shard's
        // close carries a `&'static str`, and widening that for an operator's
        // sentence would put an allocation on the broadcast path to serve one
        // rare command.
        if !reason.is_empty() && super::commands::is_printable(reason) {
            out.broadcast(
                Recipients::SlotText(key),
                &[ServerPacket::PrintJSON(PrintJson {
                    data: vec![JsonMessagePart::text(format!(
                        "You have been disconnected by an administrator: {reason}"
                    ))],
                    print_type: Some(PrintJsonType::CommandResult),
                    ..Default::default()
                })],
            );
        }

        let count = conns.len();
        for conn in conns {
            out.close(conn, CloseReason::Kicked);
        }

        // Deliberately not removed from `clients` here. The socket closing
        // produces a real disconnect, which is what runs `on_disconnect` and
        // announces the departure exactly as any other would.
        AdminOutcome::ok(
            format!(
                "Disconnected {count} connection{} for {}. They may reconnect.",
                if count == 1 { "" } else { "s" },
                self.slot_alias(key)
            ),
            vec![slot],
        )
    }
}
