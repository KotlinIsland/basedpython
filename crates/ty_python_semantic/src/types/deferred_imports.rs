//! basedpython: which module-level `from` imports can be deferred by rewriting their reads
//!
//! below python 3.15 there is no `lazy` keyword, so the transpiler defers a `from m import x`
//! by binding nothing at the import and writing every read of `x` as a read through an object
//! that resolves `m.x` on first use. that is only the program python would run when every
//! read the rewrite reaches is a read of that import, and every read of that import is
//! rewritten: [`deferrable_from_imports`] answers which imports that holds for, and with
//! which reads. an import it leaves out stays an ordinary import

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, AnyNodeRef, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::FxHashMap;

use ty_python_core::ast_ids::HasScopedUseId;
use ty_python_core::definition::{Definition, DefinitionState};
use ty_python_core::place::ScopedPlaceId;
use ty_python_core::scope::{FileScopeId, ScopeKind};
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;

/// a module-level `from` import name whose every read can be rewritten
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferrableImport {
    /// the `ast::Alias` the name is imported by
    pub alias: TextRange,
    /// every read of the binding the alias makes, as the range of its `ast::ExprName`
    pub reads: Vec<TextRange>,
}

/// where a name read or written in a module resolves
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    /// the module's own namespace
    Module,
    /// a binding of some scope inside the module
    Elsewhere,
    /// either, depending on what has run: a class body that binds the name on some paths
    /// reads its own binding where it has made one and the module's where it has not
    Either,
}

/// basedpython: the module-level `from` imports of `file` whose binding is the only thing that
/// ever binds its name, with every read of it
///
/// an import qualifies when, across the whole module:
///
/// - nothing else binds, declares or deletes the name at module scope, nor from a scope that
///   declares it `global`: the index records such a binding among the module scope's own, for
///   the reads after the nested scope, and a `del` under `global` is a name node the read walk
///   finds
/// - every read that can reach the module's binding reaches nothing else: a class body that
///   binds the name on some paths to a read and not on others disqualifies it
/// - the module has no `from … import *`, which can bind any name unseen, and binds neither
///   `__getattr__` nor `__dir__`, which a deferred import is served through
///
/// a name inside a string is not a read here: in `.by` a string in a type expression is a
/// literal type, and a forward reference the transpiler writes as a string is its business
pub(crate) fn deferrable_from_imports<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
) -> Vec<DeferrableImport> {
    let module = parsed_module(db, file.python_file(db)).load(db);
    let index = semantic_index(db, file);
    let global = FileScopeId::global();
    let global_table = index.place_table(global);

    let mut statements = ModuleStatements::default();
    statements.visit_body(module.suite());
    if statements.star_import {
        return Vec::new();
    }
    let binds = |name: &str| {
        let Some(symbol) = global_table.symbol_id(name) else {
            return false;
        };
        let symbol = global_table.symbol(symbol);
        symbol.is_bound() || symbol.is_declared()
    };
    if binds("__getattr__") || binds("__dir__") {
        return Vec::new();
    }

    // every definition the module scope makes, by the place it makes it of. a deletion is
    // a `del`, whose target is a name node the read walk below finds, or the end of an
    // `except … as` clause, which bound the name first
    let use_def = index.use_def_map(global);
    let mut definitions: FxHashMap<ScopedPlaceId, Vec<Definition<'db>>> = FxHashMap::default();
    for (_, definition, _) in use_def.definitions_with_usage() {
        definitions
            .entry(definition.place(db))
            .or_default()
            .push(definition);
    }

    let mut candidates: FxHashMap<Name, Option<TextRange>> = FxHashMap::default();
    for alias in statements.aliases {
        let bound = alias.asname.as_ref().unwrap_or(&alias.name).id.clone();
        let sole = global_table.symbol_id(&bound).is_some_and(|symbol| {
            let place = ScopedPlaceId::Symbol(symbol);
            index
                .try_definitions(AnyNodeRef::from(alias))
                .is_some_and(|own| {
                    own.len() == 1
                        && definitions
                            .get(&place)
                            .is_some_and(|made| made.as_slice() == own)
                })
        });
        // a second import of the name is a second binding, so neither import qualifies
        candidates
            .entry(bound)
            .and_modify(|slot| *slot = None)
            .or_insert(sole.then_some(alias.range()));
    }
    candidates.retain(|_, alias| alias.is_some());
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut names = NameReferences {
        names: &candidates,
        found: Vec::new(),
    };
    names.visit_body(module.suite());
    let mut reads: FxHashMap<Name, Vec<TextRange>> = FxHashMap::default();
    for reference in names.found {
        let Some(Some(_)) = candidates.get(&reference.id) else {
            continue;
        };
        match (resolve(db, file, reference), reference.ctx) {
            (Resolution::Elsewhere, _) => {}
            (Resolution::Module, ast::ExprContext::Load) => {
                reads
                    .entry(reference.id.clone())
                    .or_default()
                    .push(reference.range());
            }
            // a write the module's own table does not show — a walrus in a comprehension —
            // or a read that may find another binding
            _ => {
                candidates.insert(reference.id.clone(), None);
            }
        }
    }

    let mut deferrable: Vec<DeferrableImport> = candidates
        .into_iter()
        .filter_map(|(name, alias)| {
            Some(DeferrableImport {
                alias: alias?,
                reads: reads.remove(&name).unwrap_or_default(),
            })
        })
        .collect();
    deferrable.sort_by_key(|import| import.alias.start());
    deferrable
}

