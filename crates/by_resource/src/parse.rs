//! reading json, toml and yaml into [`Value`].

use std::fmt;

use rustc_hash::FxHashMap;

use crate::value::Value;

/// a data format a static resource can be written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Json,
    Toml,
    Yaml,
}

impl Format {
    /// the format a file extension names, or `None` when the extension is not
    /// one a static resource can be written in.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension {
            "json" => Some(Format::Json),
            "toml" => Some(Format::Toml),
            "yaml" | "yml" => Some(Format::Yaml),
            _ => None,
        }
    }

    /// every extension a static resource can have, for listing in a diagnostic.
    pub const EXTENSIONS: &'static [&'static str] = &["json", "toml", "yaml", "yml"];

    /// what to call the format in a message.
    pub fn name(self) -> &'static str {
        match self {
            Format::Json => "json",
            Format::Toml => "toml",
            Format::Yaml => "yaml",
        }
    }
}

/// why a document could not be read as a static resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
}

impl ParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// the largest document that will be turned into python.
///
/// the generated source is parsed and type checked like any other module, so a
/// resource is bounded the way a hand-written module is. the limit is far above
/// any configuration file and far below anything that would make the type
/// checker crawl.
const MAX_NODES: usize = 100_000;

/// how deeply a document may nest.
///
/// each level of mapping becomes a level of nested `class`, and python's parser
/// gives up long before this. no configuration file comes close.
const MAX_DEPTH: usize = 32;

/// what is left of a document's budget.
///
/// yaml is read against this as the tree is built rather than after, for two
/// reasons that both end the process rather than the parse. the walk is
/// recursive, so a document nested a hundred thousand deep overflows the stack
/// before anything can measure it. and an anchor is expanded wherever it is
/// used, so a two-hundred byte file can name more values than there is memory
/// to hold — the size has to be counted while the copies are being made.
///
/// json and toml need no such thing here: their own parsers refuse to recurse
/// past a limit of their own (128 levels and a comparable one), which bounds
/// the tree they hand over, and neither format can name a value twice.
struct Budget {
    nodes: usize,
    depth: usize,
}

impl Budget {
    fn new() -> Self {
        Self {
            nodes: MAX_NODES,
            depth: MAX_DEPTH,
        }
    }

    /// charge one value, and enter it.
    fn enter(&mut self) -> Result<(), ParseError> {
        self.nodes = self
            .nodes
            .checked_sub(1)
            .ok_or_else(|| ParseError::new(too_large()))?;
        self.depth = self
            .depth
            .checked_sub(1)
            .ok_or_else(|| ParseError::new(too_deep()))?;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth += 1;
    }

    /// charge `nodes` values without entering them.
    fn charge(&mut self, nodes: usize) -> Result<(), ParseError> {
        self.nodes = self
            .nodes
            .checked_sub(nodes)
            .ok_or_else(|| ParseError::new(too_large()))?;
        Ok(())
    }
}

fn too_large() -> String {
    format!(
        "the document holds more than {MAX_NODES} values, which is too large to import as a static resource"
    )
}

fn too_deep() -> String {
    format!(
        "the document nests more than {MAX_DEPTH} levels deep, which is too deep to import as a static resource"
    )
}

/// read `text` as `format`.
pub fn parse(format: Format, text: &str) -> Result<Value, ParseError> {
    let value = match format {
        Format::Json => json(text),
        Format::Toml => toml(text),
        Format::Yaml => yaml(text),
    }?;

    if value.size() > MAX_NODES {
        return Err(ParseError::new(too_large()));
    }
    if value.depth() > MAX_DEPTH {
        return Err(ParseError::new(too_deep()));
    }

    Ok(value)
}

/// a mapping being read, in document order and without duplicates.
///
/// json and yaml both allow a document to name a key twice and both read the
/// last one, so the last one is what the reader of the generated python should
/// find too — under one name, because two `Final` declarations of the same
/// attribute is not something python would accept.
///
/// the earlier entry is emptied rather than removed, and the empties are dropped
/// at the end. scanning the entries for each key instead would be quadratic, and
/// a document is allowed a hundred thousand of them.
#[derive(Default)]
struct Entries {
    entries: Vec<Option<(String, Value)>>,
    positions: FxHashMap<String, usize>,
}

impl Entries {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            positions: FxHashMap::default(),
        }
    }

    fn insert(&mut self, key: String, value: Value) {
        if let Some(previous) = self.positions.insert(key.clone(), self.entries.len()) {
            self.entries[previous] = None;
        }
        self.entries.push(Some((key, value)));
    }

    fn into_value(self) -> Value {
        Value::Map(self.entries.into_iter().flatten().collect())
    }
}

