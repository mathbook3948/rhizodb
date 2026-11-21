use std::marker::PhantomData;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct NodeId(pub(crate) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RelationshipId(pub(crate) u64);

/// One of the three name spaces interned to tokens. Each has its own token counter.
pub(crate) trait TokenKind {
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
