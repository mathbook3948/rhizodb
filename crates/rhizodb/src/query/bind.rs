use std::collections::BTreeMap;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast;
use crate::gql::ast::Direction;
use crate::result::Parameters;

use super::datum::Datum;
use super::datum::from_parameter;
use super::functions::Function;
use super::plan::Aggregate;
use super::plan::AggregateFunction;
use super::plan::Case;
use super::plan::Clause;
use super::plan::Expr;
use super::plan::Hops;
use super::plan::Items;
use super::plan::ListComprehension;
use super::plan::MatchClause;
use super::plan::MergeClause;
use super::plan::NodePattern;
use super::plan::PatternPart;
use super::plan::PatternQuery;
use super::plan::Pipeline;
use super::plan::Plan;
use super::plan::Program;
use super::plan::Projection;
use super::plan::Quantified;
use super::plan::ReadClause;
use super::plan::RelationshipPattern;
use super::plan::RemoveAction;
use super::plan::SetAction;
use super::plan::Slot;
use super::plan::Step;
use super::plan::UpdateClause;
use super::shape::NUMBER;
use super::shape::Shape;
use super::shape::admits;

/// Resolves every variable of `query` to a row slot and rejects queries that are malformed by
/// scope or by clause order. Parameters are folded in as constants.
pub(crate) fn bind(
    query: &ast::Query,
    source: &str,
    parameters: &Parameters,
) -> Result<Plan, Error> {
    let mut binder = Binder {
        source,
        parameters,
        slot_count: 0,
        closed: Vec::new(),
        expression_depth: 0,
        parameter_reads: 0,
    };
    let (singles, distinct) = match query {
        ast::Query::Single(single) => (vec![single], false),
        ast::Query::Union(union) => (
            std::iter::once(&union.first).chain(&union.rest).collect(),
            union.kind == ast::UnionKind::Distinct,
        ),
    };
    let mut parts = Vec::new();
    let mut columns: Option<Vec<String>> = None;
    for single in singles {
        let (pipeline, part_columns) = binder.single_query(single)?;
        match &columns {
            Some(first) if *first != part_columns => {
                return Err(Error::new(
                    ErrorKind::DifferentColumnsInUnion,
                    "all parts of a UNION must return the same columns",
                ));
            }
            _ => columns = Some(part_columns),
        }
        parts.push(pipeline);
    }
    let columns = columns.unwrap_or_default();
    let slot_count = binder.slot_count;
    let closed = binder.closed;
    let updating = parts.iter().any(|part| {
        part.clauses
            .iter()
            .any(|clause| matches!(clause, Clause::Update(_)))
    });
    if updating {
        return Ok(Plan::Updating(Program {
            parts,
            distinct,
            columns,
            slot_count,
            closed,
        }));
    }
    let parts = parts
        .into_iter()
        .map(|part| Pipeline {
            clauses: part
                .clauses
                .into_iter()
                .filter_map(|clause| match clause {
                    Clause::Read(clause) => Some(clause),
                    Clause::Update(_) => None,
                })
                .collect(),
            output: part.output,
        })
        .collect();
    Ok(Plan::ReadOnly(Program {
        parts,
        distinct,
        columns,
        slot_count,
        closed,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Node,
    Relationship,
    RelationshipList,
    Path,
    List,
    /// Known not to be a graph element, such as a literal or arithmetic result.
    Value,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
struct Variable {
    slot: Slot,
    kind: Kind,
}

#[derive(Clone, Debug, Default)]
struct Scope {
    variables: BTreeMap<String, Variable>,
}

impl Scope {
    fn get(&self, name: &str) -> Option<Variable> {
        self.variables.get(name).copied()
    }

    fn insert(&mut self, name: &str, slot: Slot, kind: Kind) {
        self.variables
            .insert(name.to_owned(), Variable { slot, kind });
    }
}

enum Mode<'a> {
    /// Against the scope. Aggregates are not allowed.
    Plain,
    /// Inside an aggregating projection: subexpressions equal to a grouping key read the key's
    /// slot, aggregates are collected and read their result slot, and any other variable of the
    /// incoming scope is ambiguous.
    Grouped(&'a mut Grouping),
    /// Against the scope, where subexpressions with the meaning of a projected item read the
    /// item's slot. That is how the `WHERE` and `ORDER BY` of `WITH DISTINCT` name an earlier
    /// variable's property.
    Projected(Projected<'a>),
    /// Inside `SKIP` or `LIMIT`, where no variable may appear.
    Constant,
}

#[derive(Clone, Copy)]
struct Projected<'a> {
    items: &'a [(ast::Expr, Slot)],
    scope: &'a Scope,
}

struct Grouping {
    keys: Vec<(ast::Expr, Slot)>,
    input: Scope,
    aggregates: Vec<(Aggregate, Slot)>,
    ordering: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatternUse {
    Match,
    Create,
    Merge,
    Predicate,
}

struct Binder<'a> {
    source: &'a str,
    parameters: &'a Parameters,
    slot_count: usize,
    closed: Vec<Expr>,
    expression_depth: usize,
    parameter_reads: usize,
}

fn error(kind: ErrorKind, message: impl Into<String>) -> Error {
    Error::new(kind, message)
}

impl Binder<'_> {
    fn slot(&mut self) -> Slot {
        let slot = self.slot_count;
        self.slot_count += 1;
        slot
    }

    fn single_query(
        &mut self,
        query: &ast::SingleQuery,
    ) -> Result<(Pipeline<Clause>, Vec<String>), Error> {
        let mut scope = Scope::default();
        let mut clauses = Vec::new();
        let mut output = None;
        let mut result_reads_variables = false;
        let last = query.clauses.len().saturating_sub(1);
        for (index, clause) in query.clauses.iter().enumerate() {
            if output.is_some() {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "RETURN must be the last clause",
                ));
            }
            match clause {
                ast::Clause::Match(found) => {
                    clauses.push(Clause::Read(self.match_clause(found, &mut scope)?));
                }
                ast::Clause::Unwind(unwind) => {
                    let expr = self.expr(&unwind.expr, &scope, &mut Mode::Plain)?;
                    let name = &unwind.variable.0;
                    if scope.get(name).is_some() {
                        return Err(error(
                            ErrorKind::VariableAlreadyBound,
                            format!("variable `{name}` is already bound"),
                        ));
                    }
                    let slot = self.slot();
                    scope.insert(name, slot, Kind::Unknown);
                    clauses.push(Clause::Read(ReadClause::Unwind { expr, slot }));
                }
                ast::Clause::With(with) => {
                    let (projection, next, _) =
                        self.projection(&with.projection, &scope, true, with.filter.as_ref())?;
                    scope = next;
                    clauses.push(Clause::Read(ReadClause::Project(Box::new(projection))));
                }
                ast::Clause::Return(returned) => {
                    let reads_before = self.parameter_reads;
                    let (projection, _, columns) =
                        self.projection(returned, &scope, false, None)?;
                    result_reads_variables =
                        reads_variables(returned) || self.parameter_reads > reads_before;
                    output = Some((projection.outputs.clone(), columns));
                    clauses.push(Clause::Read(ReadClause::Project(Box::new(projection))));
                }
                ast::Clause::Insert(parts) => {
                    let mut introduced = Vec::new();
                    let mut parts_bound = Vec::new();
                    for part in parts {
                        parts_bound.push(self.pattern_part(
                            part,
                            &mut scope,
                            PatternUse::Create,
                            &mut introduced,
                        )?);
                    }
                    clauses.push(Clause::Update(UpdateClause::Create(parts_bound)));
                }
                ast::Clause::Merge(merge) => {
                    let mut introduced = Vec::new();
                    let pattern = self.pattern_part(
                        &merge.pattern,
                        &mut scope,
                        PatternUse::Merge,
                        &mut introduced,
                    )?;
                    let on_create = self.set_items(&merge.on_create, &scope)?;
                    let on_match = self.set_items(&merge.on_match, &scope)?;
                    clauses.push(Clause::Update(UpdateClause::Merge(Box::new(MergeClause {
                        pattern,
                        on_create,
                        on_match,
                    }))));
                }
                ast::Clause::Set(items) => {
                    clauses.push(Clause::Update(UpdateClause::Set(
                        self.set_items(items, &scope)?,
                    )));
                }
                ast::Clause::Remove(items) => {
                    let mut actions = Vec::new();
                    for item in items {
                        actions.push(match item {
                            ast::RemoveItem::Property { base, key } => RemoveAction::Property {
                                target: self.expr(base, &scope, &mut Mode::Plain)?,
                                key: key.0.clone(),
                            },
                            ast::RemoveItem::Labels { variable, labels } => RemoveAction::Labels {
                                target: self.lookup(&variable.0, &scope)?.slot,
                                labels: names(labels),
                            },
                        });
                    }
                    clauses.push(Clause::Update(UpdateClause::Remove(actions)));
                }
                ast::Clause::Delete(delete) => {
                    let mut targets = Vec::new();
                    for target in &delete.targets {
                        if matches!(target, ast::Expr::HasLabels { .. }) {
                            return Err(error(
                                ErrorKind::InvalidDelete,
                                "DELETE takes an element, not a label expression",
                            ));
                        }
                        self.expect_shape(
                            target,
                            &scope,
                            &[Shape::Node, Shape::Relationship, Shape::Path, Shape::List],
                        )?;
                        let bound = self.expr(target, &scope, &mut Mode::Plain)?;
                        if let ast::Expr::Variable(name) = target
                            && self.lookup(&name.0, &scope)?.kind == Kind::Value
                        {
                            return Err(error(
                                ErrorKind::InvalidArgumentType,
                                "DELETE takes a node, relationship, or path",
                            ));
                        }
                        targets.push(bound);
                    }
                    clauses.push(Clause::Update(UpdateClause::Delete {
                        retain: true,
                        detach: delete.detach,
                        targets,
                    }));
                }
            }
            let ends_without_return = index == last && output.is_none();
            if ends_without_return
                && matches!(
                    clause,
                    ast::Clause::Match(_) | ast::Clause::With(_) | ast::Clause::Unwind(_)
                )
            {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "a query cannot end with a reading clause",
                ));
            }
        }
        if !result_reads_variables {
            for clause in &mut clauses {
                if let Clause::Update(UpdateClause::Delete { retain, .. }) = clause {
                    *retain = false;
                }
            }
        }
        let (output, columns) = output.unwrap_or_default();
        Ok((Pipeline { clauses, output }, columns))
    }

    fn lookup(&self, name: &str, scope: &Scope) -> Result<Variable, Error> {
        scope.get(name).ok_or_else(|| {
            error(
                ErrorKind::UndefinedVariable,
                format!("variable `{name}` is not defined"),
            )
        })
    }

    fn set_items(
        &mut self,
        items: &[ast::SetItem],
        scope: &Scope,
    ) -> Result<Vec<SetAction>, Error> {
        let mut actions = Vec::new();
        for item in items {
            actions.push(match item {
                ast::SetItem::Property { base, key, value } => SetAction::Property {
                    target: self.expr(base, scope, &mut Mode::Plain)?,
                    key: key.0.clone(),
                    value: self.expr(value, scope, &mut Mode::Plain)?,
                },
                ast::SetItem::ReplaceProperties { variable, value } => SetAction::Properties {
                    target: self.lookup(&variable.0, scope)?.slot,
                    value: self.expr(value, scope, &mut Mode::Plain)?,
                    replace: true,
                },
                ast::SetItem::MergeProperties { variable, value } => SetAction::Properties {
                    target: self.lookup(&variable.0, scope)?.slot,
                    value: self.expr(value, scope, &mut Mode::Plain)?,
                    replace: false,
                },
                ast::SetItem::Labels { variable, labels } => SetAction::Labels {
                    target: self.lookup(&variable.0, scope)?.slot,
                    labels: names(labels),
                },
            });
        }
        Ok(actions)
    }

    fn match_clause(&mut self, found: &ast::Match, scope: &mut Scope) -> Result<ReadClause, Error> {
        let mut introduced = Vec::new();
        let mut patterns = Vec::new();
        for part in &found.patterns {
            patterns.push(self.pattern_part(part, scope, PatternUse::Match, &mut introduced)?);
        }
        let filter = found
            .filter
            .as_ref()
            .map(|filter| self.filter(filter, scope, &mut Mode::Plain))
            .transpose()?;
        Ok(ReadClause::Match(MatchClause {
            optional: found.optional,
            patterns,
            filter,
            introduced,
        }))
    }

    fn pattern_part(
        &mut self,
        part: &ast::PatternPart,
        scope: &mut Scope,
        usage: PatternUse,
        introduced: &mut Vec<Slot>,
    ) -> Result<PatternPart, Error> {
        if let Some(path) = &part.path_variable
            && scope.get(&path.0).is_some()
        {
            return Err(error(
                ErrorKind::VariableAlreadyBound,
                format!("variable `{}` is already bound", path.0),
            ));
        }
        let start = self.node_pattern(&part.element.start, scope, usage, introduced)?;
        let mut steps = Vec::new();
        for step in &part.element.steps {
            let relationship =
                self.relationship_pattern(&step.relationship, scope, usage, introduced)?;
            let node = self.node_pattern(&step.node, scope, usage, introduced)?;
            steps.push(Step { relationship, node });
        }
        if matches!(usage, PatternUse::Create | PatternUse::Merge)
            && steps.is_empty()
            && start.bound
        {
            return Err(error(
                ErrorKind::VariableAlreadyBound,
                "a bound node cannot be created again",
            ));
        }
        let path = match &part.path_variable {
            Some(name) => {
                if scope.get(&name.0).is_some() {
                    return Err(error(
                        ErrorKind::VariableAlreadyBound,
                        format!("path `{}` also names an element of its pattern", name.0),
                    ));
                }
                let slot = self.slot();
                scope.insert(&name.0, slot, Kind::Path);
                introduced.push(slot);
                Some(slot)
            }
            None => None,
        };
        Ok(PatternPart { path, start, steps })
    }

    fn pattern_properties(
        &mut self,
        properties: Option<&ast::Properties>,
        scope: &Scope,
        usage: PatternUse,
    ) -> Result<Option<Expr>, Error> {
        match properties {
            None => Ok(None),
            Some(ast::Properties::Map(entries)) => {
                let mut bound = Vec::new();
                for (key, value) in entries {
                    bound.push((key.0.clone(), self.expr(value, scope, &mut Mode::Plain)?));
                }
                Ok(Some(Expr::Map(bound)))
            }
            Some(ast::Properties::Parameter(name)) => {
                if usage != PatternUse::Create {
                    return Err(error(
                        ErrorKind::InvalidParameterUse,
                        "a parameter map is only allowed in a pattern that creates",
                    ));
                }
                Ok(Some(Expr::Parameter(self.parameter(&name.0)?)))
            }
        }
    }

    fn node_pattern(
        &mut self,
        node: &ast::NodePattern,
        scope: &mut Scope,
        usage: PatternUse,
        introduced: &mut Vec<Slot>,
    ) -> Result<NodePattern, Error> {
        let properties = self.pattern_properties(node.properties.as_ref(), scope, usage)?;
        let labels = names(&node.labels);
        let (slot, bound) = match &node.variable {
            Some(name) => match scope.get(&name.0) {
                Some(variable) => {
                    if !matches!(variable.kind, Kind::Node | Kind::Unknown) {
                        return Err(error(
                            ErrorKind::VariableTypeConflict,
                            format!("variable `{}` is not a node", name.0),
                        ));
                    }
                    if matches!(usage, PatternUse::Create | PatternUse::Merge)
                        && (!labels.is_empty() || properties.is_some())
                    {
                        return Err(error(
                            ErrorKind::VariableAlreadyBound,
                            format!("node `{}` is already bound", name.0),
                        ));
                    }
                    (variable.slot, true)
                }
                None => {
                    let slot = self.slot();
                    scope.insert(&name.0, slot, Kind::Node);
                    introduced.push(slot);
                    (slot, false)
                }
            },
            None => {
                let slot = self.slot();
                introduced.push(slot);
                (slot, false)
            }
        };
        Ok(NodePattern {
            slot,
            bound,
            labels,
            properties,
        })
    }

    fn relationship_pattern(
        &mut self,
        relationship: &ast::RelationshipPattern,
        scope: &mut Scope,
        usage: PatternUse,
        introduced: &mut Vec<Slot>,
    ) -> Result<RelationshipPattern, Error> {
        let creates = matches!(usage, PatternUse::Create | PatternUse::Merge);
        if creates {
            if let Some(name) = &relationship.variable
                && scope.get(&name.0).is_some()
            {
                return Err(error(
                    ErrorKind::VariableAlreadyBound,
                    format!("relationship `{}` is already bound", name.0),
                ));
            }
            if relationship.length.is_some() {
                return Err(error(
                    ErrorKind::CreatingVarLength,
                    "a variable-length relationship cannot be created",
                ));
            }
            if relationship.types.len() != 1 {
                return Err(error(
                    ErrorKind::NoSingleRelationshipType,
                    "a relationship to create needs exactly one type",
                ));
            }
            if usage == PatternUse::Create && relationship.direction == Direction::Undirected {
                return Err(error(
                    ErrorKind::RequiresDirectedRelationship,
                    "a relationship to create needs a direction",
                ));
            }
        }
        let properties = self.pattern_properties(relationship.properties.as_ref(), scope, usage)?;
        let length = relationship.length.map(|length| Hops {
            min: length.min.unwrap_or(1),
            max: length.max,
        });
        let kind = if length.is_some() {
            Kind::RelationshipList
        } else {
            Kind::Relationship
        };
        let (slot, bound) = match &relationship.variable {
            Some(name) => match scope.get(&name.0) {
                Some(variable) => {
                    let list_of_relationships = kind == Kind::RelationshipList
                        && matches!(variable.kind, Kind::Value | Kind::List);
                    if variable.kind != kind
                        && variable.kind != Kind::Unknown
                        && !list_of_relationships
                    {
                        return Err(error(
                            ErrorKind::VariableTypeConflict,
                            format!("variable `{}` is not a relationship", name.0),
                        ));
                    }
                    if introduced.contains(&variable.slot) {
                        return Err(error(
                            ErrorKind::RelationshipUniquenessViolation,
                            format!("relationship `{}` appears twice in one pattern", name.0),
                        ));
                    }
                    (variable.slot, true)
                }
                None => {
                    let slot = self.slot();
                    scope.insert(&name.0, slot, kind);
                    introduced.push(slot);
                    (slot, false)
                }
            },
            None => {
                let slot = self.slot();
                introduced.push(slot);
                (slot, false)
            }
        };
        Ok(RelationshipPattern {
            slot,
            bound,
            types: names(&relationship.types),
            direction: relationship.direction,
            length,
            properties,
        })
    }

    fn filter(
        &mut self,
        filter: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        self.expect_shape(filter, scope, &[Shape::Boolean])?;
        self.expr(filter, scope, mode)
    }

    fn expect_shape(
        &self,
        expr: &ast::Expr,
        scope: &Scope,
        accepted: &[Shape],
    ) -> Result<(), Error> {
        match self.shape(expr, scope) {
            Some(shape) if !admits(accepted, shape) => Err(error(
                ErrorKind::InvalidArgumentType,
                format!("a {shape:?} cannot be used here; expected one of {accepted:?}"),
            )),
            _ => Ok(()),
        }
    }

    fn shape(&self, expr: &ast::Expr, scope: &Scope) -> Option<Shape> {
        match expr {
            ast::Expr::Variable(name) => match scope.get(&name.0)?.kind {
                Kind::Node => Some(Shape::Node),
                Kind::Relationship => Some(Shape::Relationship),
                Kind::Path => Some(Shape::Path),
                Kind::RelationshipList => Some(Shape::List),
                Kind::List | Kind::Value | Kind::Unknown => None,
            },
            ast::Expr::Literal(literal) => match literal {
                ast::Literal::Integer(_) => Some(Shape::Integer),
                ast::Literal::Float(_) => Some(Shape::Float),
                ast::Literal::String(_) => Some(Shape::String),
                ast::Literal::Boolean(_) => Some(Shape::Boolean),
                ast::Literal::Null => None,
            },
            ast::Expr::List(_)
            | ast::Expr::ListComprehension(_)
            | ast::Expr::PatternComprehension(_) => Some(Shape::List),
            ast::Expr::Map(_) => Some(Shape::Map),
            ast::Expr::Comparison { .. }
            | ast::Expr::IsNull { .. }
            | ast::Expr::HasLabels { .. }
            | ast::Expr::ListPredicate(_)
            | ast::Expr::Pattern(_)
            | ast::Expr::Exists(_) => Some(Shape::Boolean),
            ast::Expr::Unary {
                op: ast::UnaryOp::Not,
                ..
            } => Some(Shape::Boolean),
            ast::Expr::Binary { op, lhs, rhs } => match op {
                ast::BinaryOp::And
                | ast::BinaryOp::Or
                | ast::BinaryOp::Xor
                | ast::BinaryOp::In
                | ast::BinaryOp::StartsWith
                | ast::BinaryOp::EndsWith
                | ast::BinaryOp::Contains
                | ast::BinaryOp::RegexMatch => Some(Shape::Boolean),
                ast::BinaryOp::Add
                | ast::BinaryOp::Subtract
                | ast::BinaryOp::Multiply
                | ast::BinaryOp::Divide
                | ast::BinaryOp::Modulo
                | ast::BinaryOp::Power => {
                    let lhs = self.shape(lhs, scope)?;
                    let rhs = self.shape(rhs, scope)?;
                    (NUMBER.contains(&lhs) && NUMBER.contains(&rhs)).then_some(
                        if lhs == Shape::Integer
                            && rhs == Shape::Integer
                            && *op != ast::BinaryOp::Power
                        {
                            Shape::Integer
                        } else {
                            Shape::Float
                        },
                    )
                }
            },
            _ => None,
        }
    }

    fn parameter(&mut self, name: &str) -> Result<Datum, Error> {
        self.parameter_reads += 1;
        self.parameters
            .get(name)
            .map(from_parameter)
            .ok_or_else(|| {
                error(
                    ErrorKind::MissingParameter,
                    format!("parameter `${name}` was not supplied"),
                )
            })
    }

    fn column_name(&self, item: &ast::ProjectionItem) -> String {
        match &item.alias {
            Some(alias) => alias.0.clone(),
            None => self
                .source
                .get(item.span.start..item.span.end)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    fn projection(
        &mut self,
        projection: &ast::Projection,
        scope: &Scope,
        is_with: bool,
        filter: Option<&ast::Expr>,
    ) -> Result<(Projection, Scope, Vec<String>), Error> {
        let mut items: Vec<(ast::Expr, String)> = Vec::new();
        let explicit = match &projection.items {
            ast::ProjectionItems::Star(extra) => {
                if !is_with && scope.variables.is_empty() {
                    return Err(error(
                        ErrorKind::NoVariablesInScope,
                        "`*` needs at least one variable in scope",
                    ));
                }
                for name in scope.variables.keys() {
                    items.push((
                        ast::Expr::Variable(ast::Name::new(name.as_str())),
                        name.clone(),
                    ));
                }
                extra
            }
            ast::ProjectionItems::Items(explicit) => explicit,
        };
        for item in explicit {
            if is_with && item.alias.is_none() && !matches!(item.expr, ast::Expr::Variable(_)) {
                return Err(error(
                    ErrorKind::NoExpressionAlias,
                    "an expression in WITH needs an alias",
                ));
            }
            items.push((item.expr.clone(), self.column_name(item)));
        }
        let mut columns: Vec<String> = Vec::new();
        for (_, name) in &items {
            if columns.contains(name) {
                return Err(error(
                    ErrorKind::ColumnNameConflict,
                    format!("column `{name}` is projected twice"),
                ));
            }
            columns.push(name.clone());
        }

        let aggregating = items.iter().any(|(expr, _)| contains_aggregate(expr));
        let mut next = Scope::default();
        let mut outputs = Vec::new();
        let mut item_slots: Vec<(ast::Expr, Slot)> = Vec::new();
        let bound_items = if aggregating {
            let mut grouping = Grouping {
                keys: Vec::new(),
                input: scope.clone(),
                aggregates: Vec::new(),
                ordering: false,
            };
            let mut keys = Vec::new();
            for (expr, _) in &items {
                if !contains_aggregate(expr) {
                    let slot = self.slot();
                    keys.push((self.expr(expr, scope, &mut Mode::Plain)?, slot));
                    grouping.keys.push((expr.clone(), slot));
                }
            }
            let mut finals = Vec::new();
            let mut key_index = 0;
            for (expr, name) in &items {
                let slot = if contains_aggregate(expr) {
                    let slot = self.slot();
                    let empty = Scope::default();
                    let bound = self.expr(expr, &empty, &mut Mode::Grouped(&mut grouping))?;
                    finals.push((bound, slot));
                    slot
                } else {
                    let slot = keys[key_index].1;
                    key_index += 1;
                    slot
                };
                next.insert(name, slot, kind_of(expr, scope));
                outputs.push(slot);
                item_slots.push((expr.clone(), slot));
            }
            let order_by = {
                let mut ordering = Grouping {
                    keys: item_slots.clone(),
                    input: scope.clone(),
                    aggregates: std::mem::take(&mut grouping.aggregates),
                    ordering: true,
                };
                let mut order_by = Vec::new();
                for item in &projection.order_by {
                    let bound = match item_slots
                        .iter()
                        .find(|(expr, _)| same_meaning(expr, scope, &item.expr, &next))
                    {
                        Some((_, slot)) => Expr::Slot(*slot),
                        None => self.expr(&item.expr, &next, &mut Mode::Grouped(&mut ordering))?,
                    };
                    order_by.push((bound, item.direction));
                }
                grouping.aggregates = ordering.aggregates;
                order_by
            };
            (
                Items::Aggregate {
                    keys,
                    aggregates: grouping.aggregates,
                    finals,
                },
                order_by,
                next.clone(),
            )
        } else {
            let mut plain = Vec::new();
            for (expr, name) in &items {
                let slot = self.slot();
                plain.push((self.expr(expr, scope, &mut Mode::Plain)?, slot));
                next.insert(name, slot, kind_of(expr, scope));
                outputs.push(slot);
                item_slots.push((expr.clone(), slot));
            }
            let mut visible = if projection.distinct {
                Scope::default()
            } else {
                scope.clone()
            };
            for (name, variable) in &next.variables {
                visible.insert(name, variable.slot, variable.kind);
            }
            let mut order_by = Vec::new();
            for item in &projection.order_by {
                let projected = Projected {
                    items: &item_slots,
                    scope,
                };
                let bound = self.expr(&item.expr, &visible, &mut Mode::Projected(projected))?;
                order_by.push((bound, item.direction));
            }
            (Items::Plain(plain), order_by, visible)
        };
        let (items_bound, order_by, visible) = bound_items;
        let skip = projection
            .skip
            .as_ref()
            .map(|skip| self.expr(skip, &Scope::default(), &mut Mode::Constant))
            .transpose()?;
        let limit = projection
            .limit
            .as_ref()
            .map(|limit| self.expr(limit, &Scope::default(), &mut Mode::Constant))
            .transpose()?;
        let projected = (projection.distinct && !aggregating).then_some(Projected {
            items: &item_slots,
            scope,
        });
        let filter = filter
            .map(|filter| match projected {
                Some(projected) => self.filter(filter, &visible, &mut Mode::Projected(projected)),
                None => self.filter(filter, &visible, &mut Mode::Plain),
            })
            .transpose()?;
        Ok((
            Projection {
                items: items_bound,
                distinct: projection.distinct,
                outputs,
                order_by,
                skip,
                limit,
                filter,
            },
            next,
            columns,
        ))
    }

    fn boxed(
        &mut self,
        expr: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Box<Expr>, Error> {
        self.expr(expr, scope, mode).map(Box::new)
    }

    fn exprs(
        &mut self,
        exprs: &[ast::Expr],
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Vec<Expr>, Error> {
        exprs
            .iter()
            .map(|expr| self.expr(expr, scope, mode))
            .collect()
    }

    fn expr(
        &mut self,
        expr: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        self.expression_depth += 1;
        let bound = self.open_expr(expr, scope, mode);
        self.expression_depth -= 1;
        let bound = bound?;
        if self.expression_depth == 0 {
            let mut closed = Vec::new();
            collect_closed(&bound, &mut Vec::new(), &mut closed);
            self.closed.extend(closed.into_iter().cloned());
        }
        Ok(bound)
    }

    fn open_expr(
        &mut self,
        expr: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        if let Mode::Projected(projected) = mode
            && let Some((_, slot)) = projected
                .items
                .iter()
                .find(|(item, _)| same_meaning(item, projected.scope, expr, scope))
        {
            return Ok(Expr::Slot(*slot));
        }
        if let Mode::Grouped(grouping) = mode {
            if let Some((key, slot)) = grouping
                .keys
                .iter()
                .find(|(key, _)| same_meaning(key, &grouping.input, expr, scope))
            {
                if !is_simple_key(key) && !contains_aggregate(key) {
                    return Err(error(
                        ErrorKind::AmbiguousAggregationExpression,
                        "only a variable or a property of one can be used as a grouping key \
                         next to an aggregate",
                    ));
                }
                return Ok(Expr::Slot(*slot));
            }
            if let Some((function, distinct, args)) = aggregate_call(expr) {
                if grouping.ordering {
                    self.aggregate(function, distinct, args, scope)?;
                }
                let aggregate = self.aggregate(function, distinct, args, &grouping.input)?;
                let slot = self.slot();
                grouping.aggregates.push((aggregate, slot));
                return Ok(Expr::Slot(slot));
            }
            if let ast::Expr::Variable(name) = expr
                && scope.get(&name.0).is_none()
            {
                let known = !grouping.ordering && grouping.input.get(&name.0).is_some();
                return Err(if known {
                    error(
                        ErrorKind::AmbiguousAggregationExpression,
                        format!(
                            "`{}` is neither a grouping key nor inside an aggregate",
                            name.0
                        ),
                    )
                } else {
                    error(
                        ErrorKind::UndefinedVariable,
                        format!("variable `{}` is not defined", name.0),
                    )
                });
            }
        }
        Ok(match expr {
            ast::Expr::Literal(literal) => Expr::Constant(match literal {
                ast::Literal::Integer(value) => Datum::Integer(*value),
                ast::Literal::Float(value) => Datum::Float(*value),
                ast::Literal::String(value) => Datum::String(value.clone()),
                ast::Literal::Boolean(value) => Datum::Boolean(*value),
                ast::Literal::Null => Datum::Null,
            }),
            ast::Expr::Parameter(name) => Expr::Parameter(self.parameter(&name.0)?),
            ast::Expr::Variable(name) => {
                if matches!(mode, Mode::Constant) && scope.get(&name.0).is_none() {
                    return Err(error(
                        ErrorKind::NonConstantExpression,
                        "SKIP and LIMIT cannot refer to variables",
                    ));
                }
                Expr::Slot(self.lookup(&name.0, scope)?.slot)
            }
            ast::Expr::List(items) => Expr::List(self.exprs(items, scope, mode)?),
            ast::Expr::Map(entries) => {
                let mut bound = Vec::new();
                for (key, value) in entries {
                    bound.push((key.0.clone(), self.expr(value, scope, mode)?));
                }
                Expr::Map(bound)
            }
            ast::Expr::Property { base, key } => {
                self.expect_shape(base, scope, &[Shape::Node, Shape::Relationship, Shape::Map])?;
                Expr::Property(self.boxed(base, scope, mode)?, key.0.clone())
            }
            ast::Expr::Index { base, index } => Expr::Index(
                self.boxed(base, scope, mode)?,
                self.boxed(index, scope, mode)?,
            ),
            ast::Expr::Slice { base, from, to } => {
                let base = self.boxed(base, scope, mode)?;
                let from = match from {
                    Some(from) => Some(self.boxed(from, scope, mode)?),
                    None => None,
                };
                let to = match to {
                    Some(to) => Some(self.boxed(to, scope, mode)?),
                    None => None,
                };
                Expr::Slice(base, from, to)
            }
            ast::Expr::HasLabels { operand, labels } => {
                Expr::HasLabels(self.boxed(operand, scope, mode)?, names(labels))
            }
            ast::Expr::Unary { op, operand } => {
                let operand = self.boxed(operand, scope, mode)?;
                match op {
                    ast::UnaryOp::Not => Expr::Not(operand),
                    ast::UnaryOp::Negate => Expr::Negate(operand),
                    ast::UnaryOp::Plus => Expr::UnaryPlus(operand),
                }
            }
            ast::Expr::Binary { op, lhs, rhs } => Expr::Binary(
                *op,
                self.boxed(lhs, scope, mode)?,
                self.boxed(rhs, scope, mode)?,
            ),
            ast::Expr::Comparison { first, rest } => {
                let first = self.boxed(first, scope, mode)?;
                let mut chain = Vec::new();
                for (op, operand) in rest {
                    chain.push((*op, self.expr(operand, scope, mode)?));
                }
                Expr::Comparison(first, chain)
            }
            ast::Expr::IsNull { operand, negated } => Expr::IsNull {
                operand: self.boxed(operand, scope, mode)?,
                negated: *negated,
            },
            ast::Expr::Function(call) => {
                let name = qualified_name(call);
                if aggregate_function(&name).is_some() {
                    return Err(aggregate_not_allowed(mode));
                }
                let function = Function::lookup(&name).ok_or_else(|| {
                    error(
                        ErrorKind::UnknownFunction,
                        format!("function `{name}` does not exist"),
                    )
                })?;
                if !function.accepts(call.args.len()) {
                    return Err(error(
                        ErrorKind::InvalidNumberOfArguments,
                        format!("function `{name}` got {} arguments", call.args.len()),
                    ));
                }
                for (index, arg) in call.args.iter().enumerate() {
                    self.expect_shape(arg, scope, function.parameter(index))?;
                }
                Expr::Call(function, self.exprs(&call.args, scope, mode)?)
            }
            ast::Expr::CountStar => return Err(aggregate_not_allowed(mode)),
            ast::Expr::Case(case) => {
                let scrutinee = match &case.scrutinee {
                    Some(scrutinee) => Some(self.expr(scrutinee, scope, mode)?),
                    None => None,
                };
                let mut branches = Vec::new();
                for branch in &case.branches {
                    if case.scrutinee.is_none() {
                        self.expect_shape(&branch.when, scope, &[Shape::Boolean])?;
                    }
                    branches.push((
                        self.expr(&branch.when, scope, mode)?,
                        self.expr(&branch.then, scope, mode)?,
                    ));
                }
                let otherwise = match &case.otherwise {
                    Some(otherwise) => Some(self.expr(otherwise, scope, mode)?),
                    None => None,
                };
                Expr::Case(Box::new(Case {
                    scrutinee,
                    branches,
                    otherwise,
                }))
            }
            ast::Expr::ListComprehension(comprehension) => {
                if comprehension
                    .filter
                    .iter()
                    .chain(&comprehension.projection)
                    .any(|body| contains_aggregate(body))
                {
                    return Err(per_element_aggregate());
                }
                let source = self.expr(&comprehension.source, scope, mode)?;
                let slot = self.slot();
                let mut inner = scope.clone();
                inner.insert(&comprehension.variable.0, slot, Kind::Unknown);
                let filter = match &comprehension.filter {
                    Some(filter) => Some(self.filter(filter, &inner, mode)?),
                    None => None,
                };
                let projection = match &comprehension.projection {
                    Some(projection) => Some(self.expr(projection, &inner, mode)?),
                    None => None,
                };
                Expr::ListComprehension(Box::new(ListComprehension {
                    slot,
                    source,
                    filter,
                    projection,
                }))
            }
            ast::Expr::ListPredicate(predicate) => {
                if contains_aggregate(&predicate.predicate) {
                    return Err(per_element_aggregate());
                }
                let source = self.expr(&predicate.source, scope, mode)?;
                let slot = self.slot();
                let mut inner = scope.clone();
                inner.insert(&predicate.variable.0, slot, Kind::Unknown);
                let condition = self.filter(&predicate.predicate, &inner, mode)?;
                Expr::Quantified(Box::new(Quantified {
                    quantifier: predicate.quantifier,
                    slot,
                    source,
                    predicate: condition,
                }))
            }
            ast::Expr::Pattern(element) => {
                let scope = &self.pattern_scope(scope, pattern_variables(element), mode)?;
                if let Some(name) =
                    pattern_variables(element).find(|name| scope.get(&name.0).is_none())
                {
                    return Err(error(
                        ErrorKind::UndefinedVariable,
                        format!(
                            "variable `{}` is not defined; a pattern predicate cannot introduce one",
                            name.0
                        ),
                    ));
                }
                let part = ast::PatternPart {
                    path_variable: None,
                    element: element.clone(),
                };
                Expr::Exists(Box::new(self.pattern_query(
                    std::slice::from_ref(&part),
                    None,
                    scope,
                    mode,
                )?))
            }
            ast::Expr::Exists(subquery) => match subquery.as_ref() {
                ast::ExistsSubquery::Pattern { patterns, filter } => {
                    let names = patterns
                        .iter()
                        .flat_map(|part| pattern_variables(&part.element));
                    let scope = &self.pattern_scope(scope, names, mode)?;
                    Expr::Exists(Box::new(self.pattern_query(
                        patterns,
                        filter.as_ref(),
                        scope,
                        mode,
                    )?))
                }
                ast::ExistsSubquery::Query(_) => {
                    return Err(error(
                        ErrorKind::Unsupported,
                        "EXISTS with a full subquery is not supported",
                    ));
                }
            },
            ast::Expr::PatternComprehension(comprehension) => {
                if comprehension
                    .filter
                    .iter()
                    .chain(std::iter::once(&comprehension.projection))
                    .any(|body| contains_aggregate(body))
                {
                    return Err(per_element_aggregate());
                }
                let part = ast::PatternPart {
                    path_variable: comprehension.path_variable.clone(),
                    element: comprehension.element.clone(),
                };
                let mut inner =
                    self.pattern_scope(scope, pattern_variables(&part.element), mode)?;
                let mut introduced = Vec::new();
                let pattern =
                    self.pattern_part(&part, &mut inner, PatternUse::Predicate, &mut introduced)?;
                let filter = match &comprehension.filter {
                    Some(filter) => Some(self.expr(filter, &inner, mode)?),
                    None => None,
                };
                let projection = self.expr(&comprehension.projection, &inner, mode)?;
                Expr::PatternComprehension(
                    Box::new(PatternQuery {
                        patterns: vec![pattern],
                        filter,
                    }),
                    Box::new(projection),
                )
            }
        })
    }

    fn pattern_scope<'n>(
        &self,
        scope: &Scope,
        names: impl Iterator<Item = &'n ast::Name>,
        mode: &Mode<'_>,
    ) -> Result<Scope, Error> {
        let mut visible = scope.clone();
        let hidden = |name: &ast::Name| match mode {
            Mode::Grouped(grouping) => grouping.input.get(&name.0).map(|_| {
                if grouping.ordering {
                    ErrorKind::UndefinedVariable
                } else {
                    ErrorKind::AmbiguousAggregationExpression
                }
            }),
            Mode::Projected(projected) => projected
                .scope
                .get(&name.0)
                .map(|_| ErrorKind::UndefinedVariable),
            Mode::Plain | Mode::Constant => None,
        };
        if let Mode::Grouped(grouping) = mode
            && !grouping.ordering
        {
            for (key, slot) in &grouping.keys {
                if let ast::Expr::Variable(name) = key
                    && visible.get(&name.0).is_none()
                {
                    visible.insert(&name.0, *slot, kind_of(key, &grouping.input));
                }
            }
        }
        for name in names {
            if visible.get(&name.0).is_none()
                && let Some(kind) = hidden(name)
            {
                return Err(error(
                    kind,
                    format!("`{}` is not visible next to this projection", name.0),
                ));
            }
        }
        Ok(visible)
    }

    fn pattern_query(
        &mut self,
        patterns: &[ast::PatternPart],
        filter: Option<&ast::Expr>,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<PatternQuery, Error> {
        let mut inner = scope.clone();
        let mut introduced = Vec::new();
        let mut bound = Vec::new();
        for part in patterns {
            bound.push(self.pattern_part(
                part,
                &mut inner,
                PatternUse::Predicate,
                &mut introduced,
            )?);
        }
        let filter = match filter {
            Some(filter) => Some(self.expr(filter, &inner, mode)?),
            None => None,
        };
        Ok(PatternQuery {
            patterns: bound,
            filter,
        })
    }

    fn aggregate(
        &mut self,
        function: AggregateFunction,
        distinct: bool,
        args: &[ast::Expr],
        input: &Scope,
    ) -> Result<Aggregate, Error> {
        for arg in args {
            if contains_aggregate(arg) {
                return Err(error(
                    ErrorKind::NestedAggregation,
                    "an aggregate cannot contain another aggregate",
                ));
            }
        }
        let expected = match function {
            AggregateFunction::PercentileCont | AggregateFunction::PercentileDisc => 2,
            _ => 1,
        };
        let is_count_star = function == AggregateFunction::Count && args.is_empty();
        if !is_count_star && args.len() != expected {
            return Err(error(
                ErrorKind::InvalidNumberOfArguments,
                "wrong number of arguments to an aggregate",
            ));
        }
        let argument = match args.first() {
            Some(arg) => Some(self.expr(arg, input, &mut Mode::Plain)?),
            None => None,
        };
        let extra = match args.get(1) {
            Some(arg) => Some(self.expr(arg, input, &mut Mode::Plain)?),
            None => None,
        };
        Ok(Aggregate {
            function,
            distinct,
            argument,
            extra,
        })
    }
}

fn per_element_aggregate() -> Error {
    error(
        ErrorKind::InvalidAggregation,
        "an aggregate cannot be computed once per list element",
    )
}

fn aggregate_not_allowed(mode: &Mode<'_>) -> Error {
    match mode {
        Mode::Grouped(_) => error(
            ErrorKind::NestedAggregation,
            "an aggregate cannot contain another aggregate",
        ),
        Mode::Plain | Mode::Projected(_) | Mode::Constant => error(
            ErrorKind::InvalidAggregation,
            "aggregates are only allowed in WITH and RETURN",
        ),
    }
}

fn pattern_variables(element: &ast::PatternElement) -> impl Iterator<Item = &ast::Name> {
    element.start.variable.iter().chain(
        element
            .steps
            .iter()
            .flat_map(|step| step.relationship.variable.iter().chain(&step.node.variable)),
    )
}

fn evaluated_parts(expr: &Expr) -> Option<(Vec<&Expr>, Vec<&Expr>, Option<Slot>)> {
    Some(match expr {
        Expr::List(items) | Expr::Call(_, items) => (items.iter().collect(), Vec::new(), None),
        Expr::Map(entries) => (
            entries.iter().map(|(_, value)| value).collect(),
            Vec::new(),
            None,
        ),
        Expr::Property(operand, _)
        | Expr::HasLabels(operand, _)
        | Expr::Not(operand)
        | Expr::Negate(operand)
        | Expr::UnaryPlus(operand)
        | Expr::IsNull { operand, .. } => (vec![operand], Vec::new(), None),
        Expr::Index(base, index) | Expr::Binary(_, base, index) => {
            (vec![base, index], Vec::new(), None)
        }
        Expr::Slice(base, from, to) => (
            std::iter::once(&**base)
                .chain([from, to].into_iter().flatten().map(|bound| &**bound))
                .collect(),
            Vec::new(),
            None,
        ),
        Expr::Comparison(first, rest) => (
            std::iter::once(&**first)
                .chain(rest.iter().map(|(_, operand)| operand))
                .collect(),
            Vec::new(),
            None,
        ),
        Expr::Case(case) => {
            let mut always: Vec<&Expr> = case.scrutinee.iter().collect();
            let mut sometimes: Vec<&Expr> = Vec::new();
            for (position, (when, then)) in case.branches.iter().enumerate() {
                if position == 0 {
                    always.push(when);
                } else {
                    sometimes.push(when);
                }
                sometimes.push(then);
            }
            sometimes.extend(&case.otherwise);
            (always, sometimes, None)
        }
        Expr::ListComprehension(comprehension) => (
            vec![&comprehension.source],
            comprehension
                .filter
                .iter()
                .chain(&comprehension.projection)
                .collect(),
            Some(comprehension.slot),
        ),
        Expr::Quantified(quantified) => (
            vec![&quantified.source],
            vec![&quantified.predicate],
            Some(quantified.slot),
        ),
        Expr::Constant(_)
        | Expr::Parameter(_)
        | Expr::Slot(_)
        | Expr::Exists(_)
        | Expr::PatternComprehension(..) => return None,
    })
}

fn collect_closed<'a>(
    expr: &'a Expr,
    local: &mut Vec<Slot>,
    out: &mut Vec<&'a Expr>,
) -> Option<usize> {
    let base = local.len();
    let Some((always, sometimes, bound)) = evaluated_parts(expr) else {
        return match expr {
            Expr::Constant(_) => Some(usize::MAX),
            Expr::Slot(slot) => local.iter().rposition(|bound| bound == slot),
            _ => None,
        };
    };
    let mark = out.len();
    let mut outermost = Some(usize::MAX);
    for child in always {
        outermost = join(outermost, collect_closed(child, local, out));
    }
    let covered = out.len();
    local.extend(bound);
    for child in sometimes {
        outermost = join(outermost, collect_closed(child, local, out));
    }
    if bound.is_some() {
        local.pop();
    }
    if outermost.is_some_and(|reach| reach >= base) {
        out.drain(mark..covered);
        out.push(expr);
    }
    outermost
}

fn join(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    a.zip(b).map(|(a, b)| a.min(b))
}

fn is_simple_key(key: &ast::Expr) -> bool {
    match key {
        ast::Expr::Variable(_) => true,
        ast::Expr::Property { base, .. } => matches!(base.as_ref(), ast::Expr::Variable(_)),
        _ => false,
    }
}

fn names(names: &[ast::Name]) -> Vec<String> {
    names.iter().map(|name| name.0.clone()).collect()
}

fn qualified_name(call: &ast::FunctionCall) -> String {
    let mut parts: Vec<&str> = call.namespace.iter().map(|name| name.0.as_str()).collect();
    parts.push(&call.name.0);
    parts.join(".")
}

fn aggregate_function(name: &str) -> Option<AggregateFunction> {
    let functions = [
        ("count", AggregateFunction::Count),
        ("sum", AggregateFunction::Sum),
        ("avg", AggregateFunction::Avg),
        ("min", AggregateFunction::Min),
        ("max", AggregateFunction::Max),
        ("collect", AggregateFunction::Collect),
        ("stdev", AggregateFunction::StDev),
        ("stdevp", AggregateFunction::StDevP),
        ("percentilecont", AggregateFunction::PercentileCont),
        ("percentiledisc", AggregateFunction::PercentileDisc),
    ];
    functions
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, function)| *function)
}

