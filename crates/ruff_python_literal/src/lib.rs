pub mod cformat;
mod char;
pub mod escape;
pub mod float;
pub mod format;
pub mod mini_language;
pub mod strftime;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Case {
    Lower,
    Upper,
}
