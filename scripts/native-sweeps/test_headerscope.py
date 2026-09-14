"""tests for the runtime-header scoper `cdiff.sh` narrows its population with

run with::

    uv run --no-project --with pytest pytest scripts/native-sweeps/test_headerscope.py

each fixture is a pair of headers and the answer the scoper has to give. the ones that
matter most are the unscopable ones: a narrowing that is wrong hides a module from every
behavioural rung, so a change the scoper cannot attribute exactly has to say so
"""

# /// script
# requires-python = ">=3.11"
# dependencies = ["pytest"]
#
# # the module under test is the sibling script, imported the way pytest imports it
# [tool.ty.environment]
# extra-paths = ["."]
# ///

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path

from headerscope import parse, scope

BASE = """\
#include <Python.h>

#define BY_LIKELY(x) __builtin_expect(!!(x), 1)
#define BY_TAG 1

typedef struct {
    PyObject *value;
    int flags;
} By_Box;

/* a helper nothing else calls */
static inline int By_Leaf(int x) {
    return x + BY_TAG;
}

static inline int By_Caller(int x) {
    if (BY_LIKELY(x > 0)) return By_Leaf(x);
    return 0;
}

static inline int By_Unrelated(int x) { return x * 2; }

static PyObject *By_Cache = NULL;

#define BY_DEFINE_TWICE(name, op) \\
    static inline int name(int a) { return a op 2; } \\
    static inline int name##Rev(int a) { return 2 op a; }

BY_DEFINE_TWICE(By_Mul, *)
"""


def answer(old: str, new: str) -> str | set[str]:
    """the scoper's answer, a set of names where it scoped the change"""
    got = scope(old, new)
    return got if got in ("same", "unscopable") else set(got.split("|"))


def test_the_fixture_parses():
    # every other test leans on this: a header this cannot read is unscopable whatever
    # changed in it
    assert parse(BASE).every()


def test_identical_headers_are_the_same():
    assert answer(BASE, BASE) == "same"


def test_a_comment_or_a_blank_line_is_the_same():
    new = BASE.replace("/* a helper nothing else calls */", "/* reworded */\n\n")
    assert answer(BASE, new) == "same"


def test_an_added_function_is_scoped_to_its_own_name():
    new = BASE + "\nstatic inline int By_New(int x) {\n    return By_Leaf(x) - 1;\n}\n"
    assert answer(BASE, new) == {"By_New"}


def test_an_added_function_under_a_new_conditional_is_scoped_to_its_own_name():
    new = BASE + (
        "\n#if PY_VERSION_HEX >= 0x030E0000\n"
        "static inline int By_New(int x) { return x; }\n"
        "#else\n"
        "static inline int By_New(int x) { return -x; }\n"
        "#endif\n"
    )
    assert answer(BASE, new) == {"By_New"}


def test_an_added_macro_and_type_are_scoped_to_their_names():
    new = BASE + "\n#define BY_NEW 3\ntypedef struct { int a; } By_NewBox;\n"
    assert answer(BASE, new) == {"BY_NEW", "By_NewBox"}


def test_a_changed_body_reaches_every_caller():
    # a module calling only `By_Caller` spells nothing that changed, and is affected
    new = BASE.replace("return x + BY_TAG;", "return x + BY_TAG + 1;")
    assert answer(BASE, new) == {"By_Leaf", "By_Caller"}


def test_a_removed_function_is_scoped_to_its_name():
    new = BASE.replace("static inline int By_Unrelated(int x) { return x * 2; }\n", "")
    assert answer(BASE, new) == {"By_Unrelated"}


def test_a_changed_variable_reaches_its_readers():
    new = BASE.replace("By_Cache = NULL;", "By_Cache = Py_None;") + (
        "static PyObject *By_Read(void) { return By_Cache; }\n"
    )
    old = BASE + "static PyObject *By_Read(void) { return By_Cache; }\n"
    assert answer(old, new) == {"By_Cache", "By_Read"}


def test_a_definition_moved_under_a_new_conditional_is_scoped_to_its_name():
    new = BASE.replace(
        "static inline int By_Unrelated(int x) { return x * 2; }\n",
        "#ifdef BY_EXTRA\nstatic inline int By_Unrelated(int x) { return x * 2; }\n#endif\n",
    )
    assert answer(BASE, new) == {"By_Unrelated"}


def test_a_changed_macro_is_unscopable():
    # every expansion site is affected and the name is the only trace an expansion leaves
    new = BASE.replace("#define BY_TAG 1", "#define BY_TAG 2")
    assert answer(BASE, new) == "unscopable"


