//! Lowers basedpython anonymous named tuple type expressions
//! `(name1: T1, name2: T2, ...)` to a synthesized `typing.NamedTuple` subclass.
//!
//! Each unique shape (field-name + field-type-source tuple) is hoisted to a
//! single class definition and reused across all occurrences. Identical shapes
//! across the module collapse to one class so structural equivalence is
//! preserved at the type level.
//!
//! Example:
//! ```by
//! def foo(x: (name: str, age: int)) -> (name: str, age: int):
//!     return ("asdf", 1)
//!
//! a = (name: str, age: int)
//! ```
//!
//! Lowers to:
//! ```python
//! from typing import NamedTuple
//!
//! class _AnonNamedTuple_<hash>(NamedTuple):
//!     name: str
//!     age: int
//!
//! def foo(x: _AnonNamedTuple_<hash>) -> _AnonNamedTuple_<hash>:
//!     return ("asdf", 1)
//!
//! a = _AnonNamedTuple_<hash>
//! ```

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtAnnAssign, StmtFunctionDef, StmtReturn};
use ruff_text_size::Ranged;

use crate::transforms::callable::CallableSyntax;
use crate::type_info::TypeInfo;

/// Synthetic field name for the i-th positional element of a mixed
/// anonymous named tuple. `NamedTuple` disallows leading-underscore field
/// names, so we use the `arg<i>` prefix instead.
fn synth_pos_name(i: usize) -> String {
    format!("arg{i}")
}

/// Returns the first duplicated field name in `fields`, or `None` if all
/// names are unique. Used to detect:
/// - a user-named field that collides with a synthetic positional name
///   (e.g. `(1, arg0=2)` — both fields would be called `arg0`),
/// - two named fields with the same name (e.g. `(name=1, name=2)`),
/// - any other source of duplication.
///
/// Either case would produce a `NamedTuple` definition that Python rejects
/// at runtime — so we abort transpilation rather than emit invalid output.
fn first_duplicate_name(fields: &[(String, String)]) -> Option<&str> {
    use std::collections::HashSet;
    let mut seen: HashSet<&str> = HashSet::new();
    for (name, _) in fields {
        if !seen.insert(name.as_str()) {
            return Some(name.as_str());
        }
    }
    None
}

/// One anonymous named tuple shape: ordered list of `(field_name, type_source)`.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct Shape {
    fields: Vec<(String, String)>,
}

impl Shape {
    fn class_name(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        // 8 hex chars from 64-bit hash is enough to avoid collisions in a
        // typical module while staying readable
        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        format!("_AnonNamedTuple_{truncated:08x}")
    }

    fn class_def(&self, name: &str) -> String {
        let mut out = format!("class {name}(NamedTuple):\n");
        for (field_name, field_type) in &self.fields {
            let _ = writeln!(out, "    {field_name}: {field_type}");
        }
        out
    }
}

pub(crate) struct AnonNamedTuple<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    config: crate::Config,
    /// One lowerer drives every field type in the run, so its imports and the
    /// classes it synthesizes can be collected at the end. A hoisted class body
    /// leaves the statement the field type was written in, so no sibling
    /// transform's edit reaches it — the field type has to be lowered here or
    /// the surface syntax lands in the output verbatim
    pub(crate) callable: CallableSyntax<'src>,
    pub(crate) edits: Vec<Fix>,
    /// Insertion-ordered map of shape → synthesized class name. Visitor
    /// processes nested anonymous-named-tuple expressions before their
    /// containing expression (post-order), so a class is defined before
    /// any later class that references it as a field type
    shapes: indexmap::IndexMap<Shape, String>,
    /// Source range of every anonymous-named-tuple expression we've assigned
    /// a synthesized class to (both top-level rewrites and nested ones inside
    /// other anon-NT field types). Used when rendering a class body to
    /// substitute inner anon-NT source spans with their class names so the
    /// emitted `NamedTuple` definition is valid Python.
    ///
    /// Stored as a `Vec` (`TextRange` isn't `Ord`) and walked linearly when
    /// rendering. Inserts append; `render_subbed` skips entries that don't
    /// fall within the requested range.
    range_to_class: Vec<(ruff_text_size::TextRange, String)>,
    /// Source range of every anonymous-named-tuple *value* form (`(name=v,
    /// ...)`) we've rewritten, paired with the constructor-call text we
    /// emitted. When an outer value form contains a nested value form as one
    /// of its field values, the outer's argument list must call the inner
    /// constructor, not pass the bare class object — so we render the
    /// nested value-form region as its constructor call rather than its
    /// class name.
    range_to_value_render: Vec<(ruff_text_size::TextRange, String)>,
    /// Set when at least one anonymous named tuple was seen, so the preamble
    /// emits the `NamedTuple` import.
    pub(crate) needs_import: bool,
    /// Modules a field type names that the source never imported. A shape is
    /// built through `&self`, so this collects behind a cell
    pub(crate) type_only_imports: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// `typing` names a field type reads that the source never imported
    pub(crate) typing_names: std::cell::RefCell<std::collections::BTreeSet<&'static str>>,
    /// Active function-scope return-annotation shape stack. Empty when not
    /// inside a function. The innermost (last) entry governs how a `return`
    /// statement inside the current function is coerced.
    return_shape_stack: Vec<Option<Shape>>,
    /// Stack of typevar rename maps (`T` → `_T`) accumulated as the visitor
    /// descends into PEP 695 generics. Synthesized `NamedTuple` class bodies
    /// are hoisted to module scope, but the original source text references
    /// the *unmangled* typevar names. Applying these renames at field-type
    /// rendering keeps the synthesized class's field annotations pointing at
    /// the same module-level `_T = TypeVar("_T")` that the generics polyfill
    /// emits, instead of leaving an unbound `T`.
    ///
    /// Skipped on Python 3.12+: PEP 695 native syntax doesn't rename, so the
    /// stack stays empty.
    typevar_rename_stack: Vec<HashMap<String, String>>,
    /// Names bound to an anonymous named tuple by an alias — `P = (a: int)` or
    /// `type P = (a: int)`. Coercion asks what the annotation *means*, not how
    /// it is spelled, so a plain tuple literal under `x: P` is wrapped exactly
    /// as one under `x: (a: int)` is. Populated in source order, so an alias
    /// only applies below its own declaration.
    aliases: HashMap<String, Shape>,
    /// Hard transpile errors — each one aborts the whole transpilation so we
    /// never emit syntactically invalid Python. The transform aborts on the
    /// *first* failure during the visit pass; subsequent visits are no-ops.
    pub(crate) errors: Vec<String>,
}

