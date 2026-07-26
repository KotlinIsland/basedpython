//! Lowering for `implementation` declarations and their conversion sites.
//!
//! An `implementation A for B:` block states that `B` satisfies `A`. Python has
//! no such statement, so the block lowers to a *witness* class deriving the
//! interface, and every conversion site — a position where the checker accepted a
//! `B` for an `A` — wraps its value in that class:
//!
//! ```text
//! implementation A for B:          class _by_impl__A__B(_by_Implementation, A):
//!     override def f(self):    →       def f(self):
//!         print(self.a)                    print(self.a)
//!
//! takes_a(b)                       takes_a(_by_impl__A__B(b))
//! ```
//!
//! Three things fall out of deriving the interface rather than reimplementing it:
//! the interface's default method bodies are inherited, `super()` in a block
//! member works with no special handling, and `isinstance(witness, A)` is true.
//! The shared `_by_Implementation` base does the rest — it holds the wrapped
//! object as `__implemented__` and forwards attribute reads and writes to it, so
//! `self.a` reaches `B`'s state with the member bodies passed through verbatim.
//!
//! The witness *name* comes from the type checker rather than being derived here,
//! so the class this pass emits and the constructor the conversion pass inserts
//! can never disagree — the same reason the conversions themselves are resolved
//! by ty (see `TypeInfo::implementation_call_conversions`).

use ruff_python_ast::visitor::Visitor;
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_semantic::ImplementationConversion;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::source_util::line_start;
use crate::type_info::TypeInfo;

/// the marker-comment prefix that ties a witness class to its implementation
pub(crate) const IMPLEMENTATION_MARKER: &str = "# basedpython: implementation";

/// the shared base's class name, recognised by the reverse transform
pub(crate) const IMPLEMENTATION_RUNTIME_NAME: &str = "_by_Implementation";

/// the prefix ty mangles an *anonymous* implementation's witness class name with.
/// the reverse transform tests it to tell an inserted conversion (which unwraps)
/// from an explicit call of a named implementation (which stays)
pub(crate) const WITNESS_NAME_PREFIX: &str = "_by_impl__";

