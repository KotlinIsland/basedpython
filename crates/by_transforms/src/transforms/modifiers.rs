//! Rewrites basedpython modifier keywords to Python decorator equivalents.
//!
//! The parser synthesises a `Decorator` node for each modifier keyword — the
//! decorator range covers the keyword text plus trailing whitespace up to but
//! not including the following `class`/`def` token. Because that text starts
//! with a letter rather than `@`, this transform can distinguish synthetic
//! decorators from real ones.
//!
//! Modifier → output mapping
//! ─────────────────────────
//! `final class/def`       → `@final` (from `typing`)
//! `abstract class`        → (modifier deleted, no decorator)
//! `abstract def`          → `@abstractmethod` (from `abc`)
//! `override def`          → `@override` (from `typing`)
//! `open class`            → (modifier deleted, no decorator)
//! `static def`            → `@staticmethod`
//! `class def`             → `@classmethod`
//! `data class`            → `@dataclass(slots=True)` (from `dataclasses`)
//! `frozen data class`     → `@dataclass(frozen=True, slots=True)`
//!                           below 3.10 `slots=True` is the runtime's `@_by_dataclass_slots` instead
//! `let x = 5`             → `x: Final = 5` (from `typing`)
//! `class a = 1`           → `a: ClassVar = 1` (from `typing`)
//! `newtype Foo = int`     → `Foo = NewType("Foo", int)` (from `typing`)
//! `export`/`public`       → modifier deleted; symbol name added to auto-generated `__all__`
//! `private`               → modifier deleted; symbol renamed with `_` prefix and excluded from `__all__`
//! `private type X = V`    → `type _X = V` (the modifier is a node flag, not a synthetic decorator)

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::helpers::is_immutable_scalar_default;
use ruff_python_ast::helpers::{
    DeclarationMarker, DeclarationMarkerKind, MemberVisibility, declaration_marker_visibility,
    is_classvar_annot_marker_id, is_classvar_marker_id, is_final_marker_id, is_let_marker_id,
};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    self as ast, Expr, Stmt, StmtAnnAssign, StmtClassDef, StmtFunctionDef, StmtTypeAlias,
};
use ruff_python_stdlib::basedpython::visibility_rename;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};
use std::collections::HashMap;

use super::ast_driver::{AstPass, EpilogueLine, Fragment, PassContext};
use super::repeated_underscore::WrittenNames;
use crate::type_info::TypeInfo;
use crate::{Config, PythonVersion};

/// basedpython: the visibility a modifier keyword records, read off the
/// synthetic decorator the parser leaves in its place
fn visibility_of(modifier: &str) -> MemberVisibility {
    match modifier {
        "private" => MemberVisibility::Private,
        "protected" => MemberVisibility::Protected,
        _ => MemberVisibility::Public,
    }
}

/// whether a dataclass field's default is already a field specifier —
/// `field(...)` / `dataclasses.field(...)` — which carries its own factory
fn is_field_specifier(default: &Expr) -> bool {
    let Expr::Call(call) = default else {
        return false;
    };
    match call.func.as_ref() {
        Expr::Name(name) => name.id.as_str() == "field",
        Expr::Attribute(attribute) => attribute.attr.as_str() == "field",
        _ => false,
    }
}

/// whether an annotation declares `ClassVar[...]`, which `dataclasses` treats
/// as class state rather than a field
fn is_class_var(annotation: &Expr) -> bool {
    let head = match annotation {
        Expr::Subscript(subscript) => subscript.value.as_ref(),
        other => other,
    };
    match head {
        Expr::Name(name) => name.id.as_str() == "ClassVar",
        Expr::Attribute(attribute) => attribute.attr.as_str() == "ClassVar",
        _ => false,
    }
}

#[expect(clippy::struct_excessive_bools)]
pub(crate) struct Modifiers<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    edits: Vec<Fix>,
    /// Needs `from typing import final` (decorator for classes/methods)
    needs_final: bool,
    /// Needs `from typing import Final` (type annotation for constants)
    needs_final_annotation: bool,
    needs_abstractmethod: bool,
    needs_override: bool,
    needs_dataclass: bool,
    /// a mutable field default was rewritten to a `default_factory`
    needs_dataclass_field: bool,
    needs_protocol: bool,
    needs_classvar: bool,
    needs_newtype: bool,
    /// whether a `data class` is given its slots by the runtime helper, on a python whose
    /// `dataclass` has no `slots` option
    slots_by_helper: bool,
    /// whether a `data class` whose methods reach the class through a `__class__` cell is
    /// given its slots by the runtime helper, on a python whose `dataclass(slots=True)`
    /// leaves that cell pointing at the class it replaced
    slots_by_helper_for_class_cell: bool,
    needs_dataclass_slots: bool,
    /// Names marked `export`/`public` at module level. Used to generate `__all__`.
    exports: Vec<String>,
    /// Module-level names renamed by `private` (original → `_original`).
    private_renames: Vec<String>,
    /// for each class statement that is a member of a `sealed` class, keyed by its range,
    /// the ranges of the names of the sealed classes it is a member of
    sealed_listings: &'src HashMap<TextRange, Vec<TextRange>>,
    /// the classes declared `sealed`, in source order
    sealed_classes: Vec<DeclaredClass>,
    /// the members of sealed classes, in source order, each with the names of the sealed
    /// classes it is a member of
    sealed_members: Vec<(DeclaredClass, &'src [TextRange])>,
    /// Tracks the current class-nesting depth so visibility modifiers can
    /// distinguish module-level declarations from class members.
    class_depth: u32,

    /// tracks the current function-nesting depth, so a declaration inside a
    /// function body is not taken for a module-level one
    func_depth: u32,
    /// whether the innermost enclosing scope is a class body — where a
    /// visibility keyword names a member — rather than a function body
    class_body: bool,
}

