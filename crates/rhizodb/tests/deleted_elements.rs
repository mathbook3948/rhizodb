mod support;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const NODE_LABELS: usize = 2;
const REL_TYPES: usize = 2;

type Outcome = Result<Vec<Vec<Value>>, ErrorKind>;

#[derive(Clone, Debug)]
struct Graph {
    nodes: Vec<usize>,
    relationships: Vec<(usize, usize, usize)>,
}

#[derive(Clone, Debug)]
enum Target {
    Node {
        label: Option<usize>,
        below: Option<usize>,
    },
    Relationship {
        rel_type: Option<usize>,
    },
}

#[derive(Clone, Debug)]
enum Elem {
    Variable,
    Parameter,
    Alias(usize),
    Head(Box<Elem>),
    Coalesce(Box<Elem>),
    CaseTrue(Box<Elem>),
}

#[derive(Clone, Debug)]
enum Value1 {
    Id(Elem),
    Type(Elem),
    HasType(Elem, usize),
    Equals(Elem, Elem),
    IsNull(Elem),
    SizeOfList(Elem),
    ComprehensionIds(Elem),
    ComprehensionElements(Elem),
    AnyId(Elem),
    CaseId(Elem),
}

#[derive(Clone, Debug)]
enum Pred {
    NotNull(Elem),
    Equals(Elem, Elem),
    HasType(Elem, usize),
    ValueNotNull(Value1),
}

#[derive(Clone, Debug)]
enum Output {
    Constant,
    Elem(Elem),
    Value(Value1),
    ListOf(Elem),
    MapOf(Elem, Value1),
    CaseWhen(Pred, Elem),
}

#[derive(Clone, Debug)]
enum Middle {
    With(Option<Elem>),
    WithWhere(Option<Elem>, Pred),
    WithOrder(Value1),
    Filter(Pred),
    Let(Elem),
    Unwind(Elem),
}

#[derive(Clone, Copy, Debug)]
enum Aggregate {
    Count,
    Collect,
    CollectDistinct,
}

#[derive(Clone, Debug)]
enum Last {
    Return {
        items: Vec<Output>,
        order: Option<Value1>,
        distinct: bool,
    },
    Aggregate {
        key: Option<Value1>,
        aggregates: Vec<(Aggregate, Output)>,
    },
}

#[derive(Clone, Copy, Debug)]
enum Layout {
    Single,
    ReadThenDelete { distinct: bool },
    DeleteThenRead,
}

#[derive(Clone, Debug)]
struct Case {
    graph: Graph,
    target: Target,
    middle: Vec<Middle>,
    last: Last,
    union_outputs: (Output, Output),
    layout: Layout,
}

fn elem() -> impl Strategy<Value = Elem> {
    let leaf = prop_oneof![
        Just(Elem::Variable),
        Just(Elem::Parameter),
        (0usize..4).prop_map(Elem::Alias),
    ];
    leaf.prop_recursive(2, 6, 1, |inner| {
        prop_oneof![
            inner.clone().prop_map(|e| Elem::Head(Box::new(e))),
            inner.clone().prop_map(|e| Elem::Coalesce(Box::new(e))),
            inner.prop_map(|e| Elem::CaseTrue(Box::new(e))),
        ]
    })
}

fn value1() -> impl Strategy<Value = Value1> {
    prop_oneof![
        elem().prop_map(Value1::Id),
        elem().prop_map(Value1::Type),
        (elem(), 0..REL_TYPES).prop_map(|(e, t)| Value1::HasType(e, t)),
        (elem(), elem()).prop_map(|(a, b)| Value1::Equals(a, b)),
        elem().prop_map(Value1::IsNull),
        elem().prop_map(Value1::SizeOfList),
        elem().prop_map(Value1::ComprehensionIds),
        elem().prop_map(Value1::ComprehensionElements),
        elem().prop_map(Value1::AnyId),
        elem().prop_map(Value1::CaseId),
    ]
}

fn pred() -> impl Strategy<Value = Pred> {
    prop_oneof![
        elem().prop_map(Pred::NotNull),
        (elem(), elem()).prop_map(|(a, b)| Pred::Equals(a, b)),
        (elem(), 0..REL_TYPES).prop_map(|(e, t)| Pred::HasType(e, t)),
        value1().prop_map(Pred::ValueNotNull),
    ]
}

