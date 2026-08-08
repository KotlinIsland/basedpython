//! the code lenses a django project puts above its own files
//!
//! two unrelated things share the request. a template gets told which views
//! render it, which is a fact only the project-wide scan in [`super::project`]
//! holds. a python file gets the `manage.py` invocations that apply to it — the
//! test it declares, the migration it is, the app whose models it declares —
//! each of which is a command a developer would otherwise type out by hand, and
//! each of which the server already knows exactly.
//!
//! nothing here answers for a file django has no role for. an ordinary python
//! module in a django project gets no lens, and every file of a project with no
//! `manage.py` gets no runnable, since a runnable that cannot be run is worse
//! than none at all.

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_db::system::SystemPath;
use ruff_python_ast::{self as ast, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::file_to_module;
use ty_project::Db;

use super::project::{
    self, MIGRATIONS_PACKAGE, MODELS_MODULE, TEST_METHOD_PREFIX, is_test_class, manage_file,
};
use super::resolve;

/// where a lens with nothing above it sits
///
/// a file-level lens belongs on the first line, and a template's first line is
/// as likely as not to be a `{% extends %}` the lens must not cover.
const FILE_HEADER: TextRange = TextRange::new(
    ruff_text_size::TextSize::new(0),
    ruff_text_size::TextSize::new(0),
);

/// something a code lens offers to do
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DjangoLensAction {
    /// go to these definitions
    ///
    /// several are as ordinary as one — a template rendered by two views has two
    /// places to go — so this is always a list, and the client decides whether to
    /// jump or to offer a choice.
    Navigate(Vec<DjangoLensTarget>),
    /// run `manage.py` with these arguments
    Run(Vec<CompactString>),
}

/// a place a lens can navigate to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DjangoLensTarget {
    pub file: File,
    pub range: TextRange,
}

/// one lens of a file
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DjangoCodeLens {
    /// the range the lens sits above
    pub range: TextRange,
    pub title: String,
    pub action: DjangoLensAction,
}

/// the lenses of `file`, read as a django template
///
/// the only thing a template can be told from outside itself is what renders it,
/// so a template nothing renders gets nothing rather than a lens saying so.
pub(super) fn template_code_lenses(db: &dyn Db, file: File) -> Vec<DjangoCodeLens> {
    let Some(name) = resolve::template_name(db, file) else {
        return Vec::new();
    };

    let mut views: Vec<(CompactString, DjangoLensTarget)> = Vec::new();

    for context in project::template_contexts(db, db.project())
        .iter()
        .filter(|context| context.template == name)
    {
        let Some(view) = &context.view else { continue };
        let Some(module) = file_to_module(db, view.file) else {
            continue;
        };

        let dotted = format!("{}.{}", module.name(db), view.path).to_compact_string();
        let target = DjangoLensTarget {
            file: view.file,
            range: view.range,
        };

        if !views.iter().any(|(name, _)| *name == dotted) {
            views.push((dotted, target));
        }
    }

    if views.is_empty() {
        return Vec::new();
    }

    // the scan walks the project's files in no order at all, and a lens that
    // reorders itself between two requests reads as a lens that changed
    views.sort_by(|(left, _), (right, _)| left.cmp(right));

    let names: Vec<&str> = views.iter().map(|(name, _)| name.as_str()).collect();

    vec![DjangoCodeLens {
        range: FILE_HEADER,
        title: format!("rendered by {}", names.join(", ")),
        action: DjangoLensAction::Navigate(views.into_iter().map(|(_, target)| target).collect()),
    }]
}

