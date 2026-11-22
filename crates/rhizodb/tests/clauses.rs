mod support;

use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

fn people() -> rhizodb::Database {
    let db = db();
    run(
        &db,
        "CREATE (a:Person {name: 'Ada', age: 36}), (b:Person {name: 'Bob', age: 25}), \
         (c:Person:Admin {name: 'Cy', age: 41}), (d:Robot {name: 'R2'}), \
         (a)-[:KNOWS {since: 2001}]->(b), (b)-[:KNOWS {since: 2010}]->(c), (a)-[:OWNS]->(d)",
    );
    db
}

#[test]
fn match_filters_by_label_and_property_map() {
    let db = people();
    let rows = run(&db, "MATCH (n:Person {age: 25}) RETURN n.name");
    assert_eq!(rows, [vec![string("Bob")]]);
    let rows = run(&db, "MATCH (n:Person:Admin) RETURN n.name");
    assert_eq!(rows, [vec![string("Cy")]]);
}

#[test]
fn match_follows_relationship_direction_and_type() {
    let db = people();
    let rows = run(&db, "MATCH (a {name: 'Bob'})-[:KNOWS]->(b) RETURN b.name");
    assert_eq!(rows, [vec![string("Cy")]]);
    let rows = run(&db, "MATCH (a {name: 'Bob'})<-[:KNOWS]-(b) RETURN b.name");
    assert_eq!(rows, [vec![string("Ada")]]);
    let rows = run(
        &db,
        "MATCH ({name: 'Bob'})-[:KNOWS]-(b) RETURN b.name ORDER BY b.name",
    );
    assert_eq!(rows, [vec![string("Ada")], vec![string("Cy")]]);
    let rows = run(
        &db,
        "MATCH ({name: 'Ada'})-[:KNOWS|OWNS]->(b) RETURN count(b)",
    );
    assert_eq!(rows, [vec![int(2)]]);
}

#[test]
fn where_keeps_only_rows_whose_predicate_is_true() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (n:Person) WHERE n.age > 30 AND n.name <> 'Cy' RETURN n.name",
    );
    assert_eq!(rows, [vec![string("Ada")]]);
    let rows = run(&db, "MATCH (n) WHERE n.age > 100 OR n.missing RETURN n");
    assert!(rows.is_empty());
}

#[test]
fn optional_match_pads_missing_matches_with_null() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (n:Person) OPTIONAL MATCH (n)-[:OWNS]->(r) RETURN n.name, r.name ORDER BY n.name",
    );
    assert_eq!(
        rows,
        [
            vec![string("Ada"), string("R2")],
            vec![string("Bob"), Value::Null],
            vec![string("Cy"), Value::Null],
        ]
    );
}

#[test]
fn a_relationship_is_used_at_most_once_per_match_pattern() {
    let db = db();
    run(&db, "CREATE (a)-[:R]->(b)");
    assert!(run(&db, "MATCH (a)-[r1]-(b)-[r2]-(c) RETURN a").is_empty());
    assert_eq!(
        run(&db, "MATCH (a)-[r1]-(b) MATCH (b)-[r2]-(c) RETURN a").len(),
        2
    );
}

#[test]
fn variable_length_relationships_respect_their_bounds() {
    let db = db();
    run(
        &db,
        "CREATE (:N {i: 0})-[:R]->(:N {i: 1})-[:R]->(:N {i: 2})-[:R]->(:N {i: 3})",
    );
    let reached = |query: &str| ints(&run(&db, query));
    assert_eq!(
        reached("MATCH ({i: 0})-[:R*]->(n) RETURN n.i ORDER BY n.i"),
        [1, 2, 3]
    );
    assert_eq!(
        reached("MATCH ({i: 0})-[:R*2..]->(n) RETURN n.i ORDER BY n.i"),
        [2, 3]
    );
    assert_eq!(
        reached("MATCH ({i: 0})-[:R*0..1]->(n) RETURN n.i ORDER BY n.i"),
        [0, 1]
    );
    assert_eq!(
        reached("MATCH ({i: 0})-[rs:R*..2]->(n) RETURN size(rs) ORDER BY size(rs)"),
        [1, 2]
    );
    assert_eq!(
        reached(
            "MATCH ({i: 1})-[r1]->()-[r2]->() WITH [r1, r2] AS rs \
             MATCH (a)-[rs*]->(b) RETURN a.i * 10 + b.i"
        ),
        [13]
    );
}

