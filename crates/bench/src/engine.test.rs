use super::*;

#[test]
fn a_graph_query_has_gql_and_cypher_forms_only() {
    let texts = Texts::graph("MATCH (n) RETURN n");

    assert_eq!(texts.for_dialect(Dialect::Gql), Ok("MATCH (n) RETURN n"));
    assert_eq!(texts.for_dialect(Dialect::Cypher), Ok("MATCH (n) RETURN n"));
    assert!(matches!(
        texts.for_dialect(Dialect::Sql),
        Err(EngineError::Unsupported(_))
    ));
    assert!(matches!(
        texts.for_dialect(Dialect::Datalog),
        Err(EngineError::Unsupported(_))
    ));
}

#[test]
fn a_query_can_differ_per_dialect() {
    let texts = Texts {
        gql: Some("g"),
        cypher: Some("c"),
        sql: Some("s"),
        datalog: None,
        overrides: &[],
    };

    assert_eq!(texts.for_dialect(Dialect::Gql), Ok("g"));
    assert_eq!(texts.for_dialect(Dialect::Cypher), Ok("c"));
    assert_eq!(texts.for_dialect(Dialect::Sql), Ok("s"));
}

#[test]
fn directory_size_adds_up_nested_files() {
    let dir = std::env::temp_dir().join(format!("bench-size-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("inner")).unwrap();
    fs::write(dir.join("a"), [0u8; 10]).unwrap();
    fs::write(dir.join("inner").join("b"), [0u8; 5]).unwrap();

    assert_eq!(directory_size(&dir).unwrap(), 15);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn an_override_replaces_the_text_of_one_engine_only() {
    let texts = Texts::graph("MATCH (n) RETURN n").overriding(&[(EngineKind::Falkordb, "special")]);

    assert_eq!(
        texts.for_engine(EngineKind::Falkordb, Dialect::Cypher),
        Ok("special")
    );
    assert_eq!(
        texts.for_engine(EngineKind::Neo4j, Dialect::Cypher),
        Ok("MATCH (n) RETURN n")
    );
}

#[test]
fn every_engine_has_a_unique_name_a_category_and_a_storage() {
    let names: std::collections::HashSet<_> =
        EngineKind::ALL.iter().map(|kind| kind.name()).collect();

    assert_eq!(names.len(), EngineKind::ALL.len());
    for kind in EngineKind::ALL {
        assert_eq!(EngineKind::parse(kind.name()), Some(kind));
    }
    assert_eq!(EngineKind::Neo4j.category(), Category::Server);
    assert_eq!(EngineKind::Sqlite.category(), Category::Embedded);
    assert_eq!(EngineKind::Memgraph.storage(), Storage::Memory);
    assert_eq!(EngineKind::Duckdb.storage(), Storage::File);
}
