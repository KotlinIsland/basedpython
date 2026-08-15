//! implicit-argument resolution for basedpython `context` parameters
//!
//! a `context` parameter that no explicit argument matches is filled from the
//! `context` declarations visible at the call site. resolution is by
//! assignability, not by name: a declaration is a candidate when its type is
//! assignable to the parameter's declared type. the innermost scope with at
//! least one candidate wins; more than one candidate in that scope is
//! ambiguous. in the scope containing the call only declarations lexically
//! before the call are considered; enclosing-scope declarations count
//! regardless of position (they are read late, like any closed-over name).
//! a function's own `context` parameters are declarations in its body, so
//! context requirements propagate through call chains
//!
//! the names a trailing lambda block binds implicitly count too: `it`, and
//! `self` when the block's callback declares a receiver. nobody writes either,
//! so they are ambient in the block body the same way a `context` declaration
//! is ambient in its scope
//!
//! the lowering writes the resolved *name* at the call site, so a candidate a
//! nearer scope shadows is not offered — the emitted argument would read that
//! scope's value instead
//!
//! candidates are typed at their declaration site (`binding_type` of the
//! declaration's definition), so a later reassignment that changes the type
//! is not accounted for

use ruff_db::files::{File, FileRange};
use ruff_db::parsed::parsed_module;
use ruff_python_ast as ast;
use ruff_python_ast::name::Name;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_python_core::definition::Definition;
use ty_python_core::scope::{NodeWithScopeKind, ScopeId, ScopeKind};
use ty_python_core::{place_table, semantic_index};

use crate::Db;
use crate::types::ProgramEnvironment;
use crate::types::receivers::{ImplicitReceiverName, implicit_receiver_name};
use crate::types::soundness::single_signature;
use crate::types::trailing_lambda::{enclosing_block_callee_type, trailing_lambda_it_type};
use crate::types::{Type, binding_type};

/// the outcome of resolving one unmatched `context` parameter at a call site
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContextResolution<'db> {
    /// exactly one candidate in the winning scope matches
    Resolved {
        name: Name,
        ty: Type<'db>,
        binding: CandidateBinding<'db>,
    },
    /// no visible candidate is assignable to the parameter
    NotFound,
    /// several candidates in the winning scope match, in source order
    Ambiguous(Vec<Name>),
}

/// a value that can fill a `context` parameter, found in one scope
struct Candidate<'db> {
    name: Name,
    /// full range of the declaration statement — used to exclude a
    /// self-referential declaration from its own value's call sites. `None`
    /// for a name a trailing lambda block binds implicitly: it is bound before
    /// the body runs, and has no value expression a call could sit inside
    range: Option<TextRange>,
    binding: CandidateBinding<'db>,
}

