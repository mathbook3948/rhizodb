//! The operating-system calls that RhizoDB's multi-process sessions need and that no safe crate
//! covers: a shared mapping of a file seen only as atomic words, byte-range locks, growth of a
//! file with its blocks reserved, and whether a file lies on a local file system.
//!
//! Every lock here is one the operating system releases when its holder dies. On Linux the
//! locks are open-file-description locks, so closing another descriptor of the file drops none
//! of them. Elsewhere on Unix they are classic `fcntl` locks, which belong to the process: they
//! never conflict with another lock of the same process, and closing any descriptor of the file
//! drops every one of them, so the caller must keep one handle per file in a process.
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::io;

mod lock;
mod map;
mod space;

pub use lock::Lock;
pub use lock::downgrade;
pub use lock::lock;
pub use lock::try_lock;
pub use lock::unlock;
pub use map::Mapping;
pub use space::FileId;
pub use space::file_id;
pub use space::is_local;
pub use space::reserve;

fn check_range(offset: u64, len: u64) -> io::Result<()> {
    if len == 0
        || offset
            .checked_add(len)
            .is_none_or(|end| end > i64::MAX as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a lock covers at least one byte and ends before 2^63",
        ));
    }
    Ok(())
}
