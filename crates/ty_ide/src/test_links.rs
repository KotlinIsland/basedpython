//! which tests exercise which code: what an editor's *go to test* and *go to
//! test subject* ask
//!
//! a test exercises a subject when it calls it, the way call hierarchy sees a
//! call: a function or a method called by name or as an attribute, or a class
//! constructed. so a test's subjects are its outgoing calls, and a subject's
//! tests are the collected tests among its incoming calls. nothing here goes by
//! names: `test_circle.by` is not the test of `circle.by` unless something in
//! it calls something in `circle.by`, and a test of `circle.by` is found
//! wherever it is and whatever it is called
//!
//! which declarations are tests is pytest's collection, as
//! [`collected_pytest_tests`] models it. a subject is the project's own code: a
//! file the project checks that is not a test module (one tests are collected
//! from) and not a `conftest`, which holds fixtures for tests rather than code
//! under test

use ruff_db::files::{File, system_path_to_file};
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::source_text;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_diagnostics::Edit;
use ruff_python_ast::{self as ast};
use ruff_python_codegen::Stylist;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_python_core::ProgramFile;
use ty_python_core::program::Program;
use ty_python_semantic::{CollectedTest, collected_pytest_tests};

use crate::importer::{ImportRequest, Importer};
use crate::{CallHierarchyItem, Db, SymbolKind, incoming_calls, outgoing_calls};

/// a test, or a subject a test exercises
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestLink {
    /// for a test, the `::`-separated names after the file in its pytest node
    /// id: `TestTotal::test_total`. for a subject, its name within its module,
    /// with the classes it is declared in: `Circle.area`
    pub name: String,
    pub kind: SymbolKind,
    /// the module the test or the subject is declared in
    pub module: Option<String>,
    pub file: File,
    /// the whole declaration
    pub full_range: TextRange,
    /// its name
    pub focus_range: TextRange,
}