/// where a candidate's type comes from, and how a call site can name it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateBinding<'db> {
    /// a `context` declaration or `context` parameter written in the source
    Written(Definition<'db>),
    /// `it`, which a trailing lambda block binds implicitly. The block's `it`
    /// parameter is synthetic and carries no annotation, so its type comes from
    /// the callee's callback signature rather than from a definition
    BlockArgument(Type<'db>),
    /// `self`, the receiver a trailing lambda block binds implicitly. The
    /// lowering gives the receiver a name of its own, so a call site filling a
    /// `context` parameter from it cannot simply write `self`
    BlockReceiver(Type<'db>),
}

impl<'db> CandidateBinding<'db> {
    fn ty(self, db: &'db dyn Db) -> Type<'db> {
        match self {
            Self::Written(definition) => binding_type(db, definition),
            Self::BlockArgument(ty) | Self::BlockReceiver(ty) => ty,
        }
    }
}

/// resolve the implicit argument for one unmatched `context` parameter of a
/// call at `call_offset` inside `scope`
pub(crate) fn resolve_context_argument<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    call_offset: TextSize,
    parameter_ty: Type<'db>,
) -> ContextResolution<'db> {
    let file = scope.file(db);
    let index = semantic_index(db, db.program_file(file));
    let module = parsed_module(db, db.program_file(file).python_file(db)).load(db);

    // a trailing lambda block's implicit names belong to the block's own scope,
    // which is the first enclosing scope that is not a comprehension — a block
    // body may open one, and the names stay ambient inside it
    let mut reached_block_scope = false;
    // the scopes already passed on the way out from the call. the lowering writes
    // the resolved *name* at the call site, so a nearer scope binding that name
    // would make the emitted argument read a different value than the one
    // resolved here
    let mut nearer_scopes: Vec<ScopeId<'db>> = Vec::new();

    for (file_scope_id, ancestor) in index.visible_ancestor_scopes(scope.file_scope_id(db)) {
        let ancestor_scope = file_scope_id.to_scope_id(db, db.program_file(file));
        let is_call_scope = file_scope_id == scope.file_scope_id(db);
        let mut candidates: Vec<Candidate<'db>> = Vec::new();
        // the implicit names come first, so a `context` declaration in the block
        // body that reuses one of their names shadows it
        if !reached_block_scope && !matches!(ancestor.kind(), ScopeKind::Comprehension) {
            reached_block_scope = true;
            collect_block_candidates(db, env, file, scope, &mut candidates);
        }
        collect_candidates(index, ancestor.node(), &module, &mut candidates);

        // in the call's own scope only declarations lexically before the call
        // are visible; everywhere a declaration never feeds a call nested in
        // its own value expression
        candidates.retain(|candidate| {
            candidate.range.is_none_or(|range| {
                !range.contains(call_offset) && (!is_call_scope || range.start() < call_offset)
            })
        });

        // a scope between the call and this one holding the name shadows the
        // candidate, exactly as it would shadow an ordinary load of that name
        candidates.retain(|candidate| {
            !nearer_scopes.iter().any(|nearer| {
                place_table(db, *nearer)
                    .symbol_by_name(&candidate.name)
                    .is_some_and(|place| place.is_bound() || place.is_declared())
            })
        });

        // a name redeclared later shadows its earlier declaration
        candidates.reverse();
        let mut seen = Vec::new();
        candidates.retain(|candidate| {
            let fresh = !seen.contains(&candidate.name);
            if fresh {
                seen.push(candidate.name.clone());
            }
            fresh
        });
        candidates.reverse();

        let matching: Vec<(Name, Type<'db>, CandidateBinding<'db>)> = candidates
            .into_iter()
            .filter_map(|candidate| {
                let ty = candidate.binding.ty(db);
                ty.is_assignable_to(db, env, parameter_ty).then_some((
                    candidate.name,
                    ty,
                    candidate.binding,
                ))
            })
            .collect();

        match matching.len() {
            0 => {}
            1 => {
                let (name, ty, binding) = matching.into_iter().next().expect("length checked");
                return ContextResolution::Resolved { name, ty, binding };
            }
            _ => {
                return ContextResolution::Ambiguous(
                    matching.into_iter().map(|(name, _, _)| name).collect(),
                );
            }
        }

        nearer_scopes.push(ancestor_scope);
    }

    ContextResolution::NotFound
}

/// one `context` parameter of a call site and the value filling it
#[derive(Debug, Clone)]
pub struct ImplicitContextArgument {
    /// the `context` parameter left unmatched by the explicit arguments
    pub parameter: Name,
    /// the in-scope value resolved for it, spelled as the source spells it
    pub variable: Name,
    /// where that name is written, for an IDE to navigate to. `None` for a name
    /// a trailing lambda block binds implicitly — nothing is written for it
    pub declaration: Option<FileRange>,
    /// whether `variable` is the receiver a trailing lambda block binds. The
    /// lowering gives the receiver a name of its own, so the transpiler must
    /// write that rather than `self`
    pub is_block_receiver: bool,
}

/// the implicit arguments the transpiler must append to `call`: for each
/// `context` parameter of `callee` that no explicit argument matches, the
/// in-scope declaration that fills it, in parameter order. parameters that
/// fail to resolve are skipped — checking already reported them. calls that
/// use `*` / `**` unpacking are skipped entirely: whether the unpacking covers
/// a parameter is not knowable statically
pub fn implicit_context_arguments<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    callee: Type<'db>,
    call: &ast::ExprCall,
) -> Vec<ImplicitContextArgument> {
    let has_unpacking = call.arguments.args.iter().any(ast::Expr::is_starred_expr)
        || call.arguments.keywords.iter().any(|kw| kw.arg.is_none());
    if has_unpacking {
        return Vec::new();
    }

    let Some(signature) = single_signature(db, callee) else {
        return Vec::new();
    };
    let parameters = signature.parameters();

    let index = semantic_index(db, db.program_file(file));
    let Some(file_scope_id) = index.try_expression_scope_id(&ast::ExprRef::from(call)) else {
        return Vec::new();
    };
    let scope = file_scope_id.to_scope_id(db, db.program_file(file));

    let positional_count = call.arguments.args.len();
    let mut positional_index = 0;
    let mut implicit = Vec::new();
    for parameter in parameters {
        let fills_positional_slot = parameter.is_positional();
        let matched_positionally = fills_positional_slot && positional_index < positional_count;
        if fills_positional_slot {
            positional_index += 1;
        }
        if !parameter.is_context() {
            continue;
        }
        let Some(name) = parameter.name() else {
            continue;
        };
        let matched_by_keyword = call
            .arguments
            .keywords
            .iter()
            .any(|kw| kw.arg.as_ref().is_some_and(|arg| arg.id == *name));
        if matched_positionally || matched_by_keyword {
            continue;
        }
        if let ContextResolution::Resolved {
            name: variable,
            binding,
            ..
        } = resolve_context_argument(
            db,
            env,
            scope,
            call.range().start(),
            parameter.annotated_type(),
        ) {
            let declaration = match binding {
                CandidateBinding::Written(definition) => Some(definition.focus_range(
                    db,
                    &parsed_module(db, db.program_file(file).python_file(db)).load(db),
                )),
                CandidateBinding::BlockArgument(_) | CandidateBinding::BlockReceiver(_) => None,
            };
            implicit.push(ImplicitContextArgument {
                parameter: name.clone(),
                variable,
                declaration,
                is_block_receiver: matches!(binding, CandidateBinding::BlockReceiver(_)),
            });
        }
    }
    implicit
}

