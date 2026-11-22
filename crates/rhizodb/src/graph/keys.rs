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

pub(crate) fn adjacency_prefix(node: NodeId, rel_type: Option<Token<RelationshipType>>) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&node.0.to_be_bytes());
    if let Some(rel_type) = rel_type {
        key.extend_from_slice(&rel_type.id().to_be_bytes());
    }
    key
}

pub(crate) fn adjacency_key(
    node: NodeId,
    rel_type: Token<RelationshipType>,
    relationship: RelationshipId,
) -> [u8; 20] {
    let mut key = [0u8; 20];
    key[..8].copy_from_slice(&node.0.to_be_bytes());
    key[8..12].copy_from_slice(&rel_type.id().to_be_bytes());
    key[12..].copy_from_slice(&relationship.0.to_be_bytes());
    key
}

pub(crate) fn parse_adjacency_key(
    key: &[u8],
) -> Result<(Token<RelationshipType>, RelationshipId), DecodeError> {
    let mut reader = Reader::new(key);
    reader.u64()?;
    let rel_type = Token::new(reader.u32()?);
    let relationship = RelationshipId(reader.u64()?);
    reader.finish()?;
    Ok((rel_type, relationship))
}

pub(crate) fn label_prefix(label: Token<Label>) -> [u8; 4] {
    label.id().to_be_bytes()
}

pub(crate) fn label_key(label: Token<Label>, node: NodeId) -> [u8; 12] {
    let mut key = [0u8; 12];
    key[..4].copy_from_slice(&label.id().to_be_bytes());
    key[4..].copy_from_slice(&node.0.to_be_bytes());
    key
}

pub(crate) fn parse_label_key(key: &[u8]) -> Result<NodeId, DecodeError> {
    let mut reader = Reader::new(key);
    reader.u32()?;
    let node = NodeId(reader.u64()?);
    reader.finish()?;
    Ok(node)
}