fn output() -> impl Strategy<Value = Output> {
    prop_oneof![
        3 => Just(Output::Constant),
        3 => elem().prop_map(Output::Elem),
        2 => value1().prop_map(Output::Value),
        1 => elem().prop_map(Output::ListOf),
        1 => (elem(), value1()).prop_map(|(e, v)| Output::MapOf(e, v)),
        1 => (pred(), elem()).prop_map(|(p, e)| Output::CaseWhen(p, e)),
    ]
}

fn middle() -> impl Strategy<Value = Middle> {
    prop_oneof![
        prop::option::of(elem()).prop_map(Middle::With),
        (prop::option::of(elem()), pred()).prop_map(|(e, p)| Middle::WithWhere(e, p)),
        value1().prop_map(Middle::WithOrder),
        pred().prop_map(Middle::Filter),
        elem().prop_map(Middle::Let),
        elem().prop_map(Middle::Unwind),
    ]
}

fn aggregate() -> impl Strategy<Value = Aggregate> {
    prop_oneof![
        Just(Aggregate::Count),
        Just(Aggregate::Collect),
        Just(Aggregate::CollectDistinct),
    ]
}

fn last() -> impl Strategy<Value = Last> {
    prop_oneof![
        (
            prop::collection::vec(output(), 1..3),
            prop::option::of(value1()),
            any::<bool>(),
        )
            .prop_map(|(items, order, distinct)| Last::Return {
                items,
                order,
                distinct,
            }),
        (
            prop::option::of(value1()),
            prop::collection::vec((aggregate(), output()), 1..3),
        )
            .prop_map(|(key, aggregates)| Last::Aggregate { key, aggregates }),
    ]
}

fn graph() -> impl Strategy<Value = Graph> {
    (2usize..5).prop_flat_map(|count| {
        (
            prop::collection::vec(0..NODE_LABELS, count),
            prop::collection::vec((0..count, 0..count, 0..REL_TYPES), 1..5),
        )
            .prop_map(|(nodes, relationships)| Graph {
                nodes,
                relationships,
            })
    })
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        (
            prop::option::of(0..NODE_LABELS),
            prop::option::of(1usize..5)
        )
            .prop_map(|(label, below)| Target::Node { label, below }),
        prop::option::of(0..REL_TYPES).prop_map(|rel_type| Target::Relationship { rel_type }),
    ]
}

fn layout() -> impl Strategy<Value = Layout> {
    prop_oneof![
        3 => Just(Layout::Single),
        1 => any::<bool>().prop_map(|distinct| Layout::ReadThenDelete { distinct }),
        1 => Just(Layout::DeleteThenRead),
    ]
}

fn case() -> impl Strategy<Value = Case> {
    (
        graph(),
        target(),
        prop::collection::vec(middle(), 0..4),
        last(),
        (output(), output()),
        layout(),
    )
        .prop_map(
            |(graph, target, middle, last, union_outputs, layout)| Case {
                graph,
                target,
                middle,
                last,
                union_outputs,
                layout,
            },
        )
}

impl Target {
    fn is_relationship(&self) -> bool {
        matches!(self, Target::Relationship { .. })
    }

    fn pattern(&self) -> String {
        match self {
            Target::Node { label, below } => {
                let label = label.map(|label| format!(":N{label}")).unwrap_or_default();
                let filter = below
                    .map(|below| format!(" WHERE x.v < {below}"))
                    .unwrap_or_default();
                format!("MATCH (x{label}){filter}")
            }
            Target::Relationship { rel_type } => {
                let rel_type = rel_type
                    .map(|rel_type| format!(":T{rel_type}"))
                    .unwrap_or_default();
                format!("MATCH ()-[x{rel_type}]->()")
            }
        }
    }

    fn delete(&self) -> &'static str {
        match self {
            Target::Node { .. } => "DETACH DELETE x",
            Target::Relationship { .. } => "DELETE x",
        }
    }
}

struct Render {
    aliases: usize,
    has_parameter: bool,
    relationship: bool,
}

impl Render {
    fn elem(&self, elem: &Elem) -> String {
        match elem {
            Elem::Variable => "x".to_owned(),
            Elem::Parameter if self.has_parameter => "$p".to_owned(),
            Elem::Parameter => "x".to_owned(),
            Elem::Alias(index) if self.aliases > 0 => format!("a{}", index % self.aliases),
            Elem::Alias(_) => "x".to_owned(),
            Elem::Head(inner) => format!("head([{}])", self.elem(inner)),
            Elem::Coalesce(inner) => format!("coalesce(null, {})", self.elem(inner)),
            Elem::CaseTrue(inner) => format!("CASE WHEN true THEN {} END", self.elem(inner)),
        }
    }