#[test]
fn with_star_passes_on_rows_even_without_variables() {
    let db = db();
    run(&db, "CREATE (), ()");
    run(&db, "MATCH () CREATE () WITH * CREATE ()");
    assert_eq!(single(&db, "MATCH (n) RETURN count(n)"), int(6));
    assert_eq!(
        error_kind(&db, "MATCH () RETURN *"),
        ErrorKind::NoVariablesInScope
    );
}

#[test]
fn named_paths_alternate_nodes_and_relationships() {
    let db = people();
    let value = single(&db, "MATCH p = ({name: 'Ada'})-[:KNOWS*2]->() RETURN p");
    let Value::Path(path) = value else {
        panic!("expected a path, got {value:?}");
    };
    let names: Vec<_> = path
        .nodes()
        .iter()
        .map(|node| node.properties["name"].clone())
        .collect();
    assert_eq!(names, [string("Ada"), string("Bob"), string("Cy")]);
    assert_eq!(path.relationships().len(), 2);
    assert_eq!(path.relationships()[0].start, path.nodes()[0].id);
    assert_eq!(
        single(
            &db,
            "MATCH p = ({name: 'Ada'})-[:KNOWS*2]->() RETURN [n IN nodes(p) | n.name]"
        ),
        list([string("Ada"), string("Bob"), string("Cy")])
    );
}

#[test]
fn return_names_columns_by_alias_or_by_expression_text() {
    let db = db();
    assert_eq!(
        columns(&db, "RETURN 1 + 2, 'x' AS name, toUpper('a')"),
        ["1 + 2", "name", "toUpper('a')"]
    );
    assert_eq!(
        run(&db, "WITH 1 AS b, 2 AS a RETURN *"),
        [vec![int(2), int(1)]]
    );
}

#[test]
fn with_projects_a_new_scope_and_filters() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (n:Person) WITH n.name AS name, n.age AS age WHERE age < 40 RETURN name ORDER BY name",
    );
    assert_eq!(rows, [vec![string("Ada")], vec![string("Bob")]]);
}

#[test]
fn with_where_sees_variables_bound_before_the_with() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (n:Person) WITH n.name AS name WHERE n.age < 40 RETURN name ORDER BY name",
    );
    assert_eq!(rows, [vec![string("Ada")], vec![string("Bob")]]);
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:Person) WITH n.name AS name WHERE n.age < 40 RETURN n"
        ),
        ErrorKind::UndefinedVariable
    );
    assert_eq!(
        error_kind(
            &db,
            "MATCH (n:Person) WITH count(*) AS c WHERE n.age < 40 RETURN c"
        ),
        ErrorKind::UndefinedVariable
    );
}

#[test]
fn distinct_removes_equivalent_rows() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND [1, 1, null, null, [1, null], [1, null]] AS x RETURN DISTINCT x",
    );
    assert_eq!(rows.len(), 3);
}

#[test]
fn order_by_sees_aliases_and_earlier_variables() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (n:Person) RETURN n.name AS name ORDER BY n.age DESC",
    );
    assert_eq!(
        rows,
        [vec![string("Cy")], vec![string("Ada")], vec![string("Bob")]]
    );
    let rows = run(&db, "MATCH (n:Person) RETURN n.age AS a ORDER BY a");
    assert_eq!(ints(&rows), [25, 36, 41]);
}

#[test]
fn skip_and_limit_page_through_ordered_rows() {
    let db = db();
    let rows = run(
        &db,
        "UNWIND range(1, 10) AS x RETURN x ORDER BY x SKIP 3 LIMIT 2",
    );
    assert_eq!(ints(&rows), [4, 5]);
    let rows = run(&db, "UNWIND range(1, 3) AS x RETURN x ORDER BY x OFFSET 2");
    assert_eq!(ints(&rows), [3]);
}

#[test]
fn unwind_expands_lists_and_drops_null() {
    let db = db();
    assert_eq!(ints(&run(&db, "UNWIND [1, 2, 3] AS x RETURN x")), [1, 2, 3]);
    assert!(run(&db, "UNWIND null AS x RETURN x").is_empty());
    assert!(run(&db, "UNWIND [] AS x RETURN x").is_empty());
}

