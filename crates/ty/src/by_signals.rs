//! `by run`'s program, and `by repl`'s interpreter, are asked to end when `by` is
//!
//! `by run` stages the program and then waits on the interpreter it starts, and `by repl` serves
//! the interpreter its session runs in, so whatever stops either — an editor's stop, `docker stop`,
//! a supervisor, `kill` — reaches `by`, and only `by`: nothing else names the interpreter. left to
//! the defaults, `SIGTERM` ended `by` and left the program running with no parent and no notice,
//! and as the first process of a container `by` ignored it outright, since the kernel gives that
//! process no default action for it, so the container ran until it was killed
//!
//! so on unix `by` handles the signals that ask a process to end — `SIGTERM`, `SIGINT`, `SIGHUP`
//! and `SIGQUIT` — passes each on to the program, and exits as the program does. a program that
//! handles one, to save its work or to tidy up, gets to; one that does not is ended by it, and
//! `by` exits with 128 and the signal's number, as a shell reports it
//!
//! a signal `by` was started ignoring stays ignored, by `by` and so by the program, which inherits
//! the ignoring: `nohup` promises a program no `SIGHUP`, and a shell's `&` no `SIGINT` or `SIGQUIT`
//!
//! `SIGTERM` is always passed on. the other three are also what a terminal sends: `SIGINT` for
//! ctrl-c, `SIGQUIT` for ctrl-\ and `SIGHUP` for a hangup, each to every process of its foreground
//! process group — and the program is in `by`'s. while `by` is in that group, then, the program has
//! had any of the three that the terminal sent, and sending it again would interrupt whatever the
//! program does about the first, so they are not passed on. which process sent a signal does not
//! settle it: macos names the process that wrote the ctrl-c to the terminal as its sender, just as
//! it names the sender of a `kill`. outside a terminal's foreground — under an editor that runs
//! `by` on pipes, as the first process of a container, in the background of a shell — nothing has
//! sent the program anything, and all four are passed on
//!
//! a hangup is the one exception, because it goes to the first process of the terminal's session
//! rather than to the foreground. when that is a shell, the group hears of it as the shell exits,
//! so the program has had it. when that is `by`, which is how `ssh -t host by run main` can start
//! it, only `by` has, so it is passed on
//!
//! so a sender that means to end a run sends `by` a `SIGTERM`, and `by` alone: one sent to the
//! whole process group, or to every process of the tree, reaches the program twice
//!
//! before the program has started there is nothing to pass a signal on to, and `by` exits with
//! 128 and its number, as it would have ended without a handler — and does so as the first
//! process of a container too. after the program has ended, `by` is reporting on it, and
//! finishes doing so
//!
//! on windows, ctrl-c and ctrl-break are sent to every process attached to the console, the
//! program among them, so there is nothing to pass on: `by` lets the program answer them and
//! exits as it does, rather than ending first and leaving it running. before the program has
//! started `by` exits, as it does on unix

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
            let mut signals = Signals::new(not_ignored())?;
            let handle = signals.handle();
            let program = Arc::new(Mutex::new(Program::NotStarted));
            let state = Arc::clone(&program);
            let terminal = controlling_terminal();
            let thread = std::thread::Builder::new()
                .name("by run signals".to_owned())
                .spawn(move || {
                    for signal in signals.forever() {
                        // held while the signal is sent, so that the program cannot be reaped,
                        // and its process id handed to another process, in between. nothing here
                        // may block, or `wait` never finishes
                        let program = lock(&state);
                        match *program {
                            Program::NotStarted => std::process::exit(128 + signal),
                            Program::Running(pid)
                                if !reached_the_program_without_us(signal, terminal.as_ref()) =>
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

        /// start the program `command` describes, and wait for it to end
        pub(crate) fn run(&self, command: &mut Command) -> io::Result<ExitStatus> {
            let mut child = self.start(command)?;
            self.wait(&mut child)
        }

        /// start the program `command` describes, which the signals are passed on to from now
        /// until [`Self::wait`] has seen it end
        ///
        /// only [`Self::wait`] may reap it: a program reaped behind its back has a process id
        /// the system can hand to another process, which a signal would then be passed on to.
        /// [`Self::has_ended`] asks without reaping, and `Child::kill` is safe while it is
        /// unreaped
        pub(crate) fn start(&self, command: &mut Command) -> io::Result<Child> {
            // a signal that arrives while the program starts waits for it to have started,
            // and is then passed on to it rather than ending `by` without it
            let mut program = lock(&self.program);
            let child = command.spawn()?;
            *program = Program::Running(pid_of(&child)?);
            Ok(child)
        }

        /// whether the program [`Self::start`] started has ended, leaving it unreaped
        #[expect(
            clippy::unused_self,
            reason = "asked of the forwarding that started the program, as every question about \
                      it is, so that nothing else can reap it"
        )]
        pub(crate) fn has_ended(&self, child: &mut Child) -> io::Result<bool> {
            ended_without_reaping(child, false)
        }

        /// wait for the program [`Self::start`] started to end, and reap it
        pub(crate) fn wait(&self, child: &mut Child) -> io::Result<ExitStatus> {
            ended_without_reaping(child, true)?;
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

    /// the signals of [`TERMINATION`] that `by` was not started ignoring. handling one would end
    /// the ignoring, for `by` and for the program it starts, which inherits it only as long as it
    /// lasts
    fn not_ignored() -> Vec<libc::c_int> {
        TERMINATION
            .into_iter()
            .filter(|&signal| {
                // SAFETY: a null new action only reads the current one, into a `sigaction` of
                // our own that nothing else reads
                #[expect(unsafe_code)]
                let handler = unsafe {
                    let mut current: libc::sigaction = std::mem::zeroed();
                    let read = libc::sigaction(signal, std::ptr::null(), &raw mut current);
                    (read == 0).then_some(current.sa_sigaction)
                };
                handler != Some(libc::SIG_IGN)
            })
            .collect()
    }

    /// `by`'s controlling terminal, when it has one, which opening `/dev/tty` is how a process
    /// finds out. opened now rather than when a signal arrives, and not to wait on: once the
    /// terminal has hung up, an open that may wait never returns
    fn controlling_terminal() -> Option<File> {
        File::options()
            .read(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open("/dev/tty")
            .ok()
    }

    /// whether the program, being in `by`'s process group, has had `signal` from the terminal
    /// already; see the module's documentation
    fn reached_the_program_without_us(signal: libc::c_int, terminal: Option<&File>) -> bool {
        if signal == SIGTERM {
            return false;
        }
        let Some(terminal) = terminal else {
            return false;
        };
        // SAFETY: each reads something of the process's own, and `tcgetpgrp` a terminal behind a
        // descriptor that `terminal` keeps open for the call
        #[expect(unsafe_code)]
        let (foreground, group, first_of_session) = unsafe {
            (
                libc::tcgetpgrp(terminal.as_raw_fd()),
                libc::getpgrp(),
                libc::getsid(0) == libc::getpid(),
            )
        };
        if signal == SIGHUP && first_of_session {
            return false;
        }
        // a terminal that has gone has no foreground to ask about. the only signal of the three
        // it leaves a process is the one its going sent to the group
        foreground == group || (signal == SIGHUP && foreground <= 0)
    }

    /// whether `child` has ended, waiting for it to when `block`, and leaving it unreaped, so
    /// that its process id stays its own until nothing can send a signal to it any more
    fn ended_without_reaping(child: &Child, block: bool) -> io::Result<bool> {
        let pid = libc::id_t::from(child.id());
        let options = libc::WEXITED | libc::WNOWAIT | if block { 0 } else { libc::WNOHANG };
        loop {
            // SAFETY: `info` is a `siginfo_t` of our own that `waitid` writes into, and nothing
            // else reads it
            #[expect(unsafe_code)]
            let (waited, ended) = unsafe {
                let mut info: libc::siginfo_t = std::mem::zeroed();
                let waited = libc::waitid(libc::P_PID, pid, &raw mut info, options);
                // with `WNOHANG`, a process still running leaves `info` zeroed: no pid in it
                (waited, info.si_pid() != 0)
            };
            if waited == 0 {
                return Ok(block || ended);
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
    use std::process::{Child, Command, ExitStatus};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// the exit status of a program ended by ctrl-c, as a shell reports the `SIGINT` it is on unix
    const INTERRUPTED: i32 = 128 + 2;

    /// ctrl-c and ctrl-break left to the program, once there is one
    pub(crate) struct Forwarding {
        started: Arc<AtomicBool>,
    }

    impl Forwarding {
        pub(crate) fn install() -> io::Result<Self> {
            let started = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&started);
            ctrlc::set_handler(move || {
                // the program is attached to the console too, and answers the event itself
                if !flag.load(Ordering::SeqCst) {
                    std::process::exit(INTERRUPTED);
                }
            })
            .map_err(io::Error::other)?;
            Ok(Self { started })
        }

        pub(crate) fn run(&self, command: &mut Command) -> io::Result<ExitStatus> {
            let mut child = self.start(command)?;
            self.wait(&mut child)
        }

        pub(crate) fn start(&self, command: &mut Command) -> io::Result<Child> {
            // set first: an event that arrives while the program starts must not end `by`
            // without it
            self.started.store(true, Ordering::SeqCst);
            command.spawn()
        }

        #[expect(
            clippy::unused_self,
            reason = "the same methods as on unix, where they read what the program is"
        )]
        pub(crate) fn has_ended(&self, child: &mut Child) -> io::Result<bool> {
            Ok(child.try_wait()?.is_some())
        }

        #[expect(
            clippy::unused_self,
            reason = "the same methods as on unix, where they read what the program is"
        )]
        pub(crate) fn wait(&self, child: &mut Child) -> io::Result<ExitStatus> {
            child.wait()
        }
    }
}
