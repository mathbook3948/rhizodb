mod support;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const POOL: &[char] = &[
    'a',
    'b',
    'Z',
    'k',
    '0',
    '7',
    '9',
    'e',
    '\u{301}',
    '\u{e9}',
    '\u{c5}',
    '\u{30a}',
    '\u{130}',
    '\u{3a3}',
    '\u{ac00}',
    '\u{d55c}',
    '\u{ad6d}',
    '\u{3131}',
    '\u{314b}',
    '\u{314f}',
    '\u{1100}',
    '\u{1161}',
    '\u{11a8}',
    '\u{ffa1}',
    '\u{3042}',
    '\u{304b}',
    '\u{30ab}',
    '\u{ff76}',
    '\u{6771}',
    '\u{4eac}',
    '\u{2a6df}',
    '\u{e01}',
    '\u{e32}',
    '\u{e31}',
    '\u{e48}',
    '\u{e52}',
    '\u{e81}',
    '\u{1780}',
    '\u{1000}',
    '\u{103a}',
    '\u{915}',
    '\u{94d}',
    '\u{903}',
    '\u{200d}',
    '\u{200c}',
    '\u{fe0f}',
    '\u{fe00}',
    '\u{0}',
    '\u{7}',
    '\u{1f}',
    '\u{7f}',
    '\u{85}',
    '\u{feff}',
    '\u{200b}',
    ' ',
    ' ',
    '\t',
    '\n',
    '\r',
    '\u{a0}',
    '\u{3000}',
    '\u{2028}',
    '.',
    ',',
    '\'',
    '"',
    '*',
    '-',
    '_',
    '\u{1f469}',
    '\u{1f4bb}',
    '\u{fb01}',
    '\u{ff21}',
    '\u{b2}',
];

const ANCHOR: &str = "anchorword";

fn mixed_text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(POOL), 0..14)
        .prop_map(|characters| characters.into_iter().collect())
}

fn hits(db: &Database, query: &str) -> Vec<Value> {
    let mut parameters = Parameters::new();
    parameters.insert("index".to_owned(), string("notes"));
    parameters.insert("text".to_owned(), string(query));
    parameters.insert("limit".to_owned(), int(10));
    db.execute(
        "CALL db.index.keyword.queryNodes($index, $text, $limit) YIELD node \
         RETURN node.name AS name",
        &parameters,
    )
    .unwrap_or_else(|error| panic!("{query:?}: {error}"))
    .rows()
    .iter()
    .map(|row| row[0].clone())
    .collect()
}

fn indexed(body: &str) -> Database {
    let db = db();
    db.create_keyword_index("notes", IndexTarget::Label("Note"), &["body"])
        .unwrap();
    let mut parameters = Parameters::new();
    parameters.insert("body".to_owned(), string(body));
    db.execute("INSERT (:Note {name: 'target', body: $body})", &parameters)
        .unwrap();
    db
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_text_is_found_by_the_phrase_of_its_own_words(text in mixed_text()) {
        let db = indexed(&format!("{ANCHOR} {text}"));
        let typed = text.replace('"', " ");
        let phrase = format!("\"{ANCHOR} {typed}\"");
        prop_assert_eq!(hits(&db, &phrase), [string("target")], "{:?}", phrase);
    }
}
