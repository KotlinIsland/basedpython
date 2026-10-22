//! Helpers for parallel operations that need an independent Salsa database on each Rayon job.
//!
//! # Cancelling one operation
//!
//! Salsa cancels every computation when the database is written to, and nothing else stops one: a
//! request its client has given up on runs to the end. A [`CancellationScope`] stops the
//! computations of one operation only. The operation [enters](CancellationScope::enter) it on the
//! thread it starts on and [registers](CancellationScope::register) its database there; each Rayon
//! job the helpers here hand its work to registers the database clone it is given, and enters the
//! scope too, so that a parallel iterator started inside a job belongs to it as well.
//! [`CancellationScope::cancel`] cancels each of those databases, and every one registered later,
//! and each unwinds with [`salsa::Cancelled::Local`] at its next query.
//!
//! A computation another thread is waiting on is not lost to that thread: Salsa hands it back,
//! and the waiting thread computes it itself.

use std::cell::RefCell;
use std::sync::Arc;

use parking_lot::Mutex;
use rayon::iter::ParallelIterator;

use crate::{Db, ProjectDatabase};

/// Chooses a minimum Rayon job length without starving the worker pool on smaller inputs.
///
/// The maximum should be an empirically chosen upper bound for the operation's minimum job length.
/// Larger inputs use that minimum, while smaller inputs lower it to retain enough jobs for work
/// stealing.
pub fn minimum_parallel_job_len(item_count: usize, maximum: usize) -> usize {
    const TARGET_JOBS_PER_THREAD: usize = 4;

    let target_jobs = rayon::current_num_threads().saturating_mul(TARGET_JOBS_PER_THREAD);
    item_count.div_ceil(target_jobs).clamp(1, maximum.max(1))
}

/// Extension methods for Rayon parallel iterators that need a Salsa database.
pub trait ParallelIteratorExt: ParallelIterator + Sized {
    /// Maps items in parallel with a separate Salsa database for each Rayon job.
    ///
    /// Rayon's standard scheduling adapters can be applied before this method. For example, an
    /// indexed iterator can use [`rayon::iter::IndexedParallelIterator::with_min_len`] to reduce
    /// task and database-cloning overhead.
    ///
    /// The jobs belong to the [`CancellationScope`] the calling thread is in, if any.
    ///
    /// # Warning
    ///
    /// This method uses [`salsa::attach_allow_change`] and must never be called from within a Salsa
    /// query. It is intended for top-level request and command handlers that are outside the Salsa
    /// query stack.
    fn map_with_db<Output>(
        self,
        db: &dyn Db,
        map: impl Fn(&dyn Db, Self::Item) -> Output + Send + Sync,
    ) -> impl ParallelIterator<Item = Output>
    where
        Output: Send,
    {
        self.map_with(
            ParallelDb::new(Db::dyn_clone(db), CancellationScope::current()),
            move |db, item| {
                let ParallelDb { db, scope } = &*db;
                run_in(scope.as_ref(), &**db, || {
                    salsa::attach_allow_change(&**db, || map(&**db, item))
                })
            },
        )
    }

    /// Runs an operation in parallel with a cloned [`ProjectDatabase`] for each Rayon job.
    ///
    /// Rayon's standard scheduling adapters can be applied before this method. For example, an
    /// indexed iterator can use [`rayon::iter::IndexedParallelIterator::with_min_len`] to reduce
    /// task and database-cloning overhead.
    ///
    /// The jobs belong to the [`CancellationScope`] the calling thread is in, if any.
    ///
    /// # Warning
    ///
    /// This method uses [`salsa::attach_allow_change`] and must never be called from within a Salsa
    /// query. It is intended for top-level request and command handlers that are outside the Salsa
    /// query stack.
    fn for_each_with_project_db(
        self,
        db: &ProjectDatabase,
        op: impl Fn(&ProjectDatabase, Self::Item) + Send + Sync,
    ) {
        self.for_each_with(
            ParallelProjectDb::new(db.clone(), CancellationScope::current()),
            move |db, item| {
                let ParallelProjectDb { db, scope } = &*db;
                run_in(scope.as_ref(), db, || {
                    salsa::attach_allow_change(db, || op(db, item));
                });
            },
        );
    }
}

impl<Iter> ParallelIteratorExt for Iter where Iter: ParallelIterator {}

/// An owned Salsa snapshot that Rayon can clone whenever it splits a parallel iterator, each
/// clone registered with the scope the iterator was started in.
struct ParallelDb {
    db: Box<dyn Db>,
    scope: Option<CancellationScope>,
}

impl ParallelDb {
    fn new(db: Box<dyn Db>, scope: Option<CancellationScope>) -> Self {
        if let Some(scope) = &scope {
            scope.register(&*db);
        }
        Self { db, scope }
    }
}

impl Clone for ParallelDb {
    fn clone(&self) -> Self {
        Self::new(Db::dyn_clone(&*self.db), self.scope.clone())
    }
}

/// [`ParallelDb`] for a [`ProjectDatabase`].
struct ParallelProjectDb {
    db: ProjectDatabase,
    scope: Option<CancellationScope>,
}

impl ParallelProjectDb {
    fn new(db: ProjectDatabase, scope: Option<CancellationScope>) -> Self {
        if let Some(scope) = &scope {
            scope.register(&db);
        }
        Self { db, scope }
    }
}

