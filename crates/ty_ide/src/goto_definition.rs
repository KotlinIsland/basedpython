use crate::django_template::django_string_definition;
use crate::goto::{django_lookup_definitions, find_goto_target};
use crate::{Db, NavigationTargets, RangedValue};
use ruff_db::files::FileRange;
use ruff_db::parsed::parsed_module;
use ruff_text_size::{Ranged, TextSize};
use ty_python_core::ProgramFile;
use ty_python_semantic::{ImportAliasResolution, SemanticModel};

/// Navigate to the definition of a symbol.
///
/// A "definition" is the actual implementation of a symbol, potentially in a source file
/// rather than a stub file. This differs from "declaration" which may navigate to stub files.
/// When possible, this function will map from stub file declarations to their corresponding
/// source file implementations using the `StubMapper`.
pub fn goto_definition(
    db: &dyn Db,
    file: ProgramFile<'_>,
    offset: TextSize,
) -> Option<RangedValue<NavigationTargets>> {
    let module = parsed_module(db, file.python_file(db)).load(db);

    // a template name and a url name are plain strings, so python's own answer
    // for one is whatever `str` is — which is never where the user wanted to go
    if let Some(named) = django_string_definition(db, file.file(db), &module, offset) {
        return Some(named);
    }

    let model = SemanticModel::new(db, file);
    let goto_target = find_goto_target(&model, &module, offset)?;
    let definition_targets = goto_target
        .definitions(&model, ImportAliasResolution::ResolveAliases)
        .filter(|definitions| !definitions.is_empty())
        .or_else(|| django_lookup_definitions(&model, &module, &goto_target))?
        .goto_definition(&model, &goto_target)?
        .into_navigation_targets(model.db());

    Some(RangedValue {
        range: FileRange::new(file.file(db), goto_target.range()),
        value: definition_targets,
    })
}

#[cfg(test)]
pub(super) mod test {

    use crate::tests::{CursorTest, IntoDiagnostic, cursor_test};
    use crate::{NavigationTargets, RangedValue, goto_definition};
    use insta::assert_snapshot;
    use ruff_db::diagnostic::{
        Annotation, Diagnostic, DiagnosticId, LintName, Severity, Span, SubDiagnostic,
        SubDiagnosticSeverity,
    };
    use ruff_python_ast::PythonVersion;
    use ruff_text_size::Ranged;

    #[test]
    fn goto_definition_does_not_mix_global_and_nonlocal_comprehension_walruses() {
        let test = cursor_test(
            "
last = 0

def outer():
    last = 1

    def write_global():
        global last
        [(last := global_item) for global_item in [2]]

    def write_nonlocal():
        nonlocal last
        [(last := nonlocal_item) for nonlocal_item in [3]]

    write_global()
    write_nonlocal()
    return la<CURSOR>st
",
        );

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:17:12
           |
        17 |     return last
           |            ^^^^ Clicking here
        info: Found 2 definitions
          --> main.py:5:5
           |
         5 |     last = 1
           |     ----
           |
          ::: main.py:13:11
           |
        13 |         [(last := nonlocal_item) for nonlocal_item in [3]]
           |           ----
        ");
    }

    #[test]
    fn goto_definition_comprehension_walrus_in_function() {
        let test = cursor_test(
            "
def f(items):
    [(last := item) for item in items]
    return la<CURSOR>st
",
        );

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:12
          |
        4 |     return last
          |            ^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:7
          |
        3 |     [(last := item) for item in items]
          |       ----
        ");
    }

    #[test]
    fn goto_definition_nested_comprehension_walrus_in_function() {
        let test = cursor_test(
            "
def f(items):
    [[(last := item) for item in items] for _ in [1]]
    return la<CURSOR>st
",
        );

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:12
          |
        4 |     return last
          |            ^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:8
          |
        3 |     [[(last := item) for item in items] for _ in [1]]
          |        ----
        ");
    }

    #[test]
    fn goto_definition_imported_comprehension_walrus() {
        let test = CursorTest::builder()
            .source("lib.py", "[(last := item) for item in [1]]\n")
            .source("main.py", "from lib import last\nprint(la<CURSOR>st)\n")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:2:7
          |
        2 | print(last)
          |       ^^^^ Clicking here
        info: Found 1 definition
         --> lib.py:1:3
          |
        1 | [(last := item) for item in [1]]
          |   ----
        ");
    }

    #[test]
    fn goto_definition_nonlocal_comprehension_walrus() {
        let test = cursor_test(
            "
def outer(items):
    last = 0

    def inner():
        nonlocal last
        [(last := item) for item in items]
        return la<CURSOR>st

    return inner()
",
        );

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:16
          |
        8 |         return last
          |                ^^^^ Clicking here
        info: Found 2 definitions
         --> main.py:3:5
          |
        3 |     last = 0
          |     ----
        4 |
        5 |     def inner():
        6 |         nonlocal last
        7 |         [(last := item) for item in items]
          |           ----
        ");
    }

    #[test]
    fn goto_definition_relative_import() {
        let test = CursorTest::builder()
            .source("mypackage/__init__.py", "from . import module_a<CURSOR>")
            .source("mypackage/module_a.py", "class Test: ...")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> mypackage/__init__.py:1:15
          |
        1 | from . import module_a
          |               ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mypackage/module_a.py:1:1
          |
        1 | class Test: ...
          | -
        ");
    }

    #[test]
    fn goto_definition_relative_import_reference() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                "from . import module_a\nx = module_a<CURSOR>",
            )
            .source("mypackage/module_a.py", "class Test: ...")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> mypackage/__init__.py:2:5
          |
        2 | x = module_a
          |     ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mypackage/module_a.py:1:1
          |
        1 | class Test: ...
          | -
        ");
    }

    #[test]
    fn goto_definition_relative_star_imported_submodule_reference() {
        let test = CursorTest::builder()
            .source(
                "mypackage/__init__.py",
                "from .exporter import *\nx = module_a<CURSOR>",
            )
            .source("mypackage/exporter.py", "from . import module_a")
            .source("mypackage/module_a.py", "class Test: ...")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> mypackage/__init__.py:2:5
          |
        2 | x = module_a
          |     ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mypackage/module_a.py:1:1
          |
        1 | class Test: ...
          | -
        ");
    }

    /// goto-definition on a module should go to the .py not the .pyi
    ///
    /// TODO: this currently doesn't work right! This is especially surprising
    /// because [`goto_definition_stub_map_module_ref`] works fine.
    #[test]
    fn goto_definition_stub_map_module_import() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymo<CURSOR>dule import my_function
",
            )
            .source(
                "mymodule.py",
                r#"
def my_function():
    return "hello"
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
def my_function(): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:2:6
          |
        2 | from mymodule import my_function
          |      ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:1:1
          |
        1 |
          | -
        ");
    }

    /// goto-definition on a module ref should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_module_ref() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
import mymodule
x = mymo<CURSOR>dule
",
            )
            .source(
                "mymodule.py",
                r#"
def my_function():
    return "hello"
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
def my_function(): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:3:5
          |
        3 | x = mymodule
          |     ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:1:1
          |
        1 |
          | -
        ");
    }

    /// goto-definition on a function call should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_function() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import my_function
print(my_func<CURSOR>tion())
",
            )
            .source(
                "mymodule.py",
                r#"
def my_function():
    return "hello"

def other_function():
    return "other"
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
def my_function(): ...

def other_function(): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:3:7
          |
        3 | print(my_function())
          |       ^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def my_function():
          |     -----------
        ");
    }

    #[test]
    fn goto_definition_stub_map_reexported_function() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from a import bar
