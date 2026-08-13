use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::helpers::{has_written_def_header, is_immutable_scalar_default};
use ruff_python_ast::types::Node;
use ruff_python_ast::visitor;
use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{self as ast, AnyParameterRef, Comprehension, Expr, ExprContext, Stmt};
use ruff_text_size::Ranged;

use crate::Violation;
use crate::checkers::ast::Checker;

/// ## What it does
/// Checks for function definitions that use a loop variable.
///
/// ## Why is this bad?
/// The loop variable is not bound in the function definition, so it will always
/// have the value it had in the last iteration when the function is called.
///
/// Instead, consider using a default argument to bind the loop variable at
/// function definition time. Or, use `functools.partial`.
///
/// ## Example
/// ```python
/// adders = [lambda x: x + i for i in range(3)]
/// values = [adder(1) for adder in adders]  # [3, 3, 3]
/// ```
///
/// Use instead:
/// ```python
/// adders = [lambda x, i=i: x + i for i in range(3)]
/// values = [adder(1) for adder in adders]  # [1, 2, 3]
/// ```
///
/// Or:
/// ```python
/// from functools import partial
///
/// adders = [partial(lambda x, i: x + i, i=i) for i in range(3)]
/// values = [adder(1) for adder in adders]  # [1, 2, 3]
/// ```
///
/// ## References
/// - [The Hitchhiker's Guide to Python: Late Binding Closures](https://docs.python-guide.org/writing/gotchas/#late-binding-closures)
/// - [Python documentation: `functools.partial`](https://docs.python.org/3/library/functools.html#functools.partial)
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "v0.0.139")]
pub(crate) struct FunctionUsesLoopVariable {
    name: String,
}

impl Violation for FunctionUsesLoopVariable {
    #[derive_message_formats]
    fn message(&self) -> String {
        let FunctionUsesLoopVariable { name } = self;
        format!("Function definition does not bind loop variable `{name}`")
    }
}

#[derive(Default)]
struct LoadedNamesVisitor<'a> {
    loaded: Vec<&'a ast::ExprName>,
    stored: Vec<&'a ast::ExprName>,
}

/// `Visitor` to collect all used identifiers in a statement.
impl<'a> Visitor<'a> for LoadedNamesVisitor<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => match &name.ctx {
                ExprContext::Load => self.loaded.push(name),
                ExprContext::Store => self.stored.push(name),
                _ => {}
            },
            _ => visitor::walk_expr(self, expr),
        }
    }
}

/// The kind of closure a suspicious name was read inside. basedpython binds
/// each of them differently, so what it leaves unbound — the report this rule
/// keeps making — depends on which one read the name
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadBy {
    /// applied to the loop's values through a wrapper, wherever it sits
    Lambda,
    /// rebuilt with fresh closure cells, which only reaches a binding python
    /// compiled *as* a cell — never a module-level target
    WrittenFunctionDef,
    /// a `def` the parser synthesized for another construct, which the
    /// lowering leaves to the pass that owns it — never bound
    SynthesizedFunctionDef,
}

struct SuspiciousName<'a> {
    name: &'a ast::ExprName,
    read_by: ReadBy,
}

struct SuspiciousVariablesVisitor<'a> {
    source: &'a str,
    /// basedpython relocates a non-scalar parameter default into the body, so
    /// the reads in one have to be collected alongside the body's own
    relocates_defaults: bool,
    names: Vec<SuspiciousName<'a>>,
    safe_functions: Vec<&'a Expr>,
}

impl<'a> SuspiciousVariablesVisitor<'a> {
    fn new(source: &'a str, relocates_defaults: bool) -> Self {
        Self {
            source,
            relocates_defaults,
            names: Vec::new(),
            safe_functions: Vec::new(),
        }
    }
}

