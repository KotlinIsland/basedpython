use std::num::NonZeroUsize;

use crate::session::Session;

mod task;
mod thread;

use ruff_db::STACK_SIZE;

use self::{
    task::{BackgroundTaskBuilder, SyncTask},
    thread::ThreadPriority,
};
use crate::session::client::Client;
pub(super) use task::{BackgroundSchedule, Task};

/// The event loop thread is actually a secondary thread that we spawn from the
/// _actual_ main thread. This secondary thread has a larger stack size
/// than some OS defaults (Windows, for example) and is also designated as
/// high-priority.
pub(crate) fn spawn_main_loop(
    func: impl FnOnce() -> crate::Result<()> + Send + 'static,
) -> crate::Result<thread::JoinHandle<crate::Result<()>>> {
    const MAIN_THREAD_NAME: &str = "ty:main";
    Ok(
        thread::Builder::new(thread::ThreadPriority::LatencySensitive)
            .name(MAIN_THREAD_NAME.into())
            .stack_size(STACK_SIZE)
            .spawn(func)?,
    )
}

pub(crate) struct Scheduler {
    fmt_pool: thread::Pool,
    background_pool: thread::Pool,
}

impl Scheduler {
    pub(super) fn new(worker_threads: NonZeroUsize) -> Self {
        const FMT_THREADS: usize = 1;
        Self {
            fmt_pool: thread::Pool::new(NonZeroUsize::try_from(FMT_THREADS).unwrap()),
            background_pool: thread::Pool::new(worker_threads),
        }
    }

    /// Dispatches a `task` by either running it as a blocking function or
    /// executing it on a background thread pool.
    pub(super) fn dispatch(&mut self, task: task::Task, session: &mut Session, client: Client) {
        match task {
            Task::Sync(SyncTask { func }) => {
                func(session, &client);
            }
            Task::Background(BackgroundTaskBuilder {
                schedule,
                builder: func,
            }) => {
                let static_func = func(session);
                let task = move || static_func(&client);
                match schedule {
                    BackgroundSchedule::Worker => {
                        self.background_pool.spawn(ThreadPriority::Worker, task);
                    }
                    BackgroundSchedule::LatencySensitive => self
                        .background_pool
                        .spawn(ThreadPriority::LatencySensitive, task),
                    BackgroundSchedule::Fmt => {
                        self.fmt_pool.spawn(ThreadPriority::LatencySensitive, task);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::thread::{Pool, ThreadPriority};
    use crate::server::{Action, Event, main_loop_channel};

    /// The main loop hands work to the pool, and it takes what comes back off its own queue —
    /// and it has to get through the handing out before it reaches the taking back. So a worker
    /// that cannot put its result down stops there: it never returns for the next job, the pool's
    /// queue fills up behind it, and the main loop, waiting for room in that queue, never reaches
    /// the take that would have let the worker go. Nothing in the server moves after that.
    ///
    /// Handing out far more work than the pool can hold, before reading anything back, is what a
    /// burst of editor requests does to it — with a `by check` asking for the project's
    /// diagnostics in among them.
    #[test]
    fn a_worker_that_has_finished_never_waits_on_the_main_loop() {
        const JOBS: usize = 512;
        const WORKERS: NonZeroUsize = NonZeroUsize::new(2).unwrap();
        const PATIENCE: Duration = Duration::from_secs(30);

        let (sender, receiver) = main_loop_channel();
        let (dispatched, all_dispatched) = crossbeam::channel::bounded(1);

        let dispatching = std::thread::spawn(move || {
            let pool = Pool::new(WORKERS);
            for _ in 0..JOBS {
                let sender = sender.clone();
                pool.spawn(ThreadPriority::Worker, move || {
                    // ignored rather than unwrapped: a regression leaves this test's own
                    // receiver dropped, and a panic in a pool job aborts the process, which
                    // would take the failure message with it
                    sender.send(Event::Action(Action::RescanProjects)).ok();
                });
            }
            dispatched.send(()).ok();
        });

        all_dispatched
            .recv_timeout(PATIENCE)
            .expect("every job to have been handed to the pool without reading any event back");

        for _ in 0..JOBS {
            receiver
                .recv_timeout(PATIENCE)
                .expect("every job's event to have reached the queue");
        }
        dispatching.join().expect("the dispatching thread to end");
    }
}
