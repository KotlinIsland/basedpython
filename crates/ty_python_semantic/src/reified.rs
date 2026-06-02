//! detection of reified type parameters (basedpython)
//!
//! a pep 695 type parameter is *reified* when the function body references it
//! in a value position — anywhere other than a type annotation. detection is
//! purely syntactic so the transpiler and the type checker agree on it without
//! sharing inference state

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use rustc_hash::FxHashSet;

/// names of the function's plain type parameters that its body references in
/// a value position, in declaration order. `*Ts` / `**P` parameters never
/// participate (their reification is not supported yet)
pub fn reified_type_param_names(function: &ast::StmtFunctionDef) -> Vec<Name> {
    let Some(type_params) = function.type_params.as_deref() else {
        return Vec::new();
    };
    let candidates: Vec<&Name> = type_params
        .type_params
        .iter()
        .filter_map(|param| match param {
            ast::TypeParam::TypeVar(tv) => Some(&tv.name.id),
            ast::TypeParam::TypeVarTuple(_) | ast::TypeParam::ParamSpec(_) => None,
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut active: FxHashSet<&str> = candidates.iter().map(|name| name.as_str()).collect();
    shadow_bound_names(&function.body, &mut active);
    let mut finder = ValueUseFinder {
        active,
        found: FxHashSet::default(),
    };
    for stmt in &function.body {
        finder.visit_stmt(stmt);
    }

    candidates
        .into_iter()
        .filter(|name| finder.found.contains(name.as_str()))
        .cloned()
        .collect()
}

/// remove from `active` every name the body binds itself — a local binding
/// shadows the type parameter, so references resolve to the local instead
fn shadow_bound_names<'a>(body: &'a [Stmt], active: &mut FxHashSet<&'a str>) {
    let mut collector = StoredNames {
        stored: FxHashSet::default(),
    };
    for stmt in body {
        collector.visit_stmt(stmt);
    }
    active.retain(|name| !collector.stored.contains(name));
}

struct StoredNames<'a> {
    stored: FxHashSet<&'a str>,
}

impl<'a> Visitor<'a> for StoredNames<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            // nested scopes bind only their own name here; their bodies don't
            // shadow the enclosing function
            Stmt::FunctionDef(def) => {
                self.stored.insert(def.name.id.as_str());
            }
            Stmt::ClassDef(def) => {
                self.stored.insert(def.name.id.as_str());
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.stored.insert(bound.id.as_str());
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.stored.insert(bound.id.as_str());
                }
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_store()
        {
            self.stored.insert(name.id.as_str());
        }
        walk_expr(self, expr);
    }
}

struct ValueUseFinder<'a> {
    active: FxHashSet<&'a str>,
    found: FxHashSet<&'a str>,
}

impl<'a> ValueUseFinder<'a> {
    /// walk a nested scope's body with extra shadowed names removed
    fn visit_nested_body(&mut self, body: &'a [Stmt], shadowed: impl Iterator<Item = &'a str>) {
        let saved = self.active.clone();
        for name in shadowed {
            self.active.remove(name);
        }
        shadow_bound_names(body, &mut self.active);
        for stmt in body {
            self.visit_stmt(stmt);
        }
        self.active = saved;
    }
}

impl<'a> Visitor<'a> for ValueUseFinder<'a> {
    fn visit_annotation(&mut self, _expr: &'a Expr) {
        // type position — never a reifying reference
    }

    fn visit_type_params(&mut self, _type_params: &'a ast::TypeParams) {
        // bounds and defaults are type positions
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            // `type X = …` is entirely a type position
            Stmt::TypeAlias(_) => {}
            Stmt::FunctionDef(def) => {
                for decorator in &def.decorator_list {
                    self.visit_decorator(decorator);
                }
                // annotations skip via `visit_annotation`; defaults are value
                // expressions evaluated in the enclosing body
                self.visit_parameters(&def.parameters);
                let own_type_params = def
                    .type_params
                    .as_deref()
                    .into_iter()
                    .flat_map(|tp| tp.type_params.iter().map(|p| p.name().id.as_str()));
                let param_names = def.parameters.iter().map(|param| param.name().id.as_str());
                let shadowed: Vec<&str> = own_type_params.chain(param_names).collect();
                self.visit_nested_body(&def.body, shadowed.into_iter());
            }
            Stmt::ClassDef(def) => {
                for decorator in &def.decorator_list {
                    self.visit_decorator(decorator);
                }
                if let Some(arguments) = &def.arguments {
                    self.visit_arguments(arguments);
                }
                let own_type_params = def
                    .type_params
                    .as_deref()
                    .into_iter()
                    .flat_map(|tp| tp.type_params.iter().map(|p| p.name().id.as_str()));
                let shadowed: Vec<&str> = own_type_params.collect();
                self.visit_nested_body(&def.body, shadowed.into_iter());
            }
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => {
                if name.ctx.is_load() && self.active.contains(name.id.as_str()) {
                    self.found.insert(name.id.as_str());
                }
            }
            // `value cast T` parses as a call whose first argument is the
            // target type — a type position
            Expr::Call(call) if call.is_cast => {
                if let [_type_arg, value_arg] = &*call.arguments.args {
                    self.visit_expr(value_arg);
                } else {
                    walk_expr(self, expr);
                }
            }
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.visit_parameters(parameters);
                    let shadowed: Vec<&str> = parameters
                        .iter()
                        .map(|param| param.name().id.as_str())
                        .collect();
                    let saved = self.active.clone();
                    for name in shadowed {
                        self.active.remove(name);
                    }
                    self.visit_expr(&lambda.body);
                    self.active = saved;
                } else {
                    self.visit_expr(&lambda.body);
                }
            }
            _ => walk_expr(self, expr),
        }
    }
}
