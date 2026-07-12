//! tracking for "fluid specialization" candidate bindings
//!
//! a binding like `a = [1]` or `a = A(1)` creates a generic instance whose
//! specialization was inferred rather than declared. while no other observer
//! of the value exists, later uses of the binding are allowed to refine
//! ("widen") the inferred specialization instead of being checked against it.
//! the semantic index records every use of such a candidate binding together
//! with a syntactic classification of the use; type inference later turns the
//! classified uses into widening and locking events
//!
//! this generalizes the original full-scope inference for unconstrained
//! (empty) collection literals to non-empty collection literals and
//! constructor calls

use ruff_text_size::TextRange;

use crate::ast_ids::ExpressionNodeKey;
use crate::statement::Statement;

/// how a use of a fluid candidate binding interacts with its specialization
#[derive(Clone, Copy, Debug, PartialEq, Eq, salsa::Update, get_size2::GetSize)]
pub enum FluidUseRole {
    /// receiver of a bound-method call `a.m(...)` — the call's arguments can
    /// constrain the specialization
    MethodReceiver,
    /// object of a subscript store `a[k] = v` — the key and value can
    /// constrain the specialization
    SubscriptStore,
    /// a read that can neither constrain nor leak the specialization
    /// (`a[k]`, `if a:`, `for x in a:`, a bare expression statement)
    Read,
    /// a use inferred with bidirectional type context (call argument, return
    /// value, annotated assignment) — adopts the contextual specialization
    /// and locks the binding if the context constrains the class typevars
    TypeContextual,
    /// any other use — the value escapes to unknown observers, locking the
    /// specialization
    Escape,
}

impl FluidUseRole {
    /// whether constraints learned at this use must be read back from the
    /// containing statement's inference
    pub fn contributes_constraints(self) -> bool {
        matches!(
            self,
            FluidUseRole::MethodReceiver
                | FluidUseRole::SubscriptStore
                | FluidUseRole::TypeContextual
        )
    }
}

/// a single use of a fluid candidate binding, in source order
#[derive(Clone, Debug, PartialEq, Eq, salsa::Update, get_size2::GetSize)]
pub struct FluidUse<'db> {
    /// the use expression
    pub use_expression: ExpressionNodeKey,
    /// the range of the use expression
    pub range: TextRange,
    /// how the use interacts with the fluid specialization
    pub role: FluidUseRole,
    /// whether the use is an argument of a call whose result is discarded — a
    /// returned observer of the specialization would not survive such a call
    pub discarded_call_result: bool,
    /// the range of the containing statement
    pub statement_range: TextRange,
    /// ranges of enclosing loop statements, outermost first. a use and an
    /// event that share an enclosing loop can execute in either order
    /// regardless of their source positions
    pub loops: Box<[TextRange]>,
    /// the containing statement, for roles that contribute constraints
    pub statement: Option<Statement<'db>>,
}

impl FluidUse<'_> {
    /// whether `event` can have executed before this use: it appears earlier
    /// in the source, or shares an enclosing loop with this use
    pub fn may_follow(&self, event: &FluidUse) -> bool {
        event.range.start() <= self.range.start()
            || self
                .loops
                .iter()
                .any(|loop_range| event.loops.contains(loop_range))
    }
}
