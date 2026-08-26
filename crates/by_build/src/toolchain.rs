//! finding the C toolchain and the cpython headers
//!
//! the design promises "no new toolchain for the user — the c compiler cpython
//! was built with". so we ask the interpreter itself: `sysconfig` records the
//! compiler, the flags, and the include and library paths used to build it, and
//! those are exactly the ones an extension has to match.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use by_ir::ModuleName;
use by_ir::function::FallbackCode;
use serde::Deserialize;

/// the oldest interpreter a native build supports, stated once
///
/// this is the *only* place the floor is written on the rust side, so raising or
/// lowering it is one edit. it is a refusal rather than a decline because there is
/// nothing partial to fall back to: the emitted C names symbols an older cpython
/// does not have — `Py_TPFLAGS_IMMUTABLETYPE` arrived in 3.10, and the header's
/// unbound-local wording is 3.11's — so what an older interpreter gets without this
/// check is a wall of C compiler errors that names none of that
///
/// the C half of the same fact is the `#error` at the top of `by.h`, which catches a
/// compile driven by something other than `by compile`
pub const MINIMUM_PYTHON: (u8, u8) = (3, 11);

/// everything needed to compile and link an extension for one interpreter
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    /// the interpreter these settings came from
    pub python: String,
    /// the C compiler, as an argv (it may carry flags, e.g. `clang -pthread`)
    pub cc: Vec<String>,
    pub include_dirs: Vec<PathBuf>,
    pub library_dirs: Vec<PathBuf>,
    /// the suffix cpython looks for when importing, e.g.
    /// `.cpython-313-darwin.so`
    pub ext_suffix: String,
    /// platform flags for compiling a translation unit that will be loaded as a
    /// shared object
    pub compile_flags: Vec<String>,
    /// platform flags for producing a loadable shared object
    pub link_flags: Vec<String>,
    /// libraries the link needs by path, rather than resolving at load time
    pub link_libraries: Vec<PathBuf>,
    /// the interpreter's `major.minor`, so the embedded interpreted fallback is
    /// transpiled for the python that will actually run it
    pub version: Option<(u8, u8)>,
}

/// the script is run in the *target* interpreter, so the answers describe the
/// python that will import the result rather than the host we are running on
const PROBE: &str = r"
import json, os, sys, sysconfig
get = sysconfig.get_config_var

# on windows every `Py*` symbol is an import-table entry, so the link has to name the
# interpreter's import library — there is nothing to resolve at load time the way an
# elf or mach-o object does. `sysconfig` reports no library directory on windows at
# all (`LIBDIR` and `LIBPL` are posix-only), so the directory has to be built from the
# installation root, and the answer is only reported once a file is actually there
libs = []
if os.name == 'nt':
    stem = 'python' + (get('VERSION') or '')
    if get('Py_GIL_DISABLED'):
        stem += 't'
    if hasattr(sys, 'gettotalrefcount'):
        stem += '_d'
    roots = [get('LIBDIR'), get('LIBPL')]
    for base in (get('installed_base'), get('base'), sys.base_prefix, sys.prefix):
        if base:
            roots.append(os.path.join(base, 'libs'))
    for root in roots:
        if not root:
            continue
        # the msvc import library first, then what a mingw-built interpreter installs
        for name in (stem + '.lib', 'lib' + stem + '.dll.a', 'lib' + stem + '.a'):
            candidate = os.path.join(root, name)
            if os.path.isfile(candidate):
                libs.append(candidate)
                break
        if libs:
            break

cc = (get('CC') or 'cc').split()
print(json.dumps({
    'cc': cc,
    'include': [p for p in {sysconfig.get_paths()['include'],
                            sysconfig.get_paths()['platinclude']} if p],
    'libdir': [p for p in [get('LIBDIR'), get('LIBPL')] if p],
    'libs': libs,
    'ext_suffix': get('EXT_SUFFIX') or '.so',
    'platform': sys.platform,
    'version': f'{sys.version_info[0]}.{sys.version_info[1]}',
}))
";

/// compile a module body and hand back the marshalled code object
///
/// this runs in the *target* interpreter for the same reason the probe above does:
/// a code object is only readable by the interpreter that wrote it, and this is the
/// one that will read it. the source arrives on stdin because a module body is
/// routinely a hundred kilobytes, which is past what an argument list will take on
/// some platforms
///
/// `<string>` is the filename, which is what `PyRun_String` calls a module body — so
/// a traceback out of the interpreted twin says exactly what it said before
const MARSHAL: &str = r"
import importlib.util, marshal, sys

