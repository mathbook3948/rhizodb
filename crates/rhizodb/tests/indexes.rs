mod support;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Procedure;
use rhizodb::ProcedureOutput;
use rhizodb::Value;
use rhizodb::VectorMetric;
use support::*;

const KEYWORD_QUERY: &str = "CALL db.index.keyword.queryNodes($index, $text, $limit) YIELD node, score \
                             RETURN node.name AS name, score";
const VECTOR_QUERY: &str = "CALL db.index.vector.queryNodes($index, $vector, $limit) YIELD node, score \
                            RETURN node.name AS name, score";

fn search_parameters(index: &str, query: Value, limit: i64) -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert("index".to_owned(), string(index));
    parameters.insert("text".to_owned(), query.clone());
    parameters.insert("vector".to_owned(), query);
    parameters.insert("limit".to_owned(), int(limit));
    parameters
}

fn scored(rows: &[Vec<Value>]) -> Vec<(String, f64)> {
    rows.iter()
        .map(|row| match row.as_slice() {
            [Value::String(name), Value::Float(score)] => (name.clone(), *score),
            other => panic!("expected a name and a score, got {other:?}"),
        })
        .collect()
}

fn keyword_hits(db: &Database, index: &str, text: &str, limit: i64) -> Vec<(String, f64)> {
    let result = db
        .execute(
            KEYWORD_QUERY,
            &search_parameters(index, string(text), limit),
        )
        .unwrap();
    scored(result.rows())
}

fn keyword(db: &Database, index: &str, text: &str) -> Vec<String> {
    keyword_hits(db, index, text, 100)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

fn vector(db: &Database, index: &str, query: &[f64], limit: i64) -> Vec<(String, f64)> {
    let query = list(query.iter().map(|item| float(*item)));
    let result = db
        .execute(VECTOR_QUERY, &search_parameters(index, query, limit))
        .unwrap();
    scored(result.rows())
}

fn search_error(db: &Database, query: &str, index: &str, argument: Value, limit: i64) -> ErrorKind {
    match db.execute(query, &search_parameters(index, argument, limit)) {
        Ok(result) => panic!("search succeeded with {:?}", result.rows()),
        Err(error) => error.kind(),
    }
}

fn notes_db() -> Database {
    let db = db();
    db.create_keyword_index("notes", IndexTarget::Label("Note"), &["name", "body"])
        .unwrap();
    db
}

#[test]
fn creating_an_index_fills_it_from_the_nodes_already_there() {
    let db = db();
    run(
        &db,
        "INSERT (:Note {name: 'a', body: 'graph databases store relationships'}), \
                (:Note {name: 'b', body: 'tables store rows'}), \
                (:Other {name: 'c', body: 'graph'})",
    );
    db.create_keyword_index("notes", IndexTarget::Label("Note"), &["body"])
        .unwrap();
    assert_eq!(keyword(&db, "notes", "graph"), ["a"]);
    let mut stored = keyword(&db, "notes", "STORE");
    stored.sort();
    assert_eq!(stored, ["a", "b"]);
    assert!(keyword(&db, "notes", "missing").is_empty());
}

#[test]
fn inserts_property_writes_and_deletes_keep_the_index_current() {
    let db = notes_db();
    run(&db, "INSERT (:Note {name: 'n', body: 'first draft'})");
    assert_eq!(keyword(&db, "notes", "draft"), ["n"]);

    run(&db, "MATCH (n:Note) SET n.body = 'final version'");
    assert!(keyword(&db, "notes", "draft").is_empty());
    assert_eq!(keyword(&db, "notes", "final"), ["n"]);

    run(&db, "MATCH (n:Note) REMOVE n.body");
    assert!(keyword(&db, "notes", "final").is_empty());
    assert_eq!(keyword(&db, "notes", "n"), ["n"]);

    run(
        &db,
        "MATCH (n:Note) SET n = {name: 'm', body: 'replaced map'}",
    );
    assert!(keyword(&db, "notes", "n").is_empty());
    assert_eq!(keyword(&db, "notes", "replaced"), ["m"]);

    run(&db, "MATCH (n:Note) DETACH DELETE n");
    assert!(keyword(&db, "notes", "replaced").is_empty());
}

#[test]
fn adding_and_removing_the_label_adds_and_removes_the_node() {
    let db = notes_db();
    run(&db, "INSERT (:Draft {name: 'x', body: 'pending label'})");
    assert!(keyword(&db, "notes", "pending").is_empty());
    run(&db, "MATCH (n:Draft) SET n:Note");
    assert_eq!(keyword(&db, "notes", "pending"), ["x"]);
    run(&db, "MATCH (n:Draft) REMOVE n:Note");
    assert!(keyword(&db, "notes", "pending").is_empty());
}

#[test]
fn a_list_of_strings_is_indexed_and_other_values_are_not() {
    let db = db();
    db.create_keyword_index("tags", IndexTarget::Label("Task"), &["tags"])
        .unwrap();
    run(
        &db,
        "INSERT (:Task {name: 'list', tags: ['home', 'urgent']}), \
                (:Task {name: 'number', tags: 42}), \
                (:Task {name: 'mixed', tags: ['home', 1]})",
    );
    assert_eq!(keyword(&db, "tags", "urgent"), ["list"]);
    assert_eq!(keyword(&db, "tags", "home").len(), 2);
    assert!(keyword(&db, "tags", "42").is_empty());
}

#[test]
fn a_rolled_back_transaction_leaves_the_index_unchanged() {
    let db = notes_db();
    run(&db, "INSERT (:Note {name: 'kept', body: 'stable text'})");

    let mut tx = db.begin_write().unwrap();
    tx.execute(
        "INSERT (:Note {name: 'new', body: 'stable addition'})",
        &Parameters::new(),
    )
    .unwrap();
    tx.execute("MATCH (n:Note {name: 'kept'}) DELETE n", &Parameters::new())
        .unwrap();
    let inside = tx
        .execute(
            KEYWORD_QUERY,
            &search_parameters("notes", string("stable"), 10),
        )
        .unwrap();
    assert_eq!(
        scored(inside.rows()).len(),
        1,
        "a transaction sees its own writes"
    );
    tx.rollback().unwrap();
    assert_eq!(keyword(&db, "notes", "stable"), ["kept"]);

    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:Note) SET n.body = 'changed' WITH n RETURN 1 / 0"
        ),
        ErrorKind::DivisionByZero
    );
    assert_eq!(keyword(&db, "notes", "stable"), ["kept"]);
    assert!(keyword(&db, "notes", "changed").is_empty());
}