/// the lenses of `file`, read as one of the project's python modules
///
/// every one of these runs `manage.py`, so a project without one has none of
/// them. a module django gives no role to has none either, whether or not the
/// project has a `manage.py`.
pub(super) fn python_code_lenses(db: &dyn Db, file: File) -> Vec<DjangoCodeLens> {
    if manage_file(db, db.project()).is_none() {
        return Vec::new();
    }
    let Some(path) = file.path(db).as_system_path() else {
        return Vec::new();
    };

    let mut lenses = Vec::new();

    // a migration is addressed by path rather than by module name because it does
    // not have one: django numbers migrations, and `0001_initial` is no python
    // identifier however importable the file is by other means
    if let Some((app, migration)) = split_path(path, MIGRATIONS_PACKAGE)
        && let Some(migration) = migration
    {
        lenses.push(runnable(
            format!("migrate to {migration}"),
            ["migrate", app, migration],
        ));
        lenses.push(runnable(
            "show sql".to_string(),
            ["sqlmigrate", app, migration],
        ));
    }

    // a `models.py` of an app that declares no model has nothing to make a
    // migration from, and django's own `models.py` scaffold is exactly that file
    if let Some((app, _)) = split_path(path, MODELS_MODULE)
        && !project::django_classes_in_file(db, file).is_empty()
    {
        lenses.push(runnable(
            format!("make migrations for {app}"),
            ["makemigrations", app],
        ));
    }

    // a test, unlike the two above, is addressed by the dotted module path the
    // test runner imports it by, so this half does need the module resolver
    if let Some(module) = file_to_module(db, file) {
        lenses.extend(test_lenses(db, file, &module.name(db).to_string()));
    }

    lenses
}

/// the lenses above every test class and test method of `file`
fn test_lenses(db: &dyn Db, file: File, module: &str) -> Vec<DjangoCodeLens> {
    // a test class reaches `unittest.TestCase` through its bases, and every base
    // it is followed through has to be resolved. a file that declares no class at
    // all is the common case and costs a parse this query has already paid for
    let parsed = parsed_module(db, file).load(db);

    let mut lenses = Vec::new();

    for class in parsed.suite().iter().filter_map(class_def) {
        if !is_test_class(db, file, class) {
            continue;
        }

        let dotted = format!("{module}.{}", class.name);
        lenses.push(DjangoCodeLens {
            range: class.name.range(),
            title: format!("run {}", class.name),
            action: DjangoLensAction::Run(arguments(["test", &dotted])),
        });

        for method in class.body.iter().filter_map(function_def) {
            if !method.name.starts_with(TEST_METHOD_PREFIX) {
                continue;
            }

            lenses.push(DjangoCodeLens {
                range: method.name.range(),
                title: "run test".to_string(),
                action: DjangoLensAction::Run(arguments([
                    "test",
                    &format!("{dotted}.{}", method.name),
                ])),
            });
        }
    }

    lenses
}

/// a file-level lens that runs `manage.py`
fn runnable<S: AsRef<str>>(
    title: String,
    arguments: impl IntoIterator<Item = S>,
) -> DjangoCodeLens {
    DjangoCodeLens {
        range: FILE_HEADER,
        title,
        action: DjangoLensAction::Run(self::arguments(arguments)),
    }
}

fn arguments<S: AsRef<str>>(arguments: impl IntoIterator<Item = S>) -> Vec<CompactString> {
    arguments
        .into_iter()
        .map(|argument| argument.as_ref().to_compact_string())
        .collect()
}

fn class_def(stmt: &Stmt) -> Option<&ast::StmtClassDef> {
    match stmt {
        Stmt::ClassDef(class) => Some(class),
        _ => None,
    }
}

fn function_def(stmt: &Stmt) -> Option<&ast::StmtFunctionDef> {
    match stmt {
        Stmt::FunctionDef(function) => Some(function),
        _ => None,
    }
}

