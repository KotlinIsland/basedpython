use std::sync::Arc;

use ruff_db::system::{System, SystemPath, SystemPathBuf};
use thiserror::Error;

use ruff_ranged_value::ValueSource;

use super::options::{Options, TyTomlError};

/// The names of the standalone configuration files, in decreasing precedence.
///
/// Each holds the same options; a `basedpython.toml` is what a `[tool.basedpython]` section
/// contains, a `ty.toml` what a `[tool.ty]` section contains.
pub(crate) const CONFIG_FILE_NAMES: [&str; 2] = ["basedpython.toml", "ty.toml"];

/// A `basedpython.toml` or `ty.toml` configuration file with the options it contains.
pub(crate) struct ConfigurationFile {
    path: SystemPathBuf,
    options: Options,
}

impl ConfigurationFile {
    pub(crate) fn from_path(
        path: SystemPathBuf,
        system: &dyn System,
    ) -> Result<Self, ConfigurationFileError> {
        let config_str = system.read_to_string(&path).map_err(|source| {
            ConfigurationFileError::FileReadError {
                source,
                path: path.clone(),
            }
        })?;

        match Options::from_toml_str(&config_str, ValueSource::File(Arc::new(path.clone()))) {
            Ok(options) => Ok(Self { path, options }),
            Err(error) => Err(ConfigurationFileError::InvalidConfigFile {
                source: Box::new(error),
                path,
            }),
        }
    }

    /// Loads the user-level configuration file if it exists.
    ///
    /// Returns `None` if the file does not exist or if the concept of user-level configurations
    /// doesn't exist on `system`.
    pub(crate) fn user(system: &dyn System) -> Result<Option<Self>, ConfigurationFileError> {
        let Some(configuration_directory) = system.user_config_directory() else {
            return Ok(None);
        };

        // each file lives in the directory named after it: `ty/ty.toml`, `basedpython/basedpython.toml`
        let candidates = CONFIG_FILE_NAMES.iter().map(|name| {
            let directory = SystemPath::new(name).file_stem().unwrap_or(name);
            configuration_directory.join(directory).join(name)
        });

        for path in candidates {
            tracing::debug!("Searching for a user-level configuration at `{path}`");

            let Ok(config_str) = system.read_to_string(&path) else {
                continue;
            };

            return match Options::from_toml_str(
                &config_str,
                ValueSource::File(Arc::new(path.clone())),
            ) {
                Ok(options) => Ok(Some(Self { path, options })),
                Err(error) => Err(ConfigurationFileError::InvalidConfigFile {
                    source: Box::new(error),
                    path,
                }),
            };
        }

        Ok(None)
    }

    /// Returns the path to the configuration file.
    pub(crate) fn path(&self) -> &SystemPath {
        &self.path
    }

    pub(crate) fn into_options(self) -> Options {
        self.options
    }
}

#[derive(Debug, Error)]
pub enum ConfigurationFileError {
    #[error("{path} is not a valid configuration file")]
    InvalidConfigFile {
        source: Box<TyTomlError>,
        path: SystemPathBuf,
    },
    #[error("Failed to read `{path}`")]
    FileReadError {
        #[source]
        source: std::io::Error,
        path: SystemPathBuf,
    },
}
