use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::storage::ReadTransaction;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;
use crate::value::put_varint;

use super::GraphError;
use super::GraphTx;
use super::NodeId;
use super::NodeRecord;
use super::ids::Label;
use super::ids::PropertyKey;
use super::ids::Token;
use super::keys;
use super::tokenize;
use super::tokenize::Purpose;

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

const KIND_KEYWORD: u8 = 0;
const KIND_VECTOR: u8 = 1;

const METRIC_COSINE: u8 = 0;
const METRIC_EUCLIDEAN: u8 = 1;
const METRIC_DOT_PRODUCT: u8 = 2;

/// The tables that hold index entries, each keyed first by index ID.
pub(crate) const ENTRY_TABLES: [Table; 4] = [
    Table::KeywordPostings,
    Table::KeywordDocuments,
    Table::KeywordTotals,
    Table::Vectors,
];

/// How a vector index measures closeness. Every metric gives a score that is higher for closer
/// vectors, so results always come in descending score order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VectorMetric {
    /// The cosine of the angle between two vectors, which is also the score. A zero vector has no
    /// direction, so it is not indexed and cannot be searched for.
    Cosine,
    /// The straight-line distance between two vectors. The score is the negative distance.
    Euclidean,
    /// The dot product of two vectors, which is also the score.
    DotProduct,
}

impl VectorMetric {
    fn tag(self) -> u8 {
        match self {
            Self::Cosine => METRIC_COSINE,
            Self::Euclidean => METRIC_EUCLIDEAN,
            Self::DotProduct => METRIC_DOT_PRODUCT,
        }
    }

    fn from_tag(tag: u8) -> Result<Self, DecodeError> {
        match tag {
            METRIC_COSINE => Ok(Self::Cosine),
            METRIC_EUCLIDEAN => Ok(Self::Euclidean),
            METRIC_DOT_PRODUCT => Ok(Self::DotProduct),
            other => Err(DecodeError::UnknownTag(other)),
        }
    }

    fn admits(self, vector: &[f64]) -> bool {
        self != Self::Cosine || norm(vector) > 0.0
    }

    fn score(self, query: &[f64], stored: &[f64]) -> f64 {
        match self {
            Self::Cosine => dot(query, stored) / (norm(query) * norm(stored)),
            Self::Euclidean => {
                0.0 - query
                    .iter()
                    .zip(stored)
                    .map(|(left, right)| (left - right) * (left - right))
                    .sum::<f64>()
                    .sqrt()
            }
            Self::DotProduct => dot(query, stored),
        }
    }
}

fn dot(left: &[f64], right: &[f64]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum()
}

fn norm(vector: &[f64]) -> f64 {
    dot(vector, vector).sqrt()
}

/// What an index reads from each node with its label. Keyword properties are sorted and
/// distinct, so two definitions over the same properties are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexKind {
    Keyword {
        properties: Vec<Token<PropertyKey>>,
    },
    Vector {
        property: Token<PropertyKey>,
        dimension: u32,
        metric: VectorMetric,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexDefinition {
    pub(crate) label: Token<Label>,
    pub(crate) kind: IndexKind,
}

/// A stored index. Its entries are keyed by `id`, so a dropped index's name can be reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Index {
    pub(crate) id: u32,
    pub(crate) definition: IndexDefinition,
}

/// A keyword document: how often each term occurs in the indexed properties of one node.
pub(crate) type Document = BTreeMap<String, u32>;

/// What one node contributes to one index.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Entry {
    Keyword(Document),
    Vector(Vec<f64>),
}

/// A stored row: the table, the key, and the value.
pub(crate) type Row = (Table, Vec<u8>, Vec<u8>);

