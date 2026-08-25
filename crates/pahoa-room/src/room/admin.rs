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

/// Most copies of one item `send_multiple` will grant at once.
///
/// The reference's number (`MultiServer.py:2383-2384`), and worth keeping
/// rather than raising: every copy is queued on both of a slot's item streams
/// and replayed from index zero on each reconnect, so a slip of the keyboard
/// asking for a million is a room that never finishes sending them.
pub const SEND_MULTIPLE_LIMIT: i64 = 100;

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
    /// [`AdminCommand::SendItem`] with a count, capped at
    /// [`SEND_MULTIPLE_LIMIT`].
    SendMultiple {
        slot: u32,
        item: String,
        amount: i64,
    },
    Hint {
        slot: u32,
        item: String,
        /// Grant the hint regardless of what the slot can afford.
        force: bool,
    },
    /// The location half of [`AdminCommand::Hint`], kept a separate verb rather
    /// than a flag because the reference names it separately (`/hint_location`)
    /// and an operator who knows the console looks for that word.
    HintLocation {
        slot: u32,
        location: String,
        force: bool,
    },
    /// Check a location on a slot's behalf, as though the player had.
    SendLocation {
        slot: u32,
        location: String,
    },
    /// Exempt one slot from `release_mode`, or return it to the room's rule.
    ///
    /// Not a permission of its own: `allowed: false` clears the exemption
    /// rather than denying a release the mode would otherwise permit. See
    /// [`Room::admin_allow_release`].
    AllowRelease {
        slot: u32,
        allowed: bool,
    },
    /// Bar a slot from connecting, or let it back in.
    ///
    /// Orthogonal to the password modes and to [`AdminCommand::Kick`]: locking
    /// refuses the *next* login and leaves open connections alone.
    Lock {
        slot: u32,
        locked: bool,
    },
    /// Set a slot's completion status on its behalf.
    ///
    /// No reference equivalent — upstream's only external writer of
    /// `client_game_state` is the slot's own `StatusUpdate` packet. This exists
    /// for the case that leaves no other way out: a player has finished but
    /// their client cannot say so.
    ///
    /// **Goal is still a one-way door**, exactly as `MultiServer.py:2208`
    /// makes it. An operator may declare a slot done; nobody may undeclare it.
    SetStatus {
        slot: u32,
        status: ClientStatus,
    },
    /// Set or clear a slot's display alias, which `!alias` only lets a player
    /// do for themselves.
    Alias {
        slot: u32,
        /// Empty clears it.
        alias: String,
    },
    /// Change one of the room's rules, the same set `!admin` `/option` reaches.
    Option {
        name: String,
        value: String,
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
            AdminCommand::SendItem { slot, item } => self.admin_send_item(slot, &item, 1, out),
            AdminCommand::SendMultiple { slot, item, amount } => {
                self.admin_send_item(slot, &item, amount, out)
            }
            AdminCommand::Hint { slot, item, force } => {
                self.admin_hint(slot, &item, false, force, out)
            }
            AdminCommand::HintLocation {
                slot,
                location,
                force,
            } => self.admin_hint(slot, &location, true, force, out),
            AdminCommand::SendLocation { slot, location } => {
                self.admin_send_location(slot, &location, out)
            }
            AdminCommand::AllowRelease { slot, allowed } => {
                self.admin_allow_release(slot, allowed, out)
            }
            AdminCommand::Lock { slot, locked } => self.admin_lock(slot, locked, out),
            AdminCommand::SetStatus { slot, status } => self.admin_set_status(slot, status, out),
            AdminCommand::Alias { slot, alias } => self.admin_alias(slot, &alias, out),
            AdminCommand::Option { name, value } => self.admin_option(&name, &value, out),
            AdminCommand::Kick { slot, reason } => self.admin_kick(slot, &reason, out),
        }
    }

    /// The slot key a command targets.
    ///
    /// The team is [`pahoa_multidata::ONLY_TEAM`] because there is only one and
    /// a seed saying otherwise is refused at load; a caller that names a team
    /// explicitly is checked against it by the command parser rather than
    /// having it dropped. So this is where the assumption lives, once, instead
    /// of a literal in sixteen handlers.
    fn admin_key(&self, slot: u32) -> Option<SlotKey> {
        self.data
            .slot_info
            .contains_key(&slot)
            .then_some((pahoa_multidata::ONLY_TEAM, slot))
    }

    fn unknown_slot(slot: u32) -> AdminOutcome {
        AdminOutcome::refused(format!("There is no slot {slot} in this seed."))
    }

    /// The same per-slot rendering `!status` produces, without a caller to send
    /// it to.
    fn admin_status(&self) -> Vec<String> {
        // Every `(team, slot)`, so the counts stay right rather than reporting
        // one team's worth of a room that has more.
        let keys: Vec<SlotKey> = self
            .data
            .teams()
            .flat_map(|team| self.data.slot_info.keys().map(move |slot| (team, *slot)))
            .collect();

        let mut lines = Vec::with_capacity(keys.len() + 1);
        lines.push(format!(
            "{} of {} slots connected.",
            keys.iter()
                .filter(|key| self.connections_for(**key) > 0)
                .count(),
            keys.len(),
        ));

        for key in keys {
            let slot = &key.1;
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
    ///
    /// One implementation for `send_item` and `send_multiple`, because that is
    /// the reference's own arrangement: `_cmd_send` is `_cmd_send_multiple(1,
    /// …)` (`MultiServer.py:2400-2402`).
    fn admin_send_item(
        &mut self,
        slot: u32,
        item: &str,
        amount: i64,
        out: &mut dyn EffectSink,
    ) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        if amount < 1 {
            return AdminOutcome::refused(format!("{amount} is not a number of items to send."));
        }
        if amount > SEND_MULTIPLE_LIMIT {
            return AdminOutcome::refused(format!(
                "{amount} is too many; the most that may be sent at once is {SEND_MULTIPLE_LIMIT}."
            ));
        }
        // Deliberately not gated on `item_cheat`: that option decides whether
        // *players* may help themselves, and an administrator granting an item
        // is the sanctioned path it points people at.
        // Plain text, not a typed `ItemCheat` — see `CheatAnnounce`. This is
        // the console path, and the reference announces it without the type,
        // the item or the receiving slot.
        match self.grant_items(
            key,
            item,
            amount as usize,
            super::commands::CheatAnnounce::Plain,
            out,
        ) {
            Ok(name) => {
                out.mark_dirty();
                let who = self.slot_alias(key);
                AdminOutcome::ok(
                    if amount == 1 {
                        format!("Sent \"{name}\" to {who}.")
                    } else {
                        format!("Sent {amount} of \"{name}\" to {who}.")
                    },
                    vec![slot],
                )
            }
            Err(message) => AdminOutcome::refused(message),
        }
    }

    /// One implementation for `/hint` and `/hint_location`.
    ///
    /// `collect_hints_by_name` already takes the item/location distinction and
    /// already resolves *name groups* on both sides, which is the part worth
    /// not writing twice: `!hint_location` reaches it through the same call
    /// with the same flag, so the two surfaces cannot disagree about what a
    /// name means.
    fn admin_hint(
        &mut self,
        slot: u32,
        name: &str,
        for_location: bool,
        force: bool,
        out: &mut dyn EffectSink,
    ) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        let noun = if for_location { "location" } else { "item" };

        // **Gate on the right pool before collecting**, which is what keeps
        // the two verbs actually distinct. `collect_hints_by_name` falls
        // through to a location lookup when an item lookup misses — faithfully,
        // because the reference's `get_hints` ends its chain the same way — and
        // the reference gets away with it by choosing the candidate pool
        // *first* (`MultiServer.py:1728-1731`), so a location name never
        // reaches the fallthrough on an item hint. The chat commands do the
        // same through `fuzzy::intended`. Calling the collector directly, as
        // this used to, skipped that gate: `hint` accepted location names and
        // quietly produced location hints, which is precisely the confusion a
        // separate `hint_location` exists to remove.
        let hintable = self
            .datapackage
            .get(&self.slot_game(key.1))
            .is_some_and(|names| {
                let pool = if for_location {
                    names.location_and_group_names()
                } else {
                    names.item_and_group_names()
                };
                pool.contains(&name)
            });
        // An id addresses its target directly, with no name to gate on — the
        // reference accepts one here too (`MultiServer.py:2443`), and
        // `send_location` already does, so refusing it only here would be an
        // inconsistency a caller has to memorize.
        let hints = match name.parse::<i64>() {
            Ok(id) => self.collect_hints_by_id(key, id, for_location),
            Err(_) if hintable => self.collect_hints_by_name(key, name, for_location),
            Err(_) => Vec::new(),
        };
        if hints.is_empty() {
            return AdminOutcome::refused(format!(
                "Found no hintable {noun} matching \"{name}\" for {}.",
                self.slot_alias(key)
            ));
        }

        if force {
            // Straight past the economy: no points are spent and `hints_used`
            // does not move, because an administrator granting a hint is not the
            // slot buying one. That is the whole of what `force` changes —
            // this is the reference's console `/hint`
            // (`MultiServer.py:2451-2465`), which collects and announces every
            // matching hint with no cost and no one-per-call limit. The
            // limiting an operator might expect belongs to the *client*
            // `!hint`, lives in `get_hints`, and applies only when the hint
            // cost is non-zero; the non-forced branch below is that path.
            //
            // **Both flags default**, exactly as `notify_hints(team, hints)`
            // does there. They used to be `true, true`, which are the
            // *LocationScouts* flags: `only_new` silently dropped every hint
            // the slot already held — so re-running an admin hint announced
            // nothing while still reporting a count — and
            // `persist_even_if_found` banked hints for locations already
            // checked, which the reference stores only for scouts and says so
            // in a comment at `MultiServer.py:822-823`.
            let count = hints.len();
            self.notify_hints(key.0, hints, false, false, None, out);
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
                    "Hinted {granted} {noun}(s) for {}, at their own cost.",
                    self.slot_alias(key)
                ),
                vec![slot],
            )
        }
    }

    /// A location id, from either a numeric id or an exact location name.
    ///
    /// **Exact, not fuzzy**, unlike the chat commands. `!hint_location` runs
    /// what a player typed through `fuzzy::intended` because a person guessing
    /// at a name is the normal case there. This is a typed JSON API whose
    /// caller is a program: a near-miss should be an error it can see, not a
    /// silent decision to act on a different location — and `send_location`
    /// hands out items, which is not something to do on a guess.
    fn resolve_location(&self, key: SlotKey, input: &str) -> Option<i64> {
        if let Ok(id) = input.parse::<i64>() {
            return Some(id);
        }
        let names = self.datapackage.get(&self.slot_game(key.1))?;
        names.location_id(input)
    }

    /// Check a location on a slot's behalf, as though the player had.
    ///
    /// Routed through `register_location_checks` rather than writing the set
    /// directly, so the items that location holds are sent, the check is
    /// announced, `activity_at` moves and the hint statuses update — all the
    /// consequences a real check has. Writing to `location_checks` would look
    /// identical in the tracker and quietly deliver nothing.
    fn admin_send_location(
        &mut self,
        slot: u32,
        location: &str,
        out: &mut dyn EffectSink,
    ) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        let Some(id) = self.resolve_location(key, location) else {
            return AdminOutcome::refused(format!(
                "Found no location matching \"{location}\" for {}.",
                self.slot_alias(key)
            ));
        };

        let before = self.checked_count(key);
        self.register_location_checks(key, &[id], out);
        let checked = self.checked_count(key).saturating_sub(before);
        if checked == 0 {
            // Either already checked or not this slot's location. Both are
            // no-ops rather than errors, and an operator wants to know which.
            return AdminOutcome::refused(format!(
                "{} had nothing to check at \"{location}\"; it is already \
                 checked or does not belong to that slot.",
                self.slot_alias(key)
            ));
        }
        out.mark_dirty();
        AdminOutcome::ok(
            format!("Checked \"{location}\" for {}.", self.slot_alias(key)),
            vec![slot],
        )
    }

    /// Exempt a slot from `release_mode`, or return it to the room's rule.
    ///
    /// **Not a third permission level.** `release_mode` is the room's policy;
    /// this is a per-slot exemption checked *before* it, so `allowed: true`
    /// lets that slot `!release` whatever the mode says. Clearing it does not
    /// forbid releasing — it restores the mode, which may well still permit it.
    /// The reference is the same shape and spells the clear case out as "has to
    /// follow the server restrictions" (`MultiServer.py:2361-2371`); the
    /// response here says so too, because "forbid" reads like a denial.
    ///
    /// There is deliberately no collect equivalent: the reference has none —
    /// `!collect` consults `collect_mode` and nothing else.
    fn admin_allow_release(
        &mut self,
        slot: u32,
        allowed: bool,
        out: &mut dyn EffectSink,
    ) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        self.allow_release(key, allowed);
        out.mark_dirty();

        let who = self.slot_alias(key);
        AdminOutcome::ok(
            if allowed {
                format!("{who} may now use !release at any time.")
            } else {
                format!(
                    "{who} now follows the room's release_mode ({}), which may still allow it.",
                    self.options.release_mode.as_text()
                )
            },
            vec![slot],
        )
    }

    /// Set a slot's completion status on its behalf.
    ///
    /// Routed through the same `set_status` a `StatusUpdate` packet reaches, so
    /// declaring a goal here does everything a client declaring it would: the
    /// room announces it, and `collect_mode`/`release_mode` auto rules fire. A
    /// bare write to `client_game_state` would show the right thing in every
    /// tracker and quietly skip all of that.
    ///
    /// **Goal cannot be undone, including from here.** `MultiServer.py:2208`
    /// guards every status change with `if current != CLIENT_GOAL`, so not even
    /// the client that declared it may take it back, and pahoa keeps that
    /// invariant rather than carving out an operator exception — anything
    /// downstream is entitled to treat goal as monotonic. Where the reference
    /// silently ignores the attempt, this refuses it and says why: an operator
    /// who asked for a change is owed the news that it did not happen.
    fn admin_set_status(
        &mut self,
        slot: u32,
        status: ClientStatus,
        out: &mut dyn EffectSink,
    ) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        let who = self.slot_alias(key);

        if self.status(key) == ClientStatus::Goal {
            return AdminOutcome::refused(format!(
                "{who} has already completed their goal, and that cannot be undone."
            ));
        }

        self.set_status(key, status, out);

        let mut line = format!("{who} is now {}.", status.as_text());
        if status == ClientStatus::Goal {
            // The auto rules are the surprising part, so name them rather than
            // leaving an operator to notice a world emptied out afterwards.
            let mut also = Vec::new();
            if self.options.collect_mode.is_auto() {
                also.push("collected");
            }
            if self.options.release_mode.is_auto() {
                also.push("released");
            }
            if !also.is_empty() {
                line.push_str(&format!(
                    " Their world was automatically {}, as it would be for any goal.",
                    also.join(" and ")
                ));
            }
        } else if matches!(status, ClientStatus::Unknown | ClientStatus::Connected) {
            // Both are connection-derived and will be rewritten by the next
            // connect or disconnect, so setting one is almost never what
            // somebody meant.
            line.push_str(" This is a connection state, so it will be overwritten the next time they connect or disconnect.");
        }
        AdminOutcome::ok(line, vec![slot])
    }

    /// Bar a slot from connecting, or let it back in.
    ///
    /// **Does not disconnect anyone**, and the response says so, because the
    /// obvious reading of "locked" is that the room ejected them. Locking bars
    /// the next login; `kick` ends the current session. An administrator
    /// dealing with a griefer wants both, in that order — kicking first leaves
    /// a window in which they simply reconnect.
    ///
    /// Independent of every password mode: it applies to a room with no
    /// password and to somebody holding the correct one, which is what makes it
    /// usable as the answer to "this person, specifically, is not to come back".
    fn admin_lock(&mut self, slot: u32, locked: bool, out: &mut dyn EffectSink) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        self.lock_slot(key, locked);
        out.mark_dirty();

        let who = self.slot_alias(key);
        let open = self.connections_for(key);
        let line = if locked {
            let mut line = format!("{who} is locked and cannot connect.");
            if open > 0 {
                // The one thing an administrator is most likely to assume
                // wrongly, said at the moment they would assume it.
                line.push_str(&format!(
                    " {open} connection{} still open; locking does not disconnect anyone — use kick for that.",
                    if open == 1 { " is" } else { "s are" }
                ));
            }
            line
        } else {
            format!("{who} is unlocked and may connect again.")
        };
        tracing::info!(slot, locked, "slot lock changed");
        AdminOutcome::ok(line, vec![slot])
    }

    /// Set or clear a slot's alias, which `!alias` only lets a player do for
    /// themselves.
    ///
    /// Truncated the same way the chat command does — the reference takes the
    /// first 16 *characters* and then strips, so a padded name ends up shorter
    /// than 16 rather than being padded out to it.
    fn admin_alias(&mut self, slot: u32, alias: &str, out: &mut dyn EffectSink) -> AdminOutcome {
        let Some(key) = self.admin_key(slot) else {
            return Self::unknown_slot(slot);
        };
        let trimmed: String = alias.chars().take(16).collect();
        let trimmed = trimmed.trim().to_string();

        let line = if trimmed.is_empty() {
            if self.name_aliases.remove(&key).is_none() {
                return AdminOutcome::refused(format!(
                    "{} has no alias to clear.",
                    self.slot_name(key)
                ));
            }
            format!("Cleared the alias for {}.", self.slot_name(key))
        } else {
            self.name_aliases.insert(key, trimmed.clone());
            format!("{} is now known as {trimmed}.", self.slot_name(key))
        };

        out.mark_dirty();
        // Aliases ride in `NetworkPlayer`, so every client needs the new list —
        // the same broadcast `!alias` makes, for the same reason.
        out.broadcast(
            Recipients::All,
            &[ServerPacket::RoomUpdate(Box::new(RoomUpdate {
                players: Some(self.players_package()),
                ..Default::default()
            }))],
        );
        AdminOutcome::ok(line, vec![slot])
    }

    /// Change one of the room's rules.
    ///
    /// Deliberately the *same* code path as `!admin` `/option` — see
    /// [`Room::apply_option`]. The two surfaces differ only in who is trusted
    /// to reach them and where the answer goes.
    fn admin_option(&mut self, name: &str, value: &str, out: &mut dyn EffectSink) -> AdminOutcome {
        match self.apply_option(name, value, "admin API", out) {
            Ok(line) => AdminOutcome::ok(line, Vec::new()),
            Err(lines) => AdminOutcome {
                ok: false,
                output: lines,
                affected_slots: Vec::new(),
            },
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
