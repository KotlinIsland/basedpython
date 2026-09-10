//! the runtime helpers the emitted python calls, and the two ways a module gets
//! at them
//!
//! the helpers live in [`SOURCE`], which is `_by_runtime.py`. a build writes that
//! file beside the modules it emits and each module imports the names it calls. a
//! transpile with nowhere to write it (`by transpile <file>`, the language
//! server's `by/transpile`) pastes the definitions in instead. both are slices of
//! the one text, so the two renderings cannot drift apart
//!
//! # naming a helper
//!
//! a transform names a helper through one of the `Helper` constants, never a
//! string, so a misspelled helper does not compile. `every_helper_is_defined`
//! holds each constant to the file
//!
//! # slicing
//!
//! a helper resolves to the top-level statement that binds its name, as our own
//! parser reads the file. a comment written between two definitions falls outside
//! both and never reaches the output; one inside a body is part of it. a
//! top-level statement that binds nothing, such as
//! `_by_forward_operators(_LazyAttr)`, travels with the definition above it
//!
//! # dependencies
//!
//! helpers call each other: `_lazy_attr` resolves through `_lazy_module`,
//! `_soundness_parametric` through `_parametric_is`. what a helper needs is read
//! out of the calls in its body, so asking for one yields a set that runs

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, ExceptHandler, Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;

/// the runtime, as the file a build writes out
pub const SOURCE: &str = include_str!("runtime/_by_runtime.py");

/// the module a build writes [`SOURCE`] to, without a package qualifier
pub const MODULE_NAME: &str = "_by_runtime";

/// the file a build writes [`SOURCE`] to
pub const FILE_NAME: &str = "_by_runtime.py";

/// a runtime helper the emitted python calls, by the name it calls it under
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Helper(&'static str);

impl Helper {
    pub(crate) const fn name(self) -> &'static str {
        self.0
    }
}

macro_rules! helpers {
    ($($konst:ident = $name:literal,)*) => {
        $(pub(crate) const $konst: Helper = Helper($name);)*

        /// every helper a transform can name
        #[cfg(test)]
        const ALL: &[Helper] = &[$($konst),*];
    };
}

helpers! {
    SOUNDNESS_CHECK = "_soundness_check",
    SOUNDNESS_ITER = "_soundness_iter",
    SOUNDNESS_AITER = "_soundness_aiter",
    SOUNDNESS_PARAMETRIC = "_soundness_parametric",
    SOUNDNESS_ITER_P = "_soundness_iter_p",
    SOUNDNESS_AITER_P = "_soundness_aiter_p",
    CHECKED_CAST = "_checked_cast",
    TRY_CAST = "_try_cast",
    CHECKED_CAST_PRED = "_checked_cast_pred",
    TRY_CAST_PRED = "_try_cast_pred",
    PARAMETRIC_IS = "_parametric_is",
    PARAMETRIC_IS_LENIENT = "_parametric_is_lenient",
    PROTOCOL_IS = "_by_protocol_is",
    LITERAL = "_by_lit",
    PATTERN_IS = "_by_pattern_is",
    CONFORM = "_by_conform",
    CONFORMS = "_by_conforms",
    WITNESS = "_by_witness",
    WITNESS_CLASS = "_by_witness_class",
    WITNESS_GET = "_by_witness_get",
    TEMPLATE = "_Template",
    INTERPOLATION = "_Interpolation",
    GRAPHEMES = "_by_graphemes",
    PREFIX = "_by_prefix",
    SUFFIX = "_by_suffix",
    GENERIC = "generic",
    GENERIC_CLASS = "generic_class",
    TYPE_ARGUMENT = "_type_argument",
    DISCARD = "_by_discard",
    LOOP_BIND = "_by_loop_bind",
    STATIC_PROPERTY = "_by_static_property",
    OPTIONAL = "Optional",
    FORCE_UNWRAP = "_force_unwrap",
    MAIN_ARGS = "_by_main_args",
    RAISES = "_by_raises",
    LAZY_MODULE = "_lazy_module",
    LAZY_ATTR = "_lazy_attr",
    TY_EXT_MARKER = "_TyExtMarker",
    CHARACTER = "Character",
    MATCH_MISS = "_by_match_miss",
    MATCH_SEQ = "_by_match_seq",
    MATCH_MAP = "_by_match_map",
    MATCH_KEY = "_by_match_key",
    MATCH_REST = "_by_match_rest",
    MATCH_ARGS = "_by_match_args",
    MATCH_ATTR = "_by_match_attr",
}

