//! Parsing `POST /admin/v1/command`.
//!
//! A tagged enum rather than a command line. Feeding the existing `!` dispatcher
//! a string would have looked cheaper and self-syncing, but every one of those
//! handlers still needs a target supplied from outside — they are
//! connection-scoped — so most of the saving evaporates, and the caller's UI
//! degrades to a text box with no validation and no slot picker. A typed set
//! also makes an unknown command a `400` rather than a confusing text reply.
//!
//! Hand-parsed from `serde_json::Value` rather than derived, so that a missing
//! field names itself. `{"command":"hint","item":"Sword"}` should say which
//! field it wants, not "invalid type".

use pahoa_room::AdminCommand;
use serde_json::Value;

/// Parse a request body into a command, or explain what is wrong with it.
pub fn parse(body: &[u8]) -> Result<AdminCommand, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| format!("the body is not JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "expected a JSON object".to_string())?;

    let command = object
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing \"command\"".to_string())?;

    match command {
        "status" => Ok(AdminCommand::Status),
        "say" => Ok(AdminCommand::Say {
            text: text(object, "text")?,
        }),
        "countdown" => Ok(AdminCommand::Countdown {
            seconds: integer(object, "seconds")?,
        }),
        "release" => Ok(AdminCommand::Release {
            slot: slot(object)?,
        }),
        "collect" => Ok(AdminCommand::Collect {
            slot: slot(object)?,
        }),
        "send_item" => Ok(AdminCommand::SendItem {
            slot: slot(object)?,
            item: text(object, "item")?,
        }),
        // A separate verb rather than a count on `send_item`, mirroring the
        // reference's naming — and `send_item` stays the one-copy spelling so
        // the common case needs no count at all.
        "send_multiple" => Ok(AdminCommand::SendMultiple {
            slot: slot(object)?,
            item: text(object, "item")?,
            amount: integer(object, "amount")?,
        }),
        "hint" => Ok(AdminCommand::Hint {
            slot: slot(object)?,
            item: text(object, "item")?,
            force: force(object)?,
        }),
        // A separate verb rather than a flag on `hint`, because the reference
        // names it separately and an operator who knows `/hint_location` looks
        // for that word.
        "hint_location" => Ok(AdminCommand::HintLocation {
            slot: slot(object)?,
            location: text(object, "location")?,
            force: force(object)?,
        }),
        "send_location" => Ok(AdminCommand::SendLocation {
            slot: slot(object)?,
            location: text(object, "location")?,
        }),
        // One verb with a boolean rather than the reference's two, because
        // `forbid_release` does not forbid anything — it clears an exemption.
        // A caller reading `{"allowed": false}` is far less likely to expect a
        // denial than one reading `forbid_release`.
        "allow_release" => Ok(AdminCommand::AllowRelease {
            slot: slot(object)?,
            allowed: object.get("allowed").map_or(Ok(true), |v| {
                v.as_bool()
                    .ok_or_else(|| "\"allowed\" must be true or false".to_string())
            })?,
        }),
        "alias" => Ok(AdminCommand::Alias {
            slot: slot(object)?,
            // Optional and empty-meaning-clear, matching `!alias` with no
            // argument.
            alias: match object.get("alias") {
                None | Some(Value::Null) => String::new(),
                Some(v) => v
                    .as_str()
                    .ok_or_else(|| "\"alias\" must be a string".to_string())?
                    .to_string(),
            },
        }),
        "option" => Ok(AdminCommand::Option {
            name: text(object, "name")?,
            // Numbers and booleans are accepted as themselves rather than
            // insisting on a quoted string: `{"name":"hint_cost","value":20}`
            // is what a caller building JSON will write, and the option layer
            // parses from text anyway.
            value: match object.get("value") {
                Some(Value::String(s)) => s.clone(),
                Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
                Some(_) => return Err("\"value\" must be a string, number or boolean".to_string()),
                None => return Err("missing \"value\"".to_string()),
            },
        }),
        "kick" => Ok(AdminCommand::Kick {
            slot: slot(object)?,
            // Optional: kicking without a stated reason is allowed, and the
            // client simply is not told one.
            reason: match object.get("reason") {
                None | Some(Value::Null) => String::new(),
                Some(v) => v
                    .as_str()
                    .ok_or_else(|| "\"reason\" must be a string".to_string())?
                    .to_string(),
            },
        }),
        other => Err(format!("unknown command {other:?}")),
    }
}