#[test]
fn ranking_prefers_more_occurrences_rarer_terms_and_shorter_text() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'twice', body: 'graph graph store'}), \
                (:Note {name: 'once', body: 'graph store tables'}), \
                (:Note {name: 'short', body: 'store'}), \
                (:Note {name: 'long', body: 'store of many other unrelated words here'})",
    );
    assert_eq!(keyword(&db, "notes", "graph"), ["twice", "once"]);
    let hits = keyword_hits(&db, "notes", "graph store", 100);
    assert_eq!(hits[0].0, "twice");
    assert!(hits.windows(2).all(|pair| pair[0].1 >= pair[1].1));
    let store = keyword(&db, "notes", "store");
    let position = |name: &str| store.iter().position(|hit| hit == name).unwrap();
    assert!(position("short") < position("long"));
    assert_eq!(keyword_hits(&db, "notes", "graph store", 2).len(), 2);
}

#[test]
fn equal_scores_come_in_node_order() {
    let db = notes_db();
    run(
        &db,
        "UNWIND range(1, 5) AS i INSERT (:Note {name: toString(i), body: 'same words'})",
    );
    assert_eq!(keyword(&db, "notes", "same"), ["1", "2", "3", "4", "5"]);
}

#[test]
fn loose_hangul_jamo_are_found_one_by_one() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'laugh', body: 'ㅋㅋㅋㅋ'}), \
                (:Note {name: 'cry', body: 'ㅠㅠ'}), \
                (:Note {name: 'initials', body: 'ㄱㅅ'}), \
                (:Note {name: 'mixed', body: 'ㄱ가나'}), \
                (:Note {name: 'halfwidth', body: 'ﾡﾡ'})",
    );
    assert_eq!(keyword(&db, "notes", "ㅋ"), ["laugh"]);
    assert_eq!(keyword(&db, "notes", "ㅋㅋ"), ["laugh"]);
    assert_eq!(keyword(&db, "notes", "ㅠ"), ["cry"]);
    assert_eq!(keyword(&db, "notes", "가나"), ["mixed"]);
    assert_eq!(keyword(&db, "notes", "ㄱㄱ"), ["halfwidth"]);
    let mut initial = keyword(&db, "notes", "ㄱ");
    initial.sort();
    assert_eq!(initial, ["halfwidth", "initials", "mixed"]);
}

#[test]
fn korean_text_is_found_by_a_word_its_stem_or_one_syllable() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'ko', body: '내일 서울에서 만나요'}), \
                (:Note {name: 'home', body: '우리집 정원'}), \
                (:Note {name: 'english', body: 'meet in Seoul tomorrow'})",
    );
    assert_eq!(keyword(&db, "notes", "서울"), ["ko"]);
    assert_eq!(keyword(&db, "notes", "서울에서"), ["ko"]);
    assert_eq!(keyword(&db, "notes", "만나다")[0], "ko");
    assert_eq!(keyword(&db, "notes", "집"), ["home"]);
    assert_eq!(keyword(&db, "notes", "seoul"), ["english"]);
}

#[test]
fn japanese_and_chinese_text_is_found_without_spaces() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'ja', body: '東京都に行きます'}), (:Note {name: 'zh', body: '北京欢迎你'})",
    );
    assert_eq!(keyword(&db, "notes", "東京"), ["ja"]);
    assert_eq!(keyword(&db, "notes", "欢迎"), ["zh"]);
}

#[test]
fn thai_text_is_found_by_a_word_inside_it_and_not_by_shared_letters() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'th', body: 'ภาษาไทยง่ายมาก'}), (:Note {name: 'come', body: 'มาเร็ว'})",
    );
    assert_eq!(keyword(&db, "notes", "ภาษา"), ["th"]);
    assert_eq!(keyword(&db, "notes", "ง่าย"), ["th"]);
    assert_eq!(keyword(&db, "notes", "มา")[0], "come");
}

