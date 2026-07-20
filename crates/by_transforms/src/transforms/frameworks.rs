//! Framework lowering gates — reject basedpython constructs that are
//! runtime-broken inside framework-transformed classes.
//!
//! The conformance contract is the compatibility matrix in
//! `docs/basedpython/frameworks/index.md`. This pass only ever *rejects*
//! with a clear message; a lowering that must merely adapt to a framework
//! does so in its own pass by consulting the same
//! [`TypeInfo::framework_class_role`] query.

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef};

use super::ast_driver::{PassContext, TypeAwarePass};
use super::source_util::is_synthetic_decorator;
use crate::type_info::{FrameworkRole, TypeInfo};

pub(crate) struct FrameworksPass<'src> {
    source: &'src str,
}

impl<'src> FrameworksPass<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

fn role_name(role: FrameworkRole) -> &'static str {
    match role {
        FrameworkRole::PydanticModel => "pydantic model",
        FrameworkRole::DjangoModel => "django model",
        FrameworkRole::SqlalchemyDeclarative => "sqlalchemy declarative model",
    }
}

struct GateVisitor<'a> {
    source: &'a str,
    types: &'a dyn TypeInfo,
    errors: Vec<String>,
}

impl GateVisitor<'_> {
    fn check_class(&mut self, class: &StmtClassDef) {
        let Some(role) = self.types.framework_class_role(class) else {
            return;
        };
        let role = role_name(role);
        let class_name = class.name.as_str();

        for dec in &class.decorator_list {
            if !is_synthetic_decorator(self.source, dec) {
                continue;
            }
            let Expr::Name(name) = &dec.expression else {
                continue;
            };
            if matches!(name.id.as_str(), "data_class" | "frozen_data_class") {
                self.errors.push(format!(
                    "`data class` on {role} `{class_name}`: the framework synthesizes its own \
                     constructor and metaclass, and stacking `@dataclass` on it breaks at \
                     runtime. use a plain `class`"
                ));
            }
        }

        for stmt in &class.body {
            let Stmt::FunctionDef(func) = stmt else {
                continue;
            };
            // same detection as the init_method lowering: the parser marks the
            // `init(...)` shorthand with a synthetic `__init_method__` decorator
            if func.decorator_list.iter().any(
                |dec| matches!(&dec.expression, Expr::Name(n) if n.id.as_str() == "__init_method__"),
            ) {
                self.errors.push(format!(
                    "`init` shorthand in {role} `{class_name}`: the framework synthesizes \
                     `__init__` from field declarations. declare fields instead, or write \
                     `def __init__` explicitly to override it"
                ));
            }
        }
    }
}

impl<'a> Visitor<'a> for GateVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.check_class(class);
        }
        walk_stmt(self, stmt);
    }
}

