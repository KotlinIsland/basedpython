//! basedpython's version, as every surface of `by` reports it
//!
//! it is `project.version` in the root `pyproject.toml`, the version the wheel is published under,
//! read when this crate is built, so a build from a checkout, from the source distribution and
//! from the wheel all name the same version. it is spelled as PEP 440 spells it (`0.0.1b2`), the
//! way `pip`, `uv` and pypi show it, rather than the way cargo makes
//! `crates/basedpython/Cargo.toml` spell it (`0.0.1-b2`)

/// basedpython's version, such as `0.0.1b2`
pub const VERSION: &str = env!("BY_VERSION");

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::str::FromStr;

    use pep440_rs::Version;

    use super::VERSION;

    /// the release archives and the tag they are published under take their version from
    /// `crates/basedpython/Cargo.toml`, and the wheel and `by` take theirs from `pyproject.toml`,
    /// so a bump that misses one of them publishes two versions under one release
    #[test]
    fn the_distribution_crate_names_the_same_version() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("basedpython")
            .join("Cargo.toml");
        let manifest = std::fs::read_to_string(manifest)
            .expect("`crates/basedpython/Cargo.toml` is readable")
            .parse::<toml::Table>()
            .expect("`crates/basedpython/Cargo.toml` is toml");
        let cargo_version = manifest["package"]["version"]
            .as_str()
            .expect("`crates/basedpython/Cargo.toml` names a version");

        assert_eq!(
            Version::from_str(cargo_version),
            Version::from_str(VERSION),
            "`crates/basedpython/Cargo.toml` is at {cargo_version}, and `pyproject.toml` is at \
             {VERSION}"
        );
    }

    #[test]
    fn the_version_is_pep_440_normalised() {
        let version = Version::from_str(VERSION).expect("the version is PEP 440");
        assert_eq!(version.to_string(), VERSION);
    }
}