fn aggregate_call(expr: &ast::Expr) -> Option<(AggregateFunction, bool, &[ast::Expr])> {
    match expr {
        ast::Expr::CountStar => Some((AggregateFunction::Count, false, &[])),
        ast::Expr::Function(call) => aggregate_function(&qualified_name(call))
            .map(|function| (function, call.distinct, call.args.as_slice())),
        _ => None,
    }
}

fn reads_variables(projection: &ast::Projection) -> bool {
    match &projection.items {
        ast::ProjectionItems::Star(_) => true,
        ast::ProjectionItems::Items(items) => items
            .iter()
            .any(|item| !free_variables(&item.expr).is_empty()),
    }
}

fn same_meaning(key: &ast::Expr, key_scope: &Scope, expr: &ast::Expr, scope: &Scope) -> bool {
    key == expr
        && free_variables(expr).iter().all(|name| {
            scope.get(name).is_none_or(|used| {
                key_scope
                    .get(name)
                    .is_some_and(|keyed| keyed.slot == used.slot)
            })
        })
}

fn free_variables(expr: &ast::Expr) -> Vec<&str> {
    let mut free = Vec::new();
    collect_free(expr, &mut Vec::new(), &mut free);
    free
}

fn collect_free<'a>(expr: &'a ast::Expr, bound: &mut Vec<&'a str>, free: &mut Vec<&'a str>) {
    match expr {
        ast::Expr::Variable(name) => {
            if !bound.contains(&name.0.as_str()) {
                free.push(&name.0);
            }
        }
        ast::Expr::ListComprehension(comprehension) => {
            collect_free(&comprehension.source, bound, free);
            bound.push(&comprehension.variable.0);
            for body in comprehension.filter.iter().chain(&comprehension.projection) {
                collect_free(body, bound, free);
            }
            bound.pop();
        }
        ast::Expr::ListPredicate(predicate) => {
            collect_free(&predicate.source, bound, free);
            bound.push(&predicate.variable.0);
            collect_free(&predicate.predicate, bound, free);
            bound.pop();
        }
        ast::Expr::Pattern(element) => collect_pattern_free(element, bound, free),
        ast::Expr::Exists(subquery) => {
            if let ast::ExistsSubquery::Pattern { patterns, filter } = subquery.as_ref() {
                for part in patterns {
                    collect_pattern_free(&part.element, bound, free);
                }
                if let Some(filter) = filter {
                    collect_free(filter, bound, free);
                }
            }
        }
        ast::Expr::PatternComprehension(comprehension) => {
            collect_pattern_free(&comprehension.element, bound, free);
            for body in comprehension
                .filter
                .iter()
                .chain(std::iter::once(&comprehension.projection))
            {
                collect_free(body, bound, free);
            }
        }
        other => {
            for child in children(other) {
                collect_free(child, bound, free);
            }
        }
    }
}

