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
//! `let x = 5`             → `x: Final = 5` (from `typing`)
//! `class a = 1`           → `a: ClassVar = 1` (from `typing`)
//! `newtype Foo = int`     → `Foo = NewType("Foo", int)` (from `typing`)
//! `export`/`public`       → modifier deleted; symbol name added to auto-generated `__all__`
//! `private`               → modifier deleted; symbol renamed with `_` prefix and excluded from `__all__`

use std::collections::HashMap;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtAnnAssign, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

/// Returns the head identifier of a base-class expression: `A` for `A`, and
/// `A` for the generic form `A[int]`. Other base shapes have no simple name.
fn base_head_name(base: &Expr) -> Option<&str> {
    match base {
        Expr::Name(n) => Some(n.id.as_str()),
        Expr::Subscript(s) => match s.value.as_ref() {
            Expr::Name(n) => Some(n.id.as_str()),
            _ => None,
        },
        _ => None,
    }
}

#[expect(clippy::struct_excessive_bools)]
pub(crate) struct Modifiers<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
    /// Needs `from typing import final` (decorator for classes/methods)
    pub(crate) needs_final: bool,
    /// Needs `from typing import Final` (type annotation for constants)
    pub(crate) needs_final_annotation: bool,
    pub(crate) needs_abstractmethod: bool,
    pub(crate) needs_override: bool,
    pub(crate) needs_dataclass: bool,
    pub(crate) needs_protocol: bool,
    pub(crate) needs_classvar: bool,
    pub(crate) needs_newtype: bool,
    /// Names marked `export`/`public` at module level. Used to generate `__all__`.
    pub(crate) exports: Vec<String>,
    /// Module-level names renamed by `private` (original → `_original`).
    pub(crate) private_renames: Vec<String>,
    /// Module-level classes declared `sealed`, in source order. Each gets a
    /// `<name>.__sealed_members__` tuple of its same-module subclasses.
    pub(crate) sealed_classes: Vec<String>,
    /// Module-level `(class name, base head names, statement end offset)`
    /// triples, in source order. Used to resolve the subclasses of each sealed
    /// class and to place the `__sealed_members__` assignment after the last one.
    pub(crate) class_bases: Vec<(String, Vec<String>, TextSize)>,
    /// Tracks the current class-nesting depth so visibility modifiers can
    /// distinguish module-level declarations from class members.
    class_depth: u32,

    /// Tracks the current function-nesting depth. A class declared inside a
    /// function body is not a module-level name, so it must not be recorded as
    /// a sealed subclass (the runtime tuple assignment lives at module scope
    /// and cannot reference a function-local name).
    func_depth: u32,
}

