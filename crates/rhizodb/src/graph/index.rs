use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;

use crate::storage::ReadTransaction;
use crate::storage::Table;
use crate::storage::WriteTransaction;
use crate::value::DecodeError;
use crate::value::PropertyValue;
use crate::value::Reader;
use crate::value::put_varint;

use super::ColumnReader;
use super::GraphError;
use super::GraphTx;
use super::NodeId;
use super::NodeRecord;
use super::Properties;
use super::RelationshipId;
use super::RelationshipRecord;
use super::ids::Label;
use super::ids::PropertyKey;
use super::ids::RelationshipType;
use super::ids::Token;
use super::keys;
use super::meta;
use super::tokenize;
use super::tokenize::Clause;
use super::tokenize::Purpose;
use super::tokenize::Term;
use super::value_key;
use super::value_key::KeyRange;

const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

const VALUE_DISTANCE: u32 = 3;

const LARGEST_SQUARED_LENGTH: f64 = f64::MAX / 4.0;
const SMALLEST_COSINE_SQUARED_LENGTH: f64 = f64::MIN_POSITIVE;

const TARGET_LABEL: u8 = 0;
const TARGET_RELATIONSHIP_TYPE: u8 = 1;

const KIND_KEYWORD: u8 = 0;
const KIND_VECTOR: u8 = 1;
const KIND_PROPERTY: u8 = 2;

const METRIC_COSINE: u8 = 0;
const METRIC_EUCLIDEAN: u8 = 1;
const METRIC_DOT_PRODUCT: u8 = 2;

/// The tables that hold index entries, each keyed first by index ID.
pub(crate) const ENTRY_TABLES: [Table; 5] = [
    Table::KeywordPostings,
    Table::KeywordDocuments,
    Table::KeywordTotals,
    Table::Vectors,
    Table::PropertyEntries,
];

/// How a vector index measures closeness. Every metric gives a score that is higher for closer
/// vectors, so results always come in descending score order. A vector so long that a score
/// could overflow is not indexed and cannot be searched for, under every metric.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::IndexTarget;
/// use rhizodb::Parameters;
/// use rhizodb::Value;
/// use rhizodb::VectorMetric;
///
/// let db = Database::in_memory()?;
/// db.create_vector_index("v", IndexTarget::Label("P"), "v", 2, VectorMetric::DotProduct)?;
/// db.execute("INSERT (:P {v: [1.0, 2.0]})", &Parameters::new())?;
/// let result = db.execute(
///     "CALL db.index.vector.queryNodes('v', [3.0, 4.0], 1) YIELD score RETURN score",
///     &Parameters::new(),
/// )?;
/// assert_eq!(result.rows(), [vec![Value::Float(11.0)]]);
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VectorMetric {
    /// The cosine of the angle between two vectors, which is also the score. A zero vector has no
    /// direction, so it is not indexed and cannot be searched for, and neither is a vector so
    /// short that the product of two lengths would underflow.
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
        let smallest = if self == Self::Cosine {
            SMALLEST_COSINE_SQUARED_LENGTH
        } else {
            0.0
        };
        (smallest..=LARGEST_SQUARED_LENGTH).contains(&dot(vector, vector))
    }
}

const LANES: usize = 4;

const ITERATOR_SUM_START: f64 = -0.0;

struct Batch {
    ids: [u64; LANES],
    numbers: Vec<[f64; LANES]>,
    len: usize,
}

impl Batch {
    fn new(dimension: usize) -> Self {
        Self {
            ids: [0; LANES],
            numbers: vec![[0.0; LANES]; dimension],
            len: 0,
        }
    }

    fn push(&mut self, id: u64, stored: &[[u8; 8]]) {
        for (row, bytes) in self.numbers.iter_mut().zip(stored) {
            row[self.len] = f64::from_be_bytes(*bytes);
        }
        self.ids[self.len] = id;
        self.len += 1;
    }

    fn is_full(&self) -> bool {
        self.len == LANES
    }

