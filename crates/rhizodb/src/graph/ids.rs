use std::marker::PhantomData;

/// Identifies a node. Node IDs only grow and are never reused after a delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u64);

impl NodeId {
    /// The ID as a number, stable for the life of the database.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// The ID whose number is `id`, as [`NodeId::as_u64`] returned it, for a host that stored
    /// the number. An ID that names no node compares unequal to every node's ID.
    pub fn from_u64(id: u64) -> Self {
        Self(id)
    }
}

/// Identifies a relationship. Relationship IDs only grow and are never reused after a delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelationshipId(pub(crate) u64);

impl RelationshipId {
    /// The ID as a number, stable for the life of the database.
    pub fn as_u64(self) -> u64 {
        self.0
    }

    /// The ID whose number is `id`, as [`RelationshipId::as_u64`] returned it, for a host that stored
    /// the number. An ID that names no relationship compares unequal to every relationship's ID.
    pub fn from_u64(id: u64) -> Self {
        Self(id)
    }
}

/// One of the three name spaces interned to tokens. Each has its own token counter.
pub(crate) trait TokenKind: Copy {
    const TAG: u8;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Label;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RelationshipType;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PropertyKey;

impl TokenKind for Label {
    const TAG: u8 = 0;
}

impl TokenKind for RelationshipType {
    const TAG: u8 = 1;
}

impl TokenKind for PropertyKey {
    const TAG: u8 = 2;
}

/// A name interned to a `u32`. The kind parameter stops a label token from standing in for a key token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Token<K> {
    id: u32,
    kind: PhantomData<K>,
}

impl<K> Token<K> {
    pub(crate) fn new(id: u32) -> Self {
        Self {
            id,
            kind: PhantomData,
        }
    }

    pub(crate) fn id(self) -> u32 {
        self.id
    }
}
