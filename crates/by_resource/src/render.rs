//! turning a [`Value`] into the python that stands for it.
//!
//! a mapping becomes a class, because a class is the only python object whose
//! attributes a type checker knows one by one without anybody writing them
//! down twice. a sequence becomes a tuple, so an index reaches an element the
//! checker can name rather than the union of everything in the collection. a
//! scalar becomes a literal, annotated `Final` so it keeps the value it was
//! written with instead of widening to its class.
//!
//! this is the only place that decides what a resource means. the type checker
//! reads a resource by inferring the module rendered here, and the transpiler
//! writes the very same rendering into the python it emits, so a value's type
//! and the object the program actually gets cannot drift apart.

use std::fmt::Write as _;

use ruff_python_stdlib::identifiers::is_identifier;

use crate::value::Value;

/// the import the rendered statements need.
pub const REQUIRED_IMPORT: &str = "from typing import Final";

const INDENT: &str = "    ";

/// python standing for one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// module-level statements, in the order they must run
    pub source: String,
    /// the name the statements bind the document to
    pub root: String,
    /// paths of the keys that could not be given a name, in document order
    pub unusable_keys: Vec<String>,
}

impl Rendered {
    /// the statements as a module of their own, import included.
    pub fn module_source(&self) -> String {
        format!("{REQUIRED_IMPORT}\n\n{}", self.source)
    }
}

/// render `value` as python that binds it to `root`.
///
/// `root` must be a valid identifier; [`binding_name`] makes one out of a file
/// name.
pub(crate) fn render(value: &Value, root: &str) -> Rendered {
    let mut renderer = Renderer {
        prefix: format!("{HELPER_PREFIX}{root}_"),
        helpers: Vec::new(),
        unusable_keys: Vec::new(),
        next_helper: 0,
    };

    // a mapping at the top is the document's own namespace, so it becomes the
    // class the name is bound to rather than a class the name points at
    let binding = match value {
        Value::Map(entries) => renderer.class(root, entries, "", ""),
        _ => {
            let expression = renderer.expression(value);
            format!("{root}: Final = {expression}\n")
        }
    };

    let mut source = String::new();
    for helper in &renderer.helpers {
        source.push_str(helper);
        source.push('\n');
    }
    source.push_str(&binding);

    Rendered {
        source,
        root: root.to_string(),
        unusable_keys: renderer.unusable_keys,
    }
}

/// the name to bind a resource file's document to, made from its file stem.
///
/// the stem is used as written when it can be, so the class the checker reports
/// is the one the reader would guess from the file name.
pub fn binding_name(stem: &str) -> String {
    if is_identifier(stem) {
        return stem.to_string();
    }

    let mut name = String::with_capacity(stem.len() + 1);
    for character in stem.chars() {
        if character.is_alphanumeric() || character == '_' {
            name.push(character);
        } else {
            name.push('_');
        }
    }
    if !is_identifier(&name) {
        name.insert_str(0, "resource_");
    }
    name
}

/// whether a mapping key can be reached through an attribute of that name.
///
/// a key that python cannot spell as an attribute is left out entirely rather
/// than renamed: a reader comparing the document against the code has to be
/// able to trust that a name in one is the same name in the other.
///
/// a name with two leading underscores is left out as well. python mangles
/// `__x` inside a class body, so the attribute the reader would write is not
/// the one that would exist, and `__x__` would collide with what a class object
/// carries of its own.
///
/// two names the rendering itself needs are left out for a third reason. an
/// attribute is what a class body resolves a bare name to, ahead of the module
/// around it, so a key called `Final` would be the `Final` every sibling after
/// it is annotated with, and a key called `_by_…` would be the helper class a
/// sibling names — the value would then be the number in the document rather
/// than the class the checker described.
fn is_usable_key(key: &str) -> bool {
    is_identifier(key)
        && !key.starts_with("__")
        && !key.starts_with(HELPER_PREFIX)
        && key != "Final"
}

/// what the classes this rendering needs for itself are named after.
const HELPER_PREFIX: &str = "_by_";

struct Renderer {
    /// what the helper classes this document needs are named after
    prefix: String,
    /// helper class definitions, in the order they must run
    helpers: Vec<String>,
    unusable_keys: Vec<String>,
    next_helper: usize,
}