/// collect the names the trailing lambda block containing `scope` binds
/// implicitly: `self` when its callback declares a receiver, and `it`.
///
/// Only the innermost enclosing block is asked. Every block binds `it`, so a
/// nearer block always shadows an outer one's — and the lowering gives every
/// block's receiver the same name, so an outer receiver is shadowed too. Both
/// queries answer for the innermost block only, which keeps a name resolved
/// here meaning the same thing it means to the transpiler.
///
/// `self` goes through the same query the checker resolves a bare `self` in a
/// block with, so a method's own `self` — or anything else claiming the name —
/// keeps its meaning here as well. `it` is offered only when the callee gives
/// it a type: an uninspectable callee leaves it `Unknown`, which is assignable
/// to every `context` parameter and would fill them all with a value the block
/// never receives
fn collect_block_candidates<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    file: File,
    scope: ScopeId<'db>,
    out: &mut Vec<Candidate<'db>>,
) {
    if let Some(ImplicitReceiverName::Receiver(ty)) =
        implicit_receiver_name(db, env, file, scope, "self", None)
    {
        out.push(Candidate {
            name: Name::new_static("self"),
            range: None,
            binding: CandidateBinding::BlockReceiver(ty),
        });
    }

    if let Some(callee_ty) = enclosing_block_callee_type(db, scope)
        && let Some(ty) = trailing_lambda_it_type(db, callee_ty)
    {
        out.push(Candidate {
            name: Name::new_static("it"),
            range: None,
            binding: CandidateBinding::BlockArgument(ty),
        });
    }
}

