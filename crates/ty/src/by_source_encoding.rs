//! reading a python source that is not utf-8
//!
//! [PEP 263] lets a python file declare its own encoding in a comment on one of
//! its first two lines, so a source that is not valid utf-8 can still be a
//! perfectly ordinary module. everything downstream of this — the parser, the
//! transpiler, `ruff_db`'s `source_text` — is documented to take utf-8, so a file
//! like that is decoded here, once, on the way in.
//!
//! what comes out is utf-8, which means the declaration the file carried is no
//! longer true of it. the cookie is a *comment*, and the reverse transform copies
//! comments through verbatim, so leaving it alone would produce a `.by` (and, on
//! the way back, a `.py`) that says `latin-1` while holding utf-8 bytes — mojibake
//! for anyone who read it. so the declaration is rewritten to name the encoding
//! the file now actually has.
//!
//! only the encodings that turn up in practice are decodable here: utf-8 and the
//! single-byte latin family, which between them cover the coding cookies real
//! projects carry. anything else is refused by name rather than guessed at, since
//! a wrong guess is silent corruption. a general answer is a codec table this
//! workspace has no dependency for.
//!
//! [PEP 263]: https://peps.python.org/pep-0263/

use ruff_python_trivia::encoding_declaration;

/// a python source decoded to utf-8, and whether decoding had to rewrite it.
#[derive(Debug)]
pub(crate) struct DecodedSource {
    pub(crate) text: String,

    /// the encoding the file declared, when that was not already utf-8. the
    /// caller reports it, because the file on disk no longer says what it says.
    pub(crate) recoded_from: Option<String>,
}

/// decode `bytes` as python source.
///
/// utf-8 is tried first whatever the file says, because a cookie naming another
/// encoding on a file that is valid utf-8 anyway is far more likely to be stale
/// than to be a file that means two things at once.
pub(crate) fn decode(bytes: &[u8]) -> Result<DecodedSource, String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(DecodedSource {
            text: text.to_owned(),
            recoded_from: None,
        });
    }

    let Some(declaration) = encoding_declaration(bytes) else {
        return Err(
            "not valid utf-8, and no PEP 263 encoding declaration on its first two lines"
                .to_owned(),
        );
    };

    let Some(text) = decode_single_byte(bytes, declaration.name) else {
        return Err(format!(
            "declares `{}`, which this build cannot decode (only utf-8 and the latin family)",
            declaration.name
        ));
    };

    Ok(DecodedSource {
        text: rewrite_declaration(text),
        recoded_from: Some(declaration.name.to_owned()),
    })
}

/// decode a single-byte encoding whose code points are its byte values, which is
/// what latin-1 is by construction.
///
/// the aliases python itself accepts for it are taken, normalised the way python's
/// codec lookup normalises them (case-folded, `_` and spaces as `-`).
fn decode_single_byte(bytes: &[u8], encoding: &str) -> Option<String> {
    let normalized: String = encoding
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            '_' | ' ' => '-',
            c => c,
        })
        .collect();
    matches!(
        normalized.as_str(),
        "latin-1" | "latin1" | "latin" | "l1" | "iso-8859-1" | "iso8859-1" | "8859" | "cp819"
    )
    .then(|| bytes.iter().map(|byte| char::from(*byte)).collect())
}

/// rewrite the encoding a decoded source declares to `utf-8`, which is what it
/// now holds.
///
/// only the name is replaced, so the shape the file chose — `# -*- coding: X -*-`,
/// `# coding=X` — survives.
fn rewrite_declaration(mut text: String) -> String {
    if let Some(declaration) = encoding_declaration(text.as_bytes()) {
        let range = declaration.name_range.to_std_range();
        text.replace_range(range, "utf-8");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn utf8_passes_through() {
        let decoded = decode("x = 'é'\n".as_bytes()).unwrap();
        assert_eq!(decoded.text, "x = 'é'\n");
        assert!(decoded.recoded_from.is_none());
    }

    #[test]
    fn a_declared_latin_1_source_decodes() {
        let decoded = decode(b"# -*- coding: latin-1 -*-\ns = '\xdf'\n").unwrap();
        assert_eq!(decoded.text, "# -*- coding: utf-8 -*-\ns = 'ß'\n");
        assert_eq!(decoded.recoded_from.as_deref(), Some("latin-1"));
    }

    #[test]
    fn the_declaration_may_be_on_the_second_line() {
        let decoded = decode(b"#!/usr/bin/env python\n# coding=iso-8859-1\ns = '\xe9'\n").unwrap();
        assert_eq!(
            decoded.text,
            "#!/usr/bin/env python\n# coding=utf-8\ns = 'é'\n"
        );
    }

    // a cookie on line 2 of a file whose line 1 is code is not a declaration
    #[test]
    fn a_declaration_below_code_does_not_count() {
        let error = decode(b"s = '\xe9'\n# coding: latin-1\n").unwrap_err();
        assert!(error.contains("no PEP 263"), "{error}");
    }

    // python reads a declaration only from a comment that is the whole line
    #[test]
    fn a_declaration_after_code_on_its_line_does_not_count() {
        let error = decode(b"s = '\xe9'  # coding: latin-1\n").unwrap_err();
        assert!(error.contains("no PEP 263"), "{error}");
    }

    // `coding` not followed by `:` or `=` is skipped, and a later one is read
    #[test]
    fn the_declaration_is_any_coding_the_comment_spells_out() {
        let decoded = decode(b"# the coding, coding: latin-1\ns = '\xdf'\n").unwrap();
        assert_eq!(decoded.text, "# the coding, coding: utf-8\ns = 'ß'\n");
    }

    // a non-ascii character ahead of the declaration moves it in the decoded text
    #[test]
    fn the_declaration_is_rewritten_where_the_decoded_text_has_it() {
        let decoded = decode(b"# \xe9 coding: latin-1\ns = '\xdf'\n").unwrap();
        assert_eq!(decoded.text, "# é coding: utf-8\ns = 'ß'\n");
    }

    #[test]
    fn an_encoding_this_build_cannot_decode_is_refused_by_name() {
        let error = decode(b"# coding: shift_jis\ns = '\x82\xa0'\n").unwrap_err();
        assert!(error.contains("shift_jis"), "{error}");
    }

    // a source that is valid utf-8 is read as utf-8 whatever it claims, so a
    // stale cookie on an already-converted file changes nothing
    #[test]
    fn a_stale_declaration_on_a_utf8_source_is_left_alone() {
        let decoded = decode("# -*- coding: latin-1 -*-\ns = 'ß'\n".as_bytes()).unwrap();
        assert_eq!(decoded.text, "# -*- coding: latin-1 -*-\ns = 'ß'\n");
        assert!(decoded.recoded_from.is_none());
    }
}