bar<CURSOR>()
",
            )
            .source("a/__init__.pyi", "def bar() -> None: ...\n")
            .source("a/__init__.py", "from .impl import bar as bar\n")
            .source(
                "a/impl.py",
                r#"
def bar() -> None:
    pass
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:3:1
          |
        3 | bar()
          | ^^^ Clicking here
        info: Found 1 definition
         --> a/impl.py:2:5
          |
        2 | def bar() -> None:
          |     ---
        ");
    }

    /// goto-definition on a function definition in a .pyi should go to the .py
    #[test]
    fn goto_definition_stub_map_function_def() {
        let test = CursorTest::builder()
            .source(
                "mymodule.py",
                r#"
def my_function():
    return "hello"

def other_function():
    return "other"
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
def my_fun<CURSOR>ction(): ...

def other_function(): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> mymodule.pyi:2:5
          |
        2 | def my_function(): ...
          |     ^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def my_function():
          |     -----------
        ");
    }

    /// goto-definition on a function that's redefined many times in the impl .py
    ///
    /// Currently this yields all instances. There's an argument for only yielding
    /// the final one since that's the one "exported" but, this is consistent for
    /// how we do file-local goto-definition.
    #[test]
    fn goto_definition_stub_map_function_redefine() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import my_function
print(my_func<CURSOR>tion())
",
            )
            .source(
                "mymodule.py",
                r#"
def my_function():
    return "hello"

def my_function():
    return "hello again"

def my_function():
    return "we can't keep doing this"

def other_function():
    return "other"
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
def my_function(): ...

def other_function(): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:3:7
          |
        3 | print(my_function())
          |       ^^^^^^^^^^^ Clicking here
        info: Found 3 definitions
         --> mymodule.py:2:5
          |
        2 | def my_function():
          |     -----------
        3 |     return "hello"
        4 |
        5 | def my_function():
          |     -----------
        6 |     return "hello again"
        7 |
        8 | def my_function():
          |     -----------
        "#);
    }

    /// goto-definition on a class ref go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_class_ref() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import MyClass
x = MyC<CURSOR>lass
",
            )
            .source(
                "mymodule.py",
                r#"
class MyClass:
    def __init__(self, val):
        self.val = val

class MyOtherClass:
    def __init__(self, val):
        self.val = val + 1
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyClass:
    def __init__(self, val: bool): ...

class MyOtherClass:
    def __init__(self, val: bool): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:3:5
          |
        3 | x = MyClass
          |     ^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:7
          |
        2 | class MyClass:
          |       -------
        ");
    }

    /// goto-definition on a class def in a .pyi should go to the .py
    #[test]
    fn goto_definition_stub_map_class_def() {
        let test = CursorTest::builder()
            .source(
                "mymodule.py",
                r#"
class MyClass:
    def __init__(self, val):
        self.val = val

class MyOtherClass:
    def __init__(self, val):
        self.val = val + 1
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyCl<CURSOR>ass:
    def __init__(self, val: bool): ...

class MyOtherClass:
    def __init__(self, val: bool): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> mymodule.pyi:2:7
          |
        2 | class MyClass:
          |       ^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:7
          |
        2 | class MyClass:
          |       -------
        ");
    }

    /// goto-definition on a class init should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_class_init() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import MyClass
x = MyCl<CURSOR>ass(0)
",
            )
            .source(
                "mymodule.py",
                r#"
class MyClass:
    def __init__(self, val):
        self.val = val

class MyOtherClass:
    def __init__(self, val):
        self.val = val + 1
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyClass:
    def __init__(self, val: bool): ...

class MyOtherClass:
    def __init__(self, val: bool): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:3:5
          |
        3 | x = MyClass(0)
          |     ^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:7
          |
        2 | class MyClass:
          |       -------
        ");
    }

    /// goto-definition on a class method should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_class_method() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import MyClass
x = MyClass(0)
x.act<CURSOR>ion()
",
            )
            .source(
                "mymodule.py",
                r#"
class MyClass:
    def __init__(self, val):
        self.val = val
    def action(self):
        print(self.val)

class MyOtherClass:
    def __init__(self, val):
        self.val = val + 1
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyClass:
    def __init__(self, val: bool): ...
    def action(self): ...

class MyOtherClass:
    def __init__(self, val: bool): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:3
          |
        4 | x.action()
          |   ^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:5:9
          |
        5 |     def action(self):
          |         ------
        ");
    }

    /// goto-definition on a class attribute should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_class_attribute() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import MyClass
def f(x: MyClass):
    x.so<CURSOR>und
",
            )
            .source(
                "mymodule.py",
                r#"
class MyClass:
    sound: str = "generic"
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyClass:
    sound: str
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:4:7
          |
        4 |     x.sound
          |       ^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:3:5
          |
        3 |     sound: str = "generic"
          |     -----
        "#);
    }

    /// goto-definition on a module-level variable should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_module_variable() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
import mymodule
mymodule.CO<CURSOR>UNT
",
            )
            .source(
                "mymodule.py",
                r#"
COUNT = 0
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
COUNT: int
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r"
        info[goto-definition]: Go to definition
         --> main.py:3:10
          |
        3 | mymodule.COUNT
          |          ^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:1
          |
        2 | COUNT = 0
          | -----
        ");
    }

    /// goto-definition on a class function should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_class_function() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import MyClass
x = MyClass.act<CURSOR>ion()
",
            )
            .source(
                "mymodule.py",
                r#"
class MyClass:
    def __init__(self, val):
        self.val = val
    def action():
        print("hi!")

class MyOtherClass:
    def __init__(self, val):
        self.val = val + 1
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyClass:
    def __init__(self, val: bool): ...
    def action(): ...

class MyOtherClass:
    def __init__(self, val: bool): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:3:13
          |
        3 | x = MyClass.action()
          |             ^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:5:9
          |
        5 |     def action():
          |         ------
        ");
    }

    /// goto-definition on a class import should go to the .py not the .pyi
    #[test]
    fn goto_definition_stub_map_class_import() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import MyC<CURSOR>lass
",
            )
            .source(
                "mymodule.py",
                r#"
class MyClass: ...
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
class MyClass: ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:2:22
          |
        2 | from mymodule import MyClass
          |                      ^^^^^^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:7
          |
        2 | class MyClass: ...
          |       -------
        ");
    }

    /// goto-definition on a nested call using a keyword arg where both funcs have that arg name
    ///
    /// In this case they ultimately have different signatures.
    #[test]
    fn goto_definition_nested_keyword_arg1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
def my_func(ab, y, z = None): ...
def my_other_func(ab, y): ...

my_other_func(my_func(a<CURSOR>b=5, y=2), 0)
my_func(my_other_func(ab=5, y=2), 0)
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:5:23
          |
        5 | my_other_func(my_func(ab=5, y=2), 0)
          |                       ^^ Clicking here
        info: Found 1 definition
         --> main.py:2:13
          |
        2 | def my_func(ab, y, z = None): ...
          |             --
        ");
    }

    /// goto-definition on a nested call using a keyword arg where both funcs have that arg name
    ///
    /// In this case they ultimately have different signatures.
    #[test]
    fn goto_definition_nested_keyword_arg2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
def my_func(ab, y, z = None): ...
def my_other_func(ab, y): ...

my_other_func(my_func(ab=5, y=2), 0)
my_func(my_other_func(a<CURSOR>b=5, y=2), 0)
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:6:23
          |
        6 | my_func(my_other_func(ab=5, y=2), 0)
          |                       ^^ Clicking here
        info: Found 1 definition
         --> main.py:3:19
          |
        3 | def my_other_func(ab, y): ...
          |                   --
        ");
    }

    /// goto-definition on a nested call using a keyword arg where both funcs have that arg name
    ///
    /// In this case they have identical signatures.
    #[test]
    fn goto_definition_nested_keyword_arg3() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
def my_func(ab, y): ...
def my_other_func(ab, y): ...

my_other_func(my_func(a<CURSOR>b=5, y=2), 0)
my_func(my_other_func(ab=5, y=2), 0)
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:5:23
          |
        5 | my_other_func(my_func(ab=5, y=2), 0)
          |                       ^^ Clicking here
        info: Found 1 definition
         --> main.py:2:13
          |
        2 | def my_func(ab, y): ...
          |             --
        ");
    }

    /// goto-definition on a nested call using a keyword arg where both funcs have that arg name
    ///
    /// In this case they have identical signatures.
    #[test]
    fn goto_definition_nested_keyword_arg4() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