impl Index {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.id.to_be_bytes());
        out.extend_from_slice(&self.definition.label.id().to_be_bytes());
        match &self.definition.kind {
            IndexKind::Keyword { properties } => {
                out.push(KIND_KEYWORD);
                put_varint(&mut out, properties.len());
                for property in properties {
                    out.extend_from_slice(&property.id().to_be_bytes());
                }
            }
            IndexKind::Vector {
                property,
                dimension,
                metric,
            } => {
                out.push(KIND_VECTOR);
                out.extend_from_slice(&property.id().to_be_bytes());
                out.extend_from_slice(&dimension.to_be_bytes());
                out.push(metric.tag());
            }
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let id = reader.u32()?;
        let label = Token::new(reader.u32()?);
        let kind = match reader.u8()? {
            KIND_KEYWORD => {
                let count = reader.varint()?;
                let mut properties = Vec::new();
                for _ in 0..count {
                    properties.push(Token::new(reader.u32()?));
                }
                IndexKind::Keyword { properties }
            }
            KIND_VECTOR => IndexKind::Vector {
                property: Token::new(reader.u32()?),
                dimension: reader.u32()?,
                metric: VectorMetric::from_tag(reader.u8()?)?,
            },
            other => return Err(DecodeError::UnknownTag(other)),
        };
        reader.finish()?;
        Ok(Self {
            id,
            definition: IndexDefinition { label, kind },
        })
    }

    /// The rows that hold `entry` for `node`, leaving out the keyword totals.
    pub(crate) fn rows(&self, node: NodeId, entry: &Entry) -> Vec<Row> {
        let entry_key = keys::index_entry_key(self.id, node).to_vec();
        match entry {
            Entry::Keyword(document) => {
                let length = document_length(document);
                let mut rows: Vec<Row> = document
                    .iter()
                    .map(|(term, frequency)| {
                        let mut value = frequency.to_be_bytes().to_vec();
                        value.extend_from_slice(&length.to_be_bytes());
                        (
                            Table::KeywordPostings,
                            keys::posting_key(self.id, term, node),
                            value,
                        )
                    })
                    .collect();
                rows.push((
                    Table::KeywordDocuments,
                    entry_key,
                    encode_document(document),
                ));
                rows
            }
            Entry::Vector(vector) => vec![(
                Table::Vectors,
                entry_key,
                vector.iter().flat_map(|item| item.to_be_bytes()).collect(),
            )],
        }
    }
}

impl IndexDefinition {
    pub(crate) fn properties(&self) -> &[Token<PropertyKey>] {
        match &self.kind {
            IndexKind::Keyword { properties } => properties,
            IndexKind::Vector { property, .. } => std::slice::from_ref(property),
        }
    }

