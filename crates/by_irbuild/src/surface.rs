//! the basedpython surface forms this backend has no lowering for
//!
//! the compiler lowers the *basedpython* ast. nothing between the parse and here
//! rewrites it — the transpiler's passes produce the interpreted twin's python
//! and never touch the tree the lowering walks — so every marker the parser set
//! for a basedpython-only form is still on the node when a body is lowered. a
//! lowering that reads such a node by its plain-python meaning answers a
//! different program, and answers it silently:
//!
//! - `item?.value` is not `item.value`. the whole of the form is the `None`
//!   guard, and reading the attribute unconditionally raises `AttributeError`
//!   where the source asked for `None`
//! - `(name="ada", age=36)` is not the tuple `("ada", 36)`. its fields are read
//!   by name, and a plain tuple has none
//! - `def area(Rect(w, h): Rect)` binds `w` and `h` from the pattern. dropping
//!   the pattern leaves the body reading two names nothing bound
//!
//! so every marker the parser can set is decided here, in one place. a form the
//! lowering understands is named and allowed with the reason it is safe; every
//! other one declines, and the function runs from its interpreted definition —
//! which is the twin the transpiler *did* lower, and is therefore right.
//!
//! this is deliberately a decision table rather than a check spread through the
//! lowering: a marker added to the ast is meant to be answered here before it
//! can reach a body.
//!
//! **type expressions are not scanned.** an annotation is read through ty rather
//! than lowered, so a basedpython type form in one — `typeof x`, `(a: int, b: str)`,
//! `int.() -> str`, `some T` — has already been resolved to a type by the time the
//! lowering asks, and there is nothing left of the surface syntax to misread

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_parameter, walk_stmt, walk_with_item};
use ruff_python_ast::{self as ast, Expr, Parameter, PySourceType, Stmt, TypeParam, WithItem};
use ruff_python_stdlib::identifiers::is_identifier;
use ty_python_semantic::reified::{reified_class_reads, reified_type_param_names};

use crate::mapper::{Decline, Lowered};

/// whether this function's body may be lowered at all
pub(crate) fn gate_function(
    source_type: PySourceType,
    function: &ast::StmtFunctionDef,
) -> Lowered<()> {
    // a reified type parameter is a runtime value the *specialization step* supplies,
    // and the step is the `[int]` subscript on the function object. an emitted
    // function is not subscriptable and carries no closure to rebuild, so the
    // parameter is not merely missing — the body reads its name as a module global.
    // reification is inferred from the body as well as declared, so the marker on the
    // type parameter is not enough to find it
    if let Some(first) = reified_type_param_names(source_type, function).first() {
        return Err(Decline::new(format!(
            "`{first}` is a reified type parameter, and a specialization has no emitted function to rebuild"
        )));
    }
    let mut scanner = Scanner { found: None };
    scanner.walk_function(function);
    scanner.into_result()
}

/// whether this class may be lowered at all
pub(crate) fn gate_class(source_type: PySourceType, class: &ast::StmtClassDef) -> Lowered<()> {
    // a reified *class* parameter is read off the instance, which means the
    // specialization `Box[int]` has to build a memoized subclass carrying the type
    // argument. an emitted class refuses to be a base, so there is nowhere to put one
    if let Some(first) = reified_class_reads(source_type, class).names.first() {
        return Err(Decline::new(format!(
            "`{first}` is a reified type parameter, and an emitted class cannot carry the type argument a specialization builds"
        )));
    }
    let mut scanner = Scanner { found: None };
    scanner.walk_class(class);
    scanner.into_result()
}

struct Scanner {
    /// the first form with no lowering, and why it has none
    found: Option<String>,
}

impl Scanner {
    fn into_result(self) -> Lowered<()> {
        match self.found {
            Some(reason) => Err(Decline::new(reason)),
            None => Ok(()),
        }
    }

    fn refuse(&mut self, reason: impl Into<String>) {
        if self.found.is_none() {
            self.found = Some(reason.into());
        }
    }

    /// the parts of a `def` the walker reaches — `walk_stmt`'s `FunctionDef` arm,
    /// reached without a `Stmt` to hold the node. the return annotation is left out
    /// for the reason the module docs give
    fn walk_function(&mut self, function: &ast::StmtFunctionDef) {
        for decorator in &function.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(type_params) = function.type_params.as_deref() {
            self.visit_type_params(type_params);
        }
        self.visit_parameters(&function.parameters);
        self.visit_body(&function.body);
    }

    /// the same for a `class`, whose body carries its methods
    fn walk_class(&mut self, class: &ast::StmtClassDef) {
        for decorator in &class.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(type_params) = class.type_params.as_deref() {
            self.visit_type_params(type_params);
        }
        if let Some(arguments) = class.arguments.as_deref() {
            self.visit_arguments(arguments);
        }
        self.visit_body(&class.body);
    }
}

