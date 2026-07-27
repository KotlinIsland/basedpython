//! Type-aware pass: basedpython property accessor blocks.
//!
//! The parser has already lowered a class-body `var`/`let` declaration carrying
//! an indented `get`/`set`/`field` suite into standard python `@property`
//! members (see `parse_property_accessors`): a backing-field declaration, a
//! getter tagged with the synthetic `__property__` marker, and — for a mutable
//! `var` — a setter decorated `@<name>.setter`. `field` was rewritten to the
//! backing attribute there, so ty type-checks the property with no special rule for
//! the keyword and this pass only has to emit the matching source.
//!
//! The marker's range spans the *whole* construct (the declaration line plus the
//! accessor suite), so the lowering is a single replacement of that span:
//!
//! ```text
//! var age: int = 0          ->    def __init__(self) -> None:
//!     get() = field                   self._age: int = 0
//!     set(value):                 @property
//!         assert value >= 0       def age(self) -> int:
//!         field = value               return self._age
//!                                 @age.setter
//!                                 def age(self, value: int) -> None:
//!                                     assert value >= 0
//!                                     self._age = value
//! ```
//!
//! Type positions and the backing initialiser are emitted as `Fragment::Src`
//! passthroughs, so a sibling pass's lowering inside them (a `T?` optional, a
//! callable arrow) composes instead of being clobbered. Accessor *bodies* are
//! re-rendered from the AST rather than passed through, because the `field`
//! rewrite means the original source no longer matches. Re-rendering is what makes
//! the in-AST rewrites (`field`, the narrow view, a `private` retarget) come out
//! right, but it discards any *text* edit a sibling pass emitted inside the body —
//! so a basedpython construct written there is not lowered. When the un-lowered
//! form is invalid python the final syntax check catches it; when it is valid
//! python meaning something else (`super.a` stays `super.a`) nothing would, so
//! [`PropertiesPass::reject_discarded_edits`] reports it instead.
//!
//! A backing field's initialiser is emitted into `__init__` (synthesized when the
//! class has none) so each instance gets its own storage — a class-body
//! `_items: list[int] = []` would be one list shared by every instance. See
//! [`InitPlacement`] for the shapes that are not injected into yet.

use std::fmt::Write;

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{Fragment, PassContext, TypeAwarePass, render_stmt};
use super::source_util::{line_indent, line_start};
use crate::type_info::TypeInfo;

pub(crate) struct PropertiesPass<'src> {
    source: &'src str,
}

impl<'src> PropertiesPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

/// The runtime half of a `static let` property. Python dropped `classmethod`
/// chaining onto `property` in 3.13, and a read-only class-level property needs
/// nothing else a metaclass would offer, so a plain non-data descriptor is the
/// whole implementation. Mirrors `_by_static_property` in `ty_extensions._internal`,
/// which is ty's type-only view of the same thing.
pub(crate) const STATIC_PROPERTY_HELPER: &str = "\
class _by_static_property:
    def __init__(self, fget):
        self._fget = fget
    def __get__(self, instance, owner=None):
        return self._fget(owner if owner is not None else type(instance))
";

/// A property accessor block's marker, as recorded by the parser on the getter.
struct PropertyMarker {
    /// The whole `var`/`let` declaration plus its accessor suite — the span the
    /// lowering replaces.
    construct: TextRange,
    /// `static let`: a class-level property, lowered to [`STATIC_PROPERTY_HELPER`]
    /// rather than to `property`.
    is_static: bool,
}

/// The property marker on `func`, or `None` for any other function.
fn property_marker(func: &StmtFunctionDef) -> Option<PropertyMarker> {
    func.decorator_list
        .iter()
        .find_map(|dec| match &dec.expression {
            Expr::Name(name) => match name.id.as_str() {
                "__property__" => Some(PropertyMarker {
                    construct: dec.range(),
                    is_static: false,
                }),
                "__static_property__" => Some(PropertyMarker {
                    construct: dec.range(),
                    is_static: true,
                }),
                _ => None,
            },
            _ => None,
        })
}

/// Whether `func` is the `@<prop>.setter` half of property `prop`.
fn is_setter_of(func: &StmtFunctionDef, prop: &str) -> bool {
    func.decorator_list.iter().any(|dec| match &dec.expression {
        Expr::Attribute(attr) => {
            attr.attr.as_str() == "setter"
                && matches!(attr.value.as_ref(), Expr::Name(name) if name.id.as_str() == prop)
        }
        _ => false,
    })
}