    fn simple(&self, elem: &Elem) -> String {
        match elem {
            Elem::Variable | Elem::Parameter | Elem::Alias(_) => self.elem(elem),
            _ => "x".to_owned(),
        }
    }

    fn has_type(&self, elem: &Elem, rel_type: usize) -> String {
        if self.relationship {
            format!("{}:T{rel_type}", self.simple(elem))
        } else {
            format!("{} IS NOT NULL", self.elem(elem))
        }
    }

    fn value(&self, value: &Value1) -> String {
        match value {
            Value1::Id(e) => format!("id({})", self.elem(e)),
            Value1::Type(e) if self.relationship => format!("type({})", self.elem(e)),
            Value1::Type(e) => format!("id({})", self.elem(e)),
            Value1::HasType(e, t) => format!("({})", self.has_type(e, *t)),
            Value1::Equals(a, b) => format!("({} = {})", self.elem(a), self.elem(b)),
            Value1::IsNull(e) => format!("({} IS NULL)", self.elem(e)),
            Value1::SizeOfList(e) => format!("size([{}, {}])", self.elem(e), self.elem(e)),
            Value1::ComprehensionIds(e) => format!("[e IN [{}] | id(e)]", self.elem(e)),
            Value1::ComprehensionElements(e) => {
                format!("[e IN [{}] WHERE e IS NOT NULL | e]", self.elem(e))
            }
            Value1::AnyId(e) => format!("any(e IN [{}] WHERE id(e) >= 0)", self.elem(e)),
            Value1::CaseId(e) => format!(
                "CASE WHEN {0} IS NULL THEN -1 ELSE id({0}) END",
                self.elem(e)
            ),
        }
    }

    fn pred(&self, pred: &Pred) -> String {
        match pred {
            Pred::NotNull(e) => format!("{} IS NOT NULL", self.elem(e)),
            Pred::Equals(a, b) => format!("{} = {}", self.elem(a), self.elem(b)),
            Pred::HasType(e, t) => self.has_type(e, *t),
            Pred::ValueNotNull(value) => format!("{} IS NOT NULL", self.value(value)),
        }
    }

    fn output(&self, output: &Output) -> String {
        match output {
            Output::Constant => "1".to_owned(),
            Output::Elem(e) => self.elem(e),
            Output::Value(value) => self.value(value),
            Output::ListOf(e) => format!("[{}, {}]", self.elem(e), self.elem(e)),
            Output::MapOf(e, value) => format!("{{k: {}, n: {}}}", self.elem(e), self.value(value)),
            Output::CaseWhen(pred, e) => {
                format!(
                    "CASE WHEN {} THEN {} ELSE null END",
                    self.pred(pred),
                    self.elem(e)
                )
            }
        }
    }

    fn carried(&self) -> String {
        let mut names = vec!["x".to_owned()];
        names.extend((0..self.aliases).map(|index| format!("a{index}")));
        names.join(", ")
    }

    fn middle(&mut self, middle: &Middle) -> String {
        match middle {
            Middle::With(alias) => {
                let (extra, grows) = self.new_alias(alias.as_ref());
                let text = format!("WITH {}{extra}", self.carried());
                self.aliases += usize::from(grows);
                text
            }
            Middle::WithWhere(alias, pred) => {
                let (extra, grows) = self.new_alias(alias.as_ref());
                let carried = self.carried();
                self.aliases += usize::from(grows);
                format!("WITH {carried}{extra} WHERE {}", self.pred(pred))
            }
            Middle::WithOrder(value) => {
                format!("WITH {} ORDER BY {}", self.carried(), self.value(value))
            }
            Middle::Filter(pred) => format!("FILTER {}", self.pred(pred)),
            Middle::Let(e) => {
                let text = format!("LET a{} = {}", self.aliases, self.elem(e));
                self.aliases += 1;
                text
            }
            Middle::Unwind(e) => {
                let text = format!("UNWIND [{0}, {0}] AS a{1}", self.elem(e), self.aliases);
                self.aliases += 1;
                text
            }
        }
    }

    fn new_alias(&self, alias: Option<&Elem>) -> (String, bool) {
        match alias {
            Some(e) => (format!(", {} AS a{}", self.elem(e), self.aliases), true),
            None => (String::new(), false),
        }
    }

    fn aggregate(&self, aggregate: Aggregate, output: &Output) -> String {
        let body = self.output(output);
        match aggregate {
            Aggregate::Count => format!("count({body})"),
            Aggregate::Collect => format!("collect({body})"),
            Aggregate::CollectDistinct => format!("collect(DISTINCT {body})"),
        }
    }

