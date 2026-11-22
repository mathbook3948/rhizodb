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