    /// What `record` contributes, or `None` when it lacks the label or holds nothing to index.
    pub(crate) fn entry(&self, record: &NodeRecord) -> Option<Entry> {
        if !record.labels.contains(&self.label) {
            return None;
        }
        match &self.kind {
            IndexKind::Keyword { properties } => {
                let mut document = Document::new();
                let mut add = |text: &str| {
                    for term in tokenize::terms(text, Purpose::Document) {
                        let frequency = document.entry(term).or_insert(0);
                        *frequency = frequency.saturating_add(1);
                    }
                };
                for property in properties {
                    match record.properties.get(property) {
                        Some(PropertyValue::String(text)) => add(text),
                        Some(PropertyValue::List(items)) => {
                            for item in items {
                                if let PropertyValue::String(text) = item {
                                    add(text);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                (!document.is_empty()).then_some(Entry::Keyword(document))
            }
            IndexKind::Vector {
                property,
                dimension,
                metric,
            } => {
                let Some(PropertyValue::List(items)) = record.properties.get(property) else {
                    return None;
                };
                if usize::try_from(*dimension).ok() != Some(items.len()) {
                    return None;
                }
                let vector = items
                    .iter()
                    .map(|item| match item {
                        PropertyValue::Integer(value) => Some(*value as f64),
                        PropertyValue::Float(value) if value.is_finite() => Some(*value),
                        _ => None,
                    })
                    .collect::<Option<Vec<f64>>>()?;
                metric.admits(&vector).then_some(Entry::Vector(vector))
            }
        }
    }
}

/// The number of terms in a document, counting repeats.
pub(crate) fn document_length(document: &Document) -> u32 {
    document
        .values()
        .fold(0u32, |total, frequency| total.saturating_add(*frequency))
}

/// The keyword totals row value: how many nodes the index holds and their summed length.
pub(crate) fn encode_totals(documents: u64, length: u64) -> Vec<u8> {
    let mut out = documents.to_be_bytes().to_vec();
    out.extend_from_slice(&length.to_be_bytes());
    out
}

fn decode_totals(bytes: &[u8]) -> Result<(u64, u64), DecodeError> {
    let mut reader = Reader::new(bytes);
    let totals = (reader.u64()?, reader.u64()?);
    reader.finish()?;
    Ok(totals)
}

fn encode_document(document: &Document) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, document.len());
    for (term, frequency) in document {
        put_varint(&mut out, term.len());
        out.extend_from_slice(term.as_bytes());
        out.extend_from_slice(&frequency.to_be_bytes());
    }
    out
}

fn decode_document(bytes: &[u8]) -> Result<Document, DecodeError> {
    let mut reader = Reader::new(bytes);
    let count = reader.varint()?;
    let mut document = Document::new();
    for _ in 0..count {
        let length = reader.varint()?;
        let term = std::str::from_utf8(reader.take(length)?)
            .map_err(|_| DecodeError::InvalidUtf8)?
            .to_owned();
        document.insert(term, reader.u32()?);
    }
    reader.finish()?;
    Ok(document)
}

fn decode_posting(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let mut reader = Reader::new(bytes);
    let posting = (reader.u32()?, reader.u32()?);
    reader.finish()?;
    Ok(posting)
}

fn decode_vector(bytes: &[u8], dimension: u32) -> Result<Vec<f64>, GraphError> {
    let mut reader = Reader::new(bytes);
    let mut vector = Vec::new();
    for _ in 0..dimension {
        vector.push(f64::from_bits(reader.u64()?));
    }
    reader.finish()?;
    Ok(vector)
}

fn bm25(idf: f64, frequency: u32, length: u32, average_length: f64) -> f64 {
    let frequency = f64::from(frequency);
    let relative_length = f64::from(length) / average_length;
    idf * frequency * (BM25_K1 + 1.0)
        / (frequency + BM25_K1 * (1.0 - BM25_B + BM25_B * relative_length))
}

fn ranking(left: &(NodeId, f64), right: &(NodeId, f64)) -> Ordering {
    left.1
        .is_nan()
        .cmp(&right.1.is_nan())
        .then_with(|| right.1.total_cmp(&left.1))
        .then_with(|| left.0.cmp(&right.0))
}

fn best(mut scored: Vec<(NodeId, f64)>, limit: usize) -> Vec<(NodeId, f64)> {
    if limit < scored.len() {
        scored.select_nth_unstable_by(limit, ranking);
        scored.truncate(limit);
    }
    scored.sort_by(ranking);
    scored
}

impl<T: ReadTransaction> GraphTx<T> {
    /// Every index, by name.
    pub(crate) fn indexes(&self) -> Result<Vec<(String, Index)>, GraphError> {
        self.transaction
            .scan_prefix(Table::Indexes, &[])?
            .into_iter()
            .map(|(name, value)| {
                let name = String::from_utf8(name).map_err(|_| DecodeError::InvalidUtf8)?;
                Ok((name, Index::decode(&value)?))
            })
            .collect()
    }

    fn index(&self, name: &str) -> Result<Option<Index>, GraphError> {
        let bytes = self.transaction.get(Table::Indexes, name.as_bytes())?;
        Ok(bytes.map(|bytes| Index::decode(&bytes)).transpose()?)
    }

    /// The nodes that best match `text` in the keyword index `name`, ranked by BM25. A node
    /// matches when it holds any term of the text.
    pub(crate) fn keyword_search(
        &self,
        name: &str,
        text: &str,
        limit: usize,
    ) -> Result<Vec<(NodeId, f64)>, GraphError> {
        let index = self
            .index(name)?
            .filter(|index| matches!(index.definition.kind, IndexKind::Keyword { .. }))
            .ok_or_else(|| GraphError::IndexNotFound(name.to_owned(), "keyword index"))?;
        let totals = self
            .transaction
            .get(Table::KeywordTotals, &keys::index_prefix(index.id))?
            .ok_or(GraphError::Corrupt("a keyword index has no totals"))?;
        let (documents, length) = decode_totals(&totals)?;
        if documents == 0 {
            return Ok(Vec::new());
        }
        let average_length = length as f64 / documents as f64;
        let mut terms = tokenize::terms(text, Purpose::Query);
        terms.sort();
        terms.dedup();
        let mut scores: BTreeMap<NodeId, f64> = BTreeMap::new();
        for term in terms {
            let postings = self.transaction.scan_prefix(
                Table::KeywordPostings,
                &keys::posting_prefix(index.id, &term),
            )?;
            let matching = postings.len() as f64;
            let idf = (1.0 + (documents as f64 - matching + 0.5) / (matching + 0.5)).ln();
            for (key, value) in postings {
                let (frequency, length) = decode_posting(&value)?;
                *scores.entry(keys::parse_posting_node(&key)?).or_insert(0.0) +=
                    bm25(idf, frequency, length, average_length);
            }
        }
        Ok(best(scores.into_iter().collect(), limit))
    }

    /// The nodes whose vectors in the vector index `name` are closest to `query`, found by
    /// scoring every vector in the index.
    pub(crate) fn vector_search(
        &self,
        name: &str,
        query: &[f64],
        limit: usize,
    ) -> Result<Vec<(NodeId, f64)>, GraphError> {
        let not_found = || GraphError::IndexNotFound(name.to_owned(), "vector index");
        let index = self.index(name)?.ok_or_else(not_found)?;
        let IndexKind::Vector {
            dimension, metric, ..
        } = index.definition.kind
        else {
            return Err(not_found());
        };
        if usize::try_from(dimension).ok() != Some(query.len()) {
            return Err(GraphError::InvalidQueryVector(format!(
                "index `{name}` holds vectors of {dimension} numbers, not {}",
                query.len()
            )));
        }
        if !query.iter().all(|item| item.is_finite()) {
            return Err(GraphError::InvalidQueryVector(
                "a query vector holds only finite numbers".to_owned(),
            ));
        }
        if !metric.admits(query) {
            return Err(GraphError::InvalidQueryVector(
                "a cosine index cannot be searched with a zero vector".to_owned(),
            ));
        }
        let mut scored = Vec::new();
        for (key, value) in self
            .transaction
            .scan_prefix(Table::Vectors, &keys::index_prefix(index.id))?
        {
            let stored = decode_vector(&value, dimension)?;
            scored.push((
                keys::parse_index_entry_key(&key)?,
                metric.score(query, &stored),
            ));
        }
        Ok(best(scored, limit))
    }
}

impl<T: WriteTransaction> GraphTx<T> {
    /// Creates a keyword index over the string properties `properties` of nodes labelled
    /// `label`, filled from the nodes already there.
    pub(crate) fn create_keyword_index(
        &mut self,
        name: &str,
        label: &str,
        properties: &[&str],
    ) -> Result<(), GraphError> {
        if properties.is_empty() {
            return Err(GraphError::InvalidIndex(
                "a keyword index needs at least one property",
            ));
        }
        let label = self.intern_token(label)?;
        let mut tokens = properties
            .iter()
            .map(|property| self.intern_token(property))
            .collect::<Result<Vec<_>, _>>()?;
        tokens.sort();
        tokens.dedup();
        self.create_index(
            name,
            IndexDefinition {
                label,
                kind: IndexKind::Keyword { properties: tokens },
            },
        )
    }

    /// Creates a vector index over the list property `property` of nodes labelled `label`,
    /// filled from the nodes already there.
    pub(crate) fn create_vector_index(
        &mut self,
        name: &str,
        label: &str,
        property: &str,
        dimension: usize,
        metric: VectorMetric,
    ) -> Result<(), GraphError> {
        let dimension = u32::try_from(dimension)
            .ok()
            .filter(|dimension| *dimension > 0)
            .ok_or(GraphError::InvalidIndex(
                "a vector index needs a dimension from one to u32::MAX",
            ))?;
        let label = self.intern_token(label)?;
        let property = self.intern_token(property)?;
        self.create_index(
            name,
            IndexDefinition {
                label,
                kind: IndexKind::Vector {
                    property,
                    dimension,
                    metric,
                },
            },
        )
    }

    fn create_index(&mut self, name: &str, definition: IndexDefinition) -> Result<(), GraphError> {
        if name.is_empty() {
            return Err(GraphError::InvalidIndex("an index needs a name"));
        }
        match self.index(name)? {
            Some(existing) if existing.definition == definition => return Ok(()),
            Some(_) => return Err(GraphError::IndexExists(name.to_owned())),
            None => {}
        }
        let id = u32::try_from(self.next_counter(keys::NEXT_INDEX_ID_KEY)?)
            .map_err(|_| GraphError::IdSpaceExhausted)?;
        let index = Index { id, definition };
        self.transaction
            .put(Table::Indexes, name.as_bytes(), &index.encode())?;
        if matches!(index.definition.kind, IndexKind::Keyword { .. }) {
            self.transaction.put(
                Table::KeywordTotals,
                &keys::index_prefix(id),
                &encode_totals(0, 0),
            )?;
        }
        for node in self.node_ids_with_label(index.definition.label)? {
            let record = self.node(node)?.ok_or(GraphError::NodeNotFound(node))?;
            if let Some(entry) = index.definition.entry(&record) {
                self.insert_entry(&index, node, &entry)?;
            }
        }
        Ok(())
    }

    /// Removes the index and all its entries. Returns whether there was one to remove.
    pub(crate) fn drop_index(&mut self, name: &str) -> Result<bool, GraphError> {
        let Some(index) = self.index(name)? else {
            return Ok(false);
        };
        self.transaction.delete(Table::Indexes, name.as_bytes())?;
        for table in ENTRY_TABLES {
            for (key, _) in self
                .transaction
                .scan_prefix(table, &keys::index_prefix(index.id))?
            {
                self.transaction.delete(table, &key)?;
            }
        }
        Ok(true)
    }

    /// Brings every index in line with a node that changed from `before` to `after`. `None`
    /// stands for a node that did not exist or no longer does.
    pub(crate) fn reindex_node(
        &mut self,
        node: NodeId,
        before: Option<&NodeRecord>,
        after: Option<&NodeRecord>,
    ) -> Result<(), GraphError> {
        for (_, index) in self.indexes()? {
            let definition = &index.definition;
            let covers = |record: &&NodeRecord| record.labels.contains(&definition.label);
            let before = before.filter(covers);
            let after = after.filter(covers);
            let unchanged = match (before, after) {
                (None, None) => true,
                (Some(before), Some(after)) => definition
                    .properties()
                    .iter()
                    .all(|key| before.properties.get(key) == after.properties.get(key)),
                _ => false,
            };
            if unchanged {
                continue;
            }
            if before.is_some() {
                self.remove_entry(&index, node)?;
            }
            if let Some(entry) = after.and_then(|record| definition.entry(record)) {
                self.insert_entry(&index, node, &entry)?;
            }
        }
        Ok(())
    }

    fn insert_entry(
        &mut self,
        index: &Index,
        node: NodeId,
        entry: &Entry,
    ) -> Result<(), GraphError> {
        for (table, key, value) in index.rows(node, entry) {
            self.transaction.put(table, &key, &value)?;
        }
        if let Entry::Keyword(document) = entry {
            self.adjust_totals(index.id, document_length(document), true)?;
        }
        Ok(())
    }

    fn remove_entry(&mut self, index: &Index, node: NodeId) -> Result<(), GraphError> {
        let entry_key = keys::index_entry_key(index.id, node);
        match index.definition.kind {
            IndexKind::Keyword { .. } => {
                let Some(bytes) = self.transaction.get(Table::KeywordDocuments, &entry_key)? else {
                    return Ok(());
                };
                let document = decode_document(&bytes)?;
                for term in document.keys() {
                    self.transaction.delete(
                        Table::KeywordPostings,
                        &keys::posting_key(index.id, term, node),
                    )?;
                }
                self.transaction
                    .delete(Table::KeywordDocuments, &entry_key)?;
                self.adjust_totals(index.id, document_length(&document), false)
            }
            IndexKind::Vector { .. } => Ok(self.transaction.delete(Table::Vectors, &entry_key)?),
        }
    }

    fn adjust_totals(&mut self, index: u32, length: u32, add: bool) -> Result<(), GraphError> {
        let key = keys::index_prefix(index);
        let bytes = self
            .transaction
            .get(Table::KeywordTotals, &key)?
            .ok_or(GraphError::Corrupt("a keyword index has no totals"))?;
        let (documents, total) = decode_totals(&bytes)?;
        let length = u64::from(length);
        let updated = if add {
            documents.checked_add(1).zip(total.checked_add(length))
        } else {
            documents.checked_sub(1).zip(total.checked_sub(length))
        };
        let (documents, total) =
            updated.ok_or(GraphError::Corrupt("keyword index totals out of range"))?;
        self.transaction
            .put(Table::KeywordTotals, &key, &encode_totals(documents, total))?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "index.test.rs"]
mod tests;
