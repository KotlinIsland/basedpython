//! basedpython implicit receivers (`int.() -> str`)
//!
//! a callable type may declare a *receiver*: the leading positional parameter of
//! `int.() -> str` is bound as the receiver rather than passed like an ordinary
//! argument. it stays a real parameter — any function of the same shape satisfies
//! the type, and the callable can be called directly — and additionally unlocks
//! two forms this module answers for the checker and the transpiler:
//!
//! - `x.fn()`, where `fn` is a name in scope declared as a receiver callable that
//!   accepts `x`. only reached when `x` has no member `fn` of its own
//! - the body of a [trailing lambda] block bound to a receiver callback, where the
//!   receiver's members are in scope unqualified (`imag` for an `int` receiver)
//!   and the receiver itself is spelled `self`
//!
//! the attribute form is a *last* fallback: a declared member, and an applicable
//! extension member, both keep their ordinary meaning, so it is purely additive.
//!
//! the block form is not. a block's receiver sits in the scope tower at the
//! block's own level — inside the names the block itself binds, and outside
//! everything else — so it is resolved *before* the ordinary lookup and outranks
//! the enclosing function's locals, the module's globals and the builtins alike.
//! see [`implicit_receiver_name`] for the one thing that can turn it down
//!
//! [trailing lambda]: crate::types::trailing_lambda

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{self as ast, Expr};
use rustc_hash::FxHashMap;
use ty_python_core::node_key::NodeKey;
use ty_python_core::scope::ScopeId;
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::place::{ConsideredDefinitions, symbol};
use crate::types::ProgramEnvironment;
use crate::types::call::{Argument, CallArguments};
use crate::types::name_fallback::claimed_by_name_resolution;
use crate::types::signatures::{Parameters, Signature};
use crate::types::{Type, UnionType};

/// the single signature of `ty` when it is a callable that declares a receiver
fn receiver_signature<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<&'db Signature<'db>> {
    let Type::Callable(callable) = ty else {
        return None;
    };
    let [signature] = callable.signatures(db).overloads.as_slice() else {
        return None;
    };
    signature
        .parameters()
        .iter()
        .next()?
        .is_receiver()
        .then_some(signature)
}

/// the type `ty` binds as its receiver, when it is a receiver callable
pub(crate) fn receiver_type<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Type<'db>> {
    Some(
        receiver_signature(db, ty)?
            .parameters()
            .get_positional(0)?
            .annotated_type(),
    )
}

/// `ty` with its receiver supplied — the callable `x.fn` evaluates to. `None`
/// when `ty` is not a receiver callable, or its receiver does not accept
/// `receiver_ty`
fn bind_receiver<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    receiver_ty: Type<'db>,
) -> Option<Type<'db>> {
    let signature = receiver_signature(db, ty)?;
    let receiver = signature.parameters().get_positional(0)?;
    if !receiver_ty.is_assignable_to(db, env, receiver.annotated_type()) {
        return None;
    }
    let rest = signature.parameters().iter().skip(1).cloned();
    Some(Type::single_callable(
        db,
        Signature::new_generic(
            signature.generic_context,
            Parameters::from_annotation(db, env, rest),
            signature.return_ty,
        ),
    ))
}

/// basedpython: the callable `x.fn` resolves to when `fn` names a receiver
/// callable in scope that accepts `x`, with the receiver already bound. The
/// name must be *declared* — a receiver callable is only ever spelled as an
/// annotation, and a declaration has one type wherever it is visible. A scope
/// that binds the name to anything else shadows it, the same way it would shadow
/// the name in an ordinary load
pub(crate) fn resolve_receiver_attribute<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    receiver_ty: Type<'db>,
    name: &str,
) -> Option<Type<'db>> {
    Some(resolve_receiver_attribute_in_scope(db, env, file, scope, receiver_ty, name)?.1)
}

/// [`resolve_receiver_attribute`], plus the scope whose declaration of `name`
/// answered.
///
/// Goto-definition needs the scope: the callable's declaration is an ordinary
/// name in an enclosing scope rather than a member of anything, so the type
/// alone cannot say where it was written
pub(crate) fn resolve_receiver_attribute_in_scope<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    receiver_ty: Type<'db>,
    name: &str,
) -> Option<(ScopeId<'db>, Type<'db>)> {
    let index = semantic_index(db, db.program_file(file));
    for (ancestor_id, _) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = ancestor_id.to_scope_id(db, db.program_file(file));
        let Some(place) = place_table(db, ancestor_scope).symbol_by_name(name) else {
            continue;
        };
        if !(place.is_bound() || place.is_declared()) {
            continue;
        }
        // the first scope that gives the name a value decides it, exactly as a
        // name load would. a scope that only *binds* it holds some other value,
        // which shadows an outer receiver callable rather than deferring to it
        if !place.is_declared() {
            return None;
        }
        let declared = symbol(
            db,
            ancestor_scope,
            name,
            ConsideredDefinitions::AllReachable,
        )
        .place
        .ignore_possibly_undefined()?;
        let bound = bind_receiver(db, env, declared, receiver_ty)?;
        return Some((ancestor_scope, bound));
    }
    None
}