def my_func(ab, y): ...
def my_other_func(ab, y): ...

my_other_func(my_func(ab=5, y=2), 0)
my_func(my_other_func(a<CURSOR>b=5, y=2), 0)
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:6:23
          |
        6 | my_func(my_other_func(ab=5, y=2), 0)
          |                       ^^ Clicking here
        info: Found 1 definition
         --> main.py:3:19
          |
        3 | def my_other_func(ab, y): ...
          |                   --
        ");
    }

    #[test]
    fn goto_definition_overload_type_disambiguated1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int): ...

@overload
def ab(a: str): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:1
          |
        4 | ab(1)
          | ^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def ab(a):
          |     --
        ");
    }

    #[test]
    fn goto_definition_overload_type_disambiguated2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
from mymodule import ab

a<CURSOR>b("hello")
"#,
            )
            .source(
                "mymodule.py",
                r#"
def ab(a):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int): ...

@overload
def ab(a: str): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:4:1
          |
        4 | ab("hello")
          | ^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def ab(a):
          |     --
        "#);
    }

    #[test]
    fn goto_definition_overload_arity_disambiguated1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1, 2)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, b = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int, b: int): ...

@overload
def ab(a: int): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:1
          |
        4 | ab(1, 2)
          | ^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def ab(a, b = None):
          |     --
        ");
    }

    #[test]
    fn goto_definition_overload_arity_disambiguated2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, b = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int, b: int): ...

@overload
def ab(a: int): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:1
          |
        4 | ab(1)
          | ^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def ab(a, b = None):
          |     --
        ");
    }

    #[test]
    fn goto_definition_overload_keyword_disambiguated1() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1, b=2)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, *, b = None, c = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int): ...

@overload
def ab(a: int, *, b: int): ...

@overload
def ab(a: int, *, c: int): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:1
          |
        4 | ab(1, b=2)
          | ^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def ab(a, *, b = None, c = None):
          |     --
        ");
    }

    #[test]
    fn goto_definition_overload_keyword_disambiguated2() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from mymodule import ab

a<CURSOR>b(1, c=2)
",
            )
            .source(
                "mymodule.py",
                r#"
def ab(a, *, b = None, c = None):
    """the real implementation!"""
"#,
            )
            .source(
                "mymodule.pyi",
                r#"
from typing import overload

@overload
def ab(a: int): ...

@overload
def ab(a: int, *, b: int): ...

@overload
def ab(a: int, *, c: int): ...
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:4:1
          |
        4 | ab(1, c=2)
          | ^^ Clicking here
        info: Found 1 definition
         --> mymodule.py:2:5
          |
        2 | def ab(a, *, b = None, c = None):
          |     --
        ");
    }

    #[test]
    fn goto_definition_binary_operator() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __add__(self, other):
        return Test()


a = Test()
b = Test()

a <CURSOR>+ b
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:10:3
           |
        10 | a + b
           |   ^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __add__(self, other):
          |         -------
        ");
    }

    #[test]
    fn goto_definition_binary_operator_reflected_dunder() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class A:
    def __radd__(self, other) -> A:
        return self

class B: ...

B() <CURSOR>+ A()
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:5
          |
        8 | B() + A()
          |     ^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __radd__(self, other) -> A:
          |         --------
        ");
    }

    #[test]
    fn goto_definition_binary_operator_no_spaces_before_operator() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __add__(self, other):
        return Test()


a = Test()
b = Test()

a<CURSOR>+b
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:10:2
           |
        10 | a+b
           |  ^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __add__(self, other):
          |         -------
        ");
    }

    #[test]
    fn goto_definition_binary_operator_no_spaces_after_operator() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __add__(self, other):
        return Test()


a = Test()
b = Test()

a+<CURSOR>b
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:10:3
           |
        10 | a+b
           |   ^ Clicking here
        info: Found 1 definition
         --> main.py:8:1
          |
        8 | b = Test()
          | -
        ");
    }

    #[test]
    fn goto_definition_binary_operator_comment() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __add__(self, other):
        return Test()


(
    Test()  <CURSOR># comment
    + Test()
)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    #[test]
    fn goto_definition_unary_operator() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __invert__(self) -> 'Test': ...

a = Test()

<CURSOR>~a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:1
          |
        7 | ~a
          | ^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __invert__(self) -> 'Test': ...
          |         ----------
        ");
    }

    /// basedpython: a `typing` member is written without an import, so there is
    /// no binding in the file for the ordinary resolution to answer with — the
    /// member it means is in `typing` itself.
    #[test]
    fn goto_definition_implicit_typing_name() {
        let test = CursorTest::builder()
            .source("main.by", "a: M<CURSOR>apping\n")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.by:LL:4
           |
        LL | a: Mapping
           |    ^^^^^^^ Clicking here
        info: Found 1 definition
          --> stdlib/_collections_abc.byi:LL:7
           |
        LL | class Mapping[out Key, out Value](Collection[Key]):
           |       -------
        ");
    }

    /// basedpython makes these names available whatever the target version, so
    /// one the `typing` stub only defines from a later version than the project
    /// targets is taken from `typing_extensions` — which is where the transpiler
    /// imports it from too, and navigation follows the same route, through that
    /// module's re-export of the `typing` declaration.
    #[test]
    fn goto_definition_implicit_typing_name_added_after_the_target_version() {
        let test = CursorTest::builder()
            .python_version(PythonVersion::PY310)
            .source(
                "main.by",
                "class C:\n    def f(self) -> Sel<CURSOR>f: ...\n",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.by:LL:20
           |
        LL |     def f(self) -> Self: ...
           |                    ^^^^ Clicking here
        info: Found 2 definitions
          --> stdlib/typing.byi:LL:5
           |
        LL |     Self: _SpecialForm
           |     ----
           |
          ::: stdlib/typing_extensions.byi:LL:5
           |
        LL |     Self: _SpecialForm
           |     ----
        ");
    }

    /// The same name in a python file has nothing behind it.
    #[test]
    fn goto_definition_implicit_typing_name_is_basedpython_only() {
        let test = cursor_test("a: M<CURSOR>apping\n");

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// basedpython: `Character` comes from `ty_extensions` rather than `typing`,
    /// and means the member only where a type is being written. The value
    /// position below is an unresolved name, which leads nowhere.
    #[test]
    fn goto_definition_implicit_character() {
        let test = CursorTest::builder()
            .source("main.by", "a: Charact<CURSOR>er\n")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.by:LL:4
           |
        LL | a: Character
           |    ^^^^^^^^^ Clicking here
        info: Found 1 definition
          --> stdlib/ty_extensions/__init__.pyi:LL:7
           |
        LL | class Character(str):
           |       ---------
        ");

        let value_position = CursorTest::builder()
            .source("main.by", "a = Charact<CURSOR>er\n")
            .build();

        assert_snapshot!(value_position.goto_definition(), @"No goto target found");
    }

    /// basedpython: a trailing lambda block's receiver outranks every binding
    /// outside the block, so a name it claims must lead to the receiver's member
    /// rather than to the module-level binding the scope walk would find.
    #[test]
    fn goto_definition_trailing_block_receiver_member() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class Tag:
    def text(self, t: str) -> None: ...

def div(block: Tag.() -> None) -> None: ...

def text(a: int, b: int) -> None: ...

div:
    te<CURSOR>xt(\"hi\")
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.by:10:5
           |
        10 |     text(\"hi\")
           |     ^^^^ Clicking here
        info: Found 1 definition
         --> main.by:3:9
          |
        3 |     def text(self, t: str) -> None: ...
          |         ----
        ");
    }

    /// basedpython: a call the receiver's member cannot take walks past it, and
    /// the definition has to walk past it too.
    #[test]
    fn goto_definition_trailing_block_reaches_past_the_receiver() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class Tag:
    def text(self, t: str) -> None: ...

def div(block: Tag.() -> None) -> None: ...

def text(a: int, b: int) -> None: ...

div:
    te<CURSOR>xt(1, 2)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.by:10:5
           |
        10 |     text(1, 2)
           |     ^^^^ Clicking here
        info: Found 1 definition
         --> main.by:7:5
          |
        7 | def text(a: int, b: int) -> None: ...
          |     ----
        ");
    }

    /// basedpython: `dynamic` is a word of the language rather than a name, so
    /// it leads nowhere even though it means `typing.Any`.
    #[test]
    fn goto_definition_dynamic_keyword() {
        let test = CursorTest::builder()
            .source("main.by", "a: dyn<CURSOR>amic\n")
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// basedpython: an operator whose dunder an `extension` supplies. The
    /// operand's own type has no such member, so the ordinary dunder lookup
    /// finds nothing — without the extension fallback there is no goto target
    /// for an operator the checker accepts.
    #[test]
    fn goto_definition_unary_operator_from_an_extension() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
extension str:
    def __invert__(self) -> str:
        return self

a = \"asdf\"

<CURSOR>~a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:8:1
          |
        8 | ~a
          | ^ Clicking here
        info: Found 1 definition
         --> main.by:3:9
          |
        3 |     def __invert__(self) -> str:
          |         ----------
        ");
    }

    /// basedpython: an `extension` member reached by a *bare* attribute access.
    /// A call resolves through the call's own dispatch target and so found the
    /// extension's function by accident; `xs.second` on its own has no call to
    /// go through, and a property can never be a callee at all
    #[test]
    fn goto_definition_extension_member_without_a_call() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
extension str:
    def f(self) -> int:
        return 1

\"asdf\".<CURSOR>f
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.by:6:8
          |
        6 | "asdf".f
          |        ^ Clicking here
        info: Found 1 definition
         --> main.by:3:9
          |
        3 |     def f(self) -> int:
          |         -
        "#);
    }

    /// basedpython: an `extension` property. It is only ever read, never called,
    /// so it reaches goto through the bare-access fallback and nothing else
    #[test]
    fn goto_definition_extension_property() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class Widget: ...

extension Widget:
    let size: int
        get() = 1

w = Widget()
w.<CURSOR>size
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:9:3
          |
        9 | w.size
          |   ^^^^ Clicking here
        info: Found 1 definition
         --> main.by:5:9
          |
        5 |     let size: int
          |         ----
        ");
    }

    /// basedpython: an implicit-receiver callable — `x.fn` where `fn` names a
    /// receiver callable in scope rather than a member of `x`. The declaration
    /// is an ordinary name in an enclosing scope, so the class-hierarchy walk
    /// cannot reach it
    #[test]
    fn goto_definition_implicit_receiver_callable() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
