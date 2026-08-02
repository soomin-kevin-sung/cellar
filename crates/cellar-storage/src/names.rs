use std::fmt;

use thiserror::Error;

/// A single, exact filesystem component accepted at a storage-port boundary.
///
/// `SafeName` deliberately does not normalize Unicode or case: the catalog
/// must preserve the exact on-disk name. Platform adapters may impose stricter
/// rules before constructing one.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SafeName(String);

impl SafeName {
    pub fn parse(value: impl Into<String>) -> Result<Self, NameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(NameError::Empty);
        }
        if value == "." || value == ".." {
            return Err(NameError::Traversal);
        }
        if value.contains(['/', '\\']) {
            return Err(NameError::Separator);
        }
        if value.contains('\0') {
            return Err(NameError::Nul);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SafeName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Debug for SafeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("SafeName").field(&self.0).finish()
    }
}

impl fmt::Display for SafeName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NameError {
    #[error("a file name cannot be empty")]
    Empty,
    #[error("parent traversal is not a valid file name")]
    Traversal,
    #[error("a file name cannot contain path separators")]
    Separator,
    #[error("a file name cannot contain NUL")]
    Nul,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_is_one_exact_component() {
        for invalid in ["", ".", "..", "a/b", r"a\b", "a\0b"] {
            assert!(SafeName::parse(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(
            SafeName::parse("\u{C608}\u{C0B0} Final.txt")
                .unwrap()
                .as_str(),
            "\u{C608}\u{C0B0} Final.txt"
        );
    }
}