/// Whether `annotation` is the `__field__[T]` inference-context marker the parser
/// puts on an unannotated `field = <init>`. It carries the property's type for ty's
/// benefit only and must not reach the output.
fn is_field_context_marker(annotation: &Expr) -> bool {
    let Expr::Subscript(subscript) = annotation else {
        return false;
    };
    matches!(subscript.value.as_ref(), Expr::Name(name) if name.id.as_str() == "__field__")
}

/// Collects the source ranges of `field` occurrences the parser rewrote to the
/// backing attribute. Such a node is an attribute access whose receiver is a
/// zero-width synthetic `self`, and whose own range is still the `field` token —
/// which is exactly the span the output has to replace.
struct FieldAccessFinder<'a> {
    backing: &'a str,
    edits: Vec<TextRange>,
}

impl<'ast> Visitor<'ast> for FieldAccessFinder<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Attribute(attr) = expr
            && attr.attr.as_str() == self.backing
            && matches!(attr.value.as_ref(), Expr::Name(name)
                if name.id.as_str() == "self" && name.range.is_empty())
        {
            self.edits.push(attr.range());
            return;
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

/// The backing storage declaration synthesised for a property: an annotation
/// and/or an initialiser, each emitted as a source passthrough.
struct Backing<'a> {
    annotation: Option<&'a Expr>,
    value: Option<&'a Expr>,
}

