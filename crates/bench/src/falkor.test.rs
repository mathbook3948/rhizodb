use super::*;

#[test]
fn a_row_list_becomes_a_cypher_literal() {
    let rows = Param::Rows(vec![vec![
        ("id", Param::Int(1)),
        ("name", Param::Text("O'Hara \\ x".to_owned())),
        ("age", Param::NullInt),
        ("embedding", Param::Floats(vec![0.5, 1.0])),
    ]]);

    assert_eq!(
        literal(&rows),
        "[{id:1,name:'O\\'Hara \\\\ x',age:null,embedding:[0.5,1.0]}]"
    );
}

#[test]
fn a_server_without_a_url_is_unsupported_not_a_failure() {
    let engine = FalkorEngine::new(None);

    assert!(matches!(
        engine.open(Path::new(".")),
        Err(EngineError::Unsupported(_))
    ));
}