/// basedpython: whether `attribute` resolves through an *implicit receiver* —
/// `x.fn` where `fn` names a receiver callable (`int.() -> str`) in scope rather
/// than a member of `x`. `receiver_ty` is the type of the attribute's own value.
/// The receiver form is the last fallback, so a declared member and an
/// applicable extension member both win over it
pub(crate) fn is_implicit_receiver_attribute<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    attribute: &ast::ExprAttribute,
    receiver_ty: Type<'db>,
) -> bool {
    // an optional-chain link resolves against the chain's *present* type — the
    // `None` it short-circuits with is not part of the receiver
    let receiver_ty = if attribute.optional || spine_has_optional(&attribute.value) {
        strip_none(db, env, receiver_ty)
    } else {
        receiver_ty
    };
    let name = attribute.attr.as_str();
    if !receiver_ty.member(db, env, name).place.is_undefined() {
        return false;
    }
    // an extension member wins over a receiver callable, matching the order the
    // two fallbacks run in during inference. resolving again here is near-free
    // in a file with no extensions: the applicable-extension list is a cached
    // query that comes back empty
    if crate::types::extensions::resolve_extension_member(db, env, file, receiver_ty, name)
        .is_some()
    {
        return false;
    }
    resolve_receiver_attribute(db, env, file, scope, receiver_ty, name).is_some()
}

/// whether any link of the attribute spine `expr` is an optional access
pub(crate) fn spine_has_optional(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(attribute) => attribute.optional || spine_has_optional(&attribute.value),
        Expr::Subscript(subscript) => spine_has_optional(&subscript.value),
        Expr::Call(call) => spine_has_optional(&call.func),
        _ => false,
    }
}

/// basedpython: `ty` without the `None` an optional chain unions in
pub(crate) fn strip_none<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> Type<'db> {
    let Type::Union(union) = ty else {
        return ty;
    };
    UnionType::from_elements(
        db,
        env,
        union
            .elements(db)
            .iter()
            .copied()
            .filter(|element| !element.is_none(db)),
    )
}

/// basedpython: what a bare name in a trailing lambda block resolves to through
/// the block's receiver
pub(crate) enum ImplicitReceiverName<'db> {
    /// `self` — the receiver itself
    Receiver(Type<'db>),
    /// a member of the receiver, read off it in the lowering
    Member(Type<'db>),
    /// a member an applicable `extension` supplies for the receiver, lowered to
    /// its backing function rather than an attribute read
    ExtensionMember {
        ty: Type<'db>,
        resolution: crate::types::extensions::ExtensionMemberResolution<'db>,
    },
}

impl<'db> ImplicitReceiverName<'db> {
    pub(crate) fn ty(&self) -> Type<'db> {
        match self {
            Self::Receiver(ty) | Self::Member(ty) | Self::ExtensionMember { ty, .. } => *ty,
        }
    }
}

/// basedpython: the receiver a trailing lambda block binds, for a use in
/// `scope`. `None` when `scope` is not inside a block, or the block's callback
/// declares no receiver
pub(crate) fn block_receiver_type<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> Option<Type<'db>> {
    let (_, callee_ty) = crate::types::trailing_lambda::enclosing_block(db, scope)?;
    crate::types::trailing_lambda::trailing_lambda_receiver_type(db, callee_ty)
}