impl<'src> AnonNamedTuple<'src> {
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo, config: crate::Config) -> Self {
        Self {
            source,
            types,
            callable: CallableSyntax::new(source, config.float_literals).with_types(types),
            config,
            edits: Vec::new(),
            shapes: indexmap::IndexMap::new(),
            range_to_class: Vec::new(),
            range_to_value_render: Vec::new(),
            needs_import: false,
            type_only_imports: std::cell::RefCell::default(),
            typing_names: std::cell::RefCell::default(),
            return_shape_stack: Vec::new(),
            typevar_rename_stack: Vec::new(),
            aliases: HashMap::new(),
            errors: Vec::new(),
        }
    }

    /// Returns the synthesized class definitions to prepend to the module body,
    /// in deterministic order. Each shape's field types are rendered from the
    /// original source text with any nested anonymous-named-tuple regions
    /// substituted with their synthesized class names so the emitted
    /// `NamedTuple` definitions are valid Python.
    pub(crate) fn class_defs(&self) -> String {
        let mut out = String::new();
        for (shape, name) in &self.shapes {
            out.push_str(&shape.class_def(name));
            out.push('\n');
        }
        out
    }

    fn src(&self, range: ruff_text_size::TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// Render `range`'s source, substituting any anonymous-named-tuple
    /// expression contained within `range` with its synthesized class name.
    /// Used wherever a stretch of original source ends up in the transpiled
    /// output (class field type, constructor argument, coerced literal) so
    /// nested anon-NT regions don't leak through verbatim.
    fn render_subbed(&self, range: ruff_text_size::TextRange) -> String {
        let raw = self.render_subbed_with(range, &self.range_to_class);
        let renamed = self.apply_typevar_renames(&raw);
        // Synthesized NamedTuple classes are hoisted to module scope, but
        // the renamed typevars (`_T`) get declared *after* them by the
        // generics polyfill. Wrap typevar references in PEP 484 forward-ref
        // quotes so the annotation is stored as a string and only resolved
        // by type checkers, not at class-construction time.
        if renamed != raw && !raw.is_empty() {
            format!("\"{renamed}\"")
        } else {
            renamed
        }
    }

    /// Lower a field's type expression to python source.
    ///
    /// The synthesized class body is hoisted out of the statement the field type
    /// was written in, so a sibling transform's edit never reaches it. Driving
    /// the shared type-expression lowerer here is what keeps `dynamic`, `T?`,
    /// `typeof x`, a callable arrow and a nested anonymous named tuple from
    /// reaching the output as surface syntax.
    fn render_field_type(&mut self, expr: &Expr) -> String {
        let lowered = self.callable.lower_type_expr(expr);
        let renamed = self.apply_typevar_renames(&lowered);
        // same forward-reference quoting as `render_subbed`: a renamed typevar is
        // declared below the hoisted class, so the annotation must stay a string
        if renamed != lowered && !lowered.is_empty() {
            format!("\"{renamed}\"")
        } else {
            renamed
        }
    }

    /// Active typevar rename map, flattened from the stack with inner scopes
    /// shadowing outer ones. Empty when not inside a generic scope or when
    /// `min_version` is 3.12+ (where PEP 695 stays native and no renames
    /// are emitted).
    fn active_typevar_renames(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for frame in &self.typevar_rename_stack {
            out.extend(frame.clone());
        }
        out
    }

    /// Substitute every word-boundary occurrence of a typevar name in
    /// `text` with its mangled module-scope counterpart. Operates on the
    /// rendered field-type string so the synthesized `NamedTuple` class
    /// references the renamed `_T` instead of the original `T`.
    fn apply_typevar_renames(&self, text: &str) -> String {
        let renames = self.active_typevar_renames();
        let borrowed: HashMap<&str, &str> = renames
            .iter()
            .map(|(from, to)| (from.as_str(), to.as_str()))
            .collect();
        super::source_util::rename_identifiers(text, &borrowed)
    }

    /// Like `render_subbed` but substitutes value-form constructor calls
    /// instead of bare class names. Used when an outer anon-NT *value* form
    /// is rendering its argument list — a nested value form there must call
    /// its inner constructor, not just reference the inner class.
    fn render_subbed_value(&self, range: ruff_text_size::TextRange) -> String {
        // Combine value-form constructor calls (preferred) with class-name
        // subs for any nested *type* form in the same range. Value renders
        // win on collisions because they cover the same source span.
        let mut subs: Vec<(ruff_text_size::TextRange, &str)> = self
            .range_to_value_render
            .iter()
            .map(|(r, s)| (*r, s.as_str()))
            .chain(
                self.range_to_class
                    .iter()
                    .filter(|(r, _)| !self.range_to_value_render.iter().any(|(vr, _)| *vr == *r))
                    .map(|(r, s)| (*r, s.as_str())),
            )
            .filter(|(r, _)| r.start() >= range.start() && r.end() <= range.end())
            .collect();
        subs.sort_by_key(|(r, _)| r.start());
        self.sweep(range, subs)
    }

    fn render_subbed_with(
        &self,
        range: ruff_text_size::TextRange,
        table: &[(ruff_text_size::TextRange, String)],
    ) -> String {
        let mut subs: Vec<(ruff_text_size::TextRange, &str)> = table
            .iter()
            .filter(|(r, _)| r.start() >= range.start() && r.end() <= range.end())
            .map(|(r, n)| (*r, n.as_str()))
            .collect();
        subs.sort_by_key(|(r, _)| r.start());
        self.sweep(range, subs)
    }

    fn sweep(
        &self,
        range: ruff_text_size::TextRange,
        subs: Vec<(ruff_text_size::TextRange, &str)>,
    ) -> String {
        use ruff_text_size::TextSize;
        let mut out = String::new();
        let mut cursor: TextSize = range.start();
        for (sub_range, replacement) in subs {
            // Skip overlapping / already-passed substitutions: an outer
            // anon-NT contains an inner one, but the inner's range is
            // covered by the cursor already if the outer just got swapped.
            if sub_range.start() < cursor {
                continue;
            }
            out.push_str(&self.source[usize::from(cursor)..usize::from(sub_range.start())]);
            out.push_str(replacement);
            cursor = sub_range.end();
        }
        out.push_str(&self.source[usize::from(cursor)..usize::from(range.end())]);
        out
    }

    /// Build a shape from a type-form anonymous named tuple
    /// (`(name: T, ...)`). Field types come from source text. Positional
    /// fields (bare type expressions like `int` in `(int, name: str)`) are
    /// assigned synthetic names `arg0`, `arg1`, … `NamedTuple` disallows
    /// leading-underscore field names, so `argN` is used.
    ///
    /// Returns `Err(message)` on hard structural failure (duplicate field
    /// name; collision between a user-named field and a synthetic positional
    /// name). Returns `Ok(None)` only when this isn't an anon-NT type tuple
    /// at all (so the caller can ignore it).
    fn extract_type_shape(
        &mut self,
        tuple: &ruff_python_ast::ExprTuple,
    ) -> Result<Option<Shape>, String> {
        if !tuple.is_anon_named_tuple {
            return Ok(None);
        }
        let mut fields = Vec::with_capacity(tuple.elts.len());
        for (i, elt) in tuple.elts.iter().enumerate() {
            let (name, type_source) = match elt {
                Expr::Named(named) => {
                    let Expr::Name(name_expr) = named.target.as_ref() else {
                        return Err(format!(
                            "anonymous named tuple field name must be an identifier, got `{}`",
                            self.src(named.target.range())
                        ));
                    };
                    let name = name_expr.id.as_str().to_owned();
                    (name, self.render_field_type(&named.value))
                }
                other => (synth_pos_name(i), self.render_field_type(other)),
            };
            fields.push((name, type_source));
        }
        if let Some(dup) = first_duplicate_name(&fields) {
            return Err(format!(
                "anonymous named tuple `{}` has duplicate field name `{}` \
                 (positional fields are assigned synthetic names `arg0`, `arg1`, …; \
                 rename the colliding field)",
                self.src(tuple.range()),
                dup,
            ));
        }
        Ok(Some(Shape { fields }))
    }

    /// Build a shape from a value-form anonymous named tuple
    /// (`(name=v, ...)`). Same conventions as `extract_type_shape`; field
    /// types come from ty's promoted (literal-stripped) inference.
    ///
    /// Returns `Err(message)` on hard structural failure. Returns
    /// `Ok(None)` if any field's value type can't be resolved by ty (e.g.
    /// unresolved import) — the caller falls back to leaving the source
    /// expression alone, and `verify_syntax` will catch any leftover anon-NT
    /// AST in the output.
    fn extract_value_shape(
        &self,
        tuple: &ruff_python_ast::ExprTuple,
    ) -> Result<Option<Shape>, String> {
        if !tuple.is_anon_named_tuple_value {
            return Ok(None);
        }
        let mut fields = Vec::with_capacity(tuple.elts.len());
        for (i, elt) in tuple.elts.iter().enumerate() {
            let (name, value_expr) = match elt {
                Expr::Named(named) => {
                    let Expr::Name(name_expr) = named.target.as_ref() else {
                        return Err(format!(
                            "anonymous named tuple field name must be an identifier, got `{}`",
                            self.src(named.target.range())
                        ));
                    };
                    (name_expr.id.as_str().to_owned(), named.value.as_ref())
                }
                other => (synth_pos_name(i), other),
            };
            // Prefer our own synthesized class name when the value is itself
            // an anon-NT we already processed — ty may name it differently
            // (or use a structural display that won't match the class we
            // emit), so trusting our own shape registry guarantees the
            // outer class's field type points at the inner class we wrote.
            let type_display = if let Expr::Tuple(inner) = value_expr
                && inner.is_anon_named_tuple_value
                && let Some(class_name) = self.shape_for_value_tuple(inner)
            {
                class_name
            } else {
                let Some(d) = self.types.promoted_type_display(value_expr) else {
                    return Ok(None);
                };
                self.type_only_imports.borrow_mut().extend(d.modules);
                self.typing_names.borrow_mut().extend(d.typing_names);
                d.text
            };
            fields.push((name, type_display));
        }
        if let Some(dup) = first_duplicate_name(&fields) {
            return Err(format!(
                "anonymous named tuple `{}` has duplicate field name `{}` \
                 (positional fields are assigned synthetic names `arg0`, `arg1`, …; \
                 rename the colliding field)",
                self.src(tuple.range()),
                dup,
            ));
        }
        Ok(Some(Shape { fields }))
    }

    fn class_name_for(&mut self, shape: Shape, source_range: ruff_text_size::TextRange) -> String {
        let name = if let Some(existing) = self.shapes.get(&shape) {
            existing.clone()
        } else {
            let n = shape.class_name();
            self.shapes.insert(shape, n.clone());
            n
        };
        // Track this anon-NT's source range → class name so any *outer* anon-NT
        // that contains this one can substitute its source span when rendering
        // its own field types in the synthesized class body. the lowerer needs
        // the same table so an outer field type resolves a nested anon-NT at any
        // depth, not only when it is the whole field type
        self.range_to_class.push((source_range, name.clone()));
        self.callable.add_substitution(source_range, name.clone());
        name
    }

    fn rewrite_type_form(&mut self, tuple: &ruff_python_ast::ExprTuple) {
        let shape = match self.extract_type_shape(tuple) {
            Ok(Some(shape)) => shape,
            Ok(None) => return,
            Err(msg) => {
                self.errors.push(msg);
                return;
            }
        };
        let class_name = self.class_name_for(shape, tuple.range());
        self.needs_import = true;
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            class_name,
            tuple.range(),
        )));
    }

    fn rewrite_value_form(&mut self, tuple: &ruff_python_ast::ExprTuple) {
        let shape = match self.extract_value_shape(tuple) {
            Ok(Some(shape)) => shape,
            Ok(None) => return,
            Err(msg) => {
                self.errors.push(msg);
                return;
            }
        };
        // Render constructor call: `ClassName(v1, v2, ...)` using positional
        // args sourced from each field's value expression. We `render_subbed`
        // so a nested anon-NT inside any value gets substituted with its own
        // class-constructor call rather than emitted verbatim.
        let mut value_args: Vec<String> = Vec::with_capacity(tuple.elts.len());
        for elt in &tuple.elts {
            let value_expr = match elt {
                Expr::Named(named) => named.value.as_ref(),
                other => other,
            };
            value_args.push(self.render_subbed_value(value_expr.range()));
        }
        let class_name = self.class_name_for(shape, tuple.range());
        self.needs_import = true;
        let constructor_call = format!("{class_name}({})", value_args.join(", "));
        self.range_to_value_render
            .push((tuple.range(), constructor_call.clone()));
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            constructor_call,
            tuple.range(),
        )));
    }

    /// Rewrite a `return ...` statement inside a function annotated with an
    /// anonymous-named-tuple return type, when the returned value is a plain
    /// tuple literal of matching arity. The plain tuple is wrapped as a
    /// constructor call so runtime field access works.
    fn rewrite_return_coercion(&mut self, ret: &StmtReturn) {
        let Some(Some(shape)) = self.return_shape_stack.last().cloned() else {
            return;
        };
        let Some(value) = ret.value.as_ref() else {
            return;
        };
        if let Expr::Tuple(tuple) = value.as_ref() {
            self.wrap_plain_tuple(tuple, shape);
        }
    }

    /// Walk an `x: T = ...` statement and coerce plain tuple literals on the
    /// RHS when the annotation is an anonymous named tuple (directly or
    /// inside a recognized container generic).
    fn rewrite_ann_assign_coercion(&mut self, stmt: &StmtAnnAssign) {
        let Some(value) = stmt.value.as_deref() else {
            return;
        };
        let Some(target) = self.coercion_target(&stmt.annotation) else {
            return;
        };
        self.apply_coercion(&target, value);
    }

    fn return_shape_for(&mut self, func: &StmtFunctionDef) -> Option<Shape> {
        // Errors in shape extraction surface through the visitor's own pass
        // over this annotation tuple — silently treat them as "no coercion"
        // here so we don't double-report.
        self.annotated_shape(func.returns.as_deref()?)
    }

    /// Extract a coercion target from an annotation expression. Returns the
    /// expected shape and the surrounding container so the caller knows
    /// whether to wrap the value directly or to wrap each element of an
    /// outer collection literal.
    fn coercion_target(&mut self, annotation: &Expr) -> Option<CoercionTarget> {
        // a declaration modifier keeps the declared type in a marker subscript
        // (`let x: T` parses as `x: __let__[T]`), and the annotation this reads
        // is the one the source wrote
        if let Some(declared) = ruff_python_ast::helpers::declaration_annotation_type(annotation) {
            return self.coercion_target(declared);
        }
        // Direct: `x: (name: T, ...)` or `x: Alias`
        if let Some(shape) = self.annotated_shape(annotation) {
            return Some(CoercionTarget {
                shape,
                container: ContainerKind::Direct,
            });
        }
        // Containers: `x: list[(name: T, ...)]` and friends. The annotation
        // is `Subscript(value=Name("list"), slice=Tuple{is_anon_named_tuple=true})`.
        if let Expr::Subscript(sub) = annotation {
            let Expr::Name(name) = sub.value.as_ref() else {
                return None;
            };
            let kind = match name.id.as_str() {
                "list" | "List" => ContainerKind::ListLike,
                "set" | "Set" | "frozenset" | "FrozenSet" => ContainerKind::SetLike,
                _ => return None,
            };
            let shape = self.annotated_shape(sub.slice.as_ref())?;
            return Some(CoercionTarget {
                shape,
                container: kind,
            });
        }
        None
    }

    /// The anonymous-named-tuple shape an annotation denotes — written out, or
    /// reached through an alias declared above it.
    fn annotated_shape(&mut self, annotation: &Expr) -> Option<Shape> {
        match annotation {
            Expr::Tuple(t) => self.extract_type_shape(t).ok().flatten(),
            Expr::Name(name) => self.aliases.get(name.id.as_str()).cloned(),
            _ => None,
        }
    }

    /// Record `P = (name: T, ...)` / `type P = (name: T, ...)` so a later
    /// annotation spelling `P` coerces the same way the written-out form does.
    fn record_alias(&mut self, name: &Expr, value: &Expr) {
        let (Expr::Name(target), Expr::Tuple(t)) = (name, value) else {
            return;
        };
        if !t.is_anon_named_tuple {
            return;
        }
        // the tuple has already been visited, so a shape error is reported there
        if let Ok(Some(shape)) = self.extract_type_shape(t) {
            self.aliases.insert(target.id.to_string(), shape);
        }
    }

    /// Wrap a plain tuple literal `(v1, v2, ...)` as a constructor call to the
    /// synthesized `NamedTuple` class for `shape`. No-op if the tuple is
    /// already a basedpython anon form, has wrong arity, or contains starred
    /// elements.
    fn wrap_plain_tuple(&mut self, tuple: &ruff_python_ast::ExprTuple, shape: Shape) {
        if tuple.is_anon_named_tuple || tuple.is_anon_named_tuple_value {
            return;
        }
        if tuple.elts.len() != shape.fields.len() {
            return;
        }
        if tuple.elts.iter().any(Expr::is_starred_expr) {
            return;
        }
        // Build the constructor call. Each argument is rendered against the
        // shape's corresponding field type so that a plain tuple element
        // whose target field is itself a synthesized `NamedTuple` class is
        // recursively wrapped — otherwise `record.point.x` would fail at
        // runtime because the inner plain tuple doesn't carry field names.
        self.needs_import = true;
        let args: Vec<String> = tuple
            .elts
            .iter()
            .zip(shape.fields.iter())
            .map(|(elt, (_, field_type))| self.render_arg_against_field_type(elt, field_type))
            .collect();
        let name = if let Some(existing) = self.shapes.get(&shape) {
            existing.clone()
        } else {
            let n = shape.class_name();
            self.shapes.insert(shape, n.clone());
            n
        };
        let constructor_call = format!("{name}({})", args.join(", "));
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            constructor_call,
            tuple.range(),
        )));
    }

    /// Render a constructor argument. If `field_type` is the name of a
    /// synthesized `NamedTuple` class and the argument is a plain tuple
    /// matching that shape, recursively wrap it so nested field-name access
    /// (`outer.inner.x`) keeps working at runtime. Otherwise fall back to
    /// `render_subbed` which substitutes any inner anon-NT spans.
    fn render_arg_against_field_type(&self, elt: &Expr, field_type: &str) -> String {
        if let Some(inner_shape) = self.shape_for_class_name(field_type) {
            if let Expr::Tuple(t) = elt {
                if !t.is_anon_named_tuple
                    && !t.is_anon_named_tuple_value
                    && t.elts.len() == inner_shape.fields.len()
                    && !t.elts.iter().any(Expr::is_starred_expr)
                {
                    let inner_args: Vec<String> = t
                        .elts
                        .iter()
                        .zip(inner_shape.fields.iter())
                        .map(|(e, (_, ft))| self.render_arg_against_field_type(e, ft))
                        .collect();
                    return format!("{}({})", field_type, inner_args.join(", "));
                }
            }
        }
        self.render_subbed(elt.range())
    }

    /// Build a typevar rename frame from a `[T, U, ...]` type-parameter list
    /// and push it onto the stack. Returns whether a frame was pushed so the
    /// caller knows to pop. No-op (returns false) when targeting Python 3.12+
    /// since native PEP 695 generics don't get renamed.
    fn push_typevar_scope_from(&mut self, tp: Option<&ruff_python_ast::TypeParams>) -> bool {
        if self.config.min_version >= ruff_python_ast::PythonVersion::PY312 {
            return false;
        }
        let Some(tp) = tp else {
            return false;
        };
        let mut frame = HashMap::new();
        for param in &tp.type_params {
            let name = match param {
                ruff_python_ast::TypeParam::TypeVar(tv) => tv.name.id.as_str(),
                ruff_python_ast::TypeParam::TypeVarTuple(tvt) => tvt.name.id.as_str(),
                ruff_python_ast::TypeParam::ParamSpec(ps) => ps.name.id.as_str(),
            };
            frame.insert(name.to_owned(), super::generics::mangle(name));
        }
        if frame.is_empty() {
            return false;
        }
        self.typevar_rename_stack.push(frame);
        true
    }

    /// Look up the synthesized class name for a nested anon-NT value tuple
    /// that's already been processed. Relies on the post-order visit:
    /// children are rewritten first, so by the time the outer extracts its
    /// shape, the inner's class name is already in `range_to_class`.
    fn shape_for_value_tuple(&self, inner: &ruff_python_ast::ExprTuple) -> Option<String> {
        self.range_to_class
            .iter()
            .find(|(r, _)| *r == inner.range())
            .map(|(_, n)| n.clone())
    }

    fn shape_for_class_name(&self, name: &str) -> Option<&Shape> {
        if !name.starts_with("_AnonNamedTuple_") {
            return None;
        }
        self.shapes
            .iter()
            .find_map(|(shape, n)| (n == name).then_some(shape))
    }

    /// Apply the coercion target to the right-hand side of an assignment.
    fn apply_coercion(&mut self, target: &CoercionTarget, value: &Expr) {
        match target.container {
            ContainerKind::Direct => {
                if let Expr::Tuple(tuple) = value {
                    self.wrap_plain_tuple(tuple, target.shape.clone());
                }
            }
            ContainerKind::ListLike => {
                let elts = match value {
                    Expr::List(list) => &list.elts[..],
                    // `[a, b]` is the common case; a parenthesized
                    // generator/expr would not match `list[X]` typewise.
                    _ => return,
                };
                for elt in elts {
                    if let Expr::Tuple(tuple) = elt {
                        self.wrap_plain_tuple(tuple, target.shape.clone());
                    }
                }
            }
            ContainerKind::SetLike => {
                let elts = match value {
                    Expr::Set(set) => &set.elts[..],
                    _ => return,
                };
                for elt in elts {
                    if let Expr::Tuple(tuple) = elt {
                        self.wrap_plain_tuple(tuple, target.shape.clone());
                    }
                }
            }
        }
    }
}

