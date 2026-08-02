pub mod names;
pub mod ranges;
pub mod traits;

pub use names::{NameError, SafeName};
pub use ranges::{RangeDecision, decide_range};
pub use traits::{EntryKind, FileIdentity, Storage, StorageError, StorageErrorKind};