/// basedpython: what a bare `name` in a trailing lambda block resolves to when
/// the block's callback declares a receiver: `self` is the receiver, and any
/// other name is looked up as a member of it.
///
/// The receiver sits in the scope tower at the block's own level, so it is
/// resolved before the ordinary lookup: only a name the block itself binds — the
/// implicit `it`, or anything the body assigns — keeps its meaning, and every
/// name the receiver supplies outranks the enclosing function's locals, the
/// module's globals and the builtins alike.
///
/// A *call* is the one thing that can turn the receiver down. `use_site` is the
/// name node being resolved; when it is the callee of a call whose shape the
/// receiver's member cannot accept, the walk continues outward to whatever else
/// declares the name — `y(1)` reaching a module-level `def y(a: int)` past a
/// receiver's nullary `y`. Applicability is decided by the call's *shape* alone
/// (how many positional arguments, which keywords), never by the argument types,
/// which are not yet inferred when a name is resolved. When no level of the
/// tower has an applicable candidate the receiver's member is used anyway, so
/// the call reports its own mismatch rather than an unresolved name
pub(crate) fn implicit_receiver_name<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    name: &str,
    use_site: Option<&ast::ExprName>,
) -> Option<ImplicitReceiverName<'db>> {
    let (block_scope, callee_ty) = crate::types::trailing_lambda::enclosing_block(db, scope)?;
    let receiver = crate::types::trailing_lambda::trailing_lambda_receiver_type(db, callee_ty)?;
    if bound_within_block(db, file, scope, block_scope, name) {
        return None;
    }
    let resolved = receiver_name(db, env, file, receiver, name)?;
    // a bare `href = …` in the block was not counted above, because it writes the
    // receiver's `href` rather than binding a name. that only holds where there
    // is a member to write: `self` is the receiver itself and an extension member
    // is a function rather than state, so neither is something an assignment
    // could have meant, and the assignment is an ordinary block local after all
    if !matches!(resolved, ImplicitReceiverName::Member(_))
        && assigned_within_block(db, file, scope, block_scope, name)
    {
        return None;
    }
    // the receiver's candidate does not fit this call — so hand the name back to
    // the levels of the tower outside the block, but only if one of them
    // actually claims it. `claimed_by_name_resolution` covers the whole visible
    // chain, and the bindings *inside* the block have already been ruled out
    // above, so what it answers here is "does anything outside the block claim
    // this name"
    if let Some(use_site) = use_site
        && let Some(arguments) =
            block_scope_call_arguments(db, block_scope).get(&NodeKey::from_node(use_site))
        && !accepts_call_shape(db, env, resolved.ty(), arguments)
        && claimed_by_name_resolution(db, env, file, scope, name)
    {
        return None;
    }
    Some(resolved)
}

/// the receiver's own candidate for `name`, with no regard for what else is in
/// scope: `self` is the receiver itself, and any other name is a member of it
fn receiver_name<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    receiver: Type<'db>,
    name: &str,
) -> Option<ImplicitReceiverName<'db>> {
    if name == "self" {
        return Some(ImplicitReceiverName::Receiver(receiver));
    }
    if let Some(member) = receiver
        .member(db, env, name)
        .place
        .ignore_possibly_undefined()
    {
        return Some(ImplicitReceiverName::Member(member));
    }
    // an extension of the receiver's type supplies members too, and the block's
    // scope is the receiver's — so `p:` inside a `div:` block reaches an
    // `extension Tag: def p` exactly as `self.p:` does. reached last, after the
    // receiver's own members, like every other extension lookup
    let resolution =
        crate::types::extensions::resolve_extension_member(db, env, file, receiver, name)?;
    Some(ImplicitReceiverName::ExtensionMember {
        ty: resolution.ty,
        resolution,
    })
}

/// basedpython: the receiver member a declaration inside a trailing lambda block
/// shadows.
///
/// A bare `href = …` in a block writes the receiver's `href`, and reading `href`
/// there means the receiver's `href`. `let href = …` takes the name for the block
/// instead — for the whole block, including the lines above it — so it is worth
/// saying out loud. `None` when the block has no receiver, or the receiver has no
/// such member, which is the ordinary case of declaring a local.
pub(crate) fn shadowed_receiver_member<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    name: &str,
) -> Option<Type<'db>> {
    if name == "self" {
        return None;
    }
    let (_, callee_ty) = crate::types::trailing_lambda::enclosing_block(db, scope)?;
    let receiver = crate::types::trailing_lambda::trailing_lambda_receiver_type(db, callee_ty)?;
    receiver
        .member(db, env, name)
        .place
        .ignore_possibly_undefined()
}

/// whether a bare assignment somewhere from the use out to the block itself gave
/// `name` a value — the binding [`bound_within_block`] deliberately looks past,
/// asked about on its own
fn assigned_within_block(
    db: &dyn Db,
    file: File,
    scope: ScopeId<'_>,
    block_scope: ScopeId<'_>,
    name: &str,
) -> bool {
    let index = semantic_index(db, db.program_file(file));
    let block_scope = block_scope.file_scope_id(db);
    for (ancestor_id, _) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = ancestor_id.to_scope_id(db, db.program_file(file));
        if place_table(db, ancestor_scope)
            .symbol_by_name(name)
            .is_some_and(ty_python_core::symbol::Symbol::is_bound_by_block_assignment)
        {
            return true;
        }
        if ancestor_id == block_scope {
            break;
        }
    }
    false
}

