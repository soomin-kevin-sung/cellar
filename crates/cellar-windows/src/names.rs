use std::fmt;

use cellar_storage::{NameError, SafeName};
use thiserror::Error;

/// A Windows-safe filename component, preserving the caller's exact spelling.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowsName(SafeName);

impl WindowsName {
    pub fn parse(value: impl Into<String>) -> Result<Self, WindowsNameError> {
        let safe = SafeName::parse(value).map_err(WindowsNameError::Component)?;
        let value = safe.as_str();
        if value.encode_utf16().count() > 255 {
            return Err(WindowsNameError::TooLong);
        }
        if value.ends_with(['.', ' ']) {
            return Err(WindowsNameError::TrailingDotOrSpace);
        }
        if value.chars().any(|character| {
            character <= '\u{1f}' || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
        }) {
            return Err(WindowsNameError::ForbiddenCharacter);
        }
        let stem = value.split('.').next().unwrap_or(value);
        if is_reserved_device(stem) {
            return Err(WindowsNameError::ReservedDevice);
        }
        Ok(Self(safe))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn as_safe_name(&self) -> &SafeName {
        &self.0
    }

    pub fn into_safe_name(self) -> SafeName {
        self.0
    }
}

impl AsRef<SafeName> for WindowsName {
    fn as_ref(&self) -> &SafeName {
        self.as_safe_name()
    }
}

impl fmt::Debug for WindowsName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("WindowsName").field(&self.0).finish()
    }
}

impl fmt::Display for WindowsName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum WindowsNameError {
    #[error(transparent)]
    Component(#[from] NameError),
    #[error("the name contains a character forbidden by Windows")]
    ForbiddenCharacter,
    #[error("the name is a reserved Windows device name")]
    ReservedDevice,
    #[error("the name ends in a dot or space")]
    TrailingDotOrSpace,
    #[error("the name exceeds the Windows component limit")]
    TooLong,
}

fn is_reserved_device(stem: &str) -> bool {
    let upper = stem.to_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$") {
        return true;
    }
    let Some(suffix) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    matches!(
        suffix,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{00B9}" | "\u{00B2}" | "\u{00B3}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catches_reserved_devices_with_extensions() {
        for invalid in [
            "nul",
            "Con.txt",
            "COM\u{00B9}.log",
            "LPT\u{00B2}",
            "COM\u{00B3}",
            "lpt3.data",
            "CLOCK$",
        ] {
            assert_eq!(
                WindowsName::parse(invalid).unwrap_err(),
                WindowsNameError::ReservedDevice
            );
        }
    }
}