impl Renderer {
    /// a `class` statement binding `name` to `entries`, indented by `indent`.
    ///
    /// `path` is what the keys of `entries` are reached through, so an
    /// unusable one can be reported where the document holds it rather than by
    /// a bare name that may appear at several depths.
    fn class(
        &mut self,
        name: &str,
        entries: &[(String, Value)],
        indent: &str,
        path: &str,
    ) -> String {
        let body_indent = format!("{indent}{INDENT}");
        let mut body = String::new();

        for (key, value) in entries {
            if !is_usable_key(key) {
                self.unusable_keys.push(format!("{path}{key}"));
                continue;
            }
            match value {
                // a mapping under a name of its own nests, so the class the
                // checker reports for `config.a` is called `a`
                Value::Map(entries) => {
                    body.push_str(&self.class(
                        key,
                        entries,
                        &body_indent,
                        &format!("{path}{key}."),
                    ));
                }
                _ => {
                    let expression = self.expression(value);
                    let _ = writeln!(body, "{body_indent}{key}: Final = {expression}");
                }
            }
        }

        if body.is_empty() {
            body = format!("{body_indent}pass\n");
        }

        format!("{indent}class {name}:\n{body}")
    }

    /// an expression for `value`.
    ///
    /// a mapping has no expression form, so one is defined as a class of its own
    /// first and named here.
    fn expression(&mut self, value: &Value) -> String {
        match value {
            Value::Null => "None".to_string(),
            Value::Bool(true) => "True".to_string(),
            Value::Bool(false) => "False".to_string(),
            Value::Int(value) => value.to_string(),
            Value::BigInt(digits) => digits.clone(),
            Value::Float(value) => float_literal(*value),
            Value::Str(value) => string_literal(value),
            Value::Seq(items) => {
                let parts: Vec<_> = items.iter().map(|item| self.expression(item)).collect();
                match parts.len() {
                    0 => "()".to_string(),
                    // a one-element tuple needs its comma
                    1 => format!("({},)", parts[0]),
                    _ => format!("({})", parts.join(", ")),
                }
            }
            Value::Map(entries) => {
                let name = format!("{}{}", self.prefix, self.next_helper);
                self.next_helper += 1;
                // a class body runs when the class is defined, so a helper this
                // one names has to exist by then. rendering first and pushing
                // afterwards puts the helpers it needed ahead of it
                let definition = self.class(&name, entries, "", "");
                self.helpers.push(definition);
                name
            }
        }
    }
}

/// a python float literal for `value`.
fn float_literal(value: f64) -> String {
    if value.is_nan() {
        return "float(\"nan\")".to_string();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "float(\"-inf\")".to_string()
        } else {
            "float(\"inf\")".to_string()
        };
    }
    // `{:?}` is the shortest text that reads back as the same `f64`, and it
    // always writes a `.` or an `e`, so the result is a float literal rather
    // than an integer one
    format!("{value:?}")
}