/// collect the `context` declarations and `context` parameters belonging to
/// one scope, in source order. nested scopes are not entered — their
/// declarations belong to them
fn collect_candidates<'db>(
    index: &ty_python_core::SemanticIndex<'db>,
    node: &NodeWithScopeKind,
    module: &ruff_db::parsed::ParsedModuleRef,
    out: &mut Vec<Candidate<'db>>,
) {
    let mut push_params = |parameters: &ast::Parameters| {
        for parameter in parameters.iter().map(ast::AnyParameterRef::as_parameter) {
            if parameter.is_context
                && let Some(definition) = index.try_definition(parameter)
            {
                out.push(Candidate {
                    name: parameter.name.id.clone(),
                    range: Some(parameter.range()),
                    binding: CandidateBinding::Written(definition),
                });
            }
        }
    };

    match node {
        NodeWithScopeKind::Module => {
            collect_declarations(index, module.suite(), out);
        }
        NodeWithScopeKind::Function(function) => {
            let function = function.node(module);
            push_params(&function.parameters);
            collect_declarations(index, &function.body, out);
        }
        NodeWithScopeKind::Lambda(lambda) => {
            if let Some(parameters) = lambda.node(module).parameters.as_deref() {
                push_params(parameters);
            }
        }
        NodeWithScopeKind::Class(class) => {
            collect_declarations(index, &class.node(module).body, out);
        }
        _ => {}
    }
}

/// walk a statement suite for `context` declarations, descending into
/// compound statements but not into nested scopes
fn collect_declarations<'db>(
    index: &ty_python_core::SemanticIndex<'db>,
    suite: &[ast::Stmt],
    out: &mut Vec<Candidate<'db>>,
) {
    for stmt in suite {
        match stmt {
            ast::Stmt::AnnAssign(decl) => {
                if let Some(target) = context_declaration_target(decl)
                    && let Some(definition) = index.try_definition(decl)
                {
                    out.push(Candidate {
                        name: target.id.clone(),
                        range: Some(decl.range()),
                        binding: CandidateBinding::Written(definition),
                    });
                }
            }
            ast::Stmt::If(stmt) => {
                collect_declarations(index, &stmt.body, out);
                for clause in &stmt.elif_else_clauses {
                    collect_declarations(index, &clause.body, out);
                }
            }
            ast::Stmt::While(stmt) => {
                collect_declarations(index, &stmt.body, out);
                collect_declarations(index, &stmt.orelse, out);
            }
            ast::Stmt::For(stmt) => {
                collect_declarations(index, &stmt.body, out);
                collect_declarations(index, &stmt.orelse, out);
            }
            ast::Stmt::With(stmt) => {
                collect_declarations(index, &stmt.body, out);
            }
            ast::Stmt::Try(stmt) => {
                collect_declarations(index, &stmt.body, out);
                for ast::ExceptHandler::ExceptHandler(handler) in &stmt.handlers {
                    collect_declarations(index, &handler.body, out);
                }
                collect_declarations(index, &stmt.orelse, out);
                collect_declarations(index, &stmt.finalbody, out);
            }
            ast::Stmt::Match(stmt) => {
                for case in &stmt.cases {
                    collect_declarations(index, &case.body, out);
                }
            }
            _ => {}
        }
    }
}

/// if `decl` is a `context NAME [: T] = value` declaration (recognized by its
/// synthetic `__context__` annotation marker), return the target name
fn context_declaration_target(decl: &ast::StmtAnnAssign) -> Option<&ast::ExprName> {
    let is_marker = match &*decl.annotation {
        ast::Expr::Name(name) => name.id == "__context__",
        ast::Expr::Subscript(subscript) => {
            matches!(&*subscript.value, ast::Expr::Name(name) if name.id == "__context__")
        }
        _ => false,
    };
    if !is_marker {
        return None;
    }
    decl.target.as_name_expr()
}
