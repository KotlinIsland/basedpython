//! a stub and its runtime module side by side, declaration by declaration, for an editor to go
//! from one to the other
//!
//! a document that is a stub with a runtime module, or a runtime module with a stub, lists each of
//! its declarations the other file declares too, with where the other file declares it. one answer
//! is the whole document, so an editor that marks every such declaration (a gutter icon going to
//! the other side) asks once per pass over the document rather than once per declaration.
//!
//! which declarations pair is [`stub_pairing`]'s: by where they sit among the module's members,
//! every overload and every `sys.version_info` branch included, a stub's class-body `x: int` with
//! the `self.x = ...` a runtime method assigns.
//!
//! of a name's declarations on either side, the ones listed are the ones that declare it rather
//! than rebind it: every `def`, `class`, annotated declaration and import of it, or failing those
//! its first assignment. `x = 0` and a later `x = 1` are one variable, declared once.

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_python_ast::{self as ast, name::Name};
use ruff_text_size::{Ranged, TextRange};
use ty_python_core::ProgramFile;
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_semantic::ResolvedDefinition;
use ty_python_semantic::stub_mapping::{
    PairedMember, resolve_member_definition, stub_counterpart, stub_pairing,
};

pub use ty_python_semantic::stub_mapping::StubRole;

use crate::{Db, NavigationTarget, cached_vendored_root};

/// what a declaration paired with its counterpart declares
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterpartKind {
    Class,
    Function,
    /// a variable, an attribute, a type alias, or an import of something other than a class or a
    /// function
    Variable,
}

/// a declaration of the document the counterpart file declares too
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CounterpartDeclaration {
    /// where it sits among the module's members, dotted: `A.f`
    pub name: String,
    pub kind: CounterpartKind,
    /// the whole declaration, its decorators included, in the document
    pub full_range: TextRange,
    /// the name it declares, in the document
    pub name_range: TextRange,
    /// the counterpart's declarations of it, never empty: several for the overloads of a stub's
    /// function, or a declaration in each branch of a version check
    pub targets: Vec<NavigationTarget>,
}

/// the stub a runtime module is described by, or the runtime module a stub describes, and the
/// declarations the two share
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentCounterparts {
    /// what the document is
    pub role: StubRole,
    /// the file on the other side
    pub counterparts: Vec<File>,
    /// the document's declarations its counterpart declares too, in source order
    pub declarations: Vec<CounterpartDeclaration>,
}

/// `file`'s counterpart and the declarations they share. `None` for a file that is neither a stub
/// with a runtime module nor a runtime module with a stub
pub fn document_counterparts(db: &dyn Db, file: ProgramFile<'_>) -> Option<DocumentCounterparts> {
    let pair = stub_counterpart(db, file, cached_vendored_root(db).as_deref())?;
    let this_module = parsed_module(db, file.python_file(db)).load(db);
    let counterpart = pair.counterpart();
    let counterpart_module = parsed_module(db, counterpart.python_file(db)).load(db);

    let mut collector = Collector {
        db,
        role: pair.role,
        this_module: &this_module,
        counterpart_module: &counterpart_module,
        path: Vec::new(),
        declarations: Vec::new(),
    };
    collector.collect(stub_pairing(db, pair.stub, pair.runtime));

    let mut declarations = collector.declarations;
    declarations.sort_by_key(|declaration| declaration.name_range.start());
    Some(DocumentCounterparts {
        role: pair.role,
        counterparts: vec![counterpart.file(db)],
        declarations,
    })
}

struct Collector<'a, 'db> {
    db: &'db dyn Db,
    role: StubRole,
    this_module: &'a ParsedModuleRef,
    counterpart_module: &'a ParsedModuleRef,
    path: Vec<Name>,
    declarations: Vec<CounterpartDeclaration>,
}

