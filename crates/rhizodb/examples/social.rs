use rhizodb::Database;
use rhizodb::Parameters;
use rhizodb::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("social.rhizodb");

    let db = Database::open(&path)?;
    let mut tx = db.begin_write()?;
    for (name, city) in [
        ("Ada", "London"),
        ("Grace", "New York"),
        ("Alan", "London"),
        ("Edsger", "Austin"),
    ] {
        tx.execute(
            "MERGE (c:City {name: $city}) INSERT (:Person {name: $name})-[:LIVES_IN]->(c)",
            &Parameters::from([("name", name), ("city", city)]),
        )?;
    }
    for (from, to) in [("Ada", "Grace"), ("Grace", "Alan"), ("Alan", "Edsger")] {
        tx.execute(
            "MATCH (a:Person {name: $from}), (b:Person {name: $to}) INSERT (a)-[:KNOWS]->(b)",
            &Parameters::from([("from", from), ("to", to)]),
        )?;
    }
    tx.commit()?;
    drop(db);

    let db = Database::open(&path)?;
    let reachable = db.execute(
        "MATCH (:Person {name: $name})-[:KNOWS*1..3]->(b:Person) \
         RETURN DISTINCT b.name AS name ORDER BY name",
        &Parameters::from([("name", "Ada")]),
    )?;
    println!("Ada reaches {:?}", reachable.rows());

    let per_city = db.execute(
        "MATCH (p:Person)-[:LIVES_IN]->(c:City) \
         RETURN c.name AS city, count(p) AS people ORDER BY people DESC, city",
        &Parameters::new(),
    )?;
    for row in per_city.rows() {
        println!("{row:?}");
    }
    assert_eq!(
        per_city.rows().first(),
        Some(&vec![Value::from("London"), Value::Integer(2)])
    );
    Ok(())
}
