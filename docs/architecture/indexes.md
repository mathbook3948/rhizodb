# Indexes

Decides how keyword and vector indexes are stored, kept current, and searched.

## Background

A host that keeps people, places, and tasks for years must find them by words and by meaning, not only by label and pattern. An index kept outside the graph can drift from the data it describes, so the indexes live in the database and follow its transactions.

## Decisions

- Keyword and vector indexes live inside the graph, in the same storage as the data they index.
- An index is updated in the same write transaction as the labels and properties it covers, so a search never sees an entry that disagrees with committed data.
- A rolled-back transaction leaves every index as it was.
- A search inside a write transaction sees that transaction's own writes.
- An index covers the nodes that carry one label or the relationships of one type.
- An index entry is keyed by its element's ID alone.
- Index names are case-sensitive and unique across both kinds and both kinds of element.
- The host creates and drops indexes with `Database` methods, each in a write transaction of its own.
- The same `Database` method creates an index of one kind over a label or over a relationship type, named by an `IndexTarget`.
- Creating an index fills it from the nodes or relationships already there, in the transaction that records it.
- Creating an index that exists with the same definition changes nothing.
- Creating an index under a name that another definition holds fails with its own error kind.
- Dropping an index removes its definition and every entry in one transaction.
- Dropping an index that does not exist succeeds and reports that nothing was dropped.
- Each index has an internal ID that keys its entries, so a dropped index's name can be reused at once.
- Queries search an index over a label with the built-in procedures `db.index.keyword.queryNodes(index, query, limit)` and `db.index.vector.queryNodes(index, vector, limit)`.
- Queries search an index over a relationship type with `db.index.keyword.queryRelationships` and `db.index.vector.queryRelationships`, which take the same arguments.
- The procedures over a label yield `node` and `score`, and those over a relationship type yield `relationship` and `score`.
- Searching an index that does not exist, one of the other kind, or one over the other kind of element fails with its own error kind when the query runs.
- A null argument to an index procedure gives no rows.
- A negative limit fails the query.
- Results come in descending score order.
- Equal scores come in ascending element ID order.
- A keyword index covers one or more properties, and an element's document is the terms of all of them together.
- A keyword index reads strings and lists of strings, and ignores values of other types.
- Text is put in Unicode normalization form NFKC before it is split.
- Text splits into words at Unicode word boundaries.
- Words are lowercased.
- A word starts and ends at a grapheme cluster whose first character is a letter or digit, so punctuation that a script change leaves at its edge, and a mark after punctuation, are not part of it.
- A grapheme cluster that starts with whitespace, or with a character that Unicode grapheme rules join to the character before it, separates words.
- A run of Hangul, Han, Hiragana, Katakana, or a script that Unicode line breaking leaves to a dictionary, such as Thai, Lao, Khmer, or Myanmar, is indexed as its overlapping pairs of units and its single units.
- A unit of such a run is a grapheme cluster, except in Hangul.
- Such a run starts at a grapheme cluster whose first character is a letter.
- Digits of such a script are words, not part of a run.
- A unit of a Hangul run is a syllable or a jamo left over after normalization, with the marks and joiners of its grapheme cluster that follow it.
- A query run looks for its pairs of units, or for its one unit when it has one.
- No text is split with a word dictionary.
- Text between two `"` characters in a keyword query is a phrase.
- An unclosed `"` starts a phrase that runs to the end of the query.
- A phrase matches an element that holds its terms at the same distances from one another as in the query, inside one string value.
- A word directly followed by `*` outside a phrase is a prefix.
- A prefix matches every term that starts with it.
- A `*` after a run of Hangul, Han, kana, or another paired script changes nothing.
- A keyword query matches an element that holds any of its terms, prefixes, or phrases.
- Keyword queries offer no fuzzy matching of misspelled terms.
- Keyword matches are ranked by BM25.
- A phrase is ranked as one term whose occurrences are the phrase's.
- A prefix is ranked in each element by the best of the terms it matches there.
- Each keyword index keeps its element count and total term count.
- Each posting keeps its element's term count.
- Each posting keeps the positions of its term in the element.
- Each indexed element keeps the terms it was indexed under.
- Removing an element from a keyword index deletes the terms it kept, without tokenizing again.
- The database records which tokenizer built its keyword indexes, as the tokenizer's version and its terms and their positions for a fixed probe text that exercises the Unicode tables it depends on.
- Opening a database recorded with another tokenizer rebuilds every keyword index in the opening write transaction.
- That rebuild records the current tokenizer.
- Changing what the tokenizer produces on purpose raises its version.
- The word segmentation and normalization crates are each pinned to one exact version.
- A vector index covers one property and has a fixed dimension and one metric: cosine, euclidean, or dot product.
- An element is in a vector index when the property is a list of finite numbers of the index's dimension whose length is small enough that no score can overflow, and under cosine large enough that no product of lengths can underflow.
- Any other value leaves the element out of the vector index without an error.
- A query vector that breaks the same rules fails the query.
- Vectors are stored in a table of their own, so a search reads no node or relationship records.
- Vector search is exact: each query reads and scores every vector in its index.
- RhizoDB offers no approximate vector index, such as HNSW.
- A host with more vectors than one query can scan keeps an approximate index of its own outside the database and reads the elements it returns by ID.
- The score is higher for closer vectors: the cosine similarity, the dot product, or the negative euclidean distance.
- The integrity check derives every index entry from the nodes and relationships and compares it with the stored entries byte for byte.

## Open

None