impl<'src> Modifiers<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
            needs_final: false,
            needs_final_annotation: false,
            needs_abstractmethod: false,
            needs_override: false,
            needs_dataclass: false,
            needs_protocol: false,
            needs_classvar: false,
            needs_newtype: false,
            exports: Vec::new(),
            private_renames: Vec::new(),
            sealed_classes: Vec::new(),
            class_bases: Vec::new(),
            class_depth: 0,
            func_depth: 0,
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

    fn process_class(&mut self, class: &StmtClassDef) {
        // Record module-level class → base-head-names so sealed-member tuples can
        // be resolved after the whole module has been visited.
        if self.at_module_level() {
            let bases = class
                .arguments
                .iter()
                .flat_map(|args| args.args.iter())
                .filter_map(base_head_name)
                .map(str::to_owned)
                .collect();
            self.class_bases
                .push((class.name.as_str().to_owned(), bases, class.range().end()));
        }
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
                    // Remove the modifier prefix; the `__sealed_members__` tuple is
                    // emitted after the last subclass once they are all known.
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.at_module_level() {
                        self.sealed_classes.push(class.name.as_str().to_owned());
                    }
                }
                "final" => {
                    self.needs_final = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@final\n{indent}"),
                        dec.range(),
                    )));
                }
                "data_class" => {
                    self.needs_dataclass = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@dataclass(slots=True)\n{indent}"),
                        dec.range(),
                    )));
                }
                "frozen_data_class" => {
                    self.needs_dataclass = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@dataclass(frozen=True, slots=True)\n{indent}"),
                        dec.range(),
                    )));
                }
                "protocol_class" => {
                    self.needs_protocol = true;
                    // Replace "protocol " with "class "; Protocol base is inserted separately.
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        "class ".to_owned(),
                        dec.range(),
                    )));
                    self.insert_protocol_base(class);
                }
                "export" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.class_depth == 0 {
                        self.exports.push(class.name.as_str().to_owned());
                    }
                }
                "private" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.class_depth == 0 {
                        self.private_renames.push(class.name.as_str().to_owned());
                        self.rename_with_underscore(class.name.range());
                    } else {
                        // `private` on a nested class member uses Python
                        // name-mangling (`__name`) so it's hidden from
                        // subclass scope
                        self.rename_with_dunder(class.name.range());
                    }
                }
                _ => {}
            }
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
                        format!("@abstractmethod\n{indent}"),
                        dec.range(),
                    )));
                }
                "final" => {
                    self.needs_final = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@final\n{indent}"),
                        dec.range(),
                    )));
                }
                "override" => {
                    self.needs_override = true;
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@override\n{indent}"),
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
                        format!("@staticmethod\n{indent}"),
                        dec.range(),
                    )));
                }
                "classmethod" => {
                    self.edits.push(Fix::safe_edit(Edit::range_replacement(
                        format!("@classmethod\n{indent}"),
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
                "private" => {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(dec.range())));
                    if self.class_depth == 0 {
                        self.private_renames.push(func.name.as_str().to_owned());
                        self.rename_with_underscore(func.name.range());
                    } else {
                        // `private` method gets name-mangled `__name`
                        self.rename_with_dunder(func.name.range());
                    }
                }
                _ => {}
            }
        }
    }

    /// Replace the identifier at `range` with an underscore-prefixed copy.
    fn rename_with_underscore(&mut self, range: TextRange) {
        let original = self.src(range).to_owned();
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("_{original}"),
            range,
        )));
    }

    /// Replace the identifier at `range` with a double-underscore-prefixed
    /// copy. Used for `private` class members so Python's name-mangling
    /// applies and the symbol is hidden from subclass scope
    fn rename_with_dunder(&mut self, range: TextRange) {
        let original = self.src(range).to_owned();
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("__{original}"),
            range,
        )));
    }

    /// Returns the source text for a range.
    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn process_ann_assign(&mut self, node: &StmtAnnAssign) {
        let Some(value) = &node.value else { return };
        let name = self.src(node.target.range()).to_owned();

        match node.annotation.as_ref() {
            Expr::Name(ann) => {
                // untyped: `let a = v`, `class a = v`, `newtype Foo = v`
                let prefix_range = TextRange::new(node.range().start(), value.range().start());
                match ann.id.as_str() {
                    "__let__" => {
                        self.needs_final_annotation = true;
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("{name}: Final = "),
                            prefix_range,
                        )));
                    }
                    "__modifier_assign__" => {
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("{name} = "),
                            prefix_range,
                        )));
                    }
                    "__classvar__" => {
                        self.needs_classvar = true;
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("{name}: ClassVar = "),
                            prefix_range,
                        )));
                    }
                    "__newtype__" => {
                        let value_src = self.src(value.range()).to_owned();
                        self.needs_newtype = true;
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("{name} = NewType(\"{name}\", {value_src})"),
                            node.range(),
                        )));
                    }
                    "__abstract_annot__" | "__visibility_annot__" | "__modifier_annot__" => {
                        // erase only the modifier prefix; the rest of the
                        // statement (`a: int [= v]`) remains in source unchanged
                        let erase_range =
                            TextRange::new(node.range().start(), node.target.range().start());
                        self.edits
                            .push(Fix::safe_edit(Edit::range_deletion(erase_range)));
                    }
                    _ => {}
                }
            }
            ann @ Expr::Subscript(s) if matches!(s.value.as_ref(), Expr::Name(n) if n.id.as_str() == "__let__") =>
            {
                // typed: `let a: T = v` — annotation is Subscript(__let__, T)
                // callable transform visits only the slice independently, so emit
                // bracket edits around the slice range; they don't overlap with callable's edit
                let slice = s.slice.as_ref();
                let pre_range = TextRange::new(ann.range().start(), slice.range().start());
                let post_range = TextRange::new(slice.range().end(), value.range().start());
                if self.class_depth > 0 {
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
                        format!("{name}: Final["),
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
        self.insert_base_class(class, "Protocol");
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
                walk_stmt(self, stmt);
                self.class_depth -= 1;
                return;
            }
            Stmt::FunctionDef(f) => {
                self.process_function(f);
                // Walk the body with `func_depth` incremented so a class defined
                // inside the function is not treated as a module-level subclass.
                self.func_depth += 1;
                walk_stmt(self, stmt);
                self.func_depth -= 1;
                return;
            }
            Stmt::AnnAssign(a) => {
                self.process_ann_assign(a);
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

/// renames all `Name` expression nodes that match a `private`-renamed symbol
pub(crate) struct NameRenamer {
    renames: HashMap<String, String>,
    pub(crate) edits: Vec<Fix>,
}

impl NameRenamer {
    pub(crate) fn new(private_names: &[String]) -> Self {
        let renames = private_names
            .iter()
            .map(|n| (n.clone(), format!("_{n}")))
            .collect();
        Self {
            renames,
            edits: Vec::new(),
        }
    }
}

impl<'ast> Visitor<'ast> for NameRenamer {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(n) = expr {
            if let Some(new_name) = self.renames.get(n.id.as_str()) {
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    new_name.clone(),
                    expr.range(),
                )));
                return;
            }
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct ModifiersPass<'src> {
    source: &'src str,
}

