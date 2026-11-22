# Indexes

Decides how keyword and vector indexes are stored, kept current, and searched.

## Background

A host that keeps people, places, and tasks for years must find them by words and by meaning, not only by label and pattern. An index kept outside the graph can drift from the data it describes, so the indexes live in the database and follow its transactions.

## Decisions

- Keyword and vector indexes live inside the graph, in the same storage as the data they index.
- An index is updated in the same write transaction as the labels and properties it covers, so a search never sees an entry that disagrees with committed data.
- A rolled-back transaction leaves every index as it was.
- A search inside a write transaction sees that transaction's own writes.
- An index covers the nodes that carry one label.
- Index names are case-sensitive and unique across both kinds.
- The host creates and drops indexes with `Database` methods, each in a write transaction of its own.
- Creating an index fills it from the nodes already there, in the transaction that records it.
- Creating an index that exists with the same definition changes nothing.
- Creating an index under a name that another definition holds fails with its own error kind.
- Dropping an index removes its definition and every entry in one transaction.
- Dropping an index that does not exist succeeds and reports that nothing was dropped.
- Each index has an internal ID that keys its entries, so a dropped index's name can be reused at once.
- Queries search an index with the built-in procedures `db.index.keyword.query(index, query, limit)` and `db.index.vector.query(index, vector, limit)`.
- Both index procedures yield `node` and `score`.
- Searching an index that does not exist, or one of the other kind, fails with its own error kind when the query runs.
- A null argument to an index procedure gives no rows.
- A negative limit fails the query.
- Results come in descending score order.
- Equal scores come in ascending node ID order.
- A keyword index covers one or more properties, and a node's document is the terms of all of them together.
- A keyword index reads strings and lists of strings, and ignores values of other types.
- Text splits into words at Unicode word boundaries and is lowercased.
- A run of Hangul, Han, Hiragana, or Katakana is indexed as its overlapping character pairs and its single characters.
- A query run of such characters looks for its character pairs, or for its one character when it has one.
- A keyword query matches a node that holds any of its terms.
- Keyword matches are ranked by BM25.
- Each keyword index keeps its node count and total term count, and each posting keeps its node's term count, so ranking reads only the postings of the query terms.
- Each indexed node keeps the terms it was indexed under, and removing it from the index deletes those terms rather than tokenizing again.
- The database records which tokenizer built its keyword indexes, as the tokenizer's version and its terms for a fixed probe text that exercises the Unicode tables it depends on.
- Opening a database recorded with another tokenizer rebuilds every keyword index in the opening write transaction and records the current tokenizer.
- Changing what the tokenizer produces on purpose raises its version.
- The word segmentation crate is pinned to one exact version, so a host's lockfile cannot change the tokenizer.
- A vector index covers one property and has a fixed dimension and one metric: cosine, euclidean, or dot product.
- A node is in a vector index when the property is a list of finite numbers of the index's dimension whose length is small enough that no score can overflow, and under cosine large enough that no product of lengths can underflow.
- Any other value leaves the node out of the vector index without an error.
- A query vector that breaks the same rules fails the query.
- Vectors are stored in a table of their own, so a search reads no node records.
- Vector search is exact and scores every vector in the index.
- The score is higher for closer vectors: the cosine similarity, the dot product, or the negative euclidean distance.
- The integrity check derives every index entry from the nodes and compares it with the stored entries byte for byte.

## Open

- An approximate vector structure, such as HNSW, for indexes too large to scan
- Indexes over relationship properties
- Unicode normalization of indexed text, so composed and decomposed forms of the same text match
- Word segmentation for scripts written without spaces outside Hangul, Han, and kana, such as Thai
- Phrase, prefix, and fuzzy keyword queries
