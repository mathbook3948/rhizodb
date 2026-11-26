use crate::gql::ast::BinaryOp;

use super::datum::Datum;
use super::functions::Function;
use super::plan::Expr;
use super::plan::PatternPart;
use super::plan::Slot;

/// The conjuncts of `filter`, each with the position in `part` after which all it reads is bound:
/// 0 for the start node, then one per relationship and node in pattern order. There are none
/// unless every conjunct reads only elements `part` binds itself and can give no error, so
/// testing one early never hides an error another would raise. A conjunct that only the last
/// element completes is left to the filter.
pub(crate) fn checks(part: &PatternPart, filter: Option<&Expr>) -> Vec<(usize, Expr)> {
    let Some(filter) = filter else {
        return Vec::new();
    };
    let mut own: Vec<(Slot, usize)> = Vec::new();
    if !part.start.bound {
        own.push((part.start.slot, 0));
    }
    for (index, step) in part.steps.iter().enumerate() {
        let relationship = &step.relationship;
        if !relationship.bound && relationship.length.is_none() {
            own.push((relationship.slot, 2 * index + 1));
        }
        if !step.node.bound {
            own.push((step.node.slot, 2 * index + 2));
        }
    }
    let last = 2 * part.steps.len();
    let mut conjuncts = Vec::new();
    collect(filter, &mut conjuncts);
    let mut found = Vec::new();
    for conjunct in conjuncts {
        let mut position = None;
        if !predicate(conjunct, &own, &mut position) {
            return Vec::new();
        }
        if let Some(position) = position.filter(|position| *position < last) {
            found.push((position, conjunct.clone()));
        }
    }
    found.sort_by_key(|(position, _)| *position);
    found
}

fn collect<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match expr {
        Expr::Binary(BinaryOp::And, left, right) => {
            collect(left, out);
            collect(right, out);
        }
        other => out.push(other),
    }
}

fn predicate(expr: &Expr, own: &[(Slot, usize)], position: &mut Option<usize>) -> bool {
    match expr {
        Expr::Constant(Datum::Boolean(_)) => true,
        Expr::Not(inner) => predicate(inner, own, position),
        Expr::IsNull { operand, .. } => value(operand, own, position),
        Expr::Binary(BinaryOp::And | BinaryOp::Or | BinaryOp::Xor, left, right) => {
            predicate(left, own, position) && predicate(right, own, position)
        }
        Expr::Binary(
            BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains,
            left,
            right,
        ) => value(left, own, position) && value(right, own, position),
        Expr::Binary(BinaryOp::In, needle, list) => {
            value(needle, own, position)
                && match list.as_ref() {
                    Expr::List(items) => items.iter().all(|item| value(item, own, position)),
                    Expr::Constant(Datum::List(_)) => true,
                    _ => false,
                }
        }
        Expr::Comparison(first, rest) => {
            value(first, own, position) && rest.iter().all(|(_, item)| value(item, own, position))
        }
        _ => false,
    }
}

fn value(expr: &Expr, own: &[(Slot, usize)], position: &mut Option<usize>) -> bool {
    match expr {
        Expr::Constant(_) | Expr::Parameter(_) => true,
        Expr::Slot(slot) => reads(*slot, own, position),
        Expr::Property(base, _) => {
            matches!(base.as_ref(), Expr::Slot(slot) if reads(*slot, own, position))
        }
        Expr::Call(Function::Id, args) => {
            matches!(args.as_slice(), [Expr::Slot(slot)] if reads(*slot, own, position))
        }
        _ => false,
    }
}

fn reads(slot: Slot, own: &[(Slot, usize)], position: &mut Option<usize>) -> bool {
    let Some((_, at)) = own.iter().find(|(held, _)| *held == slot) else {
        return false;
    };
    *position = Some(position.map_or(*at, |seen| seen.max(*at)));
    true
}