/// Parse the body of `POST /admin/v1/slots/<n>/password`.
///
/// `{"password": "…"}` sets one; `{"password": null}` clears it, which is how a
/// slot is returned to having none. An absent key is the same as null, so an
/// empty body is a clear rather than an error — there is no other thing it
/// could reasonably mean.
pub fn slot_password(body: &[u8]) -> Result<Option<String>, String> {
    if body.is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|e| format!("the body is not JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "expected a JSON object".to_string())?;

    match object.get("password") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(password)) => Ok(Some(password.clone())),
        Some(_) => Err("\"password\" must be a string, or null to clear it".to_string()),
    }
}

type Object = serde_json::Map<String, Value>;

fn text(object: &Object, field: &str) -> Result<String, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("{field:?} is required and must be a string"))
}

fn integer(object: &Object, field: &str) -> Result<i64, String> {
    object
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("{field:?} is required and must be a whole number"))
}

fn slot(object: &Object) -> Result<u32, String> {
    let raw = integer(object, "slot")?;
    u32::try_from(raw).map_err(|_| format!("{raw} is not a slot number"))
}

/// Optional, defaulting to the safer behavior: a caller who does not say should
/// not be silently overriding the hint economy.
fn force(object: &Object) -> Result<bool, String> {
    object.get("force").map_or(Ok(false), |v| {
        v.as_bool()
            .ok_or_else(|| "\"force\" must be true or false".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(json: &str) -> Result<AdminCommand, String> {
        parse(json.as_bytes())
    }

    #[test]
    fn every_command_in_the_contract_parses() {
        assert_eq!(
            parsed(r#"{"command":"status"}"#).unwrap(),
            AdminCommand::Status
        );
        assert_eq!(
            parsed(r#"{"command":"say","text":"hello"}"#).unwrap(),
            AdminCommand::Say {
                text: "hello".into()
            }
        );
        assert_eq!(
            parsed(r#"{"command":"countdown","seconds":10}"#).unwrap(),
            AdminCommand::Countdown { seconds: 10 }
        );
        assert_eq!(
            parsed(r#"{"command":"release","slot":3}"#).unwrap(),
            AdminCommand::Release { slot: 3 }
        );
        assert_eq!(
            parsed(r#"{"command":"collect","slot":3}"#).unwrap(),
            AdminCommand::Collect { slot: 3 }
        );
        assert_eq!(
            parsed(r#"{"command":"send_item","slot":3,"item":"Lamp"}"#).unwrap(),
            AdminCommand::SendItem {
                slot: 3,
                item: "Lamp".into()
            }
        );
        assert_eq!(
            parsed(r#"{"command":"hint","slot":3,"item":"Progressive Sword","force":false}"#)
                .unwrap(),
            AdminCommand::Hint {
                slot: 3,
                item: "Progressive Sword".into(),
                force: false
            }
        );
        assert_eq!(
            parsed(r#"{"command":"kick","slot":3,"reason":"afk"}"#).unwrap(),
            AdminCommand::Kick {
                slot: 3,
                reason: "afk".into()
            }
        );
        assert_eq!(
            parsed(r#"{"command":"send_multiple","slot":3,"item":"Rupee","amount":5}"#).unwrap(),
            AdminCommand::SendMultiple {
                slot: 3,
                item: "Rupee".into(),
                amount: 5
            }
        );
        assert_eq!(
            parsed(r#"{"command":"hint_location","slot":3,"location":"Attic","force":true}"#)
                .unwrap(),
            AdminCommand::HintLocation {
                slot: 3,
                location: "Attic".into(),
                force: true
            }
        );
        assert_eq!(
            parsed(r#"{"command":"send_location","slot":3,"location":"Attic"}"#).unwrap(),
            AdminCommand::SendLocation {
                slot: 3,
                location: "Attic".into()
            }
        );
        assert_eq!(
            parsed(r#"{"command":"alias","slot":3,"alias":"Organizer"}"#).unwrap(),
            AdminCommand::Alias {
                slot: 3,
                alias: "Organizer".into()
            }
        );
        assert_eq!(
            parsed(r#"{"command":"option","name":"hint_cost","value":"20"}"#).unwrap(),
            AdminCommand::Option {
                name: "hint_cost".into(),
                value: "20".into()
            }
        );
    }

    /// `amount` is required rather than defaulting to one — a caller reaching
    /// for `send_multiple` means to send several, and a silent default of one
    /// would look like a working command that did a fraction of the job.
    #[test]
    fn send_multiple_requires_an_amount() {
        assert!(
            parsed(r#"{"command":"send_multiple","slot":1,"item":"Rupee"}"#)
                .is_err_and(|e| e.contains("amount"))
        );
    }

    /// `hint` and `hint_location` name different fields, so a caller cannot
    /// send one thinking it is the other and have it half-work.
    #[test]
    fn the_two_hint_verbs_want_different_fields() {
        let wrong_field = parsed(r#"{"command":"hint_location","slot":1,"item":"Lamp"}"#);
        assert!(
            wrong_field.is_err_and(|e| e.contains("location")),
            "a hint_location without a location should say which field it wants"
        );
    }

    /// `allowed` defaults to *granting* the exemption, because `allow_release`
    /// with no argument is the reference's `/allow_release`.
    #[test]
    fn allow_release_defaults_to_allowing() {
        assert_eq!(
            parsed(r#"{"command":"allow_release","slot":2}"#).unwrap(),
            AdminCommand::AllowRelease {
                slot: 2,
                allowed: true
            }
        );
        assert_eq!(
            parsed(r#"{"command":"allow_release","slot":2,"allowed":false}"#).unwrap(),
            AdminCommand::AllowRelease {
                slot: 2,
                allowed: false
            }
        );
    }

    /// An omitted alias clears it, matching `!alias` with no argument.
    #[test]
    fn an_omitted_alias_clears_it() {
        assert_eq!(
            parsed(r#"{"command":"alias","slot":2}"#).unwrap(),
            AdminCommand::Alias {
                slot: 2,
                alias: String::new()
            }
        );
    }

    /// A caller building JSON writes `"value": 20`, not `"value": "20"`. The
    /// option layer parses from text either way, so insisting on a quoted
    /// number would be a papercut with nothing behind it.
    #[test]
    fn an_option_value_may_be_a_number_or_a_boolean() {
        assert_eq!(
            parsed(r#"{"command":"option","name":"hint_cost","value":20}"#).unwrap(),
            AdminCommand::Option {
                name: "hint_cost".into(),
                value: "20".into()
            }
        );
        assert_eq!(
            parsed(r#"{"command":"option","name":"item_cheat","value":false}"#).unwrap(),
            AdminCommand::Option {
                name: "item_cheat".into(),
                value: "false".into()
            }
        );
        assert!(parsed(r#"{"command":"option","name":"hint_cost","value":[1]}"#).is_err());
    }

    /// `force` defaults to the behavior that respects the hint economy.
    #[test]
    fn force_defaults_to_false() {
        assert_eq!(
            parsed(r#"{"command":"hint","slot":1,"item":"Lamp"}"#).unwrap(),
            AdminCommand::Hint {
                slot: 1,
                item: "Lamp".into(),
                force: false
            }
        );
    }

    #[test]
    fn a_kick_needs_no_reason() {
        assert_eq!(
            parsed(r#"{"command":"kick","slot":1}"#).unwrap(),
            AdminCommand::Kick {
                slot: 1,
                reason: String::new()
            }
        );
    }

    #[test]
    fn an_unknown_command_names_itself() {
        let e = parsed(r#"{"command":"explode","slot":1}"#).unwrap_err();
        assert!(e.contains("unknown command"), "{e}");
        assert!(e.contains("explode"), "{e}");
    }

    /// The point of hand-parsing: the error says which field.
    #[test]
    fn a_missing_field_is_named() {
        let e = parsed(r#"{"command":"hint","slot":1}"#).unwrap_err();
        assert!(e.contains("item"), "{e}");

        let e = parsed(r#"{"command":"release"}"#).unwrap_err();
        assert!(e.contains("slot"), "{e}");

        let e = parsed(r#"{"command":"say"}"#).unwrap_err();
        assert!(e.contains("text"), "{e}");
    }

    #[test]
    fn a_field_of_the_wrong_type_is_refused() {
        assert!(parsed(r#"{"command":"release","slot":"three"}"#).is_err());
        assert!(parsed(r#"{"command":"say","text":42}"#).is_err());
        assert!(parsed(r#"{"command":"hint","slot":1,"item":"x","force":"yes"}"#).is_err());
    }

    /// A negative slot is not a slot, and `as_i64` would happily produce one.
    #[test]
    fn a_negative_slot_is_refused() {
        let e = parsed(r#"{"command":"release","slot":-1}"#).unwrap_err();
        assert!(e.contains("not a slot number"), "{e}");
    }

    #[test]
    fn a_body_that_is_not_a_json_object_is_refused() {
        assert!(parsed("not json").is_err());
        assert!(parsed("[1,2,3]").is_err());
        assert!(parsed(r#""a string""#).is_err());
        assert!(parsed("{}").unwrap_err().contains("command"));
    }
}