/// where `reference` resolves, as python resolves it
///
/// a class body is where this differs from a lexical walk. a name the body binds anywhere is
/// read with `LOAD_NAME`: the class namespace, and then the module's — never an enclosing
/// function's — so a read the body has not bound on any path resolves to the module even
/// when a function around the class binds the name
fn resolve<'db>(db: &'db dyn Db, file: ProgramFile<'db>, reference: &ast::ExprName) -> Resolution {
    let index = semantic_index(db, file);
    let Some(own_scope) = index.try_expression_scope_id(&ast::ExprRef::from(reference)) else {
        return Resolution::Either;
    };
    for (scope_id, scope) in index.visible_ancestor_scopes(own_scope) {
        if scope_id.is_global() {
            return Resolution::Module;
        }
        let table = index.place_table(scope_id);
        let Some(symbol) = table.symbol_by_name(&reference.id) else {
            continue;
        };
        if symbol.is_global() {
            return Resolution::Module;
        }
        if symbol.is_nonlocal() {
            return Resolution::Elsewhere;
        }
        if !(symbol.is_bound() || symbol.is_declared()) {
            continue;
        }
        if scope.kind() != ScopeKind::Class {
            return Resolution::Elsewhere;
        }
        // an annotation scope sees the class it is defined in through the class namespace,
        // falling back to the names outside it when the class has not bound the name yet
        if scope_id != own_scope || !reference.ctx.is_load() {
            return if reference.ctx.is_load() {
                Resolution::Either
            } else {
                Resolution::Elsewhere
            };
        }
        let scope = scope_id.to_scope_id(db, file);
        let use_id = reference.scoped_use_id(db, file);
        let (mut defined, mut undefined) = (false, false);
        for binding in ty_python_core::use_def_map(db, scope).bindings_at_use(use_id) {
            match binding.binding {
                DefinitionState::Defined(_) => defined = true,
                DefinitionState::Undefined | DefinitionState::Deleted => undefined = true,
            }
        }
        return match (defined, undefined) {
            (true, false) => Resolution::Elsewhere,
            (false, _) => Resolution::Module,
            (true, true) => Resolution::Either,
        };
    }
    Resolution::Module
}