impl<'src> Modifiers<'src> {
    fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        sealed_listings: &'src HashMap<TextRange, Vec<TextRange>>,
    ) -> Self {
        Self {
            source,
            written,
            edits: Vec::new(),
            needs_final: false,
            needs_final_annotation: false,
            needs_abstractmethod: false,
            needs_override: false,
            needs_dataclass: false,
            needs_dataclass_field: false,
            needs_protocol: false,
            needs_classvar: false,
            needs_newtype: false,
            slots_by_helper: false,
            slots_by_helper_for_class_cell: false,
            needs_dataclass_slots: false,
            exports: Vec::new(),
            private_renames: Vec::new(),
            sealed_listings,
            sealed_classes: Vec::new(),
            sealed_members: Vec::new(),
            class_depth: 0,
            func_depth: 0,
            class_body: false,
        }
    }

    /// True when the visitor is directly at module scope — not inside any class
    /// or function body.
    fn at_module_level(&self) -> bool {
        self.class_depth == 0 && self.func_depth == 0
    }

    fn line_indent(&self, pos: TextSize) -> &str {
        super::source_util::line_indent(self.source, pos)
    }

    /// The range an assignment's value occupies, grouping parentheses included.
    /// A modifier lowering replaces everything the statement said ahead of the
    /// value, and an expression's range starts inside its parentheses, so
    /// without this the `(` of a multi-line value is eaten and its `)` stranded
    fn value_range(&self, stmt: &StmtAnnAssign, value: &Expr) -> TextRange {
        // the scan stops at the annotation, which is the last thing written
        // before the value — past it lies only `=`, whitespace, parentheses and
        // comments, and a `#` there is unambiguously one
        let written_before = stmt
            .annotation
            .range()
            .end()
            .max(stmt.target.range().end())
            .max(stmt.range().start());
        super::source_util::parenthesized_value_range(self.source, value.range(), written_before)
    }

    fn is_synthetic(&self, dec: &ruff_python_ast::Decorator) -> bool {
        super::source_util::is_synthetic_decorator(self.source, dec)
    }

    /// Return the `id` of the synthetic decorator's Name expression, or `None`
    /// if the decorator is a normal `@…` decorator.
    fn synthetic_name<'a>(&self, dec: &'a ruff_python_ast::Decorator) -> Option<&'a str> {
        if !self.is_synthetic(dec) {
            return None;
        }
        if let Expr::Name(n) = &dec.expression {
            Some(n.id.as_str())
        } else {
            None
        }
    }

    /// `dataclasses` rejects a mutable field default outright — `tags: list[str]
    /// = []` raises `ValueError` when the class body runs — so every non-scalar
    /// default becomes a `default_factory`, re-evaluated per instance.
    ///
    /// This is the same rule [default re-evaluation](super::mutable_defaults)
    /// applies to function parameters, and the same reading: a default written
    /// in the source is written per call, not once.
    fn wrap_mutable_field_defaults(&mut self, class: &StmtClassDef) {
        for stmt in &class.body {
            let Stmt::AnnAssign(field) = stmt else {
                continue;
            };
            let Some(default) = field.value.as_deref() else {
                continue;
            };
            if is_immutable_scalar_default(default) || is_field_specifier(default) {
                continue;
            }
            // a `ClassVar` is shared class state, not a field — `dataclasses`
            // leaves it alone, and so must we
            if is_class_var(&field.annotation) {
                continue;
            }
            self.needs_dataclass_field = true;
            // two insertions rather than a replacement, so the lowerings written
            // inside the default still apply to it where it now sits
            self.edits.push(Fix::safe_edit(Edit::insertion(
                format!(
                    "{}(default_factory=lambda: ",
                    self.written.imported("dataclasses", "field")
                ),
                default.range().start(),
            )));
            self.edits.push(Fix::safe_edit(Edit::insertion(
                ")".to_owned(),
                default.range().end(),
            )));
        }
    }

    fn process_class(&mut self, class: &StmtClassDef) {
        // the name the class statement binds once the modifiers below are lowered
        let mut emitted = class.name.as_str().to_owned();
        let mut sealed = false;
        for dec in &class.decorator_list {
            let Some(name) = self.synthetic_name(dec) else {
                continue;
            };
            let indent = self.line_indent(dec.range().start()).to_owned();
            match name {
                "abstract" | "open" => {
                    // Just remove the modifier prefix, no decorator needed.
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                }
                "sealed" => {
                    // the class gets its `__sealed_members__` tuple, and each member is
                    // added to it, once every class statement has been seen
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    sealed = true;
                }
                "final" => {
                    self.needs_final = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@{}\n{indent}", self.written.imported("typing", "final")),
                        dec.range(),
                    )));
                }
                "data_class" | "frozen_data_class" => {
                    self.needs_dataclass = true;
                    let dataclass = self.written.imported("dataclasses", "dataclass");
                    let frozen = name == "frozen_data_class";
                    let decorators = if self.slots_by_helper
                        || (self.slots_by_helper_for_class_cell && reads_class_cell(&class.body))
                    {
                        self.needs_dataclass_slots = true;
                        let options = if frozen { "(frozen=True)" } else { "" };
                        format!(
                            "@{}\n{indent}@{dataclass}{options}\n{indent}",
                            crate::runtime::DATACLASS_SLOTS.name()
                        )
                    } else {
                        let frozen = if frozen { "frozen=True, " } else { "" };
                        format!("@{dataclass}({frozen}slots=True)\n{indent}")
                    };
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        decorators,
                        dec.range(),
                    )));
                    self.wrap_mutable_field_defaults(class);
                }
                "protocol_class" => {
                    self.needs_protocol = true;
                    // Replace "protocol " with "class "; Protocol base is inserted separately.
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        "class ".to_owned(),
                        dec.range(),
                    )));
                    // a class with type params and no explicit bases has its base
                    // list built by the generics pass (native `[T]` on 3.12+,
                    // `(Generic[_T])` on the legacy path), which adds `Protocol`
                    // there — so the two passes don't emit competing base-parens
                    // around the type params. every other shape keeps the base here
                    if !(class.type_params.is_some() && class.arguments.is_none()) {
                        self.insert_protocol_base(class);
                    }
                }
                "export" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.class_depth == 0 {
                        self.exports.push(class.name.as_str().to_owned());
                    }
                }
                "private" | "protected" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.at_module_level() {
                        self.private_renames.push(class.name.as_str().to_owned());
                        emitted = self.written.module_private(class.name.as_str());
                        self.rename_with_underscore(class.name.range());
                    } else if self.class_body {
                        // a nested class is a member like any other: its
                        // visibility is spelled in its name, which for `private`
                        // is the `__name` python name-mangles out of subclass
                        // scope
                        if let Some(renamed) = visibility_rename(
                            class.name.as_str(),
                            visibility_of(name).name_prefix(),
                        ) {
                            emitted = renamed;
                        }
                        self.rename_with_visibility(&class.name, visibility_of(name));
                    }
                }
                _ => {}
            }
        }
        let declared = || DeclaredClass {
            name: class.name.range(),
            emitted: emitted.clone(),
            statement: class.range(),
        };
        if sealed {
            self.sealed_classes.push(declared());
        }
        if let Some(listing) = self.sealed_listings.get(&class.range()) {
            self.sealed_members.push((declared(), listing));
        }
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
        for dec in &func.decorator_list {
            let Some(name) = self.synthetic_name(dec) else {
                continue;
            };
            let indent = self.line_indent(dec.range().start()).to_owned();
            match name {
                "abstract" => {
                    self.needs_abstractmethod = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!(
                            "@{}\n{indent}",
                            self.written.imported("abc", "abstractmethod")
                        ),
                        dec.range(),
                    )));
                }
                "final" => {
                    self.needs_final = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@{}\n{indent}", self.written.imported("typing", "final")),
                        dec.range(),
                    )));
                }
                "override" => {
                    self.needs_override = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@{}\n{indent}", self.written.imported("typing", "override")),
                        dec.range(),
                    )));
                }
                "open" => {
                    // `open` does not apply to functions; treat as no-op.
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                }
                "static" => {
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@{}\n{indent}", self.written.builtin("staticmethod")),
                        dec.range(),
                    )));
                }
                "classmethod" => {
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@{}\n{indent}", self.written.builtin("classmethod")),
                        dec.range(),
                    )));
                }
                "export" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.class_depth == 0 {
                        self.exports.push(func.name.as_str().to_owned());
                    }
                }
                "private" | "protected" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.at_module_level() {
                        self.private_renames.push(func.name.as_str().to_owned());
                        self.rename_with_underscore(func.name.range());
                    } else if self.class_body {
                        // a `private` method is name-mangled to `__name`, a
                        // `protected` one renamed to `_name`. a dunder is left
                        // alone: python calls it by its exact name, so renaming
                        // would change what the method *is* rather than who can
                        // reach it. the one dunder where the keyword says
                        // something, `__init__`, is checked by ty at the
                        // construction site instead
                        self.rename_with_visibility(&func.name, visibility_of(name));
                    }
                }
                _ => {}
            }
        }
    }

    /// Replace the identifier at `range` with its module-private spelling.
    fn rename_with_underscore(&mut self, range: TextRange) {
        let original = self.src(range).to_owned();
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            self.written.module_private(&original),
            range,
        )));
    }

    /// Replace `name` with the spelling `visibility` gives it. A no-op for a name
    /// the rename would not hide — see [`visibility_rename`].
    ///
    /// The decision is made on the name the definition *has*, not on the source
    /// the range covers: an `init(…)` shorthand is named `__init__` while its
    /// range covers the `init` keyword, and renaming that would leave the class
    /// with no constructor.
    fn rename_with_visibility(&mut self, name: &ast::Identifier, visibility: MemberVisibility) {
        let Some(renamed) = visibility_rename(name.as_str(), visibility.name_prefix()) else {
            return;
        };
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            renamed,
            name.range(),
        )));
    }

    /// Returns the source text for a range.
    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// `private type X = V` → `type _X = V`. the modifier keyword is not a
    /// synthetic decorator here (a type alias has no decorator list), it is a
    /// flag on the node, so the prefix is erased from the source directly. a
    /// module's alias is renamed, at the definition site and every reference,
    /// by the visibility rename, which reads the module's private symbols
    ///
    /// an alias in a class body is not among them, so it keeps its name
    fn process_type_alias(&mut self, alias: &StmtTypeAlias) {
        if !alias.is_private {
            return;
        }
        let start = alias.range().start();
        let Some(after_kw) = self.source[usize::from(start)..].strip_prefix("private") else {
            return;
        };
        let gap = after_kw.len() - after_kw.trim_start().len();
        let Ok(prefix_len) = TextSize::try_from("private".len() + gap) else {
            return;
        };
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                start,
                start + prefix_len,
            ))));

        self.private_renames
            .push(self.src(alias.name.range()).to_owned());
    }

    fn process_ann_assign(&mut self, node: &StmtAnnAssign) {
        // every modifier declaration binds a bare name (`let a: T`), so anything
        // else is a parser-synthesised statement wearing a marker — the
        // `self.<name>: __let__[T] = <name>` an `init(let a: T)` stands for. Those
        // have no source of their own (their ranges point back at the parameter,
        // out of statement order), and the `init_method` pass writes the real
        // lowered line, so there is nothing to rewrite here
        if !node.target.is_name_expr() {
            return;
        }
        let written = self.src(node.target.range()).to_owned();
        // a declaration's visibility is spelled in its name, so every branch
        // below emits the renamed one
        let visibility = declaration_marker_visibility(node.annotation.as_ref());
        let name = if self.class_body {
            visibility_rename(&written, visibility.name_prefix()).unwrap_or(written)
        } else if visibility == MemberVisibility::Private && self.at_module_level() {
            // a module-level `private` variable is renamed the way a module-level
            // `private def` is, and for the same reason. its references are
            // renamed by `visibility_rename`, which asks ty where each one resolves
            self.private_renames.push(written.clone());
            self.written.module_private(&written)
        } else {
            written
        };
        // every rewrite below replaces what the statement said ahead of the value,
        // which starts past any decorators written above it — those belong to the
        // decorated-binding lowering, which erases them and wraps the value
        let stmt_start =
            super::source_util::binding_start(self.source, node.range(), &node.decorator_list);

        // `[modifiers] a: T [= v]` where no modifier carries meaning — erase only
        // the modifier prefix; the rest of the statement stays exactly as written,
        // with or without an initializer
        if let Expr::Subscript(s) = node.annotation.as_ref()
            && matches!(s.value.as_ref(), Expr::Name(n) if DeclarationMarker::from_id(n.id.as_str()).is_some_and(|marker| marker.kind == DeclarationMarkerKind::Annot))
        {
            // one edit spanning the prefix *and* the name: a `private` member is
            // emitted under a different name, and a deletion alone would leave
            // the written one behind
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                name,
                TextRange::new(stmt_start, node.target.range().end()),
            )));
            return;
        }

        let Some(value) = &node.value else {
            // valueless typed `let x: T` / `final x: T` → `x: Final[T]` — a
            // read-only declaration with no initializer, `Final` in every scope
            if let Expr::Subscript(s) = node.annotation.as_ref()
                && matches!(s.value.as_ref(), Expr::Name(n) if is_classvar_annot_marker_id(n.id.as_str()))
            {
                // valueless `class var a: T` → `a: ClassVar[T]`
                let slice = s.slice.as_ref();
                let pre_range = TextRange::new(stmt_start, slice.range().start());
                self.needs_classvar = true;
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("{name}: {}[", self.written.imported("typing", "ClassVar")),
                    pre_range,
                )));
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    "]".to_owned(),
                    slice.range().end(),
                )));
            } else if let Expr::Subscript(s) = node.annotation.as_ref()
                && matches!(s.value.as_ref(), Expr::Name(n) if is_let_marker_id(n.id.as_str()) || is_final_marker_id(n.id.as_str()))
            {
                let slice = s.slice.as_ref();
                // start at the statement, not the marker, so any modifier prefix
                // ahead of `let` (e.g. `override let a: T`) is erased too
                let pre_range = TextRange::new(stmt_start, slice.range().start());
                self.needs_final_annotation = true;
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("{name}: {}[", self.written.imported("typing", "Final")),
                    pre_range,
                )));
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    "]".to_owned(),
                    slice.range().end(),
                )));
            } else if matches!(node.annotation.as_ref(), Expr::Name(n) if is_let_marker_id(n.id.as_str()))
            {
                // valueless untyped `let x` → `x: Final` — an uninitialized
                // read-only declaration
                self.needs_final_annotation = true;
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("{name}: {}", self.written.imported("typing", "Final")),
                    TextRange::new(stmt_start, node.range().end()),
                )));
            }
            return;
        };

        match node.annotation.as_ref() {
            Expr::Name(ann) => {
                // untyped: `let a = v`, `class a = v`, `newtype Foo = v`
                let value_range = self.value_range(node, value);
                let prefix_range = TextRange::new(stmt_start, value_range.start());
                match ann.id.as_str() {
                    id if is_let_marker_id(id) => {
                        self.needs_final_annotation = true;
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("{name}: {} = ", self.written.imported("typing", "Final")),
                            prefix_range,
                        )));
                    }
                    id if DeclarationMarker::from_id(id)
                        .is_some_and(|marker| marker.kind == DeclarationMarkerKind::Assign) =>
                    {
                        // the name and the `=` are two edits, so the type the
                        // inferred-annotation lowering gives a class member lands
                        // between them
                        let target_end = node.target.range().end();
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            name,
                            TextRange::new(stmt_start, target_end),
                        )));
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            " = ".to_owned(),
                            TextRange::new(target_end, value_range.start()),
                        )));
                    }
                    id if is_classvar_marker_id(id) => {
                        self.needs_classvar = true;
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("{name}: {} = ", self.written.imported("typing", "ClassVar")),
                            prefix_range,
                        )));
                    }
                    "__newtype__" => {
                        let value_src = self.src(value_range).to_owned();
                        self.needs_newtype = true;
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!(
                                "{name} = {}(\"{name}\", {value_src})",
                                self.written.imported("typing", "NewType")
                            ),
                            TextRange::new(stmt_start, value_range.end()),
                        )));
                    }
                    _ => {}
                }
            }
            Expr::Subscript(s) if matches!(s.value.as_ref(), Expr::Name(n) if is_classvar_annot_marker_id(n.id.as_str())) =>
            {
                // `class var a: T [= v]` → `a: ClassVar[T] [= v]`
                let slice = s.slice.as_ref();
                let pre_range = TextRange::new(stmt_start, slice.range().start());
                let post_range =
                    TextRange::new(slice.range().end(), self.value_range(node, value).start());
                self.needs_classvar = true;
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("{name}: {}[", self.written.imported("typing", "ClassVar")),
                    pre_range,
                )));
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    "] = ".to_owned(),
                    post_range,
                )));
            }
            Expr::Subscript(s) if matches!(s.value.as_ref(), Expr::Name(n) if is_let_marker_id(n.id.as_str()) || is_final_marker_id(n.id.as_str())) =>
            {
                // typed: `let a: T = v` / `final a: T = v` — annotation is
                // Subscript(__let__|__final__, T). callable transform visits only
                // the slice independently, so emit bracket edits around the slice
                // range; they don't overlap with callable's edit
                let is_final =
                    matches!(s.value.as_ref(), Expr::Name(n) if is_final_marker_id(n.id.as_str()));
                let slice = s.slice.as_ref();
                // start at the statement, not the marker, so any modifier prefix
                // ahead of `let` (e.g. `override let a: T = v`) is erased too
                let pre_range = TextRange::new(stmt_start, slice.range().start());
                let post_range =
                    TextRange::new(slice.range().end(), self.value_range(node, value).start());
                if self.class_depth > 0 && !is_final {
                    // inside class: `a: T = v` (no Final wrapper; keep the type)
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("{name}: "),
                        pre_range,
                    )));
                    // post_range covers ` = ` which matches source; emit as-is
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        " = ".to_owned(),
                        post_range,
                    )));
                } else {
                    self.needs_final_annotation = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("{name}: {}[", self.written.imported("typing", "Final")),
                        pre_range,
                    )));
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        "] = ".to_owned(),
                        post_range,
                    )));
                }
            }
            _ => {}
        }
    }

    fn insert_base_class(&mut self, class: &StmtClassDef, base_name: &str) {
        if let Some(args) = &class.arguments {
            let rparen = args.range().end() - TextSize::from(1);
            if args.args.is_empty() && args.keywords.is_empty() {
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    base_name.to_owned(),
                    rparen,
                )));
            } else {
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    format!(", {base_name}"),
                    rparen,
                )));
            }
        } else {
            let after_name = class.name.range().end();
            self.edits.push(Fix::safe_edit(Edit::insertion(
                format!("({base_name})"),
                after_name,
            )));
        }
    }

    fn insert_protocol_base(&mut self, class: &StmtClassDef) {
        let protocol = self.written.imported("typing", "Protocol");
        self.insert_base_class(class, &protocol);
    }
}