def apply(fn: int.() -> str) -> str:
    receiver = 1
    return receiver.<CURSOR>fn()
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:4:21
          |
        4 |     return receiver.fn()
          |                     ^^ Clicking here
        info: Found 1 definition
         --> main.by:2:11
          |
        2 | def apply(fn: int.() -> str) -> str:
          |           --
        ");
    }

    /// basedpython: a name a `let` pattern binds. The pattern is the same node a
    /// `match` case uses, so the binder is an ordinary capture
    #[test]
    fn goto_definition_let_destructuring_binder() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
def f() -> tuple[int, str]:
    return (1, \"a\")

let (a, b) := f()
print(<CURSOR>a)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:6:7
          |
        6 | print(a)
          |       ^ Clicking here
        info: Found 1 definition
         --> main.by:5:6
          |
        5 | let (a, b) := f()
          |      -
        ");
    }

    /// basedpython: the name in an `extension Widget:` header. It denotes the
    /// class the extension extends rather than declaring one, so goto belongs on
    /// `class Widget` — answering with the header itself made it self-referential
    /// and left find-references on a class unable to list its extensions
    #[test]
    fn goto_definition_extension_header_names_the_extended_class() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class Widget: ...

extension Wid<CURSOR>get:
    def go(self) -> int:
        return 1
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:4:11
          |
        4 | extension Widget:
          |           ^^^^^^ Clicking here
        info: Found 1 definition
         --> main.by:2:7
          |
        2 | class Widget: ...
          |       ------
        ");
    }

    /// basedpython: a bare enum member reached through the expected type.
    /// Nothing in scope binds `Red`, so the ordinary scope walk had nothing to
    /// answer with even though hover already resolved it to `Color.Red`
    #[test]
    fn goto_definition_context_resolved_enum_member() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
enum class Color:
    case Red
    case Green

c: Color = R<CURSOR>ed
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:6:12
          |
        6 | c: Color = Red
          |            ^^^ Clicking here
        info: Found 1 definition
         --> main.by:3:10
          |
        3 |     case Red
          |          ---
        ");
    }

    /// basedpython: a bare `case Red:` that names an enum member. It looks like
    /// a capture and is one only where the subject's type does not declare the
    /// name; answering with the pattern's own binding said the opposite of what
    /// the checker decided
    #[test]
    fn goto_definition_enum_member_case_pattern() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
enum class Color:
    case Red
    case Green

def f(c: Color) -> int:
    match c:
        case R<CURSOR>ed:
            return 1
        case Green:
            return 2
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:8:14
          |
        8 |         case Red:
          |              ^^^ Clicking here
        info: Found 1 definition
         --> main.by:3:10
          |
        3 |     case Red
          |          ---
        ");
    }

    /// basedpython: `field` inside a property getter. The getter carries the
    /// `__property__` marker, whose range spans the whole construct so the
    /// lowering knows what to replace — and the covering-node search settled
    /// inside that marker for every position in the block, so a getter's `field`
    /// answered about the property object while the setter's resolved correctly
    #[test]
    fn goto_definition_field_in_a_property_getter() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class Person:
    var age: int = 0
        get() = fi<CURSOR>eld
        set(value):
            field = value
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:4:17
          |
        4 |         get() = field
          |                 ^^^^^ Clicking here
        info: Found 1 definition
         --> main.by:3:5
          |
        3 |     var age: int = 0
          |     -
        ");
    }

    /// basedpython: an inline protocol's member. The type is structural — two
    /// written the same way anywhere are the same type — so it has no
    /// declaration of its own, and the annotation the receiver was declared with
    /// is the one place an editor can honestly point at
    #[test]
    fn goto_definition_inline_protocol_member() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
def f(x: protocol(a: int; def g(self) -> int)) -> int:
    return x.<CURSOR>a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.by:3:14
          |
        3 |     return x.a
          |              ^ Clicking here
        info: Found 1 definition
         --> main.by:2:19
          |
        2 | def f(x: protocol(a: int; def g(self) -> int)) -> int:
          |                   -
        ");
    }

    #[test]
    fn goto_definition_binary_operator_from_an_extension() {
        let test = CursorTest::builder()
            .source(
                "main.by",
                "
class Money: ...

extension Money:
    def __add__(self, other: Money) -> Money:
        return self

a = Money()
b = Money()

a <CURSOR>+ b
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.by:11:3
           |
        11 | a + b
           |   ^ Clicking here
        info: Found 1 definition
         --> main.by:5:9
          |
        5 |     def __add__(self, other: Money) -> Money:
          |         -------
        ");
    }

    /// We jump to the `__invert__` definition here even though its signature is incorrect.
    #[test]
    fn goto_definition_unary_operator_with_bad_dunder_definition() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __invert__(self, extra_arg) -> 'Test': ...

a = Test()

<CURSOR>~a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:1
          |
        7 | ~a
          | ^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __invert__(self, extra_arg) -> 'Test': ...
          |         ----------
        ");
    }

    #[test]
    fn goto_definition_unary_after_operator() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __invert__(self) -> 'Test': ...

a = Test()

~<CURSOR> a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:1
          |
        7 | ~ a
          | ^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __invert__(self) -> 'Test': ...
          |         ----------
        ");
    }

    #[test]
    fn goto_definition_unary_between_operator_and_operand() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __invert__(self) -> 'Test': ...