    fn score_into(
        &mut self,
        metric: VectorMetric,
        query: &[f64],
        query_norm: f64,
        scored: &mut Vec<(u64, f64)>,
    ) {
        let mut sums = [ITERATOR_SUM_START; LANES];
        let mut squares = [ITERATOR_SUM_START; LANES];
        match metric {
            VectorMetric::Cosine => {
                for (left, row) in query.iter().zip(&self.numbers) {
                    for lane in 0..LANES {
                        sums[lane] += left * row[lane];
                        squares[lane] += row[lane] * row[lane];
                    }
                }
            }
            VectorMetric::Euclidean => {
                for (left, row) in query.iter().zip(&self.numbers) {
                    for lane in 0..LANES {
                        sums[lane] += (left - row[lane]) * (left - row[lane]);
                    }
                }
            }
            VectorMetric::DotProduct => {
                for (left, row) in query.iter().zip(&self.numbers) {
                    for lane in 0..LANES {
                        sums[lane] += left * row[lane];
                    }
                }
            }
        }
        for lane in 0..self.len {
            let score = match metric {
                VectorMetric::Cosine => sums[lane] / (query_norm * squares[lane].sqrt()),
                VectorMetric::Euclidean => 0.0 - sums[lane].sqrt(),
                VectorMetric::DotProduct => sums[lane],
            };
            scored.push((self.ids[lane], score));
        }
        self.len = 0;
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

/// What an index covers: the nodes that carry a label, or the relationships of a type.
///
/// ```
/// use rhizodb::Database;
/// use rhizodb::IndexTarget;
///
/// let db = Database::in_memory()?;
/// db.create_property_index("person_email", IndexTarget::Label("Person"), "email")?;
/// db.create_property_index("follow_since", IndexTarget::RelationshipType("FOLLOWS"), "since")?;
/// # Ok::<(), rhizodb::Error>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexTarget<'a> {
    /// The nodes that carry this label.
    Label(&'a str),
    /// The relationships of this type.
    RelationshipType(&'a str),
}

/// The fields a search reads from the definition of the index it searches, which also fix
/// which kind of index it searches.
pub(crate) trait Searched: Sized {
    /// How an error names the index this search reads over nodes.
    const OVER_NODES: &'static str;

    /// How an error names the index this search reads over relationships.
    const OVER_RELATIONSHIPS: &'static str;

    /// The fields, or `None` when `kind` is another kind of index.
    fn fields(kind: &IndexKind) -> Option<Self>;
}

pub(crate) struct KeywordFields;

impl Searched for KeywordFields {
    const OVER_NODES: &'static str = "keyword index over nodes";
    const OVER_RELATIONSHIPS: &'static str = "keyword index over relationships";

    fn fields(kind: &IndexKind) -> Option<Self> {
        match kind {
            IndexKind::Keyword { .. } => Some(Self),
            IndexKind::Vector { .. } | IndexKind::Property { .. } => None,
        }
    }
}

pub(crate) struct VectorFields {
    dimension: u32,
    metric: VectorMetric,
}

impl Searched for VectorFields {
    const OVER_NODES: &'static str = "vector index over nodes";
    const OVER_RELATIONSHIPS: &'static str = "vector index over relationships";

    fn fields(kind: &IndexKind) -> Option<Self> {
        match kind {
            IndexKind::Vector {
                dimension, metric, ..
            } => Some(Self {
                dimension: *dimension,
                metric: *metric,
            }),
            IndexKind::Keyword { .. } | IndexKind::Property { .. } => None,
        }
    }
}

/// Whether an index holds nodes or relationships.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ElementKind {
    Node,
    Relationship,
}

/// What a stored index covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Label(Token<Label>),
    RelationshipType(Token<RelationshipType>),
}

impl Target {
    pub(crate) fn element_kind(self) -> ElementKind {
        match self {
            Self::Label(_) => ElementKind::Node,
            Self::RelationshipType(_) => ElementKind::Relationship,
        }
    }

    fn covered<'a>(self, element: Element<'a>) -> Option<&'a Properties> {
        match (self, element) {
            (Self::Label(label), Element::Node(record)) if record.labels.contains(&label) => {
                Some(&record.properties)
            }
            (Self::RelationshipType(rel_type), Element::Relationship(record))
                if record.rel_type == rel_type =>
            {
                Some(&record.properties)
            }
            _ => None,
        }
    }
}

/// A stored node or relationship, as an index reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Element<'a> {
    Node(&'a NodeRecord),
    Relationship(&'a RelationshipRecord),
}

/// What an index reads from each element it covers. Keyword properties are sorted and
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
    Property {
        property: Token<PropertyKey>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexDefinition {
    pub(crate) target: Target,
    pub(crate) kind: IndexKind,
}

/// A stored index. Its entries are keyed by `id`, so a dropped index's name can be reused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Index {
    pub(crate) id: u32,
    pub(crate) definition: IndexDefinition,
}

/// A keyword document: the positions, in ascending order, at which each term occurs in the
/// indexed properties of one element. How often a term occurs is how many positions it has.
pub(crate) type Document = BTreeMap<String, Vec<u32>>;

/// What one element contributes to one index.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Entry {
    Keyword(Document),
    Vector(Vec<f64>),
    /// The key of the property value, as `value_key::key` gives it.
    Property(Vec<u8>),
}

