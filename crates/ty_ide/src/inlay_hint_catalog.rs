//! what each kind of inlay hint is, for a client to list them
//!
//! a client that lets its user switch each kind of hint, or draw it its own way, lists the kinds
//! in a settings page of its own, and needs a name, a line saying what the hint shows, and a
//! picture of it for each — before any file is open. the kinds are this crate's, and so is what
//! they mean, so the list is kept here beside them, as exhaustive matches over the kinds: a kind
//! added without its entry does not compile, and a client never again lists a kind by a name the
//! server has dropped or misses one it added.
//!
//! the picture is a short program with the hints drawn on it. the hints are not written here:
//! [`InlayHintExamples`] computes them, with every kind enabled, by this crate's own
//! [`inlay_hints()`], on a database that holds nothing but the examples, so what a client shows in
//! its preview is what this server draws in a file.

use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{InMemorySystem, MemoryFileSystem, SystemPathBuf};
use ruff_text_size::{TextRange, TextSize};
use ty_project::{ProjectDatabase, ProjectMetadata, SemanticDb as _};

use crate::{InlayHint, InlayHintKind, InlayHintSettings, TemplateInlayHintKind, inlay_hints};

/// the families a client groups the kinds under, in the order it shows them
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum_macros::EnumIter)]
pub enum InlayHintFamily {
    Types,
    Generics,
    Arguments,
    Effects,
    Ui,
    Templates,
}

impl InlayHintFamily {
    /// what a client knows the family by
    pub const fn id(self) -> &'static str {
        match self {
            Self::Types => "types",
            Self::Generics => "generics",
            Self::Arguments => "arguments",
            Self::Effects => "effects",
            Self::Ui => "ui",
            Self::Templates => "templates",
        }
    }

    /// what a client calls the family
    pub const fn title(self) -> &'static str {
        match self {
            Self::Types => "Types",
            Self::Generics => "Generics",
            Self::Arguments => "Arguments",
            Self::Effects => "Declarations and effects",
            Self::Ui => "basedpython-ui",
            Self::Templates => "Django templates",
        }
    }
}

/// which code a hint belongs with, which decides the side of it a caret typed at its offset lands
/// on, and the side a selection takes it with
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InlayHintAttachment {
    /// the hint completes the code before it: `: int`, `-> int`, `raises E`, `[int]`
    Preceding,
    /// the hint introduces the code after it: a parameter's name, a modifier the declaration does
    /// not spell
    Following,
}

/// the files a kind of hint is drawn in
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InlayHintFileKind {
    /// python and basedpython source
    Source,
    /// django templates
    Template,
}

/// what a kind of hint is, as a client lists it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlayHintAbout {
    /// a short name, in sentence case
    pub title: &'static str,
    pub family: InlayHintFamily,
    /// one or two plain sentences saying what the hint shows and when it appears
    pub description: &'static str,
    pub attaches_to: InlayHintAttachment,
    /// a short program that shows the hint at least once, `None` for a kind no standalone
    /// program can show
    pub example: Option<&'static str>,
    /// what the example is written in: basedpython, but for a hint only python files show
    pub example_language: InlayHintExampleLanguage,
    /// the stub the example's hints are drawn from, written beside it, for a hint a runtime
    /// module takes from its stub
    pub example_stub: Option<&'static str>,
}

/// the language of a kind's example
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InlayHintExampleLanguage {
    BasedPython,
    /// for a hint basedpython files never show, as numeric promotion, which basedpython does not
    /// have
    Python,
}

impl InlayHintExampleLanguage {
    const fn extension(self) -> &'static str {
        match self {
            Self::BasedPython => "by",
            Self::Python => "py",
        }
    }
}

