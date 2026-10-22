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
use ruff_python_ast::{PythonVersion, Stmt};
use ty_project::{ProjectMetadata, TestDb};
use ty_python_semantic::{LoweringModel, ProgramEnvironment};

use crate::Language;

/// build a one-file db from basedpython source and hand its model and suite to `f`
#[cfg(test)]
pub(crate) fn with_source<T>(
    source: &str,
    f: impl FnOnce(
        &dyn ty_python_semantic::Db,
        &ProgramEnvironment<'_>,
        &LoweringModel<'_>,
        &[Stmt],
    ) -> T,
) -> T {
    with_source_in(source, Language::BasedPython, f)
}

/// build a one-file db from source of a given language and hand its model and
/// suite to `f`
#[cfg(test)]
pub(crate) fn with_source_in<T>(
    source: &str,
    language: Language,
    f: impl FnOnce(
        &dyn ty_python_semantic::Db,
        &ProgramEnvironment<'_>,
        &LoweringModel<'_>,
        &[Stmt],
    ) -> T,
) -> T {
    with_source_at(source, language, PythonVersion::latest_ty(), f)
}

/// as [`with_source_in`], with the module analysed at `python_version`
fn with_source_at<T>(
    source: &str,
    language: Language,
    python_version: PythonVersion,
    f: impl FnOnce(
        &dyn ty_python_semantic::Db,
        &ProgramEnvironment<'_>,
        &LoweringModel<'_>,
        &[Stmt],
    ) -> T,
) -> T {
    let (db, file) = make_db(source, language, python_version);
    let program_file = ty_python_semantic::Db::program_file(&db, file);
    let parsed = ruff_db::parsed::parsed_module(&db, program_file.python_file(&db)).load(&db);
    let model = LoweringModel::new(&db, program_file);
    let env = ProgramEnvironment::from_file(program_file);
    f(&db, &env, &model, parsed.suite())
}

fn make_db(source: &str, language: Language, python_version: PythonVersion) -> (TestDb, File) {
    let mut db = TestDb::new(ProjectMetadata::new(
        ruff_python_ast::name::Name::new_static(""),
        SystemPathBuf::from("/"),
    ));
    db.init_program_with_python_version(python_version)
        .expect("program init failed");
    // the extension is what tells the parser which language this is
    let path = format!("/input.{}", language.extension());
    db.write_file(&path, source).expect("write failed");
    let file = system_path_to_file(&db, &path).expect("file not in db");
    (db, file)
}

/// lower source into a module named `module_name`, analysed at ty's newest python
///
/// a module that is built for an interpreter wants [`module_from_source_at`]: what a name
/// is, and so what a value's representation is, can depend on the version
pub fn module_from_source(
    source: &str,
    module_name: impl Into<by_ir::ModuleName>,
    options: impl Into<crate::LowerOptions>,
) -> by_ir::function::ModuleIr {
    module_from_source_at(source, module_name, options, PythonVersion::latest_ty())
}

/// lower source into a module named `module_name`, analysed at `python_version`
///
/// a `sys.version_info` test settles which branch binds a name, so a module analysed at
/// a newer python than the one it runs on can lay a value out in the representation the
/// branch it never takes would have given it
pub fn module_from_source_at(
    source: &str,
    module_name: impl Into<by_ir::ModuleName>,
    options: impl Into<crate::LowerOptions>,
    python_version: PythonVersion,
) -> by_ir::function::ModuleIr {
    let options = options.into();
    with_source_at(
        source,
        options.language,
        python_version,
        |db, env, model, suite| crate::build_module(db, env, model, suite, module_name, options),
    )
}