impl<'ast> Visitor<'ast> for Modifiers<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(c) => {
                self.process_class(c);
                // Walk the class body with `class_depth` incremented so nested
                // declarations are not treated as module-level for visibility purposes.
                self.class_depth += 1;
                let enclosing = std::mem::replace(&mut self.class_body, true);
                walk_stmt(self, stmt);
                self.class_body = enclosing;
                self.class_depth -= 1;
                return;
            }
            Stmt::FunctionDef(f) => {
                self.process_function(f);
                // walk the body with `func_depth` incremented so declarations in it
                // are not treated as module-level ones
                self.func_depth += 1;
                let enclosing = std::mem::replace(&mut self.class_body, false);
                walk_stmt(self, stmt);
                self.class_body = enclosing;
                self.func_depth -= 1;
                return;
            }
            Stmt::AnnAssign(a) => {
                self.process_ann_assign(a);
            }
            Stmt::TypeAlias(a) => {
                self.process_type_alias(a);
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

/// a class statement, as the `__sealed_members__` tuples need it
struct DeclaredClass {
    /// the range of the name the class is written with
    name: TextRange,
    /// the name the class statement binds once it is lowered
    emitted: String,
    /// the class statement
    statement: TextRange,
}

/// basedpython: for each class statement of `stmts` that is a member of a `sealed` class, keyed
/// by its range, the ranges of the names of the sealed classes it is a member of. `stmts` must
/// come from the same parse `types` answers for
pub(crate) fn collect_sealed_listings(
    stmts: &[Stmt],
    types: &dyn TypeInfo,
) -> HashMap<TextRange, Vec<TextRange>> {
    struct Collector<'a> {
        types: &'a dyn TypeInfo,
        listings: HashMap<TextRange, Vec<TextRange>>,
    }
    impl<'ast> Visitor<'ast> for Collector<'_> {
        fn visit_stmt(&mut self, stmt: &'ast Stmt) {
            if let Stmt::ClassDef(class) = stmt {
                let listing = self.types.sealed_classes_listing(class);
                if !listing.is_empty() {
                    self.listings.insert(class.range(), listing);
                }
            }
            walk_stmt(self, stmt);
        }
    }
    let mut collector = Collector {
        types,
        listings: HashMap::new(),
    };
    collector.visit_body(stmts);
    collector.listings
}

