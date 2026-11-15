//! `_by_build.json` — what a build tree says about the build that wrote it.
//!
//! A build tree is not self-describing. The generated python in it was emitted
//! under a particular transpile config, and nothing in the python says which one.
//! That is tolerable while the tree is only ever written whole; it stops being
//! tolerable the moment one file is re-staged into an existing tree, because the
//! re-transpile has to reproduce bytes the original build produced, and a config
//! it re-derived from the project is not reliably the config the build used.
//!
//! `by run` is the case that proves it. It derives `min_version` from the
//! interpreter it probed, while `by build` derives it from the project config — so
//! a project targeting 3.13 that is run on a 3.11 interpreter is transpiled for
//! 3.11, and a later re-transpile that asked the project instead would emit 3.13
//! code into a tree of 3.12 modules. The debugger would refuse it as a changed
//! module body, which is the good outcome; the bad one is that it does not refuse
//! and the module runs code the interpreter cannot support.
//!
//! So the build writes down what it actually used, after the probe and after the
//! lowering flags were folded in, and a re-stage reads it back rather than
//! deriving anything.

use std::collections::BTreeMap;
use std::fs::File;
use std::hash::Hasher as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use seahash::SeaHasher;

use by_transforms::SoundnessPositions;
use by_transforms::config::FloatLiteralLowering;
use by_transforms::config::{Config, PythonVersion};

use crate::runtime::RuntimeLayout;
use crate::staging::Staging;

/// The name of the record, at the root of the output tree.
pub(crate) const BY_BUILD_FILENAME: &str = "_by_build.json";

/// which build of `by` is running, as the record and the map name it: a hash of the running
/// executable's contents, or why it cannot be had
///
/// a re-stage asks whether the running `by` is the one that wrote a tree, and the executable
/// is the exact answer. the same binary, wherever it is installed, names the same build, and a
/// rebuild names a new one, whatever it changed. two binaries built apart from the same sources
/// usually differ in their bytes, and a re-stage across them is refused, which costs a rebuild
/// rather than reloading code the running `by` would not have written
///
/// taken the first time it is asked for, and kept for the life of the process
pub(crate) fn build_identity() -> Result<&'static str, &'static str> {
    static IDENTITY: OnceLock<Result<String, String>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            identify_executable(
                EXECUTABLE.get_or_init(open_executable).as_ref(),
                std::env::current_exe(),
            )
        })
        .as_deref()
        .map_err(String::as_str)
}

/// the running executable, opened when the program starts, or `None` if it could not be
///
/// the identity is taken through this rather than through the executable's path, because a
/// rebuild replaces the file at that path while a program started from the old one keeps
/// running: a language server outlives many rebuilds of `by`, and naming itself after the new
/// file would claim a build whose code it is not running. replacing or deleting the file does
/// not touch a file already open. holding it open never stops either: on unix nothing does, and
/// on windows the standard library opens a file sharing it for deletion and renaming
static EXECUTABLE: OnceLock<Option<File>> = OnceLock::new();

/// Open the running executable now, so that the build a tree's record and map name is the
/// binary this process is running even if another build replaces it on disk later. A program
/// calls it first thing.
///
/// It never fails the program: an executable that cannot be opened now is opened when the
/// identity is first asked for, and if that fails too, it is said where a record or map is
/// written or compared.
pub fn pin_executable() {
    EXECUTABLE.get_or_init(open_executable);
}

fn open_executable() -> Option<File> {
    File::open(std::env::current_exe().ok()?).ok()
}

