//! the python forwarders a compiled module publishes under its function names
//!
//! a `PyCFunction` is not a descriptor. python's own `function` is, which is what
//! makes `Cls.method = mod.fn` install something that receives the receiver — and
//! it is exactly what `functools.total_ordering` does to every class it decorates.
//! a module publishing the native object under its own name therefore hands out a
//! callable that silently drops slot zero:
//!
//! ```python
//! # lib.py
//! def label(self=None):
//!     return "bound" if self is not None else "UNBOUND"
//! ```
//!
//! `Thing.label = lib.label; Thing().label()` is `'bound'` in python and was
//! `'UNBOUND'` compiled — no exception and no decline, which is the worst answer
//! this compiler can give.
//!
//! cpython offers no supported way to make a `PyCFunction` bind: it has no
//! `tp_descr_get`, `staticmethod` of one does not bind either, and the one C
//! callable that does bind — `PyDescr_NewMethod` — refuses to be called without a
//! receiver, so it cannot stand where a module-level function stands. a type of
//! our own would bind but be a third thing no ecosystem dispatch table knows.
//!
//! so the module publishes a real `function` that forwards to the native one. it
//! is written as python source appended to the interpreted twin, because that
//! source is already compiled by the interpreter this artefact is built for — the
//! forwarder needs a code object and this is the only place in the build that can
//! make one.
//!
//! # the forwarder takes `*args, **kwargs`, and that is deliberate
//!
//! writing out each parameter by name would have made `inspect.signature` read
//! straight off the forwarder, but it also means the forwarder *binds* the call —
//! and a forwarder that fills in a default is filling in the wrong one. the
//! transpiler rewrites `def f(b=[])` into a `_MISSING` sentinel plus a test in the
//! body, so the twin's default is a sentinel the native has never heard of, and
//! passing it on made `f()` answer with the sentinel object.
//!
//! so the forwarder passes on exactly what it was given and nothing else. every
//! decision about arity, defaults, keyword-only parameters and which calls are
//! handed back to the interpreted definition stays where it already was, in the
//! native boundary — the forwarder cannot get any of them wrong because it does
//! not make any of them.
//!
//! what that costs is the signature, and it is paid back the way python itself
//! pays it: `__wrapped__` points at the interpreted definition, so
//! `inspect.signature`, `inspect.getsource` and `inspect.getfile` all answer for
//! the definition as written. everything else a function is asked about — its
//! name, docstring, module, defaults, annotations, `__dict__` — is copied off that
//! same definition.

use std::fmt::Write;

use by_ir::function::{Function, ModuleIr, ShimInstall};

/// the base name of the installer the artefact calls once, at module init
const INSTALLER: &str = "_by_install_natives";

/// what a forwarder's frame says it came from
///
/// a traceback through one names the function and this file. the alternative — the
/// module's own file and the `def`'s line — would print the definition's source
/// against a frame that is not running it, and it would leave nothing anywhere that
/// says which build answered. the harness needs that distinction: a function that
/// fell back to its interpreted definition answers identically, and `type(f)` used
/// to be what told them apart
pub const SHIM_FILE: &str = "<by native forwarder>";

/// what a module's forwarders need: the source to append to the twin, and the
/// handle the artefact installs them through
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shims {
    /// python source defining the installer, to append to the interpreted twin
    pub source: String,
    /// the name the installer is bound to, and the functions it publishes
    pub install: ShimInstall,
}

/// the functions a module publishes forwarders for: its exported module-level
/// definitions, in table order
fn published(module: &ModuleIr) -> Vec<&Function> {
    module
        .functions
        .iter()
        .filter(|function| function.exported)
        .collect()
}

/// a name nothing in `source` already uses
///
/// the installer is bound in the module's own namespace for as long as it takes to
/// call it, so a module that happens to define the same name would have its own
/// definition overwritten. extending the name until it does not occur is cheap and
/// leaves nothing to a guess about what a module is unlikely to be called
fn free_name(base: &str, source: &str) -> String {
    let mut name = base.to_string();
    while source.contains(&name) {
        name.push('_');
    }
    name
}

