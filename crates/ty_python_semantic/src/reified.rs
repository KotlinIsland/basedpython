//! detection of reified type parameters (basedpython)
//!
//! a pep 695 type parameter is *reified* when it is declared `reified`, when
//! the body references it in a value position — anywhere other than a type
//! annotation — or when a parameter whose annotation mentions it is
//! parametrically type-tested (`x is list[int]` on `x: T`, which lowers to a
//! comparison of the reified `T` cell). detection is purely syntactic so the
//! transpiler and the type checker agree on it without sharing inference
//! state; the source text is needed only to tell the keyword `is` form from
//! the `===` identity operator, which the parser flattens to the same ast
//!
//! a function and a class both reify, but they read the value from different
//! places. a function's type argument belongs to the call, so it lives in the
//! closure the wrapper rebuilds; a class's belongs to the *instance*, so it is
//! read through a receiver — which is why [`reified_class_reads`] reports each
//! read against the method that can supply one

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, CmpOp, Expr, PySourceType, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};

/// names of the function's type parameters that are reified — declared
/// `reified`, or referenced by the body in a value position — in declaration
/// order. every kind of parameter can carry a runtime value: a plain `T` the
/// type argument, a `*Ts` the tuple of the run it absorbs, and a `**Kwargs`
/// the mapping of its fields. `**Kwargs` is a keyword-variadic pack only in a
/// basedpython *source* file — elsewhere the same spelling declares a
/// `ParamSpec`, a parameter list with no runtime object to bind — so
/// `source_type` decides whether it is a candidate
pub fn reified_type_param_names(
    source: &str,
    source_type: PySourceType,
    function: &ast::StmtFunctionDef,
) -> Vec<Name> {
    let Some(type_params) = function.type_params.as_deref() else {
        return Vec::new();
    };
    // basedpython: a `type def` is not a runtime function — its type parameters are
    // the type arguments of an application, and the declaration is erased by the
    // transpiler, so there is nothing to reify
    if ast::helpers::is_type_def(function) {
        return Vec::new();
    }
    // `(name, declared)` — a declared `reified` is reified whether or not the body
    // ever reads it, which is the point of writing the keyword
    let candidates: Vec<(&Name, bool)> = type_params
        .type_params
        .iter()
        .filter_map(|param| match param {
            // basedpython: a `some T` hole shares its parameter's name, so every use of the
            // parameter in the body would otherwise read as a value-position use of the hole.
            // a hole is type-only and never reified
            ast::TypeParam::TypeVar(tv) => {
                (!tv.is_some_hole).then_some((&tv.name.id, tv.is_reified))
            }
            ast::TypeParam::TypeVarTuple(tvt) => Some((&tvt.name.id, tvt.is_reified)),
            ast::TypeParam::ParamSpec(ps) => matches!(source_type, PySourceType::BasedPython)
                .then_some((&ps.name.id, ps.is_reified)),
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut active: FxHashSet<&str> = candidates.iter().map(|(name, _)| name.as_str()).collect();
    shadow_bound_names(&function.body, &mut active);
    let param_typevars = param_annotation_typevars(&function.parameters, &active);
    let mut finder = ValueUseFinder {
        source,
        active,
        param_typevars,
        found: Vec::new(),
    };
    for stmt in &function.body {
        finder.visit_stmt(stmt);
    }
    let found = finder.names();

    candidates
        .into_iter()
        .filter(|(name, declared)| *declared || found.contains(name.as_str()))
        .map(|(name, _)| name.clone())
        .collect()
}

/// where a class's reified type parameters are read, and which of them are
/// reified at all. see [`reified_class_reads`]
#[derive(Debug, Default)]
pub struct ReifiedClassReads<'ast> {
    /// every reified parameter of the class, in declaration order
    pub names: Vec<Name>,
    /// one entry per method that reads a parameter, in source order
    pub methods: Vec<ReifiedMethodReads<'ast>>,
    /// reads no receiver can answer, in source order
    pub unanswerable: Vec<UnansweredRead>,
    /// where the class writes its own `__class_getitem__`, if it does. that is
    /// the subscript a specialization is built by, so a class that answers it
    /// itself has nowhere to record its type arguments
    pub own_class_getitem: Option<TextRange>,
}

/// the reified parameters one method of the class reads, and the receiver they
/// are read through
#[derive(Debug)]
pub struct ReifiedMethodReads<'ast> {
    pub function: &'ast ast::StmtFunctionDef,
    /// the method's first positional parameter — `self`, or `cls` on a
    /// classmethod
    pub receiver: &'ast str,
    /// in the class's declaration order
    pub names: Vec<Name>,
}

