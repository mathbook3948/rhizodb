use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;
use crate::value::put_varint;
use crate::value::put_varint_u64;

use super::ids::Label;
use super::ids::NodeId;
use super::ids::PropertyKey;
use super::ids::RelationshipType;
use super::ids::Token;

pub(crate) type Properties = BTreeMap<Token<PropertyKey>, PropertyValue>;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NodeRecord {
    pub(crate) labels: BTreeSet<Token<Label>>,
    pub(crate) properties: Properties,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RelationshipRecord {
    pub(crate) rel_type: Token<RelationshipType>,
    pub(crate) source: NodeId,
    pub(crate) target: NodeId,
    pub(crate) properties: Properties,
}

fn encode_properties(properties: &Properties, out: &mut Vec<u8>) {
    put_varint(out, properties.len());
    for (key, value) in properties {
        out.extend_from_slice(&key.id().to_be_bytes());
        value.encode_into(out);
    }
}

fn decode_properties(reader: &mut Reader<'_>) -> Result<Properties, DecodeError> {
    let count = reader.varint()?;
    let mut properties = Properties::new();
    for _ in 0..count {
        let key = Token::new(reader.u32()?);
        properties.insert(key, reader.value()?);
    }
    Ok(properties)
}

fn read_labels(reader: &mut Reader<'_>) -> Result<BTreeSet<Token<Label>>, DecodeError> {
    let label_count = reader.varint()?;
    let mut labels = BTreeSet::new();
    for _ in 0..label_count {
        labels.insert(Token::new(reader.u32()?));
    }
    Ok(labels)
}

impl NodeRecord {
    /// The stored node record, which holds the properties. The labels go to node meta.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        encode_properties(&self.properties, &mut out);
        out
    }

    pub(crate) fn decode_properties(bytes: &[u8]) -> Result<Properties, DecodeError> {
        let mut reader = Reader::new(bytes);
        let properties = decode_properties(&mut reader)?;
        reader.finish()?;
        Ok(properties)
    }

    /// A node record written before node meta, which held the labels ahead of the properties.
    pub(crate) fn decode_with_labels(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let labels = read_labels(&mut reader)?;
        let properties = decode_properties(&mut reader)?;
        reader.finish()?;
        Ok(Self { labels, properties })
    }
}

fn token<K>(reader: &mut Reader<'_>) -> Result<Token<K>, DecodeError> {
    u32::try_from(reader.varint_u64()?)
        .map(Token::new)
        .map_err(|_| DecodeError::LengthOverflow)
}

const RECORD_GUESS: usize = 64;

impl RelationshipRecord {
    /// The value of `key` in an encoded relationship record, decoding none of the other values.
    pub(crate) fn property(
        bytes: &[u8],
        key: Token<PropertyKey>,
    ) -> Result<Option<PropertyValue>, DecodeError> {
        let mut reader = Reader::new(bytes);
        token::<RelationshipType>(&mut reader)?;
        reader.varint_u64()?;
        reader.varint_u64()?;
        let count = reader.varint()?;
        for _ in 0..count {
            if token::<PropertyKey>(&mut reader)? == key {
                return reader.value().map(Some);
            }
            reader.skip_value()?;
        }
        Ok(None)
    }

    /// The record with its type, its ends, its property count, and each property key as
    /// varints, and each integer property as a zigzag varint when that is shorter.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(RECORD_GUESS);
        put_varint_u64(&mut out, u64::from(self.rel_type.id()));
        put_varint_u64(&mut out, self.source.0);
        put_varint_u64(&mut out, self.target.0);
        put_varint(&mut out, self.properties.len());
        for (key, value) in &self.properties {
            put_varint_u64(&mut out, u64::from(key.id()));
            value.encode_compact_into(&mut out);
        }
        out.as_slice().to_vec()
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let rel_type = token(&mut reader)?;
        let source = NodeId(reader.varint_u64()?);
        let target = NodeId(reader.varint_u64()?);
        let count = reader.varint()?;
        let mut properties = Properties::new();
        for _ in 0..count {
            let key = token(&mut reader)?;
            properties.insert(key, reader.value()?);
        }
        reader.finish()?;
        Ok(Self {
            rel_type,
            source,
            target,
            properties,
        })
    }

    /// The record as format 10 and earlier stored it, for tests that write an older format.
    #[cfg(test)]
    pub(crate) fn encode_fixed(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.rel_type.id().to_be_bytes());
        out.extend_from_slice(&self.source.0.to_be_bytes());
        out.extend_from_slice(&self.target.0.to_be_bytes());
        encode_properties(&self.properties, &mut out);
        out
    }

    /// A record written before format 11, with its type and ends at fixed width.
    pub(crate) fn decode_fixed(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let rel_type = Token::new(reader.u32()?);
        let source = NodeId(reader.u64()?);
        let target = NodeId(reader.u64()?);
        let properties = decode_properties(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            rel_type,
            source,
            target,
            properties,
        })
    }
}

#[cfg(test)]
#[path = "records.test.rs"]
mod tests;
