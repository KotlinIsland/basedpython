//! runtime test for how an extension member receives its receiver
//!
//! an extension member lowers to a module-level backing function, and every call of it is
//! rewritten to call that function with the receiver already chosen: the instance for a method
//! or a property, the class for a class method or a class-level property, nothing for a static
//! method. a member made a class method, a static method or a property by a decorator — written
//! `@classmethod`, `@staticmethod`, `@property`, as well as through the `class def`, `static def`
//! and accessor-block spellings — must lower to a plain function for those calls to reach it:
//! left on the backing function, `@classmethod` makes it a `classmethod` object, which python
//! cannot call
//!
//! no third-party packages are needed, so any `python3` will do; if none is found the test skips
//! rather than fails

use std::process::Command;

use by_transforms::{Config, PythonVersion, transpile};

mod common;
use common::python;

const PROGRAM: &str = r##"
class Widget: ...
class Sub(Widget): ...

extension Widget:
    class def made(cls):
        return cls.__name__

    @classmethod
    def decorated_made(cls):
        return cls.__name__

    static def built():
        return "built"

    @staticmethod
    def decorated_built():
        return "decorated built"

    @property
    def label(self):
        return type(self).__name__

    def named(self):
        return type(self).__name__

# a class method receives the class it is reached through, from the class or an instance
assert Sub.made() == "Sub", "`class def` through the class"
assert Sub().made() == "Sub", "`class def` through an instance"
assert Sub.decorated_made() == "Sub", "`@classmethod` through the class"
assert Sub().decorated_made() == "Sub", "`@classmethod` through an instance"

# a static method receives nothing
assert Widget.built() == "built", "`static def` through the class"
assert Sub().built() == "built", "`static def` through an instance"
assert Widget.decorated_built() == "decorated built", "`@staticmethod` through the class"
assert Sub().decorated_built() == "decorated built", "`@staticmethod` through an instance"

# a property and a method receive the instance
assert Sub().label == "Sub", "`@property` reads off the instance"
assert Sub().named() == "Sub", "a method is called on the instance"

print("ok")
"##;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "a skipped test must say why it skipped, or it reads as a pass"
)]
fn extension_members_receive_their_receiver() {
    let Some(python) = python() else {
        eprintln!("skipping extension receiver runtime test: no `python3` interpreter found");
        return;
    };

    let config = Config {
        min_version: PythonVersion::PY313,
        ..Config::default()
    };
    let transpiled = transpile(PROGRAM, &config).expect("transpile should succeed");

    let output = Command::new(&python)
        .arg("-c")
        .arg(&transpiled)
        .output()
        .expect("failed to spawn python");

    assert!(
        output.status.success(),
        "transpiled extension program failed on {python}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- transpiled ---\n{transpiled}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
