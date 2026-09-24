//! the names a runtime definition reads from the globals of the module it runs in
//!
//! pasted into a module, a helper's free names are looked up in that module's globals
//! before the builtins, so a module that binds `getattr` at its top level hands the
//! helper its own `getattr`. this finds each such read, by python's scoping rules: a name
//! a function, lambda or comprehension binds is its own, and a class body's names are
//! seen by the class body alone, never by a function inside it

use std::collections::HashSet;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use super::{Bindings, bind, bind_target};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Class,
    Function,
}

struct Scope {
    kind: ScopeKind,
    /// the names the scope binds for itself
    locals: HashSet<String>,
}

/// every read, in `stmts`, of a name that resolves to the module's globals, with its range
pub(super) fn global_reads(stmts: &[Stmt]) -> Vec<(TextRange, String)> {
    let mut reads = Reads {
        scopes: vec![Scope {
            kind: ScopeKind::Module,
            locals: HashSet::default(),
        }],
        found: Vec::new(),
    };
    for stmt in stmts {
        reads.visit_stmt(stmt);
    }
    reads.found
}

struct Reads {
    scopes: Vec<Scope>,
    found: Vec<(TextRange, String)>,
}

impl Reads {
    /// whether `name`, read in the innermost scope, is bound by it or by a function
    /// around it. a class body's names are its own and reach no scope inside it
    fn is_local(&self, name: &str) -> bool {
        let Some((innermost, outer)) = self.scopes.split_last() else {
            return false;
        };
        if innermost.kind != ScopeKind::Module && innermost.locals.contains(name) {
            return true;
        }
        outer
            .iter()
            .any(|scope| scope.kind == ScopeKind::Function && scope.locals.contains(name))
    }

    fn in_scope(&mut self, kind: ScopeKind, locals: HashSet<String>, run: impl FnOnce(&mut Self)) {
        self.scopes.push(Scope { kind, locals });
        run(self);
        self.scopes.pop();
    }

    fn parameters(&mut self, parameters: &ast::Parameters) {
        for parameter in parameters {
            if let Some(default) = parameter.default() {
                self.visit_expr(default);
            }
            if let Some(annotation) = parameter.annotation() {
                self.visit_expr(annotation);
            }
        }
    }
}

impl<'a> SourceOrderVisitor<'a> for Reads {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                // the decorators, defaults and annotations are evaluated where the `def` is
                for decorator in &function.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                self.parameters(&function.parameters);
                if let Some(returns) = &function.returns {
                    self.visit_expr(returns);
                }
                let mut locals: HashSet<String> = function
                    .parameters
                    .iter()
                    .map(|parameter| parameter.name().to_string())
                    .collect();
                locals.extend(scope_bindings(&function.body));
                for name in declared_global(&function.body) {
                    locals.remove(&name);
                }
                self.in_scope(ScopeKind::Function, locals, |reads| {
                    for stmt in &function.body {
                        reads.visit_stmt(stmt);
                    }
                });
            }
            Stmt::ClassDef(class) => {
                for decorator in &class.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                if let Some(arguments) = &class.arguments {
                    self.visit_arguments(arguments);
                }
                let locals = scope_bindings(&class.body);
                self.in_scope(ScopeKind::Class, locals, |reads| {
                    for stmt in &class.body {
                        reads.visit_stmt(stmt);
                    }
                });
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) if name.ctx.is_load() => {
                if !self.is_local(name.id.as_str()) {
                    self.found.push((name.range(), name.id.to_string()));
                }
            }
            Expr::Lambda(lambda) => {
                let mut locals = HashSet::default();
                if let Some(parameters) = &lambda.parameters {
                    self.parameters(parameters);
                    locals.extend(
                        parameters
                            .iter()
                            .map(|parameter| parameter.name().to_string()),
                    );
                }
                locals.extend(walrus_targets(&lambda.body));
                self.in_scope(ScopeKind::Function, locals, |reads| {
                    reads.visit_expr(&lambda.body);
                });
            }
            Expr::ListComp(ast::ExprListComp {
                elt, generators, ..
            })
            | Expr::SetComp(ast::ExprSetComp {
                elt, generators, ..
            })
            | Expr::Generator(ast::ExprGenerator {
                elt, generators, ..
            }) => self.comprehension(generators, |reads| reads.visit_expr(elt)),
            Expr::DictComp(ast::ExprDictComp {
                key,
                value,
                generators,
                ..
            }) => self.comprehension(generators, |reads| {
                if let Some(key) = key {
                    reads.visit_expr(key);
                }
                reads.visit_expr(value);
            }),
            _ => walk_expr(self, expr),
        }
    }
}

impl Reads {
    /// a comprehension is a scope of its own, binding its targets. its first iterable is
    /// evaluated in the scope around it
    fn comprehension(
        &mut self,
        generators: &[ast::Comprehension],
        element: impl FnOnce(&mut Self),
    ) {
        let Some((first, _)) = generators.split_first() else {
            return;
        };
        self.visit_expr(&first.iter);
        let mut targets = Bindings::new();
        for generator in generators {
            bind_target(&generator.target, &mut targets);
        }
        let locals = targets.into_keys().collect();
        self.in_scope(ScopeKind::Function, locals, |reads| {
            for (index, generator) in generators.iter().enumerate() {
                if index > 0 {
                    reads.visit_expr(&generator.iter);
                }
                for condition in &generator.ifs {
                    reads.visit_expr(condition);
                }
            }
            element(reads);
        });
    }
}

/// the names the statements of one scope bind for it
fn scope_bindings(body: &[Stmt]) -> HashSet<String> {
    let mut names = Bindings::new();
    for stmt in body {
        bind(stmt, &mut names);
    }
    names.into_keys().collect()
}

/// the names a function body declares `global`, wherever in the body it does so
fn declared_global(body: &[Stmt]) -> Vec<String> {
    #[derive(Default)]
    struct Globals(Vec<String>);
    impl<'a> SourceOrderVisitor<'a> for Globals {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::Global(global) => {
                    self.0.extend(global.names.iter().map(ToString::to_string));
                }
                // a nested scope's declarations are its own
                Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
                _ => walk_stmt(self, stmt),
            }
        }
        fn visit_expr(&mut self, _: &'a Expr) {}
    }
    let mut globals = Globals::default();
    for stmt in body {
        globals.visit_stmt(stmt);
    }
    globals.0
}

/// the names a `:=` binds in `expr`, outside any lambda nested in it
fn walrus_targets(expr: &Expr) -> Vec<String> {
    #[derive(Default)]
    struct Targets(Vec<String>);
    impl<'a> SourceOrderVisitor<'a> for Targets {
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Named(named) => {
                    if let Expr::Name(name) = named.target.as_ref() {
                        self.0.push(name.id.to_string());
                    }
                }
                Expr::Lambda(_) => return,
                _ => {}
            }
            walk_expr(self, expr);
        }
    }
    let mut targets = Targets::default();
    targets.visit_expr(expr);
    targets.0
}