a = Test()

-<CURSOR>a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:2
          |
        7 | -a
          |  ^ Clicking here
        info: Found 1 definition
         --> main.py:5:1
          |
        5 | a = Test()
          | -
        ");
    }

    #[test]
    fn goto_definition_unary_not_with_dunder_bool() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __bool__(self) -> bool: ...

a = Test()

<CURSOR>not a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:1
          |
        7 | not a
          | ^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __bool__(self) -> bool: ...
          |         --------
        ");
    }

    #[test]
    fn goto_definition_unary_not_with_dunder_len() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __len__(self) -> 42: ...

a = Test()

<CURSOR>not a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:1
          |
        7 | not a
          | ^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __len__(self) -> 42: ...
          |         -------
        ");
    }

    /// If `__bool__` is defined incorrectly, `not` does not fallback to `__len__`.
    /// Instead, we jump to the `__bool__` definition as usual.
    /// The fallback only occurs if `__bool__` is not defined at all.
    #[test]
    fn goto_definition_unary_not_with_bad_dunder_bool_and_dunder_len() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __bool__(self, extra_arg) -> bool: ...
    def __len__(self) -> 42: ...

a = Test()

<CURSOR>not a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:1
          |
        8 | not a
          | ^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __bool__(self, extra_arg) -> bool: ...
          |         --------
        ");
    }

    /// Same as for unary operators that only use a single dunder,
    /// we still jump to `__len__` for `not` goto-definition even if
    /// the `__len__` signature is incorrect (but only if there is no
    /// `__bool__` definition).
    #[test]
    fn goto_definition_unary_not_with_no_dunder_bool_and_bad_dunder_len() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Test:
    def __len__(self, extra_arg) -> 42: ...

a = Test()

<CURSOR>not a
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:1
          |
        7 | not a
          | ^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __len__(self, extra_arg) -> 42: ...
          |         -------
        ");
    }

    #[test]
    fn float_annotation() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
a: float<CURSOR> = 3.14
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:LL:4
           |
        LL | a: float = 3.14
           |    ^^^^^ Clicking here
        info: Found 2 definitions
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ---
           |
          ::: stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       -----
        ");
    }

    #[test]
    fn complex_annotation() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
a: complex<CURSOR> = 3.14
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:LL:4
           |
        LL | a: complex = 3.14
           |    ^^^^^^^ Clicking here
        info: Found 3 definitions
          --> stdlib/builtins.byi:LL:7
           |
        LL | class int:
           |       ---
           |
          ::: stdlib/builtins.byi:LL:7
           |
        LL | class float:
           |       -----
           |
          ::: stdlib/builtins.byi:LL:7
           |
        LL | class complex:
           |       -------
        ");
    }

    /// Regression test for <https://github.com/astral-sh/ty/issues/1451>.
    /// We must ensure we respect re-import convention for stub files for
    /// imports in builtins.pyi.
    #[test]
    fn goto_definition_unimported_symbol_imported_in_builtins() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
Traceb<CURSOR>ackType
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// goto-definition on a class init opening parenthesis should go to constructor
    #[test]
    fn goto_definition_class_init_parenthesis_opening() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class MyClass:
    def __init__(self, val):
        self.val = val
x = MyClass<CURSOR>()
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:5:5
          |
        5 | x = MyClass()
          |     ^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __init__(self, val):
          |         --------
        ");
    }

    /// goto-definition on a class init closing parenthesis should go to constructor
    #[test]
    fn goto_definition_class_init_parenthesis_closing() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class MyClass:
    def __init__(self, val):
        self.val = val
x = MyClass(<CURSOR>)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:5:5
          |
        5 | x = MyClass()
          |     ^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __init__(self, val):
          |         --------
        ");
    }

    /// goto-definition on a class init closing parenthesis
    /// when there is an argument is somewhat ambiguous, and
    /// so doesn't find any defs.
    #[test]
    fn goto_definition_class_init_parenthesis_ambiguous_closing() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class MyClass:
    def __init__(self, val):
        self.val = val
x = MyClass(0<CURSOR>)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// goto-definition on a class init closing parenthesis when there
    /// is an argument with its own definition is somewhat ambiguous,
    /// and but we currently go to the definition of the argument.
    #[test]
    fn goto_definition_class_init_parenthesis_ambiguous_argument_closing() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
foo = 1

class MyClass:
    def __init__(self, val):
        self.val = val
x = MyClass(foo<CURSOR>)
",
            )
            .build();

        assert_snapshot!(
            test.goto_definition(),
            @"
        info[goto-definition]: Go to definition
         --> main.py:7:13
          |
        7 | x = MyClass(foo)
          |             ^^^ Clicking here
        info: Found 1 definition
         --> main.py:2:1
          |
        2 | foo = 1
          | ---
        ",
        );
    }

    /// goto-definition on a class init parenthesis includes `__new__`
    #[test]
    fn goto_definition_class_init_parenthesis_includes_new() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class MyClass:
    def __init__(self, val):
        self.val = val
    def __new__(self, val):
        self.val = val
x = MyClass<CURSOR>()
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:7:5
          |
        7 | x = MyClass()
          |     ^^^^^^^ Clicking here
        info: Found 2 definitions
         --> main.py:3:9
          |
        3 |     def __init__(self, val):
          |         --------
        4 |         self.val = val
        5 |     def __new__(self, val):
          |         -------
        ");
    }

    /// goto-definition on a dynamic class literal (created via `type()`)
    #[test]
    fn goto_definition_dynamic_class_literal() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
DynClass = type("DynClass", (), {})

x = DynCla<CURSOR>ss()
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:4:5
          |
        4 | x = DynClass()
          |     ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:2:1
          |
        2 | DynClass = type("DynClass", (), {})
          | --------
        "#);
    }

    /// goto-definition on a dynamic class literal (created via `type()`)
    /// when on the opening parenthesis.
    ///
    /// Unlike when the cursor is on the `DynClass` name itself, this
    /// will report the constructor method as the definition.
    #[test]
    fn goto_definition_dynamic_class_literal_parenthesis() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
DynClass = type("DynClass", (), {})

x = DynClass<CURSOR>()
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:LL:5
           |
        LL | x = DynClass()
           |     ^^^^^^^^ Clicking here
        info: Found 1 definition
          --> stdlib/builtins.byi:LL:9
           |
        LL |     def __new__(cls) -> Self
           |         -------
        ");
    }

    /// goto-definition on a dangling dynamic class literal (not assigned to a variable)
    #[test]
    fn goto_definition_dangling_dynamic_class_literal() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
class Foo(type("Ba<CURSOR>r", (), {})):
    pass
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// goto-definition on a dynamic namedtuple class literal (created via `collections.namedtuple()`)
    #[test]
    fn goto_definition_dynamic_namedtuple_literal() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
from collections import namedtuple

Point = namedtuple("Point", ["x", "y"])

p = Poi<CURSOR>nt(1, 2)
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:6:5
          |
        6 | p = Point(1, 2)
          |     ^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:4:1
          |
        4 | Point = namedtuple("Point", ["x", "y"])
          | -----
        "#);
    }

    /// goto-definition on a dynamic namedtuple class literal via opening parenthesis.
    ///
    /// At time of writing (2026-02-04), goto-def doesn't report
    /// any possible constructor methods for this case. But normally,
    /// clicking on an opening parenthesis only goes to constructor
    /// methods. So this tests that even in that case, we still go
    /// to the actual definition.
    #[test]
    fn goto_definition_dynamic_namedtuple_literal_parenthesis() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
from collections import namedtuple

Point = namedtuple("Point", ["x", "y"])