fn collect_pattern_free<'a>(
    element: &'a ast::PatternElement,
    bound: &mut Vec<&'a str>,
    free: &mut Vec<&'a str>,
) {
    for name in pattern_variables(element) {
        if !bound.contains(&name.0.as_str()) {
            free.push(&name.0);
        }
    }
    let nodes = std::iter::once(&element.start.properties)
        .chain(element.steps.iter().map(|step| &step.node.properties));
    let relationships = element
        .steps
        .iter()
        .map(|step| &step.relationship.properties);
    for properties in nodes.chain(relationships) {
        if let Some(ast::Properties::Map(entries)) = properties {
            for (_, value) in entries {
                collect_free(value, bound, free);
            }
        }
    }
}

fn contains_aggregate(expr: &ast::Expr) -> bool {
    aggregate_call(expr).is_some() || children(expr).into_iter().any(contains_aggregate)
}

fn children(expr: &ast::Expr) -> Vec<&ast::Expr> {
    match expr {
        ast::Expr::Literal(_)
        | ast::Expr::Parameter(_)
        | ast::Expr::Variable(_)
        | ast::Expr::CountStar
        | ast::Expr::Pattern(_)
        | ast::Expr::Exists(_) => Vec::new(),
        ast::Expr::List(items) => items.iter().collect(),
        ast::Expr::Map(entries) => entries.iter().map(|(_, value)| value).collect(),
        ast::Expr::Property { base, .. } => vec![base],
        ast::Expr::Index { base, index } => vec![base, index],
        ast::Expr::Slice { base, from, to } => std::iter::once(base.as_ref())
            .chain(from.as_deref())
            .chain(to.as_deref())
            .collect(),
        ast::Expr::HasLabels { operand, .. } => vec![operand],
        ast::Expr::Unary { operand, .. } => vec![operand],
        ast::Expr::Binary { lhs, rhs, .. } => vec![lhs, rhs],
        ast::Expr::Comparison { first, rest } => std::iter::once(first.as_ref())
            .chain(rest.iter().map(|(_, operand)| operand))
            .collect(),
        ast::Expr::IsNull { operand, .. } => vec![operand],
        ast::Expr::Function(call) => call.args.iter().collect(),
        ast::Expr::Case(case) => case
            .scrutinee
            .as_deref()
            .into_iter()
            .chain(
                case.branches
                    .iter()
                    .flat_map(|branch| [&branch.when, &branch.then]),
            )
            .chain(case.otherwise.as_deref())
            .collect(),
        ast::Expr::ListComprehension(comprehension) => vec![&comprehension.source],
        ast::Expr::ListPredicate(predicate) => vec![&predicate.source],
        ast::Expr::PatternComprehension(_) => Vec::new(),
    }
}

