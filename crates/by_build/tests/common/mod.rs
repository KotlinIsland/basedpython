//! the one place a python child's stdout is read
//!
//! both test binaries compare what an interpreter printed, so how those bytes
//! travel has to be settled once. a pipe that picks its own encoding, or a
//! platform that rewrites the line endings on the way out, is not something
//! either build decided — and a comparison that cannot tell the two apart is
//! reading the platform rather than the compiler

use std::path::Path;
use std::process::Command;

/// run `body` with `dir` on `sys.path` and hand back what it printed
pub(crate) fn python_output(python: &str, dir: &Path, body: &str) -> String {
    let prelude = format!(
        "import sys\nsys.path.insert(0, {:?})\n",
        dir.display().to_string()
    );
    let output = Command::new(python)
        // a redirected stdout otherwise takes the platform's own code page,
        // which on windows is `cp1252` and cannot spell an astral character at
        // all: the snippet dies in `charmap_encode` rather than saying what the
        // two builds answered. both children are the same interpreter, so
        // pinning the pipe settles how a character travels and nothing about
        // what either build computed — `repr` escapes anything utf-8 could not
        // carry before it is ever written
        .env("PYTHONIOENCODING", "utf-8")
        .args(["-c", &(prelude + body)])
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