/// a read of a reified class type parameter that nothing can answer
#[derive(Debug)]
pub struct UnansweredRead {
    pub name: Name,
    /// the reference itself
    pub range: TextRange,
    pub reason: UnansweredReason,
}

#[derive(Debug, Clone, Copy)]
pub enum UnansweredReason {
    /// the read is not inside a method: the class body itself, a method's
    /// decorators or parameter defaults, or the body of a class nested
    /// directly in this one. all of those run while the class is being built,
    /// before any instance carries a type argument
    OutsideMethod,
    /// the read is inside a method that is called without a receiver — a
    /// `staticmethod`, or a signature with no positional parameter
    WithoutReceiver,
}

/// names of the class's type parameters that are reified — declared `reified`,
/// or read in a value position — together with the method each read belongs to.
///
/// a class parameter's runtime value is the type argument its *instance*
/// carries, so a read is answered through the receiver of the method it sits
/// in: reads in a method's body — including any function or class nested inside
/// it, which closes over the same binding — belong to that method, and a read
/// anywhere else is reported in
/// [`unanswerable`](ReifiedClassReads::unanswerable).
///
/// a `**Kwargs` keyword pack is never a candidate. a class writes its
/// specialization as a subscript, and a subscript takes no keyword arguments,
/// so there is no way to supply one
pub fn reified_class_reads<'ast>(
    source: &str,
    source_type: PySourceType,
    class: &'ast ast::StmtClassDef,
) -> ReifiedClassReads<'ast> {
    let Some(type_params) = class.type_params.as_deref() else {
        return ReifiedClassReads::default();
    };
    // a value-position read is ordinary python everywhere else — it names the
    // `TypeVar` object, and reading it must keep meaning that
    if !source_type.is_basedpython() {
        return ReifiedClassReads::default();
    }
    let candidates: Vec<(&Name, bool)> = type_params
        .type_params
        .iter()
        .filter_map(|param| match param {
            ast::TypeParam::TypeVar(tv) => {
                (!tv.is_some_hole).then_some((&tv.name.id, tv.is_reified))
            }
            ast::TypeParam::TypeVarTuple(tvt) => Some((&tvt.name.id, tvt.is_reified)),
            ast::TypeParam::ParamSpec(_) => None,
        })
        .collect();
    if candidates.is_empty() {
        return ReifiedClassReads::default();
    }

    // a stub has no body to read a parameter in, so a declaration is the only
    // thing that can reify one — and skipping the walk keeps the vendored
    // typeshed off this path entirely
    if source_type.is_stub() {
        return ReifiedClassReads {
            names: candidates
                .iter()
                .filter(|(_, declared)| *declared)
                .map(|(name, _)| (*name).clone())
                .collect(),
            methods: Vec::new(),
            unanswerable: Vec::new(),
            own_class_getitem: own_class_getitem(&class.body),
        };
    }

    let mut active: FxHashSet<&str> = candidates.iter().map(|(name, _)| name.as_str()).collect();
    shadow_bound_names(&class.body, &mut active);

    // one walk of the whole class body, then each read is judged by where it
    // landed. bucketing by position is what lets a method keep its reads
    // wherever the body writes the `def` — guarded by a version check, say —
    // while a read in the class body itself, in a method's header, or inside a
    // class nested in the class body has no method around it
    let mut finder = ValueUseFinder::new(source, active);
    for stmt in &class.body {
        finder.visit_stmt(stmt);
    }
    let methods: Vec<&ast::StmtFunctionDef> = class_methods(&class.body);

    let mut per_method: FxHashMap<usize, FxHashSet<&str>> = FxHashMap::default();
    let mut unanswerable_names: Vec<(&Name, TextRange, UnansweredReason)> = Vec::new();
    for (name, range) in &finder.found {
        let Some(declared) = candidates
            .iter()
            .find(|(candidate, _)| candidate.as_str() == *name)
            .map(|(candidate, _)| *candidate)
        else {
            continue;
        };
        match methods
            .iter()
            .position(|method| body_span(method).is_some_and(|span| span.contains_range(*range)))
        {
            Some(index) if method_receiver(methods[index]).is_some() => {
                per_method.entry(index).or_default().insert(*name);
            }
            Some(_) => {
                unanswerable_names.push((declared, *range, UnansweredReason::WithoutReceiver));
            }
            None => unanswerable_names.push((declared, *range, UnansweredReason::OutsideMethod)),
        }
    }

    // every reported list follows the declaration order, so a reader sees the
    // parameters in the order the class header writes them
    let ordered = |found: &FxHashSet<&str>| -> Vec<Name> {
        candidates
            .iter()
            .filter(|(name, _)| found.contains(name.as_str()))
            .map(|(name, _)| (*name).clone())
            .collect()
    };

    let mut unanswerable: Vec<UnansweredRead> = unanswerable_names
        .into_iter()
        .map(|(name, range, reason)| UnansweredRead {
            name: name.clone(),
            range,
            reason,
        })
        .collect();
    unanswerable.sort_unstable_by_key(|read| read.range.start());

    let read_anywhere = finder.names();
    ReifiedClassReads {
        names: candidates
            .iter()
            .filter(|(name, declared)| *declared || read_anywhere.contains(name.as_str()))
            .map(|(name, _)| (*name).clone())
            .collect(),
        own_class_getitem: own_class_getitem(&class.body),
        methods: methods
            .iter()
            .enumerate()
            .filter_map(|(index, function)| {
                let names = per_method.get(&index)?;
                Some(ReifiedMethodReads {
                    function,
                    receiver: method_receiver(function)?,
                    names: ordered(names),
                })
            })
            .collect(),
        unanswerable,
    }
}