/// whether any scope from the use out to the block itself binds or declares
/// `name` — the one level of the scope tower that sits *inside* the receiver, so
/// the block's own `it` and anything it declares keep their meaning
///
/// a bare `href = …` in the block does *not* count. it writes to the receiver's
/// `href` when the receiver has one, and only falls back to binding a name of its
/// own when it does not — so counting it here would let the write take the name
/// away from the member it is supposed to be writing to. `let href = …` does
/// count: a declaration is how the block asks for a name of its own
fn bound_within_block(
    db: &dyn Db,
    file: File,
    scope: ScopeId<'_>,
    block_scope: ScopeId<'_>,
    name: &str,
) -> bool {
    let index = semantic_index(db, db.program_file(file));
    let block_scope = block_scope.file_scope_id(db);
    for (ancestor_id, _) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = ancestor_id.to_scope_id(db, db.program_file(file));
        if place_table(db, ancestor_scope)
            .symbol_by_name(name)
            .is_some_and(|symbol| {
                symbol.is_bound_outside_block_assignment() || symbol.is_declared()
            })
        {
            return true;
        }
        if ancestor_id == block_scope {
            break;
        }
    }
    false
}

/// whether `ty` can be called with a call of this shape, judged by matching the
/// arguments to parameters and nothing more — no argument has a type yet
fn accepts_call_shape<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
    arguments: &[ArgumentShape],
) -> bool {
    let arguments =
        CallArguments::from_argument_shapes(arguments.iter().map(ArgumentShape::as_argument));
    ty.bindings(db, env)
        .match_parameters(db, env, &arguments)
        .parameters_matched(db)
}

/// one argument of a call, as much of it as its *shape* records
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) enum ArgumentShape {
    Positional,
    /// a starred positional argument (`*args`), of unknown length
    Variadic,
    Keyword(ast::name::Name),
    /// a double-starred argument (`**kwargs`), of unknown keys
    Keywords,
}

impl ArgumentShape {
    fn as_argument(&self) -> Argument<'_> {
        match self {
            Self::Positional => Argument::Positional,
            Self::Variadic => Argument::Variadic,
            Self::Keyword(name) => Argument::Keyword(name),
            Self::Keywords => Argument::Keywords,
        }
    }
}

/// the shape of every call in the trailing lambda block `scope` whose callee is
/// a bare name, keyed by that name's node.
///
/// Resolving a name against the block's receiver needs to know whether the name
/// is being called and with what — which the AST answers and the name node on
/// its own does not. Both the checker and the transpiler read this one map, so
/// the two cannot disagree about which calls the receiver is applicable to.
/// Tracked because a block resolves every one of its names against its receiver,
/// and walking the body once per name would be quadratic
#[salsa::tracked(returns(ref), heap_size = ruff_memory_usage::heap_size)]
fn block_scope_call_arguments<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> FxHashMap<NodeKey, Box<[ArgumentShape]>> {
    let mut collector = CallArgumentShapes {
        shapes: FxHashMap::default(),
    };
    let file = scope.file(db);
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);
    if let Some(function) = scope.node(db).as_function() {
        for statement in &function.node(&module).body {
            collector.visit_stmt(statement);
        }
    }
    collector.shapes.shrink_to_fit();
    collector.shapes
}

struct CallArgumentShapes {
    shapes: FxHashMap<NodeKey, Box<[ArgumentShape]>>,
}

impl<'ast> Visitor<'ast> for CallArgumentShapes {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && let Expr::Name(name) = call.func.as_ref()
        {
            let shapes = call
                .arguments
                .iter_source_order()
                .map(|argument| match argument {
                    ast::ArgOrKeyword::Arg(Expr::Starred(_)) => ArgumentShape::Variadic,
                    ast::ArgOrKeyword::Arg(_) => ArgumentShape::Positional,
                    ast::ArgOrKeyword::Keyword(keyword) => match &keyword.arg {
                        Some(argument) => ArgumentShape::Keyword(argument.id.clone()),
                        None => ArgumentShape::Keywords,
                    },
                })
                .collect();
            self.shapes.insert(NodeKey::from_node(name), shapes);
        }
        walk_expr(self, expr);
    }
}