level = sys.flags.optimize
source = sys.stdin.buffer.read().decode('utf-8')
blob = marshal.dumps(compile(source, '<string>', 'exec', dont_inherit=True, optimize=level))
magic = int.from_bytes(importlib.util.MAGIC_NUMBER, 'little')
out = sys.stdout.buffer
out.write(('%d %d %d\n' % (magic, level, len(blob))).encode('ascii'))
out.write(blob)
out.flush()
";

/// the probe's answers, exactly as the interpreter reported them
///
/// every field defaults, because an interpreter that cannot answer one of these is
/// better served by the checks below than by a parse failure naming a key
#[derive(Debug, Default, Deserialize)]
struct Probe {
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    include: Vec<PathBuf>,
    #[serde(default)]
    libdir: Vec<PathBuf>,
    #[serde(default)]
    libs: Vec<PathBuf>,
    #[serde(default)]
    ext_suffix: Option<String>,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    version: Option<String>,
}

impl Toolchain {
    /// probe an interpreter for its build settings
    pub fn probe(python: &str) -> Result<Self> {
        let output = Command::new(python)
            .args(["-c", PROBE])
            .output()
            .with_context(|| format!("could not run `{python}`"))?;
        if !output.status.success() {
            bail!(
                "`{python}` could not report its build configuration: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Self::from_probe(python, text.trim())
    }

    /// parse a probe result. split out so the parsing is testable without an
    /// interpreter
    pub fn from_probe(python: &str, json: &str) -> Result<Self> {
        // a real json parser rather than a hand-rolled one: a windows path is full of
        // backslashes, every one of which `json.dumps` escapes, and reading the escape
        // as a literal character turns each separator into two
        let probe: Probe = serde_json::from_str(json)
            .with_context(|| format!("`{python}` reported a build configuration we cannot read"))?;
        if probe.cc.is_empty() {
            bail!("`{python}` reported no C compiler");
        }
        let ext_suffix = probe
            .ext_suffix
            .with_context(|| format!("`{python}` reported no extension suffix"))?;

        // macos resolves undefined symbols against the loading interpreter rather
        // than a linked libpython, which is also how cpython builds its own
        // extensions
        let link_flags = if probe.platform == "darwin" {
            vec![
                "-bundle".to_string(),
                "-undefined".to_string(),
                "dynamic_lookup".to_string(),
            ]
        } else {
            vec!["-shared".to_string()]
        };

        // windows code is position independent by construction, and the flag asking for
        // it is one the compiler reports back as ignored
        let compile_flags = if probe.platform == "win32" {
            Vec::new()
        } else {
            vec!["-fPIC".to_string()]
        };

        let version = probe.version.as_deref().and_then(|text| {
            let (major, minor) = text.split_once('.')?;
            Some((major.parse().ok()?, minor.parse().ok()?))
        });
        // only a version we could *read* is refused. the probe always asks for one, so
        // an unknown here means an answer that would not parse — which is not evidence
        // of an old python, and turning it into a version verdict would refuse a
        // working interpreter over a formatting surprise
        if let Some((major, minor)) = version
            && (major, minor) < MINIMUM_PYTHON
        {
            let (least_major, least_minor) = MINIMUM_PYTHON;
            bail!(
                "a native build needs python {least_major}.{least_minor} or later, \
                 and `{python}` is python {major}.{minor}"
            );
        }

        Ok(Self {
            python: python.to_string(),
            cc: probe.cc,
            include_dirs: probe.include,
            library_dirs: probe.libdir,
            ext_suffix,
            compile_flags,
            link_flags,
            link_libraries: probe.libs,
            version,
        })
    }

    /// where an extension for `module` must sit, relative to the root of an output
    /// tree, to be importable under that name
    pub fn extension_path(&self, module: &ModuleName) -> PathBuf {
        module.relative_path(&self.ext_suffix)
    }

    /// compile a module body in this interpreter, for the artefact to carry
    ///
    /// the answer is a cache and nothing depends on having it, so every way this can
    /// fail — no such interpreter, a body it will not compile, an answer we cannot
    /// read — reads as `None` and leaves the artefact running the source. that is
    /// what it does today, so the worst outcome is the speed we already have
    pub fn marshal(&self, source: &str) -> Option<FallbackCode> {
        // the emitted C has to be a function of the source and nothing else — that is
        // what lets a rebuild skip the C compiler, which is by far its slowest step. the
        // one thing in a code object that could vary between two runs of one interpreter
        // is a `set` or `frozenset` constant, which `x in {"a", "b"}` compiles to: it
        // holds strings, whose hashes are seeded per process. cpython 3.13 and 3.14 both
        // write such a constant in a fixed order regardless, so pinning the seed changes
        // nothing measurable today — it is here so that this does not *depend* on their
        // doing so. it cannot change the program either way: the set is rebuilt under the
        // reading interpreter's own hashing
        let mut child = Command::new(&self.python)
            .args(["-c", MARSHAL])
            .env("PYTHONHASHSEED", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // the child is waited for whatever happens to the write, because a `Child` that
        // is merely dropped is never reaped — and a whole-project build runs one of these
        // per module
        let written = child
            .stdin
            .take()
            .is_some_and(|mut stdin| stdin.write_all(source.as_bytes()).is_ok());
        let output = child.wait_with_output().ok()?;
        if !written || !output.status.success() {
            return None;
        }
        parse_marshal(&output.stdout)
    }
}

/// read what [`MARSHAL`] wrote: one ascii header line, then the bytes it counted
fn parse_marshal(output: &[u8]) -> Option<FallbackCode> {
    let split = output.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&output[..split]).ok()?;
    let mut fields = header.split(' ');
    let magic: i64 = fields.next()?.parse().ok()?;
    let optimize: i32 = fields.next()?.parse().ok()?;
    let length: usize = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    let marshalled = output.get(split + 1..)?;
    // a short read means the interpreter was interrupted partway through writing, and
    // a long one means something else wrote to its stdout — either way these are not
    // the bytes it counted, and half a code object is worse than none
    if marshalled.len() != length || length == 0 {
        return None;
    }
    Some(FallbackCode {
        marshalled: marshalled.into(),
        magic,
        optimize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"cc": ["clang", "-pthread"], "include": ["/usr/include/python3.13"], "libdir": ["/usr/lib"], "libs": [], "ext_suffix": ".cpython-313-darwin.so", "platform": "darwin", "version": "3.13"}"#;

    /// what a windows interpreter reports: no library directory, and the import
    /// library found under the installation root
    const WINDOWS: &str = r#"{"cc": ["cc"], "include": ["C:\\hostedtoolcache\\Python\\3.12.10\\x64\\Include"], "libdir": [], "libs": ["C:\\hostedtoolcache\\Python\\3.12.10\\x64\\libs\\python312.lib"], "ext_suffix": ".cp312-win_amd64.pyd", "platform": "win32", "version": "3.12"}"#;

    #[test]
    fn the_interpreter_version_is_recorded() {
        // the embedded fallback has to be transpiled for the python that will run
        // it: `dataclass(slots=True)` needs 3.10, and emitting it for a 3.9
        // interpreter makes the extension fail at import
        let toolchain = Toolchain::from_probe("python3", SAMPLE).unwrap();
        assert_eq!(toolchain.version, Some((3, 13)));
    }

    #[test]
    fn a_probe_with_no_version_leaves_it_unknown() {
        let json = SAMPLE.replace(r#", "version": "3.13""#, "");
        let toolchain = Toolchain::from_probe("python3", &json).unwrap();
        assert_eq!(toolchain.version, None);
    }

    #[test]
    fn an_interpreter_below_the_floor_is_refused_by_name() {
        // what an old interpreter used to get instead was a wall of C compiler errors
        // over an undeclared `Py_TPFLAGS_IMMUTABLETYPE`, which says nothing about the
        // version. the refusal names the floor and what was found, the way the emitted
        // `PyInit_`'s own version guard does
        let json = SAMPLE.replace(r#""version": "3.13""#, r#""version": "3.9""#);
        let error = Toolchain::from_probe("python3.9", &json).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a native build needs python 3.11 or later, and `python3.9` is python 3.9"
        );
    }

    #[test]
    fn the_floor_itself_is_supported() {
        // an off-by-one here would refuse the very version the floor names
        let (major, minor) = MINIMUM_PYTHON;
        let json = SAMPLE.replace(
            r#""version": "3.13""#,
            &format!(r#""version": "{major}.{minor}""#),
        );
        let toolchain = Toolchain::from_probe("python3", &json).unwrap();
        assert_eq!(toolchain.version, Some(MINIMUM_PYTHON));
    }

    #[test]
    fn an_unreadable_version_is_not_refused_as_an_old_one() {
        // the floor is a statement about interpreters known to be too old. a probe that
        // did not answer has not made that statement, and refusing it would turn a
        // parse gap into a version verdict
        let json = SAMPLE.replace(r#""version": "3.13""#, r#""version": "three.nine""#);
        let toolchain = Toolchain::from_probe("python3", &json).unwrap();
        assert_eq!(toolchain.version, None);
    }

    #[test]
    fn a_probe_result_parses_into_a_toolchain() {
        let toolchain = Toolchain::from_probe("python3", SAMPLE).unwrap();
        assert_eq!(toolchain.cc, vec!["clang", "-pthread"]);
        assert_eq!(
            toolchain.include_dirs,
            vec![PathBuf::from("/usr/include/python3.13")]
        );
        assert_eq!(toolchain.ext_suffix, ".cpython-313-darwin.so");
    }

    #[test]
    fn macos_links_a_bundle_with_dynamic_lookup() {
        let toolchain = Toolchain::from_probe("python3", SAMPLE).unwrap();
        assert!(toolchain.link_flags.contains(&"-bundle".to_string()));
        assert!(toolchain.link_flags.contains(&"dynamic_lookup".to_string()));
    }

    #[test]
    fn other_platforms_link_a_plain_shared_object() {
        let json = SAMPLE.replace("darwin", "linux");
        let toolchain = Toolchain::from_probe("python3", &json).unwrap();
        assert_eq!(toolchain.link_flags, vec!["-shared"]);
    }

    #[test]
    fn the_extension_path_mirrors_the_module_tree() {
        let toolchain = Toolchain::from_probe("python3", SAMPLE).unwrap();
        assert_eq!(
            toolchain.extension_path(&ModuleName::new("pkg.app")),
            PathBuf::from("pkg/app.cpython-313-darwin.so")
        );
        assert_eq!(
            toolchain.extension_path(&ModuleName::package("pkg")),
            PathBuf::from("pkg/__init__.cpython-313-darwin.so")
        );
    }

    #[test]
    fn a_probe_with_no_compiler_is_an_error() {
        let json = SAMPLE.replace(r#""cc": ["clang", "-pthread"]"#, r#""cc": []"#);
        let error = Toolchain::from_probe("python3", &json).unwrap_err();
        assert!(error.to_string().contains("no C compiler"));
    }

    #[test]
    fn a_probe_with_no_suffix_is_an_error() {
        let json = SAMPLE.replace(r#""ext_suffix": ".cpython-313-darwin.so", "#, "");
        let error = Toolchain::from_probe("python3", &json).unwrap_err();
        assert!(error.to_string().contains("no extension suffix"));
    }

    #[test]
    fn a_missing_list_reads_as_empty_rather_than_failing() {
        let json = SAMPLE.replace(r#""libdir": ["/usr/lib"], "#, "");
        let toolchain = Toolchain::from_probe("python3", &json).unwrap();
        assert!(toolchain.library_dirs.is_empty());
    }

    #[test]
    fn a_windows_probe_carries_the_import_library() {
        // there is nothing for the loader to resolve on windows: an unlinked `Py*` is a
        // link error, not a symbol filled in when the interpreter loads the extension
        let toolchain = Toolchain::from_probe("python3", WINDOWS).unwrap();
        assert_eq!(
            toolchain.link_libraries,
            vec![PathBuf::from(
                r"C:\hostedtoolcache\Python\3.12.10\x64\libs\python312.lib"
            )]
        );
    }

    #[test]
    fn a_windows_path_keeps_its_separators() {
        // `json.dumps` escapes every backslash, and reading the escape as a literal
        // character doubles each separator in the path
        let toolchain = Toolchain::from_probe("python3", WINDOWS).unwrap();
        assert_eq!(
            toolchain.include_dirs,
            vec![PathBuf::from(
                r"C:\hostedtoolcache\Python\3.12.10\x64\Include"
            )]
        );
    }

    #[test]
    fn a_marshal_result_parses_into_a_code_object_and_its_guards() {
        // the payload is binary and may hold anything, newlines included — only the
        // *first* one ends the header, and the count says where the rest stops
        let code = parse_marshal(b"168627699 0 5\n\xc3\n\x00\xffz").unwrap();
        assert_eq!(code.magic, 168_627_699);
        assert_eq!(code.optimize, 0);
        assert_eq!(&*code.marshalled, b"\xc3\n\x00\xffz");
    }

    #[test]
    fn a_marshal_result_that_does_not_match_its_own_count_is_refused() {
        // a code object is worthless in halves, and an interpreter that printed
        // something of its own before ours has not left us the bytes it counted
        assert!(parse_marshal(b"168627699 0 9\n\xc3\xc3").is_none());
        assert!(parse_marshal(b"168627699 0 1\n\xc3\xc3").is_none());
        assert!(parse_marshal(b"168627699 0 0\n").is_none());
        assert!(parse_marshal(b"168627699 0\n\xc3").is_none());
        assert!(parse_marshal(b"168627699 0 1 4\n\xc3").is_none());
        assert!(parse_marshal(b"no header at all").is_none());
    }

    #[test]
    fn position_independence_is_asked_for_only_where_it_is_a_choice() {
        assert_eq!(
            Toolchain::from_probe("python3", SAMPLE)
                .unwrap()
                .compile_flags,
            vec!["-fPIC"]
        );
        assert!(
            Toolchain::from_probe("python3", WINDOWS)
                .unwrap()
                .compile_flags
                .is_empty()
        );
    }
}