impl Clone for ParallelProjectDb {
    fn clone(&self) -> Self {
        Self::new(self.db.clone(), self.scope.clone())
    }
}

/// Runs `op`, one item of a Rayon job on `db`, inside `scope`.
///
/// Salsa clears a database's cancellation when the database is detached from the thread, which
/// each item does as it ends, so a cancellation that lands between two items is applied again
/// before the next one starts.
fn run_in<R>(
    scope: Option<&CancellationScope>,
    db: &dyn salsa::Database,
    op: impl FnOnce() -> R,
) -> R {
    match scope {
        Some(scope) => scope.enter(|| {
            if scope.is_cancelled() {
                db.cancellation_token().cancel();
            }
            op()
        }),
        None => op(),
    }
}

/// The Salsa computations of one operation, wherever they run, to be cancelled together. See the
/// [module documentation](self).
#[derive(Clone, Default)]
pub struct CancellationScope(Arc<Mutex<ScopeState>>);

#[derive(Default)]
struct ScopeState {
    cancelled: bool,
    /// The cancellation of each database handle the operation computes on. Cancelling one that
    /// has since been dropped does nothing.
    databases: Vec<salsa::CancellationToken>,
}

thread_local! {
    static CURRENT_SCOPE: RefCell<Option<CancellationScope>> = const { RefCell::new(None) };
}

impl CancellationScope {
    #[cfg(test)]
    fn new() -> Self {
        Self::default()
    }

    /// Cancels every database registered, and each one registered from now on.
    pub fn cancel(&self) {
        let mut state = self.0.lock();
        state.cancelled = true;
        for database in state.databases.drain(..) {
            database.cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().cancelled
    }

    /// Makes `db`'s computations part of this scope.
    pub fn register(&self, db: &dyn salsa::Database) {
        let token = db.cancellation_token();
        let mut state = self.0.lock();
        if state.cancelled {
            token.cancel();
        } else {
            state.databases.push(token);
        }
    }

    /// Runs `op` with this as the thread's scope, so that the parallel iterators `op` starts
    /// through [`ParallelIteratorExt`] belong to it.
    pub fn enter<R>(&self, op: impl FnOnce() -> R) -> R {
        struct Restore(Option<CancellationScope>);

        impl Drop for Restore {
            fn drop(&mut self) {
                let previous = self.0.take();
                CURRENT_SCOPE.with(|current| *current.borrow_mut() = previous);
            }
        }

        let previous = CURRENT_SCOPE.with(|current| current.replace(Some(self.clone())));
        let _restore = Restore(previous);
        op()
    }

    /// The scope the calling thread is in.
    fn current() -> Option<Self> {
        CURRENT_SCOPE.with(|current| current.borrow().clone())
    }
}

// a panic cannot leave the state half-changed: each lock sets a flag, or pushes or drains the
// list, and none of them panics while holding it
impl std::panic::UnwindSafe for CancellationScope {}
impl std::panic::RefUnwindSafe for CancellationScope {}

impl std::fmt::Debug for CancellationScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationScope")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;

    use rayon::prelude::*;
    use ruff_db::files::{File, system_path_to_file};
    use ruff_db::source::source_text;
    use ruff_db::system::{DbWithWritableSystem as _, SystemPathBuf};

    use super::{CancellationScope, ParallelIteratorExt};
    use crate::ProjectMetadata;
    use crate::db::testing::TestDb;

    fn project(files: usize) -> (TestDb, Vec<File>) {
        let root = SystemPathBuf::from("/project");
        let mut db = TestDb::new(ProjectMetadata::new("test", root.clone()));
        let paths: Vec<_> = (0..files)
            .map(|index| root.join(format!("module{index}.py")))
            .collect();
        db.write_files(paths.iter().map(|path| (path.clone(), "x: int = 1\n")))
            .unwrap();
        let files = paths
            .iter()
            .map(|path| system_path_to_file(&db, path).unwrap())
            .collect();
        (db, files)
    }

    fn total_length(db: &TestDb, files: &[File]) -> usize {
        files
            .to_vec()
            .into_par_iter()
            .map_with_db(db, |db, file| source_text(db, file).len())
            .sum()
    }

    /// The jobs a parallel iterator started inside a cancelled scope unwind with a local
    /// cancellation instead of computing, and the same work outside it is done.
    #[test]
    fn a_cancelled_scope_stops_its_parallel_jobs() {
        let (db, files) = project(16);
        let scope = CancellationScope::new();
        scope.cancel();

        let cancelled = salsa::Cancelled::catch(AssertUnwindSafe(|| {
            scope.enter(|| total_length(&db, &files))
        }));
        assert!(
            matches!(cancelled, Err(salsa::Cancelled::Local)),
            "{cancelled:?}"
        );

        assert_eq!(total_length(&db, &files), 16 * "x: int = 1\n".len());
    }

    /// A scope that is never cancelled changes nothing.
    #[test]
    fn a_scope_left_alone_computes() {
        let (db, files) = project(16);
        let scope = CancellationScope::new();
        assert_eq!(
            scope.enter(|| total_length(&db, &files)),
            16 * "x: int = 1\n".len()
        );
    }
}