impl<'db> Collector<'_, 'db> {
    fn collect(&mut self, members: &[PairedMember<'db>]) {
        for member in members {
            if !member.is_paired() {
                continue;
            }
            self.path.push(member.name.clone());
            let (this, other) = match self.role {
                StubRole::Stub => (&member.stub, &member.runtime),
                StubRole::Implementation => (&member.runtime, &member.stub),
            };

            let targets: Vec<NavigationTarget> = declaring(self.db, other, true)
                .map(|definition| target(self.db, self.counterpart_module, definition))
                .collect();
            let counterpart_kind = declaring(self.db, other, true)
                .find_map(|definition| own_kind(self.db, definition));
            if !targets.is_empty() {
                let name = self
                    .path
                    .iter()
                    .map(Name::as_str)
                    .collect::<Vec<_>>()
                    .join(".");
                for definition in declaring(self.db, this, false) {
                    let name_range = name_range(self.db, self.this_module, definition);
                    // a declaration the source never spells, as an `init(...)`'s `__init__`,
                    // has no name of its own to mark
                    if name_range.is_empty() {
                        continue;
                    }
                    self.declarations.push(CounterpartDeclaration {
                        name: name.clone(),
                        kind: kind(self.db, definition, counterpart_kind),
                        full_range: definition.full_range(self.db, self.this_module).range(),
                        name_range,
                        targets: targets.clone(),
                    });
                }
            }

            self.collect(&member.members);
            self.path.pop();
        }
    }
}

/// of a name's declarations, the ones that declare it: every `def`, `class`, annotated
/// declaration and import of it, or, where there is none of those, its first assignment
///
/// a star import binds every name its module exports at the `*`, which is no place to mark a
/// declaration at, so it is only ever a target, and only where nothing else binds the name
fn declaring<'a, 'db>(
    db: &'db dyn Db,
    definitions: &'a [Definition<'db>],
    as_target: bool,
) -> impl Iterator<Item = Definition<'db>> + 'a {
    let declares = |definition: &Definition<'db>| {
        matches!(
            definition.kind(db),
            DefinitionKind::Function(_)
                | DefinitionKind::Class(_)
                | DefinitionKind::TypeAlias(_)
                | DefinitionKind::AnnotatedAssignment(_)
                | DefinitionKind::Import(_)
                | DefinitionKind::ImportFrom(_)
                | DefinitionKind::ImportFromSubmodule(_)
        )
    };
    let any_declares = definitions.iter().any(declares);
    let first_binding = (!any_declares)
        .then(|| {
            definitions
                .iter()
                .find(|definition| {
                    matches!(definition.kind(db), DefinitionKind::Assignment(_))
                        || (as_target
                            && matches!(definition.kind(db), DefinitionKind::StarImport(_)))
                })
                .copied()
        })
        .flatten();
    definitions
        .iter()
        .copied()
        .filter(move |definition| any_declares && declares(definition))
        .chain(first_binding)
}

/// the name `definition` declares: a `def`'s or a `class`'s name, a variable, the `x` of an
/// attribute `self.x`, the name an import binds
pub(crate) fn name_range(
    db: &dyn Db,
    module: &ParsedModuleRef,
    definition: Definition<'_>,
) -> TextRange {
    match definition.kind(db) {
        DefinitionKind::Assignment(assignment) => target_name_range(assignment.target(module)),
        DefinitionKind::AnnotatedAssignment(assignment) => {
            target_name_range(assignment.target(module))
        }
        DefinitionKind::Import(import) => alias_name_range(import.alias(module)),
        DefinitionKind::ImportFrom(import) => alias_name_range(import.alias(module)),
        kind => kind.target_range(module),
    }
}

fn target_name_range(target: &ast::Expr) -> TextRange {
    match target {
        ast::Expr::Attribute(attribute) => attribute.attr.range(),
        target => target.range(),
    }
}

/// the name an import binds: what follows `as`, or the module or member imported, of which
/// `import a.b` binds the `a`
fn alias_name_range(alias: &ast::Alias) -> TextRange {
    match &alias.asname {
        Some(asname) => asname.range(),
        None => {
            let name = alias.name.range();
            let first = alias.name.id.split('.').next().unwrap_or_default();
            TextRange::at(name.start(), ruff_text_size::TextSize::of(first))
        }
    }
}

fn target(db: &dyn Db, module: &ParsedModuleRef, definition: Definition<'_>) -> NavigationTarget {
    NavigationTarget::create(
        db,
        definition.file(db),
        name_range(db, module, definition),
        definition.full_range(db, module).range(),
    )
}