impl InlayHintKind {
    /// what this kind of hint is, as a client lists it
    const fn about(self) -> InlayHintAbout {
        use InlayHintAttachment::{Following, Preceding};
        use InlayHintFamily::{Arguments, Effects, Generics, Types, Ui};

        const fn about(
            title: &'static str,
            family: InlayHintFamily,
            attaches_to: InlayHintAttachment,
            description: &'static str,
            example: &'static str,
        ) -> InlayHintAbout {
            InlayHintAbout {
                title,
                family,
                description,
                attaches_to,
                example: Some(example),
                example_language: InlayHintExampleLanguage::BasedPython,
                example_stub: None,
            }
        }

        match self {
            Self::VariableType => about(
                "Variable types",
                Types,
                Preceding,
                "The type of a variable, after a name bound without an annotation.",
                "def parse(line: str):\n    return line.split(\",\")\n\nfields = parse(\"a,b\")\n",
            ),
            Self::InferredReturnType => about(
                "Return types",
                Types,
                Preceding,
                "What a function returns, after a def that leaves its return annotation out.",
                "def area(r: float):\n    return 3.14 * r * r\n",
            ),
            Self::LambdaParameterType => about(
                "Lambda parameter types",
                Types,
                Preceding,
                "The type of a lambda's parameter, as the call the lambda is passed to gives it.",
                "squares = map(lambda n: n * n, [1, 2, 3])\n",
            ),
            Self::InheritedParameterType => about(
                "Inherited parameter types",
                Types,
                Preceding,
                "The type an unannotated parameter takes from the method it overrides, or the \
                 overloads it implements.",
                "class Shape:\n    def scale(self, factor: float) -> Shape: ...\n\n\
                 class Circle(Shape):\n    def scale(self, factor) -> Circle:\n        return self\n",
            ),
            Self::InferredParameterType => about(
                "Inferred parameter types",
                Types,
                Preceding,
                "The bound an unannotated parameter takes from what the function's body does with \
                 it, written as the annotation that would declare it.",
                "def greet(person):\n    name: str = person.name\n    return \"Hello, \" + name\n",
            ),
            Self::PropertyType => about(
                "Property types",
                Types,
                Preceding,
                "The type of a property declaration that leaves its type to its accessors.",
                "class Temperature:\n    celsius: float\n    let fahrenheit\n        \
                 get() = self.celsius * 9 / 5 + 32\n",
            ),
            Self::CallTypeArguments => about(
                "Call type arguments",
                Generics,
                Preceding,
                "The type arguments a call to a generic function or class was specialised with, \
                 after the name called.",
                "def first[T](items: list[T]) -> T:\n    return items[0]\n\n\
                 top = first([3, 1, 2])\n",
            ),
            Self::TypeArgumentName => about(
                "Type argument names",
                Generics,
                Following,
                "The name of the type parameter a positional type argument fills, before the \
                 argument.",
                "scores: dict[str, int] = {}\n",
            ),
            Self::InferredBaseTypeArguments => about(
                "Base type arguments",
                Generics,
                Preceding,
                "The type arguments a generic base written without them takes from the members the \
                 class body overrides.",
                "class Box[T]:\n    item: T\n\nclass IntBox(Box):\n    item = 0\n",
            ),
            Self::Variance => about(
                "Variance",
                Generics,
                Following,
                "The variance inferred for a class's type parameter that does not declare one: \
                 out, in or in out.",
                "class Reader[T]:\n    def read(self) -> T: ...\n",
            ),
            Self::Reification => about(
                "Reification",
                Generics,
                Following,
                "Marks a function's type parameter reified where its body uses the type at \
                 runtime without saying so.",
                "def make[T]() -> T:\n    return T()\n",
            ),
            Self::NumericPromotion => InlayHintAbout {
                example_language: InlayHintExampleLanguage::Python,
                ..about(
                    "Numeric promotions",
                    Generics,
                    Preceding,
                    "The types numeric promotion adds to float and complex in a python file's \
                     annotation, which accept an int, or an int or a float, as well.",
                    "def mean(total: float, count: int) -> float:\n    return total / count\n",
                )
            },
            Self::CallArgumentName => about(
                "Argument names",
                Arguments,
                Following,
                "The name of the parameter a positional argument is passed to, before the \
                 argument.",
                "def connect(host: str, port: int): ...\n\nconnect(\"localhost\", 8080)\n",
            ),
            Self::ImplicitParameter => about(
                "Implicit parameters",
                Arguments,
                Following,
                "The parameters a trailing lambda binds without spelling them: it, and the \
                 receiver named self.",
                "def repeat(times: int, action: (int) -> None): ...\n\nrepeat(3):\n    print(it)\n",
            ),
            Self::ImplicitSelf => about(
                "Implicit self",
                Arguments,
                Following,
                "The self an init(...) declaration binds without spelling it.",
                "class Point:\n    init(x: int, y: int)\n",
            ),
            Self::ImplicitArgument => about(
                "Implicit arguments",
                Arguments,
                Preceding,
                "The arguments a call fills from the context declarations in scope, where the \
                 call leaves them out.",
                "def log(message: str, context level: int): ...\n\n\
                 context verbosity = 2\nlog(\"started\")\n",
            ),
            Self::InheritedDefault => about(
                "Inherited defaults",
                Arguments,
                Preceding,
                "The default a parameter takes from the method it overrides, where the def leaves \
                 it out.",
                "class Greeter:\n    def greet(self, name: str = \"world\") -> str: ...\n\n\
                 class LoudGreeter(Greeter):\n    def greet(self, name: str) -> str:\n        \
                 return name.upper()\n",
            ),
            Self::StubTypes => InlayHintAbout {
                example_language: InlayHintExampleLanguage::Python,
                example_stub: Some(
                    "class Parser:\n    def parse(self, text: str, strict: bool = ...) -> list[str]: ...\n",
                ),
                ..about(
                    "Stub types",
                    Types,
                    Preceding,
                    "The annotations a module's stub declares for a parameter, a return or a \
                     class's bases the module leaves out, where they would be written.",
                    "class Parser:\n    def parse(self, text, strict=False):\n        \
                     return text.split()\n",
                )
            },
            Self::StubOnlyMembers => InlayHintAbout {
                example_language: InlayHintExampleLanguage::Python,
                example_stub: Some(
                    "class Parser:\n    def parse(self, text: str) -> list[str]: ...\n    \
                     def reset(self) -> None: ...\n\ndef parse_file(path: str) -> list[str]: ...\n",
                ),
                ..about(
                    "Stub-only members",
                    Effects,
                    Following,
                    "A member a module's stub declares that the module does not, on lines of its \
                     own where the stub puts it.",
                    "class Parser:\n    def parse(self, text):\n        return text.split()\n",
                )
            },
            Self::Override => about(
                "Override",
                Effects,
                Following,
                "Marks a method override where it overrides a superclass member without saying \
                 so.",
                "class Animal:\n    def speak(self) -> str: ...\n\nclass Dog(Animal):\n    \
                 def speak(self) -> str:\n        return \"woof\"\n",
            ),
            Self::Raises => about(
                "Raises",
                Effects,
                Preceding,
                "The exceptions a function can raise, where it has no raises clause of its own.",
                "def withdraw(balance: int, amount: int) -> int:\n    if amount > balance:\n        \
                 raise ValueError(\"insufficient funds\")\n    return balance - amount\n",
            ),
            Self::RevealedType => about(
                "Revealed types",
                Effects,
                Preceding,
                "The type a reveal_type(...) call reveals, at the end of its line.",
                "reveal_type([1, 2, 3])\n",
            ),
            Self::EnumValue => about(
                "Enum values",
                Effects,
                Preceding,
                "The value an enum member takes where the declaration does not write one.",
                "enum class Color:\n    case Red, Green, Blue\n",
            ),
            Self::Reads => about(
                "State reads",
                Ui,
                Preceding,
                "The observables a composable reads while composing, after its signature: the \
                 state that recomposes it when written.",
                "from basedpython_ui import composable, state, Text\n\n@composable\n\
                 def Counter():\n    let count = state(0)\n    Text(f\"{count.value}\")\n",
            ),
            Self::Stability => about(
                "Unstable parameters",
                Ui,
                Following,
                "Marks a composable's parameter unstable where the runtime cannot compare its \
                 type, so the composable is recomposed every time its caller is.",
                "from basedpython_ui import composable\n\n@composable\n\
                 def TodoList(items: list[str]): ...\n",
            ),
            Self::DerivedDeps => about(
                "Derived dependencies",
                Ui,
                Preceding,
                "What a derived(...) or remember(...) computation depends on, at the end of its \
                 line.",
                "from basedpython_ui import composable, derived, state\n\n@composable\n\
                 def Greeting():\n    let name = state(\"Ada\")\n    \
                 let text = derived(lambda: \"Hi \" + name.value)\n",
            ),
            Self::Invalidates => about(
                "Invalidations",
                Ui,
                Preceding,
                "The composables a state write in a handler recomposes, at the end of the write's \
                 line, or nothing when no one reads what it writes.",
                "from basedpython_ui import composable, state, Button\n\n@composable\n\
                 def Counter():\n    let count = state(0)\n    Button(f\"{count.value}\"):\n        \
                 count.value += 1\n",
            ),
        }
    }
}

