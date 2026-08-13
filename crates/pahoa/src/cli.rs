//! Command-line parsing.
//!
//! Hand-rolled rather than `clap`: the flag set is small and static, and a
//! `scratch`-image binary is worth keeping thin. What is not optional is
//! **rejecting what it does not understand**. The parser this replaces looked
//! for a flag by name and took the token after it, which meant a misspelled
//! `--save-dirr` was silently ignored and started a room that persisted
//! nothing — the failure an operator finds out about after the restart.
//!
//! So: unknown options are an error, `--flag=value` works, options may come
//! before the positional, and a flag given twice is a mistake rather than a
//! last-one-wins race.

use std::collections::HashMap;

/// One accepted option.
pub struct Opt {
    /// Canonical spelling, kebab-case, as the help text lists it.
    pub name: &'static str,
    /// Also accepted, and not advertised. The reference server spells its
    /// equivalents with underscores (`--hint_cost`), and anyone arriving from
    /// it will type those.
    pub aliases: &'static [&'static str],
    /// Whether it consumes a following token.
    pub takes_value: bool,
}

pub const fn flag(name: &'static str, aliases: &'static [&'static str]) -> Opt {
    Opt {
        name,
        aliases,
        takes_value: false,
    }
}

pub const fn value(name: &'static str, aliases: &'static [&'static str]) -> Opt {
    Opt {
        name,
        aliases,
        takes_value: true,
    }
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub positional: Vec<String>,
    values: HashMap<&'static str, String>,
    flags: Vec<&'static str>,
}

impl Parsed {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn is_set(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    /// Parse an option's value, naming the flag in the error.
    ///
    /// `"invalid digit found in string"` on its own tells an operator nothing
    /// about which of a dozen flags they got wrong.
    pub fn number<T: std::str::FromStr>(&self, name: &str) -> Result<Option<T>, String> {
        match self.get(name) {
            None => Ok(None),
            Some(v) => v
                .parse()
                .map(Some)
                .map_err(|_| format!("{name}: expected a number, got {v:?}")),
        }
    }
}

pub fn parse(argv: &[String], spec: &[Opt]) -> Result<Parsed, String> {
    let mut out = Parsed::default();
    let mut i = 0;
    let mut rest_is_positional = false;

    while i < argv.len() {
        let arg = &argv[i];
        i += 1;

        if rest_is_positional || !arg.starts_with('-') || arg == "-" {
            out.positional.push(arg.clone());
            continue;
        }
        if arg == "--" {
            rest_is_positional = true;
            continue;
        }

        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (arg.as_str(), None),
        };
        let opt = spec
            .iter()
            .find(|o| o.name == name || o.aliases.contains(&name))
            .ok_or_else(|| unknown(name, spec))?;

        if opt.takes_value {
            // Deliberately does not care whether the value looks like a flag:
            // `--password --weird` is a legal password, and refusing it would
            // be a guess about intent. A missing value shows up as a parse
            // error on the flag that swallowed the next one.
            let v = match inline {
                Some(v) => v.to_string(),
                None => {
                    let v = argv
                        .get(i)
                        .ok_or_else(|| format!("{} needs a value", opt.name))?;
                    i += 1;
                    v.clone()
                }
            };
            if out.values.insert(opt.name, v).is_some() {
                return Err(format!("{} given more than once", opt.name));
            }
        } else {
            if inline.is_some() {
                return Err(format!("{} takes no value", opt.name));
            }
            if !out.flags.contains(&opt.name) {
                out.flags.push(opt.name);
            }
        }
    }

    Ok(out)
}

fn unknown(name: &str, spec: &[Opt]) -> String {
    let mut msg = format!("unknown option {name:?}");
    if let Some(guess) = closest(name, spec) {
        msg.push_str(&format!(" — did you mean {guess}?"));
        return msg;
    }
    let mut names: Vec<&str> = spec.iter().map(|o| o.name).collect();
    names.sort_unstable();
    msg.push_str(&format!("\naccepted here: {}", names.join(" ")));
    msg
}

/// The option `name` was probably meant to be, if one stands out.
///
/// Aliases are searched too but the canonical spelling is what gets suggested,
/// so a near-miss on `--hint_cost` still points at `--hint-cost`.
fn closest(name: &str, spec: &[Opt]) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for opt in spec {
        let d = std::iter::once(opt.name)
            .chain(opt.aliases.iter().copied())
            .map(|candidate| distance(name, candidate))
            .min()
            .unwrap_or(usize::MAX);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, opt.name));
        }
    }
    // Close enough to be a typo rather than a different word: a couple of
    // edits, and not more than a third of what was typed.
    best.filter(|&(d, _)| d <= 2 && d * 3 <= name.len())
        .map(|(_, n)| n)
}