/// the shared base every witness class derives: it holds the implemented object
/// and forwards to it. attribute *reads* fall through `__getattr__` only for names
/// the witness itself does not define, which is exactly the member precedence the
/// checker models (block, then interface, then implemented type).
///
/// only what must always win lives here. the delegating `__eq__` / `__hash__` /
/// `__repr__` are emitted into each witness class instead, and only when the
/// interface leaves them to `object` — carried on this base they would sit ahead
/// of the interface in the MRO and silently shadow an interface's own version
/// (see [`delegated_dunder_source`])
pub(crate) const IMPLEMENTATION_RUNTIME: &str = "\
class _by_Implementation:
    __slots__ = (\"__implemented__\",)

    def __init__(self, implemented):
        object.__setattr__(self, \"__implemented__\", implemented)

    def __getattr__(self, name):
        if name == \"__implemented__\":
            raise AttributeError(name)
        return getattr(self.__implemented__, name)

    def __setattr__(self, name, value):
        setattr(self.__implemented__, name, value)
";

/// the body of one delegating dunder, indented for a class body. `__eq__` and
/// `__hash__` always come as a pair: python sets `__hash__ = None` on a class that
/// defines `__eq__` alone, which would make the witness unhashable
fn delegated_dunder_source(name: &str) -> &'static str {
    // written line by line rather than with a `\`-continued literal: that
    // continuation strips the following line's leading whitespace, which is the
    // method indentation these blocks need
    match name {
        "__eq__" => concat!(
            "    def __eq__(self, other):\n",
            "        if isinstance(other, _by_Implementation):\n",
            "            other = other.__implemented__\n",
            "        return self.__implemented__ == other\n",
        ),
        "__hash__" => concat!(
            "    def __hash__(self):\n",
            "        return hash(self.__implemented__)\n",
        ),
        "__repr__" => concat!(
            "    def __repr__(self):\n",
            "        return repr(self.__implemented__)\n",
        ),
        _ => "",
    }
}

/// the member names an implementation block defines itself
fn class_node_members(class: &ast::StmtClassDef) -> Vec<&str> {
    class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) => Some(function.name.as_str()),
            Stmt::AnnAssign(annotated) => Some(annotated.target.as_name_expr()?.id.as_str()),
            Stmt::Assign(assign) => match assign.targets.as_slice() {
                [Expr::Name(name)] => Some(name.id.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// lower each `implementation` block to its witness class
pub(crate) struct ImplementationBlockPass<'a> {
    source: &'a str,
}

impl<'a> ImplementationBlockPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }

    fn lower_block(
        &self,
        class: &ast::StmtClassDef,
        witness: &str,
        header: &ast::ImplementationHeader,
        delegated: &[&'static str],
        ctx: &mut PassContext,
    ) {
        let source = self.source;

        // the header spelling, carried on the class so the reverse transform can
        // re-sugar the block (interface, implemented type, bounds and `as` name)
        let header_end = class
            .type_params
            .as_deref()
            .map_or(class.name.range().end(), |params| params.range.end());
        let spelling = format!(
            "{} for {}{}",
            &source[usize::from(header.interface.range().start())
                ..usize::from(header.interface.range().end())],
            &source[usize::from(class.name.range().start())..usize::from(header_end)],
            match &header.witness {
                Some(name) => format!(" as {name}"),
                None => String::new(),
            }
        );

        let mut fragments: Vec<Fragment> = vec![
            Fragment::Lit(format!("class {witness}(_by_Implementation, ")),
            // the interface passes through as source so its own lowerings (a
            // dotted path, a subscript, `dynamic`, …) still compose
            Fragment::Src(header.interface.range()),
            Fragment::Lit(format!("):  {IMPLEMENTATION_MARKER} {spelling}\n")),
            // a witness has no state of its own; the wrapped object holds it all
            Fragment::Lit("    __slots__ = ()\n".to_owned()),
        ];

        // a witness and the object it wraps should be interchangeable as dict keys,
        // in sets and in `repr` output — but only where the interface has no
        // opinion, and never over a member the block supplies itself
        let block_members: Vec<&str> = class_node_members(class);
        for dunder in delegated {
            if block_members.contains(dunder) {
                continue;
            }
            fragments.push(Fragment::Lit(delegated_dunder_source(dunder).to_owned()));
        }

        let (Some(first_stmt), Some(last_stmt)) = (class.body.first(), class.body.last()) else {
            fragments.push(Fragment::Lit("    pass".to_owned()));
            ctx.template_edits.push((class.range, fragments));
            return;
        };

        let body_line_start = line_start(source, first_stmt.range().start());
        let inline = source[usize::from(body_line_start)..usize::from(first_stmt.range().start())]
            .contains(|c: char| !c.is_whitespace());
        if inline {
            // `implementation A for B: override def f(self): ...` — re-indent the
            // single-line body onto its own line
            fragments.push(Fragment::Lit("    ".to_owned()));
            fragments.push(Fragment::Src(TextRange::new(
                first_stmt.range().start(),
                last_stmt.range().end(),
            )));
        } else {
            fragments.push(Fragment::Src(TextRange::new(
                body_line_start,
                last_stmt.range().end(),
            )));
        }

        ctx.template_edits.push((class.range, fragments));
    }
}

impl TypeAwarePass for ImplementationBlockPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut lowered_any = false;
        for stmt in stmts {
            let Stmt::ClassDef(class) = stmt else {
                continue;
            };
            let Some(header) = class.implementation.as_deref() else {
                continue;
            };
            // the checker owns the witness name: an anonymous implementation's is
            // mangled from the interface and implemented type, and both sides must
            // spell it identically
            let Some(witness) = types.implementation_witness_name(class) else {
                ctx.errors.push(format!(
                    "`implementation` of `{}` could not be resolved (offset {})",
                    class.name,
                    u32::from(class.range.start()),
                ));
                continue;
            };
            let delegated = types.implementation_delegated_dunders(class);
            self.lower_block(class, &witness, header, &delegated, ctx);
            lowered_any = true;
        }
        if lowered_any {
            ctx.required_imports.push(IMPLEMENTATION_RUNTIME.to_owned());
        }
    }
}

/// wrap every conversion site's value in the witness class the checker resolved
pub(crate) struct ImplementationConversionPass<'a> {
    source: &'a str,
}

impl<'a> ImplementationConversionPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }
}

