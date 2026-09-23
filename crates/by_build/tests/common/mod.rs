//! what both test binaries share: the directory a test builds into, and the one place a
//! python child's stdout is read
//!
//! both test binaries compare what an interpreter printed, so how those bytes
//! travel has to be settled once. a pipe that picks its own encoding, or a
//! platform that rewrites the line endings on the way out, is not something
//! either build decided — and a comparison that cannot tell the two apart is
//! reading the platform rather than the compiler

use std::fmt::Display;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;

/// a directory a test builds into, which goes when the test passes
///
/// a failing test keeps it, so the C, the transpiled twin and whatever else the build
/// wrote can still be read, and names it on stderr, which both test runners print with
/// the failure. a test holds the directory for as long as it reads from it, so a helper
/// that builds one and hands back an answer hands the directory back too: otherwise it
/// would be gone before the assertion that fails on the answer
///
/// the name carries the process id. nextest gives each *test* a process of its own, so
/// one run never collides with itself — but nothing stops two *runs* choosing the same
/// path, and a 3.13 sweep beside a 3.14 one would then overwrite each other's sources
/// between the build and the read. a collision during setup fails fast enough to look
/// like a missing toolchain, and one after the build fails having genuinely compiled and
/// compared, so it reads as a difference the compiler produced — twenty-nine of those were
/// chased as a regression before the shared path was noticed
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    /// an empty place for `name` under the system temp directory
    pub(crate) fn new(name: impl Display) -> Self {
        let path = std::env::temp_dir().join(format!("{name}_p{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
}

impl Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            #[expect(
                clippy::print_stderr,
                reason = "the failure output is where a kept directory has to be named"
            )]
            {
                eprintln!("kept the failing test's directory: {}", self.0.display());
            }
        } else {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// what [`python_output`] names the program it runs, in the directory it runs it against
///
/// both legs of a comparison run a program of this name, so a frame or a warning that
/// names the program names it the same way in each
const SNIPPET: &str = "by_snippet.py";

/// run `body` with `dir` on `sys.path` and hand back what it printed
///
/// the program is written to [`SNIPPET`] in `dir` and run from there rather than passed
/// with `-c`: windows caps a command line at 32767 characters, and the helpers a
/// differential snippet carries are past that on their own. a failing test keeps `dir`,
/// so the program that failed is there to run again
pub(crate) fn python_output(python: &str, dir: &Path, body: &str) -> String {
    let prelude = format!(
        "import sys\nsys.path.insert(0, {:?})\n",
        dir.display().to_string()
    );
    std::fs::create_dir_all(dir).expect("the snippet's directory is made");
    let program = dir.join(SNIPPET);
    std::fs::write(&program, prelude + body).expect("the snippet is written");
    let output = Command::new(python)
        // a redirected stdout otherwise takes the platform's own code page,
        // which on windows is `cp1252` and cannot spell an astral character at
        // all: the snippet dies in `charmap_encode` rather than saying what the
        // two builds answered. both children are the same interpreter, so
        // pinning the pipe settles how a character travels and nothing about
        // what either build computed — `repr` escapes anything utf-8 could not
        // carry before it is ever written
        .env("PYTHONIOENCODING", "utf-8")
        .arg(&program)
        .output()
        .expect("the interpreter runs");
    assert!(
        output.status.success(),
        "the snippet failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = String::from_utf8_lossy(&output.stdout);
    let printed = if cfg!(windows) {
        undo_newline_translation(&printed)
    } else {
        printed.into_owned()
    };
    printed.trim().to_string()
}

/// give back what the child *printed*, undoing the translation its stdout
/// performed on the way out
///
/// a windows text stream writes `os.linesep` for every `\n` it is handed, so a
/// line the program ended with `\n` reaches the pipe as `\r\n`. replacing left
/// to right is that substitution's exact inverse rather than a normalisation:
/// the translation only ever inserts a `\r` in front of an existing `\n`, so a
/// `\r\n` the program printed itself travels as `\r\r\n` and comes back as
/// `\r\n`. two builds that disagree about a line ending still disagree here,
/// which is why it is applied only where the translation happened — on a
/// platform that writes the bytes through, undoing one would be a real loss
fn undo_newline_translation(printed: &str) -> String {
    printed.replace("\r\n", "\n")
}

#[test]
fn undoing_the_translation_gives_back_what_was_printed() {
    // what a windows text stream does to the bytes on the way out
    fn translated(printed: &str) -> String {
        printed.replace('\n', "\r\n")
    }
    for printed in [
        "", "a", "a\nb", "a\nb\n", "\n\n",
        // a carriage return the program printed itself, which has to survive
        "a\r\nb", "a\rb", "a\r\r\nb", "\r", "\r\n\r\n",
    ] {
        assert_eq!(
            undo_newline_translation(&translated(printed)),
            printed,
            "{printed:?} did not come back"
        );
    }
    // so two builds that disagree about a line ending still disagree
    assert_ne!(
        undo_newline_translation(&translated("a\r\nb")),
        undo_newline_translation(&translated("a\nb"))
    );
}
