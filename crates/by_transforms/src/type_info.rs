//! Abstraction over type/binding information consumed by transforms.

use ruff_python_ast::helpers::is_dotted_name;
use ruff_python_ast::{Expr, ExprCall, ExprName, Stmt, StmtClassDef};
use ruff_text_size::TextRange;
use ty_python_core::scope::ScopeKind;
use ty_python_core::{global_scope, place_table, semantic_index};
use ty_python_semantic::types::{
    DynamicType, KnownClass, KnownInstanceType, Type, UnpackedKwargs, character,
};
use ty_python_semantic::{HasType, SemanticModel};

/// How the postfix `^` / `!` operators test the "absent" arm of an operand's
/// wrapped type. `T?` lowers to `T | None`, so its absent arm is `None`; a
/// result-like `T | E` (e.g. `int | TypeError`) signals absence with an error
/// value, so the guard tests against `BaseException` instead
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AbsentTest {
    /// optional form — guard tests `x is None` and returns the `None`
    Optional,
    /// wrapped-optional form — guard tests `x is None` like the optional
    /// form, but the present value is the wrapper's `.value`
    WrappedOptional,
    /// result form — guard tests `isinstance(x, BaseException)` and returns
    /// the error value
    Result,
}

/// How a callable arrow's bare `**X` lowers to python. Mirrors ty's
/// [`UnpackedKwargs`](ty_python_semantic::types::UnpackedKwargs) with the member types
/// already rendered as source text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum UnpackedKwargsLowering {
    /// `Callable[X, R]` — the pack is the whole parameter list. Also the reading for a name
    /// ty could not resolve at all: a bare `**Name` is overwhelmingly a `ParamSpec`, and the
    /// file has an unresolved-reference error to fix either way
    ParameterPack,
    /// `**kwargs: Unpack[X]`
    TypedDict,
    /// keyword-only `(name, rendered type)` parameters; python cannot spell the protocol
    /// itself in the `**` position
    Protocol(Vec<(String, String)>),
}

/// How an assignment inside a trailing-lambda block reaches an enclosing scope,
/// so the block writes through instead of shadowing with a fresh local.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CaptureKind {
    /// the name is bound at module scope — declare `global`
    Global,
    /// the name is bound in an enclosing function — declare `nonlocal`
    Nonlocal,
}

pub(crate) trait TypeInfo {
    /// whether `X[…]` where `X` is `name` treats the slice as type arguments.
    /// returns `true` for unresolved / unknown names (covers builtins like
    /// `list`, unimported sugar like `Union`)
    fn subscript_is_type_context(&self, name: &ExprName) -> bool;

    /// stricter variant: only `true` when ty *resolved* `name` to a class /
    /// generic / special form. unresolved names return `false`. used by
    /// transforms that may fire on value-position subscripts (where an
    /// unresolved name should be treated as a runtime subscript, not a type)
    fn subscript_is_known_type_context(&self, name: &ExprName) -> bool;

    /// whether `base.attr[…]` (base = a module or class) treats slice as type args
    fn attr_base_is_type_context(&self, base: &ExprName) -> bool;

    fn is_function(&self, name: &ExprName) -> bool;

    /// basedpython: the `isinstance` target for `function`'s declared `raises`
    /// clause (`(TypeError, ValueError)`, `()` for `raises Never`), or `None`
    /// when the clause has no faithful runtime test — a gradual `raises ...`, or
    /// a set with no runtime spelling
    fn declared_raises_runtime_target(
        &self,
        function: &ruff_python_ast::StmtFunctionDef,
    ) -> Option<String>;

    /// whether `name` resolves to a basedpython *reified* generic function (a
    /// pep 695 type parameter referenced in a value position). these are
    /// wrapped in the `generic` polyfill, so their specialized call sites
    /// (`f[int](…)`) must NOT have their `[…]` stripped — they route through
    /// `generic.__getitem__`
    fn is_reified_function(&self, name: &ExprName) -> bool;

    /// the comma-joined type arguments to inject at a bare call of a reified
    /// generic (`f(1)` → `"int"`), or `None` when the call needs no injection
    /// or none is possible (ty reports the latter)
    fn reified_call_specialization(&self, call: &ruff_python_ast::ExprCall) -> Option<String>;

    /// the comma-joined type arguments to inject at a bare constructor call
    /// of a generic class (`A(1)` → `"int"`), or `None` when the callee is
    /// not a generic class literal or the solved specialization has no
    /// runtime spelling — reification of constructors is best-effort, a
    /// missing spelling is never an error
    fn constructor_specialization(&self, call: &ruff_python_ast::ExprCall) -> Option<String>;