/// The name a module-level `private` symbol is emitted under: one leading
/// underscore, which is python's own mark for "not part of the interface".
///
/// A name that already has one is left alone. Adding a second would make it a
/// `__name`, and python name-mangles every `__name` it reads inside a class
/// body — so `private def _has_room` would become `__has_room` at module level
/// and be looked up as `_Diagnostics__has_room` from a method that calls it.
pub(crate) fn module_private_name(name: &str) -> String {
    if name.starts_with('_') {
        return name.to_owned();
    }
    format!("_{name}")
}

/// whether any function in a class body can reach the class through a `__class__`
/// cell. python gives a class that cell when a function in its body names `super` or
/// `__class__`, so a body naming neither has none. a name anywhere in the body counts,
/// a nested class's included, which can only count a class that has no cell
fn reads_class_cell(body: &[Stmt]) -> bool {
    struct Finder(bool);
    impl<'a> Visitor<'a> for Finder {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && matches!(name.id.as_str(), "super" | "__class__")
            {
                self.0 = true;
            }
            walk_expr(self, expr);
        }
    }
    let mut finder = Finder(false);
    finder.visit_body(body);
    finder.0
}

pub(crate) struct ModifiersPass<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    /// see [`collect_sealed_listings`]
    sealed_listings: HashMap<TextRange, Vec<TextRange>>,
    is_stub: bool,
    /// whether the python the module targets has `dataclass(slots=True)`
    dataclass_slots: bool,
    /// whether the `dataclass(slots=True)` of the python the module targets re-points
    /// the `__class__` cell of the class's methods at the class it makes, which 3.13
    /// began doing. before it, zero-argument `super()` in a method raises a `TypeError`
    dataclass_slots_keep_class_cell: bool,
}

impl<'src> ModifiersPass<'src> {
    pub(crate) fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        sealed_listings: HashMap<TextRange, Vec<TextRange>>,
        config: &Config,
    ) -> Self {
        Self {
            source,
            written,
            sealed_listings,
            is_stub: config.is_stub,
            dataclass_slots: config.min_version >= PythonVersion::PY310,
            dataclass_slots_keep_class_cell: config.min_version >= PythonVersion::PY313,
        }
    }
}

impl AstPass for ModifiersPass<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::Modifiers)
    }

    /// a declaration whose value a statement expression moves below its suite is re-written
    /// from its `let` prefix, which carries the separator the move normalises
    fn subsumes(&self) -> &'static [super::ast_driver::Lowering] {
        &[super::ast_driver::Lowering::StatementExpression]
    }

    fn run(&self, module: &mut ruff_python_ast::ModModule, ctx: &mut PassContext) {
        let mut inner = Modifiers::new(self.source, self.written, &self.sealed_listings);
        // a stub is never run, so it keeps the option a checker reads
        inner.slots_by_helper = !self.dataclass_slots && !self.is_stub;
        inner.slots_by_helper_for_class_cell =
            !self.dataclass_slots_keep_class_cell && !self.is_stub;
        for stmt in &module.body {
            inner.visit_stmt(stmt);
        }
        let exports = std::mem::take(&mut inner.exports);
        let private_renames = std::mem::take(&mut inner.private_renames);
        let sealed_classes = std::mem::take(&mut inner.sealed_classes);
        let sealed_members = std::mem::take(&mut inner.sealed_members);

        if inner.needs_dataclass_slots {
            ctx.runtime.insert(crate::runtime::DATACLASS_SLOTS);
        }
        for (needed, module, name) in [
            (inner.needs_final, "typing", "final"),
            (inner.needs_final_annotation, "typing", "Final"),
            (inner.needs_classvar, "typing", "ClassVar"),
            (inner.needs_newtype, "typing", "NewType"),
            (inner.needs_override, "typing", "override"),
            (inner.needs_abstractmethod, "abc", "abstractmethod"),
            (inner.needs_dataclass, "dataclasses", "dataclass"),
            (inner.needs_dataclass_field, "dataclasses", "field"),
            (inner.needs_protocol, "typing", "Protocol"),
        ] {
            if needed {
                ctx.required_imports
                    .push(self.written.import_from(module, &[name]));
            }
        }

        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }

        // the references to a renamed module-level symbol are rewritten by
        // `visibility_rename`, which asks ty which binding each one resolves to —
        // a parameter or class attribute that shares the name is not the symbol

        // `private` renames the symbol, so a name it claims is not the name the
        // module ends up with — exporting it would put a name in `__all__` that
        // the module has no attribute for, and `import *` would raise. `private`
        // takes a symbol off the public surface, which is what `__all__` lists,
        // so it wins over an `export` written alongside it
        let entries = exports
            .iter()
            .filter(|name| !private_renames.contains(name))
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>();
        if !entries.is_empty() {
            ctx.epilogue.push(EpilogueLine::unattributed(format!(
                "__all__ = [{}]",
                entries.join(", ")
            )));
        }

        // the members tuple is built as the module runs, and a stub never runs. it
        // declares the classes, which is all a checker reads of them
        if !self.is_stub {
            self.add_sealed_members(&sealed_classes, &sealed_members, ctx);
        }
    }
}