    fn last(&self, last: &Last) -> String {
        match last {
            Last::Return {
                items,
                order,
                distinct,
            } => {
                let items: Vec<String> = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| format!("{} AS c{index}", self.output(item)))
                    .collect();
                let order = match order {
                    Some(value) if !distinct => format!(" ORDER BY {}", self.value(value)),
                    _ => String::new(),
                };
                let distinct = if *distinct { "DISTINCT " } else { "" };
                format!("RETURN {distinct}{}{order}", items.join(", "))
            }
            Last::Aggregate { key, aggregates } => {
                let mut items = Vec::new();
                if let Some(key) = key {
                    items.push(format!("{} AS k", self.value(key)));
                }
                for (index, (aggregate, output)) in aggregates.iter().enumerate() {
                    items.push(format!(
                        "{} AS c{index}",
                        self.aggregate(*aggregate, output)
                    ));
                }
                format!("RETURN {}", items.join(", "))
            }
        }
    }
}

impl Case {
    fn render(&self, has_parameter: bool, deleting: bool) -> String {
        let mut render = Render {
            aliases: 0,
            has_parameter,
            relationship: self.target.is_relationship(),
        };
        let delete = if deleting { self.target.delete() } else { "" };
        let pattern = self.target.pattern();
        let part = |output: &Output, deleting_part: bool, render: &Render| {
            let delete = if deleting_part { delete } else { "" };
            format!("{pattern} {delete} RETURN {} AS r", render.output(output))
        };
        match self.layout {
            Layout::Single => {
                let mut clauses = vec![format!("{pattern} {delete}")];
                if matches!(
                    self.middle.first(),
                    Some(Middle::Filter(_) | Middle::Let(_) | Middle::Unwind(_))
                ) {
                    clauses.push("WITH x".to_owned());
                }
                for middle in &self.middle {
                    clauses.push(render.middle(middle));
                }
                clauses.push(render.last(&self.last));
                clauses.join(" ")
            }
            Layout::ReadThenDelete { distinct } => {
                let union = if distinct { "UNION" } else { "UNION ALL" };
                format!(
                    "{} {union} {}",
                    part(&self.union_outputs.0, false, &render),
                    part(&self.union_outputs.1, true, &render)
                )
            }
            Layout::DeleteThenRead => {
                format!(
                    "{} UNION ALL {}",
                    part(&self.union_outputs.0, true, &render),
                    part(&self.union_outputs.1, false, &render)
                )
            }
        }
    }

    fn oracle_query(&self, has_parameter: bool) -> String {
        match self.layout {
            Layout::DeleteThenRead => {
                let mut single = self.clone();
                single.layout = Layout::Single;
                single.middle.clear();
                let render = Render {
                    aliases: 0,
                    has_parameter,
                    relationship: self.target.is_relationship(),
                };
                format!(
                    "{} RETURN {} AS r",
                    self.target.pattern(),
                    render.output(&self.union_outputs.0)
                )
            }
            _ => self.render(has_parameter, false),
        }
    }
}

fn build(graph: &Graph) -> Database {
    let db = db();
    for (index, label) in graph.nodes.iter().enumerate() {
        run(&db, &format!("CREATE (:N{label} {{v: {index}}})"));
    }
    for (index, (from, to, rel_type)) in graph.relationships.iter().enumerate() {
        run(
            &db,
            &format!(
                "MATCH (a {{v: {from}}}), (b {{v: {to}}}) CREATE (a)-[:T{rel_type} {{w: {index}}}]->(b)"
            ),
        );
    }
    db
}

fn outcome(db: &Database, query: &str, parameters: &Parameters) -> Outcome {
    match db.execute(query, parameters) {
        Ok(result) => Ok(result.rows().to_vec()),
        Err(error) => Err(error.kind()),
    }
}

fn first_match(db: &Database, target: &Target) -> (Parameters, usize) {
    let rows = run(db, &format!("{} RETURN x", target.pattern()));
    let mut parameters = Parameters::new();
    if let Some(row) = rows.first() {
        parameters.insert("p".to_owned(), row[0].clone());
    }
    (parameters, rows.len())
}

fn graph_size(db: &Database) -> (Value, Value) {
    (
        single(db, "MATCH (n) RETURN count(*)"),
        single(db, "MATCH ()-[r]->() RETURN count(*)"),
    )
}