    /// how the keyword-form parametric type test `lhs is rhs` resolves
    /// (rust-style: statically folded, reified-cell token equality, witness
    /// probe, or an unchecked runtime probe). `rhs` may spell the target
    /// specialization directly (`list[int]`) or through an alias (`X =
    /// list[int]`, `type X = list[int]`). `None` when `rhs` does not resolve to
    /// a specialization — the pair is then an ordinary isinstance lowering
    fn parametric_is_plan(
        &self,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Option<ty_python_semantic::ParametricIsPlan>;

    /// [`Self::parametric_is_plan`] for a checked cast's `(value, target)` pair.
    /// the same classification engine backs both; only the target's inference
    /// position differs (a cast target is a type expression)
    fn parametric_cast_plan(
        &self,
        value: &Expr,
        target: &Expr,
    ) -> Option<ty_python_semantic::ParametricIsPlan>;

    /// whether an `is`/`is not` comparison whose rhs is `expr` keeps python
    /// identity semantics instead of lowering to `isinstance`: true when
    /// `expr` resolves to a plain *value* — an enum member (`Color.RED`, a
    /// based-enum unit variant like `Shape.Point`), another literal, or an
    /// instance of a concrete non-type class — which `isinstance` would
    /// reject as its classinfo argument at runtime
    fn is_keeps_identity(&self, expr: &Expr) -> bool;

    /// when `attribute` resolves to a basedpython `extension` member, the
    /// backing-function rewrite to apply (`xs.second()` →
    /// `_by_ext__list__second(xs)`). `None` for ordinary attributes —
    /// extensions never shadow declared members
    fn extension_attribute_info(
        &self,
        attribute: &ruff_python_ast::ExprAttribute,
    ) -> Option<ty_python_semantic::ExtensionAttributeInfo>;

    /// the name of the witness class an `implementation A for B [as N]:` block
    /// lowers to. resolved by ty so that the emitted class and the constructor
    /// inserted at a conversion site can never disagree
    fn implementation_witness_name(&self, class_def: &StmtClassDef) -> Option<String>;

    /// the delegating dunders the witness class for `class_def` may carry: those
    /// the interface leaves to `object`. Emitting one the interface declares would
    /// shadow the interface's own version at runtime while the checker still
    /// resolves the interface's
    fn implementation_delegated_dunders(&self, class_def: &StmtClassDef) -> Vec<&'static str>;

    /// the witness conversions a statement's value needs: an annotated assignment,
    /// an attribute assignment, or a `return`. one wrap for a value that converts
    /// whole, or one per element for a collection literal
    fn implementation_statement_conversions(
        &self,
        stmt: &Stmt,
    ) -> Vec<(TextRange, ty_python_semantic::ImplementationConversion)>;

    /// the witness conversions a call's arguments need, as `(argument range,
    /// conversion)` pairs: an `implementation A for B:` in scope makes a `B`
    /// acceptable where an `A` is asked for, and the argument is wrapped in the
    /// witness class the implementation lowers to
    fn implementation_call_conversions(
        &self,
        call: &ruff_python_ast::ExprCall,
    ) -> Vec<(TextRange, ty_python_semantic::ImplementationConversion)>;

    /// whether `attribute` resolves through a basedpython *implicit receiver* —
    /// `x.fn` where `fn` names a receiver callable (`int.() -> str`) in scope
    /// rather than a member of `x`. lowered to `fn(x)`
    fn is_implicit_receiver_attribute(&self, attribute: &ruff_python_ast::ExprAttribute) -> bool;

    /// whether `name` is a member of the enclosing trailing lambda block's
    /// receiver, used unqualified. lowered to `it.<name>`
    fn is_implicit_receiver_name(&self, name: &ExprName) -> bool;

    /// whether `expr` resolves to `typing.Any` (the explicitly-annotated
    /// dynamic type). distinguishes the special form from a shadowing binding
    /// or the `Unknown` that an unresolved / invalid type expression yields,
    /// both of which are also dynamic types
    fn is_any(&self, expr: &Expr) -> bool;

    /// whether `expr` resolves to a `TypeVarTuple`. a callable's parameter list spells an
    /// unpacked variadic and an anonymous variadic identically — `(*Ts)` and `(*: T)` both
    /// parse to `Starred(_)` — so the lowering resolves them the way ty does: a
    /// `TypeVarTuple` is never a valid annotation for the individual arguments of a
    /// `*args`, so a starred one can only be an unpack
    fn is_typevartuple(&self, expr: &Expr) -> bool;

    /// whether `name` is unbound at the scope enclosing `anchor`
    /// (used to pick a fresh temp-variable name)
    fn is_unbound_at(&self, name: &str, anchor: &Expr) -> bool;

    /// whether `name` is bound at module level (used to avoid duplicate imports)
    fn is_bound_globally(&self, name: &str) -> bool;

    /// For a name assigned inside a trailing-lambda block anchored at `anchor`
    /// (an expression in the block), the declaration needed for the write to
    /// reach an enclosing binding — `global` (module scope) or `nonlocal` (an
    /// enclosing function) — or `None` for a genuinely new local.
    fn trailing_block_capture(&self, name: &str, anchor: &Expr) -> Option<CaptureKind>;

    /// the capture kind a *fresh* block binding (one bound in no enclosing scope)
    /// takes to survive the boundary: `Global` at module scope, `Nonlocal` inside
    /// a function — the nearest such enclosing scope of the block at `anchor`
    fn trailing_block_fresh_capture(&self, anchor: &Expr) -> Option<CaptureKind>;

    /// rendered inferred (literal-promoted) type of `expr`, or `None` when ty
    /// cannot resolve a type (unresolved import, parse error, etc.).
    /// example: a literal `20` inferred as `Literal[20]` is promoted to
    /// `"int"` here so two value-forms with structurally compatible fields
    /// hash to the same class shape.
    fn promoted_type_display(&self, expr: &Expr) -> Option<String>;

    /// whether `expr` is an application of a `type def` — `F[bool]` where `F` is
    /// a type function. such an application lowers to the type the type function
    /// returned, read back through
    /// [`symbolic_type_fold`](TypeInfo::symbolic_type_fold)
    fn is_type_fn_application(&self, expr: &Expr) -> bool;

    /// whether `expr` is an attribute type — `T.a`, the type of member `a` on a type
    /// parameter. python cannot express the dependency on `T`, so such an annotation
    /// lowers to the member's type on the parameter's bound, read back through
    /// [`symbolic_type_fold`](TypeInfo::symbolic_type_fold)
    fn is_attribute_type(&self, expr: &Expr) -> bool;

    /// rendered exact (non-promoted) type of `expr` in a type position. used to
    /// fold symbolic operations such as `1 + 1` → `Literal[2]` or `A + B` →
    /// `Literal[3]`: ty already evaluates these in `infer_type_expression`, so
    /// this just reads the resolved type back as source text. unlike
    /// [`promoted_type_display`](TypeInfo::promoted_type_display) literals are
    /// kept precise. returns `None` when ty resolves no concrete type — e.g. an
    /// unsupported operation inferred as `Unknown` — so the caller leaves the
    /// source unchanged and ty's own diagnostic stands
    fn symbolic_type_fold(&self, expr: &Expr) -> Option<String>;

    /// names + rendered default types of the type parameters of the class
    /// referenced by `expr`. element is `(name, Some(default))` if the
    /// typevar has a declared default, `(name, None)` otherwise. returns
    /// `None` if `expr` is not a generic class
    fn class_typevars(&self, expr: &Expr) -> Option<Vec<(String, Option<String>)>>;

    /// whether the first type parameter of the class referenced by `expr`
    /// is a `ParamSpec` (e.g. `class A[**P]` or `class A[P: Parameters]`).
    /// returns `false` when `expr` is not a generic class
    fn class_first_typevar_is_paramspec(&self, expr: &Expr) -> bool;

    /// how the bare `**X` of a callable arrow expands, resolved by the same classifier the
    /// type checker uses so the lowering can't disagree with it. `None` when `X` is an
    /// ordinary type, which makes the `**` an untyped-keyword catch-all
    fn unpacked_kwargs(&self, expr: &Expr) -> Option<UnpackedKwargsLowering>;

    /// position of the keyword-variadic pack among the type parameters of the
    /// class referenced by `expr` (`class A[T, **Kwargs]` → `Some(1)`).
    /// `None` when `expr` is not a generic class or declares no pack
    fn class_keyword_pack_index(&self, expr: &Expr) -> Option<usize>;

    /// classify the "absent" arm of `expr`'s type for `^` / `!` propagation.
    /// returns [`AbsentTest::Result`] when any arm of the (possibly union)
    /// type is a `BaseException` subtype — a result-like `T | E` — else
    /// [`AbsentTest::Optional`] when the type admits `None`. `None` when ty
    /// resolves no type, or the type is neither optional nor result-like
    fn propagate_absent_test(&self, expr: &Expr) -> Option<AbsentTest>;

    /// whether `expr`'s inferred type is a wrapped optional (`int??`, a
    /// generic `T?`) — its runtime values are `None` or the injected
    /// `Optional` wrapper, so consumers unwrap with `.value`
    fn wrapped_optional(&self, expr: &Expr) -> bool;

    /// whether a call through `callee` yields a result whose type was
    /// derived by substituting typevars — a generic function's return, or a
    /// method bound to a specialized generic instance. such results rest on
    /// an assumption ty cannot verify, so the soundness pass validates them
    fn call_result_is_typevar_derived(&self, callee: &Expr) -> bool;

    /// whether `expr`'s inferred type is an instance carrying a generic
    /// specialization (`list[str]`, a `TypedDict`) — element projections out
    /// of it consume an annotation-level claim
    fn is_specialized_generic_instance(&self, expr: &Expr) -> bool;

    /// the runtime soundness check for `expr`'s inferred type — a shallow
    /// `isinstance` target (`str`, `(int, type(None))`) or a deep parametric
    /// check for a user-generic specialization (`A[int]` + variances). `None`
    /// when the type has no faithful runtime test or its name doesn't resolve
    /// at module scope
    fn soundness_check_plan(&self, expr: &Expr) -> Option<SoundnessCheck>;

    /// the soundness check for the parameter that the positional argument at
    /// `index` binds to in a call through `callee` (see
    /// [`soundness_check_plan`](TypeInfo::soundness_check_plan)). `None` when
    /// the callee isn't a single-overload function/method, the parameter is
    /// variadic/unannotated, or its type has no runtime test
    fn call_positional_param_plan(&self, callee: &Expr, index: usize) -> Option<SoundnessCheck>;

    /// like [`call_positional_param_plan`](TypeInfo::call_positional_param_plan)
    /// but for a keyword argument matched by `name`
    fn call_keyword_param_plan(&self, callee: &Expr, name: &str) -> Option<SoundnessCheck>;

    /// the runtime check for a type expression used as a checked cast target,
    /// or `None` when the type has no faithful runtime test. a user generic
    /// whose instances carry `__orig_class__` yields a deep
    /// [`CastCheck::Kind`]`(`[`SoundnessCheck::Parametric`]`)` (`A[int]`); a
    /// protocol target yields [`CastCheck::Protocol`] (checkable structurally);
    /// anything else collapses to a shallow
    /// [`SoundnessCheck::Isinstance`] (`list[object]` → `list`,
    /// `int | str` → `(int, str)`), since `isinstance(x, list[object])` is
    /// itself a runtime error
    fn cast_check_plan(&self, type_expr: &Expr) -> Option<CastCheck>;

    /// whether a `cast`'s value is already statically the target type, so its
    /// runtime check is redundant and the value passes through unchecked. this
    /// covers upcasts a runtime probe cannot even express — a value whose
    /// static type subclasses a subscripted protocol (`A[int]() cast
    /// Sequence[object]`) or a subscripted builtin (`B[int]() cast list[int]`)
    fn cast_is_redundant(&self, value: &Expr, target: &Expr) -> bool;

    /// whether a checked cast to this target has no faithful runtime residue —
    /// a protocol with a method member — so it must degrade to an unchecked
    /// `typing.cast` rather than an `isinstance` against the protocol, which
    /// raises at runtime. a data-member protocol is checkable and returns `false`
    fn cast_target_is_unverifiable(&self, type_expr: &Expr) -> bool;

    /// the keyword a trailing lambda block is passed with — the name of the
    /// callee's last declared parameter. `None` means the lambda is appended
    /// as a positional argument instead (unknown callee signature, or a
    /// variadic / positional-only last parameter)
    fn trailing_lambda_keyword(&self, callee: &Expr) -> Option<String>;

    /// whether the trailing-lambda callee's callback parameter is `once` — the
    /// block runs exactly once, so its `return` propagates to the enclosing
    /// function rather than to the block. `false` when the marker is unreadable
    fn trailing_lambda_callee_is_once(&self, callee: &Expr) -> bool;

    /// the implicit context arguments the lowering must append to `call`:
    /// `(parameter name, variable name)` pairs for each `context` parameter
    /// that no explicit argument matches, resolved from the `context`
    /// declarations in scope at the call site. empty when the callee has no
    /// context parameters or nothing resolves
    fn implicit_context_arguments(&self, call: &ExprCall) -> Vec<(String, String)>;

    /// whether `expr`'s inferred type is a string — a `str` / `Character` /
    /// `LiteralString` / string-literal / `str`-subclass instance. dynamic
    /// types (`Any`, `Unknown`) are excluded. used by the grapheme string-surface
    /// lowerings, which must only fire on string receivers
    fn is_string_like(&self, expr: &Expr) -> bool;

    /// whether the type expression `annotation` denotes exactly the
    /// `Character` type (an annotation `x: Character`). a union / optional /
    /// subclass or a shadowed local `Character` does not qualify. used by the
    /// annotated-assignment lowering, which materialises a real `Character`
    /// instance for the annotated value
    fn annotation_is_character(&self, annotation: &Expr) -> bool;

    /// whether `expr`'s inferred type is already a `Character` instance — its
    /// class is `Character`. such values are left alone by the annotated-
    /// assignment lowering (they are already the right runtime type, so
    /// wrapping them in `Character(...)` again would be redundant)
    fn is_character_instance(&self, expr: &Expr) -> bool;

    /// the framework role of the class `class_def` defines — `Some` when ty
    /// resolves it to a class a supported framework transforms at runtime
    /// (a pydantic model today; sqlalchemy/django as their sessions land).
    /// lowering passes consult this to gate constructs that would be
    /// runtime-broken inside such a class; see
    /// `docs/basedpython/frameworks/index.md`
    fn framework_class_role(&self, class_def: &StmtClassDef) -> Option<FrameworkRole>;

    /// the inferred annotation to synthesize for a bare class-body assignment
    /// `<name> = <value>` — the promoted display of `value`'s type (`1` →
    /// `"int"`, `[1]` → `"list[int]"`), or `None` when ty resolves no concrete
    /// type or the promoted type carries a dynamic part (`Unknown` / `Any`)
    /// with no faithful spelling
    fn inferred_annotation(&self, expr: &Expr) -> Option<String>;

    /// whether adding a type annotation to a bare `name = value` assignment in
    /// `class_def`'s body would change the class's runtime semantics — true for
    /// dataclass-like classes, framework models, `NamedTuple`s, `TypedDict`s
    /// (annotated assignment becomes a field) and enums (bare assignment is a
    /// member). the inferred-annotation transform must leave these alone
    fn class_body_annotation_is_semantic(&self, class_def: &StmtClassDef) -> bool;

    /// whether `expr` is a field-specifier call — `pydantic.Field(...)`,
    /// `dataclasses.field(...)`, `attrs.field(...)`, and the like. the checker
    /// models these as assignable to the field's declared type, but their
    /// runtime value is a field-descriptor object (`FieldInfo`, `Field`), so a
    /// soundness check against the field annotation would always fail at
    /// runtime — the transpiler must not emit one
    fn is_field_specifier(&self, expr: &Expr) -> bool;
}

/// re-export of the ty-side check plan so transforms name a single type
pub(crate) use ty_python_semantic::types::soundness::CheckKind as SoundnessCheck;

/// re-export of the ty-side checked-cast plan (a superset of [`SoundnessCheck`]
/// that also carries protocol-structural and unchecked cases)
pub(crate) use ty_python_semantic::types::soundness::CastCheck;

/// re-export of the ty-side framework role so transforms name a single type
pub(crate) use ty_python_semantic::types::FrameworkRole;

impl TypeInfo for SemanticModel<'_> {
    fn is_type_fn_application(&self, expr: &Expr) -> bool {
        let Expr::Subscript(subscript) = expr else {
            return false;
        };
        subscript
            .value
            .inferred_type(self)
            .is_some_and(|ty| ty.is_type_fn(self.db()))
    }

