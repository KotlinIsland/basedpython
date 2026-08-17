//! lowering one file on its own
//!
//! the project driver hands the builder a real project database so cross-module
//! types resolve. a single file needs less, and building a db per file keeps the
//! path simple for tests and for `by compile FILE`.
//!
//! this mirrors `by_transforms`, which builds the same kind of db for its own
//! single-file entry point.

use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
use ruff_python_ast::Stmt;
use ty_project::{ProjectMetadata, TestDb};
use ty_python_semantic::{ProgramEnvironment, SemanticModel};

use crate::Language;

/// build a one-file db from basedpython source and hand its model and suite to `f`
pub fn with_source<T>(
    source: &str,
    f: impl FnOnce(
        &dyn ty_python_semantic::Db,
        &ProgramEnvironment<'_>,
        &SemanticModel<'_>,
        &[Stmt],
    ) -> T,
) -> T {
    with_source_in(source, Language::BasedPython, f)
}

/// build a one-file db from source of a given language and hand its model and
/// suite to `f`
pub fn with_source_in<T>(
    source: &str,
    language: Language,
    f: impl FnOnce(
        &dyn ty_python_semantic::Db,
        &ProgramEnvironment<'_>,
        &SemanticModel<'_>,
        &[Stmt],
    ) -> T,
) -> T {
    let (db, file) = make_db(source, language);
    let program_file = ty_python_semantic::Db::program_file(&db, file);
    let parsed = ruff_db::parsed::parsed_module(&db, program_file.python_file(&db)).load(&db);
    let model = SemanticModel::new(&db, program_file);
    let env = ProgramEnvironment::from_file(program_file);
    f(&db, &env, &model, parsed.suite())
}

fn make_db(source: &str, language: Language) -> (TestDb, File) {
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program().expect("program init failed");
    // the extension is what tells the parser which language this is
    let path = format!("/input.{}", language.extension());
    db.write_file(&path, source).expect("write failed");
    let file = system_path_to_file(&db, &path).expect("file not in db");
    (db, file)
}

/// lower source into a module named `module_name`
pub fn module_from_source(
    source: &str,
    module_name: impl Into<by_ir::ModuleName>,
    language: Language,
) -> by_ir::function::ModuleIr {
    with_source_in(source, language, |db, env, model, suite| {
        crate::build_module(
            db,
            env,
            model,
            suite,
            module_name,
            language.unique_loop_bindings(),
        )
    })
}