#[derive(Clone, Copy, Debug)]
enum Unsafe {
    Property,
    Labels,
    HasLabel,
    Properties,
    Keys,
}

#[derive(Clone, Copy, Debug)]
enum Position {
    ReturnItem,
    ReturnList,
    CaseBranch,
    CountArgument,
    CollectArgument,
    WithItem,
    OrderBy,
    WithWhere,
    UnwindList,
    ComprehensionBody,
}

fn unsafe_read() -> impl Strategy<Value = Unsafe> {
    prop_oneof![
        Just(Unsafe::Property),
        Just(Unsafe::Labels),
        Just(Unsafe::HasLabel),
        Just(Unsafe::Properties),
        Just(Unsafe::Keys),
    ]
}

fn position() -> impl Strategy<Value = Position> {
    prop_oneof![
        Just(Position::ReturnItem),
        Just(Position::ReturnList),
        Just(Position::CaseBranch),
        Just(Position::CountArgument),
        Just(Position::CollectArgument),
        Just(Position::WithItem),
        Just(Position::OrderBy),
        Just(Position::WithWhere),
        Just(Position::UnwindList),
        Just(Position::ComprehensionBody),
    ]
}

fn unsafe_expression(read: Unsafe, relationship: bool, variable: &str) -> String {
    match (read, relationship) {
        (Unsafe::Property, false) => format!("{variable}.v"),
        (Unsafe::Property, true) => format!("{variable}.w"),
        (Unsafe::Labels, false) => format!("labels({variable})"),
        (Unsafe::HasLabel, false) => format!("{variable}:N0"),
        (Unsafe::Properties, _) => format!("properties({variable})"),
        (Unsafe::Keys, _) => format!("keys({variable})"),
        (_, true) => format!("{variable}.w"),
    }
}

fn unsafe_query(target: &Target, read: Unsafe, position: Position) -> String {
    let relationship = target.is_relationship();
    let direct = unsafe_expression(read, relationship, "x");
    let prefix = format!("{} {}", target.pattern(), target.delete());
    match position {
        Position::ReturnItem => format!("{prefix} RETURN {direct} AS r"),
        Position::ReturnList => format!("{prefix} RETURN [1, {direct}] AS r"),
        Position::CaseBranch => {
            format!("{prefix} RETURN CASE WHEN true THEN {direct} ELSE 0 END AS r")
        }
        Position::CountArgument => format!("{prefix} RETURN count({direct}) AS r"),
        Position::CollectArgument => format!("{prefix} RETURN collect({direct}) AS r"),
        Position::WithItem => format!("{prefix} WITH {direct} AS v RETURN 1 AS r"),
        Position::OrderBy => format!("{prefix} WITH x ORDER BY {direct} RETURN 1 AS r"),
        Position::WithWhere => {
            format!("{prefix} WITH x WHERE {direct} IS NULL OR true RETURN 1 AS r")
        }
        Position::UnwindList => format!("{prefix} WITH x UNWIND [{direct}] AS v RETURN 1 AS r"),
        Position::ComprehensionBody => {
            let body = unsafe_expression(read, relationship, "e");
            format!("{prefix} RETURN [e IN [x] | {body}] AS r")
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn a_later_clause_reads_a_deleted_element_as_it_reads_a_live_one(case in case()) {
        let oracle_db = build(&case.graph);
        let real_db = build(&case.graph);
        let (parameters, matched) = first_match(&oracle_db, &case.target);
        let has_parameter = !parameters.is_empty();
        let expected = outcome(
            &oracle_db,
            &case.oracle_query(has_parameter),
            &parameters,
        );
        let query = case.render(has_parameter, true);
        let before = graph_size(&real_db);
        let actual = outcome(&real_db, &query, &parameters);
        prop_assert_eq!(&actual, &expected, "{}", query);
        if actual.is_ok() && matched > 0 {
            prop_assert_ne!(graph_size(&real_db), before, "{} deleted nothing", query);
        }
    }

    #[test]
    fn reading_the_properties_or_labels_of_a_deleted_element_fails_wherever_the_read_sits(
        graph in graph(),
        target in target(),
        read in unsafe_read(),
        position in position(),
    ) {
        let db = build(&graph);
        let (parameters, matched) = first_match(&db, &target);
        prop_assume!(matched > 0);
        let query = unsafe_query(&target, read, position);
        prop_assert_eq!(
            outcome(&db, &query, &parameters),
            Err(ErrorKind::DeletedEntityAccess),
            "{}",
            query
        );
    }
}