/// one definition of the runtime, with whatever finishes setting it up
struct Definition {
    /// its source as the file spells it, newline-terminated like any other
    /// preamble entry
    source: String,
    /// position in the file. a set of definitions renders in this order, which
    /// is the order they can be executed in
    order: usize,
    /// the other definitions its body reads
    needs: Vec<String>,
}

/// every definition, indexed by each name it binds
fn index() -> &'static BTreeMap<String, Definition> {
    static INDEX: OnceLock<BTreeMap<String, Definition>> = OnceLock::new();
    INDEX.get_or_init(build_index)
}

fn build_index() -> BTreeMap<String, Definition> {
    // `the_runtime_parses` pins the file as valid python. were it not, every
    // helper would come back undefined and phase 3 would report the calls
    let Ok(parsed) = parse_module(SOURCE) else {
        return BTreeMap::new();
    };
    let suite = parsed.suite();
    // two passes: the first learns every name the file defines, so the second can
    // tell a call to a sibling from a call to a builtin
    let defined: BTreeSet<String> = suite.iter().flat_map(bindings).collect();

    let mut definitions = BTreeMap::new();
    let mut order = 0usize;
    let mut at = 0usize;
    while at < suite.len() {
        let names = bindings(&suite[at]);
        if names.is_empty() {
            // the module docstring: every later statement that binds nothing is
            // swept up by the definition before it
            at += 1;
            continue;
        }
        let mut last = at;
        while last + 1 < suite.len() && bindings(&suite[last + 1]).is_empty() {
            last += 1;
        }
        let span = suite[at].range().cover(suite[last].range());
        let mut source = SOURCE[span].to_owned();
        if !source.ends_with('\n') {
            source.push('\n');
        }
        let mut needs: Vec<String> = suite[at..=last]
            .iter()
            .flat_map(reads)
            .filter(|read| defined.contains(read) && !names.contains(read))
            .collect();
        needs.sort();
        needs.dedup();
        // `every_name_has_one_definition` keeps a name from being bound twice,
        // so first-wins here never decides anything
        for name in names {
            definitions.entry(name).or_insert_with(|| Definition {
                source: source.clone(),
                order,
                needs: needs.clone(),
            });
        }
        order += 1;
        at = last + 1;
    }
    definitions
}

/// the names a top-level statement binds at module scope
pub(crate) fn bindings(stmt: &Stmt) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    bind(stmt, &mut names);
    names
}

/// a block that runs at module scope (an `if`, a `try`, a loop) binds what the
/// statements inside it bind. a function or class body is a scope of its own
fn bind(stmt: &Stmt, names: &mut BTreeSet<String>) {
    let block = |body: &[Stmt], names: &mut BTreeSet<String>| {
        for stmt in body {
            bind(stmt, names);
        }
    };
    match stmt {
        Stmt::FunctionDef(def) => {
            names.insert(def.name.to_string());
        }
        Stmt::ClassDef(def) => {
            names.insert(def.name.to_string());
        }
        Stmt::Assign(assign) => {
            for target in &assign.targets {
                if let Expr::Name(name) = target {
                    names.insert(name.id.to_string());
                }
            }
        }
        Stmt::AnnAssign(assign) => {
            if let Expr::Name(name) = assign.target.as_ref() {
                names.insert(name.id.to_string());
            }
        }
        Stmt::Import(import) => {
            for alias in &import.names {
                let bound = match &alias.asname {
                    Some(asname) => asname.to_string(),
                    // `import a.b` binds `a`
                    None => alias.name.split('.').next().unwrap_or_default().to_string(),
                };
                names.insert(bound);
            }
        }
        Stmt::ImportFrom(import) => {
            for alias in &import.names {
                names.insert(match &alias.asname {
                    Some(asname) => asname.to_string(),
                    None => alias.name.to_string(),
                });
            }
        }
        Stmt::If(node) => {
            block(&node.body, names);
            for clause in &node.elif_else_clauses {
                block(&clause.body, names);
            }
        }
        Stmt::Try(node) => {
            block(&node.body, names);
            for ExceptHandler::ExceptHandler(handler) in &node.handlers {
                block(&handler.body, names);
            }
            block(&node.orelse, names);
            block(&node.finalbody, names);
        }
        Stmt::With(node) => block(&node.body, names),
        Stmt::For(node) => {
            block(&node.body, names);
            block(&node.orelse, names);
        }
        Stmt::While(node) => {
            block(&node.body, names);
            block(&node.orelse, names);
        }
        _ => {}
    }
}