/// the installer source for `module`, or `None` where it exports no function
pub fn shims(module: &ModuleIr, twin: &str) -> Option<Shims> {
    let published = published(module);
    if published.is_empty() {
        return None;
    }
    let installer = free_name(INSTALLER, twin);
    // every name the installer binds — its parameters, its locals, and the
    // forwarders themselves — carries this prefix, which `free_name` has just
    // established does not occur in the module. so a module that defines a function
    // called `globals`, or `getattr`, cannot shadow anything the installer reads:
    // a forwarder is bound under a number here and takes the definition's name only
    // once it is on its way into the namespace
    let prefix = format!("{installer}_");
    let mut source = format!("\n\ndef {installer}({prefix}natives, {prefix}g):\n");
    // the forwarder is a different object from the definition python would have
    // built, so everything that definition would have been asked about is carried
    // onto it — and the definition itself is left reachable through `__wrapped__`,
    // which is where `inspect` looks for the signature of a function standing in
    // for another one.
    //
    // there is only something to carry across where the name still holds the `def`.
    // a module body that rebound it — `f = 2` after `def f` — put something else
    // there, and a compiled module has always published its native over the top of
    // whatever that was. the test is against the forwarder's own class rather than
    // a name, because the installer may read no name a module could have bound
    let _ = write!(
        source,
        "    def {prefix}adopt({prefix}shim, {prefix}name):\n\
         \x20       {prefix}twin = {prefix}g.get({prefix}name)\n\
         \x20       {prefix}shim.__code__ = {prefix}shim.__code__.replace(\n\
         \x20           co_name={prefix}name, co_qualname={prefix}name,\n\
         \x20           co_filename={SHIM_FILE:?})\n\
         \x20       {prefix}shim.__name__ = {prefix}name\n\
         \x20       {prefix}shim.__qualname__ = {prefix}name\n\
         \x20       if {prefix}twin.__class__ is {prefix}shim.__class__:\n\
         \x20           {prefix}shim.__doc__ = {prefix}twin.__doc__\n\
         \x20           {prefix}shim.__defaults__ = {prefix}twin.__defaults__\n\
         \x20           {prefix}shim.__kwdefaults__ = {prefix}twin.__kwdefaults__\n\
         \x20           {prefix}shim.__module__ = {prefix}twin.__module__\n\
         \x20           {prefix}shim.__qualname__ = {prefix}twin.__qualname__\n\
         \x20           {prefix}shim.__dict__.update({prefix}twin.__dict__)\n\
         \x20           if '__annotate__' in {prefix}twin.__class__.__dict__:\n\
         \x20               {prefix}shim.__annotate__ = {prefix}twin.__annotate__\n\
         \x20           else:\n\
         \x20               {prefix}shim.__annotations__ = {prefix}twin.__annotations__\n\
         \x20           {prefix}shim.__wrapped__ = {prefix}twin\n\
         \x20       {prefix}g[{prefix}name] = {prefix}shim\n"
    );
    // the natives arrive in the order the module's own function list walks, which is
    // the order the artefact's table is written in
    for (slot, function) in published.iter().enumerate() {
        // a cell of its own per forwarder. one shared name would be a single cell
        // every one of them closed over, so they would all end up calling whichever
        // native was assigned last
        let _ = write!(
            source,
            "    {prefix}n{slot} = {prefix}natives[{slot}]\n\
             \x20   def {prefix}f{slot}(*{prefix}a, **{prefix}k):\n\
             \x20       return {prefix}n{slot}(*{prefix}a, **{prefix}k)\n\
             \x20   {prefix}adopt({prefix}f{slot}, {:?})\n",
            function.name
        );
    }
    // and the installer takes itself back out of the namespace on its way, so the
    // only path where the artefact has to unbind it is one that never called it
    let _ = writeln!(source, "    del {prefix}g[{installer:?}]");
    Some(Shims {
        source,
        install: ShimInstall {
            installer,
            functions: published
                .iter()
                .map(|function| function.name.clone())
                .collect(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Language, module_from_source};

    fn built(source: &str) -> Shims {
        let module = module_from_source(source, "m", Language::Python);
        shims(&module, source).expect("the module publishes a forwarder")
    }

    #[test]
    fn each_exported_definition_gets_a_forwarder_over_a_cell_of_its_own() {
        let source = "\
def add(a: int, b: int) -> int:
    return a + b


def double(a: int) -> int:
    return a * 2
";
        let built = built(source);
        assert_eq!(built.install.functions, ["add", "double"]);
        assert!(
            built
                .source
                .contains("_n0 = _by_install_natives_natives[0]")
        );
        assert!(
            built
                .source
                .contains("_n1 = _by_install_natives_natives[1]")
        );
        assert!(built.source.contains(r#"_f0, "add")"#), "{}", built.source);
        assert!(
            built.source.contains(r#"_f1, "double")"#),
            "{}",
            built.source
        );
    }

    /// nothing the installer reads may be a name the module could have bound
    ///
    /// a module that defines a function called `globals` used to make the installer
    /// fail on its own first line: the forwarders were bound under the definitions'
    /// own names, so `globals` was a local of the installer before it was called
    #[test]
    fn the_installer_reads_no_name_a_module_could_shadow() {
        let source = "\
def globals(a: int) -> int:
    return a


def getattr(a: int) -> int:
    return a
";
        let rendered = built(source).source;
        for line in rendered.lines() {
            assert!(
                !line.contains("def globals") && !line.contains("def getattr"),
                "a forwarder is bound under a prefixed name: {rendered}"
            );
        }
        assert!(rendered.contains(r#""globals")"#), "{rendered}");
    }

    #[test]
    fn the_installer_takes_a_name_the_module_does_not_use() {
        let source = "_by_install_natives = 1\n\ndef f(a: int) -> int:\n    return a\n";
        assert_eq!(built(source).install.installer, "_by_install_natives_");
    }

    #[test]
    fn a_module_with_nothing_to_export_gets_no_installer() {
        let module = module_from_source("x = 1\n", "m", Language::Python);
        assert!(shims(&module, "x = 1\n").is_none());
    }
}