/// `Visitor` to collect all suspicious variables (those referenced in
/// functions, but not bound as arguments).
impl<'a> Visitor<'a> for SuspiciousVariablesVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(
                function @ ast::StmtFunctionDef {
                    parameters,
                    body,
                    is_trailing_lambda,
                    ..
                },
            ) => {
                // basedpython: a trailing-lambda block (`f:` + suite) lowers to a
                // closure, but whether it can outlive the loop depends on the
                // callee's `local` / `once` marker — type information this
                // syntactic rule cannot resolve. The type-aware ty lint
                // `escaping-loop-variable` handles it precisely (suppressing the
                // safe `local` / `once` case and flagging the rest); skip here so
                // this rule does not produce a false positive on the common
                // synchronous case.
                if *is_trailing_lambda {
                    return;
                }

                // Collect all loaded variable names.
                let mut visitor = LoadedNamesVisitor::default();
                visitor.visit_body(body);

                // basedpython re-evaluates every non-scalar default per call by
                // moving it into the body, so a name one reads is read where the
                // body runs — after the loop has moved on — exactly like a body
                // read. A scalar default stays in the signature and keeps
                // python's eager binding, which is what makes `value=i` the
                // documented workaround rather than a trap.
                if self.relocates_defaults {
                    for default in parameters
                        .iter()
                        .filter_map(AnyParameterRef::default)
                        .filter(|default| !is_immutable_scalar_default(default))
                    {
                        visitor.visit_expr(default);
                    }
                }

                let read_by = if has_written_def_header(self.source, function) {
                    ReadBy::WrittenFunctionDef
                } else {
                    ReadBy::SynthesizedFunctionDef
                };

                // Treat any non-arguments as "suspicious".
                self.names.extend(
                    visitor
                        .loaded
                        .into_iter()
                        .filter(|loaded| {
                            if visitor.stored.iter().any(|stored| stored.id == loaded.id) {
                                return false;
                            }

                            if parameters.includes(&loaded.id) {
                                return false;
                            }

                            true
                        })
                        .map(|name| SuspiciousName { name, read_by }),
                );

                return;
            }
            // Mark `return lambda: x` as safe.
            Stmt::Return(ast::StmtReturn {
                value: Some(value),
                range: _,
                node_index: _,
            }) if value.is_lambda_expr() => {
                self.safe_functions.push(value);
            }
            _ => {}
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Call(ast::ExprCall {
                func,
                arguments,
                range_start: _,
                node_index: _,
                is_cast: _,
                is_checked_cast: _,
                is_string_tag: _,
            }) => {
                // Mark immediately-invoked lambdas as safe — the closure
                // is consumed right away, so late-binding is not a concern.
                if func.is_lambda_expr() {
                    self.safe_functions.push(func);
                }

                match func.as_ref() {
                    Expr::Name(ast::ExprName { id, .. }) => {
                        if matches!(id.as_str(), "filter" | "reduce" | "map") {
                            for arg in &*arguments.args {
                                if arg.is_lambda_expr() {
                                    self.safe_functions.push(arg);
                                }
                            }
                        }
                    }
                    Expr::Attribute(ast::ExprAttribute { value, attr, .. }) if attr == "reduce" => {
                        if let Expr::Name(ast::ExprName { id, .. }) = value.as_ref() {
                            if id == "functools" {
                                for arg in &*arguments.args {
                                    if arg.is_lambda_expr() {
                                        self.safe_functions.push(arg);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }

                for keyword in &*arguments.keywords {
                    if keyword.arg.as_ref().is_some_and(|arg| arg == "key")
                        && keyword.value.is_lambda_expr()
                    {
                        self.safe_functions.push(&keyword.value);
                    }
                }
            }
            Expr::Lambda(ast::ExprLambda {
                parameters,
                returns: _,
                body,
                range: _,
                node_index: _,
            }) if !self.safe_functions.contains(&expr) => {
                // Collect all loaded variable names.
                let mut visitor = LoadedNamesVisitor::default();
                visitor.visit_expr(body);

                // Treat any non-arguments as "suspicious".
                self.names.extend(
                    visitor
                        .loaded
                        .into_iter()
                        .filter(|loaded| {
                            if visitor.stored.iter().any(|stored| stored.id == loaded.id) {
                                return false;
                            }

                            if parameters
                                .as_ref()
                                .is_some_and(|parameters| parameters.includes(&loaded.id))
                            {
                                return false;
                            }

                            true
                        })
                        .map(|name| SuspiciousName {
                            name,
                            read_by: ReadBy::Lambda,
                        }),
                );

                return;
            }
            _ => {}
        }
        visitor::walk_expr(self, expr);
    }
}

#[derive(Default)]
struct NamesFromAssignmentsVisitor<'a> {
    names: Vec<&'a str>,
}

/// `Visitor` to collect all names used in an assignment expression.
impl<'a> Visitor<'a> for NamesFromAssignmentsVisitor<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(ast::ExprName { id, .. }) => {
                self.names.push(id.as_str());
            }
            Expr::Starred(ast::ExprStarred { value, .. }) => {
                self.visit_expr(value);
            }
            Expr::List(ast::ExprList { elts, .. }) | Expr::Tuple(ast::ExprTuple { elts, .. }) => {
                for expr in elts {
                    self.visit_expr(expr);
                }
            }
            _ => {}
        }
    }
}

/// `Visitor` to collect the names a loop or comprehension *target* binds —
/// the subset of the assigned names basedpython gives each iteration its own
/// binding for. Mirrors [`AssignedNamesVisitor`]'s traversal, so the two agree
/// on which loops belong to this one.
#[derive(Default)]
struct IterationTargetsVisitor<'a> {
    names: Vec<&'a str>,
}