/// the module-level `from` imports, and whether there is a star import anywhere
#[derive(Default)]
struct ModuleStatements<'ast> {
    aliases: Vec<&'ast ast::Alias>,
    star_import: bool,
}

impl<'ast> SourceOrderVisitor<'ast> for ModuleStatements<'ast> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ImportFrom(import) => {
                if import.names.iter().any(|alias| &alias.name == "*") {
                    self.star_import = true;
                } else {
                    self.aliases.extend(import.names.iter());
                }
            }
            // a function's or a class's imports bind in that scope
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, _expr: &'ast Expr) {}
}

/// every name node spelling one of `names`, whatever it does and wherever it is
struct NameReferences<'a, 'ast> {
    names: &'a FxHashMap<Name, Option<TextRange>>,
    found: Vec<&'ast ast::ExprName>,
}

impl<'ast> SourceOrderVisitor<'ast> for NameReferences<'_, 'ast> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(name) = expr
            && self.names.contains_key(&name.id)
        {
            self.found.push(name);
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ty_python_core::ProgramFile;

    use crate::db::tests::TestDbBuilder;

    /// each deferrable import of `/src/main.py`, as `source` spells its alias, with the text of
    /// every read of it
    fn deferrable(source: &str) -> Vec<(String, Vec<String>)> {
        let db = TestDbBuilder::new()
            .with_file("/src/main.py", source)
            .build()
            .expect("the db builds");
        let file = system_path_to_file(&db, "/src/main.py").expect("the file exists");
        let file = ProgramFile::new(&db, file, db.program_environment().program(&db));
        super::deferrable_from_imports(&db, file)
            .into_iter()
            .map(|import| {
                (
                    source[import.alias].to_owned(),
                    import
                        .reads
                        .iter()
                        .map(|read| source[*read].to_owned())
                        .collect(),
                )
            })
            .collect()
    }

    /// whether `source`'s import of `dumps` is deferred
    fn defers_dumps(source: &str) -> bool {
        deferrable(source).iter().any(|(alias, _)| alias == "dumps")
    }

    #[test]
    fn a_name_bound_only_by_its_import_is_deferred_with_every_read() {
        let found = deferrable(
            "from json import dumps, loads as parse\n\
             def f():\n    return dumps(parse('1'))\n\
             print(dumps)\n",
        );
        assert_eq!(
            found,
            vec![
                (
                    "dumps".to_owned(),
                    vec!["dumps".to_owned(), "dumps".to_owned()]
                ),
                ("loads as parse".to_owned(), vec!["parse".to_owned()]),
            ]
        );
    }

    /// every way python binds, declares or deletes a name, at module scope or from a scope
    /// that declares it `global`, is a second binding
    #[test]
    fn any_other_binding_of_the_name_keeps_the_import() {
        for rebinding in [
            "dumps = 5",
            "dumps += 1",
            "dumps: int = 1",
            "dumps: int",
            "def dumps(): pass",
            "class dumps: pass",
            "for dumps in []: pass",
            "with open('x') as dumps: pass",
            "try:\n    pass\nexcept Exception as dumps:\n    pass",
            "if (dumps := 1): pass",
            "print([(dumps := i) for i in []])",
            "del dumps",
            "from json import dumps",
            "from pickle import dumps",
            "import dumps",
            "import pickle as dumps",
            "match 1:\n    case dumps:\n        pass",
            "match [1]:\n    case [*dumps]:\n        pass",
            "def f():\n    global dumps\n    dumps = 1",
            "def f():\n    global dumps\n    del dumps",
            "def f():\n    global dumps\n    def dumps(): pass",
            "def f():\n    global dumps\n    import json as dumps",
            "def f():\n    global dumps\n    for dumps in []: pass",
            "class C:\n    global dumps\n    dumps = 1",
        ] {
            let source = format!("from json import dumps\n{rebinding}\nprint(dumps)\n");
            assert!(!defers_dumps(&source), "deferred:\n{source}");
        }
    }

    /// a binding in a scope of its own is that scope's, and leaves the module's alone
    #[test]
    fn a_local_binding_of_the_name_is_not_the_module_s() {
        let found = deferrable(
            "from json import dumps\n\
             def f(dumps):\n    return dumps\n\
             def g():\n    dumps = 1\n    return dumps\n\
             lambda dumps: dumps\n\
             [dumps for dumps in []]\n\
             class C:\n    dumps = 1\n    seen = dumps\n\
             def h():\n    return dumps\n",
        );
        assert_eq!(found, vec![("dumps".to_owned(), vec!["dumps".to_owned()])]);
    }

    /// a read reaches the module's binding from any scope between it and the module that does
    /// not bind the name
    #[test]
    fn a_read_from_any_scope_that_reaches_the_module_is_a_read() {
        let found = deferrable(
            "from json import dumps\n\
             def nested():\n    def inner():\n        return dumps\n    return inner\n\
             class Body:\n    value = dumps\n\
             def default(f=dumps):\n    return f\n\
             def reads_global():\n    global dumps\n    return dumps\n\
             later = lambda: dumps\n\
             listed = [dumps for _ in []]\n\
             text = f'{dumps}'\n\
             @dumps\n\
             def decorated(): pass\n",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1.len(), 8, "{found:?}");
    }

    /// a class body reads a name it binds with `LOAD_NAME`: the class namespace, and then the
    /// module's — never a function's around it
    #[test]
    fn a_class_body_reads_the_module_until_it_binds_the_name() {
        let found = deferrable(
            "from json import dumps\n\
             def outer():\n    dumps = 1\n    class Body:\n        seen = dumps\n        dumps = 2\n",
        );
        assert_eq!(found, vec![("dumps".to_owned(), vec!["dumps".to_owned()])]);
    }

    /// a read the body has bound on some paths finds either binding
    #[test]
    fn a_class_body_read_that_may_find_its_own_binding_keeps_the_import() {
        assert!(!defers_dumps(
            "from json import dumps\nimport random\nclass Body:\n    if random.random():\n        dumps = 1\n    seen = dumps\n"
        ));
    }

    /// a star import can bind any name unseen, and a module's own `__getattr__` or `__dir__`
    /// is what a deferred name would be served through
    #[test]
    fn a_module_that_could_rebind_unseen_defers_nothing() {
        for source in [
            "from os import *\nfrom json import dumps\nprint(dumps)\n",
            "from json import dumps\ndef __getattr__(name): return dumps\n",
            "from json import dumps\ndef __dir__(): return []\n",
            "from json import dumps\n__getattr__ = print\n",
        ] {
            assert!(!defers_dumps(source), "deferred:\n{source}");
        }
    }

    /// an import inside a function or a class binds there
    #[test]
    fn an_import_in_a_scope_of_its_own_is_not_a_module_import() {
        assert!(deferrable("def f():\n    from json import dumps\n    return dumps\n").is_empty());
    }

    /// a name an import binds only on some paths is still bound by nothing else, and reads of
    /// it are reads of the import. what a read finds where the import did not run is the
    /// runtime's business: python's lookup falls back to the builtins
    #[test]
    fn an_import_on_some_paths_is_still_the_only_binding() {
        let found = deferrable(
            "import sys\nif sys.version_info < (3, 0):\n    from itertools import izip as zip\nprint(zip)\n",
        );
        assert_eq!(
            found,
            vec![("izip as zip".to_owned(), vec!["zip".to_owned()])]
        );
    }

    /// a package importing its own submodule, by relative or absolute name, is an ordinary
    /// import of a name nothing else binds
    #[test]
    fn a_package_importing_its_own_submodule_is_deferrable() {
        assert_eq!(
            deferrable("from . import helper\ndef use():\n    return helper.NAME\n"),
            vec![("helper".to_owned(), vec!["helper".to_owned()])]
        );
    }
}