    fn is_attribute_type(&self, expr: &Expr) -> bool {
        let Expr::Attribute(attribute) = expr else {
            return false;
        };
        // mirrors ty's rule: in a type position, an attribute on a receiver that is
        // not a plain dotted name is an attribute type — `X[A].x` — because nothing
        // else can be written that way. the type-driven test below covers the bare
        // type-parameter form (`T.a`), whose receiver *is* a dotted name and which
        // therefore always stays symbolic; a specialized receiver folds in ty the
        // moment it is ground, so by then there is no `Deferred` left to recognise
        if !is_dotted_name(&attribute.value) {
            return true;
        }
        expr.inferred_type(self)
            .is_some_and(|ty| ty.is_attribute_type(self.db()))
    }

    fn subscript_is_type_context(&self, name: &ExprName) -> bool {
        match name.inferred_type(self) {
            Some(ty) => ty.is_subscript_type_context(),
            // unresolved → assume type context (covers builtins like `list`,
            // unknown imports, basedpython sugar contexts)
            None => true,
        }
    }

    fn subscript_is_known_type_context(&self, name: &ExprName) -> bool {
        match name.inferred_type(self) {
            Some(ty) => ty.is_subscript_type_context() && !ty.is_dynamic(),
            None => false,
        }
    }

    fn attr_base_is_type_context(&self, base: &ExprName) -> bool {
        match base.inferred_type(self) {
            Some(ty) => ty.is_module_or_type(),
            None => true,
        }
    }

