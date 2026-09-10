//! Lowering for conformance extensions (`extension str(A):`) and the dispatch
//! they make possible.
//!
//! Python has no way to say "this existing type satisfies that interface" for a
//! type you do not own — you cannot monkey-patch `str`, and `abc.register`
//! provides no members. So a conformance lowers to a *witness table*: a mapping
//! from each of the interface's requirements to the backing function that
//! answers it, registered against the pair when the declaring module is
//! imported.
//!
//! ```text
//! extension str(A):                  def _by_ext__str__bar(self): ...
//!     override def bar(self): ...  →
//!                                    _by_conform(A, str, {"bar": _by_ext__str__bar})
//! ```
//!
//! Two things then read that table. A requirement accessed on a receiver the
//! checker typed as the interface cannot be a plain attribute — the value may
//! be a conforming type that carries no such member — so it goes through
//! the runtime's dispatcher, which falls back to the attribute when
//! nothing registered one. And `x is A` answers from the table first, so a
//! conforming value tests positive even though `isinstance` would not.
//!
//! Everything else about a conformance extension is an ordinary extension: its
//! members lower to the same backing functions, and a call on a receiver whose
//! *concrete* type the checker knows resolves straight to one, with no table
//! lookup at all.

use std::collections::BTreeSet;

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::ConversionImport;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::extension::spine_has_optional;
use crate::type_info::TypeInfo;

/// The registry helpers a module touching conformances calls.
///
/// The whole set at either site: a module that declares a conformance and one
/// that tests one are reading and writing the same registry, so the split is
/// not worth the risk of a site missing the one name it turned out to need
pub(crate) const WITNESS_HELPERS: &[crate::runtime::Helper] = &[
    crate::runtime::CONFORM,
    crate::runtime::CONFORMS,
    crate::runtime::WITNESS,
    crate::runtime::WITNESS_CLASS,
    crate::runtime::WITNESS_GET,
];

/// the `from <module> import <name> as <alias>` a cross-module interface
/// spelling needs
fn import_line(import: &ConversionImport) -> String {
    if import.alias == import.name {
        format!("from {} import {}", import.module, import.name)
    } else {
        format!(
            "from {} import {} as {}",
            import.module, import.name, import.alias
        )
    }
}

/// the `_by_conform(...)` registrations a conformance extension emits, appended
/// to its block's lowering so they run once the backing functions and the
/// interface are both bound.
///
/// A registration with an empty table is still emitted: the entry is what makes
/// `x is A` answer true for the conforming type, whether or not any requirement
/// needed a function of its own
pub(crate) fn registration_fragments(
    class: &ruff_python_ast::StmtClassDef,
    types: &dyn TypeInfo,
    ctx: &mut PassContext,
    fragments: &mut Vec<Fragment>,
    had_members: bool,
) {
    let registrations = types.conformance_registrations(class);
    if registrations.is_empty() {
        return;
    }
    ctx.runtime.extend(WITNESS_HELPERS.iter().copied());
    let mut first = !had_members;
    for registration in registrations {
        if let Some(import) = &registration.import {
            ctx.required_imports.push(import_line(import));
        }
        ctx.required_imports.extend(registration.imports);
        if !first {
            fragments.push(Fragment::Lit("\n\n".to_owned()));
        }
        first = false;
        let table = registration
            .entries
            .iter()
            .map(|(member, function)| format!("\"{member}\": {function}"))
            .collect::<Vec<_>>()
            .join(", ");
        fragments.push(Fragment::Lit(format!(
            "_by_conform({}, {}, {{{table}}})",
            registration.interface, class.name,
        )));
    }
}

/// rewrites a requirement read off an interface-typed receiver to a witness
/// lookup
struct WitnessDispatchLower<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    imports: BTreeSet<String>,
    errors: Vec<String>,
}

impl<'ast> Visitor<'ast> for WitnessDispatchLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Attribute(attr) = expr
            && !attr.ctx.is_load()
            && self.types.witness_dispatch(attr).is_some()
        {
            // a store cannot be forwarded: the witness that answers the
            // requirement is a function, and there is nowhere to put the value
            self.errors.push(format!(
                "protocol member `{}` cannot be assigned through the interface; \
                 the conformance answers it with a computed member",
                attr.attr,
            ));
        }
        if let Expr::Attribute(attr) = expr
            && attr.ctx.is_load()
            && let Some(dispatch) = self.types.witness_dispatch(attr)
        {
            if attr.optional || spine_has_optional(&attr.value) {
                self.errors.push(format!(
                    "protocol member `{}` cannot be reached through an optional chain yet",
                    attr.attr,
                ));
            } else {
                if let Some(import) = &dispatch.import {
                    self.imports.insert(import_line(import));
                }
                let dispatcher = match dispatch.kind {
                    ty_python_semantic::WitnessKind::Property => "_by_witness_get(",
                    ty_python_semantic::WitnessKind::ClassMethod => "_by_witness_class(",
                    ty_python_semantic::WitnessKind::Method => "_by_witness(",
                };
                self.edits.push((
                    attr.range(),
                    vec![
                        Fragment::Lit(dispatcher.to_owned()),
                        Fragment::Src(attr.value.range()),
                        Fragment::Lit(format!(
                            ", {}, \"{}\")",
                            dispatch.interface, dispatch.member
                        )),
                    ],
                ));
            }
        }
        walk_expr(self, expr);
    }
}

pub(crate) struct WitnessDispatchPass;

