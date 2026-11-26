mod support;

use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::strategy::Union;
use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const TYPE_ERRORS: &[ErrorKind] = &[
    ErrorKind::InvalidArgumentType,
    ErrorKind::InvalidArgumentValue,
    ErrorKind::NonIntegerArgument,
    ErrorKind::PropertyAccessOnNonEntity,
    ErrorKind::ListElementAccessByNonInteger,
    ErrorKind::MapElementAccessByNonString,
    ErrorKind::TypeMismatch,
];

const INTEGER_FILLINGS: &[&str] = &["(-1)", "0", "1", "2"];
const TEXT_FILLINGS: &[&str] = &["''", "'x'"];

const FUNCTIONS: &[(&str, usize)] = &[
    ("size", 1),
    ("head", 1),
    ("last", 1),
    ("tail", 1),
    ("reverse", 1),
    ("keys", 1),
    ("properties", 1),
    ("labels", 1),
    ("abs", 1),
    ("sign", 1),
    ("sqrt", 1),
    ("toString", 1),
    ("toInteger", 1),
    ("toFloat", 1),
    ("toBoolean", 1),
    ("toUpper", 1),
    ("trim", 1),
    ("range", 2),
    ("substring", 2),
    ("left", 2),
    ("split", 2),
    ("atan2", 2),
    ("replace", 3),
];

const AGGREGATES: &[&str] = &["count", "sum", "avg", "min", "max", "collect", "stDev"];

const BINARY_OPERATORS: &[&str] = &[
    "+",
    "-",
    "*",
    "/",
    "%",
    "^",
    "AND",
    "OR",
    "XOR",
    "IN",
    "STARTS WITH",
    "ENDS WITH",
    "CONTAINS",
    "=~",
    "=",
    "<>",
    "<",
    "<=",
    ">",
    ">=",
];

const COMPARISONS: &[&str] = &["=", "<>", "<", "<=", ">", ">="];

fn parameters() -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert("p0".to_owned(), Value::Integer(1));
    parameters.insert("p1".to_owned(), Value::String("x".to_owned()));
    parameters.insert("p2".to_owned(), Value::Null);
    parameters.insert(
        "p3".to_owned(),
        Value::List(vec![Value::Integer(1), Value::String("a".to_owned())]),
    );
    parameters.insert(
        "p4".to_owned(),
        Value::Map(BTreeMap::from([("a".to_owned(), Value::Integer(1))])),
    );
    parameters.insert("p5".to_owned(), Value::Float(1.5));
    parameters
}

fn graph() -> Database {
    let db = db();
    db.execute("CREATE (n:Present)-[:R]->(n)", &Parameters::new())
        .unwrap();
    db
}

