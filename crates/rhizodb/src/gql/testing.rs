use super::ast::BinaryOp;
use super::ast::Clause;
use super::ast::ComparisonOp;
use super::ast::Expr;
use super::ast::LabelExpr;
use super::ast::Literal;
use super::ast::Name;
use super::ast::PatternPart;
use super::ast::ProjectionItems;
use super::ast::Query;
use super::ast::UnaryOp;
use super::error::SyntaxErrorKind;
use super::parser::parse;

pub(super) fn single_query_clauses(source: &str) -> Vec<Clause> {
    match parse(source) {
        Ok(Query::Single(query)) => query.clauses,
        other => panic!("expected a single query for {source:?}, got {other:?}"),
    }
}

pub(super) fn expr(source: &str) -> Expr {
    let query = format!("RETURN {source}");
    match single_query_clauses(&query).into_iter().next() {
        Some(Clause::Return(projection)) => match projection.items {
            ProjectionItems::Items(mut items) if items.len() == 1 => items.remove(0).expr,
            other => panic!("expected one item, got {other:?}"),
        },
        other => panic!("expected RETURN, got {other:?}"),
    }
}

pub(super) fn patterns(source: &str) -> Vec<PatternPart> {
    let query = format!("MATCH {source} RETURN 1");
    match single_query_clauses(&query).into_iter().next() {
        Some(Clause::Match(found)) => found.patterns,
        other => panic!("expected MATCH, got {other:?}"),
    }
}

pub(super) fn error(source: &str) -> (usize, SyntaxErrorKind) {
    match parse(source) {
        Err(error) => (error.offset, error.kind),
        Ok(query) => panic!("expected an error for {source:?}, got {query:?}"),
    }
}

pub(super) fn predicate(source: &str) -> Expr {
    let query = format!("MATCH (a) WHERE {source} RETURN a");
    match single_query_clauses(&query).into_iter().next() {
        Some(Clause::Match(found)) => match found.filter {
            Some(filter) => filter,
            None => panic!("expected a WHERE in {query:?}"),
        },
        other => panic!("expected MATCH, got {other:?}"),
    }
}

pub(super) fn expr_error(source: &str) -> (usize, SyntaxErrorKind) {
    let (offset, kind) = error(&format!("RETURN {source}"));
    (offset - "RETURN ".len(), kind)
}

pub(super) fn sexp(expr: &Expr) -> String {
    let all = |items: &[Expr]| items.iter().map(sexp).collect::<Vec<_>>().join(" ");
    let optional = |item: &Option<Box<Expr>>| item.as_deref().map_or("_".into(), sexp);
    match expr {
        Expr::Literal(Literal::Integer(value)) => value.to_string(),
        Expr::Literal(Literal::Float(value)) => format!("{value:?}"),
        Expr::Literal(Literal::String(value)) => format!("'{value}'"),
        Expr::Literal(Literal::Boolean(value)) => value.to_string(),
        Expr::Literal(Literal::Null) => "null".into(),
        Expr::Parameter(Name(name)) => format!("${name}"),
        Expr::Variable(Name(name)) => name.clone(),
        Expr::List(items) => format!("[{}]", all(items)),
        Expr::Map(entries) => {
            let entries: Vec<String> = entries
                .iter()
                .map(|(Name(key), value)| format!("{key}:{}", sexp(value)))
                .collect();
            format!("{{{}}}", entries.join(" "))
        }
        Expr::Property {
            base,
            key: Name(key),
        } => format!("(. {} {key})", sexp(base)),
        Expr::Index { base, index } => format!("(index {} {})", sexp(base), sexp(index)),
        Expr::Slice { base, from, to } => {
            format!("(slice {} {} {})", sexp(base), optional(from), optional(to))
        }
        Expr::HasLabels { operand, labels } => {
            format!("(labels {} {})", sexp(operand), label_sexp(labels))
        }
        Expr::Unary { op, operand } => {
            let op = match op {
                UnaryOp::Negate => "neg",
                UnaryOp::Plus => "pos",
                UnaryOp::Not => "not",
            };
            format!("({op} {})", sexp(operand))
        }
        Expr::Binary { op, lhs, rhs } => {
            format!("({} {} {})", binary_symbol(*op), sexp(lhs), sexp(rhs))
        }
        Expr::Comparison { first, rest } => {
            let rest: Vec<String> = rest
                .iter()
                .map(|(op, rhs)| format!("{} {}", comparison_symbol(*op), sexp(rhs)))
                .collect();
            format!("(cmp {} {})", sexp(first), rest.join(" "))
        }
        Expr::IsNull { operand, negated } => {
            let name = if *negated { "is-not-null" } else { "is-null" };
            format!("({name} {})", sexp(operand))
        }
        Expr::Function(call) => {
            let mut name: Vec<&str> = call.namespace.iter().map(|n| n.0.as_str()).collect();
            name.push(&call.name.0);
            let distinct = if call.distinct { " distinct" } else { "" };
            format!("(call {}{distinct} {})", name.join("."), all(&call.args))
        }
        Expr::CountStar => "count(*)".into(),
        Expr::Case(case) => {
            let scrutinee = case.scrutinee.as_deref().map_or(String::new(), sexp);
            let branches: Vec<String> = case
                .branches
                .iter()
                .map(|b| {
                    let when: Vec<String> = b
                        .when
                        .iter()
                        .map(|operand| match operand {
                            super::ast::WhenOperand::Value(expr) => sexp(expr),
                            super::ast::WhenOperand::Compare(op, expr) => {
                                format!("({op:?} {})", sexp(expr))
                            }
                            super::ast::WhenOperand::IsNull { negated } => {
                                format!("(is-null {negated})")
                            }
                        })
                        .collect();
                    format!("(when {} {})", when.join(" "), sexp(&b.then))
                })
                .collect();
            let otherwise = case.otherwise.as_deref().map_or(String::new(), sexp);
            format!(
                "(case [{scrutinee}] {} else [{otherwise}])",
                branches.join(" ")
            )
        }
        other => format!("{other:?}"),
    }
}

pub(super) fn label_sexp(labels: &LabelExpr) -> String {
    let all = |operands: &[LabelExpr]| {
        operands
            .iter()
            .map(label_sexp)
            .collect::<Vec<_>>()
            .join(" ")
    };
    match labels {
        LabelExpr::Name(Name(name)) => name.clone(),
        LabelExpr::Wildcard => "%".into(),
        LabelExpr::Not(operand) => format!("(! {})", label_sexp(operand)),
        LabelExpr::And(operands) => format!("(& {})", all(operands)),
        LabelExpr::Or(operands) => format!("(| {})", all(operands)),
    }
}

fn binary_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Concat => "||",
        BinaryOp::Subtract => "-",
        BinaryOp::Multiply => "*",
        BinaryOp::Divide => "/",
        BinaryOp::Modulo => "%",
        BinaryOp::Power => "^",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Xor => "xor",
        BinaryOp::In => "in",
        BinaryOp::StartsWith => "starts",
        BinaryOp::EndsWith => "ends",
        BinaryOp::Contains => "contains",
        BinaryOp::RegexMatch => "=~",
    }
}

fn comparison_symbol(op: ComparisonOp) -> &'static str {
    match op {
        ComparisonOp::Equal => "=",
        ComparisonOp::NotEqual => "<>",
        ComparisonOp::Less => "<",
        ComparisonOp::LessEqual => "<=",
        ComparisonOp::Greater => ">",
        ComparisonOp::GreaterEqual => ">=",
    }
}
