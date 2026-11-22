use super::*;

const SIZE: Size = Size {
    people: 30,
    knows: 60,
};

#[test]
fn a_seed_always_gives_the_same_graph() {
    assert_eq!(Graph::generate(3, SIZE), Graph::generate(3, SIZE));
    assert_ne!(Graph::generate(3, SIZE), Graph::generate(4, SIZE));
}

#[test]
fn edges_only_join_nodes_that_exist() {
    let graph = Graph::generate(3, SIZE);
    let people: Vec<i64> = graph.people.iter().map(|person| person.id).collect();
    let cities: Vec<i64> = graph.cities.iter().map(|city| city.id).collect();

    assert_eq!(graph.people.len(), SIZE.people);
    assert_eq!(graph.knows.len(), SIZE.knows);
    assert!(
        graph
            .knows
            .iter()
            .all(|edge| people.contains(&edge.from) && people.contains(&edge.to))
    );
    assert!(
        graph
            .lives_in
            .iter()
            .all(|edge| people.contains(&edge.person) && cities.contains(&edge.city))
    );
}

#[test]
fn the_graph_has_missing_ages_and_self_loops_to_exercise_nulls_and_matching() {
    let graph = Graph::generate(3, SIZE);

    assert!(graph.people.iter().any(|person| person.age.is_none()));
    assert!(graph.people.iter().any(|person| person.age.is_some()));
    assert!(graph.knows.iter().any(|edge| edge.from == edge.to));
}

#[test]
fn a_missing_age_is_left_out_of_the_statement() {
    let graph = Graph {
        people: vec![
            Person {
                id: 1,
                name: "Ada".into(),
                age: None,
            },
            Person {
                id: 2,
                name: "Alan".into(),
                age: Some(41),
            },
        ],
        cities: Vec::new(),
        knows: vec![Knows {
            from: 1,
            to: 2,
            since: 2000,
        }],
        lives_in: Vec::new(),
    };

    assert_eq!(
        graph.statements(),
        vec![
            "CREATE (:Person {id: 1, name: 'Ada'})",
            "CREATE (:Person {id: 2, name: 'Alan', age: 41})",
            "MATCH (a:Person {id: 1}), (b:Person {id: 2}) CREATE (a)-[:KNOWS {since: 2000}]->(b)",
        ]
    );
}