/// where the class body writes its own `__class_getitem__`, if it does
fn own_class_getitem(body: &[Stmt]) -> Option<TextRange> {
    body.iter().find_map(|stmt| match stmt {
        Stmt::FunctionDef(function) if function.name.id.as_str() == "__class_getitem__" => {
            Some(function.name.range())
        }
        _ => None,
    })
}

/// the `def`s a class body writes, in source order, at any block depth — a
/// method guarded by a version check is still a method. a `def` inside another
/// `def` or inside a nested class belongs to that scope, not to this one
fn class_methods(body: &[Stmt]) -> Vec<&ast::StmtFunctionDef> {
    fn walk<'ast>(body: &'ast [Stmt], found: &mut Vec<&'ast ast::StmtFunctionDef>) {
        for stmt in body {
            match stmt {
                Stmt::FunctionDef(function) => found.push(function),
                Stmt::ClassDef(_) => {}
                Stmt::If(node) => {
                    walk(&node.body, found);
                    for clause in &node.elif_else_clauses {
                        walk(&clause.body, found);
                    }
                }
                Stmt::Try(node) => {
                    walk(&node.body, found);
                    for handler in &node.handlers {
                        let ast::ExceptHandler::ExceptHandler(handler) = handler;
                        walk(&handler.body, found);
                    }
                    walk(&node.orelse, found);
                    walk(&node.finalbody, found);
                }
                Stmt::With(node) => walk(&node.body, found),
                Stmt::For(node) => {
                    walk(&node.body, found);
                    walk(&node.orelse, found);
                }
                Stmt::While(node) => {
                    walk(&node.body, found);
                    walk(&node.orelse, found);
                }
                Stmt::Match(node) => {
                    for case in &node.cases {
                        walk(&case.body, found);
                    }
                }
                _ => {}
            }
        }
    }
    let mut found = Vec::new();
    walk(body, &mut found);
    found
}

/// the span of everything the source wrote in `function`'s body, which is what
/// tells a read in the body from one in the header the class body evaluates
fn body_span(function: &ast::StmtFunctionDef) -> Option<TextRange> {
    let written = || {
        function
            .body
            .iter()
            .map(Ranged::range)
            .filter(|range| !range.is_empty())
    };
    let start = written().map(TextRange::start).min()?;
    let end = written().map(TextRange::end).max()?;
    Some(TextRange::new(start, end))
}

/// names of the class's type parameters that are reified, in declaration order
pub fn reified_class_type_param_names(
    source: &str,
    source_type: PySourceType,
    class: &ast::StmtClassDef,
) -> Vec<Name> {
    reified_class_reads(source, source_type, class).names
}