#[test]
fn create_with_parameters_and_named_paths() {
    let db = db();
    let mut parameters = Parameters::new();
    parameters.insert(
        "props".to_owned(),
        map(&[("name", string("Ada")), ("age", int(36))]),
    );
    db.execute("CREATE (:Person $props)", &parameters).unwrap();
    assert_eq!(
        run(&db, "MATCH (n:Person) RETURN n.name, n.age"),
        [vec![string("Ada"), int(36)]]
    );
    let length = single(
        &db,
        "CREATE p = (:A)-[:R]->(:B)<-[:S]-(:C) RETURN length(p)",
    );
    assert_eq!(length, int(2));
    assert_eq!(
        single(&db, "MATCH (:C)-[:S]->(b:B) RETURN count(b)"),
        int(1)
    );
}

#[test]
fn insert_is_accepted_as_the_gql_form_of_create() {
    let db = db();
    run(&db, "INSERT (:A {x: 1})");
    assert_eq!(single(&db, "MATCH (n:A) RETURN n.x"), int(1));
}

#[test]
fn set_supports_every_form() {
    let db = db();
    run(&db, "CREATE (:N {a: 1, b: 2})");
    run(&db, "MATCH (n:N) SET n.c = 3, n.a = null");
    assert_eq!(
        single(&db, "MATCH (n:N) RETURN properties(n)"),
        map(&[("b", int(2)), ("c", int(3))])
    );
    run(&db, "MATCH (n:N) SET n += {d: 4, b: null}");
    assert_eq!(
        single(&db, "MATCH (n:N) RETURN properties(n)"),
        map(&[("c", int(3)), ("d", int(4))])
    );
    run(&db, "MATCH (n:N) SET n = {e: 5}");
    assert_eq!(
        single(&db, "MATCH (n:N) RETURN properties(n)"),
        map(&[("e", int(5))])
    );
    run(&db, "MATCH (n:N) SET n:M:O");
    assert_eq!(
        single(&db, "MATCH (n:N) RETURN n:M:N:O AND size(labels(n)) = 3"),
        boolean(true)
    );
}

#[test]
fn remove_drops_properties_and_labels() {
    let db = db();
    run(&db, "CREATE (:A:B {x: 1, y: 2})");
    run(&db, "MATCH (n:A) REMOVE n.x, n:B");
    assert_eq!(
        run(&db, "MATCH (n) RETURN labels(n), keys(n)"),
        [vec![list([string("A")]), list([string("y")])]]
    );
}

#[test]
fn delete_refuses_connected_nodes_unless_detached() {
    let db = db();
    run(&db, "CREATE (:A)-[:R]->(:B)");
    assert_eq!(
        error_kind(&db, "MATCH (a:A) DELETE a"),
        rhizodb::ErrorKind::DeleteConnectedNode
    );
    run(&db, "MATCH (a:A)-[r]->() DELETE r, a");
    assert_eq!(single(&db, "MATCH (n) RETURN count(n)"), int(1));
    run(&db, "CREATE (:C)-[:R]->(:D)");
    run(&db, "MATCH (c:C) DETACH DELETE c");
    assert_eq!(single(&db, "MATCH ()-[r]->() RETURN count(r)"), int(0));
    assert_eq!(single(&db, "MATCH (n) RETURN count(n)"), int(2));
}

#[test]
fn returning_a_deleted_element_commits_the_delete() {
    let db = db();
    run(&db, "CREATE (:A {x: 1})-[:R {w: 2}]->(:B)");
    let rows = run(&db, "MATCH (n:A) DETACH DELETE n RETURN n");
    let [row] = rows.as_slice() else {
        panic!("expected one row, got {rows:?}");
    };
    let [Value::Node(node)] = row.as_slice() else {
        panic!("expected one node, got {row:?}");
    };
    assert_eq!(node.labels, ["A".to_owned()].into());
    assert_eq!(node.properties.get("x"), Some(&int(1)));
    assert_eq!(single(&db, "MATCH (n:A) RETURN count(n)"), int(0));
    assert_eq!(single(&db, "MATCH ()-[r]->() RETURN count(r)"), int(0));
    assert_eq!(single(&db, "MATCH (n:B) RETURN count(n)"), int(1));
}

