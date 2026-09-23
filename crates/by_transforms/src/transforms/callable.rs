//! rewrites callable type syntax in annotation positions
//!
//! denotable callable types lower to `typing.Callable`:
//!
//! `(int) -> int`             → `Callable[[int], int]`
//! `(int, str) -> bool`       → `Callable[[int, str], bool]`
//! `() -> None`               → `Callable[[], None]`
//! `(int) -> (str) -> bool`   → `Callable[[int], Callable[[str], bool]]`
//!
//! non-denotable callable types — those with named parameters, `/` /
//! `*` markers, variadic `*args: T`, or kwargs `**kwargs: T` — synthesize
//! a `typing.Protocol` subclass with a `__call__` method. The protocol
//! class is hoisted to module scope and the annotation site is replaced
//! with the protocol's name. its annotations are forward-reference strings,
//! since the hoisted class precedes any user definition they mention:
//!
//! `(a: int) -> str`          → `class _Callable_<hash>(Protocol):
//!                                  def __call__(self, a: "int") -> "str": ...`
//!                              and the annotation becomes `_Callable_<hash>`

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::helpers::{is_classvar_marker_id, is_final_marker_id, is_let_marker_id};
use ruff_python_ast::{Expr, ExprCallableType, Operator, PythonVersion, Stmt, UnaryOp};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{Lowering, PassContext, TypeAwarePass};
use super::intersection::{collect_intersect, collect_union, is_intersection_node};
use super::just_float::rewrite_type_expr_with_imports;
use super::repeated_underscore::{WrittenNames, lowered_callable_parameters};
use super::type_expr_walker::RootKind;
use crate::config::{Config, FloatLiteralLowering};
use crate::type_info::{TypeInfo, UnpackedKwargsLowering};

/// the lowerings [`CallableSyntax::lower_type_expr`] writes itself, wherever it prints a type
/// expression. a pass that prints one through it in place of the source writes these too
pub(crate) const TYPE_EXPRESSION: &[Lowering] = &[
    Lowering::Callable,
    Lowering::OptionalType,
    Lowering::LiteralType,
    Lowering::JustFloat,
    Lowering::DynamicKeyword,
    Lowering::FloatConst,
];

