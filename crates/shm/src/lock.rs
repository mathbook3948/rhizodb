use std::fs::File;
use std::io;

use crate::check_range;

/// How a byte-range lock is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    /// Any number of holders at once, and no exclusive holder.
    Shared,
    /// One holder alone.
    Exclusive,
}

/// Takes `kind` over `len` bytes of `file` from `offset`, waiting while another holder keeps
/// it out. On Unix, taking a lock over a range the caller holds converts it in one step.
pub fn lock(file: &File, offset: u64, len: u64, kind: Lock) -> io::Result<()> {
    check_range(offset, len)?;
    sys::lock(file, offset, len, kind, true).map(|_| ())
}

/// Takes `kind` over `len` bytes of `file` from `offset` without waiting, and returns whether
/// it got the lock. A failed try leaves every lock the caller held as it was.
pub fn try_lock(file: &File, offset: u64, len: u64, kind: Lock) -> io::Result<bool> {
    check_range(offset, len)?;
    sys::lock(file, offset, len, kind, false)
}

/// Releases the lock over `len` bytes of `file` from `offset`, which must be exactly the range
/// of one lock the caller took, since Windows releases no other range.
pub fn unlock(file: &File, offset: u64, len: u64) -> io::Result<()> {
    check_range(offset, len)?;
    sys::unlock(file, offset, len)
}

/// Turns the exclusive lock the caller holds over `len` bytes of `file` from `offset` into a
/// shared one, with no moment at which the caller holds neither, so no other process finds the
/// range free in between.
pub fn downgrade(file: &File, offset: u64, len: u64) -> io::Result<()> {
    check_range(offset, len)?;
    sys::downgrade(file, offset, len)
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::thread;
    use std::time::Duration;

    use super::Lock;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    const SET: libc::c_int = libc::F_OFD_SETLK;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const SET_WAIT: libc::c_int = libc::F_OFD_SETLKW;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const SET: libc::c_int = libc::F_SETLK;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const SET_WAIT: libc::c_int = libc::F_SETLKW;

    const CLASSIC: bool = SET == libc::F_SETLK;

    fn fcntl(
        file: &File,
        command: libc::c_int,
        offset: u64,
        len: u64,
        kind: libc::c_short,
    ) -> io::Result<()> {
        // SAFETY: `flock` is a plain C struct, for which all zero bytes are a valid value, and
        // open-file-description locks require `l_pid` to be zero.
        let mut range: libc::flock = unsafe { std::mem::zeroed() };
        range.l_type = kind;
        range.l_whence = libc::SEEK_SET as libc::c_short;
        let out_of_range =
            |_| io::Error::new(io::ErrorKind::InvalidInput, "the range passes off_t");
        range.l_start = libc::off_t::try_from(offset).map_err(out_of_range)?;
        range.l_len = libc::off_t::try_from(len).map_err(out_of_range)?;
        // SAFETY: the descriptor is open for as long as `file` is borrowed, and `range` is a
        // valid, initialized `flock` that outlives the call, which only reads it for these
        // commands.
        let outcome = unsafe { libc::fcntl(file.as_raw_fd(), command, &raw mut range) };
        if outcome == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn flag(kind: Lock) -> libc::c_short {
        (match kind {
            Lock::Shared => libc::F_RDLCK,
            Lock::Exclusive => libc::F_WRLCK,
        }) as libc::c_short
    }

    pub(super) fn lock(
        file: &File,
        offset: u64,
        len: u64,
        kind: Lock,
        wait: bool,
    ) -> io::Result<bool> {
        let mut backoff = Duration::from_micros(50);
        loop {
            let command = if wait { SET_WAIT } else { SET };
            match fcntl(file, command, offset, len, flag(kind)) {
                Ok(()) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error)
                    if !wait
                        && matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) =>
                {
                    return Ok(false);
                }
                // ast-grep-ignore
                // Classic locks detect deadlocks per process, so they report false ones between its threads.
                Err(error) if wait && CLASSIC && error.raw_os_error() == Some(libc::EDEADLK) => {
                    thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub(super) fn unlock(file: &File, offset: u64, len: u64) -> io::Result<()> {
        loop {
            match fcntl(file, SET, offset, len, libc::F_UNLCK as libc::c_short) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                outcome => return outcome,
            }
        }
    }

    pub(super) fn downgrade(file: &File, offset: u64, len: u64) -> io::Result<()> {
        lock(file, offset, len, Lock::Shared, false).and_then(|got| {
            if got {
                Ok(())
            } else {
                Err(io::Error::other("a downgrade had to wait"))
            }
        })
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::ERROR_IO_PENDING;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_FAIL_IMMEDIATELY;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    use super::Lock;

    fn overlapped(offset: u64) -> OVERLAPPED {
        let mut overlapped = OVERLAPPED::default();
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        overlapped
    }

    pub(super) fn lock(
        file: &File,
        offset: u64,
        len: u64,
        kind: Lock,
        wait: bool,
    ) -> io::Result<bool> {
        let mut flags = 0;
        if kind == Lock::Exclusive {
            flags |= LOCKFILE_EXCLUSIVE_LOCK;
        }
        if !wait {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }
        let mut overlapped = overlapped(offset);
        // SAFETY: the handle is open for as long as `file` is borrowed, and std opens files for
        // synchronous I/O, so the call returns only once the lock is taken or refused and
        // `overlapped` outlives every use the system makes of it.
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle() as HANDLE,
                flags,
                0,
                len as u32,
                (len >> 32) as u32,
                &raw mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_LOCK_VIOLATION | ERROR_IO_PENDING) if !wait => Ok(false),
            _ => Err(error),
        }
    }

    pub(super) fn unlock(file: &File, offset: u64, len: u64) -> io::Result<()> {
        let mut overlapped = overlapped(offset);
        // SAFETY: as in `lock`, the handle is open and the call is synchronous.
        let unlocked = unsafe {
            UnlockFileEx(
                file.as_raw_handle() as HANDLE,
                0,
                len as u32,
                (len >> 32) as u32,
                &raw mut overlapped,
            )
        };
        if unlocked == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn downgrade(file: &File, offset: u64, len: u64) -> io::Result<()> {
        // ast-grep-ignore
        // The first unlock of a range that a handle holds both ways releases the exclusive lock.
        if !lock(file, offset, len, Lock::Shared, false)? {
            return Err(io::Error::other("a downgrade had to wait"));
        }
        unlock(file, offset, len)
    }
}