/// names of the class's type parameters that are reified *only* because the
/// class reads them in a value position, in declaration order. this is what an
/// editor hints, where the keyword would be written
pub fn inferred_reified_class_type_param_names(
    source: &str,
    source_type: PySourceType,
    class: &ast::StmtClassDef,
) -> Vec<Name> {
    let declared: FxHashSet<&str> = class
        .type_params
        .as_deref()
        .into_iter()
        .flat_map(|type_params| type_params.iter())
        .filter(|param| param.is_reified())
        .map(|param| param.name().id.as_str())
        .collect();
    let mut names = reified_class_type_param_names(source, source_type, class);
    names.retain(|name| !declared.contains(name.as_str()));
    names
}

/// the parameter a read inside `function` is answered through — its first
/// positional parameter, which is `self` on a method and `cls` on a
/// classmethod.
///
/// `None` when the function is called without one: a `staticmethod`, or a
/// signature whose parameters are all keyword-only or variadic
fn method_receiver(function: &ast::StmtFunctionDef) -> Option<&str> {
    if function
        .decorator_list
        .iter()
        .any(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "staticmethod"))
    {
        return None;
    }
    let parameters = &function.parameters;
    parameters
        .posonlyargs
        .first()
        .or_else(|| parameters.args.first())
        .map(|parameter| parameter.parameter.name.id.as_str())
}

/// the names a `def` binds itself — its own type parameters and its parameters
/// — which shadow anything of the same name outside it
fn own_bindings(function: &ast::StmtFunctionDef) -> Vec<&str> {
    function
        .type_params
        .as_deref()
        .into_iter()
        .flat_map(|type_params| type_params.type_params.iter().map(|p| p.name().id.as_str()))
        .chain(
            function
                .parameters
                .iter()
                .map(|param| param.name().id.as_str()),
        )
        .collect()
}

/// names of the function's type parameters that are reified *only* because the
/// body reads them in a value position, in declaration order — everything
/// [`reified_type_param_names`] finds that does not already say so itself.
/// this is what an editor hints, where the keyword would be written
pub fn inferred_reified_type_param_names(
    source: &str,
    source_type: PySourceType,
    function: &ast::StmtFunctionDef,
) -> Vec<Name> {
    let declared: FxHashSet<&str> = function
        .type_params
        .as_deref()
        .into_iter()
        .flat_map(|type_params| type_params.iter())
        .filter(|param| param.is_reified())
        .map(|param| param.name().id.as_str())
        .collect();
    let mut names = reified_type_param_names(source, source_type, function);
    names.retain(|name| !declared.contains(name.as_str()));
    names
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
            // the name belongs to another scope for the whole body, so a
            // reference reads that binding and never the type parameter —
            // and python rejects a binding written above the declaration,
            // which is where a lowering would have to put one
            Stmt::Global(global) => {
                for name in &global.names {
                    self.stored.insert(name.id.as_str());
                }
            }
            Stmt::Nonlocal(nonlocal) => {
                for name in &nonlocal.names {
                    self.stored.insert(name.id.as_str());
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
    /// every value-position read, in visit order — a name can be read in more
    /// than one place, and for a class each place is judged separately
    found: Vec<(&'a str, TextRange)>,
}

impl<'a> ValueUseFinder<'a> {
    /// a finder for a region that binds no parameters of its own, so no
    /// annotation can carry a parametric type test into it
    fn new(source: &'a str, active: FxHashSet<&'a str>) -> Self {
        Self {
            source,
            active,
            param_typevars: FxHashMap::default(),
            found: Vec::new(),
        }
    }

    /// the distinct names read, in no particular order
    fn names(&self) -> FxHashSet<&'a str> {
        self.found.iter().map(|(name, _)| *name).collect()
    }

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
        // the test itself is the read: it compares the reified value against
        // the target's type arguments
        for typevar in reified {
            self.found.push((typevar, name.range()));
        }
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
                let shadowed = own_bindings(def);
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
                    self.found.push((name.id.as_str(), name.range()));
                }
            }
            Expr::Compare(compare) => {
                self.check_parametric_tests(compare);
                walk_expr(self, expr);
            }
            // every `cast` form parses as a call whose first argument is the
            // target type — a type position
            Expr::Call(call) if call.cast_kind.is_some() => {
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