impl TypeAwarePass for WitnessDispatchPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = WitnessDispatchLower {
            types,
            edits: Vec::new(),
            imports: BTreeSet::new(),
            errors: Vec::new(),
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.errors.extend(inner.errors);
        if inner.edits.is_empty() {
            return;
        }
        ctx.runtime.extend(WITNESS_HELPERS.iter().copied());
        ctx.required_imports.extend(inner.imports);
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    const SHOW: &str = "protocol Show:\n    def show(self) -> str\n\nextension str(Show):\n    override def show(self) -> str:\n        return self\n\n";

    #[test]
    fn a_conformance_registers_its_witness_table() {
        let out = check(SHOW);
        assert!(
            out.contains(
                "def _by_ext__str__show(self):  # basedpython: extension method str(Show)"
            ),
            "got:\n{out}"
        );
        assert!(
            out.contains("_by_conform(Show, str, {\"show\": _by_ext__str__show})"),
            "got:\n{out}"
        );
        assert!(
            out.contains("def _by_conforms(value, interface"),
            "got:\n{out}"
        );
    }

    /// the registration has to run after the interface's own class statement —
    /// it names it — while the backing functions are hoisted above everything
    #[test]
    fn the_registration_follows_the_interface_declaration() {
        let out = check(SHOW);
        let interface_at = out.find("class Show(Protocol):").expect("protocol emitted");
        let register_at = out
            .find("_by_conform(Show, str")
            .expect("registration emitted");
        assert!(interface_at < register_at, "got:\n{out}");
    }

    /// a requirement read off an interface-typed receiver cannot be a plain
    /// attribute: the value may carry no such member of its own
    #[test]
    fn a_requirement_dispatches_through_the_table() {
        let out = check(&format!(
            "{SHOW}def render(value: Show) -> str:\n    return value.show()\n"
        ));
        assert!(
            out.contains("return _by_witness(value, Show, \"show\")()"),
            "got:\n{out}"
        );
    }

    /// an inherent member of a protocol extension is not a requirement, so it
    /// resolves statically to its backing function like any other extension member
    #[test]
    fn an_inherent_protocol_member_stays_static() {
        let out = check(
            "protocol Show:\n    def show(self) -> str\n\nextension Show:\n    def shout(self) -> str:\n        return self.show().upper()\n\nextension str(Show):\n    override def show(self) -> str:\n        return self\n\ndef render(value: Show) -> str:\n    return value.shout()\n",
        );
        assert!(
            out.contains("return _by_ext__Show__shout(value)"),
            "got:\n{out}"
        );
        // the default body's own call to the requirement still dispatches
        assert!(
            out.contains("return _by_witness(self, Show, \"show\")().upper()"),
            "got:\n{out}"
        );
    }

    /// a receiver whose concrete type the checker knows needs no table at all
    #[test]
    fn a_concrete_receiver_calls_the_backing_function_directly() {
        let out = check(&format!("{SHOW}print(\"hi\".show())\n"));
        assert!(
            out.contains("print(_by_ext__str__show(\"hi\"))"),
            "got:\n{out}"
        );
        assert!(!out.contains("_by_witness(\"hi\""), "got:\n{out}");
    }

    /// `isinstance` cannot answer a protocol test, and a conforming type is not a
    /// subclass of anything — the registry answers first
    #[test]
    fn an_is_test_against_a_conformed_protocol_uses_the_registry() {
        let out = check(&format!(
            "{SHOW}def describe(value: object) -> str:\n    if value is Show:\n        return value.show()\n    return \"\"\n"
        ));
        assert!(
            out.contains("if _by_conforms(value, Show, (\"show\", )):"),
            "got:\n{out}"
        );
    }

    /// an `is`-test against a protocol nothing conforms to keeps its ordinary
    /// lowering — no registry, no runtime
    #[test]
    fn an_is_test_without_a_conformance_is_untouched() {
        let out = check(
            "protocol Show:\n    def show(self) -> str\n\ndef describe(value: object) -> str:\n    if value is Show:\n        return \"yes\"\n    return \"\"\n",
        );
        assert!(!out.contains("_by_conforms"), "got:\n{out}");
    }

    /// a requirement the conforming type already answers needs no table entry:
    /// the dispatcher falls back to the attribute
    #[test]
    fn a_natively_answered_requirement_gets_no_entry() {
        let out =
            check("protocol Sized:\n    def __len__(self) -> int\n\nextension str(Sized): ...\n");
        assert!(out.contains("_by_conform(Sized, str, {})"), "got:\n{out}");
    }

    /// a data member is read through the table rather than called through it
    #[test]
    fn a_property_requirement_reads_through_the_table() {
        let out = check(
            "protocol Named:\n    @property\n    def name(self) -> str\n\nclass Widget: ...\n\nextension Widget(Named):\n    @property\n    override def name(self) -> str:\n        return \"w\"\n\ndef label(value: Named) -> str:\n    return value.name\n",
        );
        assert!(
            out.contains("return _by_witness_get(value, Named, \"name\")"),
            "got:\n{out}"
        );
    }

    /// a conformance whose block is empty still registers: the entry is what
    /// makes the type test positive
    #[test]
    fn an_empty_conformance_still_registers() {
        let out = check(
            "protocol Show:\n    def show(self) -> str\n\nextension Show:\n    def show(self) -> str:\n        return \"?\"\n\nextension str(Show): ...\n",
        );
        assert!(
            out.contains("_by_conform(Show, str, {\"show\": _by_ext__Show__show})"),
            "got:\n{out}"
        );
    }
}