/// Re-indents a rendered statement to `indent`, leaving blank lines empty.
fn indent_block(rendered: &str, indent: &str) -> String {
    rendered
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders an accessor body at `indent`. An empty body (only reachable when the
/// accessor itself failed to parse) becomes `...` so the emitted `def` is still
/// syntactically complete.
///
/// Shared with the extension lowering, which owns accessor-block members declared
/// inside an `extension` body and has the same reason to render rather than pass
/// the source through: a `get() = expr` accessor's `return` exists only in the AST.
pub(crate) fn render_body(body: &[Stmt], indent: &str) -> String {
    if body.is_empty() {
        return format!("{indent}...");
    }
    body.iter()
        .map(|stmt| indent_block(render_stmt(stmt).trim_end_matches('\n'), indent))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Where a property's backing-field initialiser can be emitted so each instance
/// gets its own storage.
///
/// A class-body `_x: T = <init>` would share one object across every instance —
/// fatal for the mutable implementation an explicit backing field usually holds
/// (`field: list[int] = []`). The initialiser therefore belongs in `__init__`.
enum InitPlacement {
    /// The class has no `__init__`; synthesise one ahead of the first property.
    Synthesize,
    /// Insert before this offset — the first statement of an existing `__init__`
    /// whose body is on its own line.
    Existing(TextSize),
    /// A shape this pass can't inject into safely: an `__init__` whose body is
    /// inline (`def __init__(self): ...`, or a bodyless `init(...)` the
    /// `init_method` pass completes). Keep the class-level declaration rather
    /// than emit wrongly indented code — those classes keep the older shared-storage
    /// behaviour until the two passes negotiate a single body
    ClassLevel,
}

impl PropertiesPass<'_> {
    /// Decides where `class`'s backing-field initialisers can be emitted.
    fn init_placement(&self, class: &StmtClassDef) -> InitPlacement {
        let Some(init) = class.body.iter().find_map(|member| match member {
            Stmt::FunctionDef(func) if func.name.as_str() == "__init__" => Some(func),
            _ => None,
        }) else {
            return InitPlacement::Synthesize;
        };
        // a bodyless `init(...)` has only the statements the parser synthesised for
        // its `let` parameters, which sit inside the parameter list's span
        let params_end = init.parameters.range().end();
        let Some(first) = init
            .body
            .iter()
            .find(|stmt| stmt.range().start() >= params_end)
        else {
            return InitPlacement::ClassLevel;
        };
        // an inline body (`def __init__(self): ...`) shares the header's line, so
        // there is no body line to insert before
        let header_line = line_start(self.source, init.range().start());
        if line_start(self.source, first.range().start()) == header_line {
            return InitPlacement::ClassLevel;
        }
        InitPlacement::Existing(first.range().start())
    }

    /// The fragments for one accessor's body.
    ///
    /// The body is emitted as a **source passthrough**, so any lowering a sibling
    /// pass emitted inside it survives — `super.a` becomes `super().a`, a `??`
    /// expands, and so on. That means this pass's own `field` rewrite cannot ride
    /// on the AST (which a passthrough ignores), so it is emitted as a text edit
    /// per occurrence, materialized inside the passthrough by the driver.
    ///
    /// A synthesized accessor (a pass-through getter or setter the author never
    /// wrote) has no source to pass through, so it is rendered from the AST.
    fn accessor_body(
        &self,
        func: &StmtFunctionDef,
        is_getter: bool,
        body_indent: &str,
        backing_name: &str,
        ctx: &mut PassContext,
    ) -> Vec<Fragment> {
        let Some(first) = func.body.first() else {
            return vec![Fragment::Lit(format!("{body_indent}..."))];
        };
        if first.range().is_empty() {
            // synthesized: nothing in the source corresponds to it
            return vec![Fragment::Lit(render_body(&func.body, body_indent))];
        }

        for stmt in &func.body {
            let mut finder = FieldAccessFinder {
                backing: backing_name,
                edits: Vec::new(),
            };
            finder.visit_stmt(stmt);
            ctx.text_edits.extend(
                finder
                    .edits
                    .into_iter()
                    .map(|range| (range, format!("self.{backing_name}"))),
            );
        }

        // a single-expression accessor (`get() = <expr>`) sits on the accessor's own
        // line; a block accessor's statements start on lines of their own, and
        // passing those through keeps their relative indentation intact
        let inline = line_start(self.source, first.range().start())
            == line_start(self.source, func.range().start());
        if inline {
            let mut frags = vec![Fragment::Lit(body_indent.to_owned())];
            if is_getter {
                frags.push(Fragment::Lit("return ".to_owned()));
            }
            frags.push(Fragment::Src(first.range()));
            frags
        } else {
            // one passthrough per statement, each placed at the method's body indent.
            // a statement's range opens at its first token, so this re-indents the
            // body's top level without touching anything inside a statement — a
            // nested block or a multi-line string keeps its source text verbatim
            let mut frags = Vec::new();
            for (idx, stmt) in func.body.iter().enumerate() {
                if idx > 0 {
                    frags.push(Fragment::Lit("\n".to_owned()));
                }
                frags.push(Fragment::Lit(body_indent.to_owned()));
                frags.push(Fragment::Src(stmt.range()));
            }
            frags
        }
    }

    fn process_class(&self, class: &StmtClassDef, ctx: &mut PassContext) {
        let placement = self.init_placement(class);
        // assignment templates for every backing field whose initialiser moved into
        // `__init__`, in class-body order
        let mut moved: Vec<Vec<Fragment>> = Vec::new();
        // each construct's replacement, held back until the constructor is known:
        // a synthesised `__init__` has to be prepended *inside* the first
        // construct's template, because the driver absorbs a zero-width insertion
        // sharing a template's start rather than emitting it alongside
        let mut pending: Vec<(TextRange, Vec<Fragment>)> = Vec::new();
        // `(public name, emitted name)` for each `private` property in this class
        let mut renames: Vec<(String, String)> = Vec::new();

        for member in &class.body {
            let Stmt::FunctionDef(getter) = member else {
                continue;
            };
            let Some(PropertyMarker {
                construct,
                is_static,
            }) = property_marker(getter)
            else {
                continue;
            };
            let prop = getter.name.as_str();
            // the getter's name node keeps the *public* name's source range while its
            // `id` carries the emitted name (they differ for a `private` property), and
            // storage is `__<public>` — a dunder so python's name mangling hides it
            let public = &self.source
                [usize::from(getter.name.range().start())..usize::from(getter.name.range().end())];
            let backing_name = format!("__{public}");

            // the setter and backing field the parser synthesised for *this*
            // construct carry ranges inside its span, which keeps a same-named
            // hand-written member elsewhere in the class from being picked up
            let within = |range: TextRange| {
                range.start() >= construct.start() && range.start() <= construct.end()
            };

            let setter = class.body.iter().find_map(|m| match m {
                Stmt::FunctionDef(func)
                    if func.name.as_str() == prop
                        && is_setter_of(func, prop)
                        && within(func.range()) =>
                {
                    Some(func)
                }
                _ => None,
            });
            let backing = class.body.iter().find_map(|m| match m {
                Stmt::AnnAssign(assign)
                    if within(assign.range())
                        && matches!(assign.target.as_ref(), Expr::Name(name) if name.id.as_str() == backing_name) =>
                {
                    // an `__field__[T]` annotation is only an inference-context
                    // marker for ty; the storage is emitted unannotated
                    let annotation = (!is_field_context_marker(&assign.annotation))
                        .then_some(assign.annotation.as_ref());
                    Some(Backing {
                        annotation,
                        value: assign.value.as_deref(),
                    })
                }
                Stmt::Assign(assign)
                    if within(assign.range())
                        && matches!(assign.targets.first(), Some(Expr::Name(name)) if name.id.as_str() == backing_name) =>
                {
                    Some(Backing {
                        annotation: None,
                        value: Some(assign.value.as_ref()),
                    })
                }
                _ => None,
            });

            let indent = line_indent(self.source, construct.start()).to_owned();
            let body_indent = format!("{indent}    ");

            // modifier keywords written ahead of the declaration compose with the
            // property. the getter's name node keeps the declaration name's real
            // source range, so the prefix is the span before it. these decorators
            // sit *under* `@property` / `@<name>.setter` so they apply to the
            // accessor function itself, which is what type checkers expect
            let prefix = &self.source
                [usize::from(construct.start())..usize::from(getter.name.range().start())];
            let modifiers: Vec<&str> = prefix.split_whitespace().collect();
            let has = |word: &str| modifiers.contains(&word);
            let is_abstract = has("abstract");

            // a `private` property is emitted one underscore deeper than the name
            // the author wrote, so in-class accesses spelled under the public name
            // have to be redirected in the output too. the parser has already
            // retargeted them in the AST, but the source still says `self.x`
            if has("private") {
                renames.push((public.to_owned(), prop.to_owned()));
            }

            let mut accessor_decorators = String::new();
            if is_abstract {
                ctx.required_imports
                    .push("from abc import abstractmethod".to_owned());
                let _ = write!(accessor_decorators, "@abstractmethod\n{indent}");
            }
            if has("override") {
                ctx.required_imports
                    .push("from typing import override".to_owned());
                let _ = write!(accessor_decorators, "@override\n{indent}");
            }
            if has("final") {
                ctx.required_imports
                    .push("from typing import final".to_owned());
                let _ = write!(accessor_decorators, "@final\n{indent}");
            }

            let mut frags: Vec<Fragment> = Vec::new();

            // backing storage. an initialiser moves into `__init__` so each
            // instance gets its own object; the bare declaration (a `late field`,
            // with no initialiser) stays class-level, where it creates no runtime
            // attribute at all and only declares the type
            if let Some(backing) = &backing {
                let moves =
                    backing.value.is_some() && !matches!(placement, InitPlacement::ClassLevel);
                if moves {
                    let mut assign = vec![Fragment::Lit(format!("self.{backing_name}"))];
                    if let Some(annotation) = backing.annotation {
                        assign.push(Fragment::Lit(": ".to_owned()));
                        assign.push(Fragment::Src(annotation.range()));
                    }
                    if let Some(value) = backing.value {
                        assign.push(Fragment::Lit(" = ".to_owned()));
                        assign.push(Fragment::Src(value.range()));
                    }
                    moved.push(assign);
                } else {
                    frags.push(Fragment::Lit(backing_name.clone()));
                    if let Some(annotation) = backing.annotation {
                        frags.push(Fragment::Lit(": ".to_owned()));
                        frags.push(Fragment::Src(annotation.range()));
                    }
                    if let Some(value) = backing.value {
                        frags.push(Fragment::Lit(" = ".to_owned()));
                        frags.push(Fragment::Src(value.range()));
                    }
                    frags.push(Fragment::Lit(format!("\n{indent}")));
                }
            }

            // getter. a `static` property is a descriptor taking the owning class
            // rather than a `property` taking an instance
            let (decorator, receiver) = if is_static {
                ctx.required_imports.push(STATIC_PROPERTY_HELPER.to_owned());
                ("_by_static_property", "cls")
            } else {
                ("property", "self")
            };
            frags.push(Fragment::Lit(format!(
                "@{decorator}\n{indent}{accessor_decorators}def {prop}({receiver})"
            )));
            if let Some(returns) = &getter.returns {
                frags.push(Fragment::Lit(" -> ".to_owned()));
                frags.push(Fragment::Src(returns.range()));
            }
            if is_abstract {
                // an abstract accessor declares a shape, not an implementation
                frags.push(Fragment::Lit(": ...".to_owned()));
            } else {
                frags.push(Fragment::Lit(":\n".to_owned()));
                frags.extend(self.accessor_body(getter, true, &body_indent, &backing_name, ctx));
            }

            // setter
            if let Some(setter) = setter {
                frags.push(Fragment::Lit(format!(
                    "\n{indent}@{prop}.setter\n{indent}{accessor_decorators}def {prop}(self"
                )));
                // the value parameter follows the synthetic `self`
                if let Some(value_param) = setter.parameters.args.get(1) {
                    frags.push(Fragment::Lit(format!(", {}", value_param.parameter.name)));
                    if let Some(annotation) = &value_param.parameter.annotation {
                        frags.push(Fragment::Lit(": ".to_owned()));
                        frags.push(Fragment::Src(annotation.range()));
                    }
                }
                if is_abstract {
                    frags.push(Fragment::Lit(") -> None: ...".to_owned()));
                } else {
                    frags.push(Fragment::Lit(") -> None:\n".to_owned()));
                    frags.extend(self.accessor_body(
                        setter,
                        false,
                        &body_indent,
                        &backing_name,
                        ctx,
                    ));
                }
            }

            pending.push((construct, frags));
        }

        if !moved.is_empty()
            && let Some((first, frags)) = pending.first_mut()
        {
            let indent = line_indent(self.source, first.start()).to_owned();
            match placement {
                // prepend the constructor inside the first construct's own
                // replacement — the driver absorbs a separate insertion here
                InitPlacement::Synthesize => {
                    let body_indent = format!("{indent}    ");
                    let mut head = vec![Fragment::Lit(format!(
                        "def __init__(self) -> None:\n{body_indent}"
                    ))];
                    join_assignments(&mut head, moved, &body_indent);
                    head.push(Fragment::Lit(format!("\n{indent}")));
                    head.append(frags);
                    *frags = head;
                }
                // insert ahead of the existing constructor's first statement. the
                // `init_method` pass inserts its `let`-parameter self-assignments at
                // the same offset and runs earlier, so those land first — matching
                // the documented order
                InitPlacement::Existing(at) => {
                    let stmt_indent = line_indent(self.source, at).to_owned();
                    let mut edit = Vec::new();
                    join_assignments(&mut edit, moved, &stmt_indent);
                    edit.push(Fragment::Lit(format!("\n{stmt_indent}")));
                    ctx.template_edits.push((TextRange::empty(at), edit));
                }
                InitPlacement::ClassLevel => {}
            }
        }
        ctx.template_edits.extend(pending);

        if !renames.is_empty() {
            self.rename_private_accesses(class, &renames, ctx);
        }
    }

    /// Redirects in-class accesses of a `private` property to the name it is
    /// actually emitted under. The parser retargeted them in the AST so ty resolves
    /// them; the source still spells the public name, so the output needs an edit
    /// per occurrence. An access from outside the class is left alone — the property
    /// genuinely is not there, and ty reports it.
    fn rename_private_accesses(
        &self,
        class: &StmtClassDef,
        renames: &[(String, String)],
        ctx: &mut PassContext,
    ) {
        for member in &class.body {
            let Stmt::FunctionDef(func) = member else {
                continue;
            };
            let Some(receiver) = func
                .parameters
                .posonlyargs
                .first()
                .or_else(|| func.parameters.args.first())
                .map(|param| param.parameter.name.id.as_str())
            else {
                continue;
            };
            let mut finder = PrivateAccessFinder {
                source: self.source,
                renames,
                receiver,
                edits: Vec::new(),
            };
            for stmt in &func.body {
                finder.visit_stmt(stmt);
            }
            ctx.text_edits.extend(finder.edits);
        }
    }
}

/// Collects the source ranges of in-class accesses that name a `private` property
/// under its public spelling.
struct PrivateAccessFinder<'a> {
    source: &'a str,
    renames: &'a [(String, String)],
    receiver: &'a str,
    edits: Vec<(TextRange, String)>,
}

impl<'ast> Visitor<'ast> for PrivateAccessFinder<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // a nested class has its own `self`
        if matches!(stmt, Stmt::ClassDef(_)) {
            return;
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Attribute(attr) = expr
            && matches!(attr.value.as_ref(), Expr::Name(name) if name.id.as_str() == self.receiver)
        {
            let range = attr.attr.range();
            let written = &self.source[usize::from(range.start())..usize::from(range.end())];
            if let Some((_, emitted)) = self.renames.iter().find(|(public, _)| public == written) {
                self.edits.push((range, emitted.clone()));
            }
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

/// Appends each assignment template, separated by a newline at `indent`.
fn join_assignments(out: &mut Vec<Fragment>, assignments: Vec<Vec<Fragment>>, indent: &str) {
    for (idx, assignment) in assignments.into_iter().enumerate() {
        if idx > 0 {
            out.push(Fragment::Lit(format!("\n{indent}")));
        }
        out.extend(assignment);
    }
}

impl TypeAwarePass for PropertiesPass<'_> {
    fn run(&self, stmts: &[Stmt], _types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut finder = ClassFinder {
            classes: Vec::new(),
        };
        for stmt in stmts {
            finder.visit_stmt(stmt);
        }
        for class in finder.classes {
            self.process_class(class, ctx);
        }
    }
}

/// Collects every class in the module, at any nesting depth.
struct ClassFinder<'a> {
    classes: Vec<&'a StmtClassDef>,
}

impl<'ast> Visitor<'ast> for ClassFinder<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            // an `extension` body's members belong to the extension lowering, which
            // replaces the whole block. this pass's template edits inside that span
            // are dropped as contained, but its side channels (`required_imports`,
            // the `private` renames) are not — so skip the body outright rather than
            // emit a helper nothing ends up referencing
            if class.is_extension() {
                return;
            }
            self.classes.push(class);
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    /// the design doc's motivating example
    #[test]
    fn stored_var_property() {
        check(
            indoc! {"
                class Person:
                    var age: int = 0
                        get() = field
                        set(value):
                            assert value >= 0
                            field = value
            "},
            indoc! {"
                class Person:
                    def __init__(self) -> None:
                        self.__age: int = 0
                    @property
                    def age(self) -> int:
                        return self.__age
                    @age.setter
                    def age(self, value: int) -> None:
                        assert value >= 0
                        self.__age = value
            "},
        );
    }

    /// an accessor that never mentions `field` is computed — no backing storage
    #[test]
    fn computed_property() {
        check(
            indoc! {"
                class Rect:
                    let area: int
                        get() = self.w * self.h
            "},
            indoc! {"
                class Rect:
                    @property
                    def area(self) -> int:
                        return self.w * self.h
            "},
        );
    }

    /// `static let` is a class-level computed property: python has no such thing,
    /// so it lowers to a descriptor emitted into the preamble
    #[test]
    fn static_property_lowers_to_a_descriptor() {
        check(
            indoc! {"
                class Config:
                    static let name: str
                        get() = \"config\"
            "},
            indoc! {"
                class _by_static_property:
                    def __init__(self, fget):
                        self._fget = fget
                    def __get__(self, instance, owner=None):
                        return self._fget(owner if owner is not None else type(instance))

                class Config:
                    @_by_static_property
                    def name(cls) -> str:
                        return \"config\"
            "},
        );
    }

    /// an explicit `field:` declaration decouples the storage type from the
    /// property's public type
    #[test]
    fn explicit_backing_field() {
        check(
            indoc! {"
                class Bag:
                    let items: Sequence[int]
                        field: list[int] = []
                        get() = field
            "},
            indoc! {"
                from typing import Sequence
                class Bag:
                    def __init__(self) -> None:
                        self.__items: list[int] = []
                    @property
                    def items(self) -> Sequence[int]:
                        return self.__items
            "},
        );
    }

    /// `var` with only a getter gains a pass-through setter
    #[test]
    fn var_get_only_gains_passthrough_setter() {
        check(
            indoc! {"
                class A:
                    var x: int = 0
                        get() = field
            "},
            indoc! {"
                class A:
                    def __init__(self) -> None:
                        self.__x: int = 0
                    @property
                    def x(self) -> int:
                        return self.__x
                    @x.setter
                    def x(self, value: int) -> None:
                        self.__x = value
            "},
        );
    }

    /// a backing initialiser is injected into an existing constructor rather than
    /// a synthesized one, ahead of the user's own statements
    #[test]
    fn backing_field_injected_into_existing_init() {
        check(
            indoc! {"
                class A:
                    def __init__(self, n: int):
                        self.n = n

                    var x: int = 0
                        get() = field
            "},
            indoc! {"
                class A:
                    def __init__(self, n: int):
                        self.__x: int = 0
                        self.n = n

                    @property
                    def x(self) -> int:
                        return self.__x
                    @x.setter
                    def x(self, value: int) -> None:
                        self.__x = value
            "},
        );
    }

    /// two properties share one synthesized constructor, in declaration order
    #[test]
    fn multiple_backing_fields_share_one_constructor() {
        check(
            indoc! {"
                class A:
                    var x: int = 0
                        get() = field
                    var y: str = \"\"
                        get() = field
            "},
            indoc! {"
                class A:
                    def __init__(self) -> None:
                        self.__x: int = 0
                        self.__y: str = \"\"
                    @property
                    def x(self) -> int:
                        return self.__x
                    @x.setter
                    def x(self, value: int) -> None:
                        self.__x = value
                    @property
                    def y(self) -> str:
                        return self.__y
                    @y.setter
                    def y(self, value: str) -> None:
                        self.__y = value
            "},
        );
    }

    /// a constructor this pass can't inject into safely (a bodyless `init(...)`,
    /// whose body the `init_method` pass completes) falls back to the class-level
    /// declaration rather than emitting wrongly indented code
    #[test]
    fn bodyless_init_falls_back_to_class_level() {
        check(
            indoc! {"
                class A:
                    init(self, let a: int)
                    var x: int = 0
                        get() = field
            "},
            indoc! {"
                class A:
                    def __init__(self, a: int):
                        self.a: int = a
                    __x: int = 0
                    @property
                    def x(self) -> int:
                        return self.__x
                    @x.setter
                    def x(self, value: int) -> None:
                        self.__x = value
            "},
        );
    }

    /// `private` shifts the construct one underscore deeper — property `_x`,
    /// storage `__x` — and in-class accesses spelled under the public name are
    /// redirected to it
    #[test]
    fn private_property_is_renamed() {
        check(
            indoc! {"
                class A:
                    private var x: int = 0
                        get() = field
                        set(value):
                            field = value

                    def bump(self):
                        self.x = self.x + 1
            "},
            indoc! {"
                class A:
                    def __init__(self) -> None:
                        self.__x: int = 0
                    @property
                    def _x(self) -> int:
                        return self.__x
                    @_x.setter
                    def _x(self, value: int) -> None:
                        self.__x = value

                    def bump(self):
                        self._x = self._x + 1
            "},
        );
    }

    /// `private` composes with `let`: a read-only private property, getter only
    #[test]
    fn private_let_property() {
        check(
            indoc! {"
                class A:
                    private let n: int
                        field: int = 5

                    def f(self):
                        return self.n
            "},
            indoc! {"
                class A:
                    def __init__(self) -> None:
                        self.__n: int = 5
                    @property
                    def _n(self) -> int:
                        return self.__n

                    def f(self):
                        return self._n
            "},
        );
    }

    /// a same-named attribute on another object is not a property access and must
    /// keep its name
    #[test]
    fn private_rename_only_touches_the_receiver() {
        check(
            indoc! {"
                class A:
                    private var x: int = 0
                        get() = field

                    def f(self, other):
                        return other.x + self.x
            "},
            indoc! {"
                class A:
                    def __init__(self) -> None:
                        self.__x: int = 0
                    @property
                    def _x(self) -> int:
                        return self.__x
                    @_x.setter
                    def _x(self, value: int) -> None:
                        self.__x = value

                    def f(self, other):
                        return other.x + self._x
            "},
        );
    }

    /// a basedpython construct inside an accessor body is lowered like anywhere
    /// else: the body is passed through as source, so a sibling pass's rewrite
    /// survives. `super.a` left un-lowered is still *valid* python, so nothing
    /// downstream would have caught it — it just raised at runtime
    #[test]
    fn accessor_body_composes_with_other_lowerings() {
        check(
            indoc! {"
                class A:
                    let a: int
                        get() = 1

                class B(A):
                    override let a
                        get() = super.a + 1
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    @property
                    def a(self) -> int:
                        return 1

                class B(A):
                    @property
                    @override
                    def a(self):
                        return super().a + 1
            "},
        );
    }

    /// the same for a block-bodied accessor, whose statements are re-indented to the
    /// method body without disturbing anything inside them
    #[test]
    fn block_accessor_body_composes_with_other_lowerings() {
        check(
            indoc! {"
                class A:
                    var v: int? = None
                        get():
                            return field
                        set(value):
                            field = value ?? 0
            "},
            indoc! {"
                class A:
                    def __init__(self) -> None:
                        self.__v: int | None = None
                    @property
                    def v(self) -> int | None:
                        return self.__v
                    @v.setter
                    def v(self, value: int | None) -> None:
                        self.__v = value if value is not None else 0
            "},
        );
    }

    /// a plain declaration with no accessor block keeps its class-attribute
    /// lowering — properties only appear when accessors are asked for
    #[test]
    fn plain_declarations_unchanged() {
        check(
            indoc! {"
                class Point:
                    var y: int = 0
            "},
            indoc! {"
                class Point:
                    y: int = 0
            "},
        );
    }

    /// `override` lands on both accessors, under `@property` / `@x.setter` so it
    /// applies to the accessor function itself
    #[test]
    fn override_modifier_decorates_both_accessors() {
        check(
            indoc! {"
                class Child:
                    override var age: int = 0
                        get() = field
                        set(value):
                            field = value
            "},
            indoc! {"
                from typing_extensions import override
                class Child:
                    def __init__(self) -> None:
                        self.__age: int = 0
                    @property
                    @override
                    def age(self) -> int:
                        return self.__age
                    @age.setter
                    @override
                    def age(self, value: int) -> None:
                        self.__age = value
            "},
        );
    }

    /// `final` marks the property; the declared type survives the `__final__`
    /// marker the modifier chain swaps in, and no `Final` annotation is emitted
    #[test]
    fn final_modifier_keeps_the_declared_type() {
        check(
            indoc! {"
                class F:
                    final var x: int = 0
                        get() = field
            "},
            indoc! {"
                from typing import final
                class F:
                    def __init__(self) -> None:
                        self.__x: int = 0
                    @property
                    @final
                    def x(self) -> int:
                        return self.__x
                    @x.setter
                    @final
                    def x(self, value: int) -> None:
                        self.__x = value
            "},
        );
    }

    /// an abstract property declares a shape: `@abstractmethod` and no body
    #[test]
    fn abstract_property_is_bodyless() {
        check(
            indoc! {"
                class Shape:
                    abstract let area: int
                        get() = field
            "},
            indoc! {"
                from abc import abstractmethod
                class Shape:
                    __area: int
                    @property
                    @abstractmethod
                    def area(self) -> int: ...
            "},
        );
    }

    /// `late var` defers initialisation: a bare class-level annotation, and
    /// no `Optional` in sight
    #[test]
    fn late_var_is_a_bare_annotation() {
        check(
            indoc! {"
                class Loader:
                    late var handle: str
            "},
            indoc! {"
                class Loader:
                    handle: str
            "},
        );
    }

    /// `late field` gives the backing storage no initialiser at all — not even
    /// the property's own
    #[test]
    fn late_backing_field_has_no_initialiser() {
        check(
            indoc! {"
                class Bag:
                    let items: list[int]
                        late field: list[int]
                        get() = field
            "},
            indoc! {"
                class Bag:
                    __items: list[int]
                    @property
                    def items(self) -> list[int]:
                        return self.__items
            "},
        );
    }

    /// a block-bodied getter keeps its statements, re-indented under the `def`
    #[test]
    fn block_bodied_getter() {
        check(
            indoc! {"
                class A:
                    let label: str
                        get():
                            prefix = \"x\"
                            return prefix + self.name
            "},
            indoc! {"
                class A:
                    @property
                    def label(self) -> str:
                        prefix = \"x\"
                        return prefix + self.name
            "},
        );
    }
}