/// a hash of the executable's contents, read through `pinned` when the program opened it at
/// startup, and through `path` when it did not
fn identify_executable(
    pinned: Option<&File>,
    path: std::io::Result<PathBuf>,
) -> Result<String, String> {
    let read = |mut reader: &File| {
        let mut hasher = SeaHasher::new();
        let mut buffer = vec![0; 1 << 20];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(hasher.finish()),
                Ok(read) => hasher.write(&buffer[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    };
    let hash = match pinned {
        Some(file) => read(file).map_err(|error| {
            format!("the running `by` could not read its own executable: {error}")
        })?,
        None => {
            let path = path.map_err(|error| {
                format!("the running `by` could not find its own executable: {error}")
            })?;
            File::open(&path)
                .and_then(|file| read(&file))
                .map_err(|error| {
                    format!(
                        "the running `by` could not read its own executable `{}`: {error}",
                        path.display()
                    )
                })?
        }
    };
    Ok(format!("{hash:016x}"))
}

/// The transpile config a build ran under, in the record's own spelling.
///
/// Only the settings a build can vary. The rest of [`Config`] is either fixed
/// (`lazy_imports`, `inject_future_annotations`) or per-file (`is_python`,
/// `is_stub`), and a re-stage that re-derived those from the file it was handed
/// gets the same answers the build did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigRecord {
    /// the python the emitted code must run on, as `major.minor`
    min_version: String,
    /// the `--soundness` spec, spelled the way the flag takes it
    soundness: String,
    runtime_raises_checks: bool,
    unique_loop_bindings: bool,
    /// how a float or complex literal type was spelled, as the option takes it
    float_literals: String,
    /// the `build:` stamps the build settled, so a re-stage emits the same ones
    ///
    /// without this a re-staged file would fall back to the block's defaults and
    /// claim a different commit than the tree around it. defaulted, because a
    /// tree written before stamps existed genuinely had none
    #[serde(default)]
    stamps: BTreeMap<String, String>,
}

/// What a build tree records about itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildRecord {
    /// [`build_identity`] of the `by` that wrote the tree
    pub(crate) by_build: String,
    /// the project the tree was built from, absolute and canonical
    pub(crate) project_root: PathBuf,
    /// the module roots the tree's layout was derived from, deepest first —
    /// the same list and the same order [`relative_destination`] was given, so a
    /// re-stage puts a file exactly where the build put it
    ///
    /// [`relative_destination`]: crate::staging::relative_destination
    pub(crate) module_roots: Vec<PathBuf>,
    /// the module that runs as `__main__`, when the build has one
    ///
    /// `by run` always does. `by build` has one only when the project configures
    /// `run.main`, and `null` there is the honest answer: an output tree nobody
    /// has named an entry point for does not have one, and inventing a name would
    /// tell a debugger that a module it must not replace is safe to replace.
    pub(crate) entry_module: Option<String>,
    /// whether the tree's modules were compiled to native extensions
    ///
    /// A compiled module has no `__code__` to assign, so a tree with this set can
    /// never be re-staged a file at a time. The record says so rather than
    /// leaving a caller to discover it from a failed replacement.
    pub(crate) compiled: bool,
    /// whether a module in no package imports the copy of the runtime at the tree's root, as
    /// in a tree nothing ships, rather than having its definitions pasted in
    ///
    /// it decides the bytes of every such module, so a re-stage has to lay the runtime out
    /// as the build did. defaulted, because a tree written before root copies existed pasted
    /// them in
    #[serde(default)]
    pub(crate) runtime_at_root: bool,
    pub(crate) config: ConfigRecord,
}

impl BuildRecord {
    /// Describe a build that is about to be written, laying the runtime out as `runtime`.
    ///
    /// An error when the running `by` cannot name its own build, which the record exists to say.
    pub fn new(
        project_root: &Path,
        module_roots: &[PathBuf],
        entry_module: Option<String>,
        compiled: bool,
        runtime: &RuntimeLayout,
        config: &Config,
    ) -> anyhow::Result<Self> {
        let by_build = build_identity().map_err(|reason| {
            anyhow::anyhow!("could not record which build of `by` writes this tree: {reason}")
        })?;
        Ok(Self {
            by_build: by_build.to_owned(),
            project_root: project_root.to_path_buf(),
            module_roots: module_roots.to_vec(),
            entry_module,
            compiled,
            runtime_at_root: runtime.is_unshipped(),
            config: ConfigRecord {
                min_version: config.min_version.to_string(),
                soundness: spell_soundness(config.soundness),
                runtime_raises_checks: config.runtime_raises_checks,
                unique_loop_bindings: config.unique_loop_bindings,
                float_literals: spell_float_literals(config.float_literals),
                stamps: config.stamps.clone(),
            },
        })
    }

