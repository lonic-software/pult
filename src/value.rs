//! A param's value, typed by its kind, and how it renders into a script.

use indexmap::IndexMap;

use crate::interp;
use crate::manifest::{ParamDef, ParamKind};

/// What one param holds: text for a pick or an input, a boolean for a flag,
/// a list for a repeat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Flag(bool),
    List(Vec<String>),
}

/// A command's values, by param name, in declaration order.
pub type Values = IndexMap<String, Value>;

impl Value {
    /// The value as the run journal records it, which is always a string: a
    /// journal reader that expects one would otherwise drop the whole run.
    /// A flag is `true`/`false`, a list the JSON array of its items.
    pub fn journal_text(&self) -> String {
        match self {
            Value::Text(s) => s.clone(),
            Value::Flag(b) => b.to_string(),
            Value::List(items) => serde_json::to_string(items).expect("strings serialize"),
        }
    }

    /// Every string this value is made of, for the output masker.
    pub fn strings(&self) -> Vec<&str> {
        match self {
            Value::Text(s) => vec![s.as_str()],
            Value::Flag(_) => Vec::new(),
            Value::List(items) => items.iter().map(String::as_str).collect(),
        }
    }
}

/// Render every value as the shell text its `{name}` placeholder becomes
/// (see [`render`]). A param missing from `params` renders as plain text,
/// which is how a step's own rebinding reads.
pub fn words(params: &IndexMap<String, ParamDef>, values: &Values) -> IndexMap<String, String> {
    values
        .iter()
        .map(|(name, value)| (name.clone(), render(params.get(name), value)))
        .collect()
}

/// The shell text one value renders as: text is one quoted word; a set flag
/// is its quoted `arg` and an unset one is nothing at all; a list is one
/// quoted word per item, each preceded by the quoted `arg` when the repeat
/// has one, and nothing when empty.
pub fn render(def: Option<&ParamDef>, value: &Value) -> String {
    let arg = def.and_then(|def| match def.kind() {
        ParamKind::Flag(flag) => flag.arg.as_deref(),
        ParamKind::Repeat(repeat) => repeat.arg.as_deref(),
        _ => None,
    });
    match value {
        Value::Text(s) => interp::shell_quote(s),
        Value::Flag(false) => String::new(),
        Value::Flag(true) => {
            interp::shell_quote(arg.expect("a resolved flag always has an `arg`"))
        }
        Value::List(items) => items
            .iter()
            .map(|item| match arg {
                Some(arg) => format!("{} {}", interp::shell_quote(arg), interp::shell_quote(item)),
                None => interp::shell_quote(item),
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(yaml: &str) -> ParamDef {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn a_flag_renders_its_arg_or_nothing() {
        let flag = def("flag: { arg: --dry-run }");
        assert_eq!(render(Some(&flag), &Value::Flag(true)), "--dry-run");
        assert_eq!(render(Some(&flag), &Value::Flag(false)), "");
    }

    #[test]
    fn a_repeat_renders_each_item_after_its_arg() {
        let option = def("repeat: { arg: --only }");
        let items = Value::List(vec!["a b".into(), "c".into()]);
        assert_eq!(render(Some(&option), &items), "--only 'a b' --only c");
        assert_eq!(render(Some(&option), &Value::List(Vec::new())), "");
        let positional = def("repeat: {}");
        assert_eq!(render(Some(&positional), &items), "'a b' c");
        // An empty item is still a word.
        assert_eq!(render(Some(&positional), &Value::List(vec![String::new()])), "''");
    }

    #[test]
    fn text_renders_as_one_quoted_word() {
        assert_eq!(render(None, &Value::Text(String::new())), "''");
        assert_eq!(render(None, &Value::Text("x y".into())), "'x y'");
    }

    #[test]
    fn the_journal_sees_only_strings() {
        assert_eq!(Value::Flag(true).journal_text(), "true");
        assert_eq!(Value::Flag(false).journal_text(), "false");
        assert_eq!(
            Value::List(vec!["a".into(), "b\"".into()]).journal_text(),
            r#"["a","b\""]"#
        );
        assert_eq!(Value::Text("t".into()).journal_text(), "t");
    }
}
