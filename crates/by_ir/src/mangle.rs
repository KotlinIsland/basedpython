//! how a python name becomes part of a C identifier
//!
//! every emitted symbol is literal text and names spliced together — `By_<module>_<class>`,
//! `by_<module>_<class>_<method>`, `<type>_get_<field>` — and two different programs must
//! never splice to one symbol, or the C does not build. a name may hold any character
//! python allows in an identifier, and a generated one a `$` or a `<` besides, where a C
//! identifier holds only ASCII letters, digits and `_`. replacing each of the rest with `_`
//! sent `é1` and `_1` to one symbol, and spliced with `_` between, a function `A_b` and the
//! method `b` of a class `A` arrived at one symbol too
//!
//! so a name is spelled in two layers. the first maps it onto the characters a C
//! identifier may hold and can be read back: a letter, a digit or `_` stands for itself, and
//! anything else, `Z` itself included, is a `Z` and the character's code point in lowercase
//! hex closed by `_`; `ZZ` is a literal `Z`. a leading digit is spelled the long way too, so
//! the spelling never begins with one. the second layer writes the spelling's length in
//! front of it, which is what makes a mangled name end where it says: whatever follows it
//! in a symbol, a literal or another mangled name, cannot be read as part of it. a symbol's
//! literal text never puts a digit where a mangled name begins, so a symbol is read back
//! one way only

use std::fmt::Write as _;

/// the escape that introduces a character spelled by its code point
const ESCAPE: char = 'Z';

/// `name` as part of a C identifier, distinct from every other name's and ending where it
/// says — see the module documentation
pub fn mangle(name: &str) -> String {
    let spelled = spell(name);
    format!("{}{spelled}", spelled.len())
}

/// the first layer: `name` in the characters a C identifier may hold, never beginning with
/// a digit
fn spell(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (at, character) in name.chars().enumerate() {
        let plain = character != ESCAPE
            && (character.is_ascii_alphabetic()
                || character == '_'
                || (character.is_ascii_digit() && at > 0));
        if plain {
            out.push(character);
        } else if character == ESCAPE {
            out.push(ESCAPE);
            out.push(ESCAPE);
        } else {
            let _ = write!(out, "{ESCAPE}{:x}_", u32::from(character));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{ESCAPE, mangle};

    /// read one mangled name off the front of `text`, handing back the name and what follows
    /// it — the inverse that proves [`mangle`] loses nothing
    fn unmangle(text: &str) -> Option<(String, &str)> {
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        let length: usize = text.get(..digits)?.parse().ok()?;
        let spelled = text.get(digits..digits + length)?;
        let rest = text.get(digits + length..)?;
        let mut name = String::new();
        let mut characters = spelled.chars();
        while let Some(character) = characters.next() {
            if character != ESCAPE {
                name.push(character);
                continue;
            }
            match characters.next()? {
                ESCAPE => name.push(ESCAPE),
                first => {
                    let mut hex = String::from(first);
                    loop {
                        match characters.next()? {
                            '_' => break,
                            digit => hex.push(digit),
                        }
                    }
                    name.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
            }
        }
        Some((name, rest))
    }

    /// names that collided, or would have, under some way of spelling them
    const ADVERSARIAL: &[&str] = &[
        "é1",
        "_1",
        "_",
        "__",
        "Z",
        "ZZ",
        "Z5a_",
        "Zz",
        "z",
        "a_b",
        "a",
        "b",
        "A_b",
        "value$set",
        "value_set",
        "$state",
        "_state",
        "<lambda>",
        "_lambda_",
        "Foo_Type",
        "Foo",
        "1",
        "11",
        "Z31_",
        "a.b",
        "a_b.c",
        "ü",
        "u",
        "日本",
        "x\u{0}y",
        "𝔘nicode",
        "",
    ];

    #[test]
    fn every_name_reads_back_as_itself() {
        for name in ADVERSARIAL {
            let mangled = mangle(name);
            assert!(
                mangled
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{name:?} became {mangled:?}, which is not C"
            );
            assert_eq!(
                unmangle(&mangled),
                Some(((*name).to_string(), "")),
                "{name:?} became {mangled:?}"
            );
        }
    }

    /// a left inverse is injectivity, and this enumerates every name up to four characters
    /// over an alphabet holding each kind of character the spelling treats apart
    #[test]
    fn no_two_names_mangle_alike() {
        let alphabet = ['a', 'Z', 'z', '_', '1', 'é', '$', '.', '5'];
        let mut names = vec![String::new()];
        let mut frontier = vec![String::new()];
        for _ in 0..4 {
            frontier = frontier
                .iter()
                .flat_map(|prefix| alphabet.iter().map(move |c| format!("{prefix}{c}")))
                .collect();
            names.extend(frontier.iter().cloned());
        }
        let mut seen: HashMap<String, &str> = HashMap::new();
        for name in &names {
            let mangled = mangle(name);
            if let Some(other) = seen.insert(mangled.clone(), name) {
                panic!("{other:?} and {name:?} both became {mangled:?}");
            }
            assert_eq!(unmangle(&mangled), Some((name.clone(), "")));
        }
    }

    /// a mangled name ends where it says, so names spliced together with literal text
    /// between them are read back as the same names — which is what keeps
    /// `by_<module>_<function>` apart from `by_<module>_<class>_<method>`
    #[test]
    fn spliced_names_read_back_apart() {
        for left in ADVERSARIAL {
            for right in ADVERSARIAL {
                let spliced = format!("{}_{}_Type", mangle(left), mangle(right));
                let (first, rest) = unmangle(&spliced).expect("the first name reads back");
                let rest = rest.strip_prefix('_').expect("the separator follows it");
                let (second, rest) = unmangle(rest).expect("the second name reads back");
                assert_eq!(
                    (first.as_str(), second.as_str(), rest),
                    (*left, *right, "_Type")
                );
            }
        }
        assert_ne!(
            format!("by_{}_{}", mangle("m"), mangle("A_b")),
            format!("by_{}_{}_{}", mangle("m"), mangle("A"), mangle("b")),
        );
        assert_ne!(
            format!("By_{}_{}", mangle("m"), mangle("Foo_Type")),
            format!("By_{}_{}_Type", mangle("m"), mangle("Foo")),
        );
    }
}
