//! Where a room's secrets come from, and which source wins.
//!
//! Everything else pahoa is configured with is argv, deliberately. Secrets are
//! the exception, because argv is readable inside the container with `ps` and,
//! more to the point, in `kubectl get pod -o yaml`. An environment variable is
//! *equally* visible there when it is written literally into a pod spec — the
//! win only materializes because the orchestrator sources these from a Secret
//! with `envFrom`, which leaves nothing but a reference in the object.
//!
//! So the environment takes precedence and the flags keep working: pahoa is
//! also a tool someone runs by hand, and removing `--password` would be a
//! breaking change with no upside. A secret that arrives through argv is
//! warned about rather than refused.

use std::collections::BTreeMap;

const PASSWORD: &str = "PAHOA_PASSWORD";
const SERVER_PASSWORD: &str = "PAHOA_SERVER_PASSWORD";
const SLOT_PASSWORDS: &str = "PAHOA_SLOT_PASSWORDS";
const ADMIN_TOKEN: &str = "PAHOA_ADMIN_TOKEN";

/// Below this, a token is not worth calling one.
///
/// The admin surface is mutating and internet-reachable, and the token is the
/// only control on it, so a short one is a misconfiguration rather than a
/// preference. Refused at startup instead of served, because the failure it
/// prevents is silent.
const MIN_ADMIN_TOKEN_BYTES: usize = 32;

/// The resolved set, with whatever should be said about how it was resolved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Secrets {
    pub password: Option<String>,
    pub server_password: Option<String>,
    /// `None` when `PAHOA_SLOT_PASSWORDS` was not set at all. `Some` puts the
    /// room in per-slot mode, in which a slot missing from the map is refused
    /// — so an empty object is a **locked room**, not an unconfigured one.
    pub slot_passwords: Option<BTreeMap<u32, String>>,
    /// Whether the room-wide password came from the environment.
    ///
    /// Needed because `--use-embedded-options` lets a seed's own
    /// `server_options` override the command line, and a password baked into a
    /// seed must not be able to shadow the one the orchestrator configured —
    /// that is the same failure as a password persisted into `room.save`, and
    /// it makes rotation appear to work and then revert. Precedence is
    /// environment, then seed, then argv.
    pub password_from_env: bool,
    pub server_password_from_env: bool,
    /// Bearer token for `/admin/v1/**`. Environment only — there is no flag,
    /// because there is no case for one that outweighs putting it in `ps`.
    /// `None` makes the admin surface answer `404`.
    pub admin_token: Option<String>,
    /// Held rather than logged, because resolution happens before the
    /// subscriber is installed. The caller emits these once it is.
    pub warnings: Vec<String>,
}

/// What the command line supplied, if anything.
#[derive(Debug, Default, Clone, Copy)]
pub struct FromArgv<'a> {
    pub password: Option<&'a str>,
    pub server_password: Option<&'a str>,
}

/// Resolve against the real environment.
pub fn resolve(argv: FromArgv<'_>) -> Result<Secrets, String> {
    merge(argv, |name| std::env::var(name).ok())
}

/// The whole of the policy, with the environment injected so it can be tested
/// without touching the process's own — which edition 2024 makes `unsafe` to
/// mutate, and which parallel tests cannot share anyway.
fn merge(argv: FromArgv<'_>, env: impl Fn(&str) -> Option<String>) -> Result<Secrets, String> {
    let mut warnings = Vec::new();

    // Presence of the variable is what turns the mode on, not the map having
    // entries in it: `{}` means "per-slot mode, nobody holds a key", which is a
    // locked room and a legitimate thing to ask for.
    let slot_passwords = match env(SLOT_PASSWORDS) {
        None => None,
        Some(raw) => Some(parse_slot_passwords(&raw)?),
    };

    let (password, password_from_env) = pick(
        PASSWORD,
        env(PASSWORD),
        argv.password,
        "--password",
        &mut warnings,
    );
    let (server_password, server_password_from_env) = pick(
        SERVER_PASSWORD,
        env(SERVER_PASSWORD),
        argv.server_password,
        "--server-password",
        &mut warnings,
    );

    // Checked after resolution rather than on the environment alone, so that
    // `--password` against `PAHOA_SLOT_PASSWORDS` is caught too. Silently
    // preferring one would give an operator a room that asks for a password
    // they did not configure.
    if password.is_some() && slot_passwords.is_some() {
        return Err(format!(
            "a room-wide password and per-slot passwords are mutually exclusive, \
             but both are set ({SLOT_PASSWORDS}, and a room-wide password from \
             {PASSWORD} or --password). Pick one mode."
        ));
    }

    let admin_token = match env(ADMIN_TOKEN) {
        None => None,
        Some(token) if token.len() < MIN_ADMIN_TOKEN_BYTES => {
            // The length is not a secret and naming it is what makes this
            // actionable; the token itself is never echoed.
            return Err(format!(
                "{ADMIN_TOKEN} is {} bytes, and the admin API needs at least \
                 {MIN_ADMIN_TOKEN_BYTES}. It is the only control on a mutating, \
                 internet-reachable surface, so generate one rather than \
                 choosing one.",
                token.len()
            ));
        }
        Some(token) => Some(token),
    };

    Ok(Secrets {
        password,
        server_password,
        slot_passwords,
        password_from_env,
        server_password_from_env,
        admin_token,
        warnings,
    })
}