impl<'src> ModifiersPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for ModifiersPass<'_> {
    fn run(&self, module: &mut ruff_python_ast::ModModule, ctx: &mut PassContext) {
        let mut inner = Modifiers::new(self.source);
        for stmt in &module.body {
            inner.visit_stmt(stmt);
        }
        let exports = std::mem::take(&mut inner.exports);
        let private_renames = std::mem::take(&mut inner.private_renames);
        let sealed_classes = std::mem::take(&mut inner.sealed_classes);
        let class_bases = std::mem::take(&mut inner.class_bases);

        // typing import grouping mirrors lib.rs's preamble logic
        let mut typing_imports: Vec<&'static str> = Vec::new();
        if inner.needs_final {
            typing_imports.push("final");
        }
        if inner.needs_final_annotation {
            typing_imports.push("Final");
        }
        if inner.needs_classvar {
            typing_imports.push("ClassVar");
        }
        if inner.needs_newtype {
            typing_imports.push("NewType");
        }
        if inner.needs_override {
            typing_imports.push("override");
        }
        for name in typing_imports {
            ctx.required_imports
                .push(format!("from typing import {name}"));
        }
        if inner.needs_abstractmethod {
            ctx.required_imports
                .push("from abc import abstractmethod".to_owned());
        }
        if inner.needs_dataclass {
            ctx.required_imports
                .push("from dataclasses import dataclass".to_owned());
        }
        if inner.needs_protocol {
            ctx.required_imports
                .push("from typing import Protocol".to_owned());
        }

        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }

        // 2nd-pass NameRenamer rewrites call sites referencing renamed
        // module-level symbols. Runs over the same AST as inner above —
        // exports/private_renames already collected
        if !private_renames.is_empty() {
            let mut renamer = NameRenamer::new(&private_renames);
            for stmt in &module.body {
                renamer.visit_stmt(stmt);
            }
            for fix in renamer.edits {
                for edit in fix.edits() {
                    let range = edit.range();
                    let repl = edit.content().unwrap_or_default().to_owned();
                    ctx.text_edits.push((range, repl));
                }
            }
        }

        if !exports.is_empty() {
            let entries = exports
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(", ");
            ctx.epilogue.push(format!("__all__ = [{entries}]"));
        }

        for sealed in &sealed_classes {
            let subclasses: Vec<&str> = class_bases
                .iter()
                .filter(|(_, bases, _)| bases.iter().any(|base| base == sealed))
                .map(|(name, _, _)| name.as_str())
                .collect();
            let tuple = match subclasses.as_slice() {
                [] => "()".to_owned(),
                [single] => format!("({single},)"),
                many => format!("({})", many.join(", ")),
            };

            // Place the assignment right after the last subclass (or the sealed
            // class itself when it has none), so module-level code that follows
            // can read `__sealed_members__`. The module epilogue runs too late.
            let anchor_end = class_bases
                .iter()
                .filter(|(name, bases, _)| {
                    name == sealed || bases.iter().any(|base| base == sealed)
                })
                .map(|(_, _, end)| *end)
                .max();
            let Some(anchor_end) = anchor_end else {
                continue;
            };
            let anchor_end = usize::from(anchor_end);

            let assignment = format!("{sealed}.__sealed_members__ = {tuple}");
            // Insert at the start of the line after the anchor statement so the
            // edit never collides with `empty_declarations`' mid-line `: ...`.
            if let Some(rel) = self.source[anchor_end..].find('\n') {
                let pos = TextSize::try_from(anchor_end + rel + 1).expect("offset fits u32");
                ctx.text_edits
                    .push((TextRange::empty(pos), format!("{assignment}\n")));
            } else {
                let pos = TextSize::try_from(self.source.len()).expect("offset fits u32");
                ctx.text_edits
                    .push((TextRange::empty(pos), format!("\n{assignment}\n")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
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
            class B(A): ...
            A.__sealed_members__ = (B,)
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
    fn let_decl() {
        check(
            "let MAX = 100\n",
            indoc! {"
                from typing import Final
                MAX: Final = 100
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
                    a = 1
            "},
        );
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
        check("final override x: int = 1\n", "x: int = 1\n");
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
    fn override_assign_in_class() {
        check(
            indoc! {"
                class Foo:
                    override a = 1
            "},
            indoc! {"
                class Foo:
                    a = 1
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
                class B(A): ...
                class C(A): ...
                A.__sealed_members__ = (B, C)
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
                def make():
                    class C(A): ...
                class B(A): ...
                A.__sealed_members__ = (B,)
            "},
        );
    }

    #[test]
    fn sealed_class_single_member() {
        check(
            indoc! {"
                sealed class A
                class B(A)
            "},
            indoc! {"
                class A: ...
                class B(A): ...
                A.__sealed_members__ = (B,)
            "},
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
}