#[derive(Clone, Debug)]
enum Expr {
    Integer(i64),
    Float(&'static str),
    Text(&'static str),
    Boolean(bool),
    Null,
    IntegerHole,
    TextHole,
    Parameter(usize),
    Node,
    Variable(usize),
    List(Vec<Expr>),
    Map(Vec<(&'static str, Expr)>),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    Property(Box<Expr>, &'static str),
    HasLabel(Box<Expr>, &'static str),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Chain(Box<Expr>, Vec<(&'static str, Expr)>),
    IsNull(Box<Expr>, bool),
    Call(&'static str, Vec<Expr>),
    Aggregate(&'static str, bool, Box<Expr>),
    CountStar,
    Case(Option<Box<Expr>>, Vec<(Expr, Expr)>, Option<Box<Expr>>),
    Comprehension(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    Quantifier(&'static str, Box<Expr>, Box<Expr>),
    PatternPredicate(Option<Box<Expr>>),
    PatternComprehension(Option<Box<Expr>>, Option<Box<Expr>>, Box<Expr>),
    Exists(Option<Box<Expr>>, Option<Box<Expr>>),
}

#[derive(Clone, Debug)]
struct Case {
    expr: Expr,
    filter: bool,
}

struct Render<'a> {
    fillings: Option<&'a [usize]>,
    holes: usize,
}

impl Render<'_> {
    fn hole(&mut self, failing: &str, fillings: &[&str]) -> String {
        let hole = self.holes;
        self.holes += 1;
        match self.fillings {
            None => failing.to_owned(),
            Some(choices) => fillings[choices[hole % choices.len()] % fillings.len()].to_owned(),
        }
    }

    fn optional(&mut self, expr: &Option<Box<Expr>>, depth: usize) -> Option<String> {
        expr.as_ref().map(|expr| self.expr(expr, depth))
    }

    fn element(&mut self, property: &Option<Box<Expr>>, depth: usize, variable: bool) -> String {
        let name = if variable {
            format!("x{depth}")
        } else {
            String::new()
        };
        match self.optional(property, depth) {
            Some(property) => format!("(n)-[]->({name} {{a: {property}}})"),
            None => format!("(n)-[]->({name})"),
        }
    }

    fn expr(&mut self, expr: &Expr, depth: usize) -> String {
        match expr {
            Expr::Integer(value) => format!("({value})"),
            Expr::Float(text) => format!("({text})"),
            Expr::Text(text) => format!("'{text}'"),
            Expr::Boolean(value) => value.to_string(),
            Expr::Null => "null".to_owned(),
            Expr::IntegerHole => self.hole("(1 / 0)", INTEGER_FILLINGS),
            Expr::TextHole => self.hole("substring('ab', -1)", TEXT_FILLINGS),
            Expr::Parameter(index) => format!("$p{index}"),
            Expr::Node => "n".to_owned(),
            Expr::Variable(index) if depth == 0 => format!("({index})"),
            Expr::Variable(index) => format!("x{}", index % depth),
            Expr::List(items) => {
                let items: Vec<String> = items.iter().map(|item| self.expr(item, depth)).collect();
                format!("[{}]", items.join(", "))
            }
            Expr::Map(entries) => {
                let entries: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| format!("{key}: {}", self.expr(value, depth)))
                    .collect();
                format!("{{{}}}", entries.join(", "))
            }
            Expr::Index(base, index) => {
                format!("({})[{}]", self.expr(base, depth), self.expr(index, depth))
            }
            Expr::Slice(base, from, to) => {
                let base = self.expr(base, depth);
                let from = self.optional(from, depth).unwrap_or_default();
                let to = self.optional(to, depth).unwrap_or_default();
                format!("({base})[{from}..{to}]")
            }
            Expr::Property(base, key) => format!("({}).{key}", self.expr(base, depth)),
            Expr::HasLabel(base, label) => format!("(({}):{label})", self.expr(base, depth)),
            Expr::Unary(op, operand) => format!("{op}({})", self.expr(operand, depth)),
            Expr::Binary(op, lhs, rhs) => {
                format!(
                    "({}) {op} ({})",
                    self.expr(lhs, depth),
                    self.expr(rhs, depth)
                )
            }
            Expr::Chain(first, rest) => {
                let mut text = format!("({})", self.expr(first, depth));
                for (op, operand) in rest {
                    text.push_str(&format!(" {op} ({})", self.expr(operand, depth)));
                }
                text
            }
            Expr::IsNull(operand, negated) => {
                let not = if *negated { " NOT" } else { "" };
                format!("({}) IS{not} NULL", self.expr(operand, depth))
            }
            Expr::Call(function, args) => {
                let args: Vec<String> = args.iter().map(|arg| self.expr(arg, depth)).collect();
                format!("{function}({})", args.join(", "))
            }
            Expr::Aggregate(function, distinct, arg) => {
                let distinct = if *distinct { "DISTINCT " } else { "" };
                format!("{function}({distinct}{})", self.expr(arg, depth))
            }
            Expr::CountStar => "count(*)".to_owned(),
            Expr::Case(scrutinee, branches, otherwise) => {
                let mut text = "(CASE".to_owned();
                if let Some(scrutinee) = self.optional(scrutinee, depth) {
                    text.push_str(&format!(" {scrutinee}"));
                }
                for (when, then) in branches {
                    let when = self.expr(when, depth);
                    let then = self.expr(then, depth);
                    text.push_str(&format!(" WHEN {when} THEN {then}"));
                }
                if let Some(otherwise) = self.optional(otherwise, depth) {
                    text.push_str(&format!(" ELSE {otherwise}"));
                }
                text.push_str(" END)");
                text
            }
            Expr::Comprehension(source, filter, projection) => {
                let source = self.expr(source, depth);
                let mut text = format!("[x{depth} IN {source}");
                if let Some(filter) = self.optional(filter, depth + 1) {
                    text.push_str(&format!(" WHERE {filter}"));
                }
                if let Some(projection) = self.optional(projection, depth + 1) {
                    text.push_str(&format!(" | {projection}"));
                }
                text.push(']');
                text
            }
            Expr::Quantifier(quantifier, source, predicate) => {
                let source = self.expr(source, depth);
                let predicate = self.expr(predicate, depth + 1);
                format!("{quantifier}(x{depth} IN {source} WHERE {predicate})")
            }
            Expr::PatternPredicate(property) => {
                format!("({})", self.element(property, depth, false))
            }
            Expr::PatternComprehension(property, filter, projection) => {
                let element = self.element(property, depth, true);
                let mut text = format!("[{element}");
                if let Some(filter) = self.optional(filter, depth + 1) {
                    text.push_str(&format!(" WHERE {filter}"));
                }
                text.push_str(&format!(" | {}]", self.expr(projection, depth + 1)));
                text
            }
            Expr::Exists(property, filter) => {
                let element = self.element(property, depth, true);
                let mut text = format!("EXISTS {{ {element}");
                if let Some(filter) = self.optional(filter, depth + 1) {
                    text.push_str(&format!(" WHERE {filter}"));
                }
                text.push_str(" }");
                text
            }
        }
    }
}

fn query(case: &Case, label: &str, fillings: Option<&[usize]>) -> String {
    let expr = Render { fillings, holes: 0 }.expr(&case.expr, 0);
    if case.filter {
        format!("MATCH (n:{label}) WHERE ({expr}) IS NULL RETURN 1 AS v")
    } else {
        format!("MATCH (n:{label}) RETURN {expr} AS v")
    }
}

fn leaf(data: bool) -> BoxedStrategy<Expr> {
    let mut forms = vec![
        (1, (-3i64..=3).prop_map(Expr::Integer).boxed()),
        (
            1,
            prop::sample::select(vec!["0.0", "1.5"])
                .prop_map(Expr::Float)
                .boxed(),
        ),
        (
            2,
            prop::sample::select(vec!["", "a"])
                .prop_map(Expr::Text)
                .boxed(),
        ),
        (1, any::<bool>().prop_map(Expr::Boolean).boxed()),
        (1, Just(Expr::Null).boxed()),
        (2, Just(Expr::IntegerHole).boxed()),
        (1, Just(Expr::TextHole).boxed()),
        (4, (0usize..2).prop_map(Expr::Variable).boxed()),
    ];
    if data {
        forms.push((2, (0usize..6).prop_map(Expr::Parameter).boxed()));
        forms.push((1, Just(Expr::Node).boxed()));
    }
    Union::new_weighted(forms).boxed()
}

fn expr(data: bool, patterns: bool) -> BoxedStrategy<Expr> {
    leaf(data)
        .prop_recursive(5, 64, 4, move |inner| {
            let boxed = inner.clone().prop_map(Box::new);
            let optional = prop::option::of(boxed.clone());
            let list = prop::collection::vec(inner.clone(), 0..4).prop_map(Expr::List);
            let source = prop_oneof![3 => list.clone(), 1 => inner.clone()].prop_map(Box::new);
            let condition = prop_oneof![
            3 => boxed.clone().prop_map(|operand| Expr::IsNull(operand, false)),
            1 => (boxed.clone(), boxed.clone()).prop_map(|(lhs, rhs)| Expr::Binary("=", lhs, rhs)),
            1 => inner.clone(),
        ]
        .prop_map(Box::new);
            let key = prop_oneof![
                2 => (-2i64..=2).prop_map(Expr::Integer),
                2 => prop::sample::select(vec!["a", "b"]).prop_map(Expr::Text),
                1 => inner.clone(),
            ]
            .prop_map(Box::new);
            let function = prop::sample::select(FUNCTIONS).prop_flat_map({
                let inner = inner.clone();
                move |(name, arity)| {
                    prop::collection::vec(inner.clone(), arity..=arity)
                        .prop_map(move |args| Expr::Call(name, args))
                }
            });
            let map =
                prop::collection::vec((prop::sample::select(vec!["a", "b"]), inner.clone()), 0..4);
            let branches = prop::collection::vec((inner.clone(), inner.clone()), 1..3);
            let comparisons =
                prop::collection::vec((prop::sample::select(COMPARISONS), inner.clone()), 1..3);
            let mut forms = vec![
                (2, list.boxed()),
                (2, map.clone().prop_map(Expr::Map).boxed()),
                (
                    2,
                    (map.clone(), prop::sample::select(vec!["a", "b"]))
                        .prop_map(|(entries, key)| {
                            Expr::Property(Box::new(Expr::Map(entries)), key)
                        })
                        .boxed(),
                ),
                (
                    2,
                    (map, prop::sample::select(vec!["a", "b"]))
                        .prop_map(|(entries, key)| {
                            Expr::Index(Box::new(Expr::Map(entries)), Box::new(Expr::Text(key)))
                        })
                        .boxed(),
                ),
                (
                    2,
                    (boxed.clone(), key)
                        .prop_map(|(base, index)| Expr::Index(base, index))
                        .boxed(),
                ),
                (
                    1,
                    (boxed.clone(), optional.clone(), optional.clone())
                        .prop_map(|(base, from, to)| Expr::Slice(base, from, to))
                        .boxed(),
                ),
                (
                    1,
                    (boxed.clone(), prop::sample::select(vec!["a", "b"]))
                        .prop_map(|(base, key)| Expr::Property(base, key))
                        .boxed(),
                ),
                (
                    2,
                    (
                        boxed.clone(),
                        prop::sample::select(vec!["Present", "Other"]),
                    )
                        .prop_map(|(base, label)| Expr::HasLabel(base, label))
                        .boxed(),
                ),
                (
                    1,
                    (prop::sample::select(vec!["-", "+", "NOT "]), boxed.clone())
                        .prop_map(|(op, operand)| Expr::Unary(op, operand))
                        .boxed(),
                ),
                (
                    3,
                    (
                        prop::sample::select(BINARY_OPERATORS),
                        boxed.clone(),
                        boxed.clone(),
                    )
                        .prop_map(|(op, lhs, rhs)| Expr::Binary(op, lhs, rhs))
                        .boxed(),
                ),
                (
                    1,
                    (boxed.clone(), comparisons)
                        .prop_map(|(first, rest)| Expr::Chain(first, rest))
                        .boxed(),
                ),
                (
                    1,
                    (boxed.clone(), any::<bool>())
                        .prop_map(|(operand, negated)| Expr::IsNull(operand, negated))
                        .boxed(),
                ),
                (3, function.boxed()),
                (
                    1,
                    prop::collection::vec(inner.clone(), 1..4)
                        .prop_map(|args| Expr::Call("coalesce", args))
                        .boxed(),
                ),
                (
                    3,
                    (optional.clone(), branches, optional.clone())
                        .prop_map(|(scrutinee, branches, otherwise)| {
                            Expr::Case(scrutinee, branches, otherwise)
                        })
                        .boxed(),
                ),
                (
                    3,
                    (
                        source.clone(),
                        prop::option::of(condition.clone()),
                        optional.clone(),
                    )
                        .prop_map(|(source, filter, projection)| {
                            Expr::Comprehension(source, filter, projection)
                        })
                        .boxed(),
                ),
                (
                    2,
                    (
                        prop::sample::select(vec!["all", "any", "none", "single"]),
                        source,
                        condition.clone(),
                    )
                        .prop_map(|(quantifier, source, predicate)| {
                            Expr::Quantifier(quantifier, source, predicate)
                        })
                        .boxed(),
                ),
            ];
            if data {
                forms.push((
                    2,
                    (
                        optional.clone(),
                        prop::option::of(condition.clone()),
                        boxed.clone(),
                    )
                        .prop_map(|(property, filter, projection)| {
                            if property.is_some() {
                                Expr::PatternComprehension(
                                    property,
                                    None,
                                    Box::new(Expr::Integer(0)),
                                )
                            } else {
                                Expr::PatternComprehension(property, filter, projection)
                            }
                        })
                        .boxed(),
                ));
                forms.push((
                    2,
                    (optional.clone(), prop::option::of(condition))
                        .prop_map(|(property, filter)| {
                            let filter = if property.is_some() { None } else { filter };
                            Expr::Exists(property, filter)
                        })
                        .boxed(),
                ));
            }
            if patterns {
                forms.push((2, optional.prop_map(Expr::PatternPredicate).boxed()));
            }
            Union::new_weighted(forms)
        })
        .boxed()
}

fn aggregated(data: bool) -> impl Strategy<Value = Expr> {
    let aggregate = prop_oneof![
        1 => Just(Expr::CountStar),
        5 => (prop::sample::select(AGGREGATES), any::<bool>(), expr(data, false).prop_map(Box::new))
            .prop_map(|(function, distinct, arg)| Expr::Aggregate(function, distinct, arg)),
    ];
    prop_oneof![
        3 => aggregate.clone(),
        1 => (aggregate, (-3i64..=3).prop_map(Expr::Integer))
            .prop_map(|(aggregate, other)| Expr::List(vec![aggregate, other])),
    ]
}

fn closed(data: bool) -> BoxedStrategy<Case> {
    let mut forms = vec![
        (
            4,
            expr(data, false)
                .prop_map(|expr| Case {
                    expr,
                    filter: false,
                })
                .boxed(),
        ),
        (
            2,
            aggregated(data)
                .prop_map(|expr| Case {
                    expr,
                    filter: false,
                })
                .boxed(),
        ),
        (
            2,
            (prop::collection::vec(leaf(data), 1..4), expr(data, false))
                .prop_map(|(items, body)| Case {
                    expr: Expr::Comprehension(
                        Box::new(Expr::List(items)),
                        None,
                        Some(Box::new(body)),
                    ),
                    filter: false,
                })
                .boxed(),
        ),
    ];
    if data {
        forms.push((
            2,
            expr(true, true)
                .prop_map(|expr| Case { expr, filter: true })
                .boxed(),
        ));
    }
    Union::new_weighted(forms).boxed()
}

fn error(db: &Database, query: &str) -> Option<ErrorKind> {
    db.execute(query, &parameters())
        .err()
        .map(|error| error.kind())
}

fn malformed(kind: Option<ErrorKind>) -> bool {
    matches!(
        kind,
        Some(
            ErrorKind::Syntax(_)
                | ErrorKind::UndefinedVariable
                | ErrorKind::VariableTypeConflict
                | ErrorKind::InvalidClauseComposition
                | ErrorKind::InvalidAggregation
                | ErrorKind::NestedAggregation
                | ErrorKind::AmbiguousAggregationExpression
        )
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_type_error_the_text_check_reports_holds_whatever_values_the_value_errors_stand_for(
        case in closed(true),
        fillings in prop::collection::vec(0usize..4, 1..8),
    ) {
        let db = graph();
        let filled = query(&case, "Present", Some(&fillings));
        let evaluated = error(&db, &filled);
        prop_assert!(!malformed(evaluated), "{filled} is not a well-formed query: {evaluated:?}");
        let failing = query(&case, "Missing", None);
        let early = error(&db, &failing);
        prop_assert!(!malformed(early), "{failing} is not a well-formed query: {early:?}");
        if let Some(kind) = early {
            prop_assert!(TYPE_ERRORS.contains(&kind), "{failing} failed with {kind:?}");
            prop_assert!(
                evaluated.is_some_and(|evaluated| TYPE_ERRORS.contains(&evaluated)),
                "{failing} failed early with {kind:?}, but {filled} gave {evaluated:?}"
            );
        }
    }

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn a_type_error_a_constant_expression_raises_is_one_the_text_check_reports(
        case in closed(false),
        filling in 0usize..4,
    ) {
        let db = graph();
        let constant = query(&case, "Present", Some(&[filling]));
        let evaluated = error(&db, &constant);
        prop_assert!(!malformed(evaluated), "{constant} is not a well-formed query: {evaluated:?}");
        if let Some(kind) = evaluated.filter(|kind| TYPE_ERRORS.contains(kind)) {
            let text = query(&case, "Missing", Some(&[filling]));
            prop_assert_eq!(
                error(&db, &text),
                Some(kind),
                "{} raised {:?} but the text check disagrees", constant, kind
            );
        }
    }
}
