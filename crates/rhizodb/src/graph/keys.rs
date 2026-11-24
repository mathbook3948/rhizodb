use crate::value::DecodeError;
use crate::value::Reader;

use super::ids::Label;
use super::ids::NodeId;
use super::ids::RelationshipId;
use super::ids::RelationshipType;
use super::ids::Token;

pub(crate) const FORMAT_VERSION_KEY: &[u8] = b"format_version";
pub(crate) const NEXT_NODE_ID_KEY: &[u8] = b"next_node_id";
pub(crate) const NEXT_RELATIONSHIP_ID_KEY: &[u8] = b"next_relationship_id";
pub(crate) const NEXT_LABEL_SET_KEY: &[u8] = b"next_label_set";

pub(crate) fn next_token_key(tag: u8) -> [u8; 11] {
    let mut key = *b"next_token\0";
    key[10] = tag;
    key
}

pub(crate) fn token_name_key(tag: u8, name: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(1 + name.len());
    key.push(tag);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn token_id_key(tag: u8, id: u32) -> [u8; 5] {
    let mut key = [tag, 0, 0, 0, 0];
    key[1..].copy_from_slice(&id.to_be_bytes());
    key
}

pub(crate) fn element_key(id: u64) -> [u8; 8] {
    id.to_be_bytes()
}

/// The node, relationship type, and relationship of a key of the adjacency tables of format
/// versions before adjacency segments.
pub(crate) fn parse_adjacency_key(
    key: &[u8],
) -> Result<(NodeId, Token<RelationshipType>, RelationshipId), DecodeError> {
    let mut reader = Reader::new(key);
    let node = NodeId(reader.u64()?);
    let rel_type = Token::new(reader.u32()?);
    let relationship = RelationshipId(reader.u64()?);
    reader.finish()?;
    Ok((node, rel_type, relationship))
}

pub(crate) fn label_key(label: Token<Label>, node: NodeId) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..4].copy_from_slice(&label.id().to_be_bytes());
    key[4..].copy_from_slice(&node.0.to_be_bytes());
    key
}

pub(crate) const NEXT_INDEX_ID_KEY: &[u8] = b"next_index_id";
pub(crate) const TOKENIZER_KEY: &[u8] = b"tokenizer";

pub(crate) fn index_prefix(index: u32) -> [u8; 4] {
    index.to_be_bytes()
}

/// The key of an index entry. An index covers nodes or relationships, never both, so the
/// element ID alone tells its entries apart.
pub(crate) fn index_entry_key(index: u32, element: u64) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..4].copy_from_slice(&index.to_be_bytes());
    key[4..].copy_from_slice(&element.to_be_bytes());
    key
}

pub(crate) fn parse_index_entry_key(key: &[u8]) -> Result<u64, DecodeError> {
    let mut reader = Reader::new(key);
    reader.u32()?;
    let element = reader.u64()?;
    reader.finish()?;
    Ok(element)
}

/// The tokenizer drops control characters, so a term never holds a zero byte, and the zero
/// after it keeps one term's postings apart from those of a longer term it starts.
pub(crate) fn posting_prefix(index: u32, term: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + term.len() + 1 + 8);
    key.extend_from_slice(&index.to_be_bytes());
    key.extend_from_slice(term.as_bytes());
    key.push(0);
    key
}

/// The start of the posting keys of every term that begins with `prefix`, the term itself
/// included.
pub(crate) fn term_prefix(index: u32, prefix: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + prefix.len());
    key.extend_from_slice(&index.to_be_bytes());
    key.extend_from_slice(prefix.as_bytes());
    key
}

pub(crate) fn posting_key(index: u32, term: &str, element: u64) -> Vec<u8> {
    let mut key = posting_prefix(index, term);
    key.extend_from_slice(&element.to_be_bytes());
    key
}

pub(crate) fn parse_posting_term(key: &[u8]) -> Result<&str, DecodeError> {
    let end = key.len().checked_sub(9).ok_or(DecodeError::UnexpectedEnd)?;
    let term = key
        .get(4..end)
        .filter(|_| key.get(end) == Some(&0))
        .ok_or(DecodeError::UnexpectedEnd)?;
    std::str::from_utf8(term).map_err(|_| DecodeError::InvalidUtf8)
}

pub(crate) fn parse_posting_element(key: &[u8]) -> Result<u64, DecodeError> {
    let start = key.len().checked_sub(8).ok_or(DecodeError::UnexpectedEnd)?;
    let mut reader = Reader::new(&key[start..]);
    let element = reader.u64()?;
    reader.finish()?;
    Ok(element)
}

/// The key of a property index entry: the index, the value's key, and the element, so the
/// entries of one value, or of a range of values, are one run of keys.
pub(crate) fn property_entry_key(index: u32, value: &[u8], element: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + value.len() + 8);
    key.extend_from_slice(&index.to_be_bytes());
    key.extend_from_slice(value);
    key.extend_from_slice(&element.to_be_bytes());
    key
}

/// The bound of a run of property index entries whose value keys start at `value`.
pub(crate) fn property_entry_bound(index: u32, value: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(4 + value.len());
    key.extend_from_slice(&index.to_be_bytes());
    key.extend_from_slice(value);
    key
}

pub(crate) fn parse_property_entry_element(key: &[u8]) -> Result<u64, DecodeError> {
    let start = key.len().checked_sub(8).ok_or(DecodeError::UnexpectedEnd)?;
    let mut reader = Reader::new(&key[start..]);
    let element = reader.u64()?;
    reader.finish()?;
    Ok(element)
}