    fn is_function(&self, name: &ExprName) -> bool {
        name.inferred_type(self)
            .is_some_and(|ty| ty.as_function_literal().is_some())
    }

    fn declared_raises_runtime_target(
        &self,
        function: &ruff_python_ast::StmtFunctionDef,
    ) -> Option<String> {
        ty_python_semantic::types::exceptions::declared_raises_runtime_target(
            self.db(),
            self.file(),
            function.inferred_type(self)?,
        )
    }

    fn is_reified_function(&self, name: &ExprName) -> bool {
        name.inferred_type(self)
            .and_then(Type::as_function_literal)
            .is_some_and(|function| function.is_reified(self.db()))
    }

    fn reified_call_specialization(&self, call: &ruff_python_ast::ExprCall) -> Option<String> {
        let arguments = self.reified_call_type_arguments(call)?;
        Some(arguments.join(", "))
    }

    fn constructor_specialization(&self, call: &ruff_python_ast::ExprCall) -> Option<String> {
        self.reified_constructor_type_arguments(call)
    }

    fn parametric_is_plan(
        &self,
        lhs: &Expr,
        rhs: &Expr,
    ) -> Option<ty_python_semantic::ParametricIsPlan> {
        SemanticModel::parametric_is_plan(self, lhs, rhs)
    }