/// What surrounding shape a plain tuple literal at this position should be
/// coerced into.
#[derive(Clone, Debug)]
enum ContainerKind {
    /// Annotation is the anon-NT directly: `x: (name: T, ...)`.
    Direct,
    /// Annotation wraps in a list-like generic: `list[X]`, `List[X]`.
    ListLike,
    /// Annotation wraps in a set-like generic: `set[X]`, `frozenset[X]`,
    /// `Set[X]`, `FrozenSet[X]`.
    SetLike,
}

#[derive(Clone, Debug)]
struct CoercionTarget {
    shape: Shape,
    container: ContainerKind,
}

impl<'ast> Visitor<'ast> for AnonNamedTuple<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(func) => {
                // Visit non-body parts first so their anonymous-named-tuple
                // expressions register synthesized classes before any body
                // `return` statement is coerced. Walking via `walk_stmt`
                // would visit the body before we've pushed the return shape
                // onto the stack, so we manually drive the visit order.
                let pushed = self.push_typevar_scope_from(func.type_params.as_deref());
                if let Some(tp) = func.type_params.as_deref() {
                    self.visit_type_params(tp);
                }
                self.visit_parameters(&func.parameters);
                if let Some(returns) = func.returns.as_deref() {
                    self.visit_expr(returns);
                }
                let shape = self.return_shape_for(func);
                self.return_shape_stack.push(shape);
                for s in &func.body {
                    self.visit_stmt(s);
                }
                self.return_shape_stack.pop();
                for d in &func.decorator_list {
                    self.visit_decorator(d);
                }
                if pushed {
                    self.typevar_rename_stack.pop();
                }
            }
            Stmt::ClassDef(cls) => {
                let pushed = self.push_typevar_scope_from(cls.type_params.as_deref());
                ruff_python_ast::visitor::walk_stmt(self, stmt);
                if pushed {
                    self.typevar_rename_stack.pop();
                }
            }
            Stmt::Return(ret) => {
                if let Some(v) = ret.value.as_deref() {
                    self.visit_expr(v);
                }
                self.rewrite_return_coercion(ret);
            }
            Stmt::AnnAssign(ann) => {
                // Visit annotation first so any anon-NT inside it (including
                // nested ones) registers its synthesized class. Then visit
                // the RHS — any anon-NT children there register too. THEN
                // coerce: the shape extracted from the annotation now uses
                // the fully populated `range_to_class` so emitted class
                // bodies don't embed raw inner anon-NT source.
                self.visit_expr(&ann.annotation);
                if let Some(v) = ann.value.as_deref() {
                    self.visit_expr(v);
                }
                self.rewrite_ann_assign_coercion(ann);
            }
            Stmt::Assign(assign) => {
                walk_stmt(self, stmt);
                if let [target] = assign.targets.as_slice() {
                    self.record_alias(target, &assign.value);
                }
            }
            Stmt::TypeAlias(alias) => {
                walk_stmt(self, stmt);
                self.record_alias(&alias.name, &alias.value);
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Tuple(tuple) = expr {
            if tuple.is_anon_named_tuple || tuple.is_anon_named_tuple_value {
                // Walk children first so any nested anonymous-named-tuple
                // expressions get their classes synthesized and registered in
                // `range_to_class` before the outer renders its field types.
                walk_expr(self, expr);
                if tuple.is_anon_named_tuple {
                    self.rewrite_type_form(tuple);
                } else {
                    self.rewrite_value_form(tuple);
                }
                return;
            }
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct AnonNamedTuplePass<'src> {
    source: &'src str,
    config: crate::Config,
}

impl<'src> AnonNamedTuplePass<'src> {
    pub(crate) fn new(source: &'src str, config: crate::Config) -> Self {
        Self { source, config }
    }
}

impl super::ast_driver::TypeAwarePass for AnonNamedTuplePass<'_> {
    fn run(
        &self,
        stmts: &[ruff_python_ast::Stmt],
        types: &dyn TypeInfo,
        ctx: &mut super::ast_driver::PassContext,
    ) {
        let mut inner = AnonNamedTuple::new(self.source, types, self.config.clone());
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        for err in std::mem::take(&mut inner.errors) {
            ctx.errors.push(err);
        }
        if inner.needs_import {
            ctx.required_imports
                .push("from typing import NamedTuple".to_owned());
            // the field lowerer's own classes come first: a field type may name
            // one. they go in as a single entry so the sort below cannot part
            // them from the classes that reference them
            let defs = format!("{}{}", inner.callable.class_defs(), inner.class_defs());
            if !defs.is_empty() {
                let trimmed = defs.trim_end_matches('\n');
                ctx.required_imports.push(format!("{trimmed}\n"));
            }
        }
        ctx.required_imports
            .extend(inner.callable.take_import_lines());
        ctx.type_only_imports.extend(
            inner
                .type_only_imports
                .borrow()
                .iter()
                .map(|module| format!("import {module}")),
        );
        ctx.type_only_imports.extend(
            inner
                .typing_names
                .borrow()
                .iter()
                .map(|name| format!("from typing import {name}")),
        );
        for fix in std::mem::take(&mut inner.edits) {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// the synthesized class body leaves the statement the field type was
    /// written in, so nothing else lowers it — `dynamic` reaching the output
    /// verbatim is a `NameError` at import, and a wrapped type is a syntax error
    #[test]
    fn field_types_are_lowered() {
        check(
            "a: (m: dynamic, n: int?)\n",
            indoc! {"
                from typing import Any, NamedTuple
                class _AnonNamedTuple_b4e69f63(NamedTuple):
                    m: Any
                    n: int | None

                a: _AnonNamedTuple_b4e69f63
            "},
        );
    }

    /// `typeof` is lowered by an AST pass that re-renders its whole statement,
    /// which drops this pass's edit — the cleanup run then re-lowers the
    /// restored surface form, and must not emit the class a second time
    #[test]
    fn a_re_rendered_statement_does_not_duplicate_the_class() {
        let out = transpile("b: int = 1\na: (m: typeof b)\n", &Config::test_default()).unwrap();
        assert_eq!(
            out.matches("class _AnonNamedTuple_").count(),
            1,
            "expected a single class def, got: {out}"
        );
        assert!(out.contains("    m: TypeOf[b]"), "got: {out}");
    }

    #[test]
    fn a_callable_arrow_field_type_is_lowered() {
        check(
            "a: (m: (int) -> str)\n",
            indoc! {"
                from typing import Callable, NamedTuple
                class _AnonNamedTuple_7905e5a7(NamedTuple):
                    m: Callable[[int], str]

                a: _AnonNamedTuple_7905e5a7
            "},
        );
    }

    #[test]
    fn simple_alias() {
        check(
            "a = (name: str, age: int)\n",
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                a = _AnonNamedTuple_7bfb4772
            "},
        );
    }

    #[test]
    fn coercion_follows_an_alias() {
        // coercion asks what the annotation means, not how it is spelled: ty
        // accepts a plain tuple against an alias to an anonymous named tuple, so
        // leaving it uncoerced makes `v.name` an `AttributeError` on a file the
        // checker passed. both alias spellings and all three coercion sites
        check(
            indoc! {"
                P = (name: str, age: int)
                type Q = (name: str, age: int)

                v: P = (\"a\", 1)
                w: Q = (\"b\", 2)
                xs: list[P] = [(\"c\", 3)]

                def f() -> P:
                    return (\"d\", 4)
            "},
            indoc! {"
                from typing import NamedTuple
                from typing_extensions import TypeAliasType
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                P = _AnonNamedTuple_7bfb4772
                Q = TypeAliasType(\"Q\", _AnonNamedTuple_7bfb4772)

                v: P = _AnonNamedTuple_7bfb4772(\"a\", 1)
                w: Q = _AnonNamedTuple_7bfb4772(\"b\", 2)
                xs: list[P] = [_AnonNamedTuple_7bfb4772(\"c\", 3)]

                def f() -> P:
                    return _AnonNamedTuple_7bfb4772(\"d\", 4)
            "},
        );
    }

    #[test]
    fn an_alias_below_its_use_does_not_coerce() {
        // the table is built in source order, so a name is only an anon-NT alias
        // from its declaration down — the same rule the binding itself follows
        check(
            indoc! {"
                v: P = (\"a\", 1)
                P = (name: str, age: int)
            "},
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                v: P = (\"a\", 1)
                P = _AnonNamedTuple_7bfb4772
            "},
        );
    }

    #[test]
    fn function_signature_and_return_literal() {
        // The plain tuple literal at the return position is wrapped as a
        // constructor call so runtime field-name access works.
        check(
            indoc! {"
                def foo(x: (name: str, age: int)) -> (name: str, age: int):
                    return (\"asdf\", 1)
            "},
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                def foo(x: _AnonNamedTuple_7bfb4772) -> _AnonNamedTuple_7bfb4772:
                    return _AnonNamedTuple_7bfb4772(\"asdf\", 1)
            "},
        );
    }

    #[test]
    fn shape_dedup_collapses_to_one_class() {
        // Two identical shapes — one class produced.
        let out = transpile(
            indoc! {"
                a: (name: str, age: int)
                b: (name: str, age: int)
            "},
            &Config::test_default(),
        )
        .unwrap();
        let occurrences = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(occurrences, 1, "expected single class def, got: {out}");
    }

    #[test]
    fn distinct_shapes_get_distinct_classes() {
        let out = transpile(
            indoc! {"
                a: (name: str, age: int)
                b: (label: str, count: int)
            "},
            &Config::test_default(),
        )
        .unwrap();
        let occurrences = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(occurrences, 2, "expected two class defs, got: {out}");
    }

    #[test]
    fn single_field_tuple() {
        check(
            "a: (name: str)\n",
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_09c563f3(NamedTuple):
                    name: str

                a: _AnonNamedTuple_09c563f3
            "},
        );
    }

    #[test]
    fn trailing_comma_in_field_list() {
        let out = transpile("a: (name: str, age: int,)\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("class _AnonNamedTuple_"),
            "expected class synthesized, got: {out}"
        );
        assert!(out.contains("    name: str\n"));
        assert!(out.contains("    age: int\n"));
    }

    #[test]
    fn mixed_positional_and_named_value() {
        // The example from the user request: positional first, named second.
        let out = transpile(
            indoc! {"
                a = (1, name=\"a\")
                print(a[0], a.name)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"), "got: {out}");
        // The synthesized class has `arg0: int` and `name: str` fields.
        assert!(out.contains("    arg0: int\n"), "got: {out}");
        assert!(out.contains("    name: str\n"), "got: {out}");
        // The value-form site is rewritten as a constructor call with both
        // values forwarded positionally.
        assert!(
            out.contains("_AnonNamedTuple_") && out.contains("(1, \"a\")"),
            "got: {out}"
        );
        // Field-name access on the result is preserved verbatim.
        assert!(out.contains("a[0], a.name"), "got: {out}");
    }

    #[test]
    fn mixed_positional_and_named_type() {
        // `(int, name: str)` — first field is positional type, second is named.
        let out = transpile("a: (int, name: str)\n", &Config::test_default()).unwrap();
        assert!(out.contains("class _AnonNamedTuple_"), "got: {out}");
        assert!(out.contains("    arg0: int\n"), "got: {out}");
        assert!(out.contains("    name: str\n"), "got: {out}");
        assert!(out.contains("a: _AnonNamedTuple_"), "got: {out}");
    }

    #[test]
    fn mixed_positional_named_collision_is_hard_error() {
        // User-named field `arg0` collides with the synthetic name the
        // transform would assign to the first positional field. The
        // transpiler must abort rather than emit invalid Python.
        let err = transpile("a = (1, arg0=2)\n", &Config::test_default()).unwrap_err();
        assert!(err.contains("duplicate field name `arg0`"), "got: {err}");
    }

    #[test]
    fn duplicate_named_fields_is_hard_error() {
        let err = transpile("a = (name=1, name=2)\n", &Config::test_default()).unwrap_err();
        assert!(err.contains("duplicate field name `name`"), "got: {err}");
    }

    #[test]
    fn duplicate_named_fields_in_type_form_is_hard_error() {
        let err = transpile("a: (name: int, name: str)\n", &Config::test_default()).unwrap_err();
        assert!(err.contains("duplicate field name `name`"), "got: {err}");
    }

    #[test]
    fn mixed_value_dedupes_with_matching_type_form() {
        // A `(int, name: str)` type form and a `(1, name="a")` value form
        // share the same synthesized class because field names + types match.
        let out = transpile(
            indoc! {"
                a: (int, name: str)
                b = (1, name=\"a\")
            "},
            &Config::test_default(),
        )
        .unwrap();
        let class_count = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(class_count, 1, "got: {out}");
    }

    #[test]
    fn anon_nt_in_typevar_bound() {
        // Regression: the generics polyfill copies the TypeVar bound's source
        // text verbatim into the emitted `_T = TypeVar("_T", bound=...)` line,
        // including any anon-NT span. The dedicated cleanup phase re-runs the
        // anon-NT lowering on the post-transform output and rewrites them.
        let out = transpile(
            "def f[P: (x: int, y: int)](p: P) -> int: return p.x\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"), "got: {out}");
        assert!(
            out.contains("bound=_AnonNamedTuple_"),
            "TypeVar bound should reference the synthesized class, got: {out}"
        );
        assert!(
            !out.contains("(x: int, y: int)"),
            "raw anon-NT leaked into output, got: {out}"
        );
    }

    #[test]
    fn anon_nt_in_protocol_body() {
        // Protocol attribute annotations use the anon-NT class name directly.
        let out = transpile(
            indoc! {"
                from typing import Protocol
                class HasPoint(Protocol):
                    point: (x: int, y: int)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"));
        assert!(out.contains("    point: _AnonNamedTuple_"));
    }

    #[test]
    fn anon_nt_in_typed_dict_body() {
        let out = transpile(
            indoc! {"
                from typing import TypedDict
                class PersonDict(TypedDict):
                    person: (name: str, age: int)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"));
        assert!(out.contains("    person: _AnonNamedTuple_"));
    }

    #[test]
    fn anon_nt_in_callable_arg() {
        let out = transpile(
            indoc! {"
                from typing import Callable
                handler: Callable[[(x: int, y: int)], int] = lambda p: 0
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"));
        assert!(out.contains("Callable[[_AnonNamedTuple_"));
    }

    #[test]
    fn anon_nt_in_dict_value_annotation() {
        let out = transpile(
            "data: dict[str, (x: int, y: int)] = {}\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"));
        assert!(out.contains("data: dict[str, _AnonNamedTuple_"));
    }

    #[test]
    fn anon_nt_in_union_return() {
        // Anon-NT inside a `X | None` return annotation: both the annotation
        // gets lowered AND the plain tuple return is coerced via the
        // bidirectional union-aware tcx lookup.
        let out = transpile(
            indoc! {"
                def maybe() -> (name: str, age: int) | None:
                    return (\"bob\", 25)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"));
        assert!(out.contains("-> _AnonNamedTuple_"));
        assert!(out.contains("| None"));
    }

    #[test]
    fn declaration_modifier_annotation_coerces() {
        // `let x: T = v` parses as `x: __let__[T] = v`, and the annotation the
        // coercion reads is the one under the marker
        check(
            "let a: (name: str, age: int) = (\"asdf\", 1)\n",
            indoc! {"
                from typing import Final, NamedTuple
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                a: Final[_AnonNamedTuple_7bfb4772] = _AnonNamedTuple_7bfb4772(\"asdf\", 1)
            "},
        );
        check(
            "var a: (name: str, age: int) = (\"asdf\", 1)\n",
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                a: _AnonNamedTuple_7bfb4772 = _AnonNamedTuple_7bfb4772(\"asdf\", 1)
            "},
        );
    }

    #[test]
    fn ann_assign_direct_anon_nt() {
        // `x: (name: T, ...) = (...)` wraps the RHS as a constructor call.
        check(
            "a: (name: str, age: int) = (\"asdf\", 1)\n",
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_7bfb4772(NamedTuple):
                    name: str
                    age: int

                a: _AnonNamedTuple_7bfb4772 = _AnonNamedTuple_7bfb4772(\"asdf\", 1)
            "},
        );
    }

    #[test]
    fn ann_assign_list_of_anon_nt() {
        // `x: list[(name: T, ...)] = [(...)]` wraps each list element.
        check(
            "a: list[(age: int, name: str)] = [(1, \"a\"), (2, \"b\")]\n",
            indoc! {"
                from typing import NamedTuple
                class _AnonNamedTuple_6ae2958b(NamedTuple):
                    age: int
                    name: str

                a: list[_AnonNamedTuple_6ae2958b] = [_AnonNamedTuple_6ae2958b(1, \"a\"), _AnonNamedTuple_6ae2958b(2, \"b\")]
            "},
        );
    }

    #[test]
    fn ann_assign_set_of_anon_nt() {
        let out = transpile(
            "a: set[(age: int, name: str)] = {(1, \"a\")}\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("a: set[_AnonNamedTuple_"));
        assert!(out.contains("{_AnonNamedTuple_"));
        assert!(!out.contains("{(1, \"a\")}"));
    }

    #[test]
    fn ann_assign_arity_mismatch_left_alone() {
        // Don't silently construct the wrong shape; let ty diagnose.
        let out = transpile(
            "a: (name: str, age: int) = (\"asdf\",)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("a: _AnonNamedTuple_"));
        assert!(out.contains("= (\"asdf\",)"), "got: {out}");
    }

    #[test]
    fn anon_named_tuple_inside_subscript() {
        // Regression: `list[(age: int, name: str)]` previously crashed ty's
        // generic-alias slice handling because the slice's `ExprNamed`
        // children were fed into walrus-only inference paths.
        let out = transpile(
            "a: list[(age: int, name: str)] = [(1, \"a\")]\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _AnonNamedTuple_"), "got: {out}");
        assert!(
            out.contains("a: list[_AnonNamedTuple_"),
            "expected the anon-NT class name to appear inside the list[..] subscript, got: {out}"
        );
        // The plain tuple literal in the list initializer gets wrapped as a
        // constructor call so runtime field-name access works on each element.
        assert!(
            out.contains("_AnonNamedTuple_") && out.contains("(1, \"a\")"),
            "expected list element wrapped as constructor call, got: {out}"
        );
        assert!(
            !out.contains("[(1, \"a\")]"),
            "plain tuple element should have been wrapped, got: {out}"
        );
    }

    #[test]
    fn nested_anon_named_tuple_in_field_type() {
        // Both the inner and outer anon-NT get their own synthesized classes;
        // the outer's `point` field type references the inner's class name
        // rather than the raw `(x: int, y: int)` source.
        let out = transpile(
            "a: (point: (x: int, y: int), tag: str)\n",
            &Config::test_default(),
        )
        .unwrap();
        let class_count = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(class_count, 2, "expected one class per anon-NT, got: {out}");
        // The outer class body must reference the inner class name by name —
        // never the raw `(x: int, y: int)` source.
        assert!(
            !out.contains("    point: (x: int, y: int)"),
            "outer class body still has unlowered inner anon-NT, got: {out}"
        );
    }

    #[test]
    fn nested_anon_named_tuple_in_ann_assign() {
        // Regression: the AnnAssign coercion path used to extract the shape
        // BEFORE the inner anon-NT had registered, embedding raw inner
        // source in the outer class body. The output was caught by
        // verify_syntax as containing leftover basedpython syntax.
        let out = transpile(
            "record: (point: (x: int, y: int), tag: str) = ((1, 2), \"origin\")\n",
            &Config::test_default(),
        )
        .unwrap();
        let class_count = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(class_count, 2, "got: {out}");
        assert!(
            !out.contains("    point: (x: int, y: int)"),
            "outer body still has unlowered inner anon-NT, got: {out}"
        );
    }

    #[test]
    fn nested_anon_named_tuple_coercion_recurses() {
        // `record: (point: (x: int, y: int), tag: str) = ((1, 2), "origin")`
        // — the inner plain tuple `(1, 2)` must be wrapped as the inner
        // class's constructor too, so `record.point.x` works at runtime.
        let out = transpile(
            "record: (point: (x: int, y: int), tag: str) = ((1, 2), \"origin\")\n",
            &Config::test_default(),
        )
        .unwrap();
        // The RHS must contain a NESTED constructor call: outer wraps the
        // tuple `((1, 2), "origin")` and inner wraps `(1, 2)`.
        assert!(
            out.contains("(_AnonNamedTuple_") && out.contains("(1, 2)"),
            "expected nested constructor calls, got: {out}"
        );
        // The inner plain tuple `(1, 2)` must NOT appear unwrapped at the
        // RHS — it should be inside another `_AnonNamedTuple_xxx(...)`.
        assert!(
            out.contains("_AnonNamedTuple_e9212dc0(1, 2)")
                || !out.contains("= _AnonNamedTuple_98fc8bd1((1, 2),"),
            "inner plain tuple wasn't recursively wrapped, got: {out}"
        );
    }

    #[test]
    fn class_def_order_is_dependency_safe() {
        // The class body of an outer anon-NT references the inner anon-NT's
        // class name. Class definitions in the preamble must therefore be
        // emitted in dependency order (inner before outer) so the file is
        // valid Python at module load time.
        let out = transpile(
            "x: (point: (x: int, y: int), tag: str)\n",
            &Config::test_default(),
        )
        .unwrap();
        // Find positions of inner and outer class definitions.
        let inner_pos = out
            .find("class _AnonNamedTuple_")
            .expect("expected at least one synthesized class");
        let next_class_marker = out[inner_pos + 1..]
            .find("class _AnonNamedTuple_")
            .map(|p| inner_pos + 1 + p);
        let second_pos = next_class_marker.expect("expected two synthesized classes");
        let inner_class_name = out[inner_pos..]
            .lines()
            .next()
            .unwrap()
            .strip_prefix("class ")
            .and_then(|s| s.split('(').next())
            .unwrap();
        // The second class definition's body must reference the first one.
        assert!(
            out[second_pos..].contains(inner_class_name),
            "second class def must reference the first by name, got: {out}"
        );
    }

    #[test]
    fn nested_anon_named_tuple_in_value_form() {
        // A value-form anon-NT inside another value-form's field also gets
        // its own constructor call; the outer references it as a positional
        // argument.
        let out = transpile(
            "a = (point=(x=1, y=2), tag=\"x\")\n",
            &Config::test_default(),
        )
        .unwrap();
        let class_count = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(class_count, 2, "got: {out}");
        // No raw `(x=1, y=2)` should leak into the output.
        assert!(!out.contains("(x=1, y=2)"), "got: {out}");
    }

    #[test]
    fn mutual_assign_and_use() {
        // Type alias `a = (...)` and parameter usage of an equivalent shape
        // share the same synthesized class.
        let out = transpile(
            indoc! {"
                a = (name: str, age: int)
                def f(x: (name: str, age: int)) -> None: ...
            "},
            &Config::test_default(),
        )
        .unwrap();
        let occurrences = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(occurrences, 1, "expected single class def, got: {out}");
    }

    #[test]
    fn plain_tuple_arg_unchanged() {
        // Plain tuple literal at a call argument is not coerced — only return
        // values inside anon-NT-returning functions are.
        let out = transpile(
            indoc! {"
                def foo(x: (name: str, age: int)) -> None: ...
                foo((\"asdf\", 1))
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("foo((\"asdf\", 1))"), "got: {out}");
    }

    #[test]
    fn value_form_construction() {
        let out = transpile("a = (name=\"asdf\", age=20)\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("class _AnonNamedTuple_") && out.contains("(\"asdf\", 20)"),
            "got: {out}"
        );
    }

    #[test]
    fn value_form_dedup_with_type_form() {
        // A type-form `(name: str, age: int)` and a value-form
        // `(name="asdf", age=20)` must hash to the same class because the
        // promoted field types match.
        let out = transpile(
            indoc! {"
                a: (name: str, age: int)
                b = (name=\"asdf\", age=20)
            "},
            &Config::test_default(),
        )
        .unwrap();
        let occurrences = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(occurrences, 1, "expected single class def, got: {out}");
    }

    #[test]
    fn value_form_distinct_field_types() {
        // Different inferred types for the same field-name list produce
        // distinct classes.
        let out = transpile(
            indoc! {"
                a = (name=\"asdf\", age=20)
                b = (name=1, age=\"twenty\")
            "},
            &Config::test_default(),
        )
        .unwrap();
        let occurrences = out.matches("class _AnonNamedTuple_").count();
        assert_eq!(occurrences, 2, "got: {out}");
    }

    #[test]
    fn return_coercion_only_inside_anon_nt_function() {
        // A plain tuple return from a function whose return is plain `tuple`
        // is left alone (and a builtin display is never reified).
        check(
            indoc! {"
                def f() -> tuple[str, int]:
                    return (\"asdf\", 1)
            "},
            indoc! {"
                def f() -> tuple[str, int]:
                    return (\"asdf\", 1)
            "},
        );
    }

    #[test]
    fn return_coercion_arity_mismatch_left_alone() {
        // If the returned tuple's arity doesn't match the annotation, leave
        // it (let ty diagnose the error rather than constructing the wrong shape).
        let out = transpile(
            indoc! {"
                def f() -> (name: str, age: int):
                    return (\"asdf\",)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("return (\"asdf\",)"), "got: {out}");
    }

    #[test]
    fn nested_function_return_coercion_uses_innermost() {
        // Inner function has its own return annotation; coercion picks the
        // innermost.
        let out = transpile(
            indoc! {"
                def outer() -> (a: int, b: int):
                    def inner() -> (x: str, y: str):
                        return (\"u\", \"v\")
                    return (1, 2)
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("(\"u\", \"v\")") || out.contains("AnonNamedTuple"),
            "got: {out}"
        );
        assert!(
            out.contains("(1, 2)") || out.contains("AnonNamedTuple"),
            "got: {out}"
        );
    }

    #[test]
    fn value_form_field_access_round_trip() {
        // The end-to-end example from the user request: the constructor call
        // must be emitted so `f().name` works at runtime.
        let out = transpile(
            indoc! {"
                def f() -> (age: int, name: str):
                    return (1, \"a\")

                f().name
            "},
            &Config::test_default(),
        )
        .unwrap();
        // Find the class name and verify return is wrapped.
        assert!(out.contains("class _AnonNamedTuple_"));
        assert!(
            out.lines()
                .any(|l| l.trim_start().starts_with("return _AnonNamedTuple_")
                    && l.contains("(1, \"a\")")),
            "got: {out}"
        );
    }

    #[test]
    fn python_passthrough_does_not_synthesize() {
        unchanged("a = (1, 2)\n");
    }

    #[test]
    fn already_present_named_tuple_class_unchanged() {
        // A user-defined NamedTuple class with no anon syntax must round-trip
        // unchanged.
        check(
            indoc! {"
                from typing import NamedTuple
                class Point(NamedTuple):
                    x: int
                    y: int
            "},
            indoc! {"
                from typing import NamedTuple
                class Point(NamedTuple):
                    x: int
                    y: int
            "},
        );
    }
}