p = Point<CURSOR>(1, 2)
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:6:5
          |
        6 | p = Point(1, 2)
          |     ^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:4:1
          |
        4 | Point = namedtuple("Point", ["x", "y"])
          | -----
        "#);
    }

    // TODO: Should only list `a: int`
    #[test]
    fn redeclarations() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
                a: str = "test"

                a: int = 10

                print(a<CURSOR>)

                a: bool = True
                "#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> main.py:6:7
          |
        6 | print(a)
          |       ^ Clicking here
        info: Found 3 definitions
         --> main.py:2:1
          |
        2 | a: str = "test"
          | -
        3 |
        4 | a: int = 10
          | -
        5 |
        6 | print(a)
        7 |
        8 | a: bool = True
          | -
        "#);
    }

    #[test]
    fn goto_definition_attribute_redeclarations() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
                class Test:
                    a: str
                    a: str

                test = Test()

                test.a<CURSOR>
                "#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:6
          |
        8 | test.a
          |      ^ Clicking here
        info: Found 2 definitions
         --> main.py:3:5
          |
        3 |     a: str
          |     -
        4 |     a: str
          |     -
        ");
    }

    #[test]
    fn goto_definition_property_getter_and_setter() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                r#"
                class Test:
                    @property
                    def a(self) -> str:
                        return ""

                    @a.setter
                    def a(self, value: str) -> None:
                        pass

                test = Test()

                test.a<CURSOR>
                "#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:13:6
           |
        13 | test.a
           |      ^ Clicking here
        info: Found 2 definitions
         --> main.py:4:9
          |
        4 |     def a(self) -> str:
          |         -
          |
         ::: main.py:8:9
          |
        8 |     def a(self, value: str) -> None:
          |         -
        ");
    }

    /// Goto-definition works when accessing type attributes on class objects.
    #[test]
    fn goto_definition_for_type_attributes_on_class_objects() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
                class Foo: ...

                Foo.__dictoff<CURSOR>set__
                ",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:LL:5
           |
        LL | Foo.__dictoffset__
           |     ^^^^^^^^^^^^^^ Clicking here
        info: Found 1 definition
          --> stdlib/builtins.byi:LL:9
           |
        LL |     let __dictoffset__: int
           |         --------------
        ");
    }

    /// Goto-definition performs lookups on the metaclass when attributes are not found.
    #[test]
    fn goto_definition_performs_lookups_on_metaclass() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
                class Foo(type):
                    a: int

                class Bar(metaclass=Foo): ...
                Bar.<CURSOR>a
                ",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:6:5
          |
        6 | Bar.a
          |     ^ Clicking here
        info: Found 1 definition
         --> main.py:3:5
          |
        3 |     a: int
          |     -
        ");
    }

    /// Goto-definition does not look up instance members on the metaclass.
    #[test]
    fn goto_definition_on_members_of_class_instances() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
                class Foo(type):
                    a: int

                class Bar(metaclass=Foo): ...
                Bar().<CURSOR>a
                ",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// Check that we don't fall into infinite recursion when e.g.
    /// looking up attributes on the metaclass of `type`
    /// (`type` is its own metaclass)
    #[test]
    fn goto_definition_on_builtins_dot_type_itself_unresolved() {
        let test = CursorTest::builder()
            .source("main.py", "type.<CURSOR>a")
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// Check that we don't fall into infinite recursion when e.g.
    /// looking up attributes on the metaclass of `type`
    /// (`type` is its own metaclass)
    #[test]
    fn goto_definition_on_builtins_dot_type_itself_resolved() {
        let test = CursorTest::builder()
            .source("main.py", "type.__dict<CURSOR>offset__")
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:LL:6
           |
        LL | type.__dictoffset__
           |      ^^^^^^^^^^^^^^ Clicking here
        info: Found 1 definition
          --> stdlib/builtins.byi:LL:9
           |
        LL |     let __dictoffset__: int
           |         --------------
        ");
    }

    /// Go-to-definition should not point to while-loop header definitions.
    #[test]
    fn goto_definition_does_not_point_to_while_loop_header() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
while True:
    variable = 1

    vari<CURSOR>able
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:5:5
          |
        5 |     variable
          |     ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:5
          |
        3 |     variable = 1
          |     --------
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_typeddict() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from typing import TypedDict

class TD(TypedDict):
    f: int
    g: str

TD(f<CURSOR>=1)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:4
          |
        8 | TD(f=1)
          |    ^ Clicking here
        info: Found 1 definition
         --> main.py:5:5
          |
        5 |     f: int
          |     -
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_typeddict_update() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from typing import TypedDict

class TD(TypedDict):
    f: int
    g: str

td = TD(f=1, g=\"\")
td.update(f<CURSOR>=2)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:9:11
          |
        9 | td.update(f=2)
          |           ^ Clicking here
        info: Found 1 definition
         --> main.py:5:5
          |
        5 |     f: int
          |     -
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_unpack_typeddict() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from typing import TypedDict, Unpack

class TD(TypedDict):
    f: int
    g: str

def func(**kwargs: Unpack[TD]): ...

func(f<CURSOR>=1)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:10:6
           |
        10 | func(f=1)
           |      ^ Clicking here
        info: Found 1 definition
         --> main.py:5:5
          |
        5 |     f: int
          |     -
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_namedtuple() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from typing import NamedTuple

class NT(NamedTuple):
    f: int
    g: str

NT(f<CURSOR>=1)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:4
          |
        8 | NT(f=1)
          |    ^ Clicking here
        info: Found 1 definition
         --> main.py:5:5
          |
        5 |     f: int
          |     -
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_dataclass() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from dataclasses import dataclass

@dataclass
class DC:
    f: int
    g: str

DC(f<CURSOR>=1)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:9:4
          |
        9 | DC(f=1)
          |    ^ Clicking here
        info: Found 1 definition
         --> main.py:6:5
          |
        6 |     f: int
          |     -
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_dataclass_custom_init() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from dataclasses import dataclass

@dataclass
class DC:
    f: int
    g: str

    def __init__(self, f: int) -> None: ...

DC(f<CURSOR>=1)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:11:4
           |
        11 | DC(f=1)
           |    ^ Clicking here
        info: Found 1 definition
         --> main.py:9:24
          |
        9 |     def __init__(self, f: int) -> None: ...
          |                        -
        ");
    }

    #[test]
    fn goto_definition_keyword_argument_dataclass_transform_alias() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
from typing import dataclass_transform

def Field(alias: str = ...): ...

@dataclass_transform(field_specifiers=(Field,))
class MyDataclass: ...

class DC(MyDataclass):
    f: int = Field(alias='g')

DC(g<CURSOR>=1)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
          --> main.py:12:4
           |
        12 | DC(g=1)
           |    ^ Clicking here
        info: Found 1 definition
          --> main.py:10:5
           |
        10 |     f: int = Field(alias='g')
           |     -
        ");
    }

    /// Go-to-definition should not point to for-loop header definitions.
    #[test]
    fn goto_definition_does_not_point_to_for_loop_header() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
for x in range(10):
    variable = 1

    vari<CURSOR>able
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:5:5
          |
        5 |     variable
          |     ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:5
          |
        3 |     variable = 1
          |     --------
        ");
    }

    /// Go-to-definition on `super()` should not lookup on the super class itself
    #[test]
    fn goto_definition_does_not_lookup_on_bound_super() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Foo:
    def __init__(self, x: int) -> None:
        self.x = x