    fn parametric_cast_plan(
        &self,
        value: &Expr,
        target: &Expr,
    ) -> Option<ty_python_semantic::ParametricIsPlan> {
        SemanticModel::parametric_cast_plan(self, value, target)
    }

    fn is_keeps_identity(&self, expr: &Expr) -> bool {
        expr.inferred_type(self).is_some_and(|ty| {
            ty_python_semantic::types::basedpython_is_keeps_identity(self.db(), ty)
        })
    }

    fn extension_attribute_info(
        &self,
        attribute: &ruff_python_ast::ExprAttribute,
    ) -> Option<ty_python_semantic::ExtensionAttributeInfo> {
        SemanticModel::extension_attribute_info(self, attribute)
    }

    fn implementation_witness_name(&self, class_def: &StmtClassDef) -> Option<String> {
        let class = class_def.inferred_type(self)?.as_class_literal()?;
        ty_python_semantic::types::implementation_witness_name(self.db(), class)
    }

    fn implementation_delegated_dunders(&self, class_def: &StmtClassDef) -> Vec<&'static str> {
        let Some(class) = class_def
            .inferred_type(self)
            .and_then(ty_python_semantic::types::Type::as_class_literal)
        else {
            return Vec::new();
        };
        ty_python_semantic::types::witness_delegated_dunders(self.db(), class)
    }

    fn implementation_statement_conversions(
        &self,
        stmt: &Stmt,
    ) -> Vec<(TextRange, ty_python_semantic::ImplementationConversion)> {
        SemanticModel::implementation_statement_conversions(self, stmt)
    }

    fn implementation_call_conversions(
        &self,
        call: &ruff_python_ast::ExprCall,
    ) -> Vec<(TextRange, ty_python_semantic::ImplementationConversion)> {
        SemanticModel::implementation_call_conversions(self, call)
    }

    fn is_implicit_receiver_attribute(&self, attribute: &ruff_python_ast::ExprAttribute) -> bool {
        SemanticModel::implicit_receiver_attribute(self, attribute)
    }

    fn is_implicit_receiver_name(&self, name: &ExprName) -> bool {
        SemanticModel::implicit_receiver_name(self, name)
    }

    fn is_any(&self, expr: &Expr) -> bool {
        expr.inferred_type(self)
            .is_some_and(|ty| matches!(ty, Type::Dynamic(DynamicType::Any)))
    }

    fn is_typevartuple(&self, expr: &Expr) -> bool {
        expr.inferred_type(self).is_some_and(
            |ty| matches!(ty, Type::TypeVar(typevar) if typevar.is_typevartuple(self.db())),
        )
    }

    fn is_unbound_at(&self, name: &str, anchor: &Expr) -> bool {
        let db = self.db();
        let file = self.file();
        let index = semantic_index(db, file);
        let Some(scope_id) = index.try_expression_scope_id(anchor) else {
            return true;
        };
        for (ancestor_id, _) in index.ancestor_scopes(scope_id) {
            let scope = ancestor_id.to_scope_id(db, file);
            let table = place_table(db, scope);
            if table
                .symbol_by_name(name)
                .is_some_and(ty_python_core::symbol::Symbol::is_bound)
            {
                return false;
            }
        }
        true
    }

    fn is_bound_globally(&self, name: &str) -> bool {
        let global = global_scope(self.db(), self.file());
        let table = place_table(self.db(), global);
        table
            .symbol_by_name(name)
            .is_some_and(ty_python_core::symbol::Symbol::is_bound)
    }

    fn trailing_block_capture(&self, name: &str, anchor: &Expr) -> Option<CaptureKind> {
        let db = self.db();
        let file = self.file();
        let index = semantic_index(db, file);
        let block_scope = index.try_expression_scope_id(anchor)?;
        // walk outward from the block's own scope (skipped) to the nearest scope
        // that already binds the name — it decides the declaration
        for (ancestor_id, scope) in index.ancestor_scopes(block_scope).skip(1) {
            let scope_id = ancestor_id.to_scope_id(db, file);
            if !place_table(db, scope_id)
                .symbol_by_name(name)
                .is_some_and(ty_python_core::symbol::Symbol::is_bound)
            {
                continue;
            }
            match scope.kind() {
                ScopeKind::Module => return Some(CaptureKind::Global),
                ScopeKind::Function | ScopeKind::Lambda => return Some(CaptureKind::Nonlocal),
                // class / type-param / type-alias / comprehension scopes are not
                // `global` / `nonlocal` targets — name resolution skips them
                _ => {}
            }
        }
        None
    }

    fn trailing_block_fresh_capture(&self, anchor: &Expr) -> Option<CaptureKind> {
        let db = self.db();
        let file = self.file();
        let index = semantic_index(db, file);
        let block_scope = index.try_expression_scope_id(anchor)?;
        // the nearest function / module ancestor — where a fresh binding becomes
        // a local (a `nonlocal` target in a function, `global` at module scope)
        for (_, scope) in index.ancestor_scopes(block_scope).skip(1) {
            match scope.kind() {
                ScopeKind::Module => return Some(CaptureKind::Global),
                ScopeKind::Function | ScopeKind::Lambda => return Some(CaptureKind::Nonlocal),
                _ => {}
            }
        }
        None
    }

    fn promoted_type_display(&self, expr: &Expr) -> Option<String> {
        let ty = expr.inferred_type(self)?;
        let promoted = ty.promote(self.db());
        let rendered = promoted.display(self.db()).to_string();
        // ty's default display tags type variables with their binding scope
        // for disambiguation (e.g. `T@render`); that suffix is not valid in
        // emitted Python source. strip it before returning so the rendered
        // type is a syntactically valid type expression
        Some(strip_binding_context_suffix(&rendered))
    }

    fn symbolic_type_fold(&self, expr: &Expr) -> Option<String> {
        let ty = expr.inferred_type(self)?;
        // fold concrete types and explicit `Any` (e.g. `dynamic + 1`, which ty
        // resolves to `Any`), but not the `Unknown` / Todo dynamics an
        // *unsupported* operation (`A + B` between two classes) resolves to —
        // leave those untouched so ty's own diagnostic stands
        if ty.is_dynamic() && !matches!(ty, Type::Dynamic(DynamicType::Any)) {
            return None;
        }
        // display with the standard (non-basedpython) renderer so literals come
        // out as `Literal[..]` rather than bare — the transpiler emits python
        Some(strip_binding_context_suffix(
            &ty.display(self.db()).to_string(),
        ))
    }

    fn class_typevars(&self, expr: &Expr) -> Option<Vec<(String, Option<String>)>> {
        let ty = expr.inferred_type(self)?;
        let class = ty.as_class_literal()?;
        let ctx = class.generic_context(self.db())?;
        Some(
            ctx.variables(self.db())
                .map(|tv| {
                    let name = tv.name(self.db()).to_string();
                    let default = tv
                        .default_type(self.db())
                        .map(|d| d.display(self.db()).to_string());
                    (name, default)
                })
                .collect(),
        )
    }

    fn unpacked_kwargs(&self, expr: &Expr) -> Option<UnpackedKwargsLowering> {
        let db = self.db();
        let ty = expr.inferred_type(self)?;
        // an unresolved name (but not an explicit `Any`) tells us nothing, so keep the
        // `ParamSpec` reading rather than committing to a shape from a type we don't have
        if ty.is_dynamic() && !matches!(ty, Type::Dynamic(DynamicType::Any)) {
            return Some(UnpackedKwargsLowering::ParameterPack);
        }
        Some(match ty.unpacked_kwargs(db)? {
            UnpackedKwargs::ParameterPack => UnpackedKwargsLowering::ParameterPack,
            UnpackedKwargs::TypedDict => UnpackedKwargsLowering::TypedDict,
            UnpackedKwargs::Protocol(members) => UnpackedKwargsLowering::Protocol(
                members
                    .into_iter()
                    .map(|(name, ty)| {
                        (
                            name.to_string(),
                            strip_binding_context_suffix(&ty.display(db).to_string()),
                        )
                    })
                    .collect(),
            ),
        })
    }

    fn class_first_typevar_is_paramspec(&self, expr: &Expr) -> bool {
        let Some(ty) = expr.inferred_type(self) else {
            return false;
        };
        let Some(class) = ty.as_class_literal() else {
            return false;
        };
        let Some(ctx) = class.generic_context(self.db()) else {
            return false;
        };
        ctx.variables(self.db())
            .next()
            .is_some_and(|tv| tv.is_paramspec(self.db()))
    }

    fn class_keyword_pack_index(&self, expr: &Expr) -> Option<usize> {
        let ty = expr.inferred_type(self)?;
        let class = ty.as_class_literal()?;
        let ctx = class.generic_context(self.db())?;
        ctx.variables(self.db())
            .position(|tv| tv.is_keyword_variadic(self.db()))
    }

    fn propagate_absent_test(&self, expr: &Expr) -> Option<AbsentTest> {
        let ty = expr.inferred_type(self)?;
        let db = self.db();
        if matches!(
            ty,
            Type::KnownInstance(KnownInstanceType::WrappedOptional(_))
        ) {
            return Some(AbsentTest::WrappedOptional);
        }
        let base_exception = KnownClass::BaseException.to_instance(db);
        let elements: Vec<Type> = match ty {
            Type::Union(union) => union.elements(db).to_vec(),
            other => vec![other],
        };
        // an exception arm wins over a `None` arm: a `T | E` (or a decomposed
        // `(T ? E)?` carrying both) propagates the error. `Any`/`Unknown` arms
        // are assignable to anything, so exclude them from the exception probe
        if elements
            .iter()
            .any(|t| !t.is_dynamic() && t.is_assignable_to(db, base_exception))
        {
            Some(AbsentTest::Result)
        } else if elements.iter().any(|t| t.is_none(db)) {
            Some(AbsentTest::Optional)
        } else {
            None
        }
    }

    fn wrapped_optional(&self, expr: &Expr) -> bool {
        matches!(
            expr.inferred_type(self),
            Some(Type::KnownInstance(KnownInstanceType::WrappedOptional(_)))
        )
    }

    fn call_result_is_typevar_derived(&self, callee: &Expr) -> bool {
        callee.inferred_type(self).is_some_and(|ty| {
            ty_python_semantic::types::soundness::call_result_is_typevar_derived(self.db(), ty)
        })
    }

    fn is_specialized_generic_instance(&self, expr: &Expr) -> bool {
        expr.inferred_type(self).is_some_and(|ty| {
            ty_python_semantic::types::soundness::is_specialized_generic_instance(self.db(), ty)
        })
    }

    fn soundness_check_plan(&self, expr: &Expr) -> Option<SoundnessCheck> {
        let ty = expr.inferred_type(self)?;
        ty_python_semantic::types::soundness::runtime_check_plan(self.db(), self.file(), ty)
    }

    fn call_positional_param_plan(&self, callee: &Expr, index: usize) -> Option<SoundnessCheck> {
        let ty = callee.inferred_type(self)?;
        ty_python_semantic::types::soundness::parameter_check_plan(
            self.db(),
            self.file(),
            ty,
            ty_python_semantic::types::soundness::ArgSelector::Positional(index),
        )
    }

    fn call_keyword_param_plan(&self, callee: &Expr, name: &str) -> Option<SoundnessCheck> {
        let ty = callee.inferred_type(self)?;
        ty_python_semantic::types::soundness::parameter_check_plan(
            self.db(),
            self.file(),
            ty,
            ty_python_semantic::types::soundness::ArgSelector::Keyword(name),
        )
    }

    fn cast_check_plan(&self, type_expr: &Expr) -> Option<CastCheck> {
        let ty = type_expr.inferred_type(self)?;
        ty_python_semantic::types::soundness::cast_check_plan(self.db(), self.file(), ty)
    }

    fn cast_is_redundant(&self, value: &Expr, target: &Expr) -> bool {
        let (Some(value_ty), Some(target_ty)) =
            (value.inferred_type(self), target.inferred_type(self))
        else {
            return false;
        };
        ty_python_semantic::types::soundness::cast_is_redundant(self.db(), value_ty, target_ty)
    }

    fn cast_target_is_unverifiable(&self, type_expr: &Expr) -> bool {
        let Some(ty) = type_expr.inferred_type(self) else {
            return false;
        };
        ty_python_semantic::types::soundness::cast_target_is_unverifiable_protocol(
            self.db(),
            self.file(),
            ty,
        )
    }

    fn trailing_lambda_keyword(&self, callee: &Expr) -> Option<String> {
        SemanticModel::trailing_lambda_keyword(self, callee)
    }

    fn trailing_lambda_callee_is_once(&self, callee: &Expr) -> bool {
        SemanticModel::trailing_lambda_callee_is_once(self, callee)
    }

    fn implicit_context_arguments(&self, call: &ExprCall) -> Vec<(String, String)> {
        let Some(callee) = call.func.inferred_type(self) else {
            return Vec::new();
        };
        ty_python_semantic::types::context_params::implicit_context_arguments(
            self.db(),
            self.file(),
            callee,
            call,
        )
        .into_iter()
        .map(|(parameter, variable)| (parameter.to_string(), variable.to_string()))
        .collect()
    }

    fn is_string_like(&self, expr: &Expr) -> bool {
        let Some(ty) = expr.inferred_type(self) else {
            return false;
        };
        let db = self.db();
        !ty.is_dynamic() && ty.is_assignable_to(db, KnownClass::Str.to_instance(db))
    }

    fn annotation_is_character(&self, annotation: &Expr) -> bool {
        annotation
            .inferred_type(self)
            .is_some_and(|ty| character::denotes_character(self.db(), ty))
    }

    fn is_character_instance(&self, expr: &Expr) -> bool {
        expr.inferred_type(self)
            .is_some_and(|ty| character::is_character_instance(self.db(), ty))
    }

    fn framework_class_role(&self, class_def: &StmtClassDef) -> Option<FrameworkRole> {
        let ty = class_def.inferred_type(self)?;
        let class = ty.as_class_literal()?;
        ty_python_semantic::types::class_framework_role(self.db(), class)
    }

    fn inferred_annotation(&self, expr: &Expr) -> Option<String> {
        let ty = expr.inferred_type(self)?;
        // a dynamic part (`Unknown` from an empty `[]`, an unresolved import,
        // `Any`) has no faithful annotation — leave the assignment bare
        if ty.has_dynamic(self.db()) {
            return None;
        }
        let promoted = ty.promote(self.db());
        Some(strip_binding_context_suffix(
            &promoted.display(self.db()).to_string(),
        ))
    }

    fn class_body_annotation_is_semantic(&self, class_def: &StmtClassDef) -> bool {
        class_def
            .inferred_type(self)
            .and_then(Type::as_class_literal)
            .is_some_and(|class| {
                ty_python_semantic::types::class_body_annotation_is_semantic(self.db(), class)
            })
    }

    fn is_field_specifier(&self, expr: &Expr) -> bool {
        matches!(
            expr.inferred_type(self),
            Some(Type::KnownInstance(KnownInstanceType::Field(_)))
        )
    }
}

/// Strip ty's `@<scope>` binding-context suffix from type variable display
/// (e.g. `T@render` → `T`, `dict[str, T@render]` → `dict[str, T]`). Used
/// when feeding ty's display string back into emitted Python source where
/// the suffix would be invalid syntax
fn strip_binding_context_suffix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            // skip `@` and any following identifier chars
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}