/// A stored row: the table, the key, and the value.
pub(crate) type Row = (Table, Vec<u8>, Vec<u8>);

impl Index {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.id.to_be_bytes());
        match self.definition.target {
            Target::Label(label) => {
                out.push(TARGET_LABEL);
                out.extend_from_slice(&label.id().to_be_bytes());
            }
            Target::RelationshipType(rel_type) => {
                out.push(TARGET_RELATIONSHIP_TYPE);
                out.extend_from_slice(&rel_type.id().to_be_bytes());
            }
        }
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
            IndexKind::Property { property } => {
                out.push(KIND_PROPERTY);
                out.extend_from_slice(&property.id().to_be_bytes());
            }
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let (id, target, reader) = Self::decode_target(bytes)?;
        Self::decode_kind(id, target, reader)
    }

    fn decode_target(bytes: &[u8]) -> Result<(u32, Target, Reader<'_>), DecodeError> {
        let mut reader = Reader::new(bytes);
        let id = reader.u32()?;
        let target = match reader.u8()? {
            TARGET_LABEL => Target::Label(Token::new(reader.u32()?)),
            TARGET_RELATIONSHIP_TYPE => Target::RelationshipType(Token::new(reader.u32()?)),
            other => return Err(DecodeError::UnknownTag(other)),
        };
        Ok((id, target, reader))
    }

    /// Reads a definition stored before format version 5, which names a label and no target
    /// tag, because every index then covered the nodes that carry a label.
    pub(crate) fn decode_label_only(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader::new(bytes);
        let id = reader.u32()?;
        let target = Target::Label(Token::new(reader.u32()?));
        Self::decode_kind(id, target, reader)
    }

    fn decode_kind(id: u32, target: Target, mut reader: Reader<'_>) -> Result<Self, DecodeError> {
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
            KIND_PROPERTY => IndexKind::Property {
                property: Token::new(reader.u32()?),
            },
            other => return Err(DecodeError::UnknownTag(other)),
        };
        reader.finish()?;
        Ok(Self {
            id,
            definition: IndexDefinition { target, kind },
        })
    }

    /// The rows that hold `entry` for the element with ID `element`, leaving out the keyword
    /// totals.
    pub(crate) fn rows(&self, element: u64, entry: &Entry) -> Vec<Row> {
        let entry_key = keys::index_entry_key(self.id, element).to_vec();
        match entry {
            Entry::Keyword(document) => {
                let length = document_length(document);
                let mut rows: Vec<Row> = document
                    .iter()
                    .map(|(term, positions)| {
                        (
                            Table::KeywordPostings,
                            keys::posting_key(self.id, term, element),
                            encode_posting(positions, length),
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
            Entry::Property(value) => vec![(
                Table::PropertyEntries,
                keys::property_entry_key(self.id, value, element),
                Vec::new(),
            )],
        }
    }
}

impl IndexDefinition {
    /// What `element` contributes, or `None` when the target does not cover it or it holds
    /// nothing to index. A keyword document numbers the terms of its string values in property
    /// order, list items in list order, and starts each value three positions past the last
    /// term of the one before. Neighbouring query terms are at most two positions apart, so a
    /// phrase never matches across two values.
    pub(crate) fn entry(&self, element: Element<'_>) -> Option<Entry> {
        self.kind.entry(self.target.covered(element)?)
    }
}

impl IndexKind {
    /// The properties the index reads.
    pub(crate) fn properties(&self) -> &[Token<PropertyKey>] {
        match self {
            IndexKind::Keyword { properties } => properties,
            IndexKind::Vector { property, .. } | IndexKind::Property { property } => {
                std::slice::from_ref(property)
            }
        }
    }

    fn entry(&self, values: &Properties) -> Option<Entry> {
        match self {
            IndexKind::Keyword { properties } => {
                let mut document = Document::new();
                let mut start = 0u32;
                let mut add = |text: &str| {
                    let terms = tokenize::terms(text, Purpose::Document);
                    let Some(last) = terms.iter().map(|term| term.position).max() else {
                        return;
                    };
                    for term in terms {
                        document
                            .entry(term.text)
                            .or_default()
                            .push(start.saturating_add(term.position));
                    }
                    start = start.saturating_add(last).saturating_add(VALUE_DISTANCE);
                };
                for property in properties {
                    match values.get(property) {
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
                for positions in document.values_mut() {
                    positions.sort_unstable();
                }
                (!document.is_empty()).then_some(Entry::Keyword(document))
            }
            IndexKind::Vector {
                property,
                dimension,
                metric,
            } => {
                let Some(PropertyValue::List(items)) = values.get(property) else {
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
            IndexKind::Property { property } => {
                value_key::key(values.get(property)?).map(Entry::Property)
            }
        }
    }
}

/// The number of terms in a document, counting repeats.
pub(crate) fn document_length(document: &Document) -> u32 {
    document.values().fold(0u32, |total, positions| {
        total.saturating_add(frequency(positions))
    })
}

fn frequency(positions: &[u32]) -> u32 {
    u32::try_from(positions.len()).unwrap_or(u32::MAX)
}

/// The keyword totals row value: how many elements the index holds and their summed length.
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
    for (term, positions) in document {
        put_varint(&mut out, term.len());
        out.extend_from_slice(term.as_bytes());
        out.extend_from_slice(&frequency(positions).to_be_bytes());
    }
    out
}

fn decode_document(bytes: &[u8]) -> Result<Vec<(String, u32)>, DecodeError> {
    let mut reader = Reader::new(bytes);
    let count = reader.varint()?;
    let mut document = Vec::new();
    for _ in 0..count {
        let length = reader.varint()?;
        let term = std::str::from_utf8(reader.take(length)?)
            .map_err(|_| DecodeError::InvalidUtf8)?
            .to_owned();
        document.push((term, reader.u32()?));
    }
    reader.finish()?;
    Ok(document)
}

#[derive(Debug)]
struct Posting {
    positions: Vec<u32>,
    length: u32,
}

fn encode_posting(positions: &[u32], length: u32) -> Vec<u8> {
    let mut out = frequency(positions).to_be_bytes().to_vec();
    out.extend_from_slice(&length.to_be_bytes());
    let mut previous = 0u32;
    for position in positions {
        put_varint(
            &mut out,
            usize::try_from(position.saturating_sub(previous)).unwrap_or(usize::MAX),
        );
        previous = *position;
    }
    out
}

fn decode_counts(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let mut reader = Reader::new(bytes);
    let counts = (reader.u32()?, reader.u32()?);
    reader.take(usize::try_from(counts.0).map_err(|_| DecodeError::LengthOverflow)?)?;
    Ok(counts)
}

fn decode_posting(bytes: &[u8]) -> Result<Posting, DecodeError> {
    let mut reader = Reader::new(bytes);
    let frequency = reader.u32()?;
    let length = reader.u32()?;
    let mut positions = Vec::new();
    let mut position = 0u32;
    for _ in 0..frequency {
        let step = u32::try_from(reader.varint()?).map_err(|_| DecodeError::LengthOverflow)?;
        position = position
            .checked_add(step)
            .ok_or(DecodeError::LengthOverflow)?;
        positions.push(position);
    }
    reader.finish()?;
    Ok(Posting { positions, length })
}

fn stored_vector(bytes: &[u8], dimension: usize) -> Result<&[[u8; 8]], DecodeError> {
    let (numbers, rest) = bytes.as_chunks::<8>();
    match numbers.len().cmp(&dimension) {
        Ordering::Less => Err(DecodeError::UnexpectedEnd),
        Ordering::Equal if rest.is_empty() => Ok(numbers),
        Ordering::Equal | Ordering::Greater => Err(DecodeError::TrailingBytes),
    }
}

fn bm25(idf: f64, frequency: u32, length: u32, average_length: f64) -> f64 {
    let frequency = f64::from(frequency);
    let relative_length = f64::from(length) / average_length;
    idf * frequency * (BM25_K1 + 1.0)
        / (frequency + BM25_K1 * (1.0 - BM25_B + BM25_B * relative_length))
}

fn ranking(left: &(u64, f64), right: &(u64, f64)) -> Ordering {
    left.1
        .is_nan()
        .cmp(&right.1.is_nan())
        .then_with(|| right.1.total_cmp(&left.1))
        .then_with(|| left.0.cmp(&right.0))
}

fn best(mut scored: Vec<(u64, f64)>, limit: usize) -> Vec<(u64, f64)> {
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

    /// The stored bytes of every index definition, read from storage at most once per transaction.
    /// The properties of the node indexes for which `covers` holds, given each index's label
    /// and properties.
    pub(crate) fn node_index_keys(
        &self,
        mut covers: impl FnMut(Token<Label>, &[Token<PropertyKey>]) -> bool,
    ) -> Result<BTreeSet<Token<PropertyKey>>, GraphError> {
        let mut keys = BTreeSet::new();
        for bytes in self.index_definitions()?.iter() {
            let (id, target, reader) = Index::decode_target(bytes)?;
            if let Target::Label(label) = target {
                let index = Index::decode_kind(id, target, reader)?;
                let properties = index.definition.kind.properties();
                if covers(label, properties) {
                    keys.extend(properties.iter().copied());
                }
            }
        }
        Ok(keys)
    }

    pub(crate) fn index_definitions(&self) -> Result<Arc<[Vec<u8>]>, GraphError> {
        if let Some(definitions) = self.index_definitions.get() {
            return Ok(Arc::clone(definitions));
        }
        let definitions: Arc<[Vec<u8>]> = self
            .transaction
            .scan_prefix(Table::Indexes, &[])?
            .into_iter()
            .map(|(_, bytes)| bytes)
            .collect();
        Ok(Arc::clone(
            self.index_definitions.get_or_init(|| definitions),
        ))
    }

    fn index(&self, name: &str) -> Result<Option<Index>, GraphError> {
        let bytes = self.transaction.get(Table::Indexes, name.as_bytes())?;
        Ok(bytes.map(|bytes| Index::decode(&bytes)).transpose()?)
    }

    fn searched_index<F: Searched>(
        &self,
        name: &str,
        element: ElementKind,
    ) -> Result<(u32, F), GraphError> {
        let what = match element {
            ElementKind::Node => F::OVER_NODES,
            ElementKind::Relationship => F::OVER_RELATIONSHIPS,
        };
        self.index(name)?
            .filter(|index| index.definition.target.element_kind() == element)
            .and_then(|index| Some((index.id, F::fields(&index.definition.kind)?)))
            .ok_or_else(|| GraphError::IndexNotFound(name.to_owned(), what))
    }

    /// The IDs of the elements of kind `element` that best match `text` in the keyword index
    /// `name`, ranked by BM25. An element matches when it matches any clause of the text. A
    /// phrase is ranked as one term whose occurrences are the phrase's, and a prefix by the
    /// best of the terms it starts.
    pub(crate) fn keyword_search(
        &self,
        name: &str,
        element: ElementKind,
        text: &str,
        limit: usize,
    ) -> Result<Vec<(u64, f64)>, GraphError> {
        let (id, KeywordFields) = self.searched_index(name, element)?;
        let totals = self
            .transaction
            .get(Table::KeywordTotals, &keys::index_prefix(id))?
            .ok_or(GraphError::Corrupt("a keyword index has no totals"))?;
        let (documents, length) = decode_totals(&totals)?;
        if documents == 0 {
            return Ok(Vec::new());
        }
        let average_length = length as f64 / documents as f64;
        let score = |matches: BTreeMap<u64, (u32, u32)>| {
            let matching = matches.len() as f64;
            let idf = (1.0 + (documents as f64 - matching + 0.5) / (matching + 0.5)).ln();
            matches
                .into_iter()
                .map(move |(element, (frequency, length))| {
                    (element, bm25(idf, frequency, length, average_length))
                })
        };
        let mut scores: BTreeMap<u64, f64> = BTreeMap::new();
        for clause in tokenize::clauses(text) {
            let clause_scores: BTreeMap<u64, f64> = match &clause {
                Clause::Term(term) => {
                    let mut counts = BTreeMap::new();
                    self.transaction.visit_prefix(
                        Table::KeywordPostings,
                        &keys::posting_prefix(id, term),
                        |key, value| -> Result<(), GraphError> {
                            counts.insert(keys::parse_posting_element(key)?, decode_counts(value)?);
                            Ok(())
                        },
                    )?;
                    score(counts).collect()
                }
                Clause::Prefix(prefix) => {
                    let mut best_scores: BTreeMap<u64, f64> = BTreeMap::new();
                    let mut current: Option<Vec<u8>> = None;
                    let mut counts = BTreeMap::new();
                    let mut merge = |counts: BTreeMap<u64, (u32, u32)>| {
                        for (element, value) in score(counts) {
                            let held = best_scores.entry(element).or_insert(value);
                            *held = held.max(value);
                        }
                    };
                    self.transaction.visit_prefix(
                        Table::KeywordPostings,
                        &keys::term_prefix(id, prefix),
                        |key, value| -> Result<(), GraphError> {
                            let term = keys::parse_posting_term(key)?.as_bytes();
                            if current.as_deref() != Some(term) {
                                merge(std::mem::take(&mut counts));
                                current = Some(term.to_vec());
                            }
                            counts.insert(keys::parse_posting_element(key)?, decode_counts(value)?);
                            Ok(())
                        },
                    )?;
                    merge(counts);
                    best_scores
                }
                Clause::Phrase(terms) => score(self.phrase_matches(id, terms)?).collect(),
            };
            for (element, value) in clause_scores {
                *scores.entry(element).or_insert(0.0) += value;
            }
        }
        Ok(best(scores.into_iter().collect(), limit))
    }

    fn postings(&self, index: u32, term: &str) -> Result<BTreeMap<u64, Posting>, GraphError> {
        let mut postings = BTreeMap::new();
        self.transaction.visit_prefix(
            Table::KeywordPostings,
            &keys::posting_prefix(index, term),
            |key, value| -> Result<(), GraphError> {
                postings.insert(keys::parse_posting_element(key)?, decode_posting(value)?);
                Ok(())
            },
        )?;
        Ok(postings)
    }

    fn phrase_matches(
        &self,
        index: u32,
        terms: &[Term],
    ) -> Result<BTreeMap<u64, (u32, u32)>, GraphError> {
        let mut lists = Vec::new();
        for term in terms {
            lists.push((term.position, self.postings(index, &term.text)?));
        }
        let mut matches = BTreeMap::new();
        let Some((rarest, (rarest_offset, anchor))) = lists
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, postings))| postings.len())
        else {
            return Ok(matches);
        };
        for (element, posting) in anchor {
            let Some(others) = lists
                .iter()
                .enumerate()
                .filter(|(position, _)| *position != rarest)
                .map(|(_, (offset, postings))| Some((*offset, postings.get(element)?)))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let occurrences = posting
                .positions
                .iter()
                .filter_map(|start| start.checked_sub(*rarest_offset))
                .filter(|base| {
                    others.iter().all(|(offset, other)| {
                        base.checked_add(*offset)
                            .is_some_and(|at| other.positions.binary_search(&at).is_ok())
                    })
                })
                .count();
            if occurrences > 0 {
                let occurrences = u32::try_from(occurrences).unwrap_or(u32::MAX);
                matches.insert(*element, (occurrences, posting.length));
            }
        }
        Ok(matches)
    }

    /// The IDs of the elements of kind `element` whose vectors in the vector index `name` are
    /// closest to `query`, found by scoring every vector in the index.
    pub(crate) fn vector_search(
        &self,
        name: &str,
        element: ElementKind,
        query: &[f64],
        limit: usize,
    ) -> Result<Vec<(u64, f64)>, GraphError> {
        let (id, VectorFields { dimension, metric }) = self.searched_index(name, element)?;
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
                "the query vector is too long, or for a cosine index too short, to score"
                    .to_owned(),
            ));
        }
        let query_norm = norm(query);
        let mut scored = Vec::new();
        let mut batch = Batch::new(query.len());
        self.transaction.visit_prefix(
            Table::Vectors,
            &keys::index_prefix(id),
            |key, value| -> Result<(), GraphError> {
                batch.push(
                    keys::parse_index_entry_key(key)?,
                    stored_vector(value, query.len())?,
                );
                if batch.is_full() {
                    batch.score_into(metric, query, query_norm, &mut scored);
                }
                Ok(())
            },
        )?;
        batch.score_into(metric, query, query_norm, &mut scored);
        Ok(best(scored, limit))
    }

    /// The ID of a property index over `property` of the elements `target` covers, if there is
    /// one.
    pub(crate) fn property_index(
        &self,
        target: Target,
        property: Token<PropertyKey>,
    ) -> Result<Option<u32>, GraphError> {
        let wanted = IndexDefinition {
            target,
            kind: IndexKind::Property { property },
        };
        for bytes in self.index_definitions()?.iter() {
            let index = Index::decode(bytes)?;
            if index.definition == wanted {
                return Ok(Some(index.id));
            }
        }
        Ok(None)
    }

    /// The IDs, ascending and each once, of the elements in the property index `index` whose
    /// value keys lie in any of `ranges`.
    pub(crate) fn property_seek(
        &self,
        index: u32,
        ranges: &[KeyRange],
    ) -> Result<Vec<u64>, GraphError> {
        let mut elements = Vec::new();
        for range in ranges {
            let entries = self.transaction.scan_range(
                Table::PropertyEntries,
                &keys::property_entry_bound(index, &range.start),
                &keys::property_entry_bound(index, &range.end),
            )?;
            for (key, _) in entries {
                elements.push(keys::parse_property_entry_element(&key)?);
            }
        }
        elements.sort_unstable();
        elements.dedup();
        Ok(elements)
    }

    fn covered_ids(&self, target: Target) -> Result<Vec<u64>, GraphError> {
        match target {
            Target::Label(label) => Ok(self
                .node_ids_with_label(label)?
                .into_iter()
                .map(|node| node.0)
                .collect()),
            Target::RelationshipType(covered) => {
                let mut ids = Vec::new();
                self.visit_relationship_records(|id, record| {
                    if RelationshipRecord::decode(record)?.rel_type == covered {
                        ids.push(id.0);
                    }
                    Ok(())
                })?;
                Ok(ids)
            }
        }
    }

    fn covered_entry(
        &self,
        reader: &mut ColumnReader,
        definition: &IndexDefinition,
        id: u64,
    ) -> Result<Option<Entry>, GraphError> {
        Ok(match definition.target {
            Target::Label(label) => {
                let (group, slot) = meta::locate(NodeId(id));
                let mut record = NodeRecord {
                    labels: BTreeSet::from([label]),
                    properties: Properties::new(),
                };
                for key in definition.kind.properties() {
                    if let Some(value) = self.column_value(reader, group, slot, *key)? {
                        record.properties.insert(*key, value);
                    }
                }
                definition.entry(Element::Node(&record))
            }
            Target::RelationshipType(_) => {
                let relationship = RelationshipId(id);
                let record = self
                    .relationship(relationship)?
                    .ok_or(GraphError::RelationshipNotFound(relationship))?;
                definition.entry(Element::Relationship(&record))
            }
        })
    }
}

