//! Lowers based-enum declarations (`enum class Name: ...`) — algebraic sum types —
//! to Python.
//!
//! This is a source→source phase that runs *before* the main `ast_driver`
//! pipeline. The parser represents `enum class Shape:` as a `ClassDef` carrying a
//! synthetic `enum_def` marker decorator, with one nested `ClassDef` per
//! variant (tagged `variant_unit` / `variant_tuple`) and any
//! ordinary members (methods, classmethods, constants) interspersed.
//!
//! Two target shapes:
//!
//! - an enum whose variants are **all unit** (no payloads) and which is not
//!   generic lowers to an idiomatic `enum.Enum` with `auto()` members. this is
//!   the form the reverse transform recognises.
//! - any other enum (one or more payload-carrying variants, or a generic enum)
//!   lowers to a sealed hierarchy: the enum class holds the shared members, and
//!   each variant becomes a module-level **subclass** of the enum (`@final`
//!   frozen `@dataclass` for payload variants, a singleton instance for unit
//!   variants), attached back as `Name.Variant`. subclassing is what makes the
//!   enum's methods dispatch on the variants.
//!
//! Member bodies are copied verbatim from the source so any basedpython syntax
//! they contain (intersection types, `?.`, match arms, …) survives to be
//! lowered by the downstream `ast_driver` passes. The phase emits the small set
//! of imports it needs (`enum`/`dataclasses`/`typing`) as a deduplicated
//! prologue.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

use ruff_python_ast::{Expr, PySourceType, PythonVersion, Stmt, StmtClassDef};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use super::source_util::{is_synthetic_decorator, line_indent, line_start};

/// Result of the enum-lowering phase: the rewritten source and an output-line →
/// original-`.by`-line table (`None` for generated lines).
pub(crate) struct EnumLowering<'a> {
    pub(crate) output: Cow<'a, str>,
    pub(crate) line_map: Vec<Option<u32>>,
    pub(crate) errors: Vec<String>,
}

/// Lower every module-level `enum` declaration in `source`. Returns the source
/// unchanged (borrowed) when there are no based enums or the source fails to
/// parse — in which case the normal pipeline surfaces the parse error.
pub(crate) fn lower(source: &str, min_version: PythonVersion) -> EnumLowering<'_> {
    let parsed = parse_unchecked_source(source, PySourceType::BasedPython);
    if !parsed.errors().is_empty() {
        return borrowed(source);
    }
    let suite = parsed.suite();

    // nested enums are not supported: their variant classes would escape to
    // module scope. flag them rather than lower them incorrectly.
    let mut errors = Vec::new();
    check_no_nested_enums(suite, source, false, &mut errors);
    if !errors.is_empty() {
        return EnumLowering {
            output: Cow::Borrowed(source),
            line_map: Vec::new(),
            errors,
        };
    }

    let enums: Vec<&StmtClassDef> = suite
        .iter()
        .filter_map(|s| match s {
            Stmt::ClassDef(c) if is_enum_def(c, source) => Some(c),
            _ => None,
        })
        .collect();
    if enums.is_empty() {
        return borrowed(source);
    }

    // validate the variants. every enum needs at least one; variant names must
    // be unique *within an enum* (they lower to variant subclasses, so a
    // collision would clash — but `A.Foo` and `B.Foo` in different enums are
    // distinct);
    // and a variant's defaulted fields must come last (a
    // default-before-required field is a dataclass error at import time)
    for enum_def in &enums {
        let (variants, _) = partition(enum_def, source);
        if variants.is_empty() {
            errors.push(format!(
                "`enum class {}` must declare at least one variant",
                enum_def.name
            ));
        }
        let mut seen_variants: BTreeSet<String> = BTreeSet::new();
        for variant in &variants {
            if !seen_variants.insert(variant.name.clone()) {
                errors.push(format!(
                    "variant `{}` is declared more than once; variant names must be unique within an `enum class`",
                    variant.name
                ));
            }
            let mut seen_default = false;
            for field in &variant.fields {
                if field.default_src.is_some() {
                    seen_default = true;
                } else if seen_default {
                    errors.push(format!(
                        "field `{}` without a default follows a defaulted field in variant `{}`; defaulted fields must come last",
                        field.name, variant.name
                    ));
                }
            }
        }
    }
    if !errors.is_empty() {
        return EnumLowering {
            output: Cow::Borrowed(source),
            line_map: Vec::new(),
            errors,
        };
    }

    let mut imports = ImportSet::default();
    let mut out = Out::default();

    // a sealed hierarchy is mutually recursive (base methods reference variants
    // and the union alias; recursive enums reference themselves), so annotations
    // must be lazy. emit `from __future__ import annotations` and skip the
    // user's own leading copy if they wrote one
    let future_skip = leading_future_skip(suite, source);
    let mut cursor = future_skip.unwrap_or_default();
    for enum_def in &enums {
        out.push_verbatim(source, TextRange::new(cursor, enum_def.range().start()));
        emit_enum(&mut out, source, enum_def, &mut imports, min_version);
        cursor = enum_def.range().end();
    }
    out.push_verbatim(source, TextRange::new(cursor, source.text_len()));

    // prologue: the `__future__` import (always first) then the deduplicated
    // imports the lowered classes need, prepended ahead of the rewritten body
    let prologue = format!("from __future__ import annotations\n{}", imports.render());
    let mut text = String::with_capacity(prologue.len() + out.text.len());
    let mut line_map = Vec::with_capacity(out.line_map.len());
    for _ in prologue.bytes().filter(|&b| b == b'\n') {
        line_map.push(None);
    }
    text.push_str(&prologue);
    text.push_str(&out.text);
    line_map.extend(out.line_map);

    EnumLowering {
        output: Cow::Owned(text),
        line_map,
        errors: Vec::new(),
    }
}

