//! Code for representing `by`'s release version number.
use serde::Serialize;
use std::fmt;

/// Information about the git repository where `by` was built from.
#[derive(Serialize)]
pub(crate) struct CommitInfo {
    short_commit_hash: String,
    commit_hash: String,
    commit_date: String,
}

/// `by`'s version.
#[derive(Serialize)]
pub(crate) struct VersionInfo {
    /// basedpython's version, such as "0.0.1b2"
    version: &'static str,
    /// Information about the git commit we may have been built from.
    ///
    /// `None` if not built from a git repo or if retrieval failed.
    commit_info: Option<CommitInfo>,
}

impl VersionInfo {
    /// basedpython's version alone, without the commit
    pub(crate) fn number(&self) -> &'static str {
        self.version
    }
}

impl fmt::Display for VersionInfo {
    /// Formatted version information: `<version>[ (<commit> <date>)]`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.version)?;

        if let Some(ref ci) = self.commit_info {
            write!(f, " ({} {})", ci.short_commit_hash, ci.commit_date)?;
        }

        Ok(())
    }
}

impl From<VersionInfo> for clap::builder::Str {
    fn from(val: VersionInfo) -> Self {
        val.to_string().into()
    }
}

/// Returns information about `by`'s version.
pub(crate) fn version() -> VersionInfo {
    // Commit info is pulled from git and set by `build.rs`
    let commit_info = match (
        option_env!("TY_COMMIT_HASH"),
        option_env!("TY_COMMIT_SHORT_HASH"),
        option_env!("TY_COMMIT_DATE"),
    ) {
        (Some(commit_hash), Some(short_commit_hash), Some(commit_date)) => Some(CommitInfo {
            short_commit_hash: short_commit_hash.to_string(),
            commit_hash: commit_hash.to_string(),
            commit_date: commit_date.to_string(),
        }),
        _ => None,
    };

    VersionInfo {
        version: by_version::VERSION,
        commit_info,
    }
}

#[cfg(test)]
mod tests {
    use insta::{assert_json_snapshot, assert_snapshot};

    use super::{CommitInfo, VersionInfo};

    #[test]
    fn version_formatting() {
        let version = VersionInfo {
            version: "0.0.0",
            commit_info: None,
        };
        assert_snapshot!(version, @"0.0.0");
    }

    #[test]
    fn version_formatting_with_commit_info() {
        let version = VersionInfo {
            version: "0.0.0",
            commit_info: Some(CommitInfo {
                short_commit_hash: "53b0f5d92".to_string(),
                commit_hash: "53b0f5d924110e5b26fbf09f6fd3a03d67b475b7".to_string(),
                commit_date: "2023-10-19".to_string(),
            }),
        };
        assert_snapshot!(version, @"0.0.0 (53b0f5d92 2023-10-19)");
    }

    #[test]
    fn version_serializable() {
        let version = VersionInfo {
            version: "0.0.0",
            commit_info: Some(CommitInfo {
                short_commit_hash: "53b0f5d92".to_string(),
                commit_hash: "53b0f5d924110e5b26fbf09f6fd3a03d67b475b7".to_string(),
                commit_date: "2023-10-19".to_string(),
            }),
        };
        assert_json_snapshot!(version);
    }
}
