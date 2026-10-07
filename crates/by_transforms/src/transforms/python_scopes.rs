//! the scope python evaluates each part of a module in, as a walk over its syntax tree
//!
//! a definition's body is a scope of its own, and the rest of the definition is not:
//! a decorator, a parameter's default and a base class are evaluated where the
//! definition is written. a comprehension's first iterable is evaluated where the
//! comprehension is written, and everything else in it in its own scope. an annotation
//! is evaluated in a scope of its own from 3.14, and the type parameters of a generic
//! definition, with everything that can name them — a generic class's bases, a generic
//! function's annotations, a `type` alias's value — in one more
//!
//! both the lowerings that bind a temporary and the check that the emitted module
//! compiles need these rules, so they are stated once here

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{AnyParameterRef, Comprehension, Expr, Parameters, Stmt};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    Class,
    Function {
        is_async: bool,
    },
    Lambda,
    /// a list, set or dict comprehension, or a generator expression
    Comprehension {
        generator: bool,
        is_async: bool,
    },
    /// an annotation
    Annotation,
    /// the type parameters of a generic definition
    TypeParams,
}

/// where the walk is: the scopes around the node, innermost last (the module is the
/// empty stack), and the constructs around it that python's rules ask about
#[derive(Default)]
pub(crate) struct ScopeState {
    scopes: Vec<Scope>,
    /// how many comprehension iterables the node is in
    iterables: usize,
    /// whether the node is in a loop's body, short of a scope opened inside it
    in_loop: bool,
    /// whether the node is in a `try` statement
    in_try: bool,
}

impl ScopeState {
    pub(crate) fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub(crate) fn in_iterable(&self) -> bool {
        self.iterables > 0
    }

    pub(crate) fn in_loop(&self) -> bool {
        self.in_loop
    }

    pub(crate) fn in_try(&self) -> bool {
        self.in_try
    }
}

/// what a walk does at each statement and expression, before it walks into it
pub(crate) trait ScopeHooks<'a> {
    fn stmt(&mut self, _stmt: &'a Stmt, _state: &ScopeState) {}
    fn expr(&mut self, _expr: &'a Expr, _state: &ScopeState) {}
}

/// walks `suite`, calling `hooks` at every statement and expression with the scope
/// state it is evaluated in
pub(crate) fn walk<'a>(suite: &'a [Stmt], hooks: &mut impl ScopeHooks<'a>) {
    let mut walker = Walker {
        state: ScopeState::default(),
        hooks,
    };
    for stmt in suite {
        walker.visit_stmt(stmt);
    }
}

struct Walker<'h, H> {
    state: ScopeState,
    hooks: &'h mut H,
}

impl<'a, H: ScopeHooks<'a>> Walker<'_, H> {
    fn in_scope(&mut self, scope: Scope, visit: impl FnOnce(&mut Self)) {
        self.state.scopes.push(scope);
        visit(self);
        self.state.scopes.pop();
    }

    /// a body is a scope of its own, and a loop around the definition does not
    /// reach into it
    fn in_body(&mut self, scope: Scope, visit: impl FnOnce(&mut Self)) {
        let in_loop = std::mem::replace(&mut self.state.in_loop, false);
        self.in_scope(scope, visit);
        self.state.in_loop = in_loop;
    }

    fn in_loop_body(&mut self, visit: impl FnOnce(&mut Self)) {
        let in_loop = std::mem::replace(&mut self.state.in_loop, true);
        visit(self);
        self.state.in_loop = in_loop;
    }

    fn visit_iterable(&mut self, iterable: &'a Expr) {
        self.state.iterables += 1;
        self.visit_expr(iterable);
        self.state.iterables -= 1;
    }

    fn visit_defaults(&mut self, parameters: &'a Parameters) {
        for default in parameters
            .iter_non_variadic_params()
            .filter_map(|parameter| parameter.default.as_deref())
        {
            self.visit_expr(default);
        }
    }

    fn visit_parameter_annotations(&mut self, parameters: &'a Parameters) {
        for parameter in parameters.iter().map(AnyParameterRef::as_parameter) {
            self.visit_parameter(parameter);
        }
    }

    fn visit_comprehension_scope(
        &mut self,
        generators: &'a [Comprehension],
        elements: &[&'a Expr],
        generator: bool,
    ) {
        let Some((first, rest)) = generators.split_first() else {
            return;
        };
        self.visit_iterable(&first.iter);
        let is_async = generators.iter().any(|generator| generator.is_async);
        self.in_scope(
            Scope::Comprehension {
                generator,
                is_async,
            },
            |walker| {
                walker.visit_expr(&first.target);
                for condition in &first.ifs {
                    walker.visit_expr(condition);
                }
                for generator in rest {
                    walker.visit_expr(&generator.target);
                    walker.visit_iterable(&generator.iter);
                    for condition in &generator.ifs {
                        walker.visit_expr(condition);
                    }
                }
                for element in elements {
                    walker.visit_expr(element);
                }
            },
        );
    }
}