fn borrowed(source: &str) -> EnumLowering<'_> {
    EnumLowering {
        output: Cow::Borrowed(source),
        line_map: Vec::new(),
        errors: Vec::new(),
    }
}

/// If the module's first statement is `from __future__ import annotations`,
/// return the offset just past its trailing newline, so the rewrite can skip
/// re-emitting it (the prologue emits its own copy first).
fn leading_future_skip(suite: &[Stmt], source: &str) -> Option<TextSize> {
    let Some(Stmt::ImportFrom(node)) = suite.first() else {
        return None;
    };
    if node.module.as_deref() != Some("__future__")
        || !node.names.iter().any(|a| a.name.as_str() == "annotations")
    {
        return None;
    }
    let end = usize::from(node.range().end());
    // advance past the trailing newline so the body copy starts on the next line
    let skip = source[end..]
        .find('\n')
        .map_or(source.len(), |i| end + i + 1);
    TextSize::try_from(skip).ok()
}

/// True when `class` carries the synthetic `enum_def` marker decorator.
fn is_enum_def(class: &StmtClassDef, source: &str) -> bool {
    has_marker(class, source, "enum_def")
}

fn has_marker(class: &StmtClassDef, source: &str, marker: &str) -> bool {
    class.decorator_list.iter().any(|dec| {
        is_synthetic_decorator(source, dec)
            && matches!(&dec.expression, Expr::Name(n) if n.id.as_str() == marker)
    })
}