#[test]
fn returning_a_deleted_relationship_commits_the_delete() {
    let db = db();
    run(&db, "CREATE (:A)-[:R {w: 2}]->(:B)");
    let rows = run(&db, "MATCH ()-[r]->() DELETE r RETURN r");
    let [row] = rows.as_slice() else {
        panic!("expected one row, got {rows:?}");
    };
    let [Value::Relationship(relationship)] = row.as_slice() else {
        panic!("expected one relationship, got {row:?}");
    };
    assert_eq!(relationship.rel_type, "R");
    assert_eq!(relationship.properties.get("w"), Some(&int(2)));
    assert_eq!(single(&db, "MATCH ()-[r]->() RETURN count(r)"), int(0));
}

#[test]
fn merge_matches_or_creates() {
    let db = db();
    run(&db, "MERGE (n:A {k: 1}) ON CREATE SET n.created = true");
    run(&db, "MERGE (n:A {k: 1}) ON MATCH SET n.matched = true");
    assert_eq!(
        single(&db, "MATCH (n:A) RETURN properties(n)"),
        map(&[
            ("k", int(1)),
            ("created", boolean(true)),
            ("matched", boolean(true))
        ])
    );
}

#[test]
fn merge_with_a_null_property_value_fails_instead_of_creating_forever() {
    let db = db();
    run(&db, "CREATE (:A), (:B)");
    for query in [
        "MERGE (n:N {k: null})",
        "MATCH (a:A), (b:B) MERGE (a)-[r:X {k: null}]->(b)",
        "MATCH (a:A) MERGE (a)-[:X]->(n:N {k: null})",
    ] {
        assert_eq!(
            error_kind(&db, query),
            rhizodb::ErrorKind::NullMergeProperty,
            "{query}"
        );
    }
    let mut parameters = Parameters::new();
    parameters.insert("v".to_owned(), Value::Null);
    let error = db
        .execute("WITH $v AS v MERGE (n:N {k: v})", &parameters)
        .unwrap_err();
    assert_eq!(error.kind(), rhizodb::ErrorKind::NullMergeProperty);
    assert_eq!(single(&db, "MATCH (n) RETURN count(n)"), int(2));
    assert_eq!(single(&db, "MATCH ()-[r]->() RETURN count(r)"), int(0));
}

#[test]
fn aggregates_group_by_the_non_aggregated_items() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (n) RETURN labels(n)[0] AS label, count(*) AS c, collect(n.name) AS names \
         ORDER BY label",
    );
    assert_eq!(
        rows,
        [
            vec![
                string("Person"),
                int(3),
                list([string("Ada"), string("Bob"), string("Cy")])
            ],
            vec![string("Robot"), int(1), list([string("R2")])],
        ]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:Person) RETURN sum(n.age), avg(n.age), min(n.age), max(n.name)"
        ),
        [vec![int(102), float(34.0), int(25), string("Cy")]]
    );
}

#[test]
fn aggregates_over_no_rows_still_return_one_row_without_keys() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "MATCH (n) RETURN count(n), sum(n.x), avg(n.x), collect(n)"
        ),
        [vec![int(0), int(0), Value::Null, list([])]]
    );
    assert!(run(&db, "MATCH (n) RETURN n.x, count(*)").is_empty());
}

#[test]
fn distinct_aggregates_drop_duplicates_and_nulls() {
    let db = db();
    assert_eq!(
        run(
            &db,
            "UNWIND [1, 2, 2, null, 3, 3] AS x RETURN count(DISTINCT x), collect(DISTINCT x)"
        ),
        [vec![int(3), list([int(1), int(2), int(3)])]]
    );
}

#[test]
fn case_list_comprehensions_and_quantifiers() {
    let db = db();
    assert_eq!(
        single(
            &db,
            "RETURN [x IN range(1, 5) WHERE x % 2 = 1 | CASE x WHEN 3 THEN 'three' ELSE x END]"
        ),
        list([int(1), string("three"), int(5)])
    );
    assert_eq!(
        run(
            &db,
            "RETURN all(x IN [1, 2] WHERE x > 0), any(x IN [1, 2] WHERE x > 1), \
             none(x IN [1, 2] WHERE x > 5), single(x IN [1, 2] WHERE x > 1)"
        ),
        [vec![boolean(true); 4]]
    );
}