#[expect(
    clippy::struct_excessive_bools,
    reason = "transform flags toggled across visit"
)]
pub(crate) struct CallableSyntax<'src> {
    source: &'src str,
    /// the module as written, whose names a repeated `_` is numbered around
    written: WrittenNames<'src>,
    types: Option<&'src (dyn TypeInfo + 'src)>,
    /// ranges already folded by `symbolic_type_op` (e.g. `1 + typeof d` →
    /// `Literal[3]`). a claimed sub-expression is opaque here: descending into
    /// it would re-render its `typeof`/operator surface and the wider edit
    /// would clobber the fold, so `rewrite` leaves it for the fold's own edit
    claimed_ranges: &'src [TextRange],
    /// the `typeof` nodes a type position may hold that this lowerer owns, when the
    /// `typeof` fold lowers the rest in the syntax tree. `None` owns every one
    owned_typeof: Option<&'src [TextRange]>,
    float_literals: FloatLiteralLowering,
    /// whether a union the runtime evaluates can be written `A | B`: from 3.10, where
    /// `type.__or__` arrived, and in a stub, which nothing evaluates
    union_operator_runs: bool,
    /// whether the runtime evaluates the type expression being lowered where it
    /// stands. an annotation it does not: below 3.10, the only targets where the
    /// spelling matters, annotations are always deferred, so a union the author
    /// wrote with the operator can stay as written there
    evaluated: bool,
    edits: Vec<Fix>,
    needs_import: bool,
    needs_union_import: bool,
    needs_concatenate_import: bool,
    needs_protocol_import: bool,
    needs_intersection_import: bool,
    needs_typeof_import: bool,
    needs_not_import: bool,
    needs_annotated_import: bool,
    needs_optional_runtime: bool,
    /// shape → synthesized class name. used to dedupe identical
    /// non-denotable callable shapes
    protocol_shapes: HashMap<ProtocolShape, String>,
    /// emitted class definitions in declaration order
    protocol_class_defs: String,
    /// import lines the per-leaf lowerings ([`lower_leaf`]) folded into this
    /// pass's wide replacements require (`JustFloat`, `Literal`, …). the
    /// sibling per-leaf passes' own edits are dropped inside our wide edit, so
    /// we re-request the imports here to keep the lowered names defined
    extra_imports: Vec<String>,
    /// source ranges another transform has already resolved to a name — an
    /// inline `protocol(...)` hoisted to a synthesized class. consulted before
    /// any structural rewrite AND when rendering a verbatim leaf, so the
    /// substitution reaches every depth of the recursion below rather than only
    /// a subtree whose range matches exactly
    substitutions: Vec<(TextRange, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct ProtocolShape {
    /// rendered `def __call__(self, ...) -> R:` parameter list (everything
    /// between the surrounding `(self, ` and `) -> R:`)
    params: String,
    returns: String,
}

impl<'src> CallableSyntax<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>, config: &Config) -> Self {
        Self {
            source,
            written,
            types: None,
            claimed_ranges: &[],
            owned_typeof: None,
            float_literals: config.float_literals,
            union_operator_runs: config.is_stub || config.min_version >= PythonVersion::PY310,
            evaluated: false,
            edits: Vec::new(),
            needs_import: false,
            needs_union_import: false,
            needs_concatenate_import: false,
            needs_protocol_import: false,
            needs_intersection_import: false,
            needs_typeof_import: false,
            needs_not_import: false,
            needs_annotated_import: false,
            needs_optional_runtime: false,
            protocol_shapes: HashMap::new(),
            protocol_class_defs: String::new(),
            extra_imports: Vec::new(),
            substitutions: Vec::new(),
        }
    }

    /// Record that `range` has already been lowered to `name` by another
    /// transform, so every rendering path below emits the name instead of the
    /// original source.
    pub(crate) fn add_substitution(&mut self, range: TextRange, name: String) {
        self.substitutions.push((range, name));
    }

    /// The name a substitution assigned to exactly `range`, if any.
    fn substitution_for(&self, range: TextRange) -> Option<&str> {
        self.substitutions
            .iter()
            .find(|(candidate, _)| *candidate == range)
            .map(|(_, name)| name.as_str())
    }

    /// say whether the runtime evaluates the type expressions lowered from here on
    /// where they stand, which decides whether a union written with the operator has
    /// to be spelled out
    pub(crate) fn set_evaluated(&mut self, evaluated: bool) {
        self.evaluated = evaluated;
    }

    /// a union of the rendered `arms`, which is `Union[A, B]` below 3.10, since
    /// `A | B` calls `type.__or__`. a union this lowerer spells for a `?` or an `or`
    /// is written so wherever it stands, as the optional lowering writes a bare `T?`:
    /// even a deferred annotation is evaluated by whatever reads it with
    /// `get_type_hints`
    fn spell_union(&mut self, arms: &[String]) -> String {
        if self.union_operator_runs {
            arms.join(" | ")
        } else {
            self.needs_union_import = true;
            format!("Union[{}]", arms.join(", "))
        }
    }

    pub(crate) fn with_types(mut self, types: &'src dyn TypeInfo) -> Self {
        self.types = Some(types);
        self
    }

    pub(crate) fn with_claimed_ranges(mut self, claimed: &'src [TextRange]) -> Self {
        self.claimed_ranges = claimed;
        self
    }

    /// leave to the `typeof` fold every `typeof` a type position holds outside `owned`
    fn with_owned_typeof(mut self, owned: &'src [TextRange]) -> Self {
        self.owned_typeof = Some(owned);
        self
    }

    pub(crate) fn class_defs(&self) -> &str {
        &self.protocol_class_defs
    }

    /// What everything this lowerer emitted needs: import lines, and the names
    /// of any runtime helpers, including for the per-leaf rewrites it folded
    /// into its own wide replacements.
    ///
    /// the two come back together because they are one answer: a caller that
    /// took the imports and dropped the helpers would emit code calling a name
    /// nothing defines
    pub(crate) fn take_requirements(&mut self) -> (Vec<String>, Vec<crate::runtime::Helper>) {
        let mut lines = Vec::new();
        for (needed, line) in [
            (self.needs_import, "from typing import Callable"),
            (
                self.needs_concatenate_import,
                "from typing import Concatenate",
            ),
            (self.needs_protocol_import, "from typing import Protocol"),
            (
                self.needs_intersection_import,
                "from ty_extensions import Intersection",
            ),
            (self.needs_typeof_import, "from ty_extensions import TypeOf"),
            (self.needs_not_import, "from ty_extensions import Not"),
            (self.needs_annotated_import, "from typing import Annotated"),
            (self.needs_union_import, "from typing import Union"),
        ] {
            if needed {
                lines.push(line.to_owned());
            }
        }
        lines.append(&mut self.extra_imports);
        let mut helpers = Vec::new();
        if self.needs_optional_runtime {
            helpers.push(crate::runtime::OPTIONAL);
        }
        // reset so a second call is a no-op rather than re-emitting every line
        self.needs_import = false;
        self.needs_concatenate_import = false;
        self.needs_protocol_import = false;
        self.needs_intersection_import = false;
        self.needs_typeof_import = false;
        self.needs_not_import = false;
        self.needs_annotated_import = false;
        self.needs_union_import = false;
        self.needs_optional_runtime = false;
        (lines, helpers)
    }

    /// Lower a single type expression to python source: the structural forms
    /// this lowerer owns (`&` / `not` / `?` / callable arrows / subscripts) plus
    /// the per-leaf rewrites. Public so a transform that re-emits a lowered type
    /// into synthesized output can drive one lowerer for a whole run and then
    /// collect its imports and class definitions.
    pub(crate) fn lower_type_expr(&mut self, expr: &Expr) -> String {
        self.rewrite_or_leaf(expr)
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// Lower a type-expression leaf that `rewrite` itself does not transform
    /// (a bare `float` / `complex`, a literal, `dynamic`, `float.inf`) through
    /// the shared per-leaf composer, recording any import it needs. Our wide
    /// replacement would otherwise re-emit the leaf verbatim and drop the
    /// dedicated pass's edit, leaving e.g. `float` unlowered with an orphan
    /// `JustFloat` import. Falls back to verbatim source when no leaf rewrite
    /// applies (or no type info is available).
    fn lower_leaf(&mut self, expr: &Expr) -> String {
        // a leaf that *contains* an already-lowered subtree is rendered by
        // sweeping the substitutions over its source instead: the per-leaf
        // composers below would re-render the subtree from source and leak the
        // surface syntax the other transform owns. this catches any container
        // `rewrite` does not descend into
        if self
            .substitutions
            .iter()
            .any(|(range, _)| expr.range().contains_range(*range) && *range != expr.range())
        {
            return self.sweep_substitutions(expr.range());
        }
        if let Some(types) = self.types
            && let Some((text, imports)) =
                rewrite_type_expr_with_imports(self.source, types, expr, self.float_literals)
        {
            self.extra_imports.extend(imports);
            return text;
        }
        self.src(expr.range()).to_owned()
    }

    /// Render `range`'s source with every substitution inside it applied.
    fn sweep_substitutions(&self, range: TextRange) -> String {
        let mut subs: Vec<(TextRange, &str)> = self
            .substitutions
            .iter()
            .filter(|(candidate, _)| range.contains_range(*candidate))
            .map(|(candidate, name)| (*candidate, name.as_str()))
            .collect();
        // an insertion at an offset goes ahead of a replacement starting there
        subs.sort_by_key(|(candidate, _)| (candidate.start(), candidate.end()));

        let mut out = String::new();
        let mut cursor = range.start();
        for (sub_range, name) in subs {
            // an outer substitution swallows any nested one; the cursor has
            // already passed those
            if sub_range.start() < cursor {
                continue;
            }
            out.push_str(&self.source[usize::from(cursor)..usize::from(sub_range.start())]);
            out.push_str(name);
            cursor = sub_range.end();
        }
        out.push_str(&self.source[usize::from(cursor)..usize::from(range.end())]);
        out
    }

    /// `rewrite` the expression if it is a callable structural form this pass
    /// owns (callable arrow, `&` / `not` / `?`, subscript, …); otherwise lower
    /// it as a leaf so per-leaf rewrites inside it still apply.
    fn rewrite_or_leaf(&mut self, expr: &Expr) -> String {
        if let Some(rewritten) = self.rewrite(expr) {
            rewritten
        } else {
            self.lower_leaf(expr)
        }
    }

    /// Whether a starred parameter-list element unpacks a variadic type (`(*Ts) -> R`)
    /// rather than declaring an anonymous variadic (`(*: T) -> R`). The two spell
    /// identically, so this resolves them the way ty does — on whether the operand is a
    /// `TypeVarTuple`
    fn is_unpacked_variadic(&self, expr: &Expr) -> bool {
        self.types.is_some_and(|types| types.is_typevartuple(expr))
    }

    /// How the operand of a bare `**X` expands, per ty's classifier.
    fn unpacked_kwargs(&self, expr: &Expr) -> Option<UnpackedKwargsLowering> {
        self.types.and_then(|types| types.unpacked_kwargs(expr))
    }

    /// `(prefix, paramspec_name)` if the last arrow parameter is a bare `**P` naming an
    /// actual parameter pack, and the prefix is plain positional types with no `/` / `*`
    /// markers — the `ParamSpec` (empty prefix) or `Concatenate` form. A `**X` naming
    /// anything else unpacks into an explicit parameter list instead, so it is left to the
    /// `Protocol.__call__` synthesis
    fn paramspec_tail<'ct>(&self, ct: &'ct ExprCallableType) -> Option<(&'ct [Expr], &'ct Expr)> {
        if ct.parameter_slash().is_some() || ct.parameter_star().is_some() {
            return None;
        }
        let (last, prefix) = ct.args.split_last()?;
        let Expr::Starred(outer) = last else {
            return None;
        };
        let Expr::Starred(inner) = outer.value.as_ref() else {
            return None;
        };
        if !matches!(inner.value.as_ref(), Expr::Name(_)) {
            return None;
        }
        if self.unpacked_kwargs(&inner.value) != Some(UnpackedKwargsLowering::ParameterPack) {
            return None;
        }
        // a `Concatenate` prefix is plain positional types (no named/variadic params)
        if prefix
            .iter()
            .any(|a| matches!(a, Expr::Named(_) | Expr::Starred(_)))
        {
            return None;
        }
        Some((prefix, inner.value.as_ref()))
    }

    /// True iff the callable signature can't be expressed by `Callable[[T,
    /// ...], R]` and needs a `Protocol.__call__` synthesis: any named
    /// parameter, marker, variadic, or kwargs catch-all
    #[expect(clippy::unused_self, reason = "kept as method for grouping")]
    fn is_non_denotable(&self, ct: &ExprCallableType) -> bool {
        if ct.parameter_slash().is_some() || ct.parameter_star().is_some() {
            return true;
        }
        ct.args
            .iter()
            .any(|a| matches!(a, Expr::Named(_) | Expr::Starred(_)))
    }

    /// Render a callable's parameter list as a `def` parameter string, after a
    /// leading `receiver`. Markers and variadic forms map to the corresponding
    /// Python parameter syntax. Every annotation is emitted as a
    /// forward-reference string: the synthesized class is hoisted to module
    /// top, ahead of any user class its annotations mention, and an unquoted
    /// annotation would be evaluated at class-body time and `NameError`
    ///
    /// `method` says `ct` is the signature of a protocol method member, whose
    /// first argument is its receiver: a name rather than a type, emitted as
    /// `receiver`. a plain callable type is given the synthesized `receiver`.
    ///
    /// `implicit_receiver` is the rendered leading parameter of a basedpython
    /// implicit receiver (`int.() -> str`), which is a type rather than a name and
    /// so is spelled by a synthesized parameter.
    ///
    /// a labelled parameter is declared under the name the lowering gives it,
    /// and a repeated `_` makes the parameters up to the last of them
    /// positional-only, as it does in a `def` — both ty's answer too
    pub(crate) fn render_protocol_params(
        &mut self,
        ct: &ExprCallableType,
        method: bool,
        receiver: &str,
        implicit_receiver: Option<String>,
    ) -> String {
        let lowered = lowered_callable_parameters(ct, method, self.written);
        let offset = ct.args.len() - lowered.names.len();
        let args = &ct.args[offset..];
        let shift = |index: Option<u32>| index.map(|i| (i as usize).saturating_sub(offset));
        let explicit_slash = shift(ct.parameter_slash());
        let star = shift(ct.parameter_star());
        let mut parts: Vec<String> = vec![receiver.to_owned()];
        // implicit `/` after the last bare positional (no label) when followed
        // by a named/labelled parameter. bare positionals are positional-only
        let implicit_slash: Option<usize> = if explicit_slash.is_some() {
            None
        } else {
            let last_bare = args.iter().enumerate().rev().find_map(|(i, a)| {
                let is_bare = !matches!(a, Expr::Named(_) | Expr::Starred(_));
                is_bare.then_some(i)
            });
            last_bare.and_then(|li| {
                if args.get(li + 1).is_some_and(
                    |a| matches!(a, Expr::Named(n) if matches!(n.target.as_ref(), Expr::Name(_))),
                ) {
                    Some(li + 1)
                } else {
                    None
                }
            })
        };
        let slash = [explicit_slash.or(implicit_slash), lowered.slash]
            .into_iter()
            .flatten()
            .max();
        // an implicit receiver leads the parameter list, positional-only. any `/`
        // the arguments themselves emit comes after the receiver and so already
        // closes it off — a second one is a `SyntaxError`
        if let Some(implicit_receiver) = implicit_receiver {
            parts.push(implicit_receiver);
            if slash.is_none() {
                parts.push("/".to_owned());
            }
        }
        let names = lowered.names;
        let mut star_emitted = false;
        for (i, arg) in args.iter().enumerate() {
            if Some(i) == slash {
                parts.push("/".to_owned());
            }
            if Some(i) == star && !star_emitted {
                let consumed = matches!(arg, Expr::Starred(_))
                    || matches!(arg, Expr::Named(n) if matches!(n.target.as_ref(), Expr::Starred(_)));
                if !consumed {
                    parts.push("*".to_owned());
                    star_emitted = true;
                }
            }
            match arg {
                Expr::Named(named) => {
                    let name = &names[i];
                    let name = match named.target.as_ref() {
                        Expr::Starred(s) => match s.value.as_ref() {
                            Expr::Starred(_) => format!("**{name}"),
                            _ => {
                                star_emitted = true;
                                format!("*{name}")
                            }
                        },
                        _ => name.to_string(),
                    };
                    let ty = quote_forward_ref(&self.rewrite_or_leaf(&named.value));
                    parts.push(format!("{name}: {ty}"));
                }
                Expr::Starred(s) => match s.value.as_ref() {
                    Expr::Starred(inner) => {
                        // `(**TD)` / `(**P)` unpack into keyword parameters. python spells the
                        // `TypedDict` case `Unpack[TD]`, but has nothing for a protocol, so its
                        // members are emitted one by one
                        match self.unpacked_kwargs(&inner.value) {
                            Some(UnpackedKwargsLowering::TypedDict) => {
                                let ty = quote_forward_ref(&format!(
                                    "Unpack[{}]",
                                    self.rewrite_or_leaf(&inner.value)
                                ));
                                self.extra_imports
                                    .push("from typing import Unpack\n".to_owned());
                                parts.push(format!("**kwargs: {ty}"));
                            }
                            Some(UnpackedKwargsLowering::Protocol(members)) => {
                                if !star_emitted {
                                    parts.push("*".to_owned());
                                    star_emitted = true;
                                }
                                for (name, member_ty) in members {
                                    parts
                                        .push(format!("{name}: {}", quote_forward_ref(&member_ty)));
                                }
                            }
                            Some(UnpackedKwargsLowering::ParameterPack) | None => {
                                let ty = quote_forward_ref(&self.rewrite_or_leaf(&inner.value));
                                parts.push(format!("**kwargs: {ty}"));
                            }
                        }
                    }
                    _ => {
                        star_emitted = true;
                        // `(*Ts)` unpacks the variadic type — the star belongs to the
                        // annotation, unlike the anonymous variadic `(*: T)`, whose
                        // annotation types each individual argument
                        let unpacks = self.is_unpacked_variadic(&s.value);
                        let inner = self.rewrite_or_leaf(&s.value);
                        let ty =
                            quote_forward_ref(&if unpacks { format!("*{inner}") } else { inner });
                        parts.push(format!("*args: {ty}"));
                    }
                },
                _ => {
                    let ty = quote_forward_ref(&self.rewrite_or_leaf(arg));
                    parts.push(format!("{}: {ty}", names[i]));
                }
            }
        }
        // markers at the very end (slash/star at args.len)
        let after_last = args.len();
        if Some(after_last) == slash {
            parts.push("/".to_owned());
        }
        if Some(after_last) == star && !star_emitted {
            parts.push("*".to_owned());
        }
        parts.join(", ")
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "shape ownership stays at call site for clarity"
    )]
    fn class_name_for(&mut self, shape: ProtocolShape) -> String {
        if let Some(name) = self.protocol_shapes.get(&shape) {
            return name.clone();
        }
        let mut hasher = DefaultHasher::new();
        shape.hash(&mut hasher);
        #[expect(clippy::cast_possible_truncation)]
        let truncated = hasher.finish() as u32;
        let name = format!("_Callable_{truncated:08x}");
        self.protocol_shapes.insert(shape.clone(), name.clone());
        let _ = writeln!(
            self.protocol_class_defs,
            "class {name}(Protocol):\n    def __call__({params}) -> {ret}: ...\n",
            params = shape.params,
            ret = shape.returns,
        );
        name
    }

    fn rewrite(&mut self, expr: &Expr) -> Option<String> {
        // a symbolic-fold-claimed sub-expression is opaque: the fold emits its
        // own edit over this exact range, so re-rendering here (and clobbering
        // it with a wider edit) must not happen
        if self.claimed_ranges.contains(&expr.range()) {
            return None;
        }
        if let Some(name) = self.substitution_for(expr.range()) {
            return Some(name.to_owned());
        }
        match expr {
            // basedpython ParamSpec/Concatenate: `(**P) -> R` is `Callable[P, R]`
            // and `(T1, …, **P) -> R` is `Callable[Concatenate[T1, …, P], R]`
            Expr::CallableType(ct) if self.paramspec_tail(ct).is_some() => {
                self.needs_import = true;
                let (prefix, paramspec) = self.paramspec_tail(ct)?;
                let ret_str = self.rewrite_or_leaf(&ct.returns);
                let ps = self.rewrite_or_leaf(paramspec);
                // an implicit receiver is the callable's leading positional
                // parameter, so it joins the `Concatenate` prefix
                let mut prefix_str: Vec<String> = ct
                    .receiver
                    .iter()
                    .map(|receiver| self.rewrite_or_leaf(receiver))
                    .collect();
                prefix_str.extend(prefix.iter().map(|a| self.rewrite_or_leaf(a)));
                if prefix_str.is_empty() {
                    Some(format!("Callable[{ps}, {ret_str}]"))
                } else {
                    self.needs_concatenate_import = true;
                    Some(format!(
                        "Callable[Concatenate[{}, {ps}], {ret_str}]",
                        prefix_str.join(", ")
                    ))
                }
            }
            Expr::CallableType(ct) if self.is_non_denotable(ct) => {
                self.needs_protocol_import = true;
                let implicit_receiver = ct.receiver.as_ref().map(|receiver| {
                    let rendered = self.rewrite_or_leaf(receiver);
                    format!(
                        "{}: {}",
                        receiver_parameter_name(ct),
                        quote_forward_ref(&rendered)
                    )
                });
                let params = self.render_protocol_params(ct, false, "self", implicit_receiver);
                let returns = quote_forward_ref(&self.rewrite_or_leaf(&ct.returns));
                let shape = ProtocolShape { params, returns };
                Some(self.class_name_for(shape))
            }
            // `(...) -> R` — a single bare ellipsis parameter list is python's
            // "any arguments" callable: `Callable[..., R]`, not the
            // single-`...`-argument `Callable[[...], R]`
            // a receiver in front of a gradual parameter list is absorbed by it:
            // `Callable[..., R]` already accepts the receiver-first call, and
            // `Concatenate[T, ...]` is not spellable on every supported version
            Expr::CallableType(ExprCallableType { args, returns, .. })
                if matches!(args.as_slice(), [Expr::EllipsisLiteral(_)]) =>
            {
                self.needs_import = true;
                let ret_str = self.rewrite_or_leaf(returns);
                Some(format!("Callable[..., {ret_str}]"))
            }

            Expr::CallableType(ExprCallableType {
                receiver,
                args,
                returns,
                ..
            }) => {
                self.needs_import = true;
                let mut rendered: Vec<String> = receiver
                    .iter()
                    .map(|receiver| self.rewrite_or_leaf(receiver))
                    .collect();
                rendered.extend(args.iter().map(|a| self.rewrite_or_leaf(a)));
                let args_str = rendered.join(", ");
                let ret_str = self.rewrite_or_leaf(returns);
                Some(format!("Callable[[{args_str}], {ret_str}]"))
            }

            // intersection: `A & B`, `A and B`, `A & B and C` → `Intersection[…]`.
            // `&` and `and` flatten into one chain; each arm recurses so nested
            // callable arrows / typeof / leaves inside it are lowered too
            _ if is_intersection_node(expr) => {
                self.needs_intersection_import = true;
                let mut parts: Vec<Expr> = Vec::new();
                collect_intersect(expr, &mut parts);
                let rendered: Vec<String> = parts.iter().map(|p| self.rewrite_or_leaf(p)).collect();
                Some(format!("Intersection[{}]", rendered.join(", ")))
            }
            // keyword union: `A or B` → `A | B`. `|` and `or` flatten into one
            // chain so the rendered output carries no redundant parentheses
            Expr::BoolOp(_) => {
                let mut parts: Vec<Expr> = Vec::new();
                collect_union(expr, &mut parts);
                let rendered: Vec<String> = parts.iter().map(|p| self.rewrite_or_leaf(p)).collect();
                Some(self.spell_union(&rendered))
            }

            // a union written with the operator is kept as written, unless the runtime
            // evaluates it on a target without the operator
            Expr::BinOp(b)
                if b.op == Operator::BitOr && self.evaluated && !self.union_operator_runs =>
            {
                let mut parts: Vec<Expr> = Vec::new();
                collect_union(expr, &mut parts);
                let rendered: Vec<String> = parts.iter().map(|p| self.rewrite_or_leaf(p)).collect();
                Some(self.spell_union(&rendered))
            }

            // `not T` → `Not[T]`
            Expr::UnaryOp(u) if matches!(u.op, UnaryOp::Not) => {
                self.needs_not_import = true;
                let inner = self.rewrite_or_leaf(&u.operand);
                Some(format!("Not[{inner}]"))
            }

            // `T?` → `T | None` (and nested `T??` → `Optional[T | None]`), so the
            // optional composes when it sits inside a callable-arrow arg/return
            Expr::UnaryOp(u) if matches!(u.op, UnaryOp::Optional) => {
                let mut depth: usize = 1;
                let mut inner: &Expr = u.operand.as_ref();
                while let Expr::UnaryOp(u2) = inner {
                    if u2.op != UnaryOp::Optional {
                        break;
                    }
                    depth += 1;
                    inner = u2.operand.as_ref();
                }
                // the `?` already rewritten by the optional lowering, in a spelling for the
                // target's version, is written with that rewrite: its insertion ahead of the
                // operand would otherwise land inside the operand's rendering, with no tail
                if self.substitutions.iter().any(|(range, _)| {
                    !range.is_empty()
                        && range.start() >= inner.end()
                        && expr.range().contains_range(*range)
                }) {
                    return Some(self.sweep_substitutions(expr.range()));
                }
                let inner_str = self.rewrite_or_leaf(inner);
                if depth >= 2 {
                    self.needs_optional_runtime = true;
                }
                let union = self.spell_union(&[inner_str, "None".to_owned()]);
                Some(format!(
                    "{}{union}{}",
                    "Optional[".repeat(depth - 1),
                    "]".repeat(depth - 1)
                ))
            }

            Expr::BinOp(b) => {
                let l = self.rewrite(&b.left);
                let r = self.rewrite(&b.right);
                if l.is_some() || r.is_some() {
                    let op = b.op.as_str();
                    let ls = l.unwrap_or_else(|| self.lower_leaf(&b.left));
                    let rs = r.unwrap_or_else(|| self.lower_leaf(&b.right));
                    Some(format!("{ls} {op} {rs}"))
                } else {
                    None
                }
            }

            // a decorated type `@meta T` → `Annotated[T, meta]`. A chain of them
            // collapses into one `Annotated`, whose metadata reads in the order
            // the decorators apply — bottom-up, as on a decorated `def`, so
            // `@a @b int` is `Annotated[int, b, a]`. Each decorator is a value
            // expression and is copied from the source rather than lowered as a
            // type
            Expr::Subscript(s) if s.is_type_decoration => {
                self.needs_annotated_import = true;
                let mut metadata = vec![self.src(s.value.range()).to_owned()];
                let mut inner: &Expr = s.slice.as_ref();
                while let Expr::Subscript(nested) = inner
                    && nested.is_type_decoration
                {
                    metadata.push(self.src(nested.value.range()).to_owned());
                    inner = nested.slice.as_ref();
                }
                metadata.reverse();
                let inner = self.rewrite_or_leaf(inner);
                Some(format!("Annotated[{inner}, {}]", metadata.join(", ")))
            }

            // `typeof X` → `TypeOf[X]` (parser tags such subscripts with `is_typeof`)
            Expr::Subscript(s) if s.is_typeof => {
                self.needs_typeof_import = true;
                let inner = self.rewrite_or_leaf(&s.slice);
                Some(format!("TypeOf[{inner}]"))
            }

            Expr::Subscript(s) => {
                // `Annotated[T, meta…]` — only the first slice element is a type
                // position; the rest is arbitrary metadata and must stay verbatim
                // (lowering a string there would wrongly wrap it in `Literal[…]`)
                let annotated = is_named(&s.value, "Annotated");
                let slice_rewrite = match s.slice.as_ref() {
                    Expr::Tuple(t) if !t.parenthesized => {
                        let rewrites: Vec<Option<String>> =
                            t.elts.iter().map(|e| self.rewrite(e)).collect();
                        if rewrites.iter().any(std::option::Option::is_some) {
                            let parts: Vec<String> = rewrites
                                .into_iter()
                                .zip(t.elts.iter())
                                .enumerate()
                                .map(|(i, (r, e))| {
                                    if annotated && i > 0 {
                                        self.src(e.range()).to_owned()
                                    } else if let Some(text) = r {
                                        text
                                    } else {
                                        self.lower_leaf(e)
                                    }
                                })
                                .collect();
                            Some(parts.join(", "))
                        } else {
                            None
                        }
                    }
                    slice => self.rewrite(slice),
                };
                slice_rewrite.map(|s_text| format!("{}[{s_text}]", self.src(s.value.range())))
            }

            // list literal inside a subscript slice (e.g. `Callable[[A, B], R]`'s
            // parameter list). recurse into elts so intersections / nested
            // callable arrows inside the list are lowered by the same wide edit
            // the outer Subscript emits — otherwise intersection.rs's narrow
            // edits would be dropped by ast_driver's first-wins overlap rule
            Expr::List(l) => {
                let rewrites: Vec<Option<String>> =
                    l.elts.iter().map(|e| self.rewrite(e)).collect();
                if rewrites.iter().any(std::option::Option::is_some) {
                    let parts: Vec<String> = rewrites
                        .into_iter()
                        .zip(l.elts.iter())
                        .map(|(r, e)| r.unwrap_or_else(|| self.lower_leaf(e)))
                        .collect();
                    Some(format!("[{}]", parts.join(", ")))
                } else {
                    None
                }
            }

            // parenthesized tuple type literal: `(int, str)` → `tuple[int, str]`.
            // a tuple with any named field is an anonymous named tuple, owned
            // by `anon_named_tuple` — don't touch it here
            Expr::Tuple(t)
                if t.parenthesized
                    && !t.elts.is_empty()
                    && !t.elts.iter().any(|e| matches!(e, Expr::Named(_))) =>
            {
                let rewrites: Vec<Option<String>> =
                    t.elts.iter().map(|e| self.rewrite(e)).collect();
                // a parenthesized tuple literal is owned by `annotation`, which
                // does not lower its leaves (the walker deliberately doesn't
                // descend into it), so keep leaves verbatim to stay consistent —
                // otherwise our `tuple[…]` and annotation's would disagree and
                // leave an orphan import behind the dropped edit
                let parts: Vec<String> = rewrites
                    .into_iter()
                    .zip(t.elts.iter())
                    .map(|(r, e)| r.unwrap_or_else(|| self.src(e.range()).to_owned()))
                    .collect();
                Some(format!("tuple[{}]", parts.join(", ")))
            }

            _ => None,
        }
    }
}