/// what `definition` is when it says so itself: a `def` or a `class`
fn own_kind(db: &dyn Db, definition: Definition<'_>) -> Option<CounterpartKind> {
    match definition.kind(db) {
        DefinitionKind::Function(_) => Some(CounterpartKind::Function),
        DefinitionKind::Class(_) => Some(CounterpartKind::Class),
        _ => None,
    }
}

/// what `definition` declares. an import declares what it imports, which is read off the
/// counterpart where that is a `def` or a `class`, and otherwise found by following the import
fn kind(
    db: &dyn Db,
    definition: Definition<'_>,
    counterpart_kind: Option<CounterpartKind>,
) -> CounterpartKind {
    if let Some(kind) = own_kind(db, definition) {
        return kind;
    }
    match definition.kind(db) {
        DefinitionKind::Import(_)
        | DefinitionKind::ImportFrom(_)
        | DefinitionKind::ImportFromSubmodule(_) => counterpart_kind
            .or_else(|| {
                resolve_member_definition(db, definition)
                    .into_iter()
                    .find_map(|resolved| match resolved {
                        ResolvedDefinition::Definition(resolved) => own_kind(db, resolved),
                        _ => None,
                    })
            })
            .unwrap_or(CounterpartKind::Variable),
        _ => CounterpartKind::Variable,
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use insta::assert_snapshot;
    use ruff_db::files::File;
    use ruff_db::source::{line_index, source_text};
    use ruff_python_trivia::textwrap::dedent;

    use super::*;
    use crate::tests::{CursorTest, CursorTestBuilder};

    impl CursorTest {
        /// the cursor's document's counterparts, a declaration a line: its dotted name, its kind,
        /// where its name is, and where each target's name is
        fn counterparts(&self) -> String {
            let Some(counterparts) =
                document_counterparts(&self.db, self.program_file(self.cursor.file))
            else {
                return "none".to_string();
            };
            let mut out = String::new();
            writeln!(
                out,
                "{:?} of {}",
                counterparts.role,
                counterparts
                    .counterparts
                    .iter()
                    .map(|file| file.path(&self.db).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .unwrap();
            for declaration in &counterparts.declarations {
                let targets = declaration
                    .targets
                    .iter()
                    .map(|target| self.location(target.file(), target.focus_range()))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    out,
                    "{} {:?} {} -> {targets}",
                    declaration.name,
                    declaration.kind,
                    self.location(self.cursor.file, declaration.name_range),
                )
                .unwrap();
            }
            out
        }

        /// `path:line:column` of `range`'s start, and its text
        fn location(&self, file: File, range: TextRange) -> String {
            let source = source_text(&self.db, file);
            let position = line_index(&self.db, file).line_column(range.start(), &source);
            format!(
                "{}:{}:{} `{}`",
                file.path(&self.db),
                position.line,
                position.column,
                &source[range]
            )
        }
    }

    fn pair(stub: (&str, &str), runtime: (&str, &str)) -> CursorTestBuilder {
        let mut builder = CursorTest::builder();
        builder.source(stub.0, stub.1).source(runtime.0, runtime.1);
        builder
    }

    #[test]
    fn a_runtime_module_lists_what_its_stub_declares() {
        let test = pair(
            (
                "mod.pyi",
                "
                class A:
                    class Inner: ...
                    def f(self) -> int: ...
                def g(a: int) -> None: ...
                X: int
                def only_in_stub() -> None: ...
                ",
            ),
            (
                "mod.py",
                "
                class <CURSOR>A:
                    class Inner:
                        pass
                    def f(self):
                        return 1
                    def only_at_runtime(self): ...
                def g(a):
                    pass
                X = 1
                X = 2
                ",
            ),
        )
        .build();

        assert_snapshot!(test.counterparts(), @"
        Implementation of /mod.pyi
        A Class /mod.py:2:7 `A` -> /mod.pyi:2:7 `A`
        A.Inner Class /mod.py:3:11 `Inner` -> /mod.pyi:3:11 `Inner`
        A.f Function /mod.py:5:9 `f` -> /mod.pyi:4:9 `f`
        g Function /mod.py:8:5 `g` -> /mod.pyi:5:5 `g`
        X Variable /mod.py:10:1 `X` -> /mod.pyi:6:1 `X`
        ");
    }

    #[test]
    fn a_stub_lists_what_its_runtime_module_declares() {
        let test = pair(
            (
                "mod.pyi",
                "
                class <CURSOR>A:
                    def f(self) -> int: ...
                X: int
                ",
            ),
            ("mod.py", "class A:\n    def f(self): ...\nX = 1\n"),
        )
        .build();

        assert_snapshot!(test.counterparts(), @"
        Stub of /mod.py
        A Class /mod.pyi:2:7 `A` -> /mod.py:1:7 `A`
        A.f Function /mod.pyi:3:9 `f` -> /mod.py:2:9 `f`
        X Variable /mod.pyi:4:1 `X` -> /mod.py:3:1 `X`
        ");
    }

    #[test]
    fn overloads_pair_with_the_one_implementation() {
        let stub = "
            from typing import overload
            @overload
            def f(x: int) -> int: ...
            @overload
            def f(x: str) -> str: ...
            ";
        let runtime = "def f(x):\n    return x\n";

        let test = pair(
            ("mod.pyi", &format!("<CURSOR>{}", dedent(stub))),
            ("mod.py", runtime),
        )
        .build();
        assert_snapshot!(test.counterparts(), @"
        Stub of /mod.py
        f Function /mod.pyi:4:5 `f` -> /mod.py:1:5 `f`
        f Function /mod.pyi:6:5 `f` -> /mod.py:1:5 `f`
        ");

        let test = pair(
            ("mod.pyi", stub),
            ("mod.py", &format!("<CURSOR>{}", dedent(runtime))),
        )
        .build();
        assert_snapshot!(test.counterparts(), @"
        Implementation of /mod.pyi
        f Function /mod.py:1:5 `f` -> /mod.pyi:4:5 `f`, /mod.pyi:6:5 `f`
        ");
    }

    #[test]
    fn every_version_branch_pairs() {
        let test = pair(
            (
                "mod.pyi",
                "
                import sys
                if sys.version_info >= (3, 12):
                    class A:
                        def f(self) -> int: ...
                else:
                    class A:
                        def f(self) -> str: ...
                ",
            ),
            ("mod.py", "<CURSOR>class A:\n    def f(self): ...\n"),
        )
        .build();

        assert_snapshot!(test.counterparts(), @"
        Implementation of /mod.pyi
        A Class /mod.py:1:7 `A` -> /mod.pyi:4:11 `A`, /mod.pyi:7:11 `A`
        A.f Function /mod.py:2:9 `f` -> /mod.pyi:5:13 `f`, /mod.pyi:8:13 `f`
        ");
    }

    #[test]
    fn a_class_body_declaration_pairs_with_an_attribute_a_method_assigns() {
        let stub = "
            class A:
                x: int
                y: str
            ";
        let runtime = "
            class A:
                def __init__(self):
                    self.x = 1
                    self.x = 2
                def reset(self):
                    self.y = ''
            ";

        let test = pair(
            ("mod.pyi", stub),
            ("mod.py", &format!("<CURSOR>{}", dedent(runtime))),
        )
        .build();
        assert_snapshot!(test.counterparts(), @"
        Implementation of /mod.pyi
        A Class /mod.py:2:7 `A` -> /mod.pyi:2:7 `A`
        A.x Variable /mod.py:4:14 `x` -> /mod.pyi:3:5 `x`
        A.y Variable /mod.py:7:14 `y` -> /mod.pyi:4:5 `y`
        ");

        let test = pair(
            ("mod.pyi", &format!("<CURSOR>{}", dedent(stub))),
            ("mod.py", runtime),
        )
        .build();
        assert_snapshot!(test.counterparts(), @"
        Stub of /mod.py
        A Class /mod.pyi:2:7 `A` -> /mod.py:2:7 `A`
        A.x Variable /mod.pyi:3:5 `x` -> /mod.py:4:14 `x`
        A.y Variable /mod.pyi:4:5 `y` -> /mod.py:7:14 `y`
        ");
    }

    #[test]
    fn a_re_export_pairs_with_the_runtime_import() {
        let test = CursorTest::builder()
            .source(
                "pkg/__init__.pyi",
                "
                from typing import <CURSOR>Any
                from ._impl import f as f
                from ._impl import C
                __all__ = ['C']
                ",
            )
            .source(
                "pkg/__init__.py",
                "from typing import Any\nfrom ._impl import f, C\n",
            )
            .source("pkg/_impl.py", "def f(): ...\nclass C: ...\n")
            .build();

        assert_snapshot!(test.counterparts(), @"
        Stub of /pkg/__init__.py
        f Function /pkg/__init__.pyi:3:25 `f` -> /pkg/__init__.py:2:20 `f`
        C Class /pkg/__init__.pyi:4:20 `C` -> /pkg/__init__.py:2:23 `C`
        ");
    }

    #[test]
    fn a_class_with_type_parameters_pairs_its_members() {
        let test = pair(
            ("mod.pyi", "class Box[T]:\n    def get(self) -> T: ...\n"),
            ("mod.py", "<CURSOR>class Box:\n    def get(self): ...\n"),
        )
        .build();

        assert_snapshot!(test.counterparts(), @"
        Implementation of /mod.pyi
        Box Class /mod.py:1:7 `Box` -> /mod.pyi:1:7 `Box`
        Box.get Function /mod.py:2:9 `get` -> /mod.pyi:2:9 `get`
        ");
    }

    #[test]
    fn basedpython_stubs_pair_with_either_language() {
        for (stub, runtime) in [
            ("mod.byi", "mod.by"),
            ("mod.byi", "mod.py"),
            ("mod.pyi", "mod.by"),
        ] {
            let test = pair(
                (stub, "def f(a: int) -> None: ...\n"),
                (runtime, "<CURSOR>def f(a):\n    pass\n"),
            )
            .build();
            assert_eq!(
                test.counterparts(),
                format!(
                    "Implementation of /{stub}\nf Function /{runtime}:1:5 `f` -> /{stub}:1:5 `f`\n"
                ),
                "{stub} and {runtime}"
            );
        }
    }

    /// a `-stubs` package describes the package of the name without the suffix
    #[test]
    fn a_stub_package_pairs_with_its_runtime_package() {
        let test = CursorTest::builder()
            .with_site_packages()
            .source("main.py", "import foo<CURSOR>\n")
            .site_packages("foo/__init__.py", "def f(a):\n    pass\n")
            .site_packages("foo-stubs/__init__.pyi", "def f(a: int) -> None: ...\n")
            .build();
        let foo = ruff_db::files::system_path_to_file(&test.db, "/site-packages/foo/__init__.py")
            .expect("the runtime package");
        let counterparts =
            document_counterparts(&test.db, test.program_file(foo)).expect("a stub package");
        assert_eq!(counterparts.role, StubRole::Implementation);
        assert_eq!(
            counterparts.counterparts[0].path(&test.db).as_str(),
            "/site-packages/foo-stubs/__init__.pyi"
        );
        assert_eq!(counterparts.declarations.len(), 1);
    }

    /// in a package that declares its `.by` sources authoritative, the `.py` generated beside a
    /// `.by` is not the module, and nothing describes it; the `.by` is, and the stub describes it
    #[test]
    fn the_python_generated_beside_an_authoritative_by_has_no_counterpart() {
        let test = CursorTest::builder()
            .source("pkg/by.typed", "")
            .source("pkg/__init__.py", "")
            .source("pkg/mod.by", "def f(a):\n    pass\n")
            .source("pkg/mod.py", "<CURSOR>def f(a):\n    pass\n")
            .source("pkg/mod.byi", "def f(a: int) -> None: ...\n")
            .build();
        assert_snapshot!(test.counterparts(), @"none");
        let by = ruff_db::files::system_path_to_file(&test.db, "/pkg/mod.by").expect("the source");
        assert_eq!(
            document_counterparts(&test.db, test.program_file(by)).map(|c| c.role),
            Some(StubRole::Implementation)
        );
    }

    #[test]
    fn a_module_without_a_stub_has_no_counterpart() {
        let test = CursorTest::builder()
            .source("mod.py", "<CURSOR>def f(): ...\n")
            .build();
        assert_snapshot!(test.counterparts(), @"none");
    }

    #[test]
    fn a_stub_without_a_runtime_module_has_no_counterpart() {
        let test = CursorTest::builder()
            .source("mod.pyi", "<CURSOR>def f() -> None: ...\n")
            .build();
        assert_snapshot!(test.counterparts(), @"none");
    }
}