/// json, read through serde rather than through `serde_json::Value`.
///
/// `serde_json::Value` holds its object in a `BTreeMap`, which would hand the
/// keys over sorted; a visitor sees them in the order the document lists them.
fn json(text: &str) -> Result<Value, ParseError> {
    serde_json::from_str::<Document>(text)
        .map(|document| document.0)
        .map_err(|error| ParseError::new(error.to_string()))
}

/// a document being read by serde.
struct Document(Value);

impl<'de> serde::Deserialize<'de> for Document {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(DocumentVisitor).map(Document)
    }
}

struct DocumentVisitor;

impl<'de> serde::de::Visitor<'de> for DocumentVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a json value")
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Int(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(i64::try_from(value).map_or_else(
            // python's integers have no upper bound, so the digits go through
            // as they were written rather than through an `f64`
            |_| Value::BigInt(value.to_string()),
            Value::Int,
        ))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Value, E> {
        Ok(Value::Float(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::Str(value.to_string()))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(Document(item)) = access.next_element()? {
            items.push(item);
        }
        Ok(Value::Seq(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
        let mut entries = Entries::default();
        while let Some((key, Document(value))) = access.next_entry::<String, Document>()? {
            entries.insert(key, value);
        }
        Ok(entries.into_value())
    }
}

/// toml, read through `toml_edit` rather than through `toml`.
///
/// `toml::Value` holds its tables sorted, and `toml_edit` keeps them in document
/// order — the same reason json is read through a visitor.
fn toml(text: &str) -> Result<Value, ParseError> {
    let document =
        toml_edit::Document::parse(text).map_err(|error| ParseError::new(error.to_string()))?;
    Ok(from_toml_table(document.as_table()))
}

fn from_toml_table(table: &toml_edit::Table) -> Value {
    let mut entries = Entries::with_capacity(table.len());
    for (key, item) in table {
        entries.insert(key.to_string(), from_toml_item(item));
    }
    entries.into_value()
}

fn from_toml_item(item: &toml_edit::Item) -> Value {
    match item {
        toml_edit::Item::Value(value) => from_toml_value(value),
        toml_edit::Item::Table(table) => from_toml_table(table),
        toml_edit::Item::ArrayOfTables(tables) => {
            Value::Seq(tables.iter().map(from_toml_table).collect())
        }
        // a key that was written without a value; toml's parser rejects that, so
        // nothing reaching here has one
        toml_edit::Item::None => Value::Null,
    }
}

fn from_toml_value(value: &toml_edit::Value) -> Value {
    match value {
        toml_edit::Value::String(value) => Value::Str(value.value().clone()),
        toml_edit::Value::Integer(value) => Value::Int(*value.value()),
        toml_edit::Value::Float(value) => Value::Float(*value.value()),
        toml_edit::Value::Boolean(value) => Value::Bool(*value.value()),
        // toml is the only one of the three formats with a date type, and python
        // has no literal for one. the text the document holds is what survives,
        // which is at least exactly what was written
        toml_edit::Value::Datetime(value) => Value::Str(value.value().to_string()),
        toml_edit::Value::Array(items) => Value::Seq(items.iter().map(from_toml_value).collect()),
        toml_edit::Value::InlineTable(table) => {
            let mut entries = Entries::with_capacity(table.len());
            for (key, value) in table {
                entries.insert(key.to_string(), from_toml_value(value));
            }
            entries.into_value()
        }
    }
}

/// yaml, read from the parser's event stream rather than from a loaded tree.
///
/// the tree a loader hands back is built by recursing once per level, and a
/// document can nest deeper than there is stack to do that in — the load
/// overflows before anything is in a position to measure it. reading the events
/// with a stack of our own has no such limit, and it is also where an anchor is
/// expanded, which is the only way to charge the copies against the budget
/// before they are made.
fn yaml(text: &str) -> Result<Value, ParseError> {
    let mut reader = YamlReader {
        budget: Budget::new(),
        frames: Vec::new(),
        anchors: FxHashMap::default(),
        document: None,
        documents: 0,
    };

    for event in saphyr_parser::Parser::new_from_str(text) {
        let (event, _span) = event.map_err(|error| ParseError::new(error.to_string()))?;
        reader.read(event)?;
    }

    // an empty file is a document holding nothing
    Ok(reader.document.unwrap_or(Value::Null))
}

/// a collection the reader is in the middle of.
enum Frame {
    Sequence {
        anchor: usize,
        items: Vec<Value>,
    },
    Mapping {
        anchor: usize,
        entries: Entries,
        /// the key this mapping is waiting for a value for
        key: Option<String>,
    },
}

struct YamlReader {
    budget: Budget,
    frames: Vec<Frame>,
    /// what each anchor in the document was defined as, to copy at an alias
    anchors: FxHashMap<usize, Value>,
    document: Option<Value>,
    documents: usize,
}

impl YamlReader {
    fn read(&mut self, event: saphyr_parser::Event<'_>) -> Result<(), ParseError> {
        use saphyr_parser::Event;

        match event {
            Event::DocumentStart(_) => {
                self.documents += 1;
                if self.documents > 1 {
                    return Err(ParseError::new(
                        "the file holds more than one yaml document, and a static resource is a single value",
                    ));
                }
            }
            Event::Scalar(text, style, anchor, tag) => {
                self.budget.enter()?;
                self.budget.leave();
                let scalar = saphyr::Scalar::parse_from_cow_and_metadata(text, style, tag.as_ref())
                    .ok_or_else(|| {
                        ParseError::new("the document holds a value yaml could not read")
                    })?;
                self.place(scalar_value(&scalar), anchor)?;
            }
            Event::SequenceStart(anchor, _) => {
                self.budget.enter()?;
                self.frames.push(Frame::Sequence {
                    anchor,
                    items: Vec::new(),
                });
            }
            Event::MappingStart(anchor, _) => {
                self.budget.enter()?;
                self.frames.push(Frame::Mapping {
                    anchor,
                    entries: Entries::default(),
                    key: None,
                });
            }
            Event::SequenceEnd | Event::MappingEnd => {
                self.budget.leave();
                let (value, anchor) = match self.frames.pop() {
                    Some(Frame::Sequence { anchor, items }) => (Value::Seq(items), anchor),
                    Some(Frame::Mapping {
                        anchor, entries, ..
                    }) => (entries.into_value(), anchor),
                    // the parser pairs every end with a start
                    None => return Ok(()),
                };
                self.place(value, anchor)?;
            }
            Event::Alias(anchor) => {
                let Some(value) = self.anchors.get(&anchor) else {
                    return Err(ParseError::new(
                        "the document uses a yaml alias for an anchor it never defined",
                    ));
                };
                // an alias is a copy, so the copy is what the budget is charged
                // for — this is the only thing standing between a handful of
                // anchors and a document that names more values than there is
                // memory to hold
                self.budget.charge(value.size())?;
                let value = value.clone();
                self.place(value, 0)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// record `value` under `anchor` if it has one, and hand it to whatever is
    /// waiting for it.
    fn place(&mut self, value: Value, anchor: usize) -> Result<(), ParseError> {
        // anchor 0 is the parser's way of saying there is no anchor
        if anchor != 0 {
            self.anchors.insert(anchor, value.clone());
        }

        match self.frames.last_mut() {
            Some(Frame::Sequence { items, .. }) => items.push(value),
            Some(Frame::Mapping { entries, key, .. }) => match key.take() {
                Some(name) => entries.insert(name, value),
                None => match value {
                    Value::Str(name) => *key = Some(name),
                    // a mapping key that is not a string cannot name an
                    // attribute, and pretending otherwise would silently drop it
                    _ => {
                        return Err(ParseError::new(
                            "a mapping key is not a string, and a static resource is read through its keys",
                        ));
                    }
                },
            },
            None => self.document = Some(value),
        }

        Ok(())
    }
}

fn scalar_value(scalar: &saphyr::Scalar<'_>) -> Value {
    use saphyr::Scalar;

    match scalar {
        Scalar::Null => Value::Null,
        Scalar::Boolean(value) => Value::Bool(*value),
        Scalar::Integer(value) => Value::Int(*value),
        Scalar::FloatingPoint(value) => Value::Float(value.into_inner()),
        Scalar::String(value) => Value::Str(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn map(entries: [(&str, Value); 1]) -> Value {
        Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        )
    }

    #[test]
    fn json_document() {
        let value = parse(Format::Json, r#"{"a": {"b": [1, 2]}}"#).unwrap();
        assert_eq!(
            value,
            map([(
                "a",
                map([("b", Value::Seq(vec![Value::Int(1), Value::Int(2)]))])
            )])
        );
    }

    #[test]
    fn toml_document() {
        let value = parse(Format::Toml, "[a]\nb = [1, 2]\n").unwrap();
        assert_eq!(
            value,
            map([(
                "a",
                map([("b", Value::Seq(vec![Value::Int(1), Value::Int(2)]))])
            )])
        );
    }

    #[test]
    fn yaml_document() {
        let value = parse(Format::Yaml, "a:\n  b:\n    - 1\n    - 2\n").unwrap();
        assert_eq!(
            value,
            map([(
                "a",
                map([("b", Value::Seq(vec![Value::Int(1), Value::Int(2)]))])
            )])
        );
    }

    #[test]
    fn yaml_scalars() {
        let value = parse(Format::Yaml, "a: ~\nb: true\nc: 1.5\nd: text\n").unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                ("a".to_string(), Value::Null),
                ("b".to_string(), Value::Bool(true)),
                ("c".to_string(), Value::Float(1.5)),
                ("d".to_string(), Value::Str("text".to_string())),
            ])
        );
    }

    #[test]
    fn empty_yaml_is_null() {
        assert_eq!(parse(Format::Yaml, "").unwrap(), Value::Null);
    }

    #[test]
    fn a_second_yaml_document_is_rejected() {
        let error = parse(Format::Yaml, "a: 1\n---\nb: 2\n").unwrap_err();
        assert!(error.to_string().contains("more than one yaml document"));
    }

    #[test]
    fn a_yaml_anchor_is_expanded() {
        let value = parse(Format::Yaml, "a: &anchor 1\nb: *anchor\n").unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                ("a".to_string(), Value::Int(1)),
                ("b".to_string(), Value::Int(1)),
            ])
        );
    }

    #[test]
    fn the_last_of_two_keys_with_one_name_wins() {
        let value = parse(Format::Json, r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                ("b".to_string(), Value::Int(2)),
                ("a".to_string(), Value::Int(3)),
            ])
        );
    }

    #[test]
    fn keys_keep_the_order_the_document_lists_them_in() {
        let value = parse(Format::Json, r#"{"b": 1, "a": 2}"#).unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                ("b".to_string(), Value::Int(1)),
                ("a".to_string(), Value::Int(2)),
            ])
        );

        let value = parse(Format::Toml, "b = 1\na = 2\n").unwrap();
        assert_eq!(
            value,
            Value::Map(vec![
                ("b".to_string(), Value::Int(1)),
                ("a".to_string(), Value::Int(2)),
            ])
        );
    }

    #[test]
    fn a_toml_array_of_tables() {
        let value = parse(Format::Toml, "[[a]]\nb = 1\n\n[[a]]\nb = 2\n").unwrap();
        assert_eq!(
            value,
            map([(
                "a",
                Value::Seq(vec![
                    Value::Map(vec![("b".to_string(), Value::Int(1))]),
                    Value::Map(vec![("b".to_string(), Value::Int(2))]),
                ])
            )])
        );
    }

    #[test]
    fn a_non_string_yaml_key_is_rejected() {
        let error = parse(Format::Yaml, "1: one\n").unwrap_err();
        assert!(error.to_string().contains("not a string"));
    }

    #[test]
    fn json_integers_larger_than_i64() {
        let value = parse(Format::Json, r#"{"a": 18446744073709551615}"#).unwrap();
        assert_eq!(
            value,
            map([("a", Value::BigInt("18446744073709551615".to_string()))])
        );
    }

    #[test]
    fn a_toml_datetime_keeps_its_text() {
        let value = parse(Format::Toml, "a = 1979-05-27\n").unwrap();
        assert_eq!(value, map([("a", Value::Str("1979-05-27".to_string()))]));
    }

    #[test]
    fn a_document_that_nests_too_deeply_is_rejected() {
        let text = "[".repeat(MAX_DEPTH + 1) + &"]".repeat(MAX_DEPTH + 1);
        let error = parse(Format::Json, &text).unwrap_err();
        assert!(error.to_string().contains("too deep"));
    }

    /// the depth is counted as the tree is built, because the walk that would
    /// measure it afterwards is the one that overflows the stack
    #[test]
    fn a_yaml_document_nested_past_any_stack_is_rejected() {
        let text = "- ".repeat(100_000) + "1\n";
        let error = parse(Format::Yaml, &text).unwrap_err();
        assert!(error.to_string().contains("too deep"), "{error}");
    }

    /// an anchor is expanded wherever it is used, so the size has to be counted
    /// while the copies are being made — a few hundred bytes name more values
    /// than there is memory to hold
    #[test]
    fn a_yaml_document_whose_anchors_expand_without_end_is_rejected() {
        let mut text = String::from("a0: &a0 [1, 1, 1, 1, 1, 1, 1, 1, 1]\n");
        for level in 1..12 {
            let refs = (0..9)
                .map(|_| format!("*a{previous}", previous = level - 1))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(text, "a{level}: &a{level} [{refs}]");
        }
        let error = parse(Format::Yaml, &text).unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn extensions() {
        assert_eq!(Format::from_extension("json"), Some(Format::Json));
        assert_eq!(Format::from_extension("yml"), Some(Format::Yaml));
        assert_eq!(Format::from_extension("txt"), None);
    }
}