/// split `[start, end)` into the span without its final character and that
/// character's text. Used to keep a `Src` fragment from ending on a position
/// where a sibling insertion lives
fn shave_last_char(source: &str, start: TextSize, end: TextSize) -> (TextRange, String) {
    let text = &source[usize::from(start)..usize::from(end)];
    match text.chars().next_back() {
        Some(last) => (
            TextRange::new(start, end - TextSize::of(last)),
            last.to_string(),
        ),
        None => (TextRange::new(start, end), String::new()),
    }
}

/// the first character of `[start, limit)` and the position after it. Used to
/// keep a `Src` fragment from starting on a position where a sibling insertion
/// lives
fn shave_first_char(source: &str, start: TextSize, limit: TextSize) -> (String, TextSize) {
    let text = &source[usize::from(start)..usize::from(limit)];
    match text.chars().next() {
        Some(first) => (first.to_string(), start + TextSize::of(first)),
        None => (String::new(), start),
    }
}

impl TypeAwarePass for ImplementationConversionPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut collector = ConversionCollector {
            types,
            sites: Vec::new(),
            function_depth: 0,
        };
        for stmt in stmts {
            collector.visit_stmt(stmt);
        }
        if collector.sites.is_empty() {
            return;
        }

        // where each witness class this module declares will be emitted, so a
        // conversion that runs at import time can be checked against it
        let declarations: Vec<(String, TextRange)> = stmts
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::ClassDef(class) if class.is_implementation() => types
                    .implementation_witness_name(class)
                    .map(|witness| (witness, class.range)),
                _ => None,
            })
            .collect();

        for site in collector.sites {
            for (value_range, info) in &site.wraps {
                if let Some(module) = &info.import_from {
                    ctx.required_imports
                        .push(format!("from {module} import {}", info.witness));
                    continue;
                }
                // python binds a class name when its statement executes, so a
                // conversion that runs at import time cannot precede the block it
                // converts through. inside a function body the name is resolved at
                // call time, so order does not matter there
                if site.runs_at_import
                    && let Some((_, declared_at)) = declarations
                        .iter()
                        .find(|(witness, _)| *witness == info.witness)
                    && value_range.start() < declared_at.start()
                {
                    ctx.errors.push(format!(
                        "the `implementation` converting this value is declared later in \
                        the module (offset {}); move it above the conversion, or wrap the \
                        value explicitly with a named implementation",
                        u32::from(value_range.start()),
                    ));
                }
            }

            // ONE edit spanning the whole argument list, not one per argument: a
            // peer pass that rewrites an argument outright (`x cast T`) claims
            // exactly the argument's range, and two replacements over the same
            // range collide — the loser is dropped silently. Claiming the enclosing
            // span makes every such edit strictly inside a `Src` fragment, where it
            // is materialized instead. Everything but the inserted constructors
            // passes through as source, so comments and formatting survive
            let mut fragments: Vec<Fragment> = Vec::new();
            let mut cursor = site.claim_range.start();
            for (value_range, info) in &site.wraps {
                // No `Src` fragment may *end* exactly where another *begins*, or a
                // sibling zero-width insertion at that position is emitted twice —
                // once as the left span's end, once as the right span's start (see
                // `materialize_fragments`). An operand's boundaries are exactly such
                // positions: `x!` inserts `_force_unwrap(` at the operand's start and
                // its `)` at the end. So the punctuation character on each side of
                // the operand is copied as a literal, moving the `Src` boundary off
                // the insertion point
                let (lead, opening_punctuation) =
                    shave_last_char(self.source, cursor, value_range.start());
                fragments.push(Fragment::Src(lead));
                fragments.push(Fragment::Lit(opening_punctuation));
                fragments.push(Fragment::Lit(format!("{}(", info.witness)));
                fragments.push(Fragment::Src(*value_range));
                fragments.push(Fragment::Lit(")".to_owned()));
                let (closing_punctuation, rest) =
                    shave_first_char(self.source, value_range.end(), site.claim_range.end());
                fragments.push(Fragment::Lit(closing_punctuation));
                cursor = rest;
            }
            fragments.push(Fragment::Src(TextRange::new(
                cursor,
                site.claim_range.end(),
            )));
            ctx.template_edits.push((site.claim_range, fragments));
        }
    }
}