/// the subjects a test exercises: the project's own functions, methods and
/// classes it calls, each once, in the order the test first calls them
///
/// at `offset` in `file` is a test, a class tests are collected under, or
/// anywhere else in a module tests are collected from, which stand for that
/// test, every test collected under that class, and every test in the module.
/// `None` when no test is collected from `file`
pub fn test_subjects(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<Vec<TestLink>> {
    let tests = collected_pytest_tests(db, file);
    if tests.is_empty() {
        return None;
    }
    let program = file.program(db);
    let project_files = db.project().files(db);
    let mut subject_files: FxHashMap<File, bool> = FxHashMap::default();
    let mut seen = FxHashSet::default();
    let mut subjects = Vec::new();
    for test in tests_at(&tests, offset) {
        let mut calls = outgoing_calls(db, file, test.focus_range.start());
        calls.sort_by_key(|call| call.from_ranges.iter().map(Ranged::start).min());
        for call in calls {
            let callee = call.to;
            if !matches!(
                callee.kind,
                SymbolKind::Class
                    | SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Constructor
            ) {
                continue;
            }
            let is_subject_file = *subject_files.entry(callee.file).or_insert_with(|| {
                callee.file != file.file(db)
                    && project_files.contains(callee.file)
                    && !is_test_support(db, ProgramFile::new(db, callee.file, program))
            });
            if !is_subject_file || !seen.insert((callee.file, callee.selection_range)) {
                continue;
            }
            subjects.push(subject_link(db, program, callee));
        }
    }
    Some(subjects)
}

/// the tests that exercise a subject, and where a new one would go
#[derive(Debug, Clone)]
pub struct SubjectTests {
    /// the collected tests that call the subject, by file and then in source
    /// order
    pub tests: Vec<TestLink>,
    /// how a test of the subject would be added. `None` when the subject's
    /// module has no name to import it by, is not a `.by` or a `.py` file, or is
    /// itself a test module
    pub new_test: Option<NewTest>,
}

/// where a new test of a subject goes, and what it imports
#[derive(Debug, Clone)]
pub struct NewTest {
    /// the test module: `test_` and the last part of the subject's module name,
    /// written in what the subject's file is written in, `.by` or `.py`, in the
    /// directory most of the project's test modules are in, or in `tests` under
    /// the project root when it has none
    pub path: SystemPathBuf,
    /// whether that module exists already
    pub exists: bool,
    /// the subject's module
    pub module: String,
    /// the name to import from [`Self::module`]: the subject's own, or for a
    /// method the class it is declared in. `None` for a module, which a test
    /// imports whole
    pub import: Option<String>,
    /// a name for the test function: `test_` and the subject's name in snake
    /// case, `test_circle_area` for `Circle.area`
    pub test_name: String,
    /// for a test module that exists, the edit that imports [`Self::import`]
    /// into it; `None` when it imports it already, or there is nothing to
    /// import
    pub import_edit: Option<Edit>,
}

/// the tests that exercise the subject at `offset` in `file`, and where a new
/// one would go
///
/// the subject is the function, method or class whose declaration `offset` is
/// in, the outermost one a test could call: a function local to another stands
/// for the function it is local to. a class stands for itself and the methods it
/// declares, so a test that only calls a method of the class counts. anywhere
/// else, the subject is the module, standing for every function and class it
/// declares
pub fn subject_tests(db: &dyn Db, file: ProgramFile<'_>, offset: TextSize) -> SubjectTests {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let subject = Subject::at(parsed.suite(), offset);
    let program = file.program(db);

    let mut collected: FxHashMap<File, Vec<CollectedTest>> = FxHashMap::default();
    let mut seen = FxHashSet::default();
    let mut tests = Vec::new();
    for name in subject.names() {
        for call in incoming_calls(db, file, name.start()) {
            let caller = call.from;
            let caller_tests = collected.entry(caller.file).or_insert_with(|| {
                collected_pytest_tests(db, ProgramFile::new(db, caller.file, program))
            });
            let Some(test) = caller_tests
                .iter()
                .find(|test| test.focus_range == caller.selection_range)
            else {
                continue;
            };
            if !seen.insert((caller.file, test.focus_range)) {
                continue;
            }
            tests.push(TestLink {
                name: node_id(test),
                kind: if test.classes.is_empty() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Method
                },
                module: caller.detail,
                file: caller.file,
                full_range: test.full_range,
                focus_range: test.focus_range,
            });
        }
    }
    tests.sort_by(|a, b| {
        a.file
            .path(db)
            .as_str()
            .cmp(b.file.path(db).as_str())
            .then(a.focus_range.start().cmp(&b.focus_range.start()))
    });

    SubjectTests {
        tests,
        new_test: new_test(db, file, &subject),
    }
}