/// The environment wins, and either source is worth a word. The flag reports
/// whether the environment is where the value came from.
fn pick(
    name: &str,
    from_env: Option<String>,
    from_argv: Option<&str>,
    flag: &str,
    warnings: &mut Vec<String>,
) -> (Option<String>, bool) {
    match (from_env, from_argv) {
        (Some(value), Some(_)) => {
            warnings.push(format!(
                "{name} and {flag} are both set; using {name}, because the \
                 environment is authoritative for secrets"
            ));
            (Some(value), true)
        }
        (Some(value), None) => (Some(value), true),
        (None, Some(value)) => {
            warnings.push(format!(
                "{flag} puts a secret in this process's argv, where `ps` inside \
                 the container and `kubectl get pod -o yaml` outside it can both \
                 read it; prefer {name}"
            ));
            (Some(value.to_string()), false)
        }
        (None, None) => (None, false),
    }
}

/// `{"1": "…", "7": "…"}` — JSON object keys are strings, so the slot number
/// arrives quoted.
///
/// Deliberately flat and deliberately strict. This is a value an orchestrator
/// renders, and a shape that silently accepted a nested object or a non-numeric
/// key would produce a room where some slots are unexpectedly passwordless.
fn parse_slot_passwords(raw: &str) -> Result<BTreeMap<u32, String>, String> {
    // Never interpolate `raw` into an error: it is entirely secrets.
    let parsed: BTreeMap<String, String> = serde_json::from_str(raw).map_err(|e| {
        format!(
            "{SLOT_PASSWORDS}: expected a flat JSON object mapping a quoted slot \
             number to a password, as {{\"1\": \"…\"}} ({})",
            e
        )
    })?;

    parsed
        .into_iter()
        .map(|(key, password)| {
            key.parse::<u32>()
                .map(|slot| (slot, password))
                .map_err(|_| format!("{SLOT_PASSWORDS}: {key:?} is not a slot number"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in environment, so nothing here touches the process's own.
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn empty() -> impl Fn(&str) -> Option<String> {
        |_: &str| None
    }

    #[test]
    fn nothing_configured_is_a_passwordless_room() {
        let s = merge(FromArgv::default(), empty()).unwrap();
        assert_eq!(s, Secrets::default());
    }

    #[test]
    fn the_environment_beats_the_flag_and_says_so() {
        let s = merge(
            FromArgv {
                password: Some("from-argv"),
                server_password: None,
            },
            env(&[("PAHOA_PASSWORD", "from-env")]),
        )
        .unwrap();
        assert_eq!(s.password.as_deref(), Some("from-env"));
        assert_eq!(s.warnings.len(), 1);
        assert!(s.warnings[0].contains("PAHOA_PASSWORD"));
    }

    /// The flag still works — pahoa is also a tool someone runs by hand — but
    /// it is worth a word, because that is a secret in `ps`.
    #[test]
    fn a_flag_alone_is_honored_with_a_warning() {
        let s = merge(
            FromArgv {
                password: Some("hunter2"),
                server_password: Some("admin"),
            },
            empty(),
        )
        .unwrap();
        assert_eq!(s.password.as_deref(), Some("hunter2"));
        assert_eq!(s.server_password.as_deref(), Some("admin"));
        assert_eq!(s.warnings.len(), 2);
    }

    #[test]
    fn the_environment_alone_is_silent() {
        let s = merge(
            FromArgv::default(),
            env(&[
                ("PAHOA_PASSWORD", "quiet"),
                ("PAHOA_SERVER_PASSWORD", "also-quiet"),
            ]),
        )
        .unwrap();
        assert_eq!(s.password.as_deref(), Some("quiet"));
        assert!(s.warnings.is_empty(), "{:?}", s.warnings);
    }

    #[test]
    fn slot_passwords_parse_with_their_numbers_unquoted() {
        let s = merge(
            FromArgv::default(),
            env(&[(
                "PAHOA_SLOT_PASSWORDS",
                r#"{"1": "quiet-harbor-ledger", "7": "amber-ferry-quartz"}"#,
            )]),
        )
        .unwrap();
        let map = s.slot_passwords.as_ref().expect("per-slot mode is on");
        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&1).map(String::as_str), Some("quiet-harbor-ledger"));
        assert_eq!(map.get(&7).map(String::as_str), Some("amber-ferry-quartz"));
        // Slots absent from the object have none.
        assert!(!map.contains_key(&2));
    }

    #[test]
    fn the_two_password_modes_are_refused_together() {
        let both = merge(
            FromArgv::default(),
            env(&[
                ("PAHOA_PASSWORD", "room-wide"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1": "per-slot"}"#),
            ]),
        );
        assert!(both.is_err());
        assert!(both.unwrap_err().contains("mutually exclusive"));
    }

    /// The same conflict, arriving half by flag. Checking the environment alone
    /// would let this one through.
    #[test]
    fn a_flag_password_conflicts_with_environment_slot_passwords() {
        let both = merge(
            FromArgv {
                password: Some("room-wide"),
                server_password: None,
            },
            env(&[("PAHOA_SLOT_PASSWORDS", r#"{"1": "per-slot"}"#)]),
        );
        assert!(both.is_err());
    }

    /// A server password is a third, orthogonal thing — it gates `!admin`, not
    /// joining — so it never conflicts with either mode.
    #[test]
    fn a_server_password_coexists_with_per_slot_passwords() {
        let s = merge(
            FromArgv::default(),
            env(&[
                ("PAHOA_SERVER_PASSWORD", "admin"),
                ("PAHOA_SLOT_PASSWORDS", r#"{"1": "per-slot"}"#),
            ]),
        )
        .unwrap();
        assert_eq!(s.server_password.as_deref(), Some("admin"));
        assert_eq!(s.slot_passwords.as_ref().map(|p| p.len()), Some(1));
    }

    #[test]
    fn a_malformed_slot_object_is_refused_without_echoing_it() {
        for bad in [
            r#"["not", "an", "object"]"#,
            r#"{"1": {"nested": "no"}}"#,
            "not json at all",
            r#"{"1": 12345}"#,
        ] {
            let e = merge(FromArgv::default(), env(&[("PAHOA_SLOT_PASSWORDS", bad)]))
                .expect_err("should be refused");
            assert!(e.contains("PAHOA_SLOT_PASSWORDS"), "{e}");
            assert!(
                !e.contains("no") || !e.contains("nested"),
                "the error must not quote the value back: {e}"
            );
        }
    }

    #[test]
    fn a_key_that_is_not_a_slot_number_is_refused() {
        let e = merge(
            FromArgv::default(),
            env(&[("PAHOA_SLOT_PASSWORDS", r#"{"Troy": "by-name-no"}"#)]),
        )
        .expect_err("should be refused");
        assert!(e.contains("not a slot number"), "{e}");
        assert!(!e.contains("by-name-no"), "leaked the password: {e}");
    }

    /// An empty object parses, and means per-slot mode with nobody holding a
    /// key — a **locked room**, since the mode fails closed. That is a
    /// legitimate thing to ask for and a dangerous thing to render by accident,
    /// so it is worth pinning.
    #[test]
    fn an_empty_slot_object_is_per_slot_mode_with_nobody_in_it() {
        let s = merge(FromArgv::default(), env(&[("PAHOA_SLOT_PASSWORDS", "{}")])).unwrap();
        let map = s.slot_passwords.as_ref().expect("the mode is on");
        assert!(map.is_empty(), "and nobody holds a key");
    }

    /// Absence of the variable is what turns the mode off, not an empty map.
    #[test]
    fn no_variable_means_no_per_slot_mode() {
        let s = merge(FromArgv::default(), empty()).unwrap();
        assert!(s.slot_passwords.is_none());
    }
}
