pub mod names;
pub mod traits;

pub use names::{NameError, SafeName};
pub use traits::{EntryKind, FileIdentity, Storage, StorageError, StorageErrorKind};