impl<T: WriteTransaction> GraphTx<T> {
    fn intern_target(&mut self, target: IndexTarget<'_>) -> Result<Target, GraphError> {
        Ok(match target {
            IndexTarget::Label(label) => Target::Label(self.intern_token(label)?),
            IndexTarget::RelationshipType(rel_type) => {
                Target::RelationshipType(self.intern_token(rel_type)?)
            }
        })
    }

    /// Creates a keyword index over the string properties `properties` of the elements
    /// `target` covers, filled from the elements already there.
    pub(crate) fn create_keyword_index(
        &mut self,
        name: &str,
        target: IndexTarget<'_>,
        properties: &[&str],
    ) -> Result<(), GraphError> {
        if properties.is_empty() {
            return Err(GraphError::InvalidIndex(
                "a keyword index needs at least one property",
            ));
        }
        let target = self.intern_target(target)?;
        let mut tokens = properties
            .iter()
            .map(|property| self.intern_token(property))
            .collect::<Result<Vec<_>, _>>()?;
        tokens.sort();
        tokens.dedup();
        self.create_index(
            name,
            IndexDefinition {
                target,
                kind: IndexKind::Keyword { properties: tokens },
            },
        )
    }

    /// Creates a vector index over the list property `property` of the elements `target`
    /// covers, filled from the elements already there.
    pub(crate) fn create_vector_index(
        &mut self,
        name: &str,
        target: IndexTarget<'_>,
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
        let target = self.intern_target(target)?;
        let property = self.intern_token(property)?;
        self.create_index(
            name,
            IndexDefinition {
                target,
                kind: IndexKind::Vector {
                    property,
                    dimension,
                    metric,
                },
            },
        )
    }

