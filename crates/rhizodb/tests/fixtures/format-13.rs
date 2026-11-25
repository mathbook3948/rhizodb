//! Writes the format 13 fixture through the public API of the last commit that wrote format 13.
//! `generate-format-13.sh` builds it against that commit. Run as `format-13 <path>`, it writes
//! the database, compacts it, closes it, and prints each query the upgrade test checks, each
//! followed by its answer on an indented line, which the script keeps in `format-13.answers`.
//! Its keyword indexes over nodes and over relationships hold one posting entry per term and
//! element, across many hundreds of elements with gaps from deleted elements, documents
//! rewritten after they were indexed, terms of Hangul and Han, list values, and rare terms.

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;

const QUERIES: &[&str] = &[
    "MATCH (d:Doc) RETURN count(*), min(id(d)), max(id(d))",
    "CALL db.index.keyword.queryNodes('doc text', 'graph', 10) YIELD node, score RETURN id(node), node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', 'river', 1000) YIELD node RETURN count(*), sum(node.i)",
    "CALL db.index.keyword.queryNodes('doc text', 'quokka', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', 'stor*', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', '\"graph store\"', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', '\"store graph\" ocean', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', '그래프 데이터', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', '图数据', 10) YIELD node, score RETURN node.i, score",
    "CALL db.index.keyword.queryNodes('doc text', 'rewritten', 1000) YIELD node RETURN count(*), sum(node.i)",
    "CALL db.index.keyword.queryNodes('doc tags', 'tag7', 1000) YIELD node RETURN count(*), sum(node.i)",
    "CALL db.index.keyword.queryNodes('doc text', 'gone', 10) YIELD node RETURN count(*)",
    "CALL db.index.keyword.queryRelationships('note text', 'bridge', 10) YIELD relationship, score RETURN id(relationship), relationship.k, score",
    "CALL db.index.keyword.queryRelationships('note text', '\"friend of\"', 1000) YIELD relationship RETURN count(*), sum(relationship.k)",
    "CALL db.index.keyword.queryRelationships('note text', 'fr*', 1000) YIELD relationship RETURN count(*)",
];

const WORDS: &[&str] = &[
    "graph", "store", "river", "ocean", "forest", "bridge", "market", "train", "summer", "winter",
    "storage", "stormy", "friend", "of", "city",
];

fn run(db: &Database, query: &str) {
    let result = db.execute(query, &Parameters::new()).unwrap();
    println!("{query}\n  {:?}", result.rows());
}

fn exec(db: &Database, query: &str) {
    db.execute(query, &Parameters::new()).unwrap();
}

fn text(i: usize) -> String {
    let words: Vec<&str> = (0..(3 + i % 9))
        .map(|at| WORDS[(i * 7 + at * at * 3 + at) % WORDS.len()])
        .collect();
    let mut text = words.join(" ");
    if i % 97 == 0 {
        text.push_str(" quokka");
    }
    if i % 13 == 0 {
        text.push_str(" 그래프 데이터베이스");
    }
    if i % 17 == 0 {
        text.push_str(" 图数据库");
    }
    text
}

fn build(db: &Database) {
    db.create_keyword_index("doc text", IndexTarget::Label("Doc"), &["title", "text"])
        .unwrap();
    db.create_keyword_index(
        "note text",
        IndexTarget::RelationshipType("NOTE"),
        &["note"],
    )
    .unwrap();
    for i in 0..1500 {
        let tags = format!("['tag{}', 'tag{}']", i % 10, i % 4);
        exec(
            db,
            &format!(
                "CREATE (:Doc {{i: {i}, title: 'doc {i}', text: '{}', tags: {tags}}})",
                text(i)
            ),
        );
        if i % 5 == 0 {
            exec(db, &format!("CREATE (:Other {{i: {i}, text: 'graph'}})"));
        }
    }
    db.create_keyword_index("doc tags", IndexTarget::Label("Doc"), &["tags"])
        .unwrap();
    exec(
        db,
        "MATCH (a:Doc), (b:Doc) WHERE b.i = a.i + 1 AND a.i % 3 = 0 CREATE (a)-[:NOTE {k: a.i, note: CASE WHEN a.i % 2 = 0 THEN 'friend of the bridge' ELSE 'friend from the city' END}]->(b)",
    );
    exec(
        db,
        "MATCH (d:Doc) WHERE d.i % 11 = 0 SET d.text = 'gone soon'",
    );
    exec(db, "MATCH (d:Doc) WHERE d.i % 11 = 0 DETACH DELETE d");
    exec(
        db,
        "MATCH (d:Doc) WHERE d.i % 23 = 4 SET d.text = 'rewritten graph store graph'",
    );
    exec(
        db,
        "MATCH (d:Doc) WHERE d.i % 29 = 5 SET d.text = ['listed river', 'graph store']",
    );
    exec(db, "MATCH ()-[r:NOTE]->() WHERE r.k % 7 = 0 DELETE r");
    for query in QUERIES {
        run(db, query);
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        panic!("usage: format-13 <path>");
    };
    let db = Database::open(&path).unwrap();
    build(&db);
    db.compact().unwrap();
}