def test_a_changed_function_like_macro_is_unscopable():
    new = BASE.replace("__builtin_expect(!!(x), 1)", "(x)")
    assert answer(BASE, new) == "unscopable"


def test_a_macro_made_function_like_by_a_removed_space_is_unscopable():
    new = BASE.replace("#define BY_TAG 1", "#define BY_TAG (1)")
    assert answer(BASE, new) == "unscopable"


def test_a_removed_macro_is_unscopable():
    new = BASE.replace("#define BY_TAG 1\n", "")
    assert answer(BASE, new) == "unscopable"


def test_a_changed_struct_layout_is_unscopable():
    new = BASE.replace("    int flags;\n", "    int flags;\n    int more;\n")
    assert answer(BASE, new) == "unscopable"


def test_an_added_include_is_unscopable():
    new = BASE.replace(
        "#include <Python.h>\n", "#include <Python.h>\n#include <errno.h>\n"
    )
    assert answer(BASE, new) == "unscopable"


def test_an_added_static_assert_is_unscopable():
    new = BASE + '_Static_assert(sizeof(int) == 4, "int");\n'
    assert answer(BASE, new) == "unscopable"


def test_a_moved_define_is_unscopable():
    # `By_Leaf` reads `BY_TAG` and its text is unchanged, but the `#define` now stands
    # below it, so it no longer expands there
    moved = BASE.replace("#define BY_TAG 1\n", "") + "#define BY_TAG 1\n"
    assert answer(BASE, moved) == "unscopable"


def test_an_added_constructor_is_unscopable():
    new = (
        BASE
        + "__attribute__((constructor)) static void By_AtLoad(void) { By_Leaf(1); }\n"
    )
    assert answer(BASE, new) == "unscopable"


def test_a_header_that_does_not_parse_is_unscopable():
    new = BASE + "static inline int By_Broken(int x) {\n"
    assert answer(BASE, new) == "unscopable"


def test_an_added_invocation_is_scoped_to_the_names_it_declares():
    # the pasted `By_AddRev` is spelled nowhere, and a module can still call it. the
    # macro could paste it anywhere it is expanded, so its every expansion comes along
    new = BASE + "BY_DEFINE_TWICE(By_Add, +)\n"
    assert answer(BASE, new) == {
        "By_Add",
        "By_AddRev",
        "BY_DEFINE_TWICE",
        "By_Mul",
        "By_MulRev",
    }


def test_a_pasting_macro_that_can_make_a_changed_name_is_in_the_answer():
    # `By_MulRev` changed, so `BY_DEFINE_TWICE` can stand for it wherever it expands
    old = BASE + "static inline int By_Twice(int a) { return By_MulRev(a); }\n"
    new = old.replace("BY_DEFINE_TWICE(By_Mul, *)", "BY_DEFINE_TWICE(By_Mul, +)")
    assert answer(old, new) == {"By_Mul", "By_MulRev", "BY_DEFINE_TWICE", "By_Twice"}


def test_an_unchanged_invocation_of_a_changed_callee_is_scoped():
    old = BASE + (
        "#define BY_CALLS(name, callee) static inline int name(int a) { return callee(a); }\n"
        "BY_CALLS(By_Through, By_Unrelated)\n"
    )
    new = old.replace("return x * 2;", "return x * 3;")
    assert answer(old, new) == {"By_Unrelated", "By_Through"}


def test_the_command_line_prints_one_line():
    with tempfile.TemporaryDirectory() as scratch:
        a = Path(scratch) / "a.h"
        b = Path(scratch) / "b.h"
        _ = a.write_text(BASE)
        _ = b.write_text(BASE.replace("#define BY_TAG 1", "#define BY_TAG 2"))
        script = Path(__file__).with_name("headerscope.py")
        result = subprocess.run(
            [sys.executable, str(script), str(a), str(b)],
            capture_output=True,
            text=True,
            check=True,
        )
    assert result.stdout == "unscopable\n"
    assert "BY_TAG" in result.stderr


def test_the_real_header_parses_into_definitions_it_can_name():
    # an `unknown` definition is unscopable the moment it differs, so each one the real
    # header holds is a change this cannot narrow
    header = Path(__file__).resolve().parents[2] / "crates/by_rt/include/by.h"
    parsed = parse(header.read_text(encoding="utf-8"))
    assert "unknown" not in {definition.kind for definition in parsed.every()}
