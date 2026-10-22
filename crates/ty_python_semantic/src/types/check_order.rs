//! basedpython: inferring a file's scopes in the order checking it does, each before the scopes
//! inside it
//!
//! a scope inferred before the scope around it reads that scope's names before that scope has
//! inferred the conditions deciding whether they are bound there, and each of those conditions
//! is then inferred inside the inference of the next. inferred in order, every condition a
//! question reads is already inferred when it is asked

use ty_python_core::scope::{FileScopeId, ScopeId};
use ty_python_core::{ProgramFile, semantic_index};

use crate::Db;
use crate::types::TypeContext;
use crate::types::infer::{ScopeInference, infer_complete_scope_types, infer_scope_types};

/// that the scopes a question about the running program reads are inferred, each before the
/// scopes inside it: the bindings a name has when the program runs read every binding the
/// module makes and every condition deciding whether each is made, those after the read
/// included ([`crate::place::runtime_binding`]).
///
/// only [`infer_file_in_check_order`] and [`infer_scope_in_check_order`] make one, so
/// inference, which those run, cannot ask such a question of the scope it is inferring. one
/// is for one file, and a question asked with it is asked of that file
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InferredInCheckOrder<'db> {
    /// the file whose scopes are inferred, and so the only one a question may be asked of
    file: ProgramFile<'db>,
}

impl<'db> InferredInCheckOrder<'db> {
    pub(crate) fn file(self) -> ProgramFile<'db> {
        self.file
    }
}

/// infers every scope of `file` in the order checking it does, handing each scope's inference to
/// `each`
pub(crate) fn infer_file_in_check_order<'db>(
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    mut each: impl FnMut(ScopeId<'db>, &'db ScopeInference<'db>),
) -> InferredInCheckOrder<'db> {
    for scope in semantic_index(db, file).scope_ids() {
        // a scope that takes type context is inferred inside the scope around it
        if !scope.accepts_type_context(db) {
            each(scope, infer_scope_types(db, scope, TypeContext::default()));
        }
    }
    InferredInCheckOrder { file }
}

/// infers every scope around `scope`, outermost first, and then `scope`
pub(crate) fn infer_scope_in_check_order<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
) -> (&'db ScopeInference<'db>, InferredInCheckOrder<'db>) {
    let file = scope.program_file(db);
    let enclosing: Vec<FileScopeId> = semantic_index(db, file)
        .ancestor_scopes(scope.file_scope_id(db))
        .skip(1)
        .map(|(ancestor, _)| ancestor)
        .collect();
    for ancestor in enclosing.into_iter().rev() {
        infer_complete_scope_types(db, ancestor.to_scope_id(db, file));
    }
    (
        infer_complete_scope_types(db, scope),
        InferredInCheckOrder { file },
    )
}