impl TemplateInlayHintKind {
    /// what this kind of template hint is, as a client lists it
    ///
    /// neither has an example: a template's hints come from the django project around it, its
    /// views, models and template directories, which no short program is
    const fn about(self) -> InlayHintAbout {
        match self {
            Self::Type => InlayHintAbout {
                title: "Loop binding types",
                family: InlayHintFamily::Templates,
                description: "The type of each element a {% for %} tag binds, after the name it \
                              binds.",
                attaches_to: InlayHintAttachment::Preceding,
                example: None,
                example_language: InlayHintExampleLanguage::BasedPython,
                example_stub: None,
            },
            Self::Template => InlayHintAbout {
                title: "Resolved templates",
                family: InlayHintFamily::Templates,
                description: "The file an {% extends %} or {% include %} tag's template name \
                              resolves to.",
                attaches_to: InlayHintAttachment::Preceding,
                example: None,
                example_language: InlayHintExampleLanguage::BasedPython,
                example_stub: None,
            },
        }
    }
}

/// a kind of hint, of either a source file or a template
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnyInlayHintKind {
    Source(InlayHintKind),
    Template(TemplateInlayHintKind),
}

impl AnyInlayHintKind {
    /// the `inlayHints` option that switches the kind, and what its hints carry as `data.kind`
    pub const fn setting(self) -> &'static str {
        match self {
            Self::Source(kind) => kind.setting(),
            Self::Template(kind) => kind.setting(),
        }
    }

    pub const fn about(self) -> InlayHintAbout {
        match self {
            Self::Source(kind) => kind.about(),
            Self::Template(kind) => kind.about(),
        }
    }

    pub const fn file_kind(self) -> InlayHintFileKind {
        match self {
            Self::Source(_) => InlayHintFileKind::Source,
            Self::Template(_) => InlayHintFileKind::Template,
        }
    }
}