impl ModifiersPass<'_> {
    /// give each sealed class its `__sealed_members__` tuple, right after its class
    /// statement, and add each member to the tuple of each sealed class it is a member of,
    /// right after the member's own class statement and in the block it is written in.
    /// ty decides which classes are members, so a base written through an alias counts
    ///
    /// the tuple then holds the members whose statements have run, in the order they ran,
    /// which is what ty types it as: a member declared under an `if` that is not taken, in
    /// a loop, or after a `return`, is in it only once its statement has run, and code after
    /// a member in the same block can read it. a member's statement is in the scope that
    /// binds the sealed class, so the sealed class's name is bound wherever it runs
    fn add_sealed_members(
        &self,
        sealed_classes: &[DeclaredClass],
        sealed_members: &[(DeclaredClass, &[TextRange])],
        ctx: &mut PassContext,
    ) {
        // the lines to insert after each class statement, which for a sealed class that is
        // also a member of another is both its own tuple and its addition to the other's
        let mut inserts: Vec<(TextRange, Vec<String>)> = Vec::new();
        let mut insert = |statement: TextRange, line: String| match inserts
            .iter_mut()
            .find(|(after, _)| *after == statement)
        {
            Some((_, lines)) => lines.push(line),
            None => inserts.push((statement, vec![line])),
        };
        for sealed in sealed_classes {
            insert(
                sealed.statement,
                format!("{}.__sealed_members__ = ()", sealed.emitted),
            );
        }
        for (member, listing) in sealed_members {
            for name in *listing {
                let Some(sealed) = sealed_classes.iter().find(|sealed| sealed.name == *name) else {
                    continue;
                };
                insert(
                    member.statement,
                    format!(
                        "{}.__sealed_members__ += ({},)",
                        sealed.emitted, member.emitted
                    ),
                );
            }
        }
        for (statement, lines) in inserts {
            let indent = super::source_util::line_indent(self.source, statement.start());
            let mut text = String::new();
            for line in lines {
                text.push_str(indent);
                text.push_str(&line);
                text.push('\n');
            }
            let end = statement.end().to_usize();
            // at the start of the line after the class statement, so the edit never
            // collides with `empty_declarations`' mid-line `: ...`. without a blank line
            // in between, that is where the next statement starts, so the insert is
            // anchored to the statement boundary: a rewrite of that statement's first
            // expression — a soundness check wrapping it — would otherwise absorb the
            // insert into itself
            match self.source[end..]
                .find('\n')
                .and_then(|newline| TextSize::try_from(end + newline + 1).ok())
            {
                Some(position) => ctx
                    .statement_inserts
                    .push((position, vec![Fragment::Lit(text)])),
                None => ctx.text_edits.push((
                    TextRange::empty(self.source.text_len()),
                    format!("\n{text}"),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn check_at(input: &str, expected: &str, version: PythonVersion) {
        let config = Config {
            min_version: version,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn final_class() {
        check(
            "final class Foo: ...\n",
            indoc! {"
                from typing import final
                @final
                class Foo: ...
            "},
        );
    }

    #[test]
    fn modifier_chain_with_non_leading_final() {
        // `final` anywhere in a modifier chain must parse — it was missing from
        // the chain-continuation set, so `sealed final class` (final non-leading)
        // panicked the parser. modifiers commute, so both orders match.
        let expected = indoc! {"
            from typing import final
            @final
            class A: ...
            A.__sealed_members__ = ()
            class B(A): ...
            A.__sealed_members__ += (B,)
        "};
        check("sealed final class A\nclass B(A)\n", expected);
        check("final sealed class A\nclass B(A)\n", expected);
    }

    #[test]
    fn final_def() {
        check(
            indoc! {"
                class Base:
                    final def method(self): ...
            "},
            indoc! {"
                from typing import final
                class Base:
                    @final
                    def method(self): ...
            "},
        );
    }

    #[test]
    fn abstract_class() {
        check("abstract class Foo: ...\n", "class Foo: ...\n");
    }

    #[test]
    fn abstract_def() {
        check(
            indoc! {"
                class Base:
                    abstract def method(self)
            "},
            indoc! {"
                from abc import abstractmethod
                class Base:
                    @abstractmethod
                    def method(self): raise NotImplementedError
            "},
        );
    }

    #[test]
    fn override_def() {
        // Default min_version is PY310; `typing.override` was added in PY312
        // so it must come from `typing_extensions` (typing_redirect handles this).
        check(
            indoc! {"
                class Child:
                    override def method(self): ...
            "},
            indoc! {"
                from typing_extensions import override
                class Child:
                    @override
                    def method(self): ...
            "},
        );
    }

    #[test]
    fn open_class() {
        check("open class Foo: ...\n", "class Foo: ...\n");
    }

    #[test]
    fn static_def() {
        check(
            indoc! {"
                class A:
                    static def helper(): ...
            "},
            indoc! {"
                class A:
                    @staticmethod
                    def helper(): ...
            "},
        );
    }

    /// the class body binds `staticmethod` itself, so the decorator reads the builtin under
    /// a name of its own
    #[test]
    fn static_def_in_a_class_that_binds_staticmethod() {
        check(
            indoc! {"
                class A:
                    staticmethod = 3
                    static def helper(): ...
            "},
            indoc! {"
                from builtins import staticmethod as staticmethod2
                class A:
                    staticmethod: int = 3
                    @staticmethod2
                    def helper(): ...
            "},
        );
    }

    #[test]
    fn class_def() {
        check(
            indoc! {"
                class A:
                    class def from_str(cls, s: str): ...
            "},
            indoc! {"
                class A:
                    @classmethod
                    def from_str(cls, s: str): ...
            "},
        );
    }

    #[test]
    fn data_class() {
        check(
            "data class Point: ...\n",
            indoc! {"
                from dataclasses import dataclass
                @dataclass(slots=True)
                class Point: ...
            "},
        );
    }

    /// `dataclasses` raises `ValueError` for a mutable field default when the
    /// class body runs, so the default has to become a factory — the same
    /// per-use re-evaluation a function parameter's default already gets
    #[test]
    fn a_mutable_field_default_becomes_a_factory() {
        check(
            indoc! {"
                data class Config:
                    tags: list[str] = []
                    opts: dict[str, int] = {}
            "},
            indoc! {"
                from dataclasses import dataclass, field
                @dataclass(slots=True)
                class Config:
                    tags: list[str] = field(default_factory=lambda: [])
                    opts: dict[str, int] = field(default_factory=lambda: {})
            "},
        );
    }

    /// a scalar default is what `dataclasses` accepts as written, an explicit
    /// `field(...)` already carries its own factory, and a `ClassVar` is class
    /// state rather than a field — none of the three is rewritten
    #[test]
    fn only_a_mutable_field_default_is_rewritten() {
        check(
            indoc! {"
                from typing import ClassVar
                from dataclasses import field

                frozen data class Config:
                    name: str = \"x\"
                    count: int = 0
                    explicit: list[int] = field(default_factory=list)
                    registry: ClassVar[list[str]] = []
            "},
            indoc! {"
                from typing import ClassVar
                from dataclasses import field, dataclass

                @dataclass(frozen=True, slots=True)
                class Config:
                    name: str = \"x\"
                    count: int = 0
                    explicit: list[int] = field(default_factory=list)
                    registry: ClassVar[list[str]] = []
            "},
        );
    }

    #[test]
    fn frozen_data_class() {
        check(
            "frozen data class Point: ...\n",
            indoc! {"
                from dataclasses import dataclass
                @dataclass(frozen=True, slots=True)
                class Point: ...
            "},
        );
    }

    /// `dataclass` has no `slots` option below 3.10, so the runtime's helper does what it
    /// would, to the class `dataclass` returns
    #[test]
    fn data_class_below_python_310() {
        let out = transpile(
            indoc! {"
                data class Point:
                    x: int

                final frozen data class Frozen:
                    x: int
            "},
            &Config {
                min_version: PythonVersion::PY39,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains("@_by_dataclass_slots\n@dataclass\nclass Point:")
                && out.contains(
                    "@final\n@_by_dataclass_slots\n@dataclass(frozen=True)\nclass Frozen:"
                )
                && out.contains("def _by_dataclass_slots(cls):")
                && !out.contains("slots=True"),
            "got:\n{out}"
        );
    }

    /// below 3.13 `dataclass(slots=True)` leaves the `__class__` cell of the class's
    /// methods pointing at the class it replaced, so a class whose methods name `super` or
    /// `__class__` is given its slots by the helper, which re-points the cell
    #[test]
    fn data_class_reaching_its_class_cell_below_python_313() {
        let source = indoc! {"
            class Base:
                def f(self) -> int:
                    return 1

            data class Plain:
                x: int

            data class Calls(Base):
                x: int

                override def f(self) -> int:
                    return super().f() + 1

            frozen data class Names:
                x: int

                def me(self) -> type:
                    return __class__
        "};
        for (target, helper) in [
            (PythonVersion::PY310, true),
            (PythonVersion::PY312, true),
            (PythonVersion::PY313, false),
        ] {
            let out = transpile(
                source,
                &Config {
                    min_version: target,
                    ..Config::test_default()
                },
            )
            .unwrap();
            assert!(
                out.contains("@dataclass(slots=True)\nclass Plain:"),
                "{target}: a class without the cell keeps the option:\n{out}"
            );
            let calls = if helper {
                "@_by_dataclass_slots\n@dataclass\nclass Calls(Base):"
            } else {
                "@dataclass(slots=True)\nclass Calls(Base):"
            };
            let names = if helper {
                "@_by_dataclass_slots\n@dataclass(frozen=True)\nclass Names:"
            } else {
                "@dataclass(frozen=True, slots=True)\nclass Names:"
            };
            assert!(
                out.contains(calls) && out.contains(names),
                "{target}: got:\n{out}"
            );
        }
    }

    #[test]
    fn nested_modifiers_in_class() {
        // Default min_version is PY310; `typing.override` was added in PY312
        // so it must come from `typing_extensions`.
        check(
            indoc! {"
                class Base:
                    override def foo(self): ...
                    static def bar(): ...
                    class def baz(cls): ...
            "},
            indoc! {"
                from typing_extensions import override
                class Base:
                    @override
                    def foo(self): ...
                    @staticmethod
                    def bar(): ...
                    @classmethod
                    def baz(cls): ...
            "},
        );
    }

    #[test]
    fn protocol_class() {
        check(
            "protocol Foo: ...\n",
            indoc! {"
                from typing import Protocol
                class Foo(Protocol): ...
            "},
        );
    }

    #[test]
    fn protocol_with_base() {
        check(
            "protocol Foo(Bar): ...\n",
            indoc! {"
                from typing import Protocol
                class Foo(Bar, Protocol): ...
            "},
        );
    }

    #[test]
    fn private_protocol() {
        // a visibility modifier composes with the `protocol` introducer: both the
        // `private` rename and the `protocol_class` rewrite lower by disjoint edits
        check(
            "private protocol Foo: ...\n",
            indoc! {"
                from typing import Protocol
                class _Foo(Protocol): ...
            "},
        );
    }

    #[test]
    fn private_module_function_already_underscored() {
        // a second underscore would make it a `__name`, which python mangles
        // inside every class body that reads it
        check(
            indoc! {"
                private def _has_room() -> bool:
                    return True
            "},
            indoc! {"
                def _has_room() -> bool:
                    return True
            "},
        );
    }

    #[test]
    fn export_protocol() {
        check(
            "export protocol Foo: ...\n",
            indoc! {"
                from typing import Protocol
                class Foo(Protocol): ...
                __all__ = [\"Foo\"]
            "},
        );
    }

    #[test]
    fn let_decl() {
        check(
            "let MAX = 100\n",
            indoc! {"
                from typing import Final
                MAX: Final = 100
            "},
        );
    }

    /// the `Final` a `let` needs joins the module's own `typing` import, which is a line
    /// already there
    #[test]
    fn let_decl_joins_the_module_s_typing_import() {
        check(
            indoc! {"
                from typing import Protocol
                let MAX = 100
            "},
            indoc! {"
                from typing import Protocol, Final
                MAX: Final = 100
            "},
        );
    }

    /// in a session the module's import is an earlier input, which has run, so the `Final`
    /// is imported on a line of its own
    #[test]
    fn let_decl_in_a_session_imports_final_apart() {
        let config = Config {
            session: true,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(
                indoc! {"
                    from typing import Protocol
                    let MAX = 100
                "},
                &config
            )
            .unwrap(),
            crate::python_passthrough::lazify_expected(indoc! {"
                from typing import Final
                from typing import Protocol
                MAX: Final = 100
            "})
        );
    }

    #[test]
    fn class_var_annotated_decl() {
        check(
            indoc! {"
                class A:
                    class var count: int = 0
                    class var kind: str
            "},
            indoc! {"
                from typing import ClassVar
                class A:
                    count: ClassVar[int] = 0
                    kind: ClassVar[str]
            "},
        );
    }

    #[test]
    fn class_let_annotated_decl_is_final() {
        // a read-only class attribute is python's `Final`, which in a class body
        // can be reassigned neither through the class nor an instance
        check(
            indoc! {"
                class A:
                    class let ORIGIN: int = 0
            "},
            indoc! {"
                from typing import Final
                class A:
                    ORIGIN: Final[int] = 0
            "},
        );
    }

    #[test]
    fn let_decl_parenthesized_multiline_value() {
        // an expression's range starts inside its parentheses, so the lowering
        // has to reach back out for the `(` or its `)` is stranded below
        check(
            indoc! {"
                let total = (
                    1
                    + 2
                )
            "},
            indoc! {"
                from typing import Final
                total: Final = (
                    1
                    + 2
                )
            "},
        );
    }

    #[test]
    fn typed_let_decl_parenthesized_multiline_value() {
        check(
            indoc! {"
                let total: int = (
                    1
                    + 2
                )
            "},
            indoc! {"
                from typing import Final
                total: Final[int] = (
                    1
                    + 2
                )
            "},
        );
    }

    #[test]
    fn let_decl_parenthesized_value_with_comment() {
        // the walk back to the `(` steps over a trailing comment on the way
        check(
            indoc! {"
                let total = (  # the sum
                    1 + 2
                )
            "},
            indoc! {"
                from typing import Final
                total: Final = (  # the sum
                    1 + 2
                )
            "},
        );
    }

    #[test]
    fn newtype_parenthesized_multiline_value() {
        // `newtype` re-emits the value, so it needs the closing parenthesis too
        check(
            indoc! {"
                newtype Id = (
                    int
                )
            "},
            indoc! {"
                from typing import NewType
                Id = NewType(\"Id\", (
                    int
                ))
            "},
        );
    }

    #[test]
    fn final_var_decl() {
        check("final a = 1", "a = 1");
    }

    #[test]
    fn final_var_decl_in_class() {
        check(
            indoc! {"
                class A:
                    final a = 1
            "},
            indoc! {"
                class A:
                    a: int = 1
            "},
        );
    }

    #[test]
    fn var_decl() {
        // `var` is the mutable counterpart of `let`: the keyword is stripped and
        // nothing is declared beyond the assignment itself — no `Final`
        check("var a = 1\n", "a = 1\n");
    }

    #[test]
    fn var_decl_typed() {
        check("var a: int = 1\n", "a: int = 1\n");
    }

    #[test]
    fn var_decl_valueless_typed() {
        check("var a: int\n", "a: int\n");
    }

    #[test]
    fn var_decl_in_class() {
        check(
            indoc! {"
                class A:
                    var a = 1
                    var b: int = 2
                    var c: str
            "},
            indoc! {"
                class A:
                    a: int = 1
                    b: int = 2
                    c: str
            "},
        );
    }

    #[test]
    fn var_decl_in_function() {
        check(
            indoc! {"
                def f():
                    var a = 1
                    return a
            "},
            indoc! {"
                def f():
                    a = 1
                    return a
            "},
        );
    }

    #[test]
    fn var_decl_with_modifier_chain() {
        // a visibility modifier ahead of `var` is stripped with it, matching the
        // bare-assignment modifier forms
        check("private var a = 1\n", "_a = 1\n");
    }

    #[test]
    fn var_as_identifier_is_not_a_declaration() {
        // as with `let`, `var` only introduces a declaration when shaped like
        // `var NAME =` or `var NAME :` — otherwise it is an ordinary identifier
        check("var = 5\n", "var = 5\n");
        check("var: int = 5\n", "var: int = 5\n");
        check("print(var)\n", "print(var)\n");
        check("var, y = 1, 2\n", "var, y = 1, 2\n");
    }

    #[test]
    fn let_decl_in_class() {
        check(
            indoc! {"
                class A:
                    let foo = 100
            "},
            indoc! {"
                from typing import Final
                class A:
                    foo: Final = 100
            "},
        );
    }

    #[test]
    fn let_decl_valueless_typed() {
        check(
            "let TOP: int\n",
            indoc! {"
                from typing import Final
                TOP: Final[int]
            "},
        );
    }

    #[test]
    fn let_decl_valueless_untyped() {
        // a bare `let x` with neither type nor initializer declares an
        // uninitialized `Final` — a later single assignment binds it
        check(
            "let a\na = 1\n",
            indoc! {"
                from typing import Final
                a: Final
                a = 1
            "},
        );
    }

    #[test]
    fn let_decl_valueless_typed_in_class() {
        // a valueless typed `let` is read-only in every scope, so it keeps the
        // `Final` wrapper inside a class body (unlike the valued form)
        check(
            indoc! {"
                class A:
                    let member: int
            "},
            indoc! {"
                from typing import Final
                class A:
                    member: Final[int]
            "},
        );
    }

    #[test]
    fn let_decl_string() {
        check(
            "let NAME = \"alice\"\n",
            indoc! {"
                from typing import Final
                NAME: Final = \"alice\"
            "},
        );
    }

    #[test]
    fn let_as_identifier_is_not_a_declaration() {
        // `let` only introduces a declaration when shaped like `let NAME =` or
        // `let NAME :`. as a plain identifier it stays untouched (all valid
        // python), and crucially the parser must not panic — a tool such as
        // ERA001 parsing a comment like `# the OS will let us` used to hit a
        // `bump(Equal)` assertion
        check("let = 5\n", "let = 5\n");
        check("let.append(1)\n", "let.append(1)\n");
        check("print(let)\n", "print(let)\n");
    }

    #[test]
    fn class_var_decl() {
        check(
            indoc! {"
                class Foo:
                    class count = 0
            "},
            indoc! {"
                from typing import ClassVar
                class Foo:
                    count: ClassVar = 0
            "},
        );
    }

    #[test]
    fn class_var_multiline_string() {
        check(
            indoc! {r#"
                class A:
                    class x = """
                    asdf
                    asdf
                    """
            "#},
            indoc! {r#"
                from typing import ClassVar
                class A:
                    x: ClassVar = """\
                asdf
                asdf\
                """
            "#},
        );
    }

    #[test]
    fn newtype_decl() {
        check(
            "newtype UserId = int\n",
            indoc! {"
                from typing import NewType
                UserId = NewType(\"UserId\", int)
            "},
        );
    }

    #[test]
    fn export_def() {
        check(
            "export def foo(): ...\n",
            indoc! {"
                def foo(): ...
                __all__ = [\"foo\"]
            "},
        );
    }

    #[test]
    fn private_beats_export_in_all() {
        // `private` renames the symbol, so exporting it too listed a name the
        // module has no attribute for and `import *` raised `AttributeError`
        check(
            indoc! {"
                export private def helper(): ...
                export def api(): ...
            "},
            indoc! {"
                def _helper(): ...
                def api(): ...
                __all__ = [\"api\"]
            "},
        );
        // nothing left to export means no `__all__` at all, as when nothing was
        // ever marked
        check("export private def helper(): ...\n", "def _helper(): ...\n");
        check("private export class Foo: ...\n", "class _Foo: ...\n");
    }

    #[test]
    fn export_class() {
        check(
            "export class Foo: ...\n",
            indoc! {"
                class Foo: ...
                __all__ = [\"Foo\"]
            "},
        );
    }

    #[test]
    fn public_alias_for_export() {
        check(
            "public def helper(): ...\n",
            indoc! {"
                def helper(): ...
                __all__ = [\"helper\"]
            "},
        );
    }

    #[test]
    fn export_multiple_collected_into_all() {
        check(
            indoc! {"
                export def first(): ...
                def internal(): ...
                export class Second: ...
            "},
            indoc! {"
                def first(): ...
                def internal(): ...
                class Second: ...
                __all__ = [\"first\", \"Second\"]
            "},
        );
    }

    #[test]
    fn no_all_emitted_when_no_exports() {
        // A file with no export markers should not produce an `__all__` block.
        check("def foo(): ...\n", "def foo(): ...\n");
    }

    #[test]
    fn private_def() {
        check("private def helper(): ...\n", "def _helper(): ...\n");
    }

    #[test]
    fn a_private_method_of_only_underscores_keeps_its_name() {
        // `__` + `_` is `___`, and python mangles only a name with at most one
        // trailing underscore, so the rename would hide nothing
        check(
            indoc! {"
                class A:
                    private def _(self): ...
            "},
            indoc! {"
                class A:
                    def _(self): ...
            "},
        );
    }

    #[test]
    fn private_def_call_site_renamed() {
        check(
            indoc! {"
                private def helper(): ...

                helper()
            "},
            indoc! {"
                def _helper(): ...

                _helper()
            "},
        );
    }

    #[test]
    fn private_type_alias() {
        check_at(
            "private type Alias = int\n",
            "type _Alias = int\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn private_type_alias_references_renamed() {
        check_at(
            indoc! {"
                private type Alias = int | str
                type Pair = tuple[Alias, Alias]

                def f(x: Alias) -> Alias:
                    return x
            "},
            indoc! {"
                type _Alias = int | str
                type Pair = tuple[_Alias, _Alias]

                def f(x: _Alias) -> _Alias:
                    return x
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn private_generic_type_alias() {
        check_at(
            "private type Pair[T] = tuple[T, T]\n",
            "type _Pair[T] = tuple[T, T]\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn public_type_alias_untouched() {
        check_at(
            "type Alias = int\n",
            "type Alias = int\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn private_type_alias_polyfilled() {
        // below 3.12 the alias lowers to `TypeAliasType`, whose whole-statement
        // replacement subsumes the `private ` deletion and the definition-site
        // rename — both have to come out of the polyfill instead
        check(
            indoc! {"
                private type Alias = int | str
                type Pair = tuple[Alias, Alias]

                def f(x: Alias) -> Alias:
                    return x
            "},
            indoc! {"
                from typing_extensions import TypeAliasType
                _Alias = TypeAliasType(\"_Alias\", int | str)
                Pair = TypeAliasType(\"Pair\", tuple[_Alias, _Alias])

                def f(x: _Alias) -> _Alias:
                    return x
            "},
        );
    }

    #[test]
    fn private_type_alias_already_underscored_polyfilled() {
        check(
            indoc! {"
                private type _Alias = int

                def f(x: _Alias) -> _Alias:
                    return x
            "},
            indoc! {"
                from typing_extensions import TypeAliasType
                _Alias = TypeAliasType(\"_Alias\", int)

                def f(x: _Alias) -> _Alias:
                    return x
            "},
        );
    }

    /// below 3.10 the value's union is spelled `Union[...]`, an edit of the value that the
    /// polyfill's replacement carries along with the `private ` deletion
    #[test]
    fn private_type_alias_with_a_union_polyfilled_below_310() {
        check_at(
            indoc! {"
                private type Alias = int | str

                def f(x: Alias) -> Alias:
                    return x
            "},
            indoc! {"
                from __future__ import annotations
                from typing import Union
                from typing_extensions import TypeAliasType
                _Alias = TypeAliasType(\"_Alias\", Union[int, str])

                def f(x: _Alias) -> _Alias:
                    return x
            "},
            PythonVersion::PY39,
        );
    }

    /// a class body's alias is polyfilled at the class body's indentation, and a
    /// `private` one is a private member there, as it is in the native `type` statement:
    /// declared as `__W`, and read as the mangled `_A__W`
    #[test]
    fn class_body_type_alias_polyfilled() {
        let source = indoc! {"
            class A:
                private type W = int
                type V = list[W]

                def f(self, w: W) -> V:
                    return [w]
        "};
        check_at(
            source,
            indoc! {"
                from typing_extensions import TypeAliasType
                class A:
                    __W = TypeAliasType(\"__W\", int)
                    V = TypeAliasType(\"V\", list[_A__W])

                    def f(self, w: _A__W) -> V:
                        return [w]
            "},
            PythonVersion::PY310,
        );
        check_at(
            source,
            indoc! {"
                class A:
                    type __W = int
                    type V = list[_A__W]

                    def f(self, w: _A__W) -> V:
                        return [w]
            "},
            PythonVersion::PY312,
        );
    }

    /// `_force_unwrap` is the runtime helper `x!` calls, and `_count` the module's own
    #[test]
    fn a_private_symbol_is_emitted_under_a_name_the_module_does_not_have() {
        let out = transpile(
            indoc! {"
                _count = 0

                private def force_unwrap(x: int) -> int:
                    return x

                private def count() -> int:
                    return force_unwrap(_count)

                def first(x: int | None) -> int:
                    return x!
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def _force_unwrap2(x: int) -> int:")
                && out.contains("def _count2() -> int:\n    return _force_unwrap2(_count)")
                && out.contains("return _force_unwrap(x)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn private_generic_type_alias_polyfilled() {
        check(
            "private type Pair[T] = tuple[T, T]\n",
            indoc! {"
                from typing import TypeVar
                from typing_extensions import TypeAliasType
                _T = TypeVar(\"_T\")
                _Pair = TypeAliasType(\"_Pair\", tuple[_T, _T], type_params=(_T,))
            "},
        );
    }

    #[test]
    fn override_assign() {
        check("override a = 1\n", "a = 1\n");
    }

    #[test]
    fn final_override_assign() {
        check("final override a = 2\n", "a = 2\n");
    }

    #[test]
    fn arbitrary_modifier_chain_assign() {
        // arbitrary-length chain ahead of `name = value`; transform strips the prefix
        check("final override abstract a = 3\n", "a = 3\n");
    }

    #[test]
    fn override_annotated_assign() {
        // annotated assignment modifiers must parse and strip just like the
        // unannotated form — previously `override x: T` was a parse error while
        // `override x = v` worked
        check("override x: int = 1\n", "x: int = 1\n");
        // `final` in a chain keeps its `Final` qualifier (only the other modifier
        // is stripped); previously it was silently dropped
        check(
            "final override x: int = 1\n",
            "from typing import Final\nx: Final[int] = 1\n",
        );
        check("override x: int\n", "x: int\n");
    }

    #[test]
    fn arbitrary_modifier_chain_def() {
        check(
            "final override abstract def foo(): ...\n",
            indoc! {"
                from abc import abstractmethod
                from typing import final
                from typing_extensions import override
                @final
                @override
                @abstractmethod
                def foo(): ...
            "},
        );
    }

    #[test]
    fn modifier_chain_let_decl() {
        // motivating example: `override final let a = 1`. parses cleanly,
        // resolves to a Final-qualified readonly variable; the modifier
        // prefix is erased by the transform along with the let prefix
        check(
            "override final let a = 1\n",
            indoc! {"
                from typing import Final
                a: Final = 1
            "},
        );
    }

    #[test]
    fn modifier_chain_typed_let_decl_in_class() {
        // `override let a: T = v` inside a class — the modifier prefix ahead of
        // `let` must be erased along with the `let` keyword, leaving a plain
        // annotated attribute. previously `override ` leaked into the output and
        // re-parsing failed
        check(
            indoc! {"
                class B:
                    override let a: int = 2
            "},
            indoc! {"
                class B:
                    a: int = 2
            "},
        );
    }

    #[test]
    fn modifier_chain_typed_let_decl_module_scope() {
        // at module scope the same declaration keeps its `Final[T]` wrapper
        check(
            "override let a: int = 2\n",
            indoc! {"
                from typing import Final
                a: Final[int] = 2
            "},
        );
    }

    #[test]
    fn override_assign_in_class() {
        check(
            indoc! {"
                class Foo:
                    override a = 1
            "},
            indoc! {"
                class Foo:
                    a: int = 1
            "},
        );
    }

    #[test]
    fn abstract_annot() {
        check("abstract a: int\n", "a: int\n");
    }

    #[test]
    fn abstract_annot_in_class() {
        check(
            indoc! {"
                class Foo:
                    abstract a: int
            "},
            indoc! {"
                class Foo:
                    a: int
            "},
        );
    }

    #[test]
    fn private_annot_in_class() {
        // `private` on a class member renames it to the `__name` python
        // name-mangles, whatever else the modifier chain says and whether or not
        // the declaration binds a value
        check(
            indoc! {"
                class Foo:
                    private t: int
                    private var u: int = 1
                    override private v: str = \"v\"
            "},
            indoc! {"
                class Foo:
                    __t: int
                    __u: int = 1
                    __v: str = \"v\"
            "},
        );
    }

    #[test]
    fn abstract_data_class() {
        check(
            "abstract data class A: ...\n",
            indoc! {"
                from dataclasses import dataclass
                @dataclass(slots=True)
                class A: ...
            "},
        );
    }

    #[test]
    fn final_data_class() {
        check(
            "final data class A: ...\n",
            indoc! {"
                from dataclasses import dataclass
                from typing import final
                @final
                @dataclass(slots=True)
                class A: ...
            "},
        );
    }

    #[test]
    fn private_class() {
        check("private class Helper: ...\n", "class _Helper: ...\n");
    }

    #[test]
    fn private_dunder_inside_class() {
        // Inside a class body, `private def`/`private class` renames the
        // symbol with the Python name-mangling prefix `__`, hiding it from
        // subclass scope at runtime
        check(
            indoc! {"
                class Outer:
                    private def helper(self): ...
            "},
            indoc! {"
                class Outer:
                    def __helper(self): ...
            "},
        );
    }

    #[test]
    fn protected_method_inside_class() {
        // `protected` renames with a single underscore, which python does not
        // mangle — a subclass reaches the member under the same name
        check(
            indoc! {"
                class Outer:
                    protected def helper(self): ...
            "},
            indoc! {"
                class Outer:
                    def _helper(self): ...
            "},
        );
    }

    #[test]
    fn a_visibility_keyword_renames_an_attribute() {
        check(
            indoc! {"
                class Outer:
                    private count: int
                    protected step: int = 2
            "},
            indoc! {"
                class Outer:
                    __count: int
                    _step: int = 2
            "},
        );
    }

    #[test]
    fn final_composes_with_a_visibility_keyword() {
        // `final` and the visibility keyword each carry something the other does
        // not — the `Final` qualifier and the rename — so neither is dropped
        check(
            indoc! {"
                class Outer:
                    final private limit: int = 3
            "},
            indoc! {"
                from typing import Final
                class Outer:
                    __limit: Final[int] = 3
            "},
        );
    }

    #[test]
    fn a_visibility_keyword_renames_a_let_binding() {
        check(
            indoc! {"
                class Outer:
                    private let fixed: int = 5
                    protected let bound = 6
            "},
            indoc! {"
                from typing import Final
                class Outer:
                    __fixed: int = 5
                    _bound: Final = 6
            "},
        );
    }

    #[test]
    fn a_visibility_keyword_renames_an_untyped_attribute() {
        // the untyped form binds a member exactly as the annotated one does, so
        // the keyword has to reach it there too
        check(
            indoc! {"
                class Outer:
                    private count = 0
                    protected step = 2
            "},
            indoc! {"
                class Outer:
                    __count: int = 0
                    _step: int = 2
            "},
        );
    }

    #[test]
    fn a_module_level_private_variable_is_renamed() {
        // renamed the way a module-level `private def` is: one leading underscore,
        // python's own mark for "not part of the interface"
        check("private count: int = 1\n", "_count: int = 1\n");
    }

    #[test]
    fn export_skipped_inside_class() {
        // `export` on a class member must not pollute the module-level `__all__`.
        check(
            indoc! {"
                class Outer:
                    export def helper(self): ...
            "},
            indoc! {"
                class Outer:
                    def helper(self): ...
            "},
        );
    }

    #[test]
    fn sealed_class_members() {
        check(
            indoc! {"
                sealed class A
                class B(A)
                class C(A)
            "},
            indoc! {"
                class A: ...
                A.__sealed_members__ = ()
                class B(A): ...
                A.__sealed_members__ += (B,)
                class C(A): ...
                A.__sealed_members__ += (C,)
            "},
        );
    }

    #[test]
    fn sealed_members_exclude_function_local_subclass() {
        // a subclass declared inside a function body is not a module-level name,
        // so it must not appear in the module-level `__sealed_members__` tuple
        // (it would be a `NameError`). matches ty's view of the member set.
        check(
            indoc! {"
                sealed class A
                def make():
                    class C(A): ...
                class B(A)
            "},
            indoc! {"
                class A: ...
                A.__sealed_members__ = ()
                def make():
                    class C(A): ...
                class B(A): ...
                A.__sealed_members__ += (B,)
            "},
        );
    }

    /// with no blank line after a member, it is added to the tuple where the next
    /// statement starts, and a soundness check wrapping that statement's expression
    /// keeps the addition out of the call it opens there
    #[test]
    fn sealed_members_ahead_of_a_checked_statement() {
        let out = transpile(
            indoc! {"
                xs: list[str] = []
                sealed class A
                class B(A)
                xs[0]
            "},
            &Config {
                soundness: crate::SoundnessPositions::defaults(),
                ..Config::test_default()
            },
        )
        .unwrap();
        assert!(
            out.contains(indoc! {"
                class B(A): ...
                A.__sealed_members__ += (B,)
                _soundness_check(xs[0], str)
            "}),
            "got:\n{out}"
        );
    }

    #[test]
    fn sealed_class_no_members() {
        check(
            "sealed class A\n",
            indoc! {"
                class A: ...
                A.__sealed_members__ = ()
            "},
        );
    }

    /// a sealed class declared in a function body gets its members tuple in that
    /// body, which is the scope that can name it and its subclasses
    #[test]
    fn sealed_class_in_function_body() {
        check(
            indoc! {"
                def make():
                    sealed class A
                    class B(A)
                    class C(A)
                    return A
            "},
            indoc! {"
                def make():
                    class A: ...
                    A.__sealed_members__ = ()
                    class B(A): ...
                    A.__sealed_members__ += (B,)
                    class C(A): ...
                    A.__sealed_members__ += (C,)
                    return A
            "},
        );
    }

    #[test]
    fn sealed_class_in_class_body() {
        check(
            indoc! {"
                class Outer:
                    sealed class A
                    class B(A)
                print(Outer.A.__sealed_members__)
            "},
            indoc! {"
                class Outer:
                    class A: ...
                    A.__sealed_members__ = ()
                    class B(A): ...
                    A.__sealed_members__ += (B,)
                print(Outer.A.__sealed_members__)
            "},
        );
    }

    /// a subclass declared in a scope nested inside the sealed class's own is not
    /// a name the sealed class's scope can reach, and a same-named class in another
    /// scope is not a member of this one
    #[test]
    fn sealed_members_are_the_binding_scopes_own() {
        check(
            indoc! {"
                def make():
                    sealed class A
                    def inner():
                        class C(A): ...
                    class B(A)
                    return A
                class A
                class D(A)
            "},
            indoc! {"
                def make():
                    class A: ...
                    A.__sealed_members__ = ()
                    def inner():
                        class C(A): ...
                    class B(A): ...
                    A.__sealed_members__ += (B,)
                    return A
                class A: ...
                class D(A): ...
            "},
        );
    }

    /// a member is added in the block its statement is written in, right after it,
    /// so a member whose statement does not run — in a branch not taken, or after a
    /// `return` — is not added, and code after it in the same block can read it
    #[test]
    fn sealed_members_are_added_where_their_statement_runs() {
        check(
            indoc! {"
                sealed class A
                if flag:
                    class B(A)
                else:
                    pass
                for _ in range(2):
                    class C(A)
                def make(flag):
                    sealed class D
                    class E(D)
                    if flag:
                        return D
                    class F(D)
                    return D
            "},
            indoc! {"
                class A: ...
                A.__sealed_members__ = ()
                if flag:
                    class B(A): ...
                    A.__sealed_members__ += (B,)
                else:
                    pass
                for _ in range(2):
                    class C(A): ...
                    A.__sealed_members__ += (C,)
                def make(flag):
                    class D: ...
                    D.__sealed_members__ = ()
                    class E(D): ...
                    D.__sealed_members__ += (E,)
                    if flag:
                        return D
                    class F(D): ...
                    D.__sealed_members__ += (F,)
                    return D
            "},
        );
    }

    /// a member is a class whose base is the sealed class, however the base is
    /// written, as ty decides it
    #[test]
    fn sealed_members_through_another_name() {
        check(
            indoc! {"
                sealed class A
                Base = A
                class B(Base)
            "},
            indoc! {"
                class A: ...
                A.__sealed_members__ = ()
                Base = A
                class B(Base): ...
                A.__sealed_members__ += (B,)
            "},
        );
    }

    /// a sealed class that is a member of another gets a tuple of its own and is
    /// added to the other's
    #[test]
    fn sealed_member_of_a_sealed_class() {
        check(
            indoc! {"
                sealed class A
                sealed class B(A)
            "},
            indoc! {"
                class A: ...
                A.__sealed_members__ = ()
                class B(A): ...
                B.__sealed_members__ = ()
                A.__sealed_members__ += (B,)
            "},
        );
    }

    /// a `private` class is added under the name it is emitted under, which python
    /// mangles in a class body as it does the class statement's own
    #[test]
    fn private_sealed_class() {
        check(
            indoc! {"
                private sealed class A
                private class B(A)
                class Outer:
                    private sealed class X
                    private class Y(X)
            "},
            indoc! {"
                class _A: ...
                _A.__sealed_members__ = ()
                class _B(_A): ...
                _A.__sealed_members__ += (_B,)
                class Outer:
                    class __X: ...
                    __X.__sealed_members__ = ()
                    class __Y(_Outer__X): ...
                    __X.__sealed_members__ += (__Y,)
            "},
        );
    }

    #[test]
    fn export_and_private_together_in_module() {
        check(
            indoc! {"
                export def api(): ...
                private def helper(): ...
            "},
            indoc! {"
                def api(): ...
                def _helper(): ...
                __all__ = [\"api\"]
            "},
        );
    }

    /// the members tuple is assigned as the module runs, and a stub never runs
    #[test]
    fn a_stub_assigns_no_sealed_members() {
        let out = transpile(
            indoc! {"
                sealed class A
                class B(A)
            "},
            &Config {
                is_stub: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            indoc! {"
                class A: ...
                class B(A): ...
            "}
        );
    }
}
