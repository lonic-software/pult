//! One manifest command's argument surface, shared by `pult <command>` and
//! `pult x <source> <command>` so the same argv tail binds identically under
//! both: pick, input and an arg-less repeat take positional slots in
//! declaration order (the repeat last, taking every trailing value), while a
//! flag is the switch `--<name>` and a repeat with an `arg` is the
//! repeatable option `--<name> <value>`.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use clap::{Arg, ArgAction, ArgMatches};
use indexmap::IndexMap;

use crate::machine;
use crate::manifest::{OptionDef, ParamDef, ParamKind};
use crate::resolver::ResolvedCommand;
use crate::value::Value;

/// The clap args for `cmd`'s params. The resolver has already refused the
/// names that would collide with pult's own options.
pub fn args(cmd: &ResolvedCommand) -> Vec<Arg> {
    let mut slot = 0;
    cmd.params
        .iter()
        .map(|(name, def)| {
            let arg = Arg::new(name.clone()).value_name(name.to_uppercase());
            let arg = if def.is_positional() {
                slot += 1;
                arg.index(slot).required(false)
            } else {
                arg.long(name.clone())
            };
            match def.kind() {
                ParamKind::Pick(pick) => match &pick.options {
                    Some(opts) => {
                        let values: Vec<&str> = opts.iter().map(OptionDef::value).collect();
                        arg.help(format!("one of: {}", values.join(", ")))
                    }
                    None => arg.help("picked from a dynamic option source if omitted"),
                },
                ParamKind::Input(input) if input.secret => {
                    arg.help("secret; prompted without echo if omitted")
                }
                ParamKind::Input(_) => arg.help("free text; prompted if omitted"),
                ParamKind::Flag(_) => arg
                    .action(ArgAction::SetTrue)
                    .help("switch; off if omitted"),
                ParamKind::Repeat(repeat) => {
                    let arg = arg.action(ArgAction::Append).help(repeat_help(repeat));
                    if def.is_positional() {
                        arg.num_args(1..)
                    } else {
                        arg.num_args(1)
                    }
                }
                ParamKind::Use(_) => unreachable!("resolver inlines every use: param"),
            }
        })
        .collect()
}

fn repeat_help(repeat: &crate::manifest::RepeatDef) -> String {
    let mut help = match (repeat.min, repeat.max) {
        (0, Some(1)) => "optional".to_string(),
        (0, None) => "any number of values".to_string(),
        (min, None) => format!("at least {min} value(s)"),
        (min, Some(max)) => format!("{min} to {max} value(s)"),
    };
    if repeat.arg.is_some() {
        help.push_str("; repeat the option for each");
    }
    if repeat.secret {
        help.push_str("; secret");
    }
    help
}

/// The values `matches` supplies for `cmd`, typed by kind. A param the
/// command line leaves out is absent, an unset switch included, so `fill`
/// decides what absence means.
pub fn provided(cmd: &ResolvedCommand, matches: &ArgMatches) -> HashMap<String, Value> {
    let mut provided = HashMap::new();
    for (name, def) in &cmd.params {
        let value = match def.kind() {
            ParamKind::Flag(_) => matches.get_flag(name).then_some(Value::Flag(true)),
            ParamKind::Repeat(_) => matches
                .get_many::<String>(name)
                .map(|items| Value::List(items.cloned().collect())),
            _ => matches.get_one::<String>(name).cloned().map(Value::Text),
        };
        if let Some(value) = value {
            provided.insert(name.clone(), value);
        }
    }
    provided
}