    /// Creates a property index over the property `property` of the elements `target` covers,
    /// filled from the elements already there.
    pub(crate) fn create_property_index(
        &mut self,
        name: &str,
        target: IndexTarget<'_>,
        property: &str,
    ) -> Result<(), GraphError> {
        let target = self.intern_target(target)?;
        let property = self.intern_token(property)?;
        self.create_index(
            name,
            IndexDefinition {
                target,
                kind: IndexKind::Property { property },
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
        self.put_index_definition(name.as_bytes(), &index)?;
        self.fill(&index)
    }

    fn forget_index_definitions(&mut self) {
        self.index_definitions.take();
        if let Some(catalog) = &mut self.catalog {
            catalog.changed = true;
        }
    }

    fn put_index_definition(&mut self, name: &[u8], index: &Index) -> Result<(), GraphError> {
        self.forget_index_definitions();
        Ok(self
            .transaction
            .put(Table::Indexes, name, &index.encode())?)
    }

    fn fill(&mut self, index: &Index) -> Result<(), GraphError> {
        if matches!(index.definition.kind, IndexKind::Keyword { .. }) {
            self.transaction.put(
                Table::KeywordTotals,
                &keys::index_prefix(index.id),
                &encode_totals(0, 0),
            )?;
        }
        for element in self.covered_ids(index.definition.target)? {
            let entry = self
                .with_reader(|tx, reader| tx.covered_entry(reader, &index.definition, element))?;
            if let Some(entry) = entry {
                self.insert_entry(index, element, &entry)?;
            }
        }
        Ok(())
    }

    /// Rewrites every index definition stored before format version 5, which names a label and
    /// no target tag, in the current layout.
    pub(crate) fn tag_index_targets(&mut self) -> Result<(), GraphError> {
        for (name, bytes) in self.transaction.scan_prefix(Table::Indexes, &[])? {
            let index = Index::decode_label_only(&bytes)?;
            self.put_index_definition(&name, &index)?;
        }
        Ok(())
    }

    /// Records the current tokenizer, first rebuilding every keyword index from the elements it
    /// covers when `stale` is set or the stored entries came from another tokenizer. Returns
    /// whether anything was written.
    pub(crate) fn adopt_current_tokenizer(&mut self, stale: bool) -> Result<bool, GraphError> {
        let fingerprint = tokenize::fingerprint();
        let stored = self.transaction.get(Table::Metadata, keys::TOKENIZER_KEY)?;
        if !stale && stored.as_deref() == Some(fingerprint.as_slice()) {
            return Ok(false);
        }
        for (_, index) in self.indexes()? {
            if !matches!(index.definition.kind, IndexKind::Keyword { .. }) {
                continue;
            }
            self.clear(&index)?;
            self.fill(&index)?;
        }
        self.transaction
            .put(Table::Metadata, keys::TOKENIZER_KEY, &fingerprint)?;
        Ok(true)
    }

    /// Removes the index and all its entries. Returns whether there was one to remove.
    pub(crate) fn drop_index(&mut self, name: &str) -> Result<bool, GraphError> {
        let Some(index) = self.index(name)? else {
            return Ok(false);
        };
        self.forget_index_definitions();
        self.transaction.delete(Table::Indexes, name.as_bytes())?;
        self.clear(&index)?;
        Ok(true)
    }

    fn clear(&mut self, index: &Index) -> Result<(), GraphError> {
        for table in ENTRY_TABLES {
            for (key, _) in self
                .transaction
                .scan_prefix(table, &keys::index_prefix(index.id))?
            {
                self.transaction.delete(table, &key)?;
            }
        }
        Ok(())
    }

    /// Brings every index in line with the element with ID `element` that changed from
    /// `before` to `after`, both records of one node or of one relationship. `None` stands for
    /// an element that did not exist or no longer does. Only the definitions whose target
    /// covers `before` or `after` are decoded past their target.
    pub(crate) fn reindex(
        &mut self,
        element: u64,
        before: Option<Element<'_>>,
        after: Option<Element<'_>>,
    ) -> Result<(), GraphError> {
        for bytes in self.index_definitions()?.iter() {
            let (id, target, reader) = Index::decode_target(bytes)?;
            let covers = |record: &Element<'_>| target.covered(*record).is_some();
            let before = before.filter(covers);
            let after = after.filter(covers);
            if before.is_none() && after.is_none() {
                continue;
            }
            let index = Index::decode_kind(id, target, reader)?;
            let definition = &index.definition;
            let rows = |entry: &Entry| index.rows(element, entry);
            let old = before.and_then(|record| definition.entry(record));
            let new = after.and_then(|record| definition.entry(record));
            if old.as_ref().map(rows) == new.as_ref().map(rows) {
                continue;
            }
            if before.is_some() {
                self.remove_entry(&index, element, old.as_ref())?;
            }
            if let Some(entry) = new {
                self.insert_entry(&index, element, &entry)?;
            }
        }
        Ok(())
    }

    fn insert_entry(
        &mut self,
        index: &Index,
        element: u64,
        entry: &Entry,
    ) -> Result<(), GraphError> {
        for (table, key, value) in index.rows(element, entry) {
            self.transaction.put(table, &key, &value)?;
        }
        if let Entry::Keyword(document) = entry {
            self.adjust_totals(index.id, document_length(document), true)?;
        }
        Ok(())
    }

    fn remove_entry(
        &mut self,
        index: &Index,
        element: u64,
        old: Option<&Entry>,
    ) -> Result<(), GraphError> {
        let entry_key = keys::index_entry_key(index.id, element);
        match index.definition.kind {
            IndexKind::Keyword { .. } => {
                let Some(bytes) = self.transaction.get(Table::KeywordDocuments, &entry_key)? else {
                    return Ok(());
                };
                let mut length = 0u32;
                for (term, frequency) in decode_document(&bytes)? {
                    self.transaction.delete(
                        Table::KeywordPostings,
                        &keys::posting_key(index.id, &term, element),
                    )?;
                    length = length.saturating_add(frequency);
                }
                self.transaction
                    .delete(Table::KeywordDocuments, &entry_key)?;
                self.adjust_totals(index.id, length, false)
            }
            IndexKind::Vector { .. } => Ok(self.transaction.delete(Table::Vectors, &entry_key)?),
            IndexKind::Property { .. } => {
                if let Some(Entry::Property(value)) = old {
                    self.transaction.delete(
                        Table::PropertyEntries,
                        &keys::property_entry_key(index.id, value, element),
                    )?;
                }
                Ok(())
            }
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