/// every name a statement loads, at any depth
fn reads(stmt: &Stmt) -> BTreeSet<String> {
    struct Reads(BTreeSet<String>);
    impl SourceOrderVisitor<'_> for Reads {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Name(ast::ExprName { id, ctx, .. }) = expr
                && ctx.is_load()
            {
                self.0.insert(id.to_string());
            }
            walk_expr(self, expr);
        }
    }
    let mut visitor = Reads(BTreeSet::new());
    ruff_python_ast::visitor::source_order::walk_stmt(&mut visitor, stmt);
    visitor.0
}

/// the definitions `helpers` need, their own included, in the order the file
/// defines them
///
/// a name the file does not define is left out rather than reported here: the
/// module then calls something it never got, and phase 3 says so with a span
fn closure(helpers: impl IntoIterator<Item = Helper>) -> Vec<&'static Definition> {
    let index = index();
    let mut resolved: BTreeMap<usize, &'static Definition> = BTreeMap::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = helpers.into_iter().map(Helper::name).collect();
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        let Some(definition) = index.get(name) else {
            continue;
        };
        // a statement bound under several names is still one definition
        resolved.insert(definition.order, definition);
        queue.extend(definition.needs.iter().map(String::as_str));
    }
    resolved.into_values().collect()
}

/// whether `name` is one of the runtime's definitions
pub(crate) fn defines(name: &str) -> bool {
    index().contains_key(name)
}

/// the definitions `helpers` need, as preamble entries for a module that has
/// nowhere to import them from
pub(crate) fn inline(helpers: impl IntoIterator<Item = Helper>) -> Vec<String> {
    closure(helpers)
        .into_iter()
        .map(|definition| definition.source.clone())
        .collect()
}

/// the import a module uses to reach helpers written out beside it
///
/// it names only what the emitted code calls: a helper that exists because
/// another helper calls it is resolved inside `_by_runtime`. absolute rather than
/// relative, because a relative import fails in a module run as `__main__`
pub(crate) fn import_line(module: &str, helpers: impl IntoIterator<Item = Helper>) -> String {
    let names: BTreeSet<&str> = helpers.into_iter().map(Helper::name).collect();
    format!(
        "from {module} import {}",
        names.into_iter().collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_parses() {
        if let Err(error) = parse_module(SOURCE) {
            panic!("{FILE_NAME} does not parse: {error}");
        }
    }

    /// a constant naming something the file does not define would reach the
    /// output as a call to nothing
    #[test]
    fn every_helper_is_defined() {
        for helper in ALL {
            assert!(
                defines(helper.name()),
                "`{}` is not defined in {FILE_NAME}",
                helper.name()
            );
        }
    }

    /// two statements binding one name would leave it to file order which one a
    /// module gets
    #[test]
    fn every_name_has_one_definition() {
        let parsed = parse_module(SOURCE).expect("the runtime parses");
        let mut seen = BTreeSet::new();
        for stmt in parsed.suite() {
            for name in bindings(stmt) {
                assert!(seen.insert(name.clone()), "`{name}` is bound twice");
            }
        }
    }

    /// the preamble is one entry per line, so an entry that did not end its own
    /// line would run into the next
    #[test]
    fn definitions_end_in_a_newline() {
        for (name, definition) in index() {
            assert!(definition.source.ends_with('\n'), "`{name}` does not");
        }
    }

    #[test]
    fn dependencies_resolve() {
        for (name, definition) in index() {
            for need in &definition.needs {
                assert!(defines(need), "`{name}` reads unknown `{need}`");
            }
        }
    }

    /// the discard adapter is spelled by the checker as well as emitted here
    #[test]
    fn the_discard_adapter_is_the_one_ty_resolves() {
        assert_eq!(DISCARD.name(), ty_python_semantic::DISCARD_ADAPTER);
    }

    /// the proxy's operator forwarding is read by nothing; it has to travel with
    /// the class it patches, or the proxy is left with no operators at all
    #[test]
    fn set_up_code_travels_with_its_definition() {
        let pasted = inline([LAZY_ATTR]).concat();
        assert!(pasted.contains("class _LazyAttr:"), "{pasted}");
        assert!(
            pasted.contains("_by_forward_operators(_LazyAttr)"),
            "{pasted}"
        );
        assert!(pasted.contains("def _lazy_module("), "{pasted}");
    }
}