/// a python string literal for `value`.
fn string_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len() + 2);
    literal.push('"');
    for character in value.chars() {
        match character {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            '\t' => literal.push_str("\\t"),
            character if (character as u32) < 0x20 || character as u32 == 0x7f => {
                let _ = write!(literal, "\\x{:02x}", character as u32);
            }
            character => literal.push(character),
        }
    }
    literal.push('"');
    literal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{Format, parse};

    fn rendered(format: Format, text: &str) -> Rendered {
        render(&parse(format, text).unwrap(), "config")
    }

    fn source(format: Format, text: &str) -> String {
        rendered(format, text).source
    }

    #[test]
    fn a_nested_mapping_nests() {
        assert_eq!(
            source(Format::Yaml, "a:\n  b:\n    - 1\n    - 2\n"),
            "\
class config:
    class a:
        b: Final = (1, 2)
"
        );
    }

    #[test]
    fn scalars() {
        assert_eq!(
            source(
                Format::Yaml,
                "a: 1\nb: text\nc: true\nd: ~\ne: 1.5\nf: -2\n"
            ),
            "\
class config:
    a: Final = 1
    b: Final = \"text\"
    c: Final = True
    d: Final = None
    e: Final = 1.5
    f: Final = -2
"
        );
    }

    #[test]
    fn a_mapping_inside_a_sequence_becomes_a_helper_class() {
        assert_eq!(
            source(
                Format::Json,
                r#"{"servers": [{"host": "a"}, {"host": "b"}]}"#
            ),
            "\
class _by_config_0:
    host: Final = \"a\"

class _by_config_1:
    host: Final = \"b\"

class config:
    servers: Final = (_by_config_0, _by_config_1)
"
        );
    }

    #[test]
    fn a_helper_a_helper_needs_is_defined_first() {
        assert_eq!(
            source(Format::Json, r#"{"a": [{"b": [{"c": 1}]}]}"#),
            "\
class _by_config_1:
    c: Final = 1

class _by_config_0:
    b: Final = (_by_config_1,)

class config:
    a: Final = (_by_config_0,)
"
        );
    }

    #[test]
    fn a_document_that_is_not_a_mapping_binds_the_value() {
        assert_eq!(source(Format::Json, "[1, 2]"), "config: Final = (1, 2)\n");
        assert_eq!(source(Format::Json, "5"), "config: Final = 5\n");
        assert_eq!(source(Format::Yaml, ""), "config: Final = None\n");
    }

    #[test]
    fn a_mapping_at_the_top_of_a_sequence_still_gets_a_helper() {
        assert_eq!(
            source(Format::Json, r#"[{"a": 1}]"#),
            "\
class _by_config_0:
    a: Final = 1

config: Final = (_by_config_0,)
"
        );
    }

    #[test]
    fn an_empty_mapping_has_an_empty_body() {
        assert_eq!(
            source(Format::Json, r#"{"a": {}}"#),
            "\
class config:
    class a:
        pass
"
        );
        assert_eq!(source(Format::Json, "{}"), "class config:\n    pass\n");
    }

    #[test]
    fn an_empty_sequence_is_an_empty_tuple() {
        assert_eq!(
            source(Format::Json, r#"{"a": []}"#),
            "class config:\n    a: Final = ()\n"
        );
    }

    #[test]
    fn a_key_python_cannot_spell_is_left_out() {
        let rendered = rendered(
            Format::Json,
            r#"{"a-b": 1, "class": 2, "__x": 3, "1": 4, "ok": 5}"#,
        );
        assert_eq!(rendered.source, "class config:\n    ok: Final = 5\n");
        assert_eq!(rendered.unusable_keys, ["a-b", "class", "__x", "1"]);
    }

    /// the same name can be a key at several depths, so a report that named one
    /// of them alone would not say which
    #[test]
    fn an_unusable_key_is_reported_where_the_document_holds_it() {
        let rendered = rendered(Format::Json, r#"{"a": {"b": {"c-d": 1}}, "c-d": 2}"#);
        assert_eq!(rendered.unusable_keys, ["a.b.c-d", "c-d"]);
    }

    #[test]
    fn a_key_the_rendering_needs_the_name_of_is_left_out() {
        let rendered = rendered(Format::Json, r#"{"Final": 1, "a": 2}"#);
        assert_eq!(rendered.source, "class config:\n    a: Final = 2\n");
        assert_eq!(rendered.unusable_keys, ["Final"]);
    }

    /// a class body resolves a bare name to its own attributes first, so a key
    /// named after a helper class would be what the sibling that names that
    /// helper actually gets — the number in the document, not the class
    #[test]
    fn a_key_that_would_shadow_a_helper_class_is_left_out() {
        let rendered = rendered(Format::Json, r#"{"_by_config_0": 1, "x": [{"y": 2}]}"#);
        assert_eq!(
            rendered.source,
            "\
class _by_config_0:
    y: Final = 2

class config:
    x: Final = (_by_config_0,)
"
        );
        assert_eq!(rendered.unusable_keys, ["_by_config_0"]);
    }

    #[test]
    fn a_dunder_key_is_left_out() {
        let rendered = rendered(Format::Json, r#"{"__x__": 1}"#);
        assert_eq!(rendered.unusable_keys, ["__x__"]);
    }

    #[test]
    fn strings_are_escaped() {
        assert_eq!(
            source(Format::Json, r#"{"a": "he said \"hi\"\n\tc:\\d\u0001"}"#),
            "class config:\n    a: Final = \"he said \\\"hi\\\"\\n\\tc:\\\\d\\x01\"\n"
        );
    }

    #[test]
    fn non_ascii_survives() {
        assert_eq!(
            source(Format::Json, r#"{"a": "héllo — ok"}"#),
            "class config:\n    a: Final = \"héllo — ok\"\n"
        );
    }

    #[test]
    fn floats_read_back_as_floats() {
        assert_eq!(float_literal(1.0), "1.0");
        assert_eq!(float_literal(-0.5), "-0.5");
        assert_eq!(float_literal(f64::INFINITY), "float(\"inf\")");
        assert_eq!(float_literal(f64::NEG_INFINITY), "float(\"-inf\")");
        assert_eq!(float_literal(f64::NAN), "float(\"nan\")");
    }

    #[test]
    fn a_module_carries_its_import() {
        assert_eq!(
            rendered(Format::Json, r#"{"a": 1}"#).module_source(),
            "from typing import Final\n\nclass config:\n    a: Final = 1\n"
        );
    }

    #[test]
    fn binding_names() {
        assert_eq!(binding_name("config"), "config");
        assert_eq!(binding_name("my-config"), "my_config");
        assert_eq!(binding_name("2020"), "resource_2020");
        assert_eq!(binding_name("class"), "resource_class");
        assert_eq!(binding_name("_hidden"), "_hidden");
    }
}
