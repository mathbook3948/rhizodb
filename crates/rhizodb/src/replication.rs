use crate::error::Error;
use crate::error::ErrorKind;

const POSITION_FORMAT: u8 = 1;

/// The longest writer id a promotion accepts.
pub(crate) const WRITER_ID_LIMIT: usize = 64;

/// A place in a database's history of changes: the log sequence number of the last change it
/// holds, and the term and writer id of the writer that made that change.
///
/// A database that never held a change, or whose changes were all made before any promotion,
/// is at term 0 with an empty writer id. Two databases at equal positions hold the same graph.
///
/// A host sends a position to another node as the bytes of [`Position::to_bytes`] and reads it
/// back with [`Position::from_bytes`].
///
/// ```
/// use rhizodb::Position;
///
/// let start = Position::start();
/// assert_eq!((start.term(), start.writer_id(), start.lsn()), (0, &[][..], 0));
/// assert_eq!(Position::from_bytes(&start.to_bytes())?, start);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Position {
    term: u64,
    writer_id: Vec<u8>,
    lsn: u64,
}

impl Position {
    /// The position of a database that holds no change.
    pub fn start() -> Self {
        Self::default()
    }

    pub(crate) fn new(term: u64, writer_id: &[u8], lsn: u64) -> Result<Self, Error> {
        check_epoch(term, writer_id)?;
        Ok(Self {
            term,
            writer_id: writer_id.to_vec(),
            lsn,
        })
    }

    /// The term of the writer that made the last change, or 0 before any promotion.
    pub fn term(&self) -> u64 {
        self.term
    }

    /// The id of the writer that made the last change, empty before any promotion.
    pub fn writer_id(&self) -> &[u8] {
        &self.writer_id
    }

    /// The log sequence number of the last change, or 0 when there is none.
    pub fn lsn(&self) -> u64 {
        self.lsn
    }

    /// The position as bytes that [`Position::from_bytes`] reads back, in a format that later
    /// releases keep reading.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(18 + self.writer_id.len());
        out.push(POSITION_FORMAT);
        out.extend_from_slice(&self.term.to_le_bytes());
        out.extend_from_slice(&self.lsn.to_le_bytes());
        out.push(self.writer_id.len() as u8);
        out.extend_from_slice(&self.writer_id);
        out
    }

    /// Reads a position from the bytes of [`Position::to_bytes`]. Bytes of another shape fail
    /// with [`ErrorKind::CorruptData`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let malformed = || Error::new(ErrorKind::CorruptData, "the bytes are not a position");
        let (&format, rest) = bytes.split_first().ok_or_else(malformed)?;
        if format != POSITION_FORMAT || rest.len() < 17 {
            return Err(malformed());
        }
        let (term, rest) = rest.split_at(8);
        let (lsn, rest) = rest.split_at(8);
        let (&length, writer_id) = rest.split_first().ok_or_else(malformed)?;
        if writer_id.len() != usize::from(length) {
            return Err(malformed());
        }
        Self::new(le_u64(term), writer_id, le_u64(lsn)).map_err(|_| malformed())
    }
}

fn le_u64(bytes: &[u8]) -> u64 {
    let mut array = [0; 8];
    array.copy_from_slice(bytes);
    u64::from_le_bytes(array)
}

/// Fails unless `term` and `writer_id` name an epoch: term 0 with no writer id, or a later term
/// with a writer id of 1 to 64 bytes.
pub(crate) fn check_epoch(term: u64, writer_id: &[u8]) -> Result<(), Error> {
    let valid = match term {
        0 => writer_id.is_empty(),
        _ => (1..=WRITER_ID_LIMIT).contains(&writer_id.len()),
    };
    if valid {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidArgumentValue,
            format!(
                "term {term} with a writer id of {} bytes names no epoch: term 0 has no writer id \
                 and every later term has one of 1 to {WRITER_ID_LIMIT} bytes",
                writer_id.len()
            ),
        ))
    }
}

#[cfg(test)]
#[path = "replication.test.rs"]
mod tests;