impl TypeAwarePass for FrameworksPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut visitor = GateVisitor {
            source: self.source,
            types,
            errors: Vec::new(),
        };
        for stmt in stmts {
            visitor.visit_stmt(stmt);
        }
        ctx.errors.append(&mut visitor.errors);
    }
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ty_project::{ProjectMetadata, TestDb};

    use crate::{Config, transpile_typed};

    /// a project db whose `/sp` directory resolves as site-packages, holding
    /// a mock pydantic package (`BaseModel` must be *defined* in
    /// `pydantic.main` on a third-party search path to be recognized)
    fn pydantic_db(files: &[(&str, &str)]) -> TestDb {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/proj"),
        ));
        db.write_file(
            "/sp/pydantic/__init__.pyi",
            "from pydantic.main import BaseModel as BaseModel\n",
        )
        .expect("write file failed");
        db.write_file("/sp/pydantic/main.pyi", "class BaseModel: ...\n")
            .expect("write file failed");
        for (path, src) in files {
            db.write_file(path, src).expect("write file failed");
        }
        db.init_program_with_site_packages(["/sp"])
            .expect("program init failed");
        db
    }

    fn transpile_result(db: &TestDb, path: &str) -> Result<String, String> {
        let file = system_path_to_file(db, path).expect("file not in db");
        transpile_typed(db, file, &Config::test_default()).map_err(|err| err.to_string())
    }

    /// a project db with a mock django package in site-packages (`Model` must
    /// be *defined* in `django.db.models.base` on a third-party search path)
    fn django_db(files: &[(&str, &str)]) -> TestDb {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/proj"),
        ));
        db.write_file("/sp/django/__init__.pyi", "")
            .expect("write file failed");
        db.write_file("/sp/django/db/__init__.pyi", "")
            .expect("write file failed");
        db.write_file(
            "/sp/django/db/models/__init__.pyi",
            "from django.db.models.base import Model as Model\n",
        )
        .expect("write file failed");
        db.write_file("/sp/django/db/models/base.pyi", "class Model: ...\n")
            .expect("write file failed");
        for (path, src) in files {
            db.write_file(path, src).expect("write file failed");
        }
        db.init_program_with_site_packages(["/sp"])
            .expect("program init failed");
        db
    }

    /// a project db with a mock sqlalchemy package in site-packages
    /// (`DeclarativeBase` must be *defined* in `sqlalchemy.orm.decl_api`,
    /// `Mapped` in `sqlalchemy.orm.base`, both on a third-party search path)
    fn sqlalchemy_db(files: &[(&str, &str)]) -> TestDb {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/proj"),
        ));
        db.write_file("/sp/sqlalchemy/__init__.pyi", "")
            .expect("write file failed");
        db.write_file(
            "/sp/sqlalchemy/orm/__init__.pyi",
            "from sqlalchemy.orm.decl_api import DeclarativeBase as DeclarativeBase\n\
             from sqlalchemy.orm.base import Mapped as Mapped\n",
        )
        .expect("write file failed");
        db.write_file(
            "/sp/sqlalchemy/orm/decl_api.pyi",
            "class DeclarativeBase: ...\n",
        )
        .expect("write file failed");
        db.write_file("/sp/sqlalchemy/orm/base.pyi", "class Mapped[T]: ...\n")
            .expect("write file failed");
        for (path, src) in files {
            db.write_file(path, src).expect("write file failed");
        }
        db.init_program_with_site_packages(["/sp"])
            .expect("program init failed");
        db
    }

    #[test]
    fn data_class_modifier_on_sqlalchemy_model_rejected() {
        let db = sqlalchemy_db(&[(
            "/proj/models.by",
            "from sqlalchemy.orm import DeclarativeBase\ndata class User(DeclarativeBase):\n    name: str\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`data class` on sqlalchemy declarative model `User`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn init_shorthand_in_sqlalchemy_model_rejected() {
        let db = sqlalchemy_db(&[(
            "/proj/models.by",
            "from sqlalchemy.orm import DeclarativeBase\nclass User(DeclarativeBase):\n    init(self, let name: str)\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`init` shorthand in sqlalchemy declarative model `User`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn plain_sqlalchemy_model_transpiles() {
        let db = sqlalchemy_db(&[(
            "/proj/models.by",
            "from sqlalchemy.orm import DeclarativeBase, Mapped\nclass User(DeclarativeBase):\n    name: Mapped[str]\n",
        )]);
        let out = transpile_result(&db, "/proj/models.by").expect("plain model should transpile");
        assert!(out.contains("class User(DeclarativeBase):"), "got:\n{out}");
    }

    #[test]
    fn soundness_guards_in_sqlalchemy_model_methods() {
        // `Config::test_default()` disables soundness; opt in to pin that the
        // guard lands inside the method body while the class structure and
        // field declarations stay untouched (conformance matrix: soundness ✓)
        let db = sqlalchemy_db(&[(
            "/proj/models.by",
            "from sqlalchemy.orm import DeclarativeBase, Mapped\n\nclass User(DeclarativeBase):\n    name: Mapped[str]\n\n    def greet(self, prefix: str) -> str:\n        return prefix + \"x\"\n",
        )]);
        let file = system_path_to_file(&db, "/proj/models.by").expect("file not in db");
        let config = Config {
            lazy_imports: false,
            soundness: crate::config::SoundnessPositions::all(),
            ..Config::default()
        };
        let out = transpile_typed(&db, file, &config).expect("model should transpile");
        assert!(
            out.contains("class User(DeclarativeBase):"),
            "class structure should survive, got:\n{out}"
        );
        assert!(
            out.contains("name: Mapped[str]"),
            "field declaration should survive, got:\n{out}"
        );
        assert!(
            out.contains("_soundness_check(prefix, str)"),
            "parameter guard should land in the method body, got:\n{out}"
        );
    }

    #[test]
    fn data_class_modifier_on_django_model_rejected() {
        let db = django_db(&[(
            "/proj/models.by",
            "from django.db import models\ndata class Author(models.Model):\n    name: str\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`data class` on django model `Author`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn init_shorthand_in_django_model_rejected() {
        let db = django_db(&[(
            "/proj/models.by",
            "from django.db import models\nclass Author(models.Model):\n    init(self, let name: str)\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`init` shorthand in django model `Author`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn plain_django_model_transpiles() {
        let db = django_db(&[(
            "/proj/models.by",
            "from django.db import models\nclass Author(models.Model):\n    pass\n",
        )]);
        let out = transpile_result(&db, "/proj/models.by").expect("plain model should transpile");
        assert!(out.contains("class Author(models.Model):"), "got:\n{out}");
    }

    #[test]
    fn soundness_guards_in_django_model_methods() {
        // `Config::test_default()` disables soundness; opt in to pin that the
        // guard lands inside the method body while the class structure and
        // field declarations stay untouched (conformance matrix: soundness ✓)
        let db = django_db(&[(
            "/proj/models.by",
            "from django.db import models\n\nclass Author(models.Model):\n    name = models.CharField(max_length=100)\n\n    def display(self, prefix: str) -> str:\n        return prefix + \"x\"\n",
        )]);
        let file = system_path_to_file(&db, "/proj/models.by").expect("file not in db");
        let config = Config {
            lazy_imports: false,
            // the `parameters` entry checks are opt-in; they are the position
            // that lands a guard inside an otherwise precisely-typed method
            soundness: crate::config::SoundnessPositions::all(),
            ..Config::default()
        };
        let out = transpile_typed(&db, file, &config).expect("model should transpile");
        assert!(
            out.contains("class Author(models.Model):"),
            "class structure should survive, got:\n{out}"
        );
        assert!(
            out.contains("name = models.CharField(max_length=100)"),
            "field declaration should survive, got:\n{out}"
        );
        assert!(
            out.contains("_soundness_check(prefix, str)"),
            "parameter guard should land in the method body, got:\n{out}"
        );
    }

    #[test]
    fn data_class_modifier_on_pydantic_model_rejected() {
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\ndata class User(BaseModel):\n    name: str\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`data class` on pydantic model `User`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn init_shorthand_in_pydantic_model_rejected() {
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\nclass User(BaseModel):\n    init(self, let name: str)\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`init` shorthand in pydantic model `User`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn inherited_model_is_gated_too() {
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\nclass Base(BaseModel): ...\ndata class User(Base):\n    name: str\n",
        )]);
        let err = transpile_result(&db, "/proj/models.by").expect_err("gate should reject");
        assert!(
            err.contains("`data class` on pydantic model `User`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ordinary_class_untouched_with_pydantic_imported() {
        let db = pydantic_db(&[(
            "/proj/models.by",
            "import pydantic\ndata class Point:\n    x: int\n",
        )]);
        let out =
            transpile_result(&db, "/proj/models.by").expect("ordinary class should transpile");
        assert!(
            out.contains("@dataclass(slots=True)"),
            "data class should lower normally, got:\n{out}"
        );
    }

    #[test]
    fn plain_fields_in_pydantic_model_transpile() {
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\nclass User(BaseModel):\n    name: str\n",
        )]);
        let out = transpile_result(&db, "/proj/models.by").expect("plain model should transpile");
        assert!(out.contains("class User(BaseModel):"), "got:\n{out}");
    }

    #[test]
    fn explicit_dunder_init_in_pydantic_model_allowed() {
        // pydantic itself allows overriding `__init__` explicitly; only the
        // shorthand (which hides the override) is gated
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\nclass User(BaseModel):\n    def __init__(self, name: str):\n        super().__init__(name=name)\n",
        )]);
        transpile_result(&db, "/proj/models.by").expect("explicit __init__ should transpile");
    }

    // ---- conformance-matrix pins (compat cells marked ✓): the lowering is
    // already runtime-correct inside a pydantic model, and these tests pin
    // that it stays so ----

    #[test]
    fn generic_pydantic_model_never_reified() {
        // the reified-generics pass wraps *functions* whose type parameters
        // reach a value position — it never visits a class, so a generic model
        // is never given a `@generic` wrapper. pydantic's own
        // `__class_getitem__` reifies `Box[int](...)` natively
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\nclass Box[T](BaseModel):\n    value: T\n",
        )]);
        let file = system_path_to_file(&db, "/proj/models.by").expect("file not in db");
        let config = Config {
            min_version: ruff_python_ast::PythonVersion::PY313,
            ..Config::test_default()
        };
        let out = transpile_typed(&db, file, &config).expect("generic model should transpile");
        assert!(
            !out.contains("@generic"),
            "a model class must never be wrapped with `@generic`, got:\n{out}"
        );
    }

    #[test]
    fn class_body_field_default_untouched_in_pydantic_model() {
        // the mutable-defaults transform rewrites *function* argument defaults;
        // a class-body field default is left alone (pydantic deep-copies field
        // defaults itself). pin that the `= []` survives verbatim
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\nclass Bag(BaseModel):\n    items: list[int] = []\n",
        )]);
        let out = transpile_result(&db, "/proj/models.by").expect("model should transpile");
        assert!(
            out.contains("items: list[int] = []"),
            "class-body field default must survive untouched, got:\n{out}"
        );
    }

    #[test]
    fn soundness_guards_in_pydantic_model_methods() {
        // `Config::test_default()` disables soundness; opt in to pin that the
        // guard lands inside the method body while the class structure and
        // field declarations stay untouched (conformance matrix: soundness ✓)
        let db = pydantic_db(&[(
            "/proj/models.by",
            "from pydantic import BaseModel\n\nclass User(BaseModel):\n    name: str\n\n    def greet(self, prefix: str) -> str:\n        return prefix + self.name\n",
        )]);
        let file = system_path_to_file(&db, "/proj/models.by").expect("file not in db");
        let config = Config {
            lazy_imports: false,
            soundness: crate::config::SoundnessPositions::all(),
            ..Config::default()
        };
        let out = transpile_typed(&db, file, &config).expect("model should transpile");
        assert!(
            out.contains("class User(BaseModel):"),
            "class structure should survive, got:\n{out}"
        );
        assert!(
            out.contains("name: str"),
            "field declaration should survive, got:\n{out}"
        );
        assert!(
            out.contains("_soundness_check(prefix, str)"),
            "parameter guard should land in the method body, got:\n{out}"
        );
    }
}
