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
/// fails with [`io::ErrorKind::Unsupported`] where the file system cannot reserve blocks.
pub fn reserve(file: &File, len: u64) -> io::Result<()> {
    if file.metadata()?.len() >= len {
        return Ok(());
    }
    sys::reserve(file, len)
}

/// Whether `file` lies on a file system of this machine, whose shared mappings and byte-range
/// locks every process that opens the file shares. A network file system and any FUSE mount
/// count as not local.
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

    const NFS: u32 = 0x6969;
    const SMB: u32 = 0x517B;
    const CIFS: u32 = 0xFF53_4D42;
    const SMB2: u32 = 0xFE53_4D42;
    const NINE_P: u32 = 0x0102_1997;
    const CEPH: u32 = 0x00C3_6400;
    const AFS: u32 = 0x5346_414F;
    const KAFS: u32 = 0x6B41_4653;
    const FUSE: u32 = 0x6573_5546;
    const CODA: u32 = 0x7375_7245;
    const NCP: u32 = 0x0000_564C;
    const LUSTRE: u32 = 0x0BD0_0BD0;
    const GFS2: u32 = 0x0116_1970;
    const OCFS2: u32 = 0x7461_636F;
    const SHARED_OR_REMOTE: [u32; 14] = [
        NFS, SMB, CIFS, SMB2, NINE_P, CEPH, AFS, KAFS, FUSE, CODA, NCP, LUSTRE, GFS2, OCFS2,
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
        Ok(!SHARED_OR_REMOTE.contains(&kind))
    }
}

#[cfg(target_vendor = "apple")]
mod sys {
    use std::fs::File;
    use std::io;

    use rustix::fs::FallocateFlags;

    pub(super) use super::unix_file_id as file_id;

    pub(super) fn reserve(file: &File, len: u64) -> io::Result<()> {
        Ok(rustix::fs::fallocate(
            file,
            FallocateFlags::empty(),
            0,
            len,
        )?)
    }

    pub(super) fn is_local(file: &File) -> io::Result<bool> {
        let flags = rustix::fs::fstatfs(file)?.f_flags;
        Ok(flags & libc::MNT_LOCAL as u32 != 0)
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

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
    use windows_sys::Win32::Storage::FileSystem::FILE_REMOTE_PROTOCOL_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
    use windows_sys::Win32::Storage::FileSystem::FileRemoteProtocolInfo;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;

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
    unsafe fn information<T>(file: &File, class: i32) -> Option<T> {
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
        (filled != 0).then_some(value)
    }

    pub(super) fn is_local(file: &File) -> io::Result<bool> {
        // SAFETY: `FILE_REMOTE_PROTOCOL_INFO` is the plain C struct that this class fills.
        let remote =
            unsafe { information::<FILE_REMOTE_PROTOCOL_INFO>(file, FileRemoteProtocolInfo) };
        Ok(remote.is_none())
    }

    pub(super) fn file_id(file: &File) -> io::Result<FileId> {
        // SAFETY: `FILE_ID_INFO` is the plain C struct that this class fills.
        let info = unsafe { information::<FILE_ID_INFO>(file, FileIdInfo) }
            .ok_or_else(io::Error::last_os_error)?;
        Ok(FileId {
            volume: info.VolumeSerialNumber,
            file: u128::from_le_bytes(info.FileId.Identifier),
        })
    }
}