/// the app label the file at `path` belongs to, and what it is called below `at`
///
/// django labels an app by the last component of its package, so a
/// `myproject/apps/blog/migrations/0001_initial.py` is the `blog` app's, and the
/// migration it names is `0001_initial`. the split component may be a directory
/// or the file's own stem, which is what lets one function answer both for an
/// app's `migrations` package and for its `models` module — and a file that *is*
/// the split component names nothing below it.
fn split_path<'a>(path: &'a SystemPath, at: &str) -> Option<(&'a str, Option<&'a str>)> {
    let stem = path.file_stem()?;
    let directory = path.parent()?;

    // the file *is* the split, as an app's `models.py` is
    if stem == at {
        // a root, or a windows drive prefix, names no app
        return Some((directory.file_name()?, None));
    }

    // the file is directly inside the split, as an app's migrations are. anything
    // further down is in some directory of its own that django does not load
    if directory.file_name() == Some(at) {
        return Some((
            directory.parent()?.file_name()?,
            // django's own `__init__` is the package, not a member of it
            (stem != "__init__").then_some(stem),
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{SystemPath, split_path};
    use crate::django_template::tests::TemplateTest;

    /// the mock django a lens test is written against
    ///
    /// `TestCase` descends from `unittest`'s, which is what a test class is
    /// recognised by, and `Model` is what makes a `models.py` worth a
    /// `makemigrations`.
    const DJANGO: &[(&str, &str)] = &[
        ("django/__init__.py", ""),
        ("django/db/__init__.py", ""),
        ("django/db/models/__init__.py", "class Model: ...\n"),
        (
            "django/test/__init__.py",
            "
            import unittest

            class SimpleTestCase(unittest.TestCase): ...

            class TestCase(SimpleTestCase): ...
            ",
        ),
    ];

    /// the entry point and settings every runnable is gated on
    const PROJECT: &[(&str, &str)] = &[
        (
            "manage.py",
            "
            import os

            os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
            ",
        ),
        ("project/__init__.py", ""),
        (
            "project/settings.py",
            "
            INSTALLED_APPS = ['blog']

            TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]
            ",
        ),
        ("blog/__init__.py", ""),
    ];

    /// a whole django project, with `sources` written last so the file under test
    /// is one of them
    fn project(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = DJANGO.to_vec();
        all.extend_from_slice(PROJECT);
        all.extend_from_slice(sources);

        TemplateTest::new(&all)
    }

    /// the same, with no `manage.py` for a runnable to be run through
    fn without_entry_point(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = DJANGO.to_vec();
        all.extend(
            PROJECT
                .iter()
                .filter(|(path, _)| *path != "manage.py")
                .copied(),
        );
        all.extend_from_slice(sources);

        TemplateTest::new(&all)
    }

    #[test]
    fn a_template_names_the_view_that_renders_it() {
        let test = project(&[
            (
                "blog/views.py",
                "
                from django.shortcuts import render

                def post(request):
                    return render(request, 'blog/post.html', {})
                ",
            ),
            ("blog/templates/blog/post.html", "<p>a</p>\n"),
        ]);

        assert_eq!(
            test.lenses(),
            ["rendered by blog.views.post -> /blog/views.py:post"]
        );
    }

    #[test]
    fn a_template_several_views_render_names_every_one_of_them() {
        let test = project(&[
            (
                "blog/views.py",
                "
                from django.shortcuts import render

                def post(request):
                    return render(request, 'blog/post.html', {})

                def preview(request):
                    return render(request, 'blog/post.html', {})
                ",
            ),
            (
                "blog/admin_views.py",
                "
                from django.shortcuts import render

                def moderate(request):
                    return render(request, 'blog/post.html', {})
                ",
            ),
            ("blog/templates/blog/post.html", "<p>a</p>\n"),
        ]);

        assert_eq!(
            test.lenses(),
            [
                "rendered by blog.admin_views.moderate, blog.views.post, blog.views.preview \
                 -> /blog/admin_views.py:moderate, /blog/views.py:post, /blog/views.py:preview"
            ]
        );
    }

    #[test]
    fn a_class_based_view_is_named_by_its_class() {
        let test = project(&[
            (
                "blog/views.py",
                "
                from django.views.generic import DetailView

                class BookDetail(DetailView):
                    template_name = 'blog/detail.html'
                ",
            ),
            ("blog/templates/blog/detail.html", "<p>a</p>\n"),
        ]);

        assert_eq!(
            test.lenses(),
            ["rendered by blog.views.BookDetail -> /blog/views.py:BookDetail"]
        );
    }

    #[test]
    fn a_template_nothing_renders_gets_no_lens_rather_than_an_empty_one() {
        let test = project(&[
            ("blog/views.py", "from django.shortcuts import render\n"),
            ("blog/templates/blog/orphan.html", "<p>a</p>\n"),
        ]);

        assert!(test.lenses().is_empty(), "got {:?}", test.lenses());
    }

    #[test]
    fn a_test_class_and_each_of_its_test_methods_are_runnable() {
        let test = project(&[(
            "blog/tests.py",
            "
            from django.test import TestCase

            class BookTest(TestCase):
                def setUp(self): ...

                def test_detail(self): ...

                def test_listing(self): ...
            ",
        )]);

        assert_eq!(
            test.lenses(),
            [
                "run BookTest -> manage.py test blog.tests.BookTest",
                "run test -> manage.py test blog.tests.BookTest.test_detail",
                "run test -> manage.py test blog.tests.BookTest.test_listing",
            ]
        );
    }

    #[test]
    fn a_test_class_is_recognised_through_a_base_of_the_projects_own() {
        let test = project(&[
            (
                "blog/base_tests.py",
                "
                from django.test import TestCase

                class SiteTest(TestCase): ...
                ",
            ),
            (
                "blog/tests.py",
                "
                from blog.base_tests import SiteTest

                class BookTest(SiteTest):
                    def test_detail(self): ...
                ",
            ),
        ]);

        assert_eq!(
            test.lenses(),
            [
                "run BookTest -> manage.py test blog.tests.BookTest",
                "run test -> manage.py test blog.tests.BookTest.test_detail",
            ]
        );
    }

    #[test]
    fn a_project_with_no_entry_point_offers_no_runnable() {
        let test = without_entry_point(&[(
            "blog/tests.py",
            "
            from django.test import TestCase

            class BookTest(TestCase):
                def test_detail(self): ...
            ",
        )]);

        assert!(test.lenses().is_empty(), "got {:?}", test.lenses());
    }

    #[test]
    fn an_ordinary_python_module_gets_no_lens_at_all() {
        let test = project(&[(
            "blog/views.py",
            "
            from django.shortcuts import render

            class Helper:
                def test_shaped_but_no_test(self): ...

            def post(request):
                return render(request, 'blog/post.html', {})
            ",
        )]);

        assert!(test.lenses().is_empty(), "got {:?}", test.lenses());
    }

    #[test]
    fn a_migration_is_runnable_both_ways() {
        let test = project(&[
            ("blog/migrations/__init__.py", ""),
            ("blog/migrations/0001_initial.py", "operations = []\n"),
        ]);

        assert_eq!(
            test.lenses(),
            [
                "migrate to 0001_initial -> manage.py migrate blog 0001_initial",
                "show sql -> manage.py sqlmigrate blog 0001_initial",
            ]
        );
    }

    #[test]
    fn the_package_a_migration_lives_in_is_not_itself_a_migration() {
        let test = project(&[("blog/migrations/__init__.py", "")]);

        assert!(test.lenses().is_empty(), "got {:?}", test.lenses());
    }

    #[test]
    fn a_models_module_that_declares_a_model_can_make_migrations() {
        let test = project(&[(
            "blog/models.py",
            "
            from django.db import models

            class Book(models.Model): ...
            ",
        )]);

        assert_eq!(
            test.lenses(),
            ["make migrations for blog -> manage.py makemigrations blog"]
        );
    }

    #[test]
    fn a_models_module_that_declares_no_model_has_nothing_to_migrate() {
        let test = project(&[("blog/models.py", "from django.db import models\n")]);

        assert!(test.lenses().is_empty(), "got {:?}", test.lenses());
    }

    #[test]
    fn a_migration_names_the_app_it_belongs_to() {
        assert_eq!(
            split_path(
                SystemPath::new("/src/blog/migrations/0001_initial.py"),
                "migrations"
            ),
            Some(("blog", Some("0001_initial")))
        );
    }

    #[test]
    fn an_app_nested_below_a_package_is_labelled_by_its_own_name() {
        // django's `AppConfig.label` defaults to the last component, not the whole
        // dotted path
        assert_eq!(
            split_path(SystemPath::new("/myproject/apps/blog/models.py"), "models"),
            Some(("blog", None))
        );
    }

    #[test]
    fn a_module_of_a_models_package_still_belongs_to_the_app() {
        assert_eq!(
            split_path(SystemPath::new("/blog/models/book.py"), "models"),
            Some(("blog", Some("book")))
        );
    }

    #[test]
    fn a_module_below_neither_splits_nowhere() {
        assert_eq!(
            split_path(SystemPath::new("/blog/views.py"), "migrations"),
            None
        );
    }

    #[test]
    fn a_module_directly_under_the_root_belongs_to_no_app() {
        assert_eq!(split_path(SystemPath::new("/models.py"), "models"), None);
        assert_eq!(
            split_path(SystemPath::new("/migrations/0001_initial.py"), "migrations"),
            None
        );
    }

    #[test]
    fn a_migration_nested_further_down_is_no_migration() {
        assert_eq!(
            split_path(
                SystemPath::new("/blog/migrations/old/0001_initial.py"),
                "migrations"
            ),
            None
        );
    }
}
