//! the settings module a django project points `DJANGO_SETTINGS_MODULE` at
//!
//! finding it needs the project's files, which is a notion of a crate above this
//! one. what lives here is the reading of a single file and the rule that picks
//! between the files that name a module — everything except the enumeration —
//! so that the type checker, the language server's django front end and the
//! mdtest harness all land on the same module by the same rule.
//!
//! nothing here guesses. a module is only the settings module because a script
//! of the project assigns `DJANGO_SETTINGS_MODULE` to its name; a project that
//! configures settings some other way names nothing and gets nothing.

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::source::source_text;
use ruff_db::system::SystemPath;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ty_module_resolver::{ModuleName, file_to_module, resolve_module};

use crate::Db;
use crate::place::imported_symbol;
use crate::types::ProgramEnvironment;
use crate::types::{ClassType, Type};
use ty_module_resolver::ImportingFile;

/// the environment variable a project names its settings module with
const SETTINGS_MODULE_VARIABLE: &str = "DJANGO_SETTINGS_MODULE";

/// the file stem of the script whose `DJANGO_SETTINGS_MODULE` is the one that counts
const SETTINGS_ENTRY_POINT: &str = "manage";

/// the module `django.conf.settings` is an instance of a class from
const SETTINGS_MODULE: &str = "django.conf";

/// the class `django.conf.settings` is an instance of
const SETTINGS_CLASS: &str = "LazySettings";

/// a file that points `DJANGO_SETTINGS_MODULE` somewhere, and where at
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct SettingsNaming {
    file: File,
    module: CompactString,
}

/// every file of `files` that names a settings module, in path order
///
/// the caller's files come in no order worth relying on, and every consumer has
/// to land on the same file twice running.
pub fn settings_namings(db: &dyn Db, files: impl IntoIterator<Item = File>) -> Vec<SettingsNaming> {
    let mut namings: Vec<SettingsNaming> = files
        .into_iter()
        // a stub sets no environment variable
        .filter(|file| !is_stub(db, *file))
        .filter_map(|file| {
            Some(SettingsNaming {
                file,
                module: settings_module_in_file(db, file).clone()?,
            })
        })
        .collect();

    namings.sort_by(|left, right| {
        left.file
            .path(db)
            .as_str()
            .cmp(right.file.path(db).as_str())
    });

    namings
}

/// the settings module the namings point `DJANGO_SETTINGS_MODULE` at
///
/// a project names it in `manage.py`, and usually again in its `wsgi.py` and
/// `asgi.py`. where they disagree it is `manage.py` that decides, since that is
/// the one a developer runs.
pub fn settings_file(db: &dyn Db, namings: &[SettingsNaming]) -> Option<File> {
    let naming = namings
        .iter()
        .find(|naming| is_entry_point(db, naming.file))
        .or_else(|| namings.first())?;

    resolve_module(
        db,
        ImportingFile::File(
            naming.file,
            db.program_file(naming.file).resolver_environment(db),
        ),
        &ModuleName::new(&naming.module)?,
    )?
    .file(db)
}

/// the naming that is django's own entry point, the script `manage.py test`,
/// `manage.py migrate` and the rest are run through
pub fn entry_point_file(db: &dyn Db, namings: &[SettingsNaming]) -> Option<File> {
    namings
        .iter()
        .find(|naming| is_entry_point(db, naming.file))
        .map(|naming| naming.file)
}

/// whether `file` is the script django's own `startproject` writes
fn is_entry_point(db: &dyn Db, file: File) -> bool {
    file.path(db)
        .as_system_path()
        .and_then(SystemPath::file_stem)
        == Some(SETTINGS_ENTRY_POINT)
}

fn is_stub(db: &dyn Db, file: File) -> bool {
    matches!(file.path(db).extension(), Some("pyi" | "byi"))
}

/// the settings module `file` points `DJANGO_SETTINGS_MODULE` at
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub fn settings_module_in_file(db: &dyn Db, file: File) -> Option<CompactString> {
    if !source_text(db, file).contains(SETTINGS_MODULE_VARIABLE) {
        return None;
    }

    let parsed = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    let mut visitor = SettingsModuleVisitor { found: None };
    visitor.visit_body(parsed.suite());

    visitor.found
}