#[test]
fn composed_and_decomposed_text_find_each_other() {
    let db = notes_db();
    let mut parameters = Parameters::new();
    parameters.insert("decomposed".to_owned(), string("Cafe\u{301} on the corner"));
    parameters.insert("composed".to_owned(), string("Crème brûlée"));
    db.execute(
        "INSERT (:Note {name: 'decomposed', body: $decomposed}), \
                (:Note {name: 'composed', body: $composed})",
        &parameters,
    )
    .unwrap();
    assert_eq!(keyword(&db, "notes", "café"), ["decomposed"]);
    assert_eq!(keyword(&db, "notes", "bru\u{302}le\u{301}e"), ["composed"]);
    assert_eq!(keyword(&db, "notes", "ＣＡＦÉ"), ["decomposed"]);
}

#[test]
fn a_quoted_phrase_matches_its_words_in_order_and_next_to_each_other() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'exact', body: 'read the New York times'}), \
                (:Note {name: 'apart', body: 'new books from york'}), \
                (:Note {name: 'reversed', body: 'york new'}), \
                (:Note {name: 'twice', body: 'new york or new york'})",
    );
    assert_eq!(keyword(&db, "notes", "\"new york\""), ["twice", "exact"]);
    let mut either = keyword(&db, "notes", "\"new york\" books");
    either.sort();
    assert_eq!(either, ["apart", "exact", "twice"]);
    assert_eq!(keyword(&db, "notes", "\"york new\""), ["reversed"]);
    assert_eq!(keyword(&db, "notes", "\"the new york"), ["exact"]);
    assert!(keyword(&db, "notes", "\"new times\"").is_empty());
}

#[test]
fn a_trailing_star_matches_every_term_that_starts_with_the_word() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'plural', body: 'graphs everywhere'}), \
                (:Note {name: 'both', body: 'graphs and graphql'}), \
                (:Note {name: 'exact', body: 'a graph'}), \
                (:Note {name: 'inside', body: 'paragraph'})",
    );
    let mut found = keyword(&db, "notes", "Graph*");
    found.sort();
    assert_eq!(found, ["both", "exact", "plural"]);
    assert_eq!(keyword(&db, "notes", "graph *"), ["exact"]);
    let score = |query: &str| {
        keyword_hits(&db, "notes", query, 100)
            .into_iter()
            .find(|(name, _)| name == "both")
            .map(|(_, score)| score)
            .unwrap()
    };
    assert_eq!(score("graph*"), score("graphs").max(score("graphql")));
}

#[test]
fn a_phrase_never_matches_across_two_values() {
    let db = notes_db();
    db.create_keyword_index("tags", IndexTarget::Label("Note"), &["tags"])
        .unwrap();
    run(
        &db,
        "INSERT (:Note {name: 'graph', body: 'store'}), \
                (:Note {name: 'split', tags: ['graph', 'store']}), \
                (:Note {name: 'joined', tags: ['graph store']})",
    );
    assert!(keyword(&db, "notes", "\"graph store\"").is_empty());
    assert_eq!(keyword(&db, "tags", "\"graph store\""), ["joined"]);
}

#[test]
fn a_phrase_of_korean_or_japanese_matches_the_run_in_order() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'ko', body: '내일 서울에서 만나요'}), \
                (:Note {name: 'swapped', body: '만나요 서울에서'}), \
                (:Note {name: 'ja', body: '東京都に行く'})",
    );
    assert_eq!(keyword(&db, "notes", "\"서울에서 만나요\""), ["ko"]);
    assert_eq!(keyword(&db, "notes", "서울에서 만나요").len(), 2);
    assert_eq!(keyword(&db, "notes", "\"東京都\""), ["ja"]);
    assert!(keyword(&db, "notes", "\"京東\"").is_empty());
}

