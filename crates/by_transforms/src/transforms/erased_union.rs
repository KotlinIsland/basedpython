//! erased union parameters (basedpython)
//!
//! a parameter typed as a union of specializations of one *erased* origin —
//! `list[int] | list[str]` — cannot be discriminated at runtime. the arms are
//! the same C-level `list`, and a builtin rejects the `__orig_class__` stamp,
//! so nothing on the value records which arm it is. a parametric test against
//! such a parameter used to answer `False` for every arm:
//!
//! ```by
//! def f(data: list[int] | list[str]):
//!     if data is list[int]: ...   # False
//!     if data is list[str]: ...   # False, too
//! ```
//!
//! the missing information is not a property of the *value* — it is a static
//! fact about the *binding*, known wherever the argument was written
//! (`f(list[int]())`). so it travels with the call rather than with the value:
//! the parameter becomes generic over the differing argument, and the type
//! parameter is [reified](super::reified_generic) so the call carries it.
//!
//! ```python
//! def f[reified T: (int, str)](data: list[T]):
//!     if data is list[int]: ...   # lowers to `T == int`
//! ```
//!
//! the rewrite is *uniform over the signature*: every function with such a
//! parameter is rewritten, whether or not its own body tests it. that is what
//! makes a chain work without whole-program analysis — an intermediate
//! function that only forwards the value (`middle` passing to `f`) is
//! rewritten from its own signature, and its cell forwards to the callee. it
//! also keeps the rewrite computable from a signature alone, so a caller in
//! another module reaches the same answer without reading the callee's body.
//!
//! this pass runs *before* the phase-0 AST passes, not among them: phase 0
//! re-parses and re-infers its input, so the passes that do the real work
//! ([`reified_generic`](super::reified_generic),
//! [`parametric_is`](super::parametric_is), and the call-site specialization
//! injector) see exactly the generic source a user could have written by hand,
//! and need no knowledge of this rewrite at all.
//!
//! # what is deliberately not rewritten
//!
//! the synthesized type parameter must never appear in anything basedpython
//! says to the user — it is an implementation detail of the lowering, and the
//! checker keeps seeing the union the user wrote. a rewrite that the solver
//! could not satisfy would surface as a transpile error naming the synthesized
//! parameter, so a parameter is only rewritten when every argument of every arm
//! has a runtime spelling (ty's [`erased_union`] answers `None` otherwise) and
//! the function is not one whose type parameters the user controls.
//!
//! [`erased_union`]: ty_python_semantic::ErasedUnion

use std::borrow::Cow;

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, PythonVersion, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange};

use crate::type_info::TypeInfo;

/// the stem of a synthesized type parameter's name. dunder-ish and namespaced
/// so it cannot collide with anything a user would write, and recognisable in
/// generated python as machinery rather than intent
const SYNTHESIZED_STEM: &str = "__by_erased";

/// one parameter to rewrite, and the type parameter standing for its
/// differing argument
struct Rewrite {
    /// the annotation to replace
    annotation: TextRange,
    /// the replacement text — `list[__by_erased_0]`
    replacement: String,
    /// the type parameter's declaration — `reified __by_erased_0: (int, str)`
    declaration: String,
}

/// build the rewrite for one qualifying parameter, or `None` when the
/// synthesized name would collide with a type parameter the function already
/// declares
fn plan(
    union: &ty_python_semantic::ErasedUnion,
    annotation: &Expr,
    index: usize,
    taken: &[String],
) -> Option<Rewrite> {
    let name = format!("{SYNTHESIZED_STEM}_{index}");
    if taken.contains(&name) {
        return None;
    }
    let width = union.fixed.len() + 1;
    let arguments: Vec<String> = (0..width)
        .map(|position| {
            if position == union.position {
                name.clone()
            } else {
                union
                    .fixed
                    .iter()
                    .find(|(at, _)| *at == position)
                    .map(|(_, text)| text.clone())
                    .unwrap_or_default()
            }
        })
        .collect();
    Some(Rewrite {
        annotation: annotation.range(),
        replacement: format!("{}[{}]", union.origin, arguments.join(", ")),
        // spelled as a *type mapping*, which is what a union of arms is: the
        // parameter ranges over exactly these types and no others. a bare
        // `: (int, str)` would be a tuple *bound* instead, which no arm satisfies
        declaration: format!("reified {name} in ({})", union.arms.join(", ")),
    })
}