/// the conversions one site needs, and whether that site runs at import time
struct SiteConversions {
    /// the span the edit claims: wide enough to strictly contain any peer edit
    /// over one of the wrapped values — a call's whole argument list, or an
    /// annotated assignment's value together with the `=` before it
    claim_range: TextRange,
    /// `(value range, conversion)` in source order
    wraps: Vec<(TextRange, ImplementationConversion)>,
    runs_at_import: bool,
}

struct ConversionCollector<'a> {
    types: &'a dyn TypeInfo,
    sites: Vec<SiteConversions>,
    /// how many function bodies enclose the node being visited. a call inside one
    /// resolves its names when the function runs, not when the module loads
    function_depth: usize,
}

impl<'ast> ast::visitor::Visitor<'ast> for ConversionCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // an annotated assignment, an attribute assignment, or a `return`: the
        // claim starts before the value (so a peer edit over it nests) and runs to
        // the last converted range
        let mut wraps = self.types.implementation_statement_conversions(stmt);
        if !wraps.is_empty() {
            wraps.sort_by_key(|(range, _)| range.start());
            let claim_start = match stmt {
                Stmt::AnnAssign(assignment) => assignment.annotation.range().end(),
                Stmt::Assign(assignment) => assignment
                    .targets
                    .last()
                    .map_or(stmt.range().start(), |target| target.range().end()),
                // `return <value>`: start inside the keyword so the span opens
                // before the value without depending on the whitespace between
                _ => stmt.range().start() + TextSize::of("return"),
            };
            let claim_end = wraps
                .last()
                .map_or(stmt.range().end(), |(range, _)| range.end());
            self.sites.push(SiteConversions {
                claim_range: TextRange::new(claim_start, claim_end),
                wraps,
                runs_at_import: self.function_depth == 0,
            });
        }
        if matches!(stmt, Stmt::FunctionDef(_)) {
            // the decorators and signature still run eagerly; only the body defers
            self.function_depth += 1;
            ast::visitor::walk_stmt(self, stmt);
            self.function_depth -= 1;
            return;
        }
        ast::visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Lambda(_) = expr {
            // a lambda body resolves its names when it is called, like a `def`
            self.function_depth += 1;
            ast::visitor::walk_expr(self, expr);
            self.function_depth -= 1;
            return;
        }
        if let Expr::Call(call) = expr {
            let mut wraps = self.types.implementation_call_conversions(call);
            if !wraps.is_empty() {
                wraps.sort_by_key(|(range, _)| range.start());
                self.sites.push(SiteConversions {
                    claim_range: call.arguments.range(),
                    wraps,
                    runs_at_import: self.function_depth == 0,
                });
            }
        }
        ast::visitor::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    const IFACE: &str = "\
abstract class A:
    abstract def f(self) -> int: ...

class B:
    a: int = 3

