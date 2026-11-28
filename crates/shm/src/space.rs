use std::fs::File;
use std::io;

/// Which file a handle refers to, the same for every path and hard link that reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId {
    /// The device on Unix, or the volume serial number on Windows.
    pub volume: u64,
    /// The inode on Unix, or the 128-bit file id of `FILE_ID_INFO` on Windows.
    pub file: u128,
}

/// Grows `file` to at least `len` bytes with every block up to `len` reserved, so a later store
/// into a mapping of that range never finds the disk full. It never shrinks the file, and it
/// fails with [`io::ErrorKind::Unsupported`] where the file system cannot reserve blocks. The
/// bytes it adds read as zeros on Unix and as a non-zero pattern on Windows, so a caller writes
/// every byte it reads.
pub fn reserve(file: &File, len: u64) -> io::Result<()> {
    if file.metadata()?.len() >= len {
        return Ok(());
    }
    sys::reserve(file, len)
}

/// Whether `file` lies on a file system of this machine, whose shared mappings and byte-range
/// locks every process that opens the file shares. Only file systems known to be local count:
/// a network file system, any FUSE mount, an unknown type, and a failed query do not.
pub fn is_local(file: &File) -> io::Result<bool> {
    sys::is_local(file)
}

/// The identity of the file `file` refers to.
pub fn file_id(file: &File) -> io::Result<FileId> {
    sys::file_id(file)
}

#[cfg(unix)]
fn unix_file_id(file: &File) -> io::Result<FileId> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileId {
        volume: metadata.dev(),
        file: u128::from(metadata.ino()),
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys {
    use std::fs::File;
    use std::io;

    use rustix::fs::FallocateFlags;

    pub(super) use super::unix_file_id as file_id;

    const EXT: u32 = 0xEF53;
    const XFS: u32 = 0x5846_5342;
    const BTRFS: u32 = 0x9123_683E;
    const TMPFS: u32 = 0x0102_1994;
    const RAMFS: u32 = 0x8584_58F6;
    const ZFS: u32 = 0x2FC1_2FC1;
    const F2FS: u32 = 0xF2F5_2010;
    const BCACHEFS: u32 = 0xCA45_1A4E;
    const JFS: u32 = 0x3153_464A;
    const REISERFS: u32 = 0x5265_4973;
    const NILFS: u32 = 0x0000_3434;
    const OVERLAYFS: u32 = 0x794C_7630;
    const LOCAL: [u32; 12] = [
        EXT, XFS, BTRFS, TMPFS, RAMFS, ZFS, F2FS, BCACHEFS, JFS, REISERFS, NILFS, OVERLAYFS,
    ];

    pub(super) fn reserve(file: &File, len: u64) -> io::Result<()> {
        match rustix::fs::fallocate(file, FallocateFlags::empty(), 0, len) {
            Err(rustix::io::Errno::OPNOTSUPP) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the file system cannot reserve blocks for a file",
            )),
            outcome => Ok(outcome?),
        }
    }

    pub(super) fn is_local(file: &File) -> io::Result<bool> {
        let kind = rustix::fs::fstatfs(file)?.f_type as u32;
        Ok(LOCAL.contains(&kind))
    }
}

#[cfg(target_vendor = "apple")]
mod sys {
    use std::fs::File;
    use std::io;

    use rustix::fs::FallocateFlags;

    pub(super) use super::unix_file_id as file_id;

    pub(super) fn reserve(file: &File, len: u64) -> io::Result<()> {
        match rustix::fs::fallocate(file, FallocateFlags::empty(), 0, len) {
            Err(rustix::io::Errno::NOTSUP | rustix::io::Errno::INVAL) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the file system cannot reserve blocks for a file",
            )),
            outcome => Ok(outcome?),
        }
    }

    pub(super) fn is_local(file: &File) -> io::Result<bool> {
        let stat = rustix::fs::fstatfs(file)?;
        let kind: Vec<u8> = stat
            .f_fstypename
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        let fuse = [&b"macfuse"[..], b"osxfuse", b"fuse"]
            .iter()
            .any(|name| kind.starts_with(name));
        Ok(stat.f_flags & libc::MNT_LOCAL as u32 != 0 && !fuse)
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
mod sys {
    use std::fs::File;
    use std::io;

    pub(super) use super::unix_file_id as file_id;

    pub(super) fn reserve(_: &File, _: u64) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "reserving blocks is not implemented on this platform",
        ))
    }

    pub(super) fn is_local(_: &File) -> io::Result<bool> {
        Ok(false)
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::windows::fs::FileExt;
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_REMOTE_PROTOCOL_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::FileRemoteProtocolInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationByHandleW;

    use super::FileId;

    const PATTERN: [u8; 4096] = [0xA5; 4096];

    pub(super) fn reserve(file: &File, len: u64) -> io::Result<()> {
        // ast-grep-ignore
        // A length set on Windows reserves no clusters, and zeros reserve none when compressed.
        let mut at = file.metadata()?.len();
        while at < len {
            let chunk = PATTERN
                .len()
                .min(usize::try_from(len - at).unwrap_or(usize::MAX));
            let written = file.seek_write(&PATTERN[..chunk], at)?;
            at += written as u64;
        }
        file.sync_data()
    }

    // ast-grep-ignore
    // Callers promise that `T` is the plain C struct `class` fills, valid as all zero bytes.
    unsafe fn information<T>(file: &File, class: i32) -> Result<T, io::Error> {
        // SAFETY: the caller promises that all zero bytes are a valid `T`.
        let mut value: T = unsafe { std::mem::zeroed() };
        // SAFETY: the handle is open for as long as `file` is borrowed, and the buffer is a
        // `T` of the size passed, which the caller promises is what `class` writes.
        let filled = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                class,
                (&raw mut value).cast(),
                size_of::<T>() as u32,
            )
        };
        if filled == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }

    pub(super) fn is_local(file: &File) -> io::Result<bool> {
        // SAFETY: `FILE_REMOTE_PROTOCOL_INFO` is the plain C struct that this class fills.
        let remote =
            unsafe { information::<FILE_REMOTE_PROTOCOL_INFO>(file, FileRemoteProtocolInfo) };
        match remote {
            Err(error) if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {}
            _ => return Ok(false),
        }
        let mut name = [0u16; 64];
        // SAFETY: the handle is open for as long as `file` is borrowed, `name` is a writable
        // buffer of the length passed, and the other out pointers are null, which the call
        // permits for information the caller does not want.
        let named = unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle() as HANDLE,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if named == 0 {
            return Ok(false);
        }
        let end = name
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(name.len());
        let kind = String::from_utf16_lossy(&name[..end]).to_ascii_uppercase();
        Ok(["NTFS", "REFS", "FAT", "FAT32", "EXFAT"].contains(&kind.as_str()))
    }

    pub(super) fn file_id(file: &File) -> io::Result<FileId> {
        // SAFETY: `FILE_ID_INFO` is the plain C struct that this class fills.
        let info = unsafe { information::<FILE_ID_INFO>(file, FileIdInfo) }?;
        Ok(FileId {
            volume: info.VolumeSerialNumber,
            file: u128::from_le_bytes(info.FileId.Identifier),
        })
    }
}