    /// The record as it is written into the tree: pretty-printed, newline
    /// terminated, so a person looking into a build directory can read it.
    fn render(&self) -> String {
        let mut rendered = serde_json::to_string_pretty(self)
            .expect("a record of strings and bools always serializes");
        rendered.push('\n');
        rendered
    }

    /// Read the record out of a build directory.
    pub(crate) fn read(build_directory: &Path) -> anyhow::Result<Self> {
        let path = build_directory.join(BY_BUILD_FILENAME);
        let contents = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!(
                "`{}` has no {BY_BUILD_FILENAME}, so nothing says which build it is: {error}",
                build_directory.display(),
            )
        })?;
        serde_json::from_str(&contents).map_err(|error| match error.classify() {
            // well-formed, but not the record this `by` writes: the one a build of `by` writes
            // is always the one it reads, so another build wrote it
            serde_json::error::Category::Data => anyhow::anyhow!(
                "`{}` was built by another by, whose {BY_BUILD_FILENAME} this one cannot read \
                 ({error}) — rebuild it before reloading into it",
                build_directory.display(),
            ),
            serde_json::error::Category::Io
            | serde_json::error::Category::Syntax
            | serde_json::error::Category::Eof => {
                anyhow::anyhow!("`{}` could not be read: {error}", path.display())
            }
        })
    }

    /// The runtime layout this build wrote, with nothing recorded in it yet.
    pub(crate) fn runtime_layout(&self) -> RuntimeLayout {
        if self.runtime_at_root {
            RuntimeLayout::unshipped()
        } else {
            RuntimeLayout::shipped()
        }
    }

    /// The transpile config this build ran under, rebuilt from the record.
    ///
    /// Everything the record does not carry comes from [`Config::default`],
    /// which is where the build got it too.
    pub(crate) fn config(&self) -> anyhow::Result<Config> {
        let min_version = self
            .config
            .min_version
            .parse::<PythonVersion>()
            .map_err(|_| {
                anyhow::anyhow!(
                    "{BY_BUILD_FILENAME} names python {:?}, which is not a version this `by` knows",
                    self.config.min_version
                )
            })?;
        Ok(Config {
            min_version,
            soundness: parse_soundness(&self.config.soundness)?,
            runtime_raises_checks: self.config.runtime_raises_checks,
            unique_loop_bindings: self.config.unique_loop_bindings,
            float_literals: parse_float_literals(&self.config.float_literals)?,
            stamps: self.config.stamps.clone(),
            ..Config::default()
        })
    }
}

/// Write the record into the output tree.
///
/// Through the [`Staging`] like every other generated file, so the manifest knows
/// about it: a record left behind by a build whose tree was later rebuilt without
/// one would describe a tree that no longer exists, and a re-stage reading it
/// would transpile under a config nobody chose.
pub fn stage_build_record(staging: &mut Staging, record: &BuildRecord) -> anyhow::Result<()> {
    staging.write(Path::new(BY_BUILD_FILENAME), None, &record.render())
}

/// Parse a `--soundness` spec: `default` (the inference-gap checks), `all`
/// (those plus the opt-in `parameters` entry checks), `none`, or a
/// comma-separated subset of the position names. Unknown names are a hard
/// error so a typo doesn't silently disable a check the user expected.
pub fn parse_soundness(spec: &str) -> anyhow::Result<SoundnessPositions> {
    match spec.trim() {
        "default" => return Ok(SoundnessPositions::defaults()),
        "all" => return Ok(SoundnessPositions::all()),
        "none" => return Ok(SoundnessPositions::none()),
        _ => {}
    }
    let mut positions = SoundnessPositions::none();
    for name in spec.split(',') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        match name {
            "generic-calls" => positions.generic_calls = true,
            "projections" => positions.projections = true,
            "iterations" => positions.iterations = true,
            "assignments" => positions.assignments = true,
            "returns" => positions.returns = true,
            "arguments" => positions.arguments = true,
            "parameters" => positions.parameters = true,
            other => anyhow::bail!(
                "unknown soundness position {other:?} — use `default`, `all`, `none`, or a \
                 comma-separated subset of: generic-calls, projections, iterations, assignments, \
                 returns, arguments, parameters"
            ),
        }
    }
    Ok(positions)
}

