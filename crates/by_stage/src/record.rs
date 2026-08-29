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

use std::path::{Path, PathBuf};

use by_transforms::SoundnessPositions;
use by_transforms::config::{Config, PythonVersion};

use crate::staging::Staging;

/// The name of the record, at the root of the output tree.
pub const BY_BUILD_FILENAME: &str = "_by_build.json";

/// Which build of `by` wrote this tree.
///
/// The package version, the commit, and whether the worktree was dirty when the
/// crate was compiled — see this crate's `build.rs` for why the last of those is
/// load-bearing. It is a constant of the crate `by` and `by server` share, so two
/// binaries built together always agree and two built either side of a change
/// never do.
pub fn build_identity() -> &'static str {
    env!("BY_BUILD_IDENTITY")
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
    pub min_version: String,
    /// the `--soundness` spec, spelled the way the flag takes it
    pub soundness: String,
    pub runtime_raises_checks: bool,
    pub unique_loop_bindings: bool,
}

/// What a build tree records about itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildRecord {
    /// [`build_identity`] of the `by` that wrote the tree
    pub by_version: String,
    /// the project the tree was built from, absolute and canonical
    pub project_root: PathBuf,
    /// the module roots the tree's layout was derived from, deepest first —
    /// the same list and the same order [`relative_destination`] was given, so a
    /// re-stage puts a file exactly where the build put it
    ///
    /// [`relative_destination`]: crate::staging::relative_destination
    pub module_roots: Vec<PathBuf>,
    /// the module that runs as `__main__`, when the build has one
    ///
    /// `by run` always does. `by build` has one only when the project configures
    /// `run.main`, and `null` there is the honest answer: an output tree nobody
    /// has named an entry point for does not have one, and inventing a name would
    /// tell a debugger that a module it must not replace is safe to replace.
    pub entry_module: Option<String>,
    /// whether the tree's modules were compiled to native extensions
    ///
    /// A compiled module has no `__code__` to assign, so a tree with this set can
    /// never be re-staged a file at a time. The record says so rather than
    /// leaving a caller to discover it from a failed replacement.
    pub compiled: bool,
    pub config: ConfigRecord,
}

impl BuildRecord {
    /// Describe a build that is about to be written.
    pub fn new(
        project_root: &Path,
        module_roots: &[PathBuf],
        entry_module: Option<String>,
        compiled: bool,
        config: &Config,
    ) -> Self {
        Self {
            by_version: build_identity().to_owned(),
            project_root: project_root.to_path_buf(),
            module_roots: module_roots.to_vec(),
            entry_module,
            compiled,
            config: ConfigRecord {
                min_version: config.min_version.to_string(),
                soundness: spell_soundness(config.soundness),
                runtime_raises_checks: config.runtime_raises_checks,
                unique_loop_bindings: config.unique_loop_bindings,
            },
        }
    }

    /// The record as it is written into the tree: pretty-printed, newline
    /// terminated, so a person looking into a build directory can read it.
    pub fn render(&self) -> String {
        let mut rendered = serde_json::to_string_pretty(self)
            .expect("a record of strings and bools always serializes");
        rendered.push('\n');
        rendered
    }

    /// Read the record out of a build directory.
    pub fn read(build_directory: &Path) -> anyhow::Result<Self> {
        let path = build_directory.join(BY_BUILD_FILENAME);
        let contents = std::fs::read_to_string(&path).map_err(|error| {
            anyhow::anyhow!(
                "`{}` has no {BY_BUILD_FILENAME}, so nothing says which build it is: {error}",
                build_directory.display(),
            )
        })?;
        serde_json::from_str(&contents)
            .map_err(|error| anyhow::anyhow!("`{}` could not be read: {error}", path.display()))
    }

    /// The transpile config this build ran under, rebuilt from the record.
    ///
    /// Everything the record does not carry comes from [`Config::default`],
    /// which is where the build got it too.
    pub fn config(&self) -> anyhow::Result<Config> {
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
pub fn spell_soundness(positions: SoundnessPositions) -> String {
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
    use super::{BuildRecord, ConfigRecord, parse_soundness, spell_soundness};
    use by_transforms::SoundnessPositions;
    use by_transforms::config::Config;
    use std::path::{Path, PathBuf};

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
            &Config::default(),
        );
        let json: serde_json::Value =
            serde_json::from_str(&record.render()).expect("the record is plain data");

        assert!(json["byVersion"].as_str().is_some_and(|v| !v.is_empty()));
        assert_eq!(json["projectRoot"], "/p");
        assert_eq!(json["moduleRoots"][0], "/p/src");
        assert_eq!(json["entryModule"], "main");
        assert_eq!(json["compiled"], false);
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
        let record = BuildRecord::new(Path::new("/p"), &[], None, false, &Config::default());
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
            ..Config::default()
        };

        let record = BuildRecord::new(Path::new("/p"), &[], None, false, &config);
        let parsed: BuildRecord = serde_json::from_str(&record.render())?;
        let recovered = parsed.config()?;

        assert_eq!(recovered.soundness, soundness);
        assert!(recovered.runtime_raises_checks);
        assert!(!recovered.unique_loop_bindings);
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
            "byVersion": "x", "projectRoot": "/p", "moduleRoots": [],
            "entryModule": null, "compiled": false,
            "config": {"minVersion": "3.13", "soundness": "none",
                       "runtimeRaisesChecks": false, "uniqueLoopBindings": true,
                       "somethingNew": true}
        }"#;
        assert!(serde_json::from_str::<BuildRecord>(json).is_err());
    }

    #[test]
    fn a_version_the_record_names_that_this_by_cannot_target_is_refused() {
        let record = BuildRecord {
            by_version: "x".to_owned(),
            project_root: PathBuf::from("/p"),
            module_roots: Vec::new(),
            entry_module: None,
            compiled: false,
            config: ConfigRecord {
                min_version: "three point twelve".to_owned(),
                soundness: "none".to_owned(),
                runtime_raises_checks: false,
                unique_loop_bindings: true,
            },
        };
        assert!(record.config().is_err());
    }
}