";

    #[test]
    fn block_lowers_to_a_witness_class() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n"
        ));
        assert!(
            out.contains("class _by_impl__A__B(_by_Implementation, A):"),
            "got:\n{out}"
        );
        assert!(out.contains("class _by_Implementation:"), "got:\n{out}");
        assert!(out.contains("return self.a"), "got:\n{out}");
    }

    #[test]
    fn named_implementation_keeps_its_name() {
        let out = check(&format!(
            "{IFACE}implementation A for B as BAsA:\n    override def f(self) -> int:\n        return self.a\n"
        ));
        assert!(
            out.contains("class BAsA(_by_Implementation, A):"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_conversion_site_wraps_its_argument() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef takes_a(a: A) -> int:\n    return a.f()\n\nb = B()\ntakes_a(b)\n"
        ));
        assert!(out.contains("takes_a(_by_impl__A__B(b))"), "got:\n{out}");
    }

    #[test]
    fn an_explicit_witness_call_is_left_alone() {
        let out = check(&format!(
            "{IFACE}implementation A for B as BAsA:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef takes_a(a: A) -> int:\n    return a.f()\n\nb = B()\ntakes_a(BAsA(b))\n"
        ));
        assert!(out.contains("takes_a(BAsA(b))"), "got:\n{out}");
        assert!(!out.contains("BAsA(BAsA("), "double-wrapped:\n{out}");
    }

    #[test]
    fn a_value_that_needs_no_conversion_is_left_alone() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef takes_b(x: B) -> int:\n    return x.a\n\nb = B()\ntakes_b(b)\n"
        ));
        assert!(out.contains("takes_b(b)"), "got:\n{out}");
    }

    #[test]
    fn lowerings_inside_a_member_body_still_compose() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        \
             return self.a if self.a else 0\n"
        ));
        assert!(out.contains("class _by_impl__A__B("), "got:\n{out}");
        assert!(out.contains("if self.a else 0"), "got:\n{out}");
    }

    /// a peer pass that rewrites the argument outright claims exactly the
    /// argument's range. the wrap must still survive — it claims the enclosing
    /// argument list, so the peer edit is materialized inside it
    #[test]
    fn a_wrap_composes_with_a_peer_rewrite_of_the_same_argument() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef takes_a(a: A) -> int:\n    return a.f()\n\ntakes_a(B() cast B)\n"
        ));
        assert!(
            out.contains("takes_a(_by_impl__A__B(cast(B, B())))"),
            "the wrap must sit outside the cast, got:\n{out}"
        );
    }

    /// an operand lowering that inserts around the value (`x!` → `_force_unwrap(x)`)
    /// sits at the wrap's fragment boundaries; it must be emitted exactly once
    #[test]
    fn a_wrap_composes_with_an_operand_insertion() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef takes_a(a: A) -> int:\n    return a.f()\n\ndef maybe() -> B?:\n    return B()\n\
             \ndef main():\n    takes_a(maybe()!)\n"
        ));
        assert!(
            out.contains("takes_a(_by_impl__A__B(_force_unwrap(maybe())))"),
            "got:\n{out}"
        );
        assert_eq!(
            out.matches("_force_unwrap(").count(),
            2,
            "emitted twice or not at all:\n{out}"
        );
    }

    /// several conversions in one call, with a comment and a keyword argument:
    /// everything but the inserted constructors passes through as source
    #[test]
    fn several_conversions_in_one_call_keep_the_source_intact() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef two(x: A, y: A, label: str = \"\") -> int:\n    return x.f()\n\
             \ndef main():\n    b = B()\n    two(\n        b,  # first\n        b,\n        label=\"k\",\n    )\n"
        ));
        assert!(out.contains("# first"), "comment dropped:\n{out}");
        assert_eq!(
            out.matches("_by_impl__A__B(b)").count(),
            2,
            "both arguments should convert:\n{out}"
        );
        assert!(out.contains("label=\"k\""), "got:\n{out}");
    }

    /// python binds a class name when its statement runs, so a conversion that
    /// runs at import time cannot precede the block it converts through
    #[test]
    fn a_conversion_before_its_block_is_rejected() {
        let err = transpile(
            &format!(
                "{IFACE}def takes_a(a: A) -> int:\n    return a.f()\n\nresult = takes_a(B())\n\
                 \nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n"
            ),
            &Config::test_default(),
        )
        .expect_err("a use-before-declaration conversion must not be emitted");
        assert!(err.contains("declared later in the module"), "got: {err}");
    }

    /// inside a function body the name resolves when the function runs, so the
    /// same shape is fine there
    #[test]
    fn a_deferred_conversion_before_its_block_is_fine() {
        let out = check(&format!(
            "{IFACE}def takes_a(a: A) -> int:\n    return a.f()\n\ndef main():\n    takes_a(B())\n\
             \nimplementation A for B:\n    override def f(self) -> int:\n        return self.a\n"
        ));
        assert!(out.contains("takes_a(_by_impl__A__B(B()))"), "got:\n{out}");
    }

    /// the delegating dunders live on the witness, not the shared base, and only
    /// where the interface leaves them to `object` — otherwise they would sit ahead
    /// of the interface in the MRO and shadow its own version
    #[test]
    fn an_interface_dunder_is_not_shadowed() {
        let delegating = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n"
        ));
        assert!(
            delegating.contains("def __repr__(self):"),
            "got:\n{delegating}"
        );

        let respecting = check(
            "abstract class Keyed:\n    abstract def key(self) -> int: ...\n\
             \x20   def __repr__(self) -> str:\n        return \"k\"\n\
             \nclass B:\n    a: int = 3\n\
             \nimplementation Keyed for B:\n    override def key(self) -> int:\n        return self.a\n",
        );
        assert!(
            !respecting.contains("def __repr__(self):\n        return repr(self.__implemented__)"),
            "the interface declares `__repr__`, so the witness must not delegate it:\n{respecting}"
        );
    }

    /// python name-mangles a `__name` reference inside a class body, so the witness
    /// name must not start with two underscores
    #[test]
    fn a_conversion_inside_a_class_body_is_not_name_mangled() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef one(x: A) -> int:\n    return x.f()\n\nclass Holder:\n    total: int = one(B())\n"
        ));
        assert!(
            out.contains("total: int = one(_by_impl__A__B(B()))"),
            "got:\n{out}"
        );
        assert!(
            !out.contains("__by_impl__"),
            "a double-underscore witness name would be mangled to `_Holder__…`:\n{out}"
        );
    }

    /// `x: A = b` is a conversion site too: the declared type is in the source and
    /// the value is one expression the wrap can enclose
    #[test]
    fn an_annotated_assignment_converts_its_value() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef main():\n    a: A = B()\n"
        ));
        assert!(out.contains("a: A = _by_impl__A__B(B())"), "got:\n{out}");
    }

    /// the assignment site claims from the annotation's end, so a peer edit over
    /// the value nests inside the wrap rather than colliding with it
    #[test]
    fn an_annotated_assignment_composes_with_a_peer_rewrite() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef main():\n    a: A = B() cast B\n"
        ));
        assert!(
            out.contains("a: A = _by_impl__A__B(cast(B, B()))"),
            "got:\n{out}"
        );
    }

    /// an annotation with no value has nothing to convert
    #[test]
    fn a_bare_annotation_is_not_a_conversion_site() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef main():\n    a: A\n"
        ));
        assert!(out.contains("a: A\n"), "got:\n{out}");
        assert!(!out.contains("a: A = "), "got:\n{out}");
    }

    /// every site the checker accepts must actually emit a wrap, or the generated
    /// python passes an unconverted value
    #[test]
    fn every_conversion_site_emits_a_wrap() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \nclass Holder:\n    field: A\n\
             \ndef takes_a(x: A) -> int:\n    return x.f()\n\
             \ndef ret() -> A:\n    return B()\n\
             \ndef sites(h: Holder, c: bool, bs: list[B]) -> None:\n\
             \x20   takes_a(B())\n\
             \x20   value: A = B()\n\
             \x20   h.field = B()\n\
             \x20   arm: A = B() if c else B()\n\
             \x20   xs: list[A] = [B(), B()]\n\
             \x20   ys: list[A] = [b for b in bs]\n\
             \x20   d: dict[str, A] = {{\"k\": B()}}\n"
        ));
        for expected in [
            "takes_a(_by_impl__A__B(B()))",
            "value: A = _by_impl__A__B(B())",
            "h.field = _by_impl__A__B(B())",
            "arm: A = _by_impl__A__B(B() if c else B())",
            "xs: list[A] = [_by_impl__A__B(B()), _by_impl__A__B(B())]",
            "ys: list[A] = [_by_impl__A__B(b) for b in",
            "d: dict[str, A] = {\"k\": _by_impl__A__B(B())}",
        ] {
            assert!(out.contains(expected), "missing `{expected}` in:\n{out}");
        }
        assert!(
            out.contains("return _by_impl__A__B(B())"),
            "the return site must wrap:\n{out}"
        );
    }

    /// the element-wise conversion is only for literals: a variable holding the
    /// collection has no element expressions here, and the checker rejects it
    #[test]
    fn a_non_literal_collection_is_left_alone() {
        let out = check(&format!(
            "{IFACE}implementation A for B:\n    override def f(self) -> int:\n        return self.a\n\
             \ndef f(bs: list[B]) -> None:\n    xs: list[B] = bs\n"
        ));
        // the witness class itself is always emitted; what matters is that the
        // assignment is untouched
        assert!(out.contains("xs: list[B] = bs\n"), "got:\n{out}");
        assert!(!out.contains("= _by_impl__A__B(bs)"), "got:\n{out}");
    }

    #[test]
    fn no_implementation_means_no_runtime_class() {
        let out = check("x = 1\n");
        assert!(!out.contains("_by_Implementation"), "got:\n{out}");
    }
}