/// every kind of hint, in the order a client lists them: by family, in [`InlayHintFamily`]'s
/// order, and within a family as its kinds read best together
///
/// a test holds it to every kind exactly once, and to its families' order
pub const INLAY_HINT_CATALOG: &[AnyInlayHintKind] = {
    use AnyInlayHintKind::{Source, Template};
    use InlayHintKind as K;
    use TemplateInlayHintKind as T;
    &[
        // types
        Source(K::VariableType),
        Source(K::InferredReturnType),
        Source(K::LambdaParameterType),
        Source(K::InheritedParameterType),
        Source(K::InferredParameterType),
        Source(K::PropertyType),
        Source(K::StubTypes),
        // generics
        Source(K::CallTypeArguments),
        Source(K::TypeArgumentName),
        Source(K::InferredBaseTypeArguments),
        Source(K::Variance),
        Source(K::Reification),
        Source(K::NumericPromotion),
        // arguments
        Source(K::CallArgumentName),
        Source(K::ImplicitParameter),
        Source(K::ImplicitSelf),
        Source(K::ImplicitArgument),
        Source(K::InheritedDefault),
        // declarations and effects
        Source(K::Override),
        Source(K::Raises),
        Source(K::RevealedType),
        Source(K::EnumValue),
        Source(K::StubOnlyMembers),
        // basedpython-ui
        Source(K::Reads),
        Source(K::Stability),
        Source(K::DerivedDeps),
        Source(K::Invalidates),
        // django templates
        Template(T::Type),
        Template(T::Template),
    ]
};

