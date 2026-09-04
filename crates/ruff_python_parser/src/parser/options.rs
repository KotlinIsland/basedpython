use ruff_python_ast::{PySourceType, PythonVersion};

use crate::{AsMode, Mode};

/// Options for controlling how a source file is parsed.
///
/// You can construct a [`ParseOptions`] directly from a [`Mode`]:
///
/// ```
/// use ruff_python_parser::{Mode, ParseOptions};
///
/// let options = ParseOptions::from(Mode::Module);
/// ```
///
/// or from a [`PySourceType`]
///
/// ```
/// use ruff_python_ast::PySourceType;
/// use ruff_python_parser::ParseOptions;
///
/// let options = ParseOptions::from(PySourceType::Python);
/// ```
#[derive(Clone, Debug)]
pub struct ParseOptions {
    /// Specify the mode in which the code will be parsed.
    pub(crate) mode: Mode,
    /// Target version for detecting version-related syntax errors.
    pub(crate) target_version: PythonVersion,
    /// When true, basedpython-specific syntax is accepted without errors.
    /// When false (default), basedpython syntax is a parse error.
    pub(crate) is_basedpython: bool,
}

impl ParseOptions {
    /// Marks the source as a basedpython (`.by`) file, enabling basedpython-specific syntax.
    #[must_use]
    pub fn with_basedpython(mut self, is_basedpython: bool) -> Self {
        self.is_basedpython = is_basedpython;
        self
    }

    #[must_use]
    pub fn with_target_version(mut self, target_version: PythonVersion) -> Self {
        self.target_version = target_version;
        self
    }

    pub fn target_version(&self) -> PythonVersion {
        self.target_version
    }
}

impl From<Mode> for ParseOptions {
    fn from(mode: Mode) -> Self {
        Self {
            mode,
            target_version: PythonVersion::default(),
            is_basedpython: false,
        }
    }
}

impl From<PySourceType> for ParseOptions {
    fn from(source_type: PySourceType) -> Self {
        Self {
            mode: source_type.as_mode(),
            target_version: PythonVersion::default(),
            is_basedpython: source_type.is_basedpython(),
        }
    }
}