/// render a synthesized annotation as a forward-reference string literal so it
/// is never evaluated at class-body time (the hoisted protocol class precedes
/// the definitions its annotations mention)
fn quote_forward_ref(ty: &str) -> String {
    format!("\"{}\"", ty.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The synthesized name of a receiver parameter in a protocol `__call__`. The
/// receiver is unnamed in the surface syntax, so any name works — but a callable
/// may itself declare a parameter called `_receiver`, and two parameters of the
/// same name is a `SyntaxError` in the emitted class rather than a parse error
/// the final verification would catch. Widen until it is free.
fn receiver_parameter_name(ct: &ExprCallableType) -> String {
    let mut name = "_receiver".to_owned();
    while declared_parameter_names(ct).any(|declared| declared == name) {
        name.push('_');
    }
    name
}

/// The parameter names a callable's arguments spell, in any of the named forms
/// (`name: T`, `*args: T`, `**kwargs: T`)
fn declared_parameter_names(ct: &ExprCallableType) -> impl Iterator<Item = &str> {
    ct.args.iter().filter_map(|arg| {
        let Expr::Named(named) = arg else {
            return None;
        };
        let target = match named.target.as_ref() {
            Expr::Starred(starred) => match starred.value.as_ref() {
                Expr::Starred(inner) => inner.value.as_ref(),
                value => value,
            },
            target => target,
        };
        target.as_name_expr().map(|name| name.id.as_str())
    })
}

/// whether `expr` is a `Name` / `Attribute` referring to the given identifier
fn is_named(expr: &Expr, ident: &str) -> bool {
    match expr {
        Expr::Name(n) => n.id.as_str() == ident,
        Expr::Attribute(a) => a.attr.id.as_str() == ident,
        _ => false,
    }
}

/// Fully lower a single type expression to text — the complete lowering
/// (`&` / `and` / `or` / `not`, callable arrows, typeof, subscripts, and the
/// per-leaf rewrites). For a caller that re-emits a lowered type into
/// synthesized output whose wide edit subsumes the in-place type-aware edits —
/// `generics`' `TypeAliasType("X", …)` / `TypeVar(bound=…)` polyfills replace
/// the whole statement, so they must splice the lowered payload themselves
/// rather than rely on `callable`/leaf passes editing in place.
///
/// Returns `Some(text)` if anything lowered, else `None` (use the original
/// source). The imports the result needs (`Intersection`, `Callable`, …) and
/// any synthesized `Protocol` class are emitted by the `callable` pass's own
/// visit of the same expression — keyed by the same shape hash, so a callable
/// arrow here resolves to the same hoisted class name — and so are not returned.
///
/// `substitutions` are ranges another transform has already lowered — a symbolic
/// fold (`T.a` → `int`), a typevar rename (`T` → `_T`). They are honoured on every
/// rendering path, which is what lets one wide edit carry rewrites it subsumes.
///
/// `root` says where the text is written, which decides how a union in it is spelled
pub(crate) fn lower_type_expr_full(
    source: &str,
    written: WrittenNames,
    types: &dyn TypeInfo,
    expr: &Expr,
    substitutions: &[(TextRange, String)],
    config: &Config,
    root: RootKind,
) -> Option<String> {
    let mut inner = CallableSyntax::new(source, written, config).with_types(types);
    inner.set_evaluated(root == RootKind::Evaluated);
    for (range, name) in substitutions {
        inner.add_substitution(*range, name.clone());
    }
    if let Some(text) = inner.rewrite(expr) {
        return Some(text);
    }
    if substitutions
        .iter()
        .any(|(range, _)| expr.range().contains_range(*range))
    {
        // nothing structural to lower, but a subsumed rewrite still has to reach the
        // output, so render the expression through the substitution sweep
        return Some(inner.lower_type_expr(expr));
    }
    // no structural type-form — fall back to the per-leaf composer
    // (`float` → `JustFloat`, a literal → `Literal[…]`, `dynamic` → `Any`)
    rewrite_type_expr_with_imports(source, types, expr, config.float_literals).map(|(text, _)| text)
}

/// if `expr` is `Subscript(Name("__let__"|"__classvar__"|"__final__"), slice)`,
/// returns the slice
fn synthetic_let_slice(expr: &Expr) -> Option<&Expr> {
    if let Expr::Subscript(s) = expr {
        if let Expr::Name(n) = s.value.as_ref() {
            if is_let_marker_id(n.id.as_str())
                || is_final_marker_id(n.id.as_str())
                || is_classvar_marker_id(n.id.as_str())
            {
                return Some(s.slice.as_ref());
            }
        }
    }
    None
}

impl crate::transforms::type_expr_walker::TypeExprVisitor for CallableSyntax<'_> {
    fn visit(
        &mut self,
        expr: &Expr,
        _pos: crate::transforms::type_expr_walker::TypePos,
    ) -> crate::transforms::type_expr_walker::Recurse {
        // ParamSpec-targeted subscripts (`A[(int, str)]` where `class
        // A[P: Parameters]`): the tuple slice is a parameter list lowered
        // by `generics.rs` to `[int, str]`, not a tuple-type. don't fire
        // here — callable's tuple-literal handling would otherwise emit
        // `A[tuple[int, str]]` that subsumes generics' polyfill edit
        if let Expr::Subscript(s) = expr
            && self
                .types
                .as_ref()
                .is_some_and(|t| t.class_first_typevar_is_paramspec(&s.value))
        {
            return crate::transforms::type_expr_walker::Recurse::Stop;
        }
        // `__let__[T]` / `__classvar__[T]` are modifier markers wrapping a
        // type expression. `modifiers` owns the outer wrapper; we only want
        // to rewrite the inner T. tell the walker to descend so it visits T
        // at the next level
        if synthetic_let_slice(expr).is_some() {
            return crate::transforms::type_expr_walker::Recurse::Descend;
        }
        // a bare top-level optional (`int?`, `int??`) is owned by
        // `optional_type`, which emits narrow edits (a zero-width `Optional[`
        // insertion for nested layers). our whole-range rewrite would collide
        // with that insertion at the shared start offset. descend instead so a
        // callable nested inside the operand is still lowered, while the `?`
        // layers stay with their dedicated pass. (an optional *inside* a
        // callable arg/return is handled by `rewrite`'s recursion, where the
        // callable's wider edit cleanly subsumes the optional's narrow ones.)
        if matches!(expr, Expr::UnaryOp(u) if u.op == UnaryOp::Optional) {
            return crate::transforms::type_expr_walker::Recurse::Descend;
        }
        // a `typeof` the fold rewrites in the syntax tree is re-emitted from there, and
        // an edit of our own at the same node would be a second lowering of it that
        // nothing emits
        if let Expr::Subscript(s) = expr
            && s.is_typeof
            && self
                .owned_typeof
                .is_some_and(|owned| !owned.contains(&s.range))
        {
            return crate::transforms::type_expr_walker::Recurse::Stop;
        }
        // `rewrite` is the single type-expression lowerer — it owns every
        // structural type-form (callable arrows, `&` / `and`, `or`, `not`,
        // `typeof`, subscripts) and composes leaves through `lower_leaf`. it
        // produces one replacement for the whole expression; emit and stop
        if let Some(rewrite) = self.rewrite(expr) {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                rewrite,
                expr.range(),
            )));
        }
        crate::transforms::type_expr_walker::Recurse::Stop
    }

    fn enter_root(&mut self, kind: RootKind) {
        self.set_evaluated(kind == RootKind::Evaluated);
    }
}

