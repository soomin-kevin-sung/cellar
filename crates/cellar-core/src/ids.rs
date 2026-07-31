use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::{Uuid, Version};

/// An error returned when text does not contain a valid UUID-v7 identifier.
#[derive(Debug, Error)]
pub enum ParseIdError {
    #[error("invalid UUID syntax")]
    InvalidUuid(#[source] uuid::Error),
    #[error("identifier UUID must be version 7")]
    WrongVersion,
}

macro_rules! define_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates a new UUID-v7 identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{}", self.0.hyphenated())
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                let uuid = Uuid::parse_str(value).map_err(ParseIdError::InvalidUuid)?;
                if uuid.get_version() != Some(Version::SortRand) {
                    return Err(ParseIdError::WrongVersion);
                }

                Ok(Self(uuid))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                value.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(ProjectId);
define_id!(FileEntryId);
define_id!(UploadId);
define_id!(OperationId);
define_id!(TrashId);
