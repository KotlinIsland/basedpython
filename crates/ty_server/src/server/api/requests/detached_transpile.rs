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
///
/// ## a cancellation is not a panic
///
/// Salsa stops a query by unwinding it with a [`salsa::Cancelled`] payload: an edit
/// arriving while `op` reads the project database is a write, and a write waits for
/// every reader to let go of the revision it is replacing. That unwind is routine — it
/// happens whenever a document changes under a request, and in an editor that is
/// whenever someone types and then presses a button — and it is not an answer to
/// report. It is resumed on the calling thread instead, which is inside the request
/// dispatcher's own catch: that is where every other request's cancellation lands,
/// and where it becomes a retry against the new revision or a `ContentModified`, as
/// the handler chose, rather than being reported as a transpiler panic.
///
/// Every variant goes back, `PropagatedPanic` included, because that is what the
/// dispatcher does with them: a query this one waited on having unwound is as often
/// that query's own cancellation as a panic, and a real panic there is met again, as
/// a panic, by whichever asks next — the retry, or the client asking again.
pub(super) fn transpile_detached<R: Send>(op: impl FnOnce() -> R + Send) -> Result<R, ()> {
    let caught =
        std::thread::scope(|scope| scope.spawn(|| catch_unwind(AssertUnwindSafe(op))).join());
    match caught {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(cancelled))
            if cancelled
                .payload
                .downcast_ref::<salsa::Cancelled>()
                .is_some() =>
        {
            cancelled.resume_unwind()
        }
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

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    use super::transpile_detached;

    /// A panic in the transpiler is still a panic: the caller is told there is nothing to return,
    /// and says so.
    #[test]
    fn a_panic_is_reported_as_nothing_to_return() {
        let answer = transpile_detached(|| -> u32 { panic!("the transpiler is broken") });
        assert_eq!(answer, Err(()));
    }

    /// A cancellation goes back up the calling thread with its payload intact, where the request
    /// dispatcher reads it as one. `resume_unwind` of the payload is how salsa itself throws it.
    #[test]
    fn a_cancellation_goes_back_to_the_caller() {
        for cancelled in [
            salsa::Cancelled::PendingWrite,
            salsa::Cancelled::Local,
            salsa::Cancelled::PropagatedPanic,
        ] {
            let unwound = catch_unwind(AssertUnwindSafe(|| {
                transpile_detached(|| -> u32 { resume_unwind(Box::new(cancelled)) })
            }));
            let payload = unwound.expect_err("a cancellation was answered as a result");
            assert!(
                payload.downcast_ref::<salsa::Cancelled>().is_some(),
                "the unwind that came back is not salsa's cancellation"
            );
        }
    }
}