#[test]
fn a_search_can_feed_the_rest_of_a_query() {
    let db = notes_db();
    run(
        &db,
        "INSERT (:Note {name: 'a', body: 'trip plan'})-[:FOR]->(:Place {name: 'Jeju'})",
    );
    assert_eq!(
        run(
            &db,
            "CALL db.index.keyword.queryNodes('notes', 'trip', 5) YIELD node \
             MATCH (node)-[:FOR]->(place) RETURN place.name"
        ),
        [vec![string("Jeju")]]
    );
    assert_eq!(
        run(
            &db,
            "CALL db.index.keyword.queryNodes('notes', 'trip', 5) YIELD node, score WHERE score > 1000.0 \
             RETURN node"
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn indexes_survive_reopening_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    {
        let db = Database::open(&path).unwrap();
        db.create_keyword_index("notes", IndexTarget::Label("Note"), &["body"])
            .unwrap();
        db.create_vector_index(
            "points",
            IndexTarget::Label("Note"),
            "at",
            2,
            VectorMetric::Euclidean,
        )
        .unwrap();
        run(
            &db,
            "INSERT (:Note {name: 'p', body: 'durable', at: [1.0, 2.0]})",
        );
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(keyword(&db, "notes", "durable"), ["p"]);
    assert_eq!(
        vector(&db, "points", &[1.0, 2.0], 1),
        [("p".to_owned(), 0.0)]
    );
}

#[test]
fn creating_and_dropping_indexes_is_idempotent_and_names_are_unique() {
    let db = db();
    db.create_keyword_index("notes", IndexTarget::Label("Note"), &["body", "name"])
        .unwrap();
    db.create_keyword_index(
        "notes",
        IndexTarget::Label("Note"),
        &["name", "body", "name"],
    )
    .unwrap();
    let taken = |result: Result<(), rhizodb::Error>| result.unwrap_err().kind();
    assert_eq!(
        taken(db.create_keyword_index("notes", IndexTarget::Label("Note"), &["body"])),
        ErrorKind::IndexAlreadyExists
    );
    assert_eq!(
        taken(db.create_vector_index(
            "notes",
            IndexTarget::Label("Note"),
            "at",
            2,
            VectorMetric::Cosine
        )),
        ErrorKind::IndexAlreadyExists
    );
    assert!(db.drop_index("notes").unwrap());
    assert!(!db.drop_index("notes").unwrap());
    assert_eq!(
        search_error(&db, KEYWORD_QUERY, "notes", string("x"), 1),
        ErrorKind::IndexNotFound
    );
    db.create_vector_index(
        "notes",
        IndexTarget::Label("Note"),
        "at",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    assert_eq!(
        search_error(&db, KEYWORD_QUERY, "notes", string("x"), 1),
        ErrorKind::IndexNotFound
    );
}

#[test]
fn a_dropped_index_forgets_its_entries() {
    let db = notes_db();
    run(&db, "INSERT (:Note {name: 'old', body: 'remembered'})");
    db.drop_index("notes").unwrap();
    db.create_keyword_index("notes", IndexTarget::Label("Note"), &["name"])
        .unwrap();
    assert!(keyword(&db, "notes", "remembered").is_empty());
    assert_eq!(keyword(&db, "notes", "old"), ["old"]);
}

#[test]
fn invalid_definitions_are_rejected() {
    let db = db();
    let kind = |result: Result<(), rhizodb::Error>| result.unwrap_err().kind();
    assert_eq!(
        kind(db.create_keyword_index("", IndexTarget::Label("Note"), &["body"])),
        ErrorKind::InvalidIndexDefinition
    );
    assert_eq!(
        kind(db.create_keyword_index("notes", IndexTarget::Label("Note"), &[])),
        ErrorKind::InvalidIndexDefinition
    );
    assert_eq!(
        kind(db.create_vector_index(
            "points",
            IndexTarget::Label("Note"),
            "at",
            0,
            VectorMetric::Cosine
        )),
        ErrorKind::InvalidIndexDefinition
    );
}

#[test]
fn search_arguments_are_checked() {
    let db = notes_db();
    db.create_vector_index(
        "points",
        IndexTarget::Label("Note"),
        "at",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    assert_eq!(
        search_error(&db, KEYWORD_QUERY, "missing", string("x"), 1),
        ErrorKind::IndexNotFound
    );
    assert_eq!(
        search_error(&db, KEYWORD_QUERY, "notes", string("x"), -1),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        search_error(&db, VECTOR_QUERY, "points", list([float(1.0)]), 1),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        search_error(&db, VECTOR_QUERY, "points", list([float(0.0), int(0)]), 1),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        search_error(&db, VECTOR_QUERY, "points", list([string("a"), int(0)]), 1),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        search_error(&db, VECTOR_QUERY, "points", string("a"), 1),
        ErrorKind::InvalidArgumentValue
    );
    assert_eq!(
        search_error(&db, VECTOR_QUERY, "notes", list([float(1.0)]), 1),
        ErrorKind::IndexNotFound
    );
    assert_eq!(
        error_kind(&db, "CALL db.index.keyword.queryNodes('notes', 'x', 1.5)"),
        ErrorKind::InvalidArgumentType
    );
    assert!(run(&db, "CALL db.index.keyword.queryNodes('notes', null, 1)").is_empty());
    assert!(run(&db, "CALL db.index.keyword.queryNodes(null, 'x', -1)").is_empty());
    assert!(keyword_hits(&db, "notes", "x", 0).is_empty());
}

#[test]
fn a_standalone_search_returns_every_output() {
    let db = notes_db();
    run(&db, "INSERT (:Note {name: 'n', body: 'word'})");
    assert_eq!(
        columns(&db, "CALL db.index.keyword.queryNodes('notes', 'word', 1)"),
        ["node", "score"]
    );
}

#[test]
fn the_db_namespace_is_reserved_for_built_in_procedures() {
    let db = db();
    let error = db
        .register_procedure(Procedure::new("db.index.keyword.queryNodes", |_| {
            Ok(ProcedureOutput::new())
        }))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ReservedProcedureName);
    assert_eq!(
        db.register_procedure(Procedure::new("db", |_| Ok(ProcedureOutput::new())))
            .unwrap_err()
            .kind(),
        ErrorKind::ReservedProcedureName
    );
    db.register_procedure(Procedure::new("dbx.tool", |_| Ok(ProcedureOutput::new())))
        .unwrap();
    assert_eq!(
        error_kind(&db, "CALL db.index.unknown()"),
        ErrorKind::ProcedureNotFound
    );
}

#[test]
fn vector_entries_follow_writes_and_skip_values_that_do_not_fit() {
    let db = db();
    db.create_vector_index(
        "points",
        IndexTarget::Label("P"),
        "at",
        2,
        VectorMetric::Euclidean,
    )
    .unwrap();
    run(
        &db,
        "INSERT (:P {name: 'ints', at: [1, 1]}), \
                (:P {name: 'short', at: [1.0]}), \
                (:P {name: 'text', at: ['a', 'b']}), \
                (:P {name: 'scalar', at: 3.0}), \
                (:Q {name: 'unlabelled', at: [1.0, 1.0]})",
    );
    let names =
        |hits: Vec<(String, f64)>| hits.into_iter().map(|(name, _)| name).collect::<Vec<_>>();
    assert_eq!(names(vector(&db, "points", &[0.0, 0.0], 10)), ["ints"]);

    run(&db, "MATCH (n:P {name: 'short'}) SET n.at = [5.0, 5.0]");
    assert_eq!(
        vector(&db, "points", &[5.0, 5.0], 10),
        [
            ("short".to_owned(), 0.0),
            ("ints".to_owned(), 0.0 - 32.0f64.sqrt())
        ]
    );
    run(&db, "MATCH (n:Q) SET n:P");
    run(&db, "MATCH (n:P {name: 'ints'}) REMOVE n.at");
    run(&db, "MATCH (n:P {name: 'short'}) REMOVE n:P");
    assert_eq!(
        names(vector(&db, "points", &[0.0, 0.0], 10)),
        ["unlabelled"]
    );
    run(&db, "MATCH (n:P {name: 'unlabelled'}) DELETE n");
    assert!(vector(&db, "points", &[0.0, 0.0], 10).is_empty());
}

#[test]
fn a_cosine_index_skips_zero_vectors() {
    let db = db();
    db.create_vector_index(
        "directions",
        IndexTarget::Label("P"),
        "at",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    run(
        &db,
        "INSERT (:P {name: 'zero', at: [0.0, 0.0]}), (:P {name: 'east', at: [2.0, 0.0]}), \
                (:P {name: 'north', at: [0.0, 3.0]})",
    );
    assert_eq!(
        vector(&db, "directions", &[1.0, 0.0], 10),
        [("east".to_owned(), 1.0), ("north".to_owned(), 0.0)]
    );
}

#[test]
fn vectors_too_long_or_too_short_to_score_are_left_out_and_cannot_be_searched_for() {
    let db = db();
    for (name, metric) in [
        ("cosine", VectorMetric::Cosine),
        ("euclidean", VectorMetric::Euclidean),
        ("dot", VectorMetric::DotProduct),
    ] {
        db.create_vector_index(name, IndexTarget::Label("P"), "at", 2, metric)
            .unwrap();
    }
    run(
        &db,
        "INSERT (:P {name: 'huge', at: [1.0e200, 1.0e200]}), \
                (:P {name: 'far', at: [1.3e154, 0.0]}), \
                (:P {name: 'tiny', at: [1.0e-200, 0.0]}), \
                (:P {name: 'plain', at: [1.0, 1.0]})",
    );
    for index in ["cosine", "euclidean", "dot"] {
        let hits = vector(&db, index, &[1.0, 1.0], 10);
        assert!(
            hits.iter().all(|(_, score)| score.is_finite()),
            "{index}: {hits:?}"
        );
        assert!(
            !hits.iter().any(|(name, _)| name == "huge"),
            "{index}: {hits:?}"
        );
        assert_eq!(
            search_error(
                &db,
                VECTOR_QUERY,
                index,
                list([float(1.0e200), float(0.0)]),
                1
            ),
            ErrorKind::InvalidArgumentValue,
            "{index}"
        );
    }
    let cosine = vector(&db, "cosine", &[1.0, 1.0], 10);
    assert!(!cosine.iter().any(|(name, _)| name == "tiny"), "{cosine:?}");
    assert_eq!(
        search_error(
            &db,
            VECTOR_QUERY,
            "cosine",
            list([float(1.0e-200), float(0.0)]),
            1
        ),
        ErrorKind::InvalidArgumentValue
    );
    let far = vector(&db, "euclidean", &[-6.0e153, 0.0], 10);
    assert!(far.iter().all(|(_, score)| score.is_finite()), "{far:?}");
}

const KEYWORD_RELATIONSHIPS: &str = "CALL db.index.keyword.queryRelationships($index, $text, $limit) YIELD relationship, score \
     RETURN relationship.name AS name, score";
const VECTOR_RELATIONSHIPS: &str = "CALL db.index.vector.queryRelationships($index, $vector, $limit) YIELD relationship, score \
     RETURN relationship.name AS name, score";

fn relationship_keyword(db: &Database, index: &str, text: &str) -> Vec<String> {
    let result = db
        .execute(
            KEYWORD_RELATIONSHIPS,
            &search_parameters(index, string(text), 100),
        )
        .unwrap();
    let mut names: Vec<String> = scored(result.rows())
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort();
    names
}

fn relationship_vector(db: &Database, index: &str, query: &[f64]) -> Vec<(String, f64)> {
    let query = list(query.iter().map(|item| float(*item)));
    let result = db
        .execute(VECTOR_RELATIONSHIPS, &search_parameters(index, query, 100))
        .unwrap();
    scored(result.rows())
}

fn trips_db() -> Database {
    let db = db();
    run(
        &db,
        "INSERT (ada:Person {name: 'Ada'}), (bob:Person {name: 'Bob'}), \
                (ada)-[:VISITED {name: 'lisbon', notes: 'tram rides and pastries'}]->(bob), \
                (bob)-[:VISITED {name: 'kyoto', notes: 'temples and trains'}]->(ada), \
                (ada)-[:PLANNED {name: 'oslo', notes: 'trains in winter'}]->(bob)",
    );
    db.create_keyword_index(
        "trips",
        IndexTarget::RelationshipType("VISITED"),
        &["notes"],
    )
    .unwrap();
    db
}

#[test]
fn a_relationship_index_covers_only_relationships_of_its_type() {
    let db = trips_db();
    assert_eq!(relationship_keyword(&db, "trips", "trains"), ["kyoto"]);
    assert_eq!(
        relationship_keyword(&db, "trips", "trains tram"),
        ["kyoto", "lisbon"]
    );
    run(&db, "INSERT (:VISITED {name: 'node', notes: 'trains'})");
    assert_eq!(relationship_keyword(&db, "trips", "trains"), ["kyoto"]);
}

#[test]
fn a_relationship_search_yields_the_relationship_and_its_score() {
    let db = trips_db();
    let rows = run(
        &db,
        "CALL db.index.keyword.queryRelationships('trips', 'pastries', 5) \
         YIELD relationship, score \
         MATCH (from)-[relationship]->(to) \
         RETURN from.name, type(relationship), to.name, score > 0.0",
    );
    assert_eq!(
        rows,
        [vec![
            string("Ada"),
            string("VISITED"),
            string("Bob"),
            boolean(true)
        ]]
    );
    assert_eq!(
        columns(
            &db,
            "CALL db.index.keyword.queryRelationships('trips', 'pastries', 5)"
        ),
        ["relationship", "score"]
    );
    assert!(matches!(
        single(
            &db,
            "CALL db.index.keyword.queryRelationships('trips', 'pastries', 5) \
             YIELD relationship RETURN relationship"
        ),
        Value::Relationship(_)
    ));
}

#[test]
fn every_write_to_a_relationship_keeps_its_index_current() {
    let db = trips_db();
    run(
        &db,
        "MATCH ()-[r:VISITED {name: 'lisbon'}]->() SET r.notes = 'fado at night'",
    );
    assert!(relationship_keyword(&db, "trips", "pastries").is_empty());
    assert_eq!(relationship_keyword(&db, "trips", "fado"), ["lisbon"]);

    run(
        &db,
        "MATCH ()-[r:VISITED {name: 'lisbon'}]->() REMOVE r.notes",
    );
    assert!(relationship_keyword(&db, "trips", "fado").is_empty());

    run(
        &db,
        "MATCH ()-[r:VISITED {name: 'lisbon'}]->() SET r = {name: 'porto', notes: 'river wine'}",
    );
    assert_eq!(relationship_keyword(&db, "trips", "wine"), ["porto"]);

    run(
        &db,
        "MATCH (a {name: 'Ada'}), (b {name: 'Bob'}) \
         MERGE (a)-[r:VISITED {name: 'rome'}]->(b) ON CREATE SET r.notes = 'forum ruins'",
    );
    assert_eq!(relationship_keyword(&db, "trips", "ruins"), ["rome"]);
    run(
        &db,
        "MATCH (a {name: 'Ada'}), (b {name: 'Bob'}) \
         MERGE (a)-[r:VISITED {name: 'rome'}]->(b) ON MATCH SET r.notes = 'colosseum'",
    );
    assert_eq!(relationship_keyword(&db, "trips", "colosseum"), ["rome"]);
    assert!(relationship_keyword(&db, "trips", "ruins").is_empty());

    run(
        &db,
        "MATCH (b {name: 'Bob'}) CREATE (b)-[:VISITED {name: 'nara', notes: 'deer park'}]->(:Place)",
    );
    assert_eq!(relationship_keyword(&db, "trips", "deer"), ["nara"]);

    run(&db, "MATCH ()-[r:VISITED {name: 'rome'}]->() DELETE r");
    assert!(relationship_keyword(&db, "trips", "colosseum").is_empty());

    run(&db, "MATCH (b {name: 'Bob'}) DETACH DELETE b");
    assert!(relationship_keyword(&db, "trips", "wine deer temples").is_empty());
}

#[test]
fn a_rolled_back_transaction_leaves_a_relationship_index_unchanged() {
    let db = trips_db();
    let mut tx = db.begin_write().unwrap();
    tx.execute(
        "MATCH (a {name: 'Ada'}) CREATE (a)-[:VISITED {name: 'paris', notes: 'trains'}]->(a)",
        &Parameters::new(),
    )
    .unwrap();
    tx.execute(
        "MATCH ()-[r:VISITED {name: 'kyoto'}]->() DELETE r",
        &Parameters::new(),
    )
    .unwrap();
    let inside = tx
        .execute(
            KEYWORD_RELATIONSHIPS,
            &search_parameters("trips", string("trains"), 10),
        )
        .unwrap();
    assert_eq!(
        scored(inside.rows()),
        [("paris".to_owned(), scored(inside.rows())[0].1)],
        "a transaction sees its own writes"
    );
    tx.rollback().unwrap();
    assert_eq!(relationship_keyword(&db, "trips", "trains"), ["kyoto"]);

    assert_eq!(
        error_kind(&db, "MATCH (a {name: 'Ada'}) DELETE a"),
        ErrorKind::DeleteConnectedNode
    );
    assert_eq!(relationship_keyword(&db, "trips", "tram"), ["lisbon"]);
}

#[test]
fn a_relationship_vector_index_scores_every_relationship_vector() {
    let db = db();
    db.create_vector_index(
        "legs",
        IndexTarget::RelationshipType("LEG"),
        "at",
        2,
        VectorMetric::Euclidean,
    )
    .unwrap();
    run(
        &db,
        "INSERT (a:Stop)-[:LEG {name: 'near', at: [1.0, 1.0]}]->(b:Stop), \
                (b)-[:LEG {name: 'far', at: [4.0, 5.0]}]->(a), \
                (b)-[:LEG {name: 'flat', at: [1.0]}]->(a), \
                (a)-[:HOP {name: 'other', at: [1.0, 1.0]}]->(b)",
    );
    assert_eq!(
        relationship_vector(&db, "legs", &[1.0, 1.0]),
        [("near".to_owned(), 0.0), ("far".to_owned(), -5.0)]
    );
    run(
        &db,
        "MATCH ()-[r:LEG {name: 'far'}]->() SET r.at = [1.0, 2.0]",
    );
    assert_eq!(
        relationship_vector(&db, "legs", &[1.0, 2.0]),
        [("far".to_owned(), 0.0), ("near".to_owned(), -1.0)]
    );
}

#[test]
fn each_procedure_searches_only_indexes_of_its_kind_and_element() {
    let db = trips_db();
    db.create_vector_index(
        "trip points",
        IndexTarget::RelationshipType("VISITED"),
        "at",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    db.create_keyword_index("people", IndexTarget::Label("Person"), &["name"])
        .unwrap();
    db.create_vector_index(
        "person points",
        IndexTarget::Label("Person"),
        "at",
        2,
        VectorMetric::Cosine,
    )
    .unwrap();
    let text = || string("ada");
    let point = || list([float(1.0), float(0.0)]);
    for (query, index, argument) in [
        (KEYWORD_QUERY, "trips", text()),
        (KEYWORD_RELATIONSHIPS, "people", text()),
        (KEYWORD_RELATIONSHIPS, "trip points", text()),
        (VECTOR_QUERY, "trip points", point()),
        (VECTOR_RELATIONSHIPS, "person points", point()),
        (VECTOR_RELATIONSHIPS, "trips", point()),
    ] {
        assert_eq!(
            search_error(&db, query, index, argument, 1),
            ErrorKind::IndexNotFound,
            "{query} over {index}"
        );
    }
    assert_eq!(keyword(&db, "people", "ada"), ["Ada"]);
}

#[test]
fn a_relationship_index_and_a_node_index_may_share_a_type_name_and_label_name() {
    let db = db();
    db.create_keyword_index("nodes", IndexTarget::Label("Same"), &["name"])
        .unwrap();
    db.create_keyword_index(
        "relationships",
        IndexTarget::RelationshipType("Same"),
        &["name"],
    )
    .unwrap();
    run(
        &db,
        "INSERT (:Same {name: 'shared'})-[:Same {name: 'shared'}]->(:Same {name: 'other'})",
    );
    assert_eq!(keyword(&db, "nodes", "shared"), ["shared"]);
    assert_eq!(
        relationship_keyword(&db, "relationships", "shared"),
        ["shared"]
    );
    assert_eq!(
        db.create_keyword_index("nodes", IndexTarget::RelationshipType("Same"), &["name"])
            .unwrap_err()
            .kind(),
        ErrorKind::IndexAlreadyExists
    );
}

#[test]
fn relationship_indexes_survive_reopening_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph.redb");
    {
        let db = Database::open(&path).unwrap();
        db.create_keyword_index(
            "trips",
            IndexTarget::RelationshipType("VISITED"),
            &["notes"],
        )
        .unwrap();
        run(
            &db,
            "INSERT (:Person)-[:VISITED {name: 'lima', notes: 'durable ceviche'}]->(:Person)",
        );
    }
    let db = Database::open(&path).unwrap();
    assert_eq!(relationship_keyword(&db, "trips", "ceviche"), ["lima"]);
}

fn brute_force(
    vectors: &[Vec<f64>],
    query: &[f64],
    metric: VectorMetric,
    limit: usize,
) -> Vec<(String, f64)> {
    let dot = |left: &[f64], right: &[f64]| -> f64 {
        left.iter()
            .zip(right)
            .map(|(left, right)| left * right)
            .sum()
    };
    let norm = |vector: &[f64]| dot(vector, vector).sqrt();
    let mut scored: Vec<(usize, f64)> = vectors
        .iter()
        .enumerate()
        .filter(|(_, vector)| metric != VectorMetric::Cosine || norm(vector) > 0.0)
        .map(|(position, vector)| {
            let score = match metric {
                VectorMetric::Cosine => dot(query, vector) / (norm(query) * norm(vector)),
                VectorMetric::Euclidean => {
                    0.0 - query
                        .iter()
                        .zip(vector)
                        .map(|(left, right)| (left - right) * (left - right))
                        .sum::<f64>()
                        .sqrt()
                }
                _ => dot(query, vector),
            };
            (position, score + 0.0)
        })
        .collect();
    scored.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    scored
        .into_iter()
        .take(limit)
        .map(|(position, score)| (position.to_string(), score))
        .collect()
}

fn metric() -> impl Strategy<Value = VectorMetric> {
    prop_oneof![
        Just(VectorMetric::Cosine),
        Just(VectorMetric::Euclidean),
        Just(VectorMetric::DotProduct),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn vector_search_returns_exactly_the_brute_force_top_k(
        vectors in prop::collection::vec(prop::collection::vec(-4i8..=4, 3), 0..24),
        query in prop::collection::vec(-4i8..=4, 3),
        metric in metric(),
        limit in 0usize..30,
    ) {
        let as_floats = |vector: &[i8]| vector.iter().map(|item| f64::from(*item)).collect::<Vec<_>>();
        let vectors: Vec<Vec<f64>> = vectors.iter().map(|vector| as_floats(vector)).collect();
        let query = as_floats(&query);
        prop_assume!(metric != VectorMetric::Cosine || query.iter().any(|item| *item != 0.0));
        let db = db();
        db.create_vector_index("v", IndexTarget::Label("V"), "at", 3, metric).unwrap();
        for (position, vector) in vectors.iter().enumerate() {
            let mut parameters = Parameters::new();
            parameters.insert("name".to_owned(), string(&position.to_string()));
            parameters.insert("at".to_owned(), list(vector.iter().map(|item| float(*item))));
            db.execute("INSERT (:V {name: $name, at: $at})", &parameters).unwrap();
        }
        let found = vector(&db, "v", &query, limit as i64);
        prop_assert_eq!(found, brute_force(&vectors, &query, metric, limit));
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn vector_search_scores_match_the_brute_force_bit_for_bit(
        (vectors, query) in (1usize..=8).prop_flat_map(|dimension| (
            prop::collection::vec(prop::collection::vec(-6i8..=6, dimension), 0..40),
            prop::collection::vec(-6i8..=6, dimension),
        )),
        metric in metric(),
        limit in 0usize..50,
    ) {
        let thirds = |vector: &[i8]| vector.iter().map(|item| f64::from(*item) / 3.0).collect::<Vec<_>>();
        let vectors: Vec<Vec<f64>> = vectors.iter().map(|vector| thirds(vector)).collect();
        let query = thirds(&query);
        prop_assume!(metric != VectorMetric::Cosine || query.iter().any(|item| *item != 0.0));
        let db = db();
        db.create_vector_index("v", IndexTarget::Label("V"), "at", query.len(), metric).unwrap();
        for (position, vector) in vectors.iter().enumerate() {
            let mut parameters = Parameters::new();
            parameters.insert("name".to_owned(), string(&position.to_string()));
            parameters.insert("at".to_owned(), list(vector.iter().map(|item| float(*item))));
            db.execute("INSERT (:V {name: $name, at: $at})", &parameters).unwrap();
        }
        let bits = |hits: Vec<(String, f64)>| {
            hits.into_iter().map(|(name, score)| (name, score.to_bits())).collect::<Vec<_>>()
        };
        let found = vector(&db, "v", &query, limit as i64);
        prop_assert_eq!(bits(found), bits(brute_force(&vectors, &query, metric, limit)));
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_phrase_finds_exactly_the_nodes_with_a_string_that_holds_its_words_in_a_row(
        documents in prop::collection::vec(
            prop::collection::vec(prop::collection::vec(0usize..3, 0..6), 0..4),
            0..12,
        ),
        phrase in prop::collection::vec(0usize..3, 1..4),
    ) {
        let words = ["red", "green", "blue"];
        let sentence = |picks: &[usize]| {
            picks.iter().map(|pick| words[*pick]).collect::<Vec<_>>().join(" ")
        };
        let db = db();
        db.create_keyword_index("tags", IndexTarget::Label("T"), &["tags"]).unwrap();
        for (position, strings) in documents.iter().enumerate() {
            let mut parameters = Parameters::new();
            parameters.insert("name".to_owned(), string(&position.to_string()));
            parameters.insert(
                "tags".to_owned(),
                list(strings.iter().map(|picks| string(&sentence(picks)))),
            );
            db.execute("INSERT (:T {name: $name, tags: $tags})", &parameters).unwrap();
        }
        let mut found = keyword(&db, "tags", &format!("\"{}\"", sentence(&phrase)));
        found.sort();
        let mut expected: Vec<String> = documents
            .iter()
            .enumerate()
            .filter(|(_, strings)| {
                strings.iter().any(|picks| picks.windows(phrase.len()).any(|window| window == phrase))
            })
            .map(|(position, _)| position.to_string())
            .collect();
        expected.sort();
        prop_assert_eq!(found, expected);
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_prefix_finds_exactly_the_nodes_with_a_word_that_starts_with_it(
        documents in prop::collection::vec(prop::collection::vec(0usize..4, 0..5), 0..12),
        prefix in prop::sample::select(vec!["r", "re", "rea", "read", "reader", "b", "x"]),
    ) {
        let words = ["red", "read", "reader", "blue"];
        let db = db();
        for (position, picks) in documents.iter().enumerate() {
            let mut parameters = Parameters::new();
            parameters.insert("name".to_owned(), string(&position.to_string()));
            let body = picks.iter().map(|pick| words[*pick]).collect::<Vec<_>>().join(" ");
            parameters.insert("body".to_owned(), string(&body));
            db.execute("INSERT (:Note {name: $name, body: $body})", &parameters).unwrap();
        }
        db.create_keyword_index("notes", IndexTarget::Label("Note"), &["body"]).unwrap();
        let mut found = keyword(&db, "notes", &format!("{prefix}*"));
        found.sort();
        let mut expected: Vec<String> = documents
            .iter()
            .enumerate()
            .filter(|(_, picks)| picks.iter().any(|pick| words[*pick].starts_with(prefix)))
            .map(|(position, _)| position.to_string())
            .collect();
        expected.sort();
        prop_assert_eq!(found, expected);
    }
}
