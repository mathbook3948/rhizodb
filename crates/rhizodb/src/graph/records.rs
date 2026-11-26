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

fn token<K>(reader: &mut Reader<'_>) -> Result<Token<K>, DecodeError> {
    u32::try_from(reader.varint_u64()?)
        .map(Token::new)
        .map_err(|_| DecodeError::LengthOverflow)
}

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
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    /// Appends [`RelationshipRecord::encode`] of the record to `out`.
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        put_varint_u64(out, u64::from(self.rel_type.id()));
        put_varint_u64(out, self.source.0);
        put_varint_u64(out, self.target.0);
        put_varint(out, self.properties.len());
        for (key, value) in &self.properties {
            put_varint_u64(out, u64::from(key.id()));
            value.encode_compact_into(out);
        }
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
}

#[cfg(test)]
#[path = "records.test.rs"]
mod tests;