/// what a request for tests is about
enum Subject<'ast> {
    Function(&'ast ast::StmtFunctionDef),
    Method {
        class: &'ast ast::StmtClassDef,
        method: &'ast ast::StmtFunctionDef,
    },
    Class(&'ast ast::StmtClassDef),
    Module(&'ast [ast::Stmt]),
}

impl<'ast> Subject<'ast> {
    /// the subject whose declaration `offset` is in
    ///
    /// a method of a class nested in another class is the nested class's; a
    /// class or a function nested in a function is the function's, since only
    /// the function is reachable from a test
    fn at(suite: &'ast [ast::Stmt], offset: TextSize) -> Self {
        let mut subject = Subject::Module(suite);
        let mut body = suite;
        loop {
            let Some(statement) = body
                .iter()
                .find(|statement| statement.range().contains_inclusive(offset))
            else {
                return subject;
            };
            match statement {
                ast::Stmt::ClassDef(class) => {
                    subject = Subject::Class(class);
                    body = &class.body;
                }
                ast::Stmt::FunctionDef(function) => {
                    return match subject {
                        Subject::Class(class) => Subject::Method {
                            class,
                            method: function,
                        },
                        _ => Subject::Function(function),
                    };
                }
                _ => return subject,
            }
        }
    }

    /// the names to find calls to: of the subject, and for a class or a module
    /// of what it declares
    fn names(&self) -> Vec<TextRange> {
        fn class_names(class: &ast::StmtClassDef, names: &mut Vec<TextRange>) {
            names.push(class.name.range());
            for statement in &class.body {
                if let ast::Stmt::FunctionDef(method) = statement {
                    names.push(method.name.range());
                }
            }
        }
        let mut names = Vec::new();
        match self {
            Subject::Function(function)
            | Subject::Method {
                method: function, ..
            } => names.push(function.name.range()),
            Subject::Class(class) => class_names(class, &mut names),
            Subject::Module(suite) => {
                for statement in *suite {
                    match statement {
                        ast::Stmt::ClassDef(class) => class_names(class, &mut names),
                        ast::Stmt::FunctionDef(function) => names.push(function.name.range()),
                        _ => {}
                    }
                }
            }
        }
        names
    }

    /// the name a test imports the subject by, from its module
    fn import(&self) -> Option<&'ast str> {
        match self {
            Subject::Function(function) => Some(function.name.as_str()),
            Subject::Method { class, .. } | Subject::Class(class) => Some(class.name.as_str()),
            Subject::Module(_) => None,
        }
    }

    /// what a test of the subject is called after: `None` for a module, which is
    /// named for its file
    fn test_name(&self) -> Option<String> {
        match self {
            Subject::Function(function) => Some(snake_case(function.name.as_str())),
            Subject::Method { class, method } => Some(format!(
                "{}_{}",
                snake_case(class.name.as_str()),
                snake_case(method.name.as_str())
            )),
            Subject::Class(class) => Some(snake_case(class.name.as_str())),
            Subject::Module(_) => None,
        }
    }
}

/// the tests at `offset`: the test whose declaration it is in, else every test
/// collected under the innermost class it is in, else every test there is
fn tests_at(tests: &[CollectedTest], offset: TextSize) -> Vec<&CollectedTest> {
    if let Some(test) = tests
        .iter()
        .find(|test| test.full_range.contains_inclusive(offset))
    {
        return vec![test];
    }
    let class = tests
        .iter()
        .flat_map(|test| &test.classes)
        .filter(|class| class.full_range.contains_inclusive(offset))
        .min_by_key(|class| class.full_range.len());
    match class {
        Some(class) => tests
            .iter()
            .filter(|test| {
                test.classes
                    .iter()
                    .any(|under| under.full_range == class.full_range)
            })
            .collect(),
        None => tests.iter().collect(),
    }
}

/// a test's node id after its file: its classes and its name, `::`-separated
fn node_id(test: &CollectedTest) -> String {
    let mut id = String::new();
    for class in &test.classes {
        id.push_str(&class.name);
        id.push_str("::");
    }
    id.push_str(&test.name);
    id
}

/// whether `file` is for tests rather than under test: tests are collected from
/// it, or it is a `conftest`
fn is_test_support(db: &dyn Db, file: ProgramFile<'_>) -> bool {
    file.file(db)
        .path(db)
        .as_system_path()
        .and_then(SystemPath::file_stem)
        .is_some_and(|stem| stem == "conftest")
        || !collected_pytest_tests(db, file).is_empty()
}

/// a subject as a [`TestLink`], named with the classes it is declared in
fn subject_link(db: &dyn Db, program: Program<'_>, item: CallHierarchyItem) -> TestLink {
    let parsed =
        parsed_module(db, ProgramFile::new(db, item.file, program).python_file(db)).load(db);
    let mut name = enclosing_classes(parsed.suite(), item.full_range).join(".");
    if !name.is_empty() {
        name.push('.');
    }
    name.push_str(item.name.as_str());
    TestLink {
        name,
        kind: item.kind,
        module: item.detail,
        file: item.file,
        full_range: item.full_range,
        focus_range: item.selection_range,
    }
}

/// the names of the classes whose bodies hold the declaration at `range`,
/// outermost first
fn enclosing_classes(suite: &[ast::Stmt], range: TextRange) -> Vec<&str> {
    let mut names = Vec::new();
    let mut body = suite;
    while let Some(class) = body.iter().find_map(|statement| match statement {
        ast::Stmt::ClassDef(class)
            if class.range().contains_range(range) && class.range() != range =>
        {
            Some(class)
        }
        _ => None,
    }) {
        names.push(class.name.as_str());
        body = &class.body;
    }
    names
}

/// where a new test of `subject`, in `file`, goes
fn new_test(db: &dyn Db, file: ProgramFile<'_>, subject: &Subject<'_>) -> Option<NewTest> {
    if is_test_support(db, file) {
        return None;
    }
    let module = ty_module_resolver::file_to_module(db, file.resolver_file(db))?
        .name(db)
        .to_string();
    let stem = module.rsplit('.').next().unwrap_or(&module);
    let test_name = format!(
        "test_{}",
        subject.test_name().unwrap_or_else(|| snake_case(stem))
    );
    // a new test module is written in what the subject is, so a project that writes python goes on
    // writing python; a test module there already is used whatever it is written in, since a `.by`
    // one beside a `.py` one of the same name would shadow it, and the `.by` one of two is the
    // module the name imports
    let extension = file
        .file(db)
        .path(db)
        .extension()
        .filter(|extension| matches!(*extension, "by" | "py"))?;
    let directory = test_directory(db, file);
    let (path, existing) = ["by", "py"]
        .into_iter()
        .find_map(|extension| {
            let path = directory.join(format!("test_{stem}.{extension}"));
            let existing = system_path_to_file(db, &path).ok()?;
            Some((path, Some(existing)))
        })
        .unwrap_or_else(|| (directory.join(format!("test_{stem}.{extension}")), None));
    let import = subject.import().map(str::to_string);

    let test_name = match existing {
        Some(test_file) => unused_name(
            db,
            ProgramFile::new(db, test_file, file.program(db)),
            test_name,
        ),
        None => test_name,
    };
    let import_edit = existing
        .zip(import.as_deref())
        .and_then(|(test_file, name)| {
            import_edit(
                db,
                ProgramFile::new(db, test_file, file.program(db)),
                &module,
                name,
            )
        });
    Some(NewTest {
        path,
        exists: existing.is_some(),
        module,
        import,
        test_name,
        import_edit,
    })
}

/// `name`, or when the module binds it at the top level already, `name` with the
/// first of `_2`, `_3` and so on that it does not, so that a new test does not
/// redefine one that is there
fn unused_name(db: &dyn Db, file: ProgramFile<'_>, name: String) -> String {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let bound: FxHashSet<&str> = parsed
        .suite()
        .iter()
        .filter_map(|statement| match statement {
            ast::Stmt::FunctionDef(function) => Some(function.name.as_str()),
            ast::Stmt::ClassDef(class) => Some(class.name.as_str()),
            _ => None,
        })
        .collect();
    if !bound.contains(name.as_str()) {
        return name;
    }
    (2..)
        .map(|suffix| format!("{name}_{suffix}"))
        .find(|candidate| !bound.contains(candidate.as_str()))
        .unwrap_or(name)
}

/// the directory most of the project's test modules are in, nearest the
/// project root among those holding as many; `tests` under the project root
/// when no test is collected anywhere
fn test_directory(db: &dyn Db, file: ProgramFile<'_>) -> SystemPathBuf {
    let program = file.program(db);
    let mut counts: FxHashMap<SystemPathBuf, usize> = FxHashMap::default();
    for project_file in &db.project().files(db) {
        let Some(directory) = project_file
            .path(db)
            .as_system_path()
            .and_then(SystemPath::parent)
        else {
            continue;
        };
        if collected_pytest_tests(db, ProgramFile::new(db, project_file, program)).is_empty() {
            continue;
        }
        *counts.entry(directory.to_path_buf()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|(a, a_count), (b, b_count)| {
            a_count
                .cmp(b_count)
                .then_with(|| b.components().count().cmp(&a.components().count()))
                .then_with(|| b.cmp(a))
        })
        .map_or_else(
            || db.project().root(db).join("tests"),
            |(directory, _)| directory,
        )
}

/// the edit that imports `name` from `module` into `file`, or `None` when it is
/// imported there already
fn import_edit(db: &dyn Db, file: ProgramFile<'_>, module: &str, name: &str) -> Option<Edit> {
    let parsed: ParsedModuleRef = parsed_module(db, file.python_file(db)).load(db);
    let source = source_text(db, file.file(db));
    let stylist = Stylist::from_tokens(parsed.tokens(), source.as_str());
    let importer = Importer::new(db, &stylist, file, source.as_str(), &parsed);
    // in scope at the end of the module, which is where a new test goes
    let end = TextSize::of(source.as_str());
    let members = importer.members_in_scope_at(parsed.syntax().into(), end);
    importer
        .import(ImportRequest::import_from(module, name), &members)
        .import()
        .cloned()
}

/// `CamelCase` and `snake_case` names in snake case: `CircleArea` is
/// `circle_area`, `HTTPServer` is `http_server`
fn snake_case(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let mut snake = String::with_capacity(name.len() + 4);
    for (index, &character) in characters.iter().enumerate() {
        if character.is_uppercase() {
            let previous = index.checked_sub(1).map(|previous| characters[previous]);
            let next = characters.get(index + 1);
            let starts_word = previous.is_some_and(|previous| {
                previous.is_lowercase()
                    || previous.is_ascii_digit()
                    || (previous.is_uppercase() && next.is_some_and(|next| next.is_lowercase()))
            });
            if starts_word && !snake.ends_with('_') {
                snake.push('_');
            }
            snake.extend(character.to_lowercase());
        } else {
            snake.push(character);
        }
    }
    snake
}

#[cfg(test)]
mod tests {
    use crate::tests::CursorTest;
    use insta::assert_snapshot;
    use ruff_db::source::source_text;
    use ruff_source_file::LineIndex;

    use super::{TestLink, snake_case};

    const SHAPES: &str = "\
class Shape:
    def area(self) -> float:
        return 0.0

class Circle(Shape):
    def __init__(self, radius: float) -> None:
        self.radius = radius

    def area(self) -> float:
        return 3.14 * self.radius ** 2

def total(shapes: list[Shape]) -> float:
    return sum(shape.area() for shape in shapes)

def unused() -> None: ...
";

    const TESTS: &str = "\
from shapes import Circle, total

def helper() -> Circle:
    return Circle(1.0)

def test_area() -> None:
    assert Circle(1.0).area() > 3

def test_total() -> None:
    assert total([helper()]) > 3

class TestShapes:
    def test_circle(self) -> None:
        assert Circle(2.0).radius == 2.0
";

    impl CursorTest {
        fn links(&self, links: &[TestLink]) -> String {
            links
                .iter()
                .map(|link| {
                    let source = source_text(&self.db, link.file);
                    let line =
                        LineIndex::from_source_text(&source).line_index(link.focus_range.start());
                    format!(
                        "{} {} ({}) {}:{line}",
                        link.kind.to_string(),
                        link.name,
                        link.module.as_deref().unwrap_or("-"),
                        link.file.path(&self.db).as_str(),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn test_subjects(&self) -> String {
            match super::test_subjects(
                &self.db,
                self.program_file(self.cursor.file),
                self.cursor.offset,
            ) {
                None => "None".to_string(),
                Some(subjects) => self.links(&subjects),
            }
        }

        fn subject_tests(&self) -> String {
            let answer = super::subject_tests(
                &self.db,
                self.program_file(self.cursor.file),
                self.cursor.offset,
            );
            let new_test = match &answer.new_test {
                None => "no new test".to_string(),
                Some(new_test) => format!(
                    "new test {} {} in {}{} importing {} from {}{}",
                    new_test.test_name,
                    if new_test.exists { "added" } else { "created" },
                    new_test.path,
                    "",
                    new_test.import.as_deref().unwrap_or("nothing"),
                    new_test.module,
                    match &new_test.import_edit {
                        Some(edit) => format!(" with {:?}", edit.content().unwrap_or_default()),
                        None => String::new(),
                    },
                ),
            };
            format!("{}\n{new_test}", self.links(&answer.tests))
        }
    }

    fn test(shapes: &str, tests: &str) -> CursorTest {
        CursorTest::builder()
            .source("shapes.py", shapes)
            .source("tests/test_shapes.py", tests)
            .build()
    }

    #[test]
    fn a_test_exercises_what_it_calls_in_the_order_it_first_calls_it() {
        let test = test(
            SHAPES,
            &TESTS.replace("def test_area", "def test_<CURSOR>area"),
        );
        assert_snapshot!(test.test_subjects(), @"
        Class Circle (shapes) /shapes.py:5
        Method Circle.area (shapes) /shapes.py:9
        ");
    }

    #[test]
    fn a_test_class_exercises_what_its_tests_call() {
        let test = test(
            SHAPES,
            &TESTS.replace("class TestShapes", "class Test<CURSOR>Shapes"),
        );
        assert_snapshot!(test.test_subjects(), @"Class Circle (shapes) /shapes.py:5");
    }

    #[test]
    fn a_test_module_exercises_what_all_its_tests_call_and_not_its_helpers() {
        let test = test(SHAPES, &TESTS.replace("from shapes", "from <CURSOR>shapes"));
        assert_snapshot!(test.test_subjects(), @"
        Class Circle (shapes) /shapes.py:5
        Method Circle.area (shapes) /shapes.py:9
        Function total (shapes) /shapes.py:12
        ");
    }

    #[test]
    fn a_module_no_test_is_collected_from_has_no_test_subjects() {
        let test = test(&SHAPES.replace("def total", "def <CURSOR>total"), TESTS);
        assert_snapshot!(test.test_subjects(), @"None");
    }

    #[test]
    fn a_method_is_exercised_by_the_tests_that_call_it() {
        let test = test(
            &SHAPES.replace("        return 3.14", "        return 3.<CURSOR>14"),
            TESTS,
        );
        assert_snapshot!(test.subject_tests(), @"
        Function test_area (tests.test_shapes) /tests/test_shapes.py:6
        new test test_circle_area added in /tests/test_shapes.py importing Circle from shapes
        ");
    }

    #[test]
    fn a_class_is_exercised_by_the_tests_that_construct_it_or_call_its_methods() {
        let test = test(
            &SHAPES.replace("class Circle", "class Cir<CURSOR>cle"),
            TESTS,
        );
        assert_snapshot!(test.subject_tests(), @"
        Function test_area (tests.test_shapes) /tests/test_shapes.py:6
        Method TestShapes::test_circle (tests.test_shapes) /tests/test_shapes.py:13
        new test test_circle added in /tests/test_shapes.py importing Circle from shapes
        ");
    }

    #[test]
    fn a_function_no_test_calls_is_exercised_by_none() {
        let test = test(&SHAPES.replace("def unused", "def un<CURSOR>used"), TESTS);
        assert_snapshot!(test.subject_tests(), @r#"

        new test test_unused added in /tests/test_shapes.py importing unused from shapes with ", unused"
        "#);
    }

    #[test]
    fn a_module_is_exercised_by_the_tests_that_call_anything_it_declares() {
        let test = test(&format!("# shapes<CURSOR>\n{SHAPES}"), TESTS);
        assert_snapshot!(test.subject_tests(), @"
        Function test_area (tests.test_shapes) /tests/test_shapes.py:6
        Function test_total (tests.test_shapes) /tests/test_shapes.py:9
        Method TestShapes::test_circle (tests.test_shapes) /tests/test_shapes.py:13
        new test test_shapes added in /tests/test_shapes.py importing nothing from shapes
        ");
    }

    #[test]
    fn a_new_test_goes_beside_the_project_tests_in_a_module_named_for_the_subjects() {
        let test = CursorTest::builder()
            .source(
                "geometry.by",
                "def perimeter<CURSOR>(sides: list[float]) -> float: ...\n",
            )
            .source("tests/test_shapes.py", "def test_nothing() -> None: ...\n")
            .build();
        assert_snapshot!(test.subject_tests(), @"

        new test test_perimeter created in /tests/test_geometry.by importing perimeter from geometry
        ");
    }

    #[test]
    fn a_new_test_in_a_test_module_that_exists_comes_with_the_import_it_needs() {
        let test = CursorTest::builder()
            .source(
                "geometry.py",
                "def perimeter<CURSOR>(sides: list[float]) -> float: ...\n",
            )
            .source(
                "tests/test_geometry.py",
                "import math\n\ndef test_nothing() -> None: ...\n",
            )
            .build();
        assert_snapshot!(test.subject_tests(), @r#"

        new test test_perimeter added in /tests/test_geometry.py importing perimeter from geometry with "from geometry import perimeter\n"
        "#);
    }

    #[test]
    fn a_new_test_in_a_test_module_that_imports_the_subject_needs_no_import() {
        let test = CursorTest::builder()
            .source(
                "geometry.py",
                "def perimeter<CURSOR>(sides: list[float]) -> float: ...\n",
            )
            .source(
                "tests/test_geometry.py",
                "from geometry import perimeter\n\ndef test_nothing() -> None: ...\n",
            )
            .build();
        assert_snapshot!(test.subject_tests(), @"

        new test test_perimeter added in /tests/test_geometry.py importing perimeter from geometry
        ");
    }

    /// a test module written in the other language is the one a new test goes in: a `.by`
    /// module beside a `.py` one of the same name would shadow it
    #[test]
    fn a_new_test_goes_in_the_test_module_there_whatever_it_is_written_in() {
        let test = CursorTest::builder()
            .source(
                "geometry.by",
                "def perimeter<CURSOR>(sides: list[float]) -> float: ...\n",
            )
            .source(
                "tests/test_geometry.py",
                "from geometry import perimeter\n\ndef test_nothing() -> None: ...\n",
            )
            .build();
        assert_snapshot!(test.subject_tests(), @"

        new test test_perimeter added in /tests/test_geometry.py importing perimeter from geometry
        ");
    }

    #[test]
    fn a_new_test_is_named_so_as_not_to_redefine_one_there() {
        let test = CursorTest::builder()
            .source("geometry.py", "def perimeter<CURSOR>(sides: list[float]) -> float: ...\n")
            .source(
                "tests/test_geometry.py",
                "from geometry import perimeter\n\ndef test_perimeter() -> None: ...\n\ndef test_perimeter_2() -> None: ...\n",
            )
            .build();
        assert_snapshot!(test.subject_tests(), @"");
    }

    #[test]
    fn with_no_tests_anywhere_a_new_test_goes_in_tests() {
        let test = CursorTest::builder()
            .source(
                "geometry.py",
                "def perimeter<CURSOR>(sides: list[float]) -> float: ...\n",
            )
            .build();
        assert_snapshot!(test.subject_tests(), @"

        new test test_perimeter created in /tests/test_geometry.py importing perimeter from geometry
        ");
    }

    #[test]
    fn a_test_module_is_no_subject_to_add_a_test_of() {
        let test = test(SHAPES, &TESTS.replace("def helper", "def hel<CURSOR>per"));
        assert_snapshot!(test.subject_tests(), @"
        Function test_total (tests.test_shapes) /tests/test_shapes.py:9
        no new test
        ");
    }

    #[test]
    fn names_go_into_snake_case() {
        assert_eq!(snake_case("CircleArea"), "circle_area");
        assert_eq!(snake_case("HTTPServer"), "http_server");
        assert_eq!(snake_case("area"), "area");
        assert_eq!(snake_case("total_area"), "total_area");
    }
}