fn check_no_nested_enums(body: &[Stmt], source: &str, nested: bool, errors: &mut Vec<String>) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(c) if is_enum_def(c, source) => {
                if nested {
                    errors
                        .push("`enum` declarations are only supported at module level".to_string());
                }
                // an enum body holds variant classes, never further enums
            }
            Stmt::ClassDef(c) => check_no_nested_enums(&c.body, source, true, errors),
            Stmt::FunctionDef(f) => check_no_nested_enums(&f.body, source, true, errors),
            Stmt::If(node) => {
                check_no_nested_enums(&node.body, source, true, errors);
                for clause in &node.elif_else_clauses {
                    check_no_nested_enums(&clause.body, source, true, errors);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VariantKind {
    Unit,
    Tuple,
}

struct Field {
    name: String,
    type_src: String,
    default_src: Option<String>,
}

struct Variant {
    name: String,
    kind: VariantKind,
    fields: Vec<Field>,
    indent: String,
}

/// Partition an enum body into variants and ordinary members (the latter kept
/// as source ranges so they can be copied verbatim).
fn partition<'a>(class: &'a StmtClassDef, source: &str) -> (Vec<Variant>, Vec<&'a Stmt>) {
    let mut variants = Vec::new();
    let mut members = Vec::new();
    for stmt in &class.body {
        if let Stmt::ClassDef(c) = stmt
            && let Some(kind) = variant_kind(c, source)
        {
            variants.push(extract_variant(c, source, kind));
        } else {
            members.push(stmt);
        }
    }
    (variants, members)
}

fn variant_kind(class: &StmtClassDef, source: &str) -> Option<VariantKind> {
    if has_marker(class, source, "variant_unit") {
        Some(VariantKind::Unit)
    } else if has_marker(class, source, "variant_tuple") {
        Some(VariantKind::Tuple)
    } else {
        None
    }
}

fn extract_variant(class: &StmtClassDef, source: &str, kind: VariantKind) -> Variant {
    let mut fields = Vec::new();
    for stmt in &class.body {
        if let Stmt::AnnAssign(a) = stmt {
            let name = match a.target.as_ref() {
                Expr::Name(n) => n.id.to_string(),
                _ => continue,
            };
            let type_src = slice(source, a.annotation.range()).to_string();
            let default_src = a
                .value
                .as_ref()
                .map(|v| slice(source, v.range()).to_string());
            fields.push(Field {
                name,
                type_src,
                default_src,
            });
        }
    }
    Variant {
        name: class.name.to_string(),
        kind,
        fields,
        indent: line_indent(source, class.range().start()).to_string(),
    }
}

/// Emit the lowered form of one enum into `out`, recording the imports it needs.
fn emit_enum(
    out: &mut Out,
    source: &str,
    class: &StmtClassDef,
    imports: &mut ImportSet,
    min_version: PythonVersion,
) {
    let (variants, members) = partition(class, source);
    let name = class.name.as_str();
    let is_generic = class.type_params.is_some();

    // `is_all_unit_enum` is the shared predicate with ty (it models such enums
    // as idiomatic `Enum`s): all variants unit AND no assignment members —
    // python's `Enum` would turn a class-body constant into a *member*,
    // silently diverging from the constant semantics the checker models
    if class.is_all_unit_enum() && !is_generic {
        emit_plain_enum(out, source, name, &variants, &members, imports);
    } else {
        emit_sealed_hierarchy(
            out,
            source,
            class,
            &variants,
            &members,
            imports,
            min_version,
        );
    }

    // the replaced source range excludes its trailing newline, so the lowered
    // text must too — the following source byte (a `\n`, or EOF) provides the
    // line break. this keeps a single newline at the splice boundary
    out.pop_trailing_newline();
}

/// `enum class Color: case Red, Green` → `class Color(Enum): Red = auto(); Green = auto()`.
fn emit_plain_enum(
    out: &mut Out,
    source: &str,
    name: &str,
    variants: &[Variant],
    members: &[&Stmt],
    imports: &mut ImportSet,
) {
    imports.add("enum", "Enum");
    imports.add("enum", "auto");

    out.push_gen(&format!("class {name}(Enum):\n"));
    for variant in variants {
        out.push_gen(&format!("{}{} = auto()\n", variant.indent, variant.name));
    }
    for member in members {
        emit_member(out, source, member);
    }
    if variants.is_empty() && members.is_empty() {
        out.push_gen("    pass\n");
    }
}

/// Emit a payload-bearing based enum as an enum class plus one **module-level
/// subclass per variant**, each attached back as a class attribute so it is
/// reached qualified — `A.Foo(1)`, `A.Baz`. subclassing the enum is what makes
/// methods/classmethods/properties defined on the enum body dispatch on the
/// variants (a variant *is* an enum), and matches ty's model where a variant
/// instance is a subtype of the enum. payload variants are frozen dataclasses;
/// unit variants are singleton values.
///
/// variants must be module-level (not nested in the enum body) because a class
/// cannot subclass another class while the latter's body is still executing.
fn emit_sealed_hierarchy(
    out: &mut Out,
    source: &str,
    class: &StmtClassDef,
    variants: &[Variant],
    members: &[&Stmt],
    imports: &mut ImportSet,
    min_version: PythonVersion,
) {
    // payload variants lower to `@final` frozen dataclasses; unit variants are
    // plain singleton values needing neither import
    if variants.iter().any(|v| v.kind != VariantKind::Unit) {
        imports.add("typing", "final");
        imports.add("dataclasses", "dataclass");
    }

    let name = class.name.as_str();
    // declaration form (`[T: bound]`) — valid where a class is *declared*; the
    // generics pass lowers these to mangled `TypeVar`s
    let params = class
        .type_params
        .as_ref()
        .map(|tp| slice(source, tp.range()).to_string())
        .unwrap_or_default();

    out.push_gen(&format!("class {name}{params}:\n"));
    // ordinary members (methods, classmethods, constants) — copied verbatim,
    // already indented under the enum in the source. they may refer to variants
    // (`A.Foo`) freely: the references resolve lazily at call time, by which
    // point the attachments below have run
    for member in members {
        emit_member(out, source, member);
    }
    if members.is_empty() {
        out.push_gen("    pass\n");
    }
    // variant subclasses, emitted at module level and attached to the enum
    for variant in variants {
        out.push_gen("\n");
        emit_variant_class(out, name, variant, min_version);
    }
}

/// Emit one variant as a module-level subclass of the enum and attach it (or, for
/// a unit variant, its singleton instance) as `EnumName.Variant`.
fn emit_variant_class(
    out: &mut Out,
    enum_name: &str,
    variant: &Variant,
    min_version: PythonVersion,
) {
    // a private module-level name holds the subclass; the public binding is the
    // attached `EnumName.Variant`
    let mangled = format!("_{enum_name}_{}", variant.name);
    match variant.kind {
        VariantKind::Unit => {
            // a payload-less variant is a *value*, not a class — `A.Baz` is the
            // singleton itself (like a Rust/Swift unit variant or a Python enum
            // member), matched as `case A.Baz:`. `__repr__` is the bare name
            out.push_gen(&format!("class {mangled}({enum_name}):\n"));
            out.push_gen("    __slots__ = ()\n");
            out.push_gen(&format!(
                "    def __repr__(self): return {:?}\n",
                variant.name
            ));
            emit_variant_name_reset(out, enum_name, &variant.name, &mangled);
            out.push_gen(&format!("{enum_name}.{} = {mangled}()\n", variant.name));
        }
        VariantKind::Tuple => {
            // `slots=True` is a dataclass option only on python 3.10+; a frozen
            // dataclass already blocks attribute mutation, so on older targets
            // we simply omit it
            let slots = if min_version >= PythonVersion::PY310 {
                ", slots=True"
            } else {
                ""
            };
            out.push_gen("@final\n");
            out.push_gen(&format!("@dataclass(frozen=True{slots})\n"));
            out.push_gen(&format!("class {mangled}({enum_name}):\n"));
            if variant.fields.is_empty() {
                // a zero-field payload variant (`A()`) is a valid
                // no-argument constructor; emit a body so the class is valid
                out.push_gen("    pass\n");
            }
            for field in &variant.fields {
                match &field.default_src {
                    Some(default) => out.push_gen(&format!(
                        "    {}: {} = {}\n",
                        field.name, field.type_src, default
                    )),
                    None => {
                        out.push_gen(&format!("    {}: {}\n", field.name, field.type_src));
                    }
                }
            }
            emit_variant_name_reset(out, enum_name, &variant.name, &mangled);
            out.push_gen(&format!("{enum_name}.{} = {mangled}\n", variant.name));
        }
    }
}

/// Reset a variant subclass's `__name__`/`__qualname__` from the private mangled
/// name (`_Enum_Variant`) to the public qualified form (`Enum.Variant`), so
/// dataclass `repr` and `type(x).__name__` read naturally rather than leaking
/// the internal name.
fn emit_variant_name_reset(out: &mut Out, enum_name: &str, variant: &str, mangled: &str) {
    out.push_gen(&format!("{mangled}.__name__ = {variant:?}\n"));
    out.push_gen(&format!(
        "{mangled}.__qualname__ = {:?}\n",
        format!("{enum_name}.{variant}")
    ));
}

/// Copy a member statement verbatim (including its indentation and any
/// decorators), preserving its source-line origins in the map.
fn emit_member(out: &mut Out, source: &str, stmt: &Stmt) {
    let start = line_start(source, member_start(stmt));
    let end = stmt.range().end();
    out.push_verbatim(source, TextRange::new(start, end));
    if !out.text.ends_with('\n') {
        out.push_gen("\n");
    }
}

/// Start offset of a statement including any leading decorators.
fn member_start(stmt: &Stmt) -> TextSize {
    let decorators = match stmt {
        Stmt::FunctionDef(f) => f.decorator_list.first(),
        Stmt::ClassDef(c) => c.decorator_list.first(),
        _ => None,
    };
    match decorators {
        Some(dec) => dec.range().start().min(stmt.range().start()),
        None => stmt.range().start(),
    }
}

fn slice(source: &str, range: TextRange) -> &str {
    &source[usize::from(range.start())..usize::from(range.end())]
}

fn line_of(source: &str, offset: TextSize) -> u32 {
    u32::try_from(
        source[..usize::from(offset)]
            .bytes()
            .filter(|&b| b == b'\n')
            .count(),
    )
    .unwrap_or(0)
}

/// Accumulates the rewritten output text alongside its per-line origin map.
#[derive(Default)]
struct Out {
    text: String,
    /// one entry per *completed* output line (i.e. per `\n` emitted)
    line_map: Vec<Option<u32>>,
}

impl Out {
    /// Append generated text whose lines have no `.by` origin.
    fn push_gen(&mut self, s: &str) {
        for ch in s.chars() {
            self.text.push(ch);
            if ch == '\n' {
                self.line_map.push(None);
            }
        }
    }

    /// Drop a single trailing newline (and its map entry) if present.
    fn pop_trailing_newline(&mut self) {
        if self.text.ends_with('\n') {
            self.text.pop();
            self.line_map.pop();
        }
    }

    /// Append a verbatim copy of `source[range]`, mapping each completed line
    /// back to the source line it came from.
    fn push_verbatim(&mut self, source: &str, range: TextRange) {
        if range.is_empty() {
            return;
        }
        let mut src_line = line_of(source, range.start());
        for ch in slice(source, range).chars() {
            self.text.push(ch);
            if ch == '\n' {
                self.line_map.push(Some(src_line));
                src_line += 1;
            }
        }
    }
}

/// A small set of `from <module> import <name>` requests, deduplicated and
/// merged per module.
#[derive(Default)]
struct ImportSet {
    modules: BTreeMap<&'static str, Vec<&'static str>>,
}

impl ImportSet {
    fn add(&mut self, module: &'static str, name: &'static str) {
        let names = self.modules.entry(module).or_default();
        if !names.contains(&name) {
            names.push(name);
        }
    }

    fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for (module, names) in &self.modules {
            let _ = writeln!(out, "from {module} import {}", names.join(", "));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, PythonVersion, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    /// Transpile targeting a specific minimum version (clean native PEP 695 at
    /// 3.13, no generics polyfill).
    fn check_at(input: &str, min_version: PythonVersion, expected: &str) {
        let config = Config {
            min_version,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    #[test]
    fn all_unit_lowers_to_enum() {
        // one `case` line may declare several comma-separated variants
        check(
            indoc! {"
                enum class Color:
                    case Red, Green
                    case Blue
            "},
            indoc! {"
                from __future__ import annotations
                from enum import Enum, auto
                class Color(Enum):
                    Red = auto()
                    Green = auto()
                    Blue = auto()
            "},
        );
    }

    #[test]
    fn all_unit_with_constant_avoids_enum_form() {
        // python's `Enum` turns every class-body assignment into a *member* —
        // `E.MAX + 5` would crash at runtime while the checker types `MAX` as a
        // constant. an assignment member therefore disqualifies the `Enum`
        // lowering; the sealed hierarchy keeps constants as constants
        check(
            indoc! {"
                enum class E:
                    case A
                    MAX = 10
            "},
            indoc! {"
                from __future__ import annotations
                class E:
                    MAX = 10

                class _E_A(E):
                    __slots__ = ()
                    def __repr__(self): return \"A\"
                _E_A.__name__ = \"A\"
                _E_A.__qualname__ = \"E.A\"
                E.A = _E_A()
            "},
        );
    }

    #[test]
    fn payload_lowers_to_sealed_hierarchy() {
        check(
            indoc! {"
                enum class Shape:
                    case Circle(radius: int)
                    case Point
            "},
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Shape:
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _Shape_Circle(Shape):
                    radius: int
                _Shape_Circle.__name__ = \"Circle\"
                _Shape_Circle.__qualname__ = \"Shape.Circle\"
                Shape.Circle = _Shape_Circle

                class _Shape_Point(Shape):
                    __slots__ = ()
                    def __repr__(self): return \"Point\"
                _Shape_Point.__name__ = \"Point\"
                _Shape_Point.__qualname__ = \"Shape.Point\"
                Shape.Point = _Shape_Point()
            "},
        );
    }

    #[test]
    fn named_tuple_variants_with_defaults() {
        // named fields may carry defaults, making them optional at the call
        // site (positionally or as keywords, like any dataclass field)
        check(
            indoc! {"
                enum class Shape:
                    case Rectangle(width: int, height: int)
                    case Polygon(sides: int, closed: bool = True)
            "},
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Shape:
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _Shape_Rectangle(Shape):
                    width: int
                    height: int
                _Shape_Rectangle.__name__ = \"Rectangle\"
                _Shape_Rectangle.__qualname__ = \"Shape.Rectangle\"
                Shape.Rectangle = _Shape_Rectangle

                @final
                @dataclass(frozen=True, slots=True)
                class _Shape_Polygon(Shape):
                    sides: int
                    closed: bool = True
                _Shape_Polygon.__name__ = \"Polygon\"
                _Shape_Polygon.__qualname__ = \"Shape.Polygon\"
                Shape.Polygon = _Shape_Polygon
            "},
        );
    }

    #[test]
    fn positional_anonymous_fields_get_synthetic_names() {
        check(
            indoc! {"
                enum class Value:
                    case Pair(int, str)
                    case Nothing
            "},
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Value:
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _Value_Pair(Value):
                    _0: int
                    _1: str
                _Value_Pair.__name__ = \"Pair\"
                _Value_Pair.__qualname__ = \"Value.Pair\"
                Value.Pair = _Value_Pair

                class _Value_Nothing(Value):
                    __slots__ = ()
                    def __repr__(self): return \"Nothing\"
                _Value_Nothing.__name__ = \"Nothing\"
                _Value_Nothing.__qualname__ = \"Value.Nothing\"
                Value.Nothing = _Value_Nothing()
            "},
        );
    }

    #[test]
    fn methods_on_base_are_lowered() {
        // the verbatim-copied method body still flows through the downstream
        // passes: the `float` return annotation becomes `JustFloat`
        check(
            indoc! {"
                enum class Shape:
                    case Circle(radius: int)

                    def area(self) -> float:
                        return 0.0
            "},
            indoc! {"
                from __future__ import annotations
                from ty_extensions import JustFloat
                from dataclasses import dataclass
                from typing import final
                class Shape:
                    def area(self) -> JustFloat:
                        return 0.0

                @final
                @dataclass(frozen=True, slots=True)
                class _Shape_Circle(Shape):
                    radius: int
                _Shape_Circle.__name__ = \"Circle\"
                _Shape_Circle.__qualname__ = \"Shape.Circle\"
                Shape.Circle = _Shape_Circle
            "},
        );
    }

    #[test]
    fn unit_enum_keeps_methods() {
        check(
            indoc! {"
                enum class Direction:
                    case North
                    case South

                    def label(self) -> str:
                        return self.name
            "},
            indoc! {"
                from __future__ import annotations
                from enum import Enum, auto
                class Direction(Enum):
                    North = auto()
                    South = auto()
                    def label(self) -> str:
                        return self.name
            "},
        );
    }

    #[test]
    fn payload_variant_omits_slots_below_py310() {
        // `@dataclass(slots=…)` is a 3.10+ option; targeting 3.9 must omit it so
        // the output runs (a frozen dataclass already blocks attribute mutation)
        check_at(
            indoc! {"
                enum class Shape:
                    case Circle(radius: int)
            "},
            PythonVersion::PY39,
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Shape:
                    pass

                @final
                @dataclass(frozen=True)
                class _Shape_Circle(Shape):
                    radius: int
                _Shape_Circle.__name__ = \"Circle\"
                _Shape_Circle.__qualname__ = \"Shape.Circle\"
                Shape.Circle = _Shape_Circle
            "},
        );
    }

    #[test]
    fn zero_field_payload_variant_gets_pass_body() {
        // `A()` is a no-argument constructor — distinct from the unit
        // value `A` — so it lowers to a fieldless dataclass with a `pass` body
        // (without the body the emitted class would be invalid Python)
        check(
            indoc! {"
                enum class E:
                    case A()
                    case B
            "},
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class E:
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _E_A(E):
                    pass
                _E_A.__name__ = \"A\"
                _E_A.__qualname__ = \"E.A\"
                E.A = _E_A

                class _E_B(E):
                    __slots__ = ()
                    def __repr__(self): return \"B\"
                _E_B.__name__ = \"B\"
                _E_B.__qualname__ = \"E.B\"
                E.B = _E_B()
            "},
        );
    }

    #[test]
    fn generic_enum_polyfill_renames_nested_variant_fields() {
        // on the 3.10 polyfill path the enum's type params become module-level
        // `TypeVar`s (`T` → `_T`); the references inside the variant subclass
        // fields must be renamed to match, or they would name an undefined `T`
        check_at(
            indoc! {"
                enum class Result[T, E]:
                    case Ok(T)
                    case Err(E)
            "},
            PythonVersion::PY310,
            indoc! {"
                from __future__ import annotations
                from typing import TypeVar, Generic
                from dataclasses import dataclass
                from typing import final
                _T = TypeVar(\"_T\")
                _E = TypeVar(\"_E\")
                class Result(Generic[_T, _E]):
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _Result_Ok(Result):
                    _0: _T
                _Result_Ok.__name__ = \"Ok\"
                _Result_Ok.__qualname__ = \"Result.Ok\"
                Result.Ok = _Result_Ok

                @final
                @dataclass(frozen=True, slots=True)
                class _Result_Err(Result):
                    _0: _E
                _Result_Err.__name__ = \"Err\"
                _Result_Err.__qualname__ = \"Result.Err\"
                Result.Err = _Result_Err
            "},
        );
    }

    #[test]
    fn generic_enum_result() {
        check_at(
            indoc! {"
                enum class Result[T, E]:
                    case Ok(T)
                    case Err(E)
            "},
            PythonVersion::PY313,
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Result[T, E]:
                    pass

                @final
                @dataclass(frozen=True, slots=True)
                class _Result_Ok(Result):
                    _0: T
                _Result_Ok.__name__ = \"Ok\"
                _Result_Ok.__qualname__ = \"Result.Ok\"
                Result.Ok = _Result_Ok

                @final
                @dataclass(frozen=True, slots=True)
                class _Result_Err(Result):
                    _0: E
                _Result_Err.__name__ = \"Err\"
                _Result_Err.__qualname__ = \"Result.Err\"
                Result.Err = _Result_Err
            "},
        );
    }

    #[test]
    fn generic_recursive_enum_tree() {
        // every variant subclasses the enum (so it inherits methods and is a
        // subtype); the unit variant is a singleton instance, the payload
        // variant a frozen dataclass carrying the type params
        check_at(
            indoc! {"
                enum class Tree[T]:
                    case Leaf
                    case Node(T, Tree[T], Tree[T])
            "},
            PythonVersion::PY313,
            indoc! {"
                from __future__ import annotations
                from dataclasses import dataclass
                from typing import final
                class Tree[T]:
                    pass

                class _Tree_Leaf(Tree):
                    __slots__ = ()
                    def __repr__(self): return \"Leaf\"
                _Tree_Leaf.__name__ = \"Leaf\"
                _Tree_Leaf.__qualname__ = \"Tree.Leaf\"
                Tree.Leaf = _Tree_Leaf()

                @final
                @dataclass(frozen=True, slots=True)
                class _Tree_Node(Tree):
                    _0: T
                    _1: Tree[T]
                    _2: Tree[T]
                _Tree_Node.__name__ = \"Node\"
                _Tree_Node.__qualname__ = \"Tree.Node\"
                Tree.Node = _Tree_Node
            "},
        );
    }

    #[test]
    fn enum_with_no_variants_is_an_error() {
        let err = transpile(
            "enum class Empty:\n    def f(self): ...\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(err.contains("at least one variant"), "got: {err}");
    }

    #[test]
    fn duplicate_variant_name_is_an_error() {
        // variants lower to subclasses attached as `A.Name`, so a within-enum
        // collision would clash rather than miscompile — reject it. (the same
        // name in two *different* enums is fine: `A.Same` vs `B.Same`.)
        let err = transpile(
            "enum class A:\n    case Same(int)\n    case Same(str)\n    case Y\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(err.contains("declared more than once"), "got: {err}");
    }

    #[test]
    fn same_variant_name_across_enums_is_allowed() {
        // variants are qualified (`A.Same`, `B.Same`), so no collision
        transpile(
            "enum class A:\n    case Same(int)\n    case X\nenum class B:\n    case Same(str)\n    case Y\n",
            &Config::test_default(),
        )
        .unwrap();
    }

    #[test]
    fn default_before_required_is_an_error() {
        // a defaulted field before a required one is a dataclass error at import
        // time; surface it as a by-level diagnostic
        let err = transpile(
            "enum class E:\n    case V(a: int = 1, b: int)\n    case Z\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(err.contains("without a default"), "got: {err}");
    }
}