/// the names a function's own type parameter list already binds
fn declared_type_params(function: &StmtFunctionDef) -> Vec<String> {
    function
        .type_params
        .as_deref()
        .map(|params| {
            params
                .type_params
                .iter()
                .map(|param| param.name().id.to_string())
                .collect()
        })
        .unwrap_or_default()
}

struct Reify<'ti> {
    types: &'ti dyn TypeInfo,
    edits: Vec<(TextRange, String)>,
}

impl Reify<'_> {
    fn visit_function(&mut self, function: &StmtFunctionDef) {
        let taken = declared_type_params(function);
        let mut rewrites = Vec::new();
        for parameter in &function.parameters {
            // the parser injects a zero-width synthetic `self` for `init(...)`
            // parameter modifiers; it has no annotation to read anyway, but an
            // empty range must never be edited
            if parameter.range().is_empty() {
                continue;
            }
            let Some(annotation) = parameter.annotation() else {
                continue;
            };
            let Some(union) = self.types.erased_union(annotation) else {
                continue;
            };
            if let Some(rewrite) = plan(&union, annotation, rewrites.len(), &taken) {
                rewrites.push(rewrite);
            }
        }
        if rewrites.is_empty() {
            return;
        }

        let declarations = rewrites
            .iter()
            .map(|rewrite| rewrite.declaration.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        match function.type_params.as_deref() {
            // extend the existing list, keeping the user's parameters first so
            // an explicit `f[int]` still binds what it always bound
            Some(params) => {
                let close = params.range().end() - ruff_text_size::TextSize::from(1);
                self.edits
                    .push((TextRange::empty(close), format!(", {declarations}")));
            }
            None => {
                self.edits.push((
                    TextRange::empty(function.name.range().end()),
                    format!("[{declarations}]"),
                ));
            }
        }
        for rewrite in &rewrites {
            self.edits
                .push((rewrite.annotation, rewrite.replacement.clone()));
        }
    }
}

impl<'ast> Visitor<'ast> for Reify<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            self.visit_function(function);
        }
        walk_stmt(self, stmt);
    }
}

