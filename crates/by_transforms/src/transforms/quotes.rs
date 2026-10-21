//! the forward references the transpiler writes as strings
//!
//! python evaluates such a string against the module's namespace whenever something asks for
//! the type, so the lazy-import polyfill binds every name one reads where the name is
//! imported. writing a quote is a capability: only a holder of [`WrittenQuotes`] can write
//! one, as text ([`WrittenQuotes::quote`]) or as a passthrough of source the driver lowers in
//! place ([`WrittenQuotes::quote_span`]), and each is recorded as it is written. code that only
//! picks names never holds one

use std::cell::RefCell;

use ruff_text_size::TextRange;

use super::ast_driver::Fragment;
use super::source_util::python_string_literal;

/// every forward reference the module's lowerings write as a string, by the type expression
/// inside the quotes
#[derive(Debug, Default)]
pub(crate) struct WrittenQuotes {
    texts: RefCell<Vec<String>>,
}

impl WrittenQuotes {
    /// `ty`, a lowered type expression, written as the forward reference python reads lazily
    pub(crate) fn quote(&self, ty: &str) -> String {
        self.record(ty);
        python_string_literal(ty)
    }

    /// `range` written as a string, with whatever lowers inside it lowered too. the driver
    /// records what it lowered to once it has written it
    #[expect(
        clippy::unused_self,
        reason = "taking the registry is what makes writing a quote a capability of its holder"
    )]
    pub(crate) fn quote_span(&self, range: TextRange) -> Fragment {
        Fragment::Quote(QuotedSpan { range })
    }

    /// record `ty`, the type expression a quote the driver wrote holds
    pub(super) fn record(&self, ty: &str) {
        self.texts.borrow_mut().push(ty.to_owned());
    }

    /// the type expressions the recorded quotes hold
    pub(crate) fn texts(&self) -> Vec<String> {
        self.texts.borrow().clone()
    }
}

/// the source a [`Fragment::Quote`] writes as a string, which only
/// [`WrittenQuotes::quote_span`] makes
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuotedSpan {
    range: TextRange,
}

impl QuotedSpan {
    pub(crate) fn range(self) -> TextRange {
        self.range
    }
}