/// finds the one string that names the settings module
struct SettingsModuleVisitor {
    found: Option<CompactString>,
}

impl<'ast> Visitor<'ast> for SettingsModuleVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // `os.environ["DJANGO_SETTINGS_MODULE"] = "project.settings"`
        if self.found.is_none()
            && let Stmt::Assign(assign) = stmt
            && let [Expr::Subscript(subscript)] = assign.targets.as_slice()
            && string_literal(&subscript.slice).as_deref() == Some(SETTINGS_MODULE_VARIABLE)
        {
            self.found = string_literal(&assign.value);
            return;
        }

        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // `os.environ.setdefault("DJANGO_SETTINGS_MODULE", "project.settings")`,
        // and every other two-string call that sets the variable the same way
        if self.found.is_none()
            && let Expr::Call(call) = expr
            && let [variable, module] = call.arguments.args.as_ref()
            && string_literal(variable).as_deref() == Some(SETTINGS_MODULE_VARIABLE)
        {
            self.found = string_literal(module);
            return;
        }

        walk_expr(self, expr);
    }
}

/// whether `ty` is `django.conf.settings` — the one object whose attributes are
/// the project's settings
pub(crate) fn is_settings_instance(
    db: &dyn Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
) -> bool {
    // a `LazySettings()` built by hand is inferred exactly, and the restriction
    // says nothing about which class it is
    let Type::NominalInstance(instance) = ty.erase_restriction(db) else {
        return false;
    };
    let class = instance.class(db, env).class_literal(db);

    class.name(db) == SETTINGS_CLASS
        && file_to_module(db, class.program_file(db).resolver_file(db))
            .is_some_and(|module| module.name(db) == SETTINGS_MODULE)
}

/// the type `django.conf.settings.NAME` has, read off the module the project
/// points `DJANGO_SETTINGS_MODULE` at
///
/// `None` — which leaves the stubs' `__getattr__` to answer `Any`, as it does
/// today — whenever the module cannot be reached, does not bind the name, or
/// binds it to something whose type says less than it appears to. see
/// [`describes_the_setting`].
pub(crate) fn settings_member<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    name: &Name,
) -> Option<Type<'db>> {
    // django copies a name off the settings module only when `name.isupper()`,
    // so a module's `BASE_DIR` is a setting and its `os` is not
    if !is_setting_name(name) {
        return None;
    }

    let module = db.django_settings_file()?;
    let member = imported_symbol(db, env, Some(db.program_file(module)), name, None)
        .place
        .ignore_possibly_undefined()?
        // the module assigns one deployment's value; what the setting *is* is the
        // type of that value, not the value
        .promote(db, env);

    describes_the_setting(db, env, member).then_some(member)
}

/// python's `str.isupper`, which is the test django copies a name by
fn is_setting_name(name: &Name) -> bool {
    let mut cased = false;
    for character in name.chars() {
        if character.is_lowercase() {
            return false;
        }
        cased |= character.is_uppercase();
    }
    cased
}

/// whether a settings module's binding says what the setting is, rather than
/// only what this deployment happens to put in it
///
/// a container literal is the second kind. `DATABASES = {"default": {"ENGINE":
/// ..., "NAME": ...}}` infers `dict[str, dict[str, str]]`, but django's contract
/// is that anything may read and write keys the literal never mentions —
/// `settings.DATABASES[alias]["TEST"]["USER"]` is django's own code — so the
/// inferred element types are narrower than the setting. a value whose type
/// carries no arguments has no such gap: `ROOT_URLCONF = "project.urls"` is a
/// `str` and there is nothing about the string's contents to be too narrow
/// about.
fn describes_the_setting(db: &dyn Db, env: &ProgramEnvironment<'_>, ty: Type<'_>) -> bool {
    match ty {
        Type::NominalInstance(instance) => {
            matches!(instance.class(db, env), ClassType::NonGeneric(_))
        }
        Type::Union(union) => union
            .elements(db)
            .iter()
            .all(|element| describes_the_setting(db, env, *element)),
        _ => false,
    }
}

fn string_literal(expr: &Expr) -> Option<CompactString> {
    match expr {
        Expr::StringLiteral(literal) => Some(literal.value.to_str().to_compact_string()),
        _ => None,
    }
}