impl<'a> Visitor<'a> for Scanner {
    /// a type expression is resolved by ty, never lowered — see the module docs
    fn visit_annotation(&mut self, _expr: &'a Expr) {}

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if self.found.is_some() {
            return;
        }
        match stmt {
            // `if let P := subject:` matches a pattern and binds its captures. read as
            // an ordinary `if`, the subject becomes the condition and every capture the
            // body reads is an unbound name
            Stmt::If(node) if node.pattern.is_some() => {
                self.refuse("an `if let` pattern binds the names its body reads");
            }
            // `for P in xs:` — the same, once per iteration
            Stmt::For(node) if node.pattern.is_some() => {
                self.refuse("a `for` destructuring pattern binds the names its body reads");
            }
            // `break <value>` yields out of a loop being used as an expression, which
            // is the statement-expression form and is declined with it
            Stmt::Break(node) if node.value.is_some() => {
                self.refuse(
                    "a `break` that yields a value belongs to a loop used as an expression",
                );
            }
            // a decorator on a binding replaces what the name holds
            Stmt::Assign(node) if !node.decorator_list.is_empty() => {
                self.refuse("a decorator on a binding replaces the value the name holds");
            }
            Stmt::AnnAssign(node) if !node.decorator_list.is_empty() => {
                self.refuse("a decorator on a binding replaces the value the name holds");
            }
            // `context x: T = …` declares a value later calls are filled from
            Stmt::AnnAssign(node) if node.is_context => {
                self.refuse("a `context` declaration is read by the call sites it fills");
            }
            // `from x export y` re-exports as well as binds
            Stmt::ImportFrom(node) if node.is_export => {
                self.refuse(
                    "`from … export …` binds and re-exports, and only the module body does that",
                );
            }
            // a trailing-lambda block is an *argument* to the call above it, not a
            // definition standing in the body
            Stmt::FunctionDef(node) if node.is_trailing_lambda => {
                self.refuse("a trailing-lambda block is an argument to the call it follows");
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if self.found.is_some() {
            return;
        }
        match expr {
            // `a?.b` answers `None` for a `None` receiver. reading the attribute
            // unconditionally raises where the source asked for `None`
            Expr::Attribute(node) if node.optional => {
                self.refuse(
                    "`?.` answers `None` for a `None` receiver, and nothing here guards it",
                );
            }
            // `p.1` indexes a tuple by position. the name is a number, so the dynamic
            // attribute read it lowers to can never find it
            Expr::Attribute(node) if !is_identifier(node.attr.as_str()) => {
                self.refuse("a positional tuple index is not an attribute name");
            }
            // `v cast! T` / `v cast? T` are checks, not calls
            Expr::Call(node) if node.cast_kind.is_some() => {
                self.refuse("a checked cast tests its value rather than calling it");
            }
            // `tag"…"` passes the literal to `tag` as a template
            Expr::Call(node) if node.is_string_tag => {
                self.refuse("a custom string tag is called with a template, not with the string");
            }
            // `(name="ada", age=36)` is read by field name
            Expr::Tuple(node) if node.is_anon_named_tuple_value => {
                self.refuse("an anonymous named tuple is read by field name, and a tuple has none");
            }
            // the remaining tuple markers are type forms, and a type expression is
            // never lowered — one reaching a value position is a form with no meaning
            // here at all
            Expr::Tuple(node)
                if node.is_anon_named_tuple
                    || node.is_parameter_shape
                    || node.callable_shape.is_some() =>
            {
                self.refuse("a callable-parameter or named-tuple type stands in a value position");
            }
            Expr::Subscript(node) if node.is_typeof || node.is_type_decoration => {
                self.refuse("a `typeof` or decorated type stands in a value position");
            }
            // the postfix `?` / `!` / `^` operators and the `??` / `?` infix ones are
            // declined by the unary and binary lowerings themselves, which name the
            // operator they refused
            _ => {}
        }
        walk_expr(self, expr);
    }

    fn visit_parameter(&mut self, parameter: &'a Parameter) {
        if self.found.is_some() {
            return;
        }
        // `def area(Rect(w, h): Rect)` binds `w` and `h` from the pattern. the
        // parameter itself is a synthetic name the body never reads
        if parameter.pattern.is_some() {
            self.refuse("a destructuring parameter binds the names the body reads");
        }
        // `context b: str` is filled from the call site, and a call with no argument
        // for it is declined there; `some T` is a type-only hole
        walk_parameter(self, parameter);
    }

    fn visit_with_item(&mut self, with_item: &'a WithItem) {
        if self.found.is_some() {
            return;
        }
        if with_item.pattern.is_some() {
            self.refuse("a destructuring `with` item binds the names the body reads");
        }
        walk_with_item(self, with_item);
    }

    fn visit_type_param(&mut self, type_param: &'a TypeParam) {
        if self.found.is_some() {
            return;
        }
        // a declared `reified` parameter is reified whether or not the body reads it.
        // an inferred one is found by the queries above, which see the whole body
        let declared = match type_param {
            TypeParam::TypeVar(node) => node.is_reified,
            TypeParam::TypeVarTuple(node) => node.is_reified,
            TypeParam::ParamSpec(node) => node.is_reified,
        };
        if declared {
            self.refuse("a `reified` type parameter is supplied by a specialization step");
        }
        // and no walk: every other part of a type parameter — a bound, a bound range, a
        // type mapping, a variance keyword, a `some` hole, a pep 696 default — is a type
        // expression ty reads and nothing the body runs, so it is left alone for the
        // reason the module docs give for an annotation
    }
}
