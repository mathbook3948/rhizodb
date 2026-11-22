use std::collections::BTreeMap;

use super::*;
use crate::run::run_scenario;
use crate::scenario::Expected;
use crate::scenario::GraphSetup;
use crate::scenario::Ordering;
use crate::scenario::Scenario;
use crate::value::Lists;

fn procedure(signature: &str) -> Procedure {
    Procedure {
        signature: signature.into(),
        columns: vec![],
        rows: vec![],
    }
}

#[test]
fn a_procedure_with_a_valid_signature_is_registered() {
    let mut graph = RhizoDb.fresh_graph();

    assert_eq!(
        graph.register_procedure(&procedure("test.doNothing() :: ()")),
        Ok(())
    );
}

#[test]
fn a_signature_that_does_not_parse_is_an_adapter_error() {
    let mut graph = RhizoDb.fresh_graph();

    assert!(
        graph
            .register_procedure(&procedure("test.broken(:: INTEGER"))
            .is_err()
    );
}

#[test]
fn a_scenario_whose_procedure_cannot_be_registered_fails() {
    let scenario = Scenario {
        feature: "f.feature".into(),
        name: "s".into(),
        example: None,
        tags: Default::default(),
        graph: GraphSetup::Empty,
        procedures: vec![procedure("test.broken(:: INTEGER")],
        setup: vec![],
        parameters: BTreeMap::new(),
        query: "RETURN 1 AS n".into(),
        expected: Expected::Rows {
            columns: vec!["n".into()],
            rows: vec![vec![Value::Integer(1)]],
            ordering: Ordering::AnyOrder,
            lists: Lists::Strict,
        },
        side_effects: None,
        control: None,
    };

    let failure = run_scenario(&RhizoDb, &scenario, None).unwrap_err();

    assert!(failure.contains("test.broken(:: INTEGER"), "{failure}");
}
