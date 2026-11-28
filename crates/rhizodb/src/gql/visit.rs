use super::ast::Clause;
use super::ast::Expr;
use super::ast::PatternElement;
use super::ast::PatternPart;
use super::ast::Projection;
use super::ast::ProjectionItems;
use super::ast::Properties;
use super::ast::Query;
use super::ast::RemoveItem;
use super::ast::SetItem;
use super::ast::SingleQuery;
use super::ast::Subquery;
use super::ast::Test;
use super::ast::Yield;

/// Whether `found` holds for `expr` or for any expression inside it, at any depth: the bodies of
/// comprehensions, quantifiers, and `reduce`, the property maps of patterns, and every clause of
/// a subquery included.
pub(crate) fn any_expr(expr: &Expr, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    if found(expr) {
        return true;
    }
    match expr {
        Expr::Literal(_) | Expr::Parameter(_) | Expr::Variable(_) | Expr::CountStar => false,
        Expr::List(items) => items.iter().any(|item| any_expr(item, found)),
        Expr::Map(entries) => entries.iter().any(|(_, value)| any_expr(value, found)),
        Expr::Property { base, .. } => any_expr(base, found),
        Expr::Index { base, index } => any_expr(base, found) || any_expr(index, found),
        Expr::Slice { base, from, to } => {
            any_expr(base, found) || from.iter().chain(to).any(|bound| any_expr(bound, found))
        }
        Expr::HasLabels { operand, .. }
        | Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. } => any_expr(operand, found),
        Expr::Binary { lhs, rhs, .. } => any_expr(lhs, found) || any_expr(rhs, found),
        Expr::Comparison { first, rest } => {
            any_expr(first, found) || rest.iter().any(|(_, operand)| any_expr(operand, found))
        }
        Expr::Function(call) => call.args.iter().any(|arg| any_expr(arg, found)),
        Expr::Case(case) => {
            case.scrutinee
                .iter()
                .chain(&case.otherwise)
                .any(|operand| any_expr(operand, found))
                || case.branches.iter().any(|branch| {
                    branch
                        .when
                        .iter()
                        .filter_map(|when| when.expr())
                        .chain(std::iter::once(&branch.then))
                        .any(|operand| any_expr(operand, found))
                })
        }
        Expr::ListComprehension(comprehension) => std::iter::once(&comprehension.source)
            .chain(&comprehension.filter)
            .chain(&comprehension.projection)
            .any(|part| any_expr(part, found)),
        Expr::ListPredicate(predicate) => {
            any_expr(&predicate.source, found) || any_expr(&predicate.predicate, found)
        }
        Expr::Pattern(element) => any_in_pattern(element, found),
        Expr::PatternComprehension(comprehension) => {
            any_in_pattern(&comprehension.element, found)
                || comprehension
                    .filter
                    .iter()
                    .chain(std::iter::once(&comprehension.projection))
                    .any(|part| any_expr(part, found))
        }
        Expr::Exists(subquery) | Expr::Count(subquery) => match subquery.as_ref() {
            Subquery::Pattern { patterns, filter } => {
                any_in_patterns(patterns, found)
                    || filter.iter().any(|filter| any_expr(filter, found))
            }
            Subquery::Query(query) => any_in_single(query, found),
        },
        Expr::Collect(query) | Expr::Value(query) => any_in_single(query, found),
        Expr::Is(test) => {
            any_expr(&test.operand, found)
                || match &test.test {
                    Test::DistinctFrom(other) => any_expr(other, found),
                    Test::Typed(_) | Test::Truth(_) | Test::Normalized(_) => false,
                }
        }
        Expr::Cast(cast) => any_expr(&cast.operand, found),
        Expr::Reduce(reduce) => [&reduce.init, &reduce.source, &reduce.body]
            .into_iter()
            .any(|part| any_expr(part, found)),
    }
}

fn any_in_query(query: &Query, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match query {
        Query::Single(single) => any_in_single(single, found),
        Query::Union(union) => std::iter::once(&union.first)
            .chain(&union.rest)
            .any(|single| any_in_single(single, found)),
        Query::Next(blocks) => blocks.iter().any(|block| any_in_query(&block.query, found)),
    }
}

fn any_in_single(query: &SingleQuery, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    query
        .clauses
        .iter()
        .any(|clause| any_in_clause(clause, found))
}

fn any_in_clause(clause: &Clause, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match clause {
        Clause::Match(clause) => {
            any_in_patterns(&clause.patterns, found)
                || clause.filter.iter().any(|filter| any_expr(filter, found))
        }
        Clause::Merge(merge) => {
            any_in_pattern(&merge.pattern.element, found)
                || merge
                    .on_create
                    .iter()
                    .chain(&merge.on_match)
                    .any(|item| any_in_set_item(item, found))
        }
        Clause::Insert(patterns) => any_in_patterns(patterns, found),
        Clause::With(with) => {
            any_in_projection(&with.projection, found)
                || with.filter.iter().any(|filter| any_expr(filter, found))
        }
        Clause::Return(projection) => any_in_projection(projection, found),
        Clause::Unwind(unwind) => any_expr(&unwind.expr, found),
        Clause::Set(items) => items.iter().any(|item| any_in_set_item(item, found)),
        Clause::Remove(items) => items.iter().any(|item| match item {
            RemoveItem::Property { base, .. } => any_expr(base, found),
            RemoveItem::Labels { .. } => false,
        }),
        Clause::Delete(delete) => delete.targets.iter().any(|target| any_expr(target, found)),
        Clause::Call(call) => {
            call.args.iter().flatten().any(|arg| any_expr(arg, found))
                || match &call.yields {
                    Some(Yield::Items {
                        filter: Some(filter),
                        ..
                    }) => any_expr(filter, found),
                    Some(Yield::Items { filter: None, .. } | Yield::Star) | None => false,
                }
        }
        Clause::Filter(condition) => any_expr(condition, found),
        Clause::Let(bindings) => bindings.iter().any(|(_, value)| any_expr(value, found)),
        Clause::CallSubquery(call) => any_in_query(&call.body, found),
    }
}

fn any_in_projection(projection: &Projection, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    let (ProjectionItems::Star(items) | ProjectionItems::Items(items)) = &projection.items;
    items
        .iter()
        .map(|item| &item.expr)
        .chain(projection.order_by.iter().map(|item| &item.expr))
        .chain(&projection.skip)
        .chain(&projection.limit)
        .any(|expr| any_expr(expr, found))
}

fn any_in_set_item(item: &SetItem, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match item {
        SetItem::Property { base, value, .. } => any_expr(base, found) || any_expr(value, found),
        SetItem::ReplaceProperties { value, .. } | SetItem::MergeProperties { value, .. } => {
            any_expr(value, found)
        }
        SetItem::Labels { .. } => false,
    }
}

fn any_in_patterns(patterns: &[PatternPart], found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    patterns
        .iter()
        .any(|part| any_in_pattern(&part.element, found))
}

fn any_in_pattern(element: &PatternElement, found: &mut dyn FnMut(&Expr) -> bool) -> bool {
    std::iter::once(&element.start.properties)
        .chain(
            element
                .steps
                .iter()
                .flat_map(|step| [&step.relationship.properties, &step.node.properties]),
        )
        .any(|properties| match properties {
            Some(Properties::Map(entries)) => {
                entries.iter().any(|(_, value)| any_expr(value, found))
            }
            Some(Properties::Parameter(_)) | None => false,
        })
}

#[cfg(test)]
#[path = "visit.test.rs"]
mod tests;