/// a stand-in for the `basedpython_ui` framework, which the ui examples import: the framework is
/// a package of its own, not shipped with this server, and its hints come from what its
/// declarations are — `@composable`, the slot functions, the observables, the builders — which
/// this declares as the framework does, and nothing else of it
const BASEDPYTHON_UI: &[(&str, &str)] = &[
    (
        "/basedpython_ui/__init__.by",
        "from .runtime export State, Derived, state, derived, remember, composable\n\
         from .widgets export Text, Button\n",
    ),
    (
        "/basedpython_ui/runtime.by",
        "class State[T]:\n    value: T\n    init(self, initial: T)\n\n\
         class Derived[T]:\n    value: T\n\n\
         def state[T](initial: T) -> State[T]: ...\n\
         def derived[T](compute: () -> T) -> Derived[T]: ...\n\
         def remember[T](compute: () -> T) -> T: ...\n\
         def composable[F](fn: F) -> F: ...\n\
         def builder[F](fn: F) -> F: ...\n",
    ),
    (
        "/basedpython_ui/widgets.by",
        "from .runtime import builder\n\n\
         @builder\ndef Text(text: str) -> None: ...\n\
         @builder\ndef Button(label: str, on_click: () -> None) -> None: ...\n",
    ),
];

/// the examples' hints, as this server draws them
///
/// every source kind's example is a file of a database of its own, which holds nothing but the
/// examples and the stand-in for the ui framework they import, read with the analysis a fresh
/// project gets. the database is attached as it is read, so it is built and read on a thread that
/// has attached no other: a request handler that holds the session's must not do this on its own
/// thread
pub struct InlayHintExamples {
    db: ProjectDatabase,
    files: Vec<(InlayHintKind, File)>,
}

impl InlayHintExamples {
    /// `None` where the in-memory database could not be made, which it always can
    pub fn new() -> Option<Self> {
        let root = SystemPathBuf::from("/");
        let system =
            InMemorySystem::from_memory_fs(MemoryFileSystem::with_current_directory(&root));

        let mut paths = Vec::new();
        for kind in INLAY_HINT_CATALOG {
            let AnyInlayHintKind::Source(kind) = *kind else {
                continue;
            };
            let about = kind.about();
            let Some(example) = about.example else {
                continue;
            };
            let path = root.join(format!(
                "{}.{}",
                kind.setting(),
                about.example_language.extension()
            ));
            system.fs().write_file_all(&path, example).ok()?;
            // the stub resolves for the example's module ahead of the example itself, which makes
            // the example the runtime module it describes
            if let Some(stub) = about.example_stub {
                let stub_path =
                    path.with_extension(&format!("{}i", about.example_language.extension()));
                system.fs().write_file_all(&stub_path, stub).ok()?;
            }
            paths.push((kind, path));
        }
        for (path, source) in BASEDPYTHON_UI {
            system.fs().write_file_all(path, source).ok()?;
        }

        let db = ProjectDatabase::use_defaults(ProjectMetadata::new("examples", root), system);
        let files = paths
            .into_iter()
            .map(|(kind, path)| Some((kind, system_path_to_file(&db, &path).ok()?)))
            .collect::<Option<_>>()?;
        Some(Self { db, files })
    }