#[test]
fn scalar_and_string_functions() {
    let db = db();
    let rows = run(
        &db,
        "RETURN size([1, 2]), head([1, 2]), last([1, 2]), tail([1, 2]), reverse('abc'), \
         coalesce(null, 2), toInteger('42'), toFloat(1), toString(1.5), toBoolean('true'), \
         abs(-3), sign(-2.5), ceil(1.2), floor(1.8), round(2.5), sqrt(16), \
         substring('hello', 1, 3), replace('aXbX', 'X', '-'), split('a,b', ','), \
         left('hello', 2), right('hello', 2), trim('  x  '), toLower('AB')",
    );
    assert_eq!(
        rows,
        [vec![
            int(2),
            int(1),
            int(2),
            list([int(2)]),
            string("cba"),
            int(2),
            int(42),
            float(1.0),
            string("1.5"),
            boolean(true),
            int(3),
            int(-1),
            float(2.0),
            float(1.0),
            float(3.0),
            float(4.0),
            string("ell"),
            string("a-b-"),
            list([string("a"), string("b")]),
            string("he"),
            string("lo"),
            string("x"),
            string("ab"),
        ]]
    );
}

#[test]
fn graph_functions_read_elements() {
    let db = people();
    let rows = run(
        &db,
        "MATCH (a {name: 'Ada'})-[r:KNOWS]->(b) \
         RETURN type(r), startNode(r).name, endNode(r).name, id(a) = id(startNode(r)), r.since",
    );
    assert_eq!(
        rows,
        [vec![
            string("KNOWS"),
            string("Ada"),
            string("Bob"),
            boolean(true),
            int(2001)
        ]]
    );
}

#[test]
fn parameters_are_values_in_expressions() {
    let db = db();
    let mut parameters = Parameters::new();
    parameters.insert("xs".to_owned(), list([int(1), int(2)]));
    parameters.insert("n".to_owned(), int(10));
    let result = db
        .execute("UNWIND $xs AS x RETURN x * $n AS y", &parameters)
        .unwrap();
    assert_eq!(result.rows(), [vec![int(10)], vec![int(20)]]);
}

#[test]
fn union_concatenates_and_union_distinct_dedupes() {
    let db = db();
    assert_eq!(run(&db, "RETURN 1 AS x UNION ALL RETURN 1 AS x").len(), 2);
    assert_eq!(run(&db, "RETURN 1 AS x UNION RETURN 1 AS x").len(), 1);
}

#[test]
fn where_after_with_distinct_reads_projected_expressions_of_earlier_variables() {
    let db = db();
    run(
        &db,
        "CREATE ({name2: 'A', other: 1}), ({name2: 'A', other: 2}), ({name2: 'B', other: 3})",
    );
    assert_eq!(
        run(
            &db,
            "MATCH (a) WITH DISTINCT a.name2 AS name WHERE a.name2 = 'B' RETURN *"
        ),
        [vec![string("B")]]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (a) WITH DISTINCT a.name2 AS name WHERE a.name2 + 'x' = 'Ax' RETURN name"
        ),
        [vec![string("A")]]
    );
    for query in [
        "MATCH (a) WITH DISTINCT a.name2 AS name WHERE a.other = 3 RETURN name",
        "MATCH (a) WITH DISTINCT a.name2 AS name WHERE a IS NOT NULL RETURN name",
    ] {
        assert_eq!(
            error_kind(&db, query),
            rhizodb::ErrorKind::UndefinedVariable,
            "{query}"
        );
    }
}

#[test]
fn delete_takes_a_list_of_elements_whether_collected_or_written_out() {
    let db = db();
    for query in [
        "MATCH (n:A) WITH collect(n) AS ns DELETE ns",
        "MATCH (n:A) WITH [n] AS ns DELETE ns",
        "MATCH (n:A) DELETE [n]",
        "MATCH (n:A) WITH n.k AS k, [n] AS ns DELETE ns",
        "MATCH (n:A) WITH [n] + [] AS ns DETACH DELETE ns",
    ] {
        run(&db, "CREATE (:A {k: 1}), (:A {k: 2})");
        run(&db, query);
        assert_eq!(single(&db, "MATCH (n) RETURN count(*)"), int(0), "{query}");
    }
    assert_eq!(
        error_kind(&db, "MATCH (n) WITH 1 + 1 AS x DELETE x"),
        ErrorKind::InvalidArgumentType
    );
    run(&db, "CREATE (:A)");
    assert_eq!(
        error_kind(&db, "MATCH (n) WITH [1] AS xs DELETE xs"),
        ErrorKind::TypeMismatch
    );
}