/// Give every erased-union parameter in `source` a reified type parameter.
/// Returns a borrowed `Cow` when there are none — the overwhelmingly common
/// case, and the signal to the caller that its view of the source is current.
///
/// `suite` must be the parse the `types` model was built from: annotations are
/// looked up by AST node identity.
/// Reification compiles the type parameter into a PEP 695 closure cell, which
/// needs native type-parameter syntax in the generated python. Below 3.12 the
/// rewrite could only produce a hard error, so the parameter keeps its union
/// and the test keeps the (unhelpful but working) probe it always had —
/// turning running code into a transpile failure would be a far worse trade
pub(crate) fn reify<'a>(
    source: &'a str,
    suite: &[Stmt],
    types: &dyn TypeInfo,
    min_version: PythonVersion,
) -> Cow<'a, str> {
    if min_version < PythonVersion::PY312 {
        return Cow::Borrowed(source);
    }
    let mut visitor = Reify {
        types,
        edits: Vec::new(),
    };
    for stmt in suite {
        visitor.visit_stmt(stmt);
    }
    if visitor.edits.is_empty() {
        return Cow::Borrowed(source);
    }

    // a type-parameter insertion is a zero-width point at the def header and an
    // annotation replacement covers one annotation, so the ranges are disjoint;
    // sorting is all that is needed to splice them in a single forward pass
    visitor.edits.sort_by_key(|(range, _)| range.start());
    let mut output = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (range, replacement) in visitor.edits {
        output.push_str(&source[cursor..range.start().into()]);
        output.push_str(&replacement);
        cursor = range.end().into();
    }
    output.push_str(&source[cursor..]);
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn out(input: &str) -> String {
        transpile(
            input,
            &Config {
                min_version: PythonVersion::PY313,
                ..Config::test_default()
            },
        )
        .unwrap()
    }

    #[test]
    fn erased_union_parameter_becomes_generic() {
        let out = out(indoc! {"
            def f(data: list[int] | list[str]): ...
        "});
        assert!(
            out.contains("def f[__by_erased_0: (int, str)](data: list[__by_erased_0]):"),
            "the union parameter is generic over its differing argument: {out}"
        );
        assert!(
            out.contains("@generic  # basedpython: reified"),
            "the synthesized parameter is reified: {out}"
        );
    }

    #[test]
    fn the_whole_chain_reifies_from_signatures_alone() {
        // `middle` never tests its parameter — it only forwards. it is rewritten
        // from its own signature, which is what makes the chain work without
        // looking at any other function's body
        let out = out(indoc! {"
            def f(data: list[int] | list[str]):
                if data is list[int]:
                    return

            def middle(data: list[int] | list[str]):
                f(data)

            def main():
                middle(list[int]())
        "});
        assert!(
            out.contains("if (__by_erased_0 == int):"),
            "the test reads the reified cell: {out}"
        );
        assert!(
            out.contains("f[__by_erased_0](data)"),
            "the forwarding hop passes its own cell along: {out}"
        );
        assert!(
            out.contains("middle[int](list[int]())"),
            "the outermost call solves the specialization: {out}"
        );
    }

    #[test]
    fn a_user_generic_union_is_untouched() {
        // `A` carries `__orig_class__`, so the runtime can already discriminate
        // the arms — there is nothing to reify
        let out = out(indoc! {"
            class A[T]:
                def __init__(self, t: T):
                    self.v: list[T] = [t]

            def f(data: A[int] | A[str]): ...
        "});
        assert!(
            !out.contains("__by_erased"),
            "a union the runtime can discriminate is left alone: {out}"
        );
    }

    #[test]
    fn a_single_specialization_is_untouched() {
        // one arm is already decidable statically; there is nothing to tell apart
        let out = out(indoc! {"
            def f(data: list[int]): ...
        "});
        assert!(
            !out.contains("__by_erased"),
            "a lone specialization needs no type parameter: {out}"
        );
    }

    #[test]
    fn a_union_of_different_origins_is_untouched() {
        // `isinstance` alone separates a list from a set, so the arms are
        // already discriminable without carrying the specialization
        let out = out(indoc! {"
            def f(data: list[int] | set[int]): ...
        "});
        assert!(
            !out.contains("__by_erased"),
            "different origins are discriminable by isinstance: {out}"
        );
    }

    #[test]
    fn a_fixed_argument_position_is_preserved() {
        // only the differing position becomes the type parameter; the arms'
        // shared argument has to be written back where it was
        let out = out(indoc! {"
            def f(data: dict[str, int] | dict[str, bool]): ...
        "});
        assert!(
            out.contains("data: dict[str, __by_erased_0]"),
            "the agreed-on argument stays put: {out}"
        );
    }

    #[test]
    fn an_existing_type_parameter_list_is_extended() {
        let out = out(indoc! {"
            def f[U](other: U, data: list[int] | list[str]) -> U:
                return other
        "});
        assert!(
            out.contains("def f[U, __by_erased_0: (int, str)]"),
            "the user's parameters stay first: {out}"
        );
    }

    #[test]
    fn a_colliding_name_is_left_alone() {
        // the synthesized name is already bound, so rewriting would capture it.
        // refusing is correct: the parameter keeps the union it always had
        let out = out(indoc! {"
            def f[__by_erased_0](data: list[int] | list[str]): ...
        "});
        assert!(
            out.contains("data: list[int] | list[str]"),
            "a collision leaves the annotation untouched: {out}"
        );
    }
}