    /// the database the examples are files of, which a hint's offsets are read against
    pub fn db(&self) -> &ProjectDatabase {
        &self.db
    }

    /// the example of `kind` and every hint drawn on it, of every kind. `None` for a kind with
    /// no example
    pub fn hints(&self, kind: InlayHintKind) -> Option<(File, Vec<InlayHint>)> {
        let (_, file) = self.files.iter().find(|(other, _)| *other == kind)?;
        let db = &self.db;
        let length = TextSize::of(source_text(db, *file).as_str());
        let hints = inlay_hints(
            db,
            db.program_file(*file),
            TextRange::up_to(length),
            &InlayHintSettings::default(),
        );
        Some((*file, hints))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use strum::IntoEnumIterator;

    use super::*;

    #[test]
    fn every_kind_is_listed_once() {
        let listed: HashSet<AnyInlayHintKind> = INLAY_HINT_CATALOG.iter().copied().collect();
        assert_eq!(
            listed.len(),
            INLAY_HINT_CATALOG.len(),
            "a kind is listed twice"
        );
        for kind in InlayHintKind::iter() {
            assert!(
                listed.contains(&AnyInlayHintKind::Source(kind)),
                "{kind:?} is not listed"
            );
        }
        for kind in TemplateInlayHintKind::iter() {
            assert!(
                listed.contains(&AnyInlayHintKind::Template(kind)),
                "{kind:?} is not listed"
            );
        }
    }

    /// the kinds are listed family by family, the families in the order they are shown, and
    /// every family has a kind
    #[test]
    fn kinds_are_listed_by_family() {
        let families: Vec<InlayHintFamily> = INLAY_HINT_CATALOG
            .iter()
            .map(|kind| kind.about().family)
            .fold(Vec::new(), |mut families, family| {
                if families.last() != Some(&family) {
                    families.push(family);
                }
                families
            });
        assert_eq!(families, InlayHintFamily::iter().collect::<Vec<_>>());
    }

    #[test]
    fn template_kinds_are_in_the_template_family() {
        for kind in INLAY_HINT_CATALOG {
            assert_eq!(
                kind.file_kind() == InlayHintFileKind::Template,
                kind.about().family == InlayHintFamily::Templates,
                "{kind:?}"
            );
        }
    }

    /// what a client shows as plain text says it in plain text, briefly
    #[test]
    fn titles_and_descriptions_are_plain() {
        for kind in INLAY_HINT_CATALOG {
            let about = kind.about();
            assert!(
                !about.title.is_empty() && about.title.len() <= 30,
                "{kind:?}"
            );
            assert!(!about.description.contains('`'), "{kind:?}");
            assert!(about.description.ends_with('.'), "{kind:?}");
            assert!(
                about.description.starts_with(char::is_uppercase),
                "{kind:?}"
            );
            assert!(!about.description.contains("  "), "{kind:?}");
        }
    }

    /// every source kind has an example, and the example shows its hint
    #[test]
    fn every_example_shows_its_own_hint() {
        let examples = InlayHintExamples::new().expect("the examples' database");
        for kind in InlayHintKind::iter() {
            let (_, hints) = examples
                .hints(kind)
                .unwrap_or_else(|| panic!("{kind:?} has no example"));
            assert!(
                hints.iter().any(|hint| hint.kind == kind),
                "the example of {kind:?} shows no hint of its own, only {:?}",
                hints
                    .iter()
                    .map(|hint| (hint.kind, hint.display().to_string()))
                    .collect::<Vec<_>>()
            );
        }
    }
}