/// Write a set of positions back as a spec [`parse_soundness`] reads.
///
/// The named sets are preferred where they fit, because that is what a person
/// looking at the record wrote on the command line. Anything else is the explicit
/// list, which round-trips exactly — and it has to round-trip exactly rather than
/// approximately, since these positions decide which runtime checks the emitted
/// python carries.
/// The spelling a [`FloatLiteralLowering`] is written down as, matching the
/// `lowering.float-literals` option a project sets it with.
fn spell_float_literals(lowering: FloatLiteralLowering) -> String {
    match lowering {
        FloatLiteralLowering::Nominal => "nominal".to_owned(),
        FloatLiteralLowering::Literal => "literal".to_owned(),
    }
}

/// Read back what [`spell_float_literals`] wrote.
fn parse_float_literals(spelled: &str) -> anyhow::Result<FloatLiteralLowering> {
    match spelled {
        "nominal" => Ok(FloatLiteralLowering::Nominal),
        "literal" => Ok(FloatLiteralLowering::Literal),
        other => anyhow::bail!(
            "{BY_BUILD_FILENAME} spells float literals {other:?}, \
             which is not `nominal` or `literal`"
        ),
    }
}

pub(crate) fn spell_soundness(positions: SoundnessPositions) -> String {
    if positions == SoundnessPositions::defaults() {
        return "default".to_owned();
    }
    if positions == SoundnessPositions::all() {
        return "all".to_owned();
    }
    if positions == SoundnessPositions::none() {
        return "none".to_owned();
    }
    [
        ("generic-calls", positions.generic_calls),
        ("projections", positions.projections),
        ("iterations", positions.iterations),
        ("assignments", positions.assignments),
        ("returns", positions.returns),
        ("arguments", positions.arguments),
        ("parameters", positions.parameters),
    ]
    .into_iter()
    .filter_map(|(name, on)| on.then_some(name))
    .collect::<Vec<_>>()
    .join(",")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        BuildRecord, ConfigRecord, build_identity, identify_executable, parse_soundness,
        spell_soundness,
    };
    use crate::runtime::RuntimeLayout;
    use by_transforms::SoundnessPositions;
    use by_transforms::config::{Config, FloatLiteralLowering};
    use std::fs::File;
    use std::path::{Path, PathBuf};

    /// the identity is a hash of the running executable, the same each time it is asked
    #[test]
    fn the_build_identity_names_the_running_executable() {
        let identity = build_identity().expect("the test binary can read itself");
        assert!(
            identity.len() == 16 && identity.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{identity}"
        );
        assert_eq!(build_identity(), Ok(identity));
        let executable = std::env::current_exe().expect("the test knows where it is");
        assert_eq!(
            identify_executable(None, Ok(executable)).as_deref(),
            Ok(identity),
            "read through its path, the same binary names the same build"
        );
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("the file is written");
    }

    /// a rebuild that replaces the executable on disk while a program started from it still
    /// runs leaves that program naming the binary it is running, not the new one. holding the
    /// file open does not stop the replacement, nor a deletion, on any platform
    #[test]
    fn a_binary_replaced_on_disk_keeps_the_identity_it_started_with() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("by");
        write_executable(&path, b"the binary that started");
        let started = identify_executable(None, Ok(path.clone())).expect("it reads");
        let pinned = File::open(&path).expect("it opens");

        let rebuilt = dir.path().join("by.new");
        write_executable(&rebuilt, b"the binary a rebuild wrote");
        std::fs::rename(&rebuilt, &path).expect("the open file is replaced");

        assert_eq!(
            identify_executable(Some(&pinned), Ok(path.clone())).as_deref(),
            Ok(started.as_str()),
            "the program that pinned the old binary names it"
        );
        let replaced = identify_executable(None, Ok(path.clone())).expect("it reads");
        assert_ne!(
            replaced, started,
            "a program started now names the new binary"
        );

        let pinned = File::open(&path).expect("it opens");
        std::fs::remove_file(&path).expect("the open file is deleted");
        assert_eq!(
            identify_executable(Some(&pinned), Ok(path)).as_deref(),
            Ok(replaced.as_str())
        );
    }

    /// an executable that cannot be read is said, naming it, where the identity is asked for
    #[test]
    fn an_unreadable_executable_is_said() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("gone");
        let reason = identify_executable(None, Ok(path.clone())).expect_err("there is no file");
        assert!(reason.contains(&path.display().to_string()), "{reason}");
        let reason = identify_executable(None, Err(std::io::Error::other("nowhere")))
            .expect_err("there is no path");
        assert!(reason.contains("nowhere"), "{reason}");
    }

    /// the wire form, exactly as the plugin and `bpd` read it. nothing else here
    /// exercises the field names, and a serde attribute that renamed one would be
    /// invisible from inside the crate and total from outside it
    #[test]
    fn the_record_serializes_under_the_names_the_contract_names() {
        let record = BuildRecord::new(
            Path::new("/p"),
            &[PathBuf::from("/p/src")],
            Some("main".to_owned()),
            false,
            &RuntimeLayout::unshipped(),
            &Config::default(),
        )
        .expect("the running `by` names its build");
        let json: serde_json::Value =
            serde_json::from_str(&record.render()).expect("the record is plain data");

        assert!(json["byBuild"].as_str().is_some_and(|v| !v.is_empty()));
        assert_eq!(json["projectRoot"], "/p");
        assert_eq!(json["moduleRoots"][0], "/p/src");
        assert_eq!(json["entryModule"], "main");
        assert_eq!(json["compiled"], false);
        assert_eq!(json["runtimeAtRoot"], true);
        assert_eq!(
            json["config"]["minVersion"],
            Config::default().min_version.to_string()
        );
        // spelled the way `parse_soundness` reads it back, which is the only thing
        // this string is for — a literal here would pass while the round trip the
        // record exists for was broken
        assert_eq!(
            json["config"]["soundness"],
            spell_soundness(Config::default().soundness)
        );
        assert_eq!(json["config"]["runtimeRaisesChecks"], false);
        assert_eq!(json["config"]["uniqueLoopBindings"], true);
    }

    /// **the property the string is for.** the record exists so that a re-stage
    /// transpiles with the configuration the build used, and every field of it
    /// crosses as text — so a spelling `parse_soundness` cannot read back is a
    /// re-stage silently emitting for a different soundness than the tree holds
    #[test]
    fn the_soundness_a_record_writes_is_one_it_can_read_back() {
        for positions in [
            Config::default().soundness,
            SoundnessPositions::none(),
            SoundnessPositions::all(),
        ] {
            let spelled = spell_soundness(positions);
            let read = parse_soundness(&spelled)
                .unwrap_or_else(|e| panic!("`{spelled}` should read back: {e}"));
            assert_eq!(read, positions, "`{spelled}` did not round trip");
        }
    }

    /// a tree nobody named an entry point for says so, rather than naming one
    #[test]
    fn a_build_with_no_entry_module_says_null_and_keeps_the_key() {
        let record = BuildRecord::new(
            Path::new("/p"),
            &[],
            None,
            false,
            &RuntimeLayout::shipped(),
            &Config::default(),
        )
        .expect("the running `by` names its build");
        let json: serde_json::Value =
            serde_json::from_str(&record.render()).expect("the record is plain data");
        assert!(json.get("entryModule").is_some(), "the key stays: {json}");
        assert!(json["entryModule"].is_null());
    }

    /// the config a re-stage transpiles under has to be the one the build used,
    /// down to the individual soundness positions
    #[test]
    fn the_config_survives_the_round_trip() -> anyhow::Result<()> {
        let mut soundness = SoundnessPositions::none();
        soundness.returns = true;
        soundness.iterations = true;
        let config = Config {
            soundness,
            runtime_raises_checks: true,
            unique_loop_bindings: false,
            float_literals: FloatLiteralLowering::Literal,
            ..Config::default()
        };

        let record = BuildRecord::new(
            Path::new("/p"),
            &[],
            None,
            false,
            &RuntimeLayout::shipped(),
            &config,
        )
        .expect("the running `by` names its build");
        let parsed: BuildRecord = serde_json::from_str(&record.render())?;
        let recovered = parsed.config()?;

        assert_eq!(recovered.soundness, soundness);
        assert!(recovered.runtime_raises_checks);
        assert!(!recovered.unique_loop_bindings);
        assert_eq!(recovered.float_literals, FloatLiteralLowering::Literal);
        assert_eq!(recovered.min_version, config.min_version);
        Ok(())
    }

    #[test]
    fn every_soundness_spelling_round_trips() -> anyhow::Result<()> {
        for spec in ["default", "all", "none", "returns", "iterations,returns"] {
            let positions = parse_soundness(spec)?;
            assert_eq!(
                parse_soundness(&spell_soundness(positions))?,
                positions,
                "{spec} did not survive being written back"
            );
        }
        Ok(())
    }

    /// a record carrying a field this `by` does not know is a record written by a
    /// different `by`, and reading it as though the unknown field were absent is
    /// how a re-stage would transpile under a config nobody chose
    #[test]
    fn a_record_with_an_unknown_field_is_refused() {
        let json = r#"{
            "byBuild": "x", "projectRoot": "/p", "moduleRoots": [],
            "entryModule": null, "compiled": false,
            "config": {"minVersion": "3.13", "soundness": "none",
                       "runtimeRaisesChecks": false, "uniqueLoopBindings": true,
                       "somethingNew": true}
        }"#;
        assert!(serde_json::from_str::<BuildRecord>(json).is_err());
    }

    /// a re-stage lays the runtime out as the build that wrote the tree did: a module in no
    /// package of a `by run` tree imports the root copy, and one of a `by build` tree has the
    /// definitions pasted in
    #[test]
    fn the_record_keeps_how_the_build_laid_the_runtime_out() {
        for layout in [RuntimeLayout::unshipped(), RuntimeLayout::shipped()] {
            let record = BuildRecord::new(
                Path::new("/p"),
                &[],
                None,
                false,
                &layout,
                &Config::default(),
            )
            .expect("the running `by` names its build");
            let read: BuildRecord =
                serde_json::from_str(&record.render()).expect("the record reads back");
            assert_eq!(
                read.runtime_layout().is_unshipped(),
                layout.is_unshipped(),
                "{}",
                record.render()
            );
        }
    }

    /// a tree written before root copies existed had the definitions pasted in
    #[test]
    fn a_record_without_a_runtime_layout_pasted_the_definitions_in() {
        let mut json: serde_json::Value = serde_json::from_str(
            &BuildRecord::new(
                Path::new("/p"),
                &[],
                None,
                false,
                &RuntimeLayout::unshipped(),
                &Config::default(),
            )
            .expect("the running `by` names its build")
            .render(),
        )
        .expect("the record is plain data");
        json.as_object_mut()
            .expect("the record is an object")
            .remove("runtimeAtRoot");
        let read: BuildRecord = serde_json::from_value(json).expect("the record reads back");
        assert!(!read.runtime_layout().is_unshipped());
    }

    #[test]
    fn a_version_the_record_names_that_this_by_cannot_target_is_refused() {
        let record = BuildRecord {
            by_build: "x".to_owned(),
            project_root: PathBuf::from("/p"),
            module_roots: Vec::new(),
            entry_module: None,
            compiled: false,
            runtime_at_root: false,
            config: ConfigRecord {
                min_version: "three point twelve".to_owned(),
                soundness: "none".to_owned(),
                runtime_raises_checks: false,
                unique_loop_bindings: true,
                float_literals: "nominal".to_owned(),
                stamps: BTreeMap::new(),
            },
        };
        assert!(record.config().is_err());
    }
}
