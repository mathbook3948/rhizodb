use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use rhizodb::VectorMetric;

fn main() -> Result<(), rhizodb::Error> {
    let db = Database::in_memory()?;
    db.create_keyword_index("abstracts", IndexTarget::Label("Paper"), &["abstract"])?;
    db.create_vector_index(
        "embeddings",
        IndexTarget::Label("Paper"),
        "embedding",
        3,
        VectorMetric::Cosine,
    )?;

    let papers = [
        (
            "Graph traversal at scale",
            "breadth first search over large graphs",
            [0.9, 0.1, 0.0],
        ),
        (
            "Ranking text",
            "keyword search ranked with BM25",
            [0.1, 0.9, 0.0],
        ),
        (
            "Nearest neighbours",
            "exact vector search with cosine distance",
            [0.0, 0.2, 0.9],
        ),
    ];
    for (title, text, embedding) in papers {
        db.execute(
            "INSERT (:Paper {title: $title, abstract: $text, embedding: $embedding})",
            &Parameters::from([
                ("title", Value::from(title)),
                ("text", Value::from(text)),
                (
                    "embedding",
                    Value::List(embedding.map(Value::Float).to_vec()),
                ),
            ]),
        )?;
    }

    let words = db.execute(
        "CALL db.index.keyword.queryNodes('abstracts', 'search', 10) YIELD node, score \
         RETURN node.title AS title, score ORDER BY score DESC",
        &Parameters::new(),
    )?;
    for row in words.rows() {
        println!("keyword hit {row:?}");
    }

    let nearest = db.execute(
        "CALL db.index.vector.queryNodes('embeddings', [0.0, 0.1, 1.0], 1) YIELD node \
         RETURN node.title AS title",
        &Parameters::new(),
    )?;
    println!("nearest {:?}", nearest.rows());
    assert_eq!(nearest.rows(), [vec![Value::from("Nearest neighbours")]]);
    Ok(())
}