/// Name a JSON value's type for an error message, never its content.
/// `--params-json` (below) is the mechanism `docs/reference.md` recommends
/// specifically for passing a secret without putting it in argv — so a
/// tooling bug that sends the wrong type (`{"token": ["hunter2-…"]}`, or a
/// bare number) must not print that value back out on the very path chosen
/// to avoid exposure. There's no name-based slot to redact by here (this
/// runs before `provided` even exists as param values) and no masker yet
/// either (that's built post-`fill`, and this rejects the input before
/// `fill` ever runs) — the only fix available at this site is to never
/// format the value at all.
fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Merge param values read from stdin (as a JSON object) into `provided`,
/// which is already populated from the command line. Kept as a pure function
/// so the parsing/merge rules are unit-testable without an actual stdin pipe.
///
/// Rules: stdin must be a JSON object; each value has its param's type — a
/// string for a pick or an input, a boolean for a flag, an array of strings
/// for a repeat — and any other type is a usage error naming the param and
/// the type it needs; every key must be a param this command declares (typo
/// safety — the error names the valid ones); and a param supplied both on
/// the command line and via JSON is a conflict, not a silent override.
pub fn merge_params_json(
    raw: &str,
    declared: &IndexMap<String, ParamDef>,
    provided: &mut HashMap<String, Value>,
) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("--params-json: stdin is not valid JSON")?;
    let obj = value
        .as_object()
        .context("--params-json: stdin must be a JSON object of param name -> value")?;
    for (key, v) in obj {
        let Some(def) = declared.get(key) else {
            let known: Vec<&str> = declared.keys().map(String::as_str).collect();
            bail!(
                "--params-json: unknown param `{key}` — this command declares: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            );
        };
        let (typed, expected) = match def.kind() {
            ParamKind::Flag(_) => (v.as_bool().map(Value::Flag), "a boolean"),
            ParamKind::Repeat(_) => (
                v.as_array().and_then(|items| {
                    items
                        .iter()
                        .map(|i| i.as_str().map(str::to_string))
                        .collect::<Option<Vec<_>>>()
                        .map(Value::List)
                }),
                "an array of strings",
            ),
            _ => (v.as_str().map(|s| Value::Text(s.to_string())), "a string"),
        };
        let Some(typed) = typed else {
            return Err(machine::usage(format!(
                "--params-json: param `{key}` must be {expected}, got {}",
                json_type_name(v)
            )));
        };
        if provided.contains_key(key) {
            bail!(
                "param `{key}` was given both on the command line and via --params-json — pick one"
            );
        }
        provided.insert(key.clone(), typed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{manifest, resolver};

    /// A command's declared params — going through a real manifest instead
    /// of hand-building `ParamDef`s keeps the fixture honest with what the
    /// resolver actually produces.
    fn declared_params(yaml: &str) -> IndexMap<String, ParamDef> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pult.yaml"), yaml).unwrap();
        let loaded = manifest::load(&dir.path().join("pult.yaml")).unwrap();
        let resolved = resolver::resolve(loaded).unwrap();
        resolved.commands[0].params.clone()
    }

    const TOKEN_CMD: &str = "version: 1\ncommands:\n  - id: c\n    title: C\n    params:\n      \
         token: { input: { secret: true } }\n      region: { input: { default: eu } }\n    run: \"true\"\n";

    #[test]
    fn merge_params_json_happy_path() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        provided.insert("region".to_string(), Value::Text("us".to_string()));
        merge_params_json(r#"{"token":"hunter2"}"#, &declared, &mut provided).unwrap();
        assert_eq!(provided["token"], Value::Text("hunter2".to_string()));
        assert_eq!(provided["region"], Value::Text("us".to_string()));
    }

    #[test]
    fn merge_params_json_unknown_key_names_valid_params() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_params_json(r#"{"toekn":"x"}"#, &declared, &mut provided).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown param `toekn`"), "got: {msg}");
        assert!(
            msg.contains("token"),
            "should list valid params, got: {msg}"
        );
    }

    #[test]
    fn merge_params_json_conflict_with_the_command_line_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        provided.insert("token".to_string(), Value::Text("positional".to_string()));
        let err =
            merge_params_json(r#"{"token":"hunter2"}"#, &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("both on the command line and via --params-json"),
            "got: {err:#}"
        );
    }

    #[test]
    fn merge_params_json_non_string_value_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_params_json(r#"{"token": 123}"#, &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a string"),
            "got: {err:#}"
        );
    }

    /// Follow-up review finding on #12's fix, same class, a fifth site:
    /// `--params-json` is the mechanism `docs/reference.md` recommends
    /// specifically for passing a secret without putting it in argv, so a
    /// non-string value must never be echoed back on this path — a token
    /// wrapped in an array by a tooling bug, say, must render as a *type*
    /// name, not the array's contents. Reddens if `json_type_name` is
    /// dropped in favor of `format!("... got {v}")`.
    #[test]
    fn merge_params_json_non_string_value_error_names_the_type_not_the_value() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let secret = "hunter2-super-secret";
        let err = merge_params_json(
            &format!(r#"{{"token": ["{secret}"]}}"#),
            &declared,
            &mut provided,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains(secret), "leaked: {msg}");
        assert!(msg.contains("must be a string"), "got: {msg}");
        assert!(msg.contains("an array"), "got: {msg}");
    }

    #[test]
    fn merge_params_json_invalid_json_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_params_json("not json", &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("not valid JSON"),
            "got: {err:#}"
        );
    }

    #[test]
    fn merge_params_json_non_object_json_errors() {
        let declared = declared_params(TOKEN_CMD);
        let mut provided = HashMap::new();
        let err = merge_params_json(r#"["token"]"#, &declared, &mut provided).unwrap_err();
        assert!(
            format!("{err:#}").contains("must be a JSON object"),
            "got: {err:#}"
        );
    }

    const KINDS_CMD: &str = "version: 1\ncommands:\n  - id: c\n    title: C\n    params:\n      \
         dry: { flag: }\n      only: { repeat: { arg: --only } }\n      env: { input: {} }\n    run: \"true\"\n";

    /// Reddens if params-json stops typing values by kind.
    #[test]
    fn merge_params_json_types_values_by_kind() {
        let declared = declared_params(KINDS_CMD);
        let mut provided = HashMap::new();
        merge_params_json(
            r#"{"dry": true, "only": ["a", ""], "env": "dev"}"#,
            &declared,
            &mut provided,
        )
        .unwrap();
        assert_eq!(provided["dry"], Value::Flag(true));
        assert_eq!(
            provided["only"],
            Value::List(vec!["a".into(), String::new()])
        );
        assert_eq!(provided["env"], Value::Text("dev".into()));
    }

    /// A wrong type is a usage error naming the param and the type it needs.
    #[test]
    fn merge_params_json_wrong_type_is_a_usage_error() {
        let declared = declared_params(KINDS_CMD);
        for (raw, needs) in [
            (r#"{"dry": "true"}"#, "must be a boolean"),
            (r#"{"only": "a"}"#, "must be an array of strings"),
            (r#"{"only": ["a", 1]}"#, "must be an array of strings"),
            (r#"{"env": false}"#, "must be a string"),
        ] {
            let err = merge_params_json(raw, &declared, &mut HashMap::new()).unwrap_err();
            assert!(format!("{err:#}").contains(needs), "{raw}: {err:#}");
            assert_eq!(machine::classify(&err), machine::ErrorCode::Usage, "{raw}");
        }
    }

    /// A switch on the command line and the same flag in params-json is the
    /// same conflict as any other param given twice.
    #[test]
    fn merge_params_json_flag_given_twice_conflicts() {
        let declared = declared_params(KINDS_CMD);
        let mut provided = HashMap::from([("dry".to_string(), Value::Flag(true))]);
        let err = merge_params_json(r#"{"dry": false}"#, &declared, &mut provided).unwrap_err();
        assert!(format!("{err:#}").contains("given both"), "{err:#}");
    }
}
