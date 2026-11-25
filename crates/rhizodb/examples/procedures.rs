use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Procedure;
use rhizodb::ProcedureOutput;
use rhizodb::Value;
use rhizodb::ValueType;

fn main() -> Result<(), rhizodb::Error> {
    let db = Database::in_memory()?;
    db.register_procedure(
        Procedure::new("text.words", |args| {
            let Some(Value::String(text)) = args.first() else {
                return Ok(ProcedureOutput::new());
            };
            Ok(text
                .split_whitespace()
                .map(|word| vec![Value::String(word.to_lowercase())])
                .collect())
        })
        .parameter("text", ValueType::String)
        .output("word"),
    )?;

    db.execute(
        "INSERT (:Post {body: 'Graphs make joins cheap'}), (:Post {body: 'Cheap joins win'})",
        &Parameters::new(),
    )?;
    let counts = db.execute(
        "MATCH (p:Post) CALL text.words(p.body) YIELD word \
         RETURN word, count(*) AS uses ORDER BY uses DESC, word LIMIT 2",
        &Parameters::new(),
    )?;
    for row in counts.rows() {
        println!("{row:?}");
    }
    assert_eq!(
        counts.rows(),
        [
            vec![Value::from("cheap"), Value::Integer(2)],
            vec![Value::from("joins"), Value::Integer(2)],
        ]
    );
    Ok(())
}
