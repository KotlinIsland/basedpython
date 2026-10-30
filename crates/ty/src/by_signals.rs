//! `by run`'s program is asked to end when `by` is.
//!
//! `by run` stages the program and then waits on the interpreter it starts, so whatever stops a
//! run — an editor's Stop, `docker stop`, a supervisor, `kill` — reaches `by`, and only `by`:
//! nothing else names the interpreter. Left to the defaults, `SIGTERM` ended `by` and left the
//! program running with no parent and no notice, and as the first process of a container `by`
//! ignored it outright, since the kernel gives that process no default action for it, so the
//! container ran until it was killed.
//!
//! So on unix `by` handles the signals that ask a process to end — `SIGTERM`, `SIGINT`, `SIGHUP`
//! and `SIGQUIT` — passes each on to the program, and exits as the program does. A program that
//! handles one, to save its work or to tidy up, gets to; one that does not is ended by it, and
//! `by` exits with 128 and the signal's number, as a shell reports it.
//!
//! `SIGTERM` is always passed on. The other three are also what a terminal sends: `SIGINT` for
//! Ctrl+C, `SIGQUIT` for Ctrl+\ and `SIGHUP` for a hangup, each to every process of its foreground
//! process group — and the program is in `by`'s. While `by` is in that group, then, the program has
//! had any of the three that the terminal sent, and sending it again would interrupt whatever the
//! program does about the first, so they are not passed on. Which process sent a signal does not
//! settle it: macOS names the process that wrote the Ctrl+C to the terminal as its sender, just as
//! it names the sender of a `kill`. Outside a terminal's foreground — under an editor that runs
//! `by` on pipes, as the first process of a container, in the background of a shell — nothing has
//! sent the program anything, and all four are passed on.
//!
//! So a sender that means to end a run sends `by` a `SIGTERM`, and `by` alone: one sent to the
//! whole process group, or to every process of the tree, reaches the program twice.
//!
//! Before the program has started there is nothing to pass a signal on to, and `by` exits with
//! 128 and its number, as it would have ended without a handler — and does so as the first
//! process of a container too. After the program has ended, `by` is reporting on it, and
//! finishes doing so.
//!
//! On windows, Ctrl+C and Ctrl+Break are sent to every process attached to the console, the
//! program among them, so there is nothing to pass on: `by` lets the program answer them and
//! exits as it does, rather than ending first and leaving it running.

pub(crate) use platform::Forwarding;

#[cfg(unix)]
mod platform {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::process::{Child, Command, ExitStatus};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
    use std::thread::JoinHandle;

    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::{Handle, Signals};

    /// the signals that ask a process to end
    const TERMINATION: [libc::c_int; 4] = [SIGTERM, SIGINT, SIGHUP, SIGQUIT];

    /// where the program is, which decides what a signal does
    #[derive(Clone, Copy)]
    enum Program {
        /// not started: a signal ends `by`
        NotStarted,
        /// running as this process: a signal is passed on to it
        Running(libc::pid_t),
        /// ended, but not yet reaped, so its process id is still its own: nothing is passed on
        Ended,
    }

    /// the signals `by run` handles, from before the program is staged until it has ended
    pub(crate) struct Forwarding {
        program: Arc<Mutex<Program>>,
        handle: Handle,
        thread: Option<JoinHandle<()>>,
    }

    impl Forwarding {
        pub(crate) fn install() -> io::Result<Self> {
            let mut signals = Signals::new(TERMINATION)?;
            let handle = signals.handle();
            let program = Arc::new(Mutex::new(Program::NotStarted));
            let state = Arc::clone(&program);
            let thread = std::thread::Builder::new()
                .name("by run signals".to_owned())
                .spawn(move || {
                    for signal in signals.forever() {
                        // held while the signal is sent, so that the program cannot be reaped,
                        // and its process id handed to another process, in between
                        let program = lock(&state);
                        match *program {
                            Program::NotStarted => std::process::exit(128 + signal),
                            Program::Running(pid)
                                if signal == SIGTERM || !in_terminal_foreground() =>
                            {
                                // SAFETY: `kill` reads nothing of ours. `pid` is the program's,
                                // which `run` has not reaped while it reads `Running`
                                #[expect(unsafe_code)]
                                let _ = unsafe { libc::kill(pid, signal) };
                            }
                            Program::Running(_) | Program::Ended => {}
                        }
                    }
                })?;
            Ok(Self {
                program,
                handle,
                thread: Some(thread),
            })
        }

        /// Start the program `command` describes, and wait for it to end.
        pub(crate) fn run(&self, command: &mut Command) -> io::Result<ExitStatus> {
            let mut child = {
                // a signal that arrives while the program starts waits for it to have started,
                // and is then passed on to it rather than ending `by` without it
                let mut program = lock(&self.program);
                let child = command.spawn()?;
                *program = Program::Running(pid_of(&child)?);
                child
            };
            wait_without_reaping(&child)?;
            *lock(&self.program) = Program::Ended;
            child.wait()
        }
    }

    impl Drop for Forwarding {
        fn drop(&mut self) {
            self.handle.close();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn lock(program: &Mutex<Program>) -> MutexGuard<'_, Program> {
        // a value that is replaced whole is never left half-written by a panic
        program.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pid_of(child: &Child) -> io::Result<libc::pid_t> {
        libc::pid_t::try_from(child.id()).map_err(io::Error::other)
    }

    /// Whether `by` is in the foreground process group of its controlling terminal, the group
    /// the terminal sends Ctrl+C's `SIGINT`, Ctrl+\'s `SIGQUIT` and a hangup's `SIGHUP` to.
    ///
    /// A process with no controlling terminal is in nobody's foreground, and opening `/dev/tty`
    /// is how it finds out it has none.
    fn in_terminal_foreground() -> bool {
        let Ok(terminal) = File::options()
            .read(true)
            .custom_flags(libc::O_NOCTTY)
            .open("/dev/tty")
        else {
            return false;
        };
        // SAFETY: `tcgetpgrp` reads the terminal behind a descriptor that `terminal` keeps open
        // for the call, and `getpgrp` reads nothing of ours
        #[expect(unsafe_code)]
        unsafe {
            libc::tcgetpgrp(terminal.as_raw_fd()) == libc::getpgrp()
        }
    }

    /// Wait for `child` to end and leave it unreaped, so that its process id stays its own until
    /// nothing can send a signal to it any more.
    fn wait_without_reaping(child: &Child) -> io::Result<()> {
        let pid = libc::id_t::from(child.id());
        loop {
            // SAFETY: `info` is a `siginfo_t` of our own that `waitid` writes into, and nothing
            // else reads it
            #[expect(unsafe_code)]
            let waited = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if waited == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::io;
    use std::process::{Command, ExitStatus};

    /// Ctrl+C and Ctrl+Break left to the program
    pub(crate) struct Forwarding;

    impl Forwarding {
        pub(crate) fn install() -> io::Result<Self> {
            // the program is attached to the console too, and answers the event itself
            ctrlc::set_handler(|| {}).map_err(io::Error::other)?;
            Ok(Self)
        }

        pub(crate) fn run(&self, command: &mut Command) -> io::Result<ExitStatus> {
            command.status()
        }
    }
}