impl<'a> Visitor<'a> for IterationTargetsVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if stmt.is_function_def_stmt() {
            // Don't recurse.
            return;
        }

        if let Stmt::For(ast::StmtFor { target, .. }) = stmt {
            let mut visitor = NamesFromAssignmentsVisitor::default();
            visitor.visit_expr(target);
            self.names.extend(visitor.names);
        }

        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if expr.is_lambda_expr() {
            // Don't recurse.
            return;
        }

        visitor::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, comprehension: &'a Comprehension) {
        let mut visitor = NamesFromAssignmentsVisitor::default();
        visitor.visit_expr(&comprehension.target);
        self.names.extend(visitor.names);

        visitor::walk_comprehension(self, comprehension);
    }
}

#[derive(Default)]
struct AssignedNamesVisitor<'a> {
    names: Vec<&'a str>,
}

/// `Visitor` to collect all used identifiers in a statement.
impl<'a> Visitor<'a> for AssignedNamesVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if stmt.is_function_def_stmt() {
            // Don't recurse.
            return;
        }

        match stmt {
            Stmt::Assign(ast::StmtAssign { targets, .. }) => {
                let mut visitor = NamesFromAssignmentsVisitor::default();
                for expr in targets {
                    visitor.visit_expr(expr);
                }
                self.names.extend(visitor.names);
            }
            Stmt::AugAssign(ast::StmtAugAssign { target, .. })
            | Stmt::AnnAssign(ast::StmtAnnAssign { target, .. })
            | Stmt::For(ast::StmtFor { target, .. }) => {
                let mut visitor = NamesFromAssignmentsVisitor::default();
                visitor.visit_expr(target);
                self.names.extend(visitor.names);
            }
            _ => {}
        }

        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if expr.is_lambda_expr() {
            // Don't recurse.
            return;
        }

        visitor::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, comprehension: &'a Comprehension) {
        let mut visitor = NamesFromAssignmentsVisitor::default();
        visitor.visit_expr(&comprehension.target);
        self.names.extend(visitor.names);

        visitor::walk_comprehension(self, comprehension);
    }
}

/// B023
pub(crate) fn function_uses_loop_variable(checker: &Checker, node: &Node) {
    // Identify any "suspicious" variables. These are defined as variables that are
    // referenced in a function or lambda body, but aren't bound as arguments.
    let suspicious_variables = {
        let mut visitor =
            SuspiciousVariablesVisitor::new(checker.source(), checker.source_type.is_basedpython());
        match node {
            Node::Stmt(stmt) => visitor.visit_stmt(stmt),
            Node::Expr(expr) => visitor.visit_expr(expr),
        }
        visitor.names
    };

    if !suspicious_variables.is_empty() {
        // Identify any variables that are assigned in the loop (ignoring functions).
        let reassigned_in_loop = {
            let mut visitor = AssignedNamesVisitor::default();
            match node {
                Node::Stmt(stmt) => visitor.visit_stmt(stmt),
                Node::Expr(expr) => visitor.visit_expr(expr),
            }
            visitor.names
        };

        // basedpython gives every iteration its own binding for a loop or
        // comprehension *target*, so a closure made in the body reads that
        // iteration's value and the late-binding trap is gone for those names.
        // A name merely assigned in the body keeps python's late binding, and
        // so keeps its report.
        let bound_per_iteration = if checker.source_type.is_basedpython() {
            let mut visitor = IterationTargetsVisitor::default();
            match node {
                Node::Stmt(stmt) => visitor.visit_stmt(stmt),
                Node::Expr(expr) => visitor.visit_expr(expr),
            }
            visitor.names
        } else {
            Vec::new()
        };
        // the bindings the rebind cannot reach: a `def` reads a module-level
        // target as a global, which the rebuilt closure has no cell for, and a
        // `def` the parser synthesized is never rebound at all
        let rebind_reaches_a_def = !checker.semantic().current_scope().kind.is_module();

        // If a variable was used in a function or lambda body, and assigned in the
        // loop, flag it.
        for suspicious in suspicious_variables {
            let name = suspicious.name;
            if !reassigned_in_loop.contains(&name.id.as_str()) {
                continue;
            }
            let bound = bound_per_iteration.contains(&name.id.as_str())
                && match suspicious.read_by {
                    ReadBy::Lambda => true,
                    ReadBy::WrittenFunctionDef => rebind_reaches_a_def,
                    ReadBy::SynthesizedFunctionDef => false,
                };
            if bound {
                continue;
            }
            if checker.insert_flake8_bugbear_range(name.range()) {
                checker.report_diagnostic(
                    FunctionUsesLoopVariable {
                        name: name.id.to_string(),
                    },
                    name.range(),
                );
            }
        }
    }
}
