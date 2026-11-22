mod support;

use proptest::prelude::*;
use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::Parameters;
use support::*;

const TYPE_ERRORS: &[ErrorKind] = &[
    ErrorKind::InvalidArgumentType,
    ErrorKind::InvalidArgumentValue,
    ErrorKind::NonIntegerArgument,
    ErrorKind::PropertyAccessOnNonEntity,
    ErrorKind::ListElementAccessByNonInteger,
    ErrorKind::MapElementAccessByNonString,
];

const INTEGER_FILLINGS: &[&str] = &["(-1)", "0", "1", "2"];
const TEXT_FILLINGS: &[&str] = &["''", "'x'"];

#[derive(Clone, Debug)]
enum Expr {
    Integer(i64),
    Text(&'static str),
    Boolean(bool),
    Null,
    IntegerHole,
    TextHole,
    Variable(usize),
    List(Vec<Expr>),
    Map(Option<Box<Expr>>, Option<Box<Expr>>),
    Index(Box<Expr>, Box<Expr>),
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    Property(Box<Expr>, &'static str),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>),
    Call(&'static str, Vec<Expr>),
    Case(Box<Expr>, Box<Expr>, Box<Expr>),
    Comprehension(Box<Expr>, Option<Box<Expr>>, Box<Expr>),
    Any(Box<Expr>, Box<Expr>),
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

    fn expr(&mut self, expr: &Expr, depth: usize) -> String {
        match expr {
            Expr::Integer(value) => format!("({value})"),
            Expr::Text(text) => format!("'{text}'"),
            Expr::Boolean(value) => value.to_string(),
            Expr::Null => "null".to_owned(),
            Expr::IntegerHole => self.hole("(1 / 0)", INTEGER_FILLINGS),
            Expr::TextHole => self.hole("substring('ab', -1)", TEXT_FILLINGS),
            Expr::Variable(index) if depth == 0 => format!("({index})"),
            Expr::Variable(index) => format!("x{}", index % depth),
            Expr::List(items) => {
                let items: Vec<String> = items.iter().map(|item| self.expr(item, depth)).collect();
                format!("[{}]", items.join(", "))
            }
            Expr::Map(a, b) => {
                let mut entries = Vec::new();
                if let Some(a) = a {
                    entries.push(format!("a: {}", self.expr(a, depth)));
                }
                if let Some(b) = b {
                    entries.push(format!("b: {}", self.expr(b, depth)));
                }
                format!("{{{}}}", entries.join(", "))
            }
            Expr::Index(base, index) => {
                format!("({})[{}]", self.expr(base, depth), self.expr(index, depth))
            }
            Expr::Slice(base, from, to) => {
                let base = self.expr(base, depth);
                let from = from
                    .as_ref()
                    .map_or(String::new(), |from| self.expr(from, depth));
                let to = to.as_ref().map_or(String::new(), |to| self.expr(to, depth));
                format!("({base})[{from}..{to}]")
            }
            Expr::Property(base, key) => format!("({}).{key}", self.expr(base, depth)),
            Expr::Binary(op, lhs, rhs) => {
                format!(
                    "({}) {op} ({})",
                    self.expr(lhs, depth),
                    self.expr(rhs, depth)
                )
            }
            Expr::Not(operand) => format!("NOT ({})", self.expr(operand, depth)),
            Expr::IsNull(operand) => format!("({}) IS NULL", self.expr(operand, depth)),
            Expr::Call(function, args) => {
                let args: Vec<String> = args.iter().map(|arg| self.expr(arg, depth)).collect();
                format!("{function}({})", args.join(", "))
            }
            Expr::Case(condition, then, otherwise) => format!(
                "(CASE WHEN {} THEN {} ELSE {} END)",
                self.expr(condition, depth),
                self.expr(then, depth),
                self.expr(otherwise, depth)
            ),
            Expr::Comprehension(source, filter, projection) => {
                let source = self.expr(source, depth);
                let filter = filter.as_ref().map_or(String::new(), |filter| {
                    format!(" WHERE {}", self.expr(filter, depth + 1))
                });
                let projection = self.expr(projection, depth + 1);
                format!("[x{depth} IN {source}{filter} | {projection}]")
            }
            Expr::Any(source, predicate) => {
                let source = self.expr(source, depth);
                let predicate = self.expr(predicate, depth + 1);
                format!("any(x{depth} IN {source} WHERE {predicate})")
            }
        }
    }
}

fn render(expr: &Expr, fillings: Option<&[usize]>) -> String {
    Render { fillings, holes: 0 }.expr(expr, 0)
}

fn leaf() -> impl Strategy<Value = Expr> {
    prop_oneof![
        1 => (-3i64..=3).prop_map(Expr::Integer),
        2 => prop::sample::select(vec!["", "a"]).prop_map(Expr::Text),
        1 => any::<bool>().prop_map(Expr::Boolean),
        1 => Just(Expr::Null),
        2 => Just(Expr::IntegerHole),
        1 => Just(Expr::TextHole),
        4 => (0usize..2).prop_map(Expr::Variable),
    ]
}

fn expr() -> impl Strategy<Value = Expr> {
    leaf().prop_recursive(5, 64, 4, |inner| {
        let boxed = inner.clone().prop_map(Box::new);
        let optional = prop::option::of(boxed.clone());
        let list = prop::collection::vec(inner.clone(), 0..4).prop_map(Expr::List);
        let source = prop_oneof![3 => list.clone(), 1 => inner.clone()].prop_map(Box::new);
        let condition = prop_oneof![
            3 => boxed.clone().prop_map(Expr::IsNull),
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
        prop_oneof![
            2 => list,
            1 => (optional.clone(), optional.clone()).prop_map(|(a, b)| Expr::Map(a, b)),
            2 => (boxed.clone(), key).prop_map(|(base, index)| Expr::Index(base, index)),
            1 => (boxed.clone(), optional.clone(), optional.clone())
                .prop_map(|(base, from, to)| Expr::Slice(base, from, to)),
            1 => (boxed.clone(), prop::sample::select(vec!["a", "b"]))
                .prop_map(|(base, key)| Expr::Property(base, key)),
            3 => (
                prop::sample::select(vec!["+", "-", "*", "AND", "OR", "=", "<", "IN"]),
                boxed.clone(),
                boxed.clone()
            )
                .prop_map(|(op, lhs, rhs)| Expr::Binary(op, lhs, rhs)),
            1 => boxed.clone().prop_map(Expr::Not),
            1 => boxed.clone().prop_map(Expr::IsNull),
            2 => (
                prop::sample::select(vec![
                    "size", "head", "last", "tail", "reverse", "keys", "abs"
                ]),
                inner.clone()
            )
                .prop_map(|(function, arg)| Expr::Call(function, vec![arg])),
            1 => prop::collection::vec(inner.clone(), 1..4)
                .prop_map(|args| Expr::Call("coalesce", args)),
            2 => (condition.clone(), boxed.clone(), boxed.clone())
                .prop_map(|(condition, then, otherwise)| Expr::Case(condition, then, otherwise)),
            3 => (source.clone(), prop::option::of(condition.clone()), boxed.clone()).prop_map(
                |(source, filter, projection)| Expr::Comprehension(source, filter, projection)
            ),
            1 => (source, condition).prop_map(|(source, predicate)| Expr::Any(source, predicate)),
        ]
    })
}

fn closed() -> impl Strategy<Value = Expr> {
    prop_oneof![
        expr(),
        (prop::collection::vec(leaf(), 1..4), expr()).prop_map(|(items, body)| {
            Expr::Comprehension(Box::new(Expr::List(items)), None, Box::new(body))
        }),
    ]
}

fn type_error(db: &Database, query: &str) -> Option<ErrorKind> {
    let kind = db.execute(query, &Parameters::new()).err()?.kind();
    assert!(TYPE_ERRORS.contains(&kind), "{query} failed with {kind:?}");
    Some(kind)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    #[test]
    fn a_type_error_the_text_check_reports_holds_whatever_values_the_value_errors_stand_for(
        expr in closed(),
        fillings in prop::collection::vec(0usize..4, 1..8),
    ) {
        let db = db();
        let filled = render(&expr, Some(&fillings));
        let evaluated = type_error(&db, &format!("RETURN {filled} AS v"));
        let failing = render(&expr, None);
        if let Some(kind) = db
            .execute(&format!("MATCH (n:Missing) RETURN {failing} AS v"), &Parameters::new())
            .err()
            .map(|error| error.kind())
        {
            prop_assert!(TYPE_ERRORS.contains(&kind), "{failing} failed with {kind:?}");
            prop_assert!(
                evaluated.is_some(),
                "{failing} failed early with {kind:?}, but {filled} evaluated without error"
            );
        }
    }
}
