//! Which text a document request is about, when the client says.
//!
//! A document request names a document by its URI, and the server answers about whatever it holds
//! for that URI when the request runs: the buffer the client last synchronised, or nothing at all
//! when the client has not opened it, which is refused. That is enough for a client that only asks
//! about documents it has opened and that sends every edit before the request that depends on it.
//!
//! It is not enough for a client that keeps an answer against a revision of its own. The server
//! cannot tell such a client which text an answer was for, so the client has to assume it was the
//! text it had when it asked, and there are two moments when that is not true: before the client's
//! `didOpen` for a document has arrived, and — for a document the client has not opened — while the
//! file on disk and the client's copy of it disagree. An answer about the wrong text, kept against
//! the client's revision, is wrong for as long as that revision lasts.
//!
//! So a client may name the text it means, by adding `textHash` to any document request's params:
//! the [`TextHash`] of the text the request is about. A request that names its text is answered
//! about that text and no other —
//!
//!  - at once, if the text the server has for the document is that text: the open buffer, or, for
//!    a handler that can answer a document that is not open, the file as the server reads it;
//!  - otherwise once the server has it, which is the next time the session changes to hold it: a
//!    `didOpen`, a `didChange`, or the file system reporting the file written. Until then the
//!    request waits, and it is answered with `ServerCancelled` if it waits longer than
//!    `HOLD_LIMIT` — a client that stops waiting sooner cancels it, as any request is cancelled.
//!
//! A request that names no text is answered as it always was.

use std::fmt;

/// The params field a client names its text in.
const TEXT_HASH_FIELD: &str = "textHash";

/// A hash of a document's text, as client and server both compute it.
///
/// FNV-1a, 64 bits, over the text's UTF-16 code units, with each line ending counted as a single
/// `\n`, sent as sixteen lowercase hex digits. UTF-16 because that is what an editor's text is
/// likely to be made of already, so a client hashes what it holds without converting it. Line
/// endings are counted alike because they are the one way a file on disk and an editor's copy of it
/// routinely differ while meaning the same positions: an LSP position counts lines and the
/// characters within one, and `\r\n`, `\r` and `\n` each end a line.
///
/// A byte order mark is *not* skipped. An editor drops it from its copy, and a server reading the
/// file keeps it, and the mark is a character on the first line — so the two do not name the same
/// positions, and should not hash alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextHash(u64);

impl TextHash {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    /// The hash of `text`.
    pub(crate) fn of(text: &str) -> Self {
        let mut hash = Self::OFFSET_BASIS;
        let mut after_carriage_return = false;
        for unit in text.encode_utf16() {
            let unit = match unit {
                // the `\n` of a `\r\n` has been counted already, as the `\r`
                0x0A if after_carriage_return => {
                    after_carriage_return = false;
                    continue;
                }
                0x0D => {
                    after_carriage_return = true;
                    0x0A
                }
                unit => {
                    after_carriage_return = false;
                    unit
                }
            };
            for byte in unit.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(Self::PRIME);
            }
        }
        Self(hash)
    }

    /// Reads the hash a client wrote: sixteen hex digits.
    fn parse(value: &str) -> Option<Self> {
        if value.len() != 16 {
            return None;
        }
        u64::from_str_radix(value, 16).ok().map(Self)
    }

    /// Takes the named text out of a request's params, so that the handler reads the params it
    /// has always read.
    ///
    /// # Errors
    ///
    /// When the field is there but holds no hash — a client that meant to name its text and
    /// cannot be answered as though it had not.
    pub(crate) fn take_from(params: &mut serde_json::Value) -> anyhow::Result<Option<Self>> {
        let Some(value) = params
            .as_object_mut()
            .and_then(|params| params.remove(TEXT_HASH_FIELD))
        else {
            return Ok(None);
        };
        value
            .as_str()
            .and_then(Self::parse)
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("`{TEXT_HASH_FIELD}` is not a text hash: {value}"))
    }
}

impl fmt::Display for TextHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::TextHash;

    #[test]
    fn line_endings_hash_alike() {
        let unix = TextHash::of("a\nb\n");
        assert_eq!(TextHash::of("a\r\nb\r\n"), unix);
        assert_eq!(TextHash::of("a\rb\r"), unix);
        assert_ne!(TextHash::of("a\n\nb\n"), unix);
        // `\r\n\n` is two line endings, not one
        assert_eq!(TextHash::of("a\r\n\nb"), TextHash::of("a\n\nb"));
    }

    #[test]
    fn a_byte_order_mark_is_part_of_the_text() {
        assert_ne!(TextHash::of("\u{feff}x = 1\n"), TextHash::of("x = 1\n"));
    }

    /// The values a client's implementation is checked against, so that the two are held to the
    /// same function rather than to each other's bugs.
    #[test]
    fn known_values() {
        assert_eq!(TextHash::of("").to_string(), "cbf29ce484222325");
        assert_eq!(TextHash::of("a").to_string(), "089be207b544f1e4");
        // a character outside the basic plane is its two surrogates, as an editor holds it
        assert_eq!(TextHash::of("é𝄞\n").to_string(), "3a3901ff4b1faf17");
        assert_eq!(TextHash::of("é𝄞\r\n").to_string(), "3a3901ff4b1faf17");
    }

    #[test]
    fn a_hash_reads_back() {
        let hash = TextHash::of("def f():\n    pass\n");
        assert_eq!(TextHash::parse(&hash.to_string()), Some(hash));
        assert_eq!(TextHash::parse("xyz"), None);
        assert_eq!(TextHash::parse("0123456789abcdef0"), None);
    }
}
