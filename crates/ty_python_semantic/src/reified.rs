//! detection of reified type parameters (basedpython)
//!
//! a pep 695 type parameter is *reified* when the function body references it
//! in a value position — anywhere other than a type annotation — or when a
//! parameter whose annotation mentions it is parametrically type-tested
//! (`x is list[int]` on `x: T`, which lowers to a comparison of the reified
//! `T` cell). detection is purely syntactic so the transpiler and the type
//! checker agree on it without sharing inference state; the source text is
//! needed only to tell the keyword `is` form from the `===` identity
//! operator, which the parser flattens to the same ast

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, CmpOp, Expr, Stmt};
use ruff_text_size::Ranged;
use rustc_hash::{FxHashMap, FxHashSet};

/// names of the function's plain type parameters that its body references in
/// a value position, in declaration order. `*Ts` / `**P` parameters never
/// participate (their reification is not supported yet)
pub fn reified_type_param_names(source: &str, function: &ast::StmtFunctionDef) -> Vec<Name> {
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
    let param_typevars = param_annotation_typevars(&function.parameters, &active);
    let mut finder = ValueUseFinder {
        source,
        active,
        param_typevars,
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

/// whether the `is` / `is not` between two compare operands is the keyword
/// form (isinstance semantics) rather than the `===` / `!==` identity
/// operators, which the parser flattens to the same ast
pub fn is_keyword_comparison(source: &str, op: CmpOp, lhs: &Expr, rhs: &Expr) -> bool {
    let between = &source[usize::from(lhs.range().end())..usize::from(rhs.range().start())];
    let trimmed = between.trim();
    match op {
        CmpOp::Is => trimmed == "is",
        CmpOp::IsNot => !trimmed.starts_with("!=="),
        _ => false,
    }
}

/// parameter name → the still-active type-param names its annotation
/// mentions. a parametric `is` test on such a parameter lowers to an equality
/// check of those params' reified cells, so the test is a value-position use
fn param_annotation_typevars<'a>(
    parameters: &'a ast::Parameters,
    active: &FxHashSet<&'a str>,
) -> FxHashMap<&'a str, Vec<&'a str>> {
    let mut map = FxHashMap::default();
    for parameter in parameters {
        if let Some(annotation) = parameter.annotation() {
            let mut mentions = AnnotationMentions {
                active,
                mentioned: Vec::new(),
            };
            mentions.visit_expr(annotation);
            if !mentions.mentioned.is_empty() {
                map.insert(parameter.name().id.as_str(), mentions.mentioned);
            }
        }
    }
    map
}

struct AnnotationMentions<'a, 'b> {
    active: &'b FxHashSet<&'a str>,
    mentioned: Vec<&'a str>,
}

impl<'a> Visitor<'a> for AnnotationMentions<'a, '_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && self.active.contains(name.id.as_str())
            && !self.mentioned.contains(&name.id.as_str())
        {
            self.mentioned.push(name.id.as_str());
        }
        walk_expr(self, expr);
    }
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
    source: &'a str,
    active: FxHashSet<&'a str>,
    /// parameters of the innermost enclosing def whose annotations mention
    /// active type params — parametric `is` tests on them reify those params
    param_typevars: FxHashMap<&'a str, Vec<&'a str>>,
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

    /// a keyword-form `is` / `is not` pair testing a `T`-annotated parameter
    /// against a subscripted type reifies `T` — the lowering compares the
    /// reified cell against the target's type arguments
    fn check_parametric_tests(&mut self, compare: &'a ast::ExprCompare) {
        let mut lhs: &Expr = &compare.left;
        for (op, rhs) in compare.ops.iter().zip(&compare.comparators) {
            if matches!(op, CmpOp::Is | CmpOp::IsNot)
                && matches!(rhs, Expr::Subscript(_))
                && is_keyword_comparison(self.source, *op, lhs, rhs)
            {
                self.reify_tested_param(lhs);
            }
            lhs = rhs;
        }
    }

    /// a parametric `cast` / `cast?` of a `T`-annotated parameter against a
    /// subscripted target reifies `T` for the same reason the `is` form does:
    /// the check lowers to a comparison of the reified cell against the
    /// target's type arguments
    fn check_parametric_cast(&mut self, type_arg: &'a Expr, value_arg: &'a Expr) {
        if matches!(type_arg, Expr::Subscript(_)) {
            self.reify_tested_param(value_arg);
        }
    }

    /// mark as reified every still-active type parameter mentioned by the
    /// declared annotation of the parameter `value` names
    fn reify_tested_param(&mut self, value: &'a Expr) {
        let Expr::Name(name) = value else {
            return;
        };
        let Some(typevars) = self.param_typevars.get(name.id.as_str()) else {
            return;
        };
        let reified: Vec<&'a str> = typevars
            .iter()
            .copied()
            .filter(|typevar| self.active.contains(typevar))
            .collect();
        self.found.extend(reified);
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
                // a nested def's parameters may re-annotate with an enclosing
                // (still-active) type param; parametric tests on them inside
                // the nested body reach the same reified cell via the closure
                let mut nested_active = self.active.clone();
                for name in &shadowed {
                    nested_active.remove(name);
                }
                let nested_map = param_annotation_typevars(&def.parameters, &nested_active);
                let saved_map = std::mem::replace(&mut self.param_typevars, nested_map);
                self.visit_nested_body(&def.body, shadowed.into_iter());
                self.param_typevars = saved_map;
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
            Expr::Compare(compare) => {
                self.check_parametric_tests(compare);
                walk_expr(self, expr);
            }
            // `value cast T` / `value cast? T` parse as a call whose first
            // argument is the target type — a type position
            Expr::Call(call) if call.is_cast || call.is_checked_cast => {
                if let [type_arg, value_arg] = &*call.arguments.args {
                    self.check_parametric_cast(type_arg, value_arg);
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
