//! Running a transpile from a request handler.
//!
//! The transpiler builds databases of its own — a single-file one holding the source
//! it is lowering, and the project rebuilt over that source when a pre-pass rewrote
//! it. Reading a salsa field attaches the database it belongs to, and attaching a
//! second one while a first is attached is a panic: "Cannot change database
//! mid-query". Every request handler runs inside `salsa::attach(&db, …)`, so every
//! one of those reads is a panic here.
//!
//! Attachment is per thread, so a thread that has attached nothing is a thread the
//! nested build is legal on — which is the state the `by` command line runs in, and
//! why none of this reproduces there. That is also why it went unnoticed: the tests
//! that cover these same operations call the library directly.
//!
//! This is a workaround and worth naming as one. The nesting belongs to the
//! transpiler, not to any request, so the fix that would remove this is for a
//! type-aware pass to be handed a database rather than construct one. Until then
//! every caller that already holds one attached has to arrange not to be — the same
//! reason `by_stage::emit`'s transpile loop cannot be parallelised.

use std::panic::AssertUnwindSafe;

use ruff_db::panic::catch_unwind;

/// Run `op` on a thread that has attached no database.
///
/// `Err` when it panicked, which the caller reports rather than propagates: a client
/// has one shape to read, and a request that got no answer at all leaves whatever it
/// was about to do with the result in an unknown state.
///
/// The panic is caught *inside* the thread rather than read off its join handle. The
/// hook that captures a panic rather than printing it is armed per thread, so one
/// caught only by the join would still have printed a raw backtrace to the server's
/// log on the way out. Caught here it is logged the way the rest of the server logs
/// a panicking handler, and the caller is told only that there is nothing to return.
pub(super) fn transpile_detached<R: Send>(op: impl FnOnce() -> R + Send) -> Result<R, ()> {
    let caught =
        std::thread::scope(|scope| scope.spawn(|| catch_unwind(AssertUnwindSafe(op))).join());
    match caught {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(panic)) => {
            tracing::error!("the transpiler panicked: {panic}");
            Err(())
        }
        // the thread unwound past the catch, which leaves nothing to report but the
        // same answer: there is no result
        Err(_) => {
            tracing::error!("the transpiler's thread failed");
            Err(())
        }
    }
}