impl<'a, H: ScopeHooks<'a>> Visitor<'a> for Walker<'_, H> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        self.hooks.stmt(stmt, &self.state);
        match stmt {
            Stmt::ClassDef(class) => {
                for decorator in &class.decorator_list {
                    self.visit_decorator(decorator);
                }
                match &class.type_params {
                    Some(type_params) => self.in_scope(Scope::TypeParams, |walker| {
                        walker.visit_type_params(type_params);
                        if let Some(arguments) = &class.arguments {
                            walker.visit_arguments(arguments);
                        }
                    }),
                    None => {
                        if let Some(arguments) = &class.arguments {
                            self.visit_arguments(arguments);
                        }
                    }
                }
                self.in_body(Scope::Class, |walker| walker.visit_body(&class.body));
            }
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
                self.visit_defaults(&function.parameters);
                let annotations = |walker: &mut Self| {
                    walker.visit_parameter_annotations(&function.parameters);
                    if let Some(returns) = &function.returns {
                        walker.visit_annotation(returns);
                    }
                };
                match &function.type_params {
                    Some(type_params) => self.in_scope(Scope::TypeParams, |walker| {
                        walker.visit_type_params(type_params);
                        annotations(walker);
                    }),
                    None => annotations(self),
                }
                let scope = Scope::Function {
                    is_async: function.is_async,
                };
                self.in_body(scope, |walker| walker.visit_body(&function.body));
            }
            Stmt::TypeAlias(alias) => {
                self.visit_expr(&alias.name);
                self.in_scope(Scope::TypeParams, |walker| {
                    if let Some(type_params) = &alias.type_params {
                        walker.visit_type_params(type_params);
                    }
                    walker.visit_expr(&alias.value);
                    for case in &alias.cases {
                        walker.visit_match_case(case);
                    }
                });
            }
            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.iter);
                self.visit_expr(&for_stmt.target);
                if let Some(pattern) = &for_stmt.pattern {
                    self.visit_pattern(pattern);
                }
                self.in_loop_body(|walker| walker.visit_body(&for_stmt.body));
                self.visit_body(&for_stmt.orelse);
            }
            Stmt::While(while_stmt) => {
                self.visit_expr(&while_stmt.test);
                self.in_loop_body(|walker| walker.visit_body(&while_stmt.body));
                self.visit_body(&while_stmt.orelse);
            }
            Stmt::Try(_) => {
                let in_try = std::mem::replace(&mut self.state.in_try, true);
                walk_stmt(self, stmt);
                self.state.in_try = in_try;
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_annotation(&mut self, expr: &'a Expr) {
        self.in_scope(Scope::Annotation, |walker| walker.visit_expr(expr));
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        self.hooks.expr(expr, &self.state);
        match expr {
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.visit_defaults(parameters);
                    self.visit_parameter_annotations(parameters);
                }
                if let Some(returns) = &lambda.returns {
                    self.visit_annotation(returns);
                }
                self.in_body(Scope::Lambda, |walker| walker.visit_expr(&lambda.body));
            }
            Expr::ListComp(comprehension) => {
                self.visit_comprehension_scope(
                    &comprehension.generators,
                    &[&comprehension.elt],
                    false,
                );
            }
            Expr::SetComp(comprehension) => {
                self.visit_comprehension_scope(
                    &comprehension.generators,
                    &[&comprehension.elt],
                    false,
                );
            }
            Expr::Generator(comprehension) => {
                self.visit_comprehension_scope(
                    &comprehension.generators,
                    &[&comprehension.elt],
                    true,
                );
            }
            Expr::DictComp(comprehension) => {
                let elements: Vec<&Expr> = comprehension
                    .key
                    .as_deref()
                    .into_iter()
                    .chain([&*comprehension.value])
                    .collect();
                self.visit_comprehension_scope(&comprehension.generators, &elements, false);
            }
            _ => walk_expr(self, expr),
        }
    }
}