pub(crate) struct CallableSyntaxPass<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    config: Config,
}

impl<'src> CallableSyntaxPass<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>, config: Config) -> Self {
        Self {
            source,
            written,
            config,
        }
    }
}

/// Walks value positions and lowers any `(...) -> ...` callable type found
/// there (e.g. `print(() -> int)`). The type-position walker only visits
/// annotation contexts; a `CallableType` node is always invalid python wherever
/// it appears, so it must be lowered everywhere. Fires *only* on `CallableType`
/// — never on value-position `&` or tuples, which have real runtime meaning.
struct ValueCallableWalker<'a, 'src> {
    inner: &'a mut CallableSyntax<'src>,
}

impl<'ast> ruff_python_ast::visitor::Visitor<'ast> for ValueCallableWalker<'_, '_> {
    /// every annotation is a type position the type-position walk has lowered, as
    /// the annotation it is. lowered again here as a value, a union in it would be
    /// spelled for a runtime that never evaluates it
    fn visit_annotation(&mut self, _expr: &'ast Expr) {}

    fn visit_expr(&mut self, expr: &'ast Expr) {
        // an inline protocol is lowered whole by `protocol_type`, which renders
        // its members through its own lowerer — rewriting a method member's
        // arrow here would only leave an orphan `Callable` import and class
        if matches!(expr, Expr::ProtocolType(_)) {
            return;
        }
        if matches!(expr, Expr::CallableType(_)) {
            if let Some(repl) = self.inner.rewrite(expr) {
                self.inner
                    .edits
                    .push(Fix::safe_edit(Edit::range_replacement(repl, expr.range())));
            }
            // `rewrite` already lowered any nested callables/types; don't recurse
            return;
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

impl TypeAwarePass for CallableSyntaxPass<'_> {
    fn lowering(&self) -> Option<Lowering> {
        Some(Lowering::Callable)
    }

    /// a type expression it replaces is printed by the shared lowerer, which writes every
    /// leaf inside it too, an inline protocol as the name of the class it hoists to, and a
    /// union the runtime evaluates in the spelling the target can run
    fn subsumes(&self) -> &'static [Lowering] {
        &[
            Lowering::Callable,
            Lowering::OptionalType,
            Lowering::LiteralType,
            Lowering::JustFloat,
            Lowering::DynamicKeyword,
            Lowering::FloatConst,
            Lowering::ProtocolType,
            Lowering::RuntimeUnion,
        ]
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        // owned copy so `inner`'s borrow doesn't pin `ctx` against the mutable
        // `required_imports` / `edits` uses below
        let claimed = ctx.claimed_type_op_ranges.clone();
        let structural_typeof = ctx.structural_typeof_ranges.clone();
        let mut inner = CallableSyntax::new(self.source, self.written, &self.config)
            .with_types(types)
            .with_claimed_ranges(&claimed)
            .with_owned_typeof(&structural_typeof);
        crate::transforms::type_expr_walker::walk_type_positions_skipping(
            stmts,
            Some(types),
            &ctx.claimed_type_op_ranges,
            &mut inner,
        );
        // also lower callable types appearing in value positions; duplicate
        // edits over type-position callables dedup in the splice
        {
            // a callable type standing as a value is evaluated there
            inner.set_evaluated(true);
            let mut walker = ValueCallableWalker { inner: &mut inner };
            for stmt in stmts {
                ruff_python_ast::visitor::Visitor::visit_stmt(&mut walker, stmt);
            }
        }
        // includes the imports for per-leaf rewrites folded into our wide
        // replacements (e.g. a `float` arm lowered to `JustFloat` inside a
        // callable type) — the dedicated leaf passes' own import requests are
        // dropped along with their edits when our edit wins the overlap, so
        // they are re-requested here
        let (imports, helpers) = inner.take_requirements();
        ctx.required_imports.extend(imports);
        ctx.runtime.extend(helpers);
        let defs = inner.class_defs().to_owned();
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
        if !defs.is_empty() {
            // preserve one trailing newline so the blank line between the
            // synthesized class defs and the rest of the file survives
            // (driver's required_imports loop appends one `\n` per entry)
            let trimmed = defs.trim_end_matches('\n');
            ctx.required_imports.push(format!("{trimmed}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// transpile for python 3.9, which has no `type.__or__`
    fn check_py39(input: &str, expected: &str) {
        let config = Config {
            min_version: crate::PythonVersion::PY39,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    /// a type written where the runtime evaluates it is spelled with `Union` below 3.10,
    /// at any depth, whether the union was written with `|`, `or` or `?`
    #[test]
    fn an_evaluated_union_is_spelled_out_below_310() {
        check_py39(
            indoc! {"
                Optionals = list[int?]
                Values = dict[str, int | None]
                Either = list[int or str]
                Pairs = list[(int, str | None)]
                Handlers = list[(int | None) -> str]
                class Items(list[int | str]): ...
            "},
            indoc! {"
                from __future__ import annotations
                from typing import Callable, Union
                Optionals = list[Union[int, None]]
                Values = dict[str, Union[int, None]]
                Either = list[Union[int, str]]
                Pairs = list[tuple[int, Union[str, None]]]
                Handlers = list[Callable[[Union[int, None]], str]]
                class Items(list[Union[int, str]]): ...
            "},
        );
    }

    /// a `cast` target and a type argument of a typing construct are evaluated by the
    /// call. a union at the top of a `cast` target is also a value the runtime-union
    /// lowering reads, and this pass writes it in its place
    #[test]
    fn a_cast_target_and_a_typevar_bound_are_spelled_out_below_310() {
        check_py39(
            indoc! {"
                from typing import TypeVar, cast
                Bound = TypeVar(\"Bound\", bound=int | str)
                def f(v: object):
                    return cast(int | str, v), cast(list[int?], v)
            "},
            indoc! {"
                from __future__ import annotations
                from typing import TypeVar, cast, Union
                Bound = TypeVar(\"Bound\", bound=Union[int, str])
                def f(v: object):
                    return cast(Union[int, str], v), cast(list[Union[int, None]], v)
            "},
        );
    }

    /// a bound, a default and an alias value are call arguments once polyfilled, so the
    /// union the author wrote with `|` is spelled out there too
    #[test]
    fn a_polyfilled_bound_default_and_alias_are_spelled_out_below_310() {
        check_py39(
            indoc! {"
                def f[T: int | str](x: T) -> T:
                    return x
                def g[T = int | None]():
                    pass
                type A = list[int?] | str
            "},
            indoc! {"
                from __future__ import annotations
                from typing import Union
                from typing_extensions import TypeVar, TypeAliasType
                _T = TypeVar(\"_T\", bound=Union[int, str])
                def f(x: _T) -> _T:
                    return x
                _T_1 = TypeVar(\"_T_1\", default=Union[int, None])
                def g():
                    pass
                A = TypeAliasType(\"A\", Union[list[Union[int, None]], str])
            "},
        );
    }

    /// an annotation is deferred, so a union the author wrote with `|` keeps its spelling.
    /// one this lowerer spells for a `?` or an `or` is written as the optional lowering
    /// writes a bare `T?`, which `get_type_hints` can evaluate
    #[test]
    fn an_annotation_keeps_a_written_operator_below_310() {
        check_py39(
            "def f(a: list[int?], b: int | str, c: int or str, d: (int | str) -> str) -> list[int] | None: ...\n",
            indoc! {"
                from __future__ import annotations
                from typing import Callable, Union
                def f(a: list[Union[int, None]], b: int | str, c: Union[int, str], d: Callable[[int | str], str]) -> list[int] | None: ...
            "},
        );
    }

    /// nothing in a stub is evaluated, so a written union keeps its spelling whatever the
    /// target
    #[test]
    fn a_stub_keeps_a_written_operator_below_310() {
        let config = Config {
            min_version: crate::PythonVersion::PY39,
            is_stub: true,
            ..Config::test_default()
        };
        assert_eq!(
            transpile("Values = dict[str, int | None]\n", &config).unwrap(),
            "Values = dict[str, int | None]\n"
        );
    }

    /// a callable type naming its parameters is declared as a `__call__` in the
    /// synthesized protocol, where a repeated `_` is numbered and made positional-only as
    /// it is in any `def`
    #[test]
    fn a_repeated_underscore_is_numbered_in_the_synthesized_call() {
        let out = transpile(
            "a: (_: int, _: str, name: bytes) -> None\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains(
                "def __call__(self, _: \"int\", _2: \"str\", /, name: \"bytes\") -> \"None\": ..."
            ),
            "got:\n{out}"
        );
    }

    /// the name a bare positional is given is taken, and numbering skips it
    #[test]
    fn a_numbered_underscore_skips_a_bare_positional_name() {
        let out = transpile(
            "a: (int, int, int, _: int, _: str) -> None\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains(
                "def __call__(self, _0: \"int\", _1: \"int\", _2: \"int\", _: \"int\", _3: \"str\", /) -> \"None\": ..."
            ),
            "got:\n{out}"
        );
    }

    /// a numbered `_` after the `*` would be reachable by its number alone, a name the
    /// author never wrote, so the callable type is refused as a `def` would be
    #[test]
    fn a_keyword_only_repeated_underscore_is_refused() {
        let error = transpile(
            "a: (int, /, _: int, *args: int, _: str) -> None\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            error.contains("a repeated `_` parameter cannot be keyword-only in a callable type"),
            "got:\n{error}"
        );
    }

    #[test]
    fn simple_callable() {
        check(
            "a: (int) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int], int]
            "},
        );
    }

    #[test]
    fn no_args() {
        check(
            "a: () -> None\n",
            indoc! {"
                from typing import Callable
                a: Callable[[], None]
            "},
        );
    }

    #[test]
    fn paramspec_callable() {
        // `(**P) -> R` is `Callable[P, R]`
        check(
            "a: (**P) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[P, int]
            "},
        );
    }

    #[test]
    fn concatenate_callable() {
        // `(T1, …, **P) -> R` is `Callable[Concatenate[T1, …, P], R]`
        check(
            "a: (int, str, **P) -> bool\n",
            indoc! {"
                from typing import Callable, Concatenate
                a: Callable[Concatenate[int, str, P], bool]
            "},
        );
    }

    #[test]
    fn ellipsis_args() {
        // `(...) -> R` is the "any arguments" callable: `Callable[..., R]`,
        // not the single-`...`-argument `Callable[[...], R]`
        check(
            "a: (...) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[..., int]
            "},
        );
    }

    #[test]
    fn ellipsis_args_nested_return() {
        check(
            "a: (...) -> (int) -> str\n",
            indoc! {"
                from typing import Callable
                a: Callable[..., Callable[[int], str]]
            "},
        );
    }

    #[test]
    fn multi_args() {
        check(
            "a: (int, str) -> bool\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int, str], bool]
            "},
        );
    }

    #[test]
    fn callable_in_union() {
        check(
            "a: (int) -> int | None\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int], int] | None
            "},
        );
    }

    /// an optional `?` on a callable arg lowers inside the `Callable[...]`
    /// rendering — the callable's whole-range edit subsumes `optional_type`'s
    #[test]
    fn callable_arg_optional() {
        check(
            "a: (int?) -> int\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int | None], int]
            "},
        );
    }

    #[test]
    fn callable_as_return_type() {
        check(
            indoc! {"
                def f(x: (int) -> bool) -> (str) -> None:
                    pass
            "},
            indoc! {"
                from typing import Callable
                def f(x: Callable[[int], bool]) -> Callable[[str], None]:
                    pass
            "},
        );
    }

    #[test]
    fn nested_callable() {
        check(
            "a: (int) -> (str) -> bool\n",
            indoc! {"
                from typing import Callable
                a: Callable[[int], Callable[[str], bool]]
            "},
        );
    }

    #[test]
    fn callable_inside_subscript() {
        check(
            "a: list[(int) -> int]\n",
            indoc! {"
                from typing import Callable
                a: list[Callable[[int], int]]
            "},
        );
    }

    #[test]
    fn value_context_not_rewritten() {
        check("x = (int)\n", "x = (int)\n");
    }

    #[test]
    fn non_denotable_named_param() {
        // annotations are forward-ref strings: the class is hoisted to module
        // top, ahead of any user definition they mention
        check(
            "a: (a: int) -> str\n",
            indoc! {"
                from typing import Protocol
                class _Callable_db356498(Protocol):
                    def __call__(self, a: \"int\") -> \"str\": ...

                a: _Callable_db356498
            "},
        );
    }

    #[test]
    fn non_denotable_full_param_form() {
        // `(int, /, a: str, *args: int, **kwargs: str) -> None`
        let out = transpile(
            "f: (int, /, a: str, *args: int, **kwargs: str) -> None\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("class _Callable_"), "got: {out}");
        assert!(
            out.contains(
                "def __call__(self, _0: \"int\", /, a: \"str\", *args: \"int\", **kwargs: \"str\") -> \"None\": ..."
            ),
            "got: {out}"
        );
        assert!(
            out.starts_with("from typing import Protocol\n"),
            "got: {out}"
        );
    }

    #[test]
    fn non_denotable_unpacked_typevartuple() {
        // `(*Args)` unpacks the pack, so the star belongs to the annotation — unlike the
        // anonymous variadic `(*: int)`, whose annotation types each argument
        let out = transpile(
            "def f[*Args](fn: (*Args) -> object): ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def __call__(self, *args: \"*Args\") -> \"object\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn non_denotable_unpacked_typevartuple_after_prefix() {
        let out = transpile(
            "def f[*Args](fn: (int, *Args) -> object): ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def __call__(self, _0: \"int\", *args: \"*Args\") -> \"object\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn non_denotable_anonymous_variadic_keeps_bare_annotation() {
        let out = transpile("f: (*: int) -> None\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("def __call__(self, *args: \"int\") -> \"None\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn non_denotable_unpacked_typed_dict_kwargs() {
        // `(**TD)` unpacks the `TypedDict`'s keys; python spells that `Unpack[TD]`
        let out = transpile(
            "type TD = {\"a\": int}\n\ndef f(fn: (**TD) -> None): ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def __call__(self, **kwargs: \"Unpack[TD]\") -> \"None\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn non_denotable_unpacked_protocol_kwargs() {
        // python cannot spell "the keywords of protocol `P`", so the members are emitted
        let out = transpile(
            "protocol P:\n    b: str\n\ndef f(fn: (**P) -> None): ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def __call__(self, *, b: \"str\") -> \"None\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn labelled_kwargs_of_unpackable_type_stays_a_catch_all() {
        // only the bare spelling unpacks — `**kwargs: TD` types every keyword's value
        let out = transpile(
            "type TD = {\"a\": int}\n\ndef f(fn: (**kwargs: TD) -> None): ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def __call__(self, **kwargs: \"TD\") -> \"None\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn non_denotable_user_class_param_is_forward_ref() {
        // the synthesized class is hoisted above `class A`; quoting is what
        // keeps its annotations from evaluating `A` before it exists
        let out = transpile("class A: ...\nf: (a: A) -> A\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("def __call__(self, a: \"A\") -> \"A\": ..."),
            "got: {out}"
        );
    }

    #[test]
    fn duplicate_non_denotable_dedupes() {
        // identical shapes share a single Protocol class
        let out = transpile(
            "a: (n: int) -> str\nb: (n: int) -> str\n",
            &Config::test_default(),
        )
        .unwrap();
        let count = out.matches("class _Callable_").count();
        assert_eq!(count, 1, "got: {out}");
    }

    #[test]
    fn callable_in_call_argument() {
        // value position: a bare callable type passed as an argument
        check(
            "print(() -> int)\n",
            indoc! {"
                from typing import Callable
                print(Callable[[], int])
            "},
        );
    }

    #[test]
    fn callable_in_assignment_value() {
        check(
            "x = (int, str) -> bool\n",
            indoc! {"
                from typing import Callable
                x = Callable[[int, str], bool]
            "},
        );
    }

    #[test]
    fn nested_callable_in_value_position() {
        check(
            "y = (int) -> (str) -> None\n",
            indoc! {"
                from typing import Callable
                y = Callable[[int], Callable[[str], None]]
            "},
        );
    }

    #[test]
    fn non_denotable_callable_in_value_position() {
        // named-param callable in value position synthesizes a Protocol class
        let out = transpile("x = (n: int) -> str\n", &Config::test_default()).unwrap();
        assert!(out.contains("class _Callable_"), "got: {out}");
        assert!(
            out.contains("x = _Callable_"),
            "value site should reference the protocol name, got: {out}"
        );
    }

    // a per-leaf lowering (`float` → `JustFloat`, a literal → `Literal[…]`)
    // inside a callable type must still fire — our wide edit renders the whole
    // type, so it has to compose those leaves itself rather than re-emit them
    // verbatim and orphan the dedicated pass's import
    #[test]
    fn callable_arg_float_composes() {
        check(
            "a: (float) -> int\n",
            indoc! {"
                from ty_extensions import JustFloat
                from typing import Callable
                a: Callable[[JustFloat], int]
            "},
        );
    }

    #[test]
    fn callable_intersection_arg_float_composes() {
        check(
            "a: (A & float) -> R\n",
            indoc! {"
                from ty_extensions import Intersection, JustFloat
                from typing import Callable
                a: Callable[[Intersection[A, JustFloat]], R]
            "},
        );
    }

    #[test]
    fn callable_not_arg_float_composes() {
        check(
            "a: (not float) -> R\n",
            indoc! {"
                from ty_extensions import JustFloat, Not
                from typing import Callable
                a: Callable[[Not[JustFloat]], R]
            "},
        );
    }

    #[test]
    fn callable_literal_arg_composes() {
        check(
            "a: (A & 1) -> R\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import Callable, Literal
                a: Callable[[Intersection[A, Literal[1]]], R]
            "},
        );
    }

    #[test]
    fn callable_subscript_intersection_float_composes() {
        check(
            "from typing import Callable\nf: Callable[[A & float], C]\n",
            indoc! {"
                from ty_extensions import Intersection, JustFloat
                from typing import Callable
                f: Callable[[Intersection[A, JustFloat]], C]
            "},
        );
    }

    #[test]
    fn protocol_param_float_composes() {
        // a `float` param of a non-denotable callable lowers to `JustFloat`
        // inside the synthesized Protocol body, with the import requested
        let out = transpile("a: (n: float) -> str\n", &Config::test_default()).unwrap();
        assert!(
            out.contains("n: \"JustFloat\""),
            "protocol param should lower float, got: {out}"
        );
        assert!(
            out.contains("from ty_extensions import JustFloat"),
            "JustFloat import must be present, got: {out}"
        );
    }
}