/// Levenshtein distance, only ever run on a failing command line.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &[Opt] = &[
        flag("--help", &["-h"]),
        value("--port", &[]),
        value("--save-dir", &[]),
        value("--hint-cost", &["--hint_cost"]),
        flag("--no-item-cheat", &["--disable_item_cheat"]),
    ];

    fn run(args: &[&str]) -> Result<Parsed, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&owned, SPEC)
    }

    #[test]
    fn takes_values_either_spelling() {
        let p = run(&["--port", "38281", "--save-dir=/tmp/room"]).unwrap();
        assert_eq!(p.get("--port"), Some("38281"));
        assert_eq!(p.get("--save-dir"), Some("/tmp/room"));
    }

    #[test]
    fn options_may_precede_the_positional() {
        let p = run(&["--port", "1", "seed.archipelago"]).unwrap();
        assert_eq!(p.positional, ["seed.archipelago"]);
        assert_eq!(p.get("--port"), Some("1"));
    }

    #[test]
    fn aliases_resolve_to_the_canonical_name() {
        let p = run(&["--hint_cost", "20", "--disable_item_cheat"]).unwrap();
        assert_eq!(p.get("--hint-cost"), Some("20"));
        assert!(p.is_set("--no-item-cheat"));
    }

    /// The bug this parser exists to fix.
    #[test]
    fn an_unknown_option_is_an_error() {
        let e = run(&["--save-dirr", "/tmp/room"]).unwrap_err();
        assert!(e.contains("--save-dirr"), "{e}");
        assert!(e.contains("--save-dir"), "{e}");
    }

    #[test]
    fn a_wild_miss_lists_what_is_accepted_instead_of_guessing() {
        let e = run(&["--quorum"]).unwrap_err();
        assert!(e.contains("accepted here"), "{e}");
        assert!(!e.contains("did you mean"), "{e}");
    }

    #[test]
    fn a_missing_value_is_an_error() {
        assert!(run(&["--port"]).unwrap_err().contains("needs a value"));
    }

    #[test]
    fn a_repeated_option_is_an_error() {
        let e = run(&["--port", "1", "--port", "2"]).unwrap_err();
        assert!(e.contains("more than once"), "{e}");
    }

    #[test]
    fn a_boolean_flag_refuses_a_value() {
        assert!(
            run(&["--no-item-cheat=yes"])
                .unwrap_err()
                .contains("takes no value")
        );
    }

    #[test]
    fn a_value_may_look_like_a_flag() {
        // A password is whatever the operator says it is.
        let p = run(&["--port=--8", "--save-dir", "-h"]).unwrap();
        assert_eq!(p.get("--port"), Some("--8"));
        assert_eq!(p.get("--save-dir"), Some("-h"));
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        let p = run(&["--", "--port", "seed"]).unwrap();
        assert_eq!(p.positional, ["--port", "seed"]);
        assert_eq!(p.get("--port"), None);
    }

    #[test]
    fn numbers_name_the_flag_when_they_fail() {
        let p = run(&["--port", "eight"]).unwrap();
        let e = p.number::<u16>("--port").unwrap_err();
        assert!(e.contains("--port") && e.contains("eight"), "{e}");
        assert_eq!(p.number::<u16>("--save-dir").unwrap(), None);
    }
}
