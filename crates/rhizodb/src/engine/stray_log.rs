use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::block_file::BlockFile;
use super::block_file::BlockSize;
use super::block_file::FileSystem;
use super::block_file::SECTOR_SIZE;
use super::error::EngineError;
use super::os_fs::OsFile;
use super::os_fs::OsFs;
use super::store::Engine;
use super::store::EngineOptions;
use super::wal::segment_files;
use super::wal::segment_name;
use super::wal::segment_sequence;

/// Where a database file, or a symlink to it, lies: a directory and a file name in it.
#[derive(Debug, PartialEq)]
pub(crate) struct Place {
    pub(crate) directory: PathBuf,
    pub(crate) name: String,
}

impl Place {
    /// The place `path` names, with every symlink and `..` in its directory resolved.
    pub(crate) fn of(path: &Path) -> io::Result<Self> {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "the database path {} names no file in UTF-8",
                        path.display()
                    ),
                )
            })?;
        let directory = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        Ok(Self {
            directory: std::fs::canonicalize(directory)?,
            name: name.to_owned(),
        })
    }

    pub(crate) fn path(&self) -> PathBuf {
        self.directory.join(&self.name)
    }

    pub(crate) fn segments(&self) -> io::Result<Vec<u64>> {
        segment_files(&OsFs::new(&self.directory), &self.name)
    }

    fn segment(&self, sequence: u64) -> PathBuf {
        self.directory.join(segment_name(&self.name, sequence))
    }
}

/// Moves the log segments beside `stray`, under its name, to `real`, the database file `stray`
/// is a symlink to, as a release that named the log after the path it was given left them.
/// Holds the file's lock throughout, and moves nothing unless the segments, after any beside
/// `real`, replay as the file's log. Moves the newest first and syncs both directories after
/// each, so whatever a crash leaves beside `real` lacks the start of the log, which an open
/// through `real` refuses, and the next call finishes the move.
pub(crate) fn adopt(
    real: &Place,
    stray: &Place,
    options: EngineOptions,
) -> Result<(), EngineError> {
    let directory = OsFs::new(&real.directory);
    let sector = BlockSize::new(SECTOR_SIZE).ok_or(EngineError::TooLarge)?;
    let database = directory.open(&real.name, sector)?;
    database
        .lock_or_refuse_where_unsupported()
        .map_err(|error| match error.kind() {
            io::ErrorKind::Unsupported => refusal(
                real,
                stray,
                "the file system has no file locks, so another process could be using the log",
            ),
            _ => error.into(),
        })?;
    let moving = stray.segments()?;
    let Some(&last_moving) = moving.last() else {
        return Ok(());
    };
    let own = real.segments()?;
    let continues = own
        .iter()
        .zip(last_moving + 1..)
        .all(|(sequence, expected)| *sequence == expected);
    if !continues {
        return Err(refusal(
            real,
            stray,
            "the segments beside the file do not continue the ones beside the symlink, so they \
             may be two logs; remove the set that is stale",
        ));
    }
    let view = View {
        real: directory,
        stray: OsFs::new(&stray.directory),
        real_name: real.name.clone(),
        stray_name: stray.name.clone(),
        moving: moving.clone(),
        database,
    };
    drop(
        Engine::open_read_only(Arc::new(view), &real.name, options).map_err(|error| {
            refusal(
                real,
                stray,
                &format!("they do not replay as the file's log: {error}"),
            )
        })?,
    );
    let stray_directory = OsFs::new(&stray.directory);
    for &sequence in moving.iter().rev() {
        let (from, to) = (stray.segment(sequence), real.segment(sequence));
        std::fs::rename(&from, &to).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot move the log segment {} to {} ({error}), so move it there by hand, \
                     copying it and then removing the original if a rename cannot reach it, \
                     before opening the database again",
                    from.display(),
                    to.display()
                ),
            )
        })?;
        OsFs::new(&real.directory).sync_names()?;
        stray_directory.sync_names()?;
    }
    Ok(())
}

fn refusal(real: &Place, stray: &Place, why: &str) -> EngineError {
    let files = stray.segments().unwrap_or_default();
    let files: Vec<String> = files
        .iter()
        .map(|&sequence| stray.segment(sequence).display().to_string())
        .collect();
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "log segments that an older release named after the symlink {} lie beside it ({}), \
             and none was moved to {} because {why}",
            stray.path().display(),
            files.join(", "),
            real.path().display()
        ),
    )
    .into()
}

#[derive(Debug)]
struct View {
    real: OsFs,
    stray: OsFs,
    real_name: String,
    stray_name: String,
    moving: Vec<u64>,
    database: OsFile,
}

impl View {
    fn moving(&self, name: &str) -> Option<u64> {
        segment_sequence(&self.real_name, name).filter(|sequence| self.moving.contains(sequence))
    }
}

fn unchanged() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "checking a log before moving it changes no file",
    )
}

impl FileSystem for View {
    type File = ViewFile;

    fn create(&self, _name: &str, _size: BlockSize) -> io::Result<ViewFile> {
        Err(unchanged())
    }

    fn open(&self, name: &str, size: BlockSize) -> io::Result<ViewFile> {
        if name == self.real_name {
            return Ok(ViewFile {
                file: self.database.share(size)?,
                locked: true,
            });
        }
        let file = match self.moving(name) {
            Some(sequence) => self
                .stray
                .open(&segment_name(&self.stray_name, sequence), size)?,
            None => self.real.open(name, size)?,
        };
        Ok(ViewFile {
            file,
            locked: false,
        })
    }

    fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
        Err(unchanged())
    }

    fn remove(&self, _name: &str) -> io::Result<()> {
        Err(unchanged())
    }

    fn names(&self) -> io::Result<Vec<String>> {
        let mut names = self.real.names()?;
        names.extend(
            self.moving
                .iter()
                .map(|&sequence| segment_name(&self.real_name, sequence)),
        );
        names.sort();
        Ok(names)
    }

    fn sync_names(&self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct ViewFile {
    file: OsFile,
    locked: bool,
}

impl BlockFile for ViewFile {
    fn read(&self, first: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.file.read(first, buffer)
    }

    fn write(&self, _first: u64, _buffer: &[u8]) -> io::Result<()> {
        Err(unchanged())
    }

    fn block_count(&self) -> io::Result<u64> {
        self.file.block_count()
    }

    fn is_empty(&self) -> io::Result<bool> {
        self.file.is_empty()
    }

    fn lock(&self) -> io::Result<()> {
        if self.locked {
            Ok(())
        } else {
            self.file.lock()
        }
    }

    fn lock_shared(&self) -> io::Result<()> {
        if self.locked {
            Ok(())
        } else {
            self.file.lock_shared()
        }
    }

    fn set_block_count(&self, _count: u64) -> io::Result<()> {
        Err(unchanged())
    }

    fn sync(&self) -> io::Result<()> {
        self.file.sync()
    }
}