fn kind_of(expr: &ast::Expr, scope: &Scope) -> Kind {
    match expr {
        ast::Expr::Variable(name) => scope
            .get(&name.0)
            .map_or(Kind::Unknown, |variable| variable.kind),
        ast::Expr::Literal(ast::Literal::Null) => Kind::Unknown,
        ast::Expr::List(_)
        | ast::Expr::ListComprehension(_)
        | ast::Expr::PatternComprehension(_) => Kind::List,
        ast::Expr::Binary {
            op: ast::BinaryOp::Add,
            lhs,
            rhs,
        } => {
            if kind_of(lhs, scope) == Kind::Value && kind_of(rhs, scope) == Kind::Value {
                Kind::Value
            } else {
                Kind::Unknown
            }
        }
        ast::Expr::Literal(_)
        | ast::Expr::Map(_)
        | ast::Expr::Binary { .. }
        | ast::Expr::Comparison { .. }
        | ast::Expr::IsNull { .. }
        | ast::Expr::HasLabels { .. }
        | ast::Expr::Unary { .. }
        | ast::Expr::ListPredicate(_)
        | ast::Expr::Pattern(_)
        | ast::Expr::Exists(_)
        | ast::Expr::CountStar => Kind::Value,
        _ => Kind::Unknown,
    }
}

#[cfg(test)]
#[path = "bind.test.rs"]
mod tests;