class Bar(Foo):
    def __init__(self):
        super().__init<CURSOR>__(x)
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:17
          |
        8 |         super().__init__(x)
          |                 ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __init__(self, x: int) -> None:
          |         --------
        ");
    }

    /// Go-to-definition should resolve to the parent class
    #[test]
    fn goto_definition_resolves_super_for_generic_class() {
        let test = CursorTest::builder()
            .source(
                "main.py",
                "
class Base:
    def __init__(self, x: int) -> None:
        self.x = x

class GenericFoo[T](Base):
    def __init__(self, x: int, y: T):
        super().__init<CURSOR>__(x)
        self.y = y
",
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> main.py:8:17
          |
        8 |         super().__init__(x)
          |                 ^^^^^^^^ Clicking here
        info: Found 1 definition
         --> main.py:3:9
          |
        3 |     def __init__(self, x: int) -> None:
          |         --------
        ");
    }

    /// A django project whose `blog/views.py` carries the cursor.
    fn django_test(views: &str) -> CursorTest {
        CursorTest::builder()
            .source("blog/templates/blog/post.html", "<h1>{{ post.title }}</h1>")
            .source("blog/templates/blog/list.html", "<ul></ul>")
            .source(
                "blog/urls.py",
                r#"
app_name = "blog"

urlpatterns = [
    path("<int:pk>/", detail, name="detail"),
]
"#,
            )
            .source("blog/views.py", views)
            .build()
    }

    #[test]
    fn goto_definition_django_rendered_template() {
        let test = django_test(
            r#"
def show(request):
    return render(request, "blog/po<CURSOR>st.html")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> blog/views.py:3:29
          |
        3 |     return render(request, "blog/post.html")
          |                             ^^^^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> blog/templates/blog/post.html:1:1
          |
        1 | <h1>{{ post.title }}</h1>
          | -
        "#);
    }

    #[test]
    fn goto_definition_django_template_of_a_template_response() {
        let test = django_test(
            r#"
def show(request):
    return TemplateResponse(request, "blog/li<CURSOR>st.html")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> blog/views.py:3:39
          |
        3 |     return TemplateResponse(request, "blog/list.html")
          |                                       ^^^^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> blog/templates/blog/list.html:1:1
          |
        1 | <ul></ul>
          | -
        "#);
    }

    #[test]
    fn goto_definition_django_template_name_of_a_view_class() {
        let test = django_test(
            r#"
class PostDetail(DetailView):
    template_name = "blog/po<CURSOR>st.html"
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> blog/views.py:3:22
          |
        3 |     template_name = "blog/post.html"
          |                      ^^^^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> blog/templates/blog/post.html:1:1
          |
        1 | <h1>{{ post.title }}</h1>
          | -
        "#);
    }

    #[test]
    fn goto_definition_django_reversed_route() {
        let test = django_test(
            r#"
def show(request):
    return redirect(reverse("blog:de<CURSOR>tail"))
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> blog/views.py:3:30
          |
        3 |     return redirect(reverse("blog:detail"))
          |                              ^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> blog/urls.py:5:36
          |
        5 |     path("<int:pk>/", detail, name="detail"),
          |                                    --------
        "#);
    }

    #[test]
    fn goto_definition_django_redirected_route() {
        let test = django_test(
            r#"
def show(request):
    return redirect("blog:de<CURSOR>tail")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> blog/views.py:3:22
          |
        3 |     return redirect("blog:detail")
          |                      ^^^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> blog/urls.py:5:36
          |
        5 |     path("<int:pk>/", detail, name="detail"),
          |                                    --------
        "#);
    }

    #[test]
    fn goto_definition_django_redirected_url_path() {
        // a path is not a route name, and a redirect is the one call that takes
        // both — so it leads nowhere django knows
        let test = django_test(
            r#"
def show(request):
    return redirect("/blog/<CURSOR>1/")
"#,
        );

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    #[test]
    fn goto_definition_django_template_that_is_not_there() {
        let test = django_test(
            r#"
def show(request):
    return render(request, "blog/mis<CURSOR>sing.html")
"#,
        );

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    /// a django whose model, field and manager classes ty recognizes: the
    /// `KnownClass` names are only read off a third-party search path, so they
    /// live in site-packages, and the field classes carry the same
    /// `_pyi_private_*_type` markers the real stubs pin their specializations
    /// with
    ///
    /// `queries` is written as `blog/queries.by`, since a lookup written as an
    /// expression is basedpython syntax
    pub(crate) fn django_lookup_test(queries: &str) -> CursorTest {
        CursorTest::builder()
            .with_site_packages()
            .site_packages("django/__init__.pyi", "")
            .site_packages("django/db/__init__.pyi", "from django.db import models as models")
            .site_packages(
                "django/db/models/__init__.pyi",
                "from django.db.models.base import Model as Model\n\
                 from django.db.models.fields import CharField as CharField, DateField as DateField\n\
                 from django.db.models.fields.json import JSONField as JSONField\n\
                 from django.db.models.fields.related import CASCADE as CASCADE, ForeignKey as ForeignKey\n\
                 from django.db.models.manager import Manager as Manager\n\
                 from django.db.models.query import QuerySet as QuerySet",
            )
            .site_packages(
                "django/db/models/base.pyi",
                "from typing import Any, ClassVar\n\
                 from typing_extensions import Self\n\
                 from django.db.models.manager import Manager\n\
                 \n\
                 class Model:\n\
                 \x20   objects: ClassVar[Manager[Self]]\n\
                 \x20   pk: Any",
            )
            .site_packages(
                "django/db/models/manager.pyi",
                "from typing import Any, Generic, TypeVar\n\
                 from django.db.models.query import QuerySet\n\
                 \n\
                 _M = TypeVar(\"_M\")\n\
                 \n\
                 class Manager(Generic[_M]):\n\
                 \x20   def filter(self, *args: Any, **kwargs: Any) -> QuerySet[_M, _M]: ...\n\
                 \x20   def exclude(self, *args: Any, **kwargs: Any) -> QuerySet[_M, _M]: ...\n\
                 \x20   def get(self, *args: Any, **kwargs: Any) -> _M: ...\n\
                 \x20   async def aget(self, *args: Any, **kwargs: Any) -> _M: ...",
            )
            .site_packages(
                "django/db/models/query.pyi",
                "from typing import Generic, TypeVar\n\
                 \n\
                 _M = TypeVar(\"_M\")\n\
                 _Row = TypeVar(\"_Row\")\n\
                 \n\
                 class QuerySet(Generic[_M, _Row]): ...",
            )
            .site_packages(
                "django/db/models/fields/__init__.pyi",
                "from datetime import date\n\
                 from typing import Any, Generic, TypeVar\n\
                 \n\
                 _ST = TypeVar(\"_ST\")\n\
                 _GT = TypeVar(\"_GT\")\n\
                 \n\
                 class Field(Generic[_ST, _GT]):\n\
                 \x20   _pyi_private_set_type: Any\n\
                 \x20   _pyi_private_get_type: Any\n\
                 \x20   def __init__(self, **kwargs: Any) -> None: ...\n\
                 \x20   def __get__(self, instance: Any, owner: Any = None) -> _GT: ...\n\
                 \x20   def __set__(self, instance: Any, value: _ST) -> None: ...\n\
                 \n\
                 class CharField(Field[_ST, _GT]):\n\
                 \x20   _pyi_private_set_type: str\n\
                 \x20   _pyi_private_get_type: str\n\
                 \n\
                 class DateField(Field[_ST, _GT]):\n\
                 \x20   _pyi_private_set_type: date\n\
                 \x20   _pyi_private_get_type: date",
            )
            .site_packages(
                "django/db/models/fields/json.pyi",
                "from typing import Any, TypeVar\n\
                 from django.db.models.fields import Field\n\
                 \n\
                 _ST = TypeVar(\"_ST\")\n\
                 _GT = TypeVar(\"_GT\")\n\
                 \n\
                 class JSONField(Field[_ST, _GT]):\n\
                 \x20   _pyi_private_set_type: Any\n\
                 \x20   _pyi_private_get_type: Any",
            )
            .site_packages(
                "django/db/models/fields/related.pyi",
                "from typing import Any, TypeVar\n\
                 from django.db.models.fields import Field\n\
                 \n\
                 _ST = TypeVar(\"_ST\")\n\
                 _GT = TypeVar(\"_GT\")\n\
                 \n\
                 CASCADE: Any\n\
                 \n\
                 class ForeignKey(Field[_ST, _GT]):\n\
                 \x20   _pyi_private_set_type: Any\n\
                 \x20   _pyi_private_get_type: Any\n\
                 \x20   def __init__(self, to: Any, on_delete: Any = ..., **kwargs: Any) -> None: ...",
            )
            .source(
                "blog/models.py",
                r#"
from django.db import models

class Author(models.Model):
    name = models.CharField(max_length=100)

class Book(models.Model):
    title = models.CharField(max_length=200)
    published = models.DateField()
    author = models.ForeignKey(Author, on_delete=models.CASCADE)

class Doc(models.Model):
    data = models.JSONField()
"#,
            )
            .source("blog/queries.by", queries)
            .build()
    }

    #[test]
    fn goto_definition_django_lookup_relation() {
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    return Book.objects.filter(auth<CURSOR>or.name == "x")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:5:32
          |
        5 |     return Book.objects.filter(author.name == "x")
          |                                ^^^^^^ Clicking here
        info: Found 1 definition
          --> src/blog/models.py:10:5
           |
        10 |     author = models.ForeignKey(Author, on_delete=models.CASCADE)
           |     ------
        "#);
    }

    #[test]
    fn goto_definition_django_lookup_segment_after_a_relation() {
        // the segment after a relation hop resolves against the model the path
        // traverses into, which the ordinary member resolution already answers
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    return Book.objects.filter(author.na<CURSOR>me == "x")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:5:39
          |
        5 |     return Book.objects.filter(author.name == "x")
          |                                       ^^^^ Clicking here
        info: Found 1 definition
         --> src/blog/models.py:5:5
          |
        5 |     name = models.CharField(max_length=100)
          |     ----
        "#);
    }

    #[test]
    fn goto_definition_django_lookup_with_an_operator() {
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    return Book.objects.filter(publ<CURSOR>ished > 1)
"#,
        );

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:5:32
          |
        5 |     return Book.objects.filter(published > 1)
          |                                ^^^^^^^^^ Clicking here
        info: Found 1 definition
         --> src/blog/models.py:9:5
          |
        9 |     published = models.DateField()
          |     ---------
        ");
    }

    #[test]
    fn goto_definition_django_lookup_on_exclude() {
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    return Book.objects.exclude(ti<CURSOR>tle == "x")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:5:33
          |
        5 |     return Book.objects.exclude(title == "x")
          |                                 ^^^^^ Clicking here
        info: Found 1 definition
         --> src/blog/models.py:8:5
          |
        8 |     title = models.CharField(max_length=200)
          |     -----
        "#);
    }

    #[test]
    fn goto_definition_django_lookup_json_field() {
        let test = django_lookup_test(
            r#"
from blog.models import Doc

def q():
    return Doc.objects.get(da<CURSOR>ta["key"] == 1)
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:5:28
          |
        5 |     return Doc.objects.get(data["key"] == 1)
          |                            ^^^^ Clicking here
        info: Found 1 definition
          --> src/blog/models.py:13:5
           |
        13 |     data = models.JSONField()
           |     ----
        "#);
    }

    #[test]
    fn goto_definition_django_lookup_json_key() {
        // a json key is an arbitrary string that nothing declares
        let test = django_lookup_test(
            r#"
from blog.models import Doc

def q():
    return Doc.objects.filter(data["k<CURSOR>ey"] == 1)
"#,
        );

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    #[test]
    fn goto_definition_django_lookup_pk() {
        // `pk` names no field of the model — django declares it on `Model`, and
        // that declaration is the only one there is. it is deliberately not
        // resolved to whichever field the primary key happens to be: that field
        // is synthesized when the model declares none, and guessing at it would
        // send the editor somewhere the source never says
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    return Book.objects.filter(p<CURSOR>k == 1)
"#,
        );

        assert_snapshot!(test.goto_definition(), @"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:5:32
          |
        5 |     return Book.objects.filter(pk == 1)
          |                                ^^ Clicking here
        info: Found 1 definition
         --> site-packages/django/db/models/base.pyi:7:5
          |
        7 |     pk: Any
          |     --
        ");
    }

    #[test]
    fn goto_definition_django_lookup_refused_operator() {
        // `!=` spells no lookup, so the name is not a field path and means
        // whatever it meant without the DSL — here, nothing
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    return Book.objects.filter(ti<CURSOR>tle != "x")
"#,
        );

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    #[test]
    fn goto_definition_django_lookup_shadowed_by_a_local() {
        // a name something else already claims is not a lookup path at all
        let test = django_lookup_test(
            r#"
from blog.models import Book

def q():
    title = "x"
    return Book.objects.filter(ti<CURSOR>tle == "x")
"#,
        );

        assert_snapshot!(test.goto_definition(), @r#"
        info[goto-definition]: Go to definition
         --> src/blog/queries.by:6:32
          |
        6 |     return Book.objects.filter(title == "x")
          |                                ^^^^^ Clicking here
        info: Found 1 definition
         --> src/blog/queries.by:5:5
          |
        5 |     title = "x"
          |     -----
        "#);
    }

    #[test]
    fn goto_definition_django_lookup_in_a_python_file() {
        // the DSL is basedpython's; in a python file the comparison is an
        // ordinary one against an undefined name
        let test = CursorTest::builder()
            .with_site_packages()
            .site_packages("django/__init__.pyi", "")
            .source(
                "main.py",
                r#"
class Book: ...

def q():
    return Book.objects.filter(auth<CURSOR>or.name == "x")
"#,
            )
            .build();

        assert_snapshot!(test.goto_definition(), @"No goto target found");
    }

    impl CursorTest {
        fn goto_definition(&self) -> String {
            let Some(targets) = salsa::attach(&self.db, || {
                goto_definition(
                    &self.db,
                    self.program_file(self.cursor.file),
                    self.cursor.offset,
                )
            }) else {
                return "No goto target found".to_string();
            };

            if targets.is_empty() {
                return "No definitions found".to_string();
            }

            self.render_diagnostics([GotoDiagnostic::new(GotoAction::Definition, targets)])
        }
    }

    pub(crate) struct GotoDiagnostic {
        action: GotoAction,
        targets: RangedValue<NavigationTargets>,
    }

    impl GotoDiagnostic {
        pub(crate) fn new(action: GotoAction, targets: RangedValue<NavigationTargets>) -> Self {
            Self { action, targets }
        }
    }

    impl IntoDiagnostic for GotoDiagnostic {
        fn into_diagnostic(self) -> Diagnostic {
            let source = self.targets.range;
            let mut main = Diagnostic::new(
                DiagnosticId::Lint(LintName::of(self.action.name())),
                Severity::Info,
                self.action.label().to_string(),
            );

            main.annotate(
                Annotation::primary(Span::from(source.file()).with_range(source.range()))
                    .message("Clicking here"),
            );

            let mut sub = SubDiagnostic::new(
                SubDiagnosticSeverity::Info,
                format_args!(
                    "Found {} {}{}",
                    self.targets.len(),
                    self.action.item_label(),
                    if self.targets.len() == 1 { "" } else { "s" }
                ),
            );

            for target in self.targets {
                sub.annotate(Annotation::secondary(
                    Span::from(target.file()).with_range(target.focus_range()),
                ));
            }

            main.sub(sub);

            main
        }
    }

    pub(crate) enum GotoAction {
        Definition,
        Declaration,
        TypeDefinition,
        Implementation,
    }

    impl GotoAction {
        fn name(&self) -> &'static str {
            match self {
                GotoAction::Definition => "goto-definition",
                GotoAction::Declaration => "goto-declaration",
                GotoAction::TypeDefinition => "goto-type definition",
                GotoAction::Implementation => "goto-implementation",
            }
        }

        fn label(&self) -> &'static str {
            match self {
                GotoAction::Definition => "Go to definition",
                GotoAction::Declaration => "Go to declaration",
                GotoAction::TypeDefinition => "Go to type definition",
                GotoAction::Implementation => "Go to implementation",
            }
        }

        fn item_label(&self) -> &'static str {
            match self {
                GotoAction::Definition => "definition",
                GotoAction::Declaration => "declaration",
                GotoAction::TypeDefinition => "type definition",
                GotoAction::Implementation => "implementation",
            }
        }
    }
}
