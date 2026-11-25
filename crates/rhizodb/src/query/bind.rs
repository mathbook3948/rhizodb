use std::collections::BTreeMap;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::gql::ast;
use crate::gql::ast::Direction;
use crate::procedure::Procedures;
use crate::result::Parameters;

use super::builtin::Builtin;
use super::datum::Datum;
use super::functions::Function;
use super::plan::Aggregate;
use super::plan::AggregateFunction;
use super::plan::CallClause;
use super::plan::CallSubquery;
use super::plan::Callee;
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
use super::plan::Reduce;
use super::plan::RelationshipPattern;
use super::plan::RemoveAction;
use super::plan::SetAction;
use super::plan::Slot;
use super::plan::Step;
use super::plan::SubqueryKind;
use super::plan::UpdateClause;
use super::seek;
use super::shape::NUMBER;
use super::shape::Shape;
use super::shape::admits;
use super::shape::binary_error;
use super::shape::index_error;
use super::shape::label_test_error;
use super::shape::procedure_parameter;
use super::shape::slice_bound_error;
use super::shape::slice_error;
use super::shape::unary_error;

/// Resolves every variable of `query` to a row slot and rejects queries that are malformed by
/// scope or by clause order. Parameters are folded in as constants.
pub(crate) fn bind(
    query: &ast::Query,
    source: &str,
    parameters: &Parameters,
    procedures: &Procedures,
) -> Result<Plan, Error> {
    let mut binder = Binder {
        source,
        parameters,
        procedures,
        slot_count: 0,
        closed: Vec::new(),
        expression_depth: 0,
        parameter_reads: 0,
        parameter_names: Vec::new(),
        type_error: None,
        updates_allowed: true,
    };
    let (singles, combine) = union_parts(query);
    let mut parts = Vec::new();
    let mut columns: Option<Vec<String>> = None;
    let mut results_read_variables = false;
    for single in singles {
        let (pipeline, part_columns, reads_variables) =
            binder.single_query(single, Scope::default(), Body::Query)?;
        results_read_variables |= reads_variables;
        match &columns {
            Some(first) if *first != part_columns => {
                return Err(Error::new(
                    ErrorKind::DifferentColumnsInUnion,
                    "all parts of a UNION, EXCEPT, INTERSECT, or OTHERWISE must return the same columns",
                ));
            }
            _ => columns = Some(part_columns),
        }
        parts.push(pipeline);
    }
    if results_read_variables {
        for clause in parts.iter_mut().flat_map(|part| &mut part.clauses) {
            if let Clause::Update(UpdateClause::Delete { retain, .. }) = clause {
                *retain = true;
            }
        }
    }
    if let Some(type_error) = binder.type_error {
        return Err(type_error);
    }
    let columns = columns.unwrap_or_default();
    let slot_count = binder.slot_count;
    let closed = binder.closed;
    let parameters = binder.parameter_names;
    let updating = parts.iter().any(|part| {
        part.clauses
            .iter()
            .any(|clause| matches!(clause, Clause::Update(_)))
    });
    if updating {
        return Ok(Plan::Updating(Program {
            parts,
            combine,
            columns,
            slot_count,
            closed,
            parameters,
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
        combine,
        columns,
        slot_count,
        closed,
        parameters,
    }))
}

fn query_updates(query: &ast::Query) -> bool {
    let (singles, _) = union_parts(query);
    singles
        .iter()
        .any(|single| single.clauses.iter().any(clause_updates))
}

fn clause_updates(clause: &ast::Clause) -> bool {
    match clause {
        ast::Clause::Insert(_)
        | ast::Clause::Merge(_)
        | ast::Clause::Set(_)
        | ast::Clause::Remove(_)
        | ast::Clause::Delete(_) => true,
        ast::Clause::CallSubquery(call) => query_updates(&call.body),
        ast::Clause::Match(_)
        | ast::Clause::With(_)
        | ast::Clause::Return(_)
        | ast::Clause::Unwind(_)
        | ast::Clause::Call(_)
        | ast::Clause::Filter(_)
        | ast::Clause::Let(_) => false,
    }
}

fn union_parts(query: &ast::Query) -> (Vec<&ast::SingleQuery>, ast::UnionKind) {
    match query {
        ast::Query::Single(single) => (vec![single], ast::UnionKind::All),
        ast::Query::Union(union) => (
            std::iter::once(&union.first).chain(&union.rest).collect(),
            union.kind,
        ),
    }
}

fn imported_scope(
    imports: Option<&ast::Imports>,
    body: &ast::SingleQuery,
    outer: &Scope,
) -> Result<Scope, Error> {
    let mut inner = Scope::default();
    match imports {
        Some(ast::Imports::All) => return Ok(outer.clone()),
        Some(ast::Imports::Variables(names)) => {
            for name in names {
                let Some(variable) = outer.get(&name.0) else {
                    return Err(error(
                        ErrorKind::UndefinedVariable,
                        format!("variable `{}` is not defined", name.0),
                    ));
                };
                inner.variables.insert(name.0.clone(), variable);
            }
        }
        None => {
            if let Some(ast::Clause::With(with)) = body.clauses.first() {
                match &with.projection.items {
                    ast::ProjectionItems::Star(_) => return Ok(outer.clone()),
                    ast::ProjectionItems::Items(items) => {
                        for item in items {
                            if let ast::Expr::Variable(name) = &item.expr
                                && let Some(variable) = outer.get(&name.0)
                            {
                                inner.variables.insert(name.0.clone(), variable);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(inner)
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
    procedures: &'a Procedures,
    slot_count: usize,
    closed: Vec<Expr>,
    expression_depth: usize,
    parameter_reads: usize,
    parameter_names: Vec<String>,
    type_error: Option<Error>,
    updates_allowed: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    Query,
    Call,
    Expression,
}

fn error(kind: ErrorKind, message: impl Into<String>) -> Error {
    Error::new(kind, message)
}

/// The error for a query that reads the parameter `name` when it was not supplied.
pub(crate) fn missing_parameter(name: &str) -> Error {
    error(
        ErrorKind::MissingParameter,
        format!("parameter `${name}` was not supplied"),
    )
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
        mut scope: Scope,
        body: Body,
    ) -> Result<(Pipeline<Clause>, Vec<String>, bool), Error> {
        let mut clauses = Vec::new();
        let mut output = None;
        let mut result_reads_variables = false;
        let mut follows_update = false;
        let last = query.clauses.len().saturating_sub(1);
        for (index, clause) in query.clauses.iter().enumerate() {
            if output.is_some() {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "RETURN must be the last clause",
                ));
            }
            if follows_update
                && matches!(
                    clause,
                    ast::Clause::Match(_)
                        | ast::Clause::Unwind(_)
                        | ast::Clause::Call(_)
                        | ast::Clause::CallSubquery(_)
                )
            {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "WITH is needed between an updating clause and a reading clause",
                ));
            }
            let updates = clause_updates(clause);
            follows_update = updates
                || (follows_update
                    && matches!(clause, ast::Clause::Filter(_) | ast::Clause::Let(_)));
            if updates && !self.updates_allowed {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "a subquery cannot update the graph",
                ));
            }
            clauses.push(self.clause(
                clause,
                &query.clauses[index + 1..],
                body == Body::Query && query.clauses.len() == 1,
                &mut scope,
                &mut output,
                &mut result_reads_variables,
            )?);
            let ends_without_return = index == last && output.is_none() && body != Body::Expression;
            if ends_without_return
                && !updates
                && matches!(
                    clause,
                    ast::Clause::Match(_)
                        | ast::Clause::With(_)
                        | ast::Clause::Unwind(_)
                        | ast::Clause::Call(_)
                        | ast::Clause::CallSubquery(_)
                        | ast::Clause::Filter(_)
                        | ast::Clause::Let(_)
                )
            {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "a query cannot end with a reading clause",
                ));
            }
        }
        let (output, columns) = output.unwrap_or_default();
        Ok((
            Pipeline { clauses, output },
            columns,
            result_reads_variables,
        ))
    }

    fn clause(
        &mut self,
        clause: &ast::Clause,
        later: &[ast::Clause],
        standalone: bool,
        scope: &mut Scope,
        output: &mut Option<(Vec<Slot>, Vec<String>)>,
        reads: &mut bool,
    ) -> Result<Clause, Error> {
        match clause {
            ast::Clause::Match(found) => Ok(Clause::Read(self.match_clause(found, scope)?)),
            ast::Clause::Unwind(unwind) => self.unwind_clause(unwind, scope),
            ast::Clause::Call(call) => self.call_clause(call, scope, standalone, output, reads),
            ast::Clause::Filter(condition) => self.filter_clause(condition, scope),
            ast::Clause::Let(bindings) => self.let_clause(bindings, scope),
            ast::Clause::CallSubquery(call) => self.call_subquery(call, scope),
            ast::Clause::With(with) => self.with_clause(with, scope),
            ast::Clause::Return(returned) => self.return_clause(returned, scope, output, reads),
            ast::Clause::Insert(parts) => self.insert_clause(parts, scope),
            ast::Clause::Merge(merge) => self.merge_clause(merge, scope),
            ast::Clause::Set(items) => Ok(Clause::Update(UpdateClause::Set(
                self.set_items(items, scope)?,
            ))),
            ast::Clause::Remove(items) => self.remove_clause(items, scope),
            ast::Clause::Delete(delete) => self.delete_clause(delete, later, scope),
        }
    }

    fn call_clause(
        &mut self,
        call: &ast::Call,
        scope: &mut Scope,
        standalone: bool,
        output: &mut Option<(Vec<Slot>, Vec<String>)>,
        reads: &mut bool,
    ) -> Result<Clause, Error> {
        let (bound, columns) = self.call(call, scope, standalone)?;
        if standalone {
            *reads = true;
            let slots = bound.yields.iter().map(|(_, slot)| *slot).collect();
            *output = Some((slots, columns));
        }
        Ok(Clause::Read(ReadClause::Call(Box::new(bound))))
    }

    fn return_clause(
        &mut self,
        returned: &ast::Projection,
        scope: &Scope,
        output: &mut Option<(Vec<Slot>, Vec<String>)>,
        reads: &mut bool,
    ) -> Result<Clause, Error> {
        let reads_before = self.parameter_reads;
        let (projection, _, columns) = self.projection(returned, scope, false, None)?;
        *reads = reads_variables(returned, scope) || self.parameter_reads > reads_before;
        *output = Some((projection.outputs.clone(), columns));
        Ok(Clause::Read(ReadClause::Project(Box::new(projection))))
    }

    fn delete_clause(
        &mut self,
        delete: &ast::Delete,
        later: &[ast::Clause],
        scope: &Scope,
    ) -> Result<Clause, Error> {
        let mut targets = Vec::new();
        for target in &delete.targets {
            if is_label_predicate(target) {
                return Err(error(
                    ErrorKind::InvalidDelete,
                    "DELETE takes an element, not a label expression",
                ));
            }
            self.expect_shape(
                target,
                scope,
                &[Shape::Node, Shape::Relationship, Shape::Path, Shape::List],
            );
            let bound = self.expr(target, scope, &mut Mode::Plain)?;
            if let ast::Expr::Variable(name) = target
                && self.lookup(&name.0, scope)?.kind == Kind::Value
            {
                return Err(error(
                    ErrorKind::InvalidArgumentType,
                    "DELETE takes a node, relationship, or path",
                ));
            }
            targets.push(bound);
        }
        let mut later_reads = Vec::new();
        collect_clauses_free(later, &mut Vec::new(), &mut later_reads);
        Ok(Clause::Update(UpdateClause::Delete {
            retain: later_reads.iter().any(|name| scope.get(name).is_some()),
            detach: delete.detach,
            targets,
        }))
    }

    fn unwind_clause(&mut self, unwind: &ast::Unwind, scope: &mut Scope) -> Result<Clause, Error> {
        let expr = self.expr(&unwind.expr, scope, &mut Mode::Plain)?;
        let name = &unwind.variable.0;
        if scope.get(name).is_some() {
            return Err(error(
                ErrorKind::VariableAlreadyBound,
                format!("variable `{name}` is already bound"),
            ));
        }
        let slot = self.slot();
        scope.insert(name, slot, Kind::Unknown);
        Ok(Clause::Read(ReadClause::Unwind { expr, slot }))
    }

    fn filter_clause(&mut self, condition: &ast::Expr, scope: &Scope) -> Result<Clause, Error> {
        let condition = self.filter(condition, scope, &mut Mode::Plain)?;
        Ok(Clause::Read(ReadClause::Filter(condition)))
    }

    fn let_clause(
        &mut self,
        bindings: &[(ast::Name, ast::Expr)],
        scope: &mut Scope,
    ) -> Result<Clause, Error> {
        let mut bound = Vec::new();
        let mut names: Vec<(&str, Slot, Kind)> = Vec::new();
        for (variable, value) in bindings {
            let expr = self.expr(value, scope, &mut Mode::Plain)?;
            let name = variable.0.as_str();
            if scope.get(name).is_some() || names.iter().any(|(held, ..)| *held == name) {
                return Err(error(
                    ErrorKind::VariableAlreadyBound,
                    format!("variable `{name}` is already bound"),
                ));
            }
            let slot = self.slot();
            names.push((name, slot, kind_of(value, scope)));
            bound.push((expr, slot));
        }
        for (name, slot, kind) in names {
            scope.insert(name, slot, kind);
        }
        Ok(Clause::Read(ReadClause::Let(bound)))
    }

    fn call_subquery(
        &mut self,
        call: &ast::CallSubquery,
        scope: &mut Scope,
    ) -> Result<Clause, Error> {
        let (singles, combine) = union_parts(&call.body);
        let updating = query_updates(&call.body);
        let mut branches = Vec::new();
        let mut columns: Option<Vec<String>> = None;
        for single in singles {
            if !updating && !matches!(single.clauses.last(), Some(ast::Clause::Return(_))) {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "a CALL subquery that does not update the graph must end with RETURN",
                ));
            }
            let inner = imported_scope(call.imports.as_ref(), single, scope)?;
            let (pipeline, part_columns, _) = self.single_query(single, inner, Body::Call)?;
            if columns.as_ref().is_some_and(|first| *first != part_columns) {
                return Err(error(
                    ErrorKind::DifferentColumnsInUnion,
                    "all parts of a UNION, EXCEPT, INTERSECT, or OTHERWISE must return the same columns",
                ));
            }
            columns = Some(part_columns);
            branches.push(pipeline);
        }
        let mut outputs = Vec::new();
        for name in columns.unwrap_or_default() {
            if scope.get(&name).is_some() {
                return Err(error(
                    ErrorKind::VariableAlreadyBound,
                    format!("variable `{name}` is already bound"),
                ));
            }
            let slot = self.slot();
            scope.insert(&name, slot, Kind::Unknown);
            outputs.push(slot);
        }
        if updating {
            for clause in branches.iter_mut().flat_map(|branch| &mut branch.clauses) {
                if let Clause::Update(UpdateClause::Delete { retain, .. }) = clause {
                    *retain = true;
                }
            }
            return Ok(Clause::Update(UpdateClause::CallSubquery(Box::new(
                CallSubquery {
                    optional: call.optional,
                    combine,
                    branches,
                    outputs,
                },
            ))));
        }
        let branches = branches
            .into_iter()
            .map(|branch| Pipeline {
                clauses: branch
                    .clauses
                    .into_iter()
                    .filter_map(|clause| match clause {
                        Clause::Read(clause) => Some(clause),
                        Clause::Update(_) => None,
                    })
                    .collect(),
                output: branch.output,
            })
            .collect();
        Ok(Clause::Read(ReadClause::CallSubquery(Box::new(
            CallSubquery {
                optional: call.optional,
                combine,
                branches,
                outputs,
            },
        ))))
    }

    fn with_clause(&mut self, with: &ast::With, scope: &mut Scope) -> Result<Clause, Error> {
        let (projection, next, _) =
            self.projection(&with.projection, scope, true, with.filter.as_ref())?;
        *scope = next;
        Ok(Clause::Read(ReadClause::Project(Box::new(projection))))
    }

    fn insert_clause(
        &mut self,
        parts: &[ast::PatternPart],
        scope: &mut Scope,
    ) -> Result<Clause, Error> {
        let mut introduced = Vec::new();
        let mut parts_bound = Vec::new();
        for part in parts {
            parts_bound.push(self.pattern_part(
                part,
                scope,
                PatternUse::Create,
                &mut introduced,
            )?);
        }
        Ok(Clause::Update(UpdateClause::Create(parts_bound)))
    }

    fn merge_clause(&mut self, merge: &ast::Merge, scope: &mut Scope) -> Result<Clause, Error> {
        let mut introduced = Vec::new();
        let mut pattern =
            self.pattern_part(&merge.pattern, scope, PatternUse::Merge, &mut introduced)?;
        pattern.seeks = seek::derive(&pattern, None, &introduced);
        let on_create = self.set_items(&merge.on_create, scope)?;
        let on_match = self.set_items(&merge.on_match, scope)?;
        Ok(Clause::Update(UpdateClause::Merge(Box::new(MergeClause {
            pattern,
            on_create,
            on_match,
        }))))
    }

    fn remove_clause(&mut self, items: &[ast::RemoveItem], scope: &Scope) -> Result<Clause, Error> {
        let mut actions = Vec::new();
        for item in items {
            actions.push(match item {
                ast::RemoveItem::Property { base, key } => RemoveAction::Property {
                    target: self.expr(base, scope, &mut Mode::Plain)?,
                    key: key.0.clone(),
                },
                ast::RemoveItem::Labels { variable, labels } => RemoveAction::Labels {
                    target: self.lookup(&variable.0, scope)?.slot,
                    labels: label_set(labels)?,
                },
            });
        }
        Ok(Clause::Update(UpdateClause::Remove(actions)))
    }

    fn call(
        &mut self,
        call: &ast::Call,
        scope: &mut Scope,
        standalone: bool,
    ) -> Result<(CallClause, Vec<String>), Error> {
        let name = names(&call.name).join(".");
        let callee = match Builtin::named(&name) {
            Some(builtin) => Callee::Builtin(builtin),
            None => Callee::Host(self.procedures.get(&name).cloned().ok_or_else(|| {
                error(
                    ErrorKind::ProcedureNotFound,
                    format!("procedure `{name}` is not registered"),
                )
            })?),
        };
        let signature = callee.signature();
        let args = match &call.args {
            Some(args) => {
                if args.len() != signature.parameters.len() {
                    return Err(error(
                        ErrorKind::InvalidNumberOfArguments,
                        format!(
                            "procedure `{name}` takes {} arguments, not {}",
                            signature.parameters.len(),
                            args.len()
                        ),
                    ));
                }
                let bound = self.exprs(args, scope, &mut Mode::Plain)?;
                for (arg, (_, accepts)) in args.iter().zip(&signature.parameters) {
                    self.expect_shape(arg, scope, procedure_parameter(*accepts));
                }
                bound
            }
            None if !standalone && !signature.parameters.is_empty() => {
                return Err(error(
                    ErrorKind::InvalidArgumentPassingMode,
                    format!("a CALL inside a query must pass the arguments of `{name}`"),
                ));
            }
            None => signature
                .parameters
                .iter()
                .map(|(parameter, _)| Ok(Expr::Parameter(self.parameter(parameter)?)))
                .collect::<Result<_, Error>>()?,
        };
        let all_outputs = || {
            signature
                .outputs
                .iter()
                .map(|output| ast::YieldItem {
                    output: ast::Name::new(output.as_str()),
                    alias: None,
                })
                .collect::<Vec<_>>()
        };
        let (items, filter) = match &call.yields {
            Some(ast::Yield::Items { items, filter }) => (items.clone(), filter.as_ref()),
            Some(ast::Yield::Star) if standalone => (all_outputs(), None),
            Some(ast::Yield::Star) => {
                return Err(error(
                    ErrorKind::InvalidClauseComposition,
                    "`YIELD *` is allowed only in a query that is a single `CALL`",
                ));
            }
            None if standalone => (all_outputs(), None),
            None => (Vec::new(), None),
        };
        let mut yields = Vec::new();
        let mut columns: Vec<String> = Vec::new();
        for item in &items {
            let output = &item.output.0;
            let index = signature
                .outputs
                .iter()
                .position(|candidate| candidate == output)
                .ok_or_else(|| {
                    error(
                        ErrorKind::UnknownProcedureOutput,
                        format!("procedure `{name}` has no output `{output}`"),
                    )
                })?;
            let target = item.alias.as_ref().unwrap_or(&item.output).0.as_str();
            if scope.get(target).is_some() || columns.iter().any(|column| column == target) {
                return Err(error(
                    ErrorKind::VariableAlreadyBound,
                    format!("variable `{target}` is already bound"),
                ));
            }
            yields.push((index, self.slot()));
            columns.push(target.to_owned());
        }
        for (column, (_, slot)) in columns.iter().zip(&yields) {
            scope.insert(column, *slot, Kind::Unknown);
        }
        let filter = filter
            .map(|filter| self.filter(filter, scope, &mut Mode::Plain))
            .transpose()?;
        Ok((
            CallClause {
                callee,
                args,
                yields,
                filter,
            },
            columns,
        ))
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
                    labels: label_set(labels)?,
                },
            });
        }
        Ok(actions)
    }

    fn match_clause(&mut self, found: &ast::Match, scope: &mut Scope) -> Result<ReadClause, Error> {
        let mut introduced = Vec::new();
        let mut patterns = Vec::new();
        let mut firsts = Vec::new();
        for part in &found.patterns {
            firsts.push(introduced.len());
            patterns.push(self.pattern_part(part, scope, PatternUse::Match, &mut introduced)?);
        }
        let filter = found
            .filter
            .as_ref()
            .map(|filter| self.filter(filter, scope, &mut Mode::Plain))
            .transpose()?;
        for (part, first) in patterns.iter_mut().zip(firsts) {
            part.seeks = seek::derive(part, filter.as_ref(), &introduced[first..]);
        }
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
        Ok(PatternPart {
            path,
            start,
            steps,
            seeks: Vec::new(),
        })
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
        let labels = node.labels.clone();
        if matches!(usage, PatternUse::Create | PatternUse::Merge)
            && labels
                .as_ref()
                .is_some_and(|labels| labels.label_set().is_none())
        {
            return Err(error(
                ErrorKind::NoLabelSet,
                "a node to create takes labels joined by `&` or `:` only",
            ));
        }
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
                        && (labels.is_some() || properties.is_some())
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
            if !matches!(relationship.labels, Some(ast::LabelExpr::Name(_))) {
                return Err(error(
                    ErrorKind::NoSingleRelationshipType,
                    "a relationship to create needs exactly one type",
                ));
            }
            let undirected = matches!(
                relationship.direction,
                Direction::Undirected
                    | Direction::IncomingOrUndirected
                    | Direction::UndirectedOrOutgoing
            );
            let either_way = matches!(
                relationship.direction,
                Direction::Any | Direction::IncomingOrOutgoing
            );
            if undirected || (usage == PatternUse::Create && either_way) {
                return Err(error(
                    ErrorKind::RequiresDirectedRelationship,
                    "a relationship to create needs a direction, since every stored edge has one",
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
            anonymous: relationship.variable.is_none(),
            labels: relationship.labels.clone(),
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
        let bound = self.expr(filter, scope, mode)?;
        self.expect_shape(filter, scope, &[Shape::Boolean]);
        Ok(bound)
    }

    fn expect_shape(&mut self, expr: &ast::Expr, scope: &Scope, accepted: &[Shape]) {
        self.expect_argument(expr, scope, accepted, None);
    }

    fn expect_argument(
        &mut self,
        expr: &ast::Expr,
        scope: &Scope,
        accepted: &[Shape],
        kind: Option<ErrorKind>,
    ) {
        if let Some(shape) = self.shape(expr, scope)
            && !admits(accepted, shape)
        {
            self.reject(Some(kind.unwrap_or(ErrorKind::InvalidArgumentType)), || {
                format!("a {shape:?} cannot be used here; expected one of {accepted:?}")
            });
        }
    }

    fn reject(&mut self, kind: Option<ErrorKind>, message: impl FnOnce() -> String) {
        let Some(kind) = kind else {
            return;
        };
        if self.type_error.is_none() {
            let kind = match kind {
                ErrorKind::TypeMismatch => ErrorKind::InvalidArgumentType,
                kind => kind,
            };
            self.type_error = Some(error(kind, message()));
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
            | ast::Expr::PatternComprehension(_)
            | ast::Expr::Collect(_) => Some(Shape::List),
            ast::Expr::Count(_) => Some(Shape::Integer),
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
                ast::BinaryOp::Concat => {
                    let lhs = self.shape(lhs, scope)?;
                    let rhs = self.shape(rhs, scope)?;
                    (lhs == rhs && matches!(lhs, Shape::String | Shape::List)).then_some(lhs)
                }
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

    fn parameter(&mut self, name: &str) -> Result<usize, Error> {
        self.parameter_reads += 1;
        if self.parameters.get(name).is_none() {
            return Err(missing_parameter(name));
        }
        Ok(
            match self.parameter_names.iter().position(|known| known == name) {
                Some(index) => index,
                None => {
                    self.parameter_names.push(name.to_owned());
                    self.parameter_names.len() - 1
                }
            },
        )
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
        let missing_alias = is_with
            && explicit
                .iter()
                .any(|item| item.alias.is_none() && !matches!(item.expr, ast::Expr::Variable(_)));
        for item in explicit {
            items.push((item.expr.clone(), self.column_name(item)));
        }
        let mut columns: Vec<String> = Vec::new();
        let mut repeated = None;
        for (_, name) in &items {
            if columns.contains(name) {
                repeated.get_or_insert_with(|| name.clone());
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
        if missing_alias {
            return Err(error(
                ErrorKind::NoExpressionAlias,
                "an expression in WITH needs an alias",
            ));
        }
        if let Some(name) = repeated {
            return Err(error(
                ErrorKind::ColumnNameConflict,
                format!("column `{name}` is projected twice"),
            ));
        }
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

    fn root<R>(&mut self, bind: impl FnOnce(&mut Self) -> R) -> R {
        let enclosing = std::mem::replace(&mut self.expression_depth, 0);
        let bound = bind(self);
        self.expression_depth = enclosing;
        bound
    }

    fn open_expr(
        &mut self,
        expr: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        if let Some(bound) = self.projected_slot(expr, scope, mode)? {
            return Ok(bound);
        }
        match expr {
            ast::Expr::Literal(literal) => Ok(Expr::Constant(literal_value(literal))),
            ast::Expr::Parameter(name) => Ok(Expr::Parameter(self.parameter(&name.0)?)),
            ast::Expr::Variable(name) => self.variable(name, scope, mode),
            ast::Expr::List(items) => Ok(Expr::List(self.exprs(items, scope, mode)?)),
            ast::Expr::Map(entries) => self.map(entries, scope, mode),
            ast::Expr::Property { base, key } => self.property(base, key, scope, mode),
            ast::Expr::Index { base, index } => self.index(base, index, scope, mode),
            ast::Expr::Slice { base, from, to } => {
                self.slice(base, from.as_deref(), to.as_deref(), scope, mode)
            }
            ast::Expr::HasLabels { operand, labels } => {
                self.has_labels(operand, labels, scope, mode)
            }
            ast::Expr::Unary { op, operand } => self.unary(*op, operand, scope, mode),
            ast::Expr::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, scope, mode),
            ast::Expr::Comparison { first, rest } => self.comparison(first, rest, scope, mode),
            ast::Expr::IsNull { operand, negated } => Ok(Expr::IsNull {
                operand: self.boxed(operand, scope, mode)?,
                negated: *negated,
            }),
            ast::Expr::Function(call) => self.function(call, scope, mode),
            ast::Expr::CountStar => Err(aggregate_not_allowed(mode)),
            ast::Expr::Case(case) => self.case(case, scope, mode),
            ast::Expr::ListComprehension(comprehension) => {
                self.list_comprehension(comprehension, scope, mode)
            }
            ast::Expr::ListPredicate(predicate) => self.list_predicate(predicate, scope, mode),
            ast::Expr::Pattern(element) => self.pattern_predicate(element, scope, mode),
            ast::Expr::Exists(subquery) => self.subquery(subquery, false, scope, mode),
            ast::Expr::Count(subquery) => self.subquery(subquery, true, scope, mode),
            ast::Expr::Collect(query) => self.collect(query, scope, mode),
            ast::Expr::Reduce(reduce) => self.reduce(reduce, scope, mode),
            ast::Expr::PatternComprehension(comprehension) => {
                self.pattern_comprehension(comprehension, scope, mode)
            }
        }
    }

    fn projected_slot(
        &mut self,
        expr: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Option<Expr>, Error> {
        if let Mode::Projected(projected) = mode
            && let Some((_, slot)) = projected
                .items
                .iter()
                .find(|(item, _)| same_meaning(item, projected.scope, expr, scope))
        {
            return Ok(Some(Expr::Slot(*slot)));
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
                return Ok(Some(Expr::Slot(*slot)));
            }
            if let Some((function, distinct, args)) = aggregate_call(expr) {
                if grouping.ordering {
                    let registered = self.closed.len();
                    self.aggregate(function, distinct, args, scope)?;
                    self.closed.truncate(registered);
                }
                let aggregate = self.aggregate(function, distinct, args, &grouping.input)?;
                let slot = self.slot();
                grouping.aggregates.push((aggregate, slot));
                return Ok(Some(Expr::Slot(slot)));
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
        Ok(None)
    }

    fn variable(
        &mut self,
        name: &ast::Name,
        scope: &Scope,
        mode: &Mode<'_>,
    ) -> Result<Expr, Error> {
        if matches!(mode, Mode::Constant) && scope.get(&name.0).is_none() {
            return Err(error(
                ErrorKind::NonConstantExpression,
                "SKIP and LIMIT cannot refer to variables",
            ));
        }
        Ok(Expr::Slot(self.lookup(&name.0, scope)?.slot))
    }

    fn map(
        &mut self,
        entries: &[(ast::Name, ast::Expr)],
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let mut bound = Vec::new();
        for (key, value) in entries {
            bound.push((key.0.clone(), self.expr(value, scope, mode)?));
        }
        Ok(Expr::Map(bound))
    }

    fn property(
        &mut self,
        base: &ast::Expr,
        key: &ast::Name,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let bound = self.boxed(base, scope, mode)?;
        self.expect_shape(base, scope, &[Shape::Node, Shape::Relationship, Shape::Map]);
        Ok(Expr::Property(bound, key.0.clone()))
    }

    fn index(
        &mut self,
        base: &ast::Expr,
        index: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let bound_base = self.boxed(base, scope, mode)?;
        let bound_index = self.boxed(index, scope, mode)?;
        if let (Some(base), Some(index)) = (self.shape(base, scope), self.shape(index, scope)) {
            self.reject(index_error(base, index), || {
                format!("a {base:?} cannot be indexed with a {index:?}")
            });
        }
        Ok(Expr::Index(bound_base, bound_index))
    }

    fn slice(
        &mut self,
        base: &ast::Expr,
        from: Option<&ast::Expr>,
        to: Option<&ast::Expr>,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let bound_base = self.boxed(base, scope, mode)?;
        let bound_from = match from {
            Some(from) => Some(self.boxed(from, scope, mode)?),
            None => None,
        };
        let bound_to = match to {
            Some(to) => Some(self.boxed(to, scope, mode)?),
            None => None,
        };
        if let Some(shape) = self.shape(base, scope) {
            self.reject(slice_error(shape), || {
                format!("a {shape:?} cannot be sliced")
            });
            for bound in [from, to].into_iter().flatten() {
                if let Some(bound) = self.shape(bound, scope) {
                    self.reject(slice_bound_error(bound), || {
                        format!("a slice bound cannot be a {bound:?}")
                    });
                }
            }
        }
        Ok(Expr::Slice(bound_base, bound_from, bound_to))
    }

    fn has_labels(
        &mut self,
        operand: &ast::Expr,
        labels: &ast::LabelExpr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let bound = self.boxed(operand, scope, mode)?;
        if let Some(shape) = self.shape(operand, scope) {
            self.reject(label_test_error(shape), || {
                format!("labels cannot be tested on a {shape:?}")
            });
        }
        Ok(Expr::HasLabels(bound, labels.clone()))
    }

    fn unary(
        &mut self,
        op: ast::UnaryOp,
        ast_operand: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let operand = self.boxed(ast_operand, scope, mode)?;
        if let Some(shape) = self.shape(ast_operand, scope) {
            self.reject(unary_error(op, shape), || {
                format!("{op:?} cannot take a {shape:?}")
            });
        }
        Ok(match op {
            ast::UnaryOp::Not => Expr::Not(operand),
            ast::UnaryOp::Negate => Expr::Negate(operand),
            ast::UnaryOp::Plus => Expr::UnaryPlus(operand),
        })
    }

    fn binary(
        &mut self,
        op: ast::BinaryOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let bound_lhs = self.boxed(lhs, scope, mode)?;
        let bound_rhs = self.boxed(rhs, scope, mode)?;
        if let (Some(left), Some(right)) = (self.shape(lhs, scope), self.shape(rhs, scope)) {
            self.reject(binary_error(op, left, right), || {
                format!("{op:?} cannot take a {left:?} and a {right:?}")
            });
        }
        Ok(Expr::Binary(op, bound_lhs, bound_rhs))
    }

    fn comparison(
        &mut self,
        first: &ast::Expr,
        rest: &[(ast::ComparisonOp, ast::Expr)],
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let first = self.boxed(first, scope, mode)?;
        let mut chain = Vec::new();
        for (op, operand) in rest {
            chain.push((*op, self.expr(operand, scope, mode)?));
        }
        Ok(Expr::Comparison(first, chain))
    }

    fn function(
        &mut self,
        call: &ast::FunctionCall,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
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
        let bound = self.exprs(&call.args, scope, mode)?;
        for (index, arg) in call.args.iter().enumerate() {
            self.expect_argument(
                arg,
                scope,
                function.parameter(index),
                function.argument_error(),
            );
        }
        Ok(Expr::Call(function, bound))
    }

    fn case(
        &mut self,
        case: &ast::Case,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let plain = case
            .branches
            .iter()
            .all(|branch| matches!(branch.when.as_slice(), [ast::WhenOperand::Value(_)]));
        if !plain && let Some(scrutinee) = &case.scrutinee {
            return self.tested_case(scrutinee, case, scope, mode);
        }
        let scrutinee = match &case.scrutinee {
            Some(scrutinee) => Some(self.expr(scrutinee, scope, mode)?),
            None => None,
        };
        let mut branches = Vec::new();
        for branch in &case.branches {
            let [ast::WhenOperand::Value(condition)] = branch.when.as_slice() else {
                continue;
            };
            let when = self.expr(condition, scope, mode)?;
            if case.scrutinee.is_none() {
                self.expect_shape(condition, scope, &[Shape::Boolean]);
            }
            branches.push((when, self.expr(&branch.then, scope, mode)?));
        }
        let otherwise = match &case.otherwise {
            Some(otherwise) => Some(self.expr(otherwise, scope, mode)?),
            None => None,
        };
        Ok(Expr::Case(Box::new(Case {
            scrutinee,
            branches,
            otherwise,
        })))
    }

    fn tested_case(
        &mut self,
        scrutinee: &ast::Expr,
        case: &ast::Case,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let source = self.expr(scrutinee, scope, mode)?;
        let slot = self.slot();
        let held = || Box::new(Expr::Slot(slot));
        let mut branches = Vec::new();
        for branch in &case.branches {
            let mut condition: Option<Expr> = None;
            for operand in &branch.when {
                let test = match operand {
                    ast::WhenOperand::Value(value) => Expr::Comparison(
                        held(),
                        vec![(ast::ComparisonOp::Equal, self.expr(value, scope, mode)?)],
                    ),
                    ast::WhenOperand::Compare(op, value) => {
                        Expr::Comparison(held(), vec![(*op, self.expr(value, scope, mode)?)])
                    }
                    ast::WhenOperand::IsNull { negated } => Expr::IsNull {
                        operand: held(),
                        negated: *negated,
                    },
                };
                condition = Some(match condition {
                    Some(earlier) => {
                        Expr::Binary(ast::BinaryOp::Or, Box::new(earlier), Box::new(test))
                    }
                    None => test,
                });
            }
            if let Some(condition) = condition {
                branches.push((condition, self.expr(&branch.then, scope, mode)?));
            }
        }
        let otherwise = match &case.otherwise {
            Some(otherwise) => Some(self.expr(otherwise, scope, mode)?),
            None => None,
        };
        let projection = Expr::Case(Box::new(Case {
            scrutinee: None,
            branches,
            otherwise,
        }));
        Ok(Expr::Index(
            Box::new(Expr::ListComprehension(Box::new(ListComprehension {
                slot,
                source: Expr::List(vec![source]),
                filter: None,
                projection: Some(projection),
            }))),
            Box::new(Expr::Constant(Datum::Integer(0))),
        ))
    }

    fn list_comprehension(
        &mut self,
        comprehension: &ast::ListComprehension,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
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
        Ok(Expr::ListComprehension(Box::new(ListComprehension {
            slot,
            source,
            filter,
            projection,
        })))
    }

    fn list_predicate(
        &mut self,
        predicate: &ast::ListPredicate,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        if contains_aggregate(&predicate.predicate) {
            return Err(per_element_aggregate());
        }
        let source = self.expr(&predicate.source, scope, mode)?;
        let slot = self.slot();
        let mut inner = scope.clone();
        inner.insert(&predicate.variable.0, slot, Kind::Unknown);
        let condition = self.filter(&predicate.predicate, &inner, mode)?;
        Ok(Expr::Quantified(Box::new(Quantified {
            quantifier: predicate.quantifier,
            slot,
            source,
            predicate: condition,
        })))
    }

    fn pattern_predicate(
        &mut self,
        element: &ast::PatternElement,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let scope = &self.pattern_scope(scope, names_of(pattern_variables(element)), mode)?;
        if let Some(name) = pattern_variables(element).find(|name| scope.get(&name.0).is_none()) {
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
        Ok(Expr::Exists(Box::new(self.pattern_query(
            std::slice::from_ref(&part),
            None,
            scope,
            mode,
        )?)))
    }

    fn subquery(
        &mut self,
        subquery: &ast::Subquery,
        count: bool,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        match subquery {
            ast::Subquery::Pattern { patterns, filter } => {
                let names = patterns
                    .iter()
                    .flat_map(|part| pattern_variables(&part.element));
                let scope = &self.pattern_scope(scope, names_of(names), mode)?;
                let query = Box::new(self.pattern_query(patterns, filter.as_ref(), scope, mode)?);
                Ok(if count {
                    Expr::CountMatches(query)
                } else {
                    Expr::Exists(query)
                })
            }
            ast::Subquery::Query(query) => {
                let kind = if count {
                    SubqueryKind::Count
                } else {
                    SubqueryKind::Exists
                };
                let (clauses, _) = self.subquery_clauses(query, scope, mode)?;
                Ok(Expr::Subquery(kind, clauses))
            }
        }
    }

    fn collect(
        &mut self,
        query: &ast::SingleQuery,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        let (clauses, output) = self.subquery_clauses(query, scope, mode)?;
        match output.as_slice() {
            [slot] => Ok(Expr::Subquery(SubqueryKind::Collect(*slot), clauses)),
            _ => Err(error(
                ErrorKind::InvalidClauseComposition,
                "a COLLECT subquery must end with a RETURN of one column",
            )),
        }
    }

    fn subquery_clauses(
        &mut self,
        query: &ast::SingleQuery,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<(Vec<ReadClause>, Vec<Slot>), Error> {
        let mut free = Vec::new();
        collect_clauses_free(&query.clauses, &mut Vec::new(), &mut free);
        let scope = self.pattern_scope(scope, free.into_iter(), mode)?;
        let updates_allowed = std::mem::replace(&mut self.updates_allowed, false);
        let bound = self.root(|binder| binder.single_query(query, scope, Body::Expression));
        self.updates_allowed = updates_allowed;
        let (pipeline, _, _) = bound?;
        let clauses = pipeline
            .clauses
            .into_iter()
            .filter_map(|clause| match clause {
                Clause::Read(clause) => Some(clause),
                Clause::Update(_) => None,
            })
            .collect();
        Ok((clauses, pipeline.output))
    }

    fn reduce(
        &mut self,
        reduce: &ast::Reduce,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
        if contains_aggregate(&reduce.body) {
            return Err(per_element_aggregate());
        }
        if reduce.accumulator == reduce.variable {
            return Err(error(
                ErrorKind::VariableAlreadyBound,
                format!("variable `{}` is already bound", reduce.variable.0),
            ));
        }
        let init = self.expr(&reduce.init, scope, mode)?;
        let source = self.expr(&reduce.source, scope, mode)?;
        let accumulator = self.slot();
        let slot = self.slot();
        let mut inner = scope.clone();
        inner.insert(&reduce.accumulator.0, accumulator, Kind::Unknown);
        inner.insert(&reduce.variable.0, slot, Kind::Unknown);
        let body = self.expr(&reduce.body, &inner, mode)?;
        Ok(Expr::Reduce(Box::new(Reduce {
            accumulator,
            init,
            slot,
            source,
            body,
        })))
    }

    fn pattern_comprehension(
        &mut self,
        comprehension: &ast::PatternComprehension,
        scope: &Scope,
        mode: &mut Mode<'_>,
    ) -> Result<Expr, Error> {
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
            self.pattern_scope(scope, names_of(pattern_variables(&part.element)), mode)?;
        let mut introduced = Vec::new();
        let pattern =
            self.pattern_part(&part, &mut inner, PatternUse::Predicate, &mut introduced)?;
        let filter = match &comprehension.filter {
            Some(filter) => Some(self.expr(filter, &inner, mode)?),
            None => None,
        };
        let projection = self.expr(&comprehension.projection, &inner, mode)?;
        Ok(Expr::PatternComprehension(
            Box::new(PatternQuery {
                patterns: vec![pattern],
                filter,
            }),
            Box::new(projection),
        ))
    }

    fn pattern_scope<'n>(
        &self,
        scope: &Scope,
        names: impl Iterator<Item = &'n str>,
        mode: &Mode<'_>,
    ) -> Result<Scope, Error> {
        let mut visible = scope.clone();
        let hidden = |name: &str| match mode {
            Mode::Grouped(grouping) => grouping.input.get(name).map(|_| {
                if grouping.ordering {
                    ErrorKind::UndefinedVariable
                } else {
                    ErrorKind::AmbiguousAggregationExpression
                }
            }),
            Mode::Projected(projected) => projected
                .scope
                .get(name)
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
            if visible.get(name).is_none()
                && let Some(kind) = hidden(name)
            {
                return Err(error(
                    kind,
                    format!("`{name}` is not visible next to this projection"),
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
            if calls_rand(arg) {
                return Err(error(
                    ErrorKind::NonConstantExpression,
                    "an aggregate cannot take a random value",
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
        let (argument, extra) = self.root(|binder| {
            let argument = match args.first() {
                Some(arg) => Some(binder.expr(arg, input, &mut Mode::Plain)?),
                None => None,
            };
            let extra = match args.get(1) {
                Some(arg) => Some(binder.expr(arg, input, &mut Mode::Plain)?),
                None => None,
            };
            Ok::<_, Error>((argument, extra))
        })?;
        for arg in args {
            self.expect_shape(arg, input, function.accepted());
        }
        for bound in argument.iter().chain(&extra) {
            if is_closed(bound) {
                self.closed
                    .push(Expr::Admit(function, Box::new(bound.clone())));
            }
        }
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

#[derive(Default)]
struct Parts<'a> {
    always: Vec<&'a Expr>,
    sometimes: Vec<&'a Expr>,
    bound: Vec<Slot>,
    reads_graph: bool,
}

impl<'a> Parts<'a> {
    fn always(always: Vec<&'a Expr>) -> Self {
        Parts {
            always,
            ..Parts::default()
        }
    }

    fn matching(query: &'a PatternQuery, projection: Option<&'a Expr>) -> Self {
        let properties = query.patterns.iter().flat_map(|part| {
            std::iter::once(&part.start.properties)
                .chain(
                    part.steps
                        .iter()
                        .flat_map(|step| [&step.relationship.properties, &step.node.properties]),
                )
                .flatten()
        });
        Parts {
            sometimes: properties.chain(&query.filter).chain(projection).collect(),
            reads_graph: true,
            ..Parts::default()
        }
    }
}

fn evaluated_parts(expr: &Expr) -> Option<Parts<'_>> {
    Some(match expr {
        Expr::List(items) | Expr::Call(_, items) => Parts::always(items.iter().collect()),
        Expr::Map(entries) => Parts::always(entries.iter().map(|(_, value)| value).collect()),
        Expr::Admit(_, operand)
        | Expr::Property(operand, _)
        | Expr::HasLabels(operand, _)
        | Expr::Not(operand)
        | Expr::Negate(operand)
        | Expr::UnaryPlus(operand)
        | Expr::IsNull { operand, .. } => Parts::always(vec![operand]),
        Expr::Index(base, index) | Expr::Binary(_, base, index) => Parts::always(vec![base, index]),
        Expr::Slice(base, from, to) => Parts::always(
            std::iter::once(&**base)
                .chain([from, to].into_iter().flatten().map(|bound| &**bound))
                .collect(),
        ),
        Expr::Comparison(first, rest) => Parts::always(
            std::iter::once(&**first)
                .chain(rest.iter().map(|(_, operand)| operand))
                .collect(),
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
            Parts {
                always,
                sometimes,
                ..Parts::default()
            }
        }
        Expr::ListComprehension(comprehension) => Parts {
            always: vec![&comprehension.source],
            sometimes: comprehension
                .filter
                .iter()
                .chain(&comprehension.projection)
                .collect(),
            bound: vec![comprehension.slot],
            ..Parts::default()
        },
        Expr::Quantified(quantified) => Parts {
            always: vec![&quantified.source],
            sometimes: vec![&quantified.predicate],
            bound: vec![quantified.slot],
            ..Parts::default()
        },
        Expr::Reduce(reduce) => Parts {
            always: vec![&reduce.init, &reduce.source],
            sometimes: vec![&reduce.body],
            bound: vec![reduce.accumulator, reduce.slot],
            ..Parts::default()
        },
        Expr::Exists(query) | Expr::CountMatches(query) => Parts::matching(query, None),
        Expr::Subquery(..) => Parts {
            reads_graph: true,
            ..Parts::default()
        },
        Expr::PatternComprehension(query, projection) => {
            Parts::matching(query, Some(&**projection))
        }
        Expr::Constant(_) | Expr::Parameter(_) | Expr::Slot(_) => return None,
    })
}

fn collect_closed<'a>(
    expr: &'a Expr,
    local: &mut Vec<Slot>,
    out: &mut Vec<&'a Expr>,
) -> Option<usize> {
    let base = local.len();
    let Some(parts) = evaluated_parts(expr) else {
        return match expr {
            Expr::Constant(_) => Some(usize::MAX),
            Expr::Slot(slot) => local.iter().rposition(|bound| bound == slot),
            _ => None,
        };
    };
    let mark = out.len();
    let mut outermost = (!parts.reads_graph).then_some(usize::MAX);
    for child in parts.always {
        outermost = join(outermost, collect_closed(child, local, out));
    }
    let covered = out.len();
    let depth = local.len();
    local.extend(parts.bound);
    for child in parts.sometimes {
        outermost = join(outermost, collect_closed(child, local, out));
    }
    local.truncate(depth);
    if outermost.is_some_and(|reach| reach >= base) {
        out.drain(mark..covered);
        out.push(expr);
    }
    outermost
}

fn is_closed(expr: &Expr) -> bool {
    collect_closed(expr, &mut Vec::new(), &mut Vec::new()).is_some()
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

fn label_set(labels: &ast::LabelExpr) -> Result<Vec<String>, Error> {
    match labels.label_set() {
        Some(names) => Ok(names.into_iter().map(|name| name.0.clone()).collect()),
        None => Err(error(
            ErrorKind::NoLabelSet,
            "SET and REMOVE take labels joined by `&` or `:` only",
        )),
    }
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

fn is_label_predicate(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::HasLabels { .. } => true,
        ast::Expr::Unary {
            op: ast::UnaryOp::Not,
            operand,
        } => is_label_predicate(operand),
        ast::Expr::Binary {
            op: ast::BinaryOp::And | ast::BinaryOp::Or | ast::BinaryOp::Xor,
            lhs,
            rhs,
        } => is_label_predicate(lhs) || is_label_predicate(rhs),
        _ => false,
    }
}

fn reads_variables(projection: &ast::Projection, scope: &Scope) -> bool {
    match &projection.items {
        ast::ProjectionItems::Star(_) => true,
        ast::ProjectionItems::Items(items) => items.iter().any(|item| {
            free_variables(&item.expr)
                .iter()
                .any(|name| scope.get(name).is_some())
        }),
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
        ast::Expr::Variable(name) => collect_free_name(name, bound, free),
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
        ast::Expr::Reduce(reduce) => {
            collect_free(&reduce.init, bound, free);
            collect_free(&reduce.source, bound, free);
            bound.push(&reduce.accumulator.0);
            bound.push(&reduce.variable.0);
            collect_free(&reduce.body, bound, free);
            bound.truncate(bound.len() - 2);
        }
        ast::Expr::Collect(query) => collect_clauses_free(&query.clauses, bound, free),
        ast::Expr::Exists(subquery) | ast::Expr::Count(subquery) => match subquery.as_ref() {
            ast::Subquery::Pattern { patterns, filter } => {
                for part in patterns {
                    collect_pattern_free(&part.element, bound, free);
                }
                if let Some(filter) = filter {
                    collect_free(filter, bound, free);
                }
            }
            ast::Subquery::Query(query) => collect_clauses_free(&query.clauses, bound, free),
        },
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

fn names_of<'n>(names: impl Iterator<Item = &'n ast::Name>) -> impl Iterator<Item = &'n str> {
    names.map(|name| name.0.as_str())
}

fn collect_clauses_free<'a>(
    clauses: &'a [ast::Clause],
    bound: &mut Vec<&'a str>,
    free: &mut Vec<&'a str>,
) {
    let depth = bound.len();
    for clause in clauses {
        match clause {
            ast::Clause::Match(clause) => {
                collect_patterns_free(&clause.patterns, bound, free);
                if let Some(filter) = &clause.filter {
                    collect_free(filter, bound, free);
                }
            }
            ast::Clause::Insert(patterns) => collect_patterns_free(patterns, bound, free),
            ast::Clause::Merge(merge) => {
                collect_patterns_free(std::slice::from_ref(&merge.pattern), bound, free);
                for item in merge.on_create.iter().chain(&merge.on_match) {
                    collect_set_item_free(item, bound, free);
                }
            }
            ast::Clause::Set(items) => {
                for item in items {
                    collect_set_item_free(item, bound, free);
                }
            }
            ast::Clause::Remove(items) => {
                for item in items {
                    match item {
                        ast::RemoveItem::Property { base, .. } => collect_free(base, bound, free),
                        ast::RemoveItem::Labels { variable, .. } => {
                            collect_free_name(variable, bound, free);
                        }
                    }
                }
            }
            ast::Clause::Delete(delete) => {
                for target in &delete.targets {
                    collect_free(target, bound, free);
                }
            }
            ast::Clause::Unwind(unwind) => {
                collect_free(&unwind.expr, bound, free);
                bound.push(&unwind.variable.0);
            }
            ast::Clause::Filter(condition) => collect_free(condition, bound, free),
            ast::Clause::CallSubquery(call) => {
                let (singles, _) = union_parts(&call.body);
                for single in &singles {
                    match &call.imports {
                        Some(ast::Imports::Variables(names)) => {
                            for name in names {
                                collect_free_name(name, bound, free);
                            }
                        }
                        _ => collect_clauses_free(&single.clauses, bound, free),
                    }
                }
                if let Some(ast::Clause::Return(projection)) =
                    singles.first().and_then(|single| single.clauses.last())
                    && let ast::ProjectionItems::Items(items) = &projection.items
                {
                    bound.extend(
                        items
                            .iter()
                            .filter_map(|item| match (&item.alias, &item.expr) {
                                (Some(alias), _) | (None, ast::Expr::Variable(alias)) => {
                                    Some(alias.0.as_str())
                                }
                                _ => None,
                            }),
                    );
                }
            }
            ast::Clause::Let(bindings) => {
                for (_, value) in bindings {
                    collect_free(value, bound, free);
                }
                bound.extend(bindings.iter().map(|(variable, _)| variable.0.as_str()));
            }
            ast::Clause::Call(call) => {
                for arg in call.args.iter().flatten() {
                    collect_free(arg, bound, free);
                }
                if let Some(ast::Yield::Items { items, filter }) = &call.yields {
                    bound.extend(
                        items
                            .iter()
                            .map(|item| item.alias.as_ref().unwrap_or(&item.output).0.as_str()),
                    );
                    if let Some(filter) = filter {
                        collect_free(filter, bound, free);
                    }
                }
            }
            ast::Clause::With(with) => {
                if !collect_projection_free(&with.projection, with.filter.as_ref(), bound, free) {
                    break;
                }
            }
            ast::Clause::Return(projection) => {
                collect_projection_free(projection, None, bound, free);
                break;
            }
        }
    }
    bound.truncate(depth);
}

fn collect_patterns_free<'a>(
    patterns: &'a [ast::PatternPart],
    bound: &mut Vec<&'a str>,
    free: &mut Vec<&'a str>,
) {
    for part in patterns {
        collect_pattern_free(&part.element, bound, free);
    }
    for part in patterns {
        bound.extend(names_of(pattern_variables(&part.element)));
        bound.extend(names_of(part.path_variable.iter()));
    }
}

fn collect_set_item_free<'a>(
    item: &'a ast::SetItem,
    bound: &mut Vec<&'a str>,
    free: &mut Vec<&'a str>,
) {
    match item {
        ast::SetItem::Property { base, value, .. } => {
            collect_free(base, bound, free);
            collect_free(value, bound, free);
        }
        ast::SetItem::ReplaceProperties { variable, value }
        | ast::SetItem::MergeProperties { variable, value } => {
            collect_free_name(variable, bound, free);
            collect_free(value, bound, free);
        }
        ast::SetItem::Labels { variable, .. } => collect_free_name(variable, bound, free),
    }
}

fn collect_free_name<'a>(name: &'a ast::Name, bound: &[&'a str], free: &mut Vec<&'a str>) {
    if !bound.contains(&name.0.as_str()) {
        free.push(&name.0);
    }
}

fn collect_projection_free<'a>(
    projection: &'a ast::Projection,
    filter: Option<&'a ast::Expr>,
    bound: &mut Vec<&'a str>,
    free: &mut Vec<&'a str>,
) -> bool {
    let (items, keeps_scope) = match &projection.items {
        ast::ProjectionItems::Star(items) => (items, true),
        ast::ProjectionItems::Items(items) => (items, false),
    };
    for item in items {
        collect_free(&item.expr, bound, free);
    }
    bound.extend(
        items
            .iter()
            .filter_map(|item| match (&item.alias, &item.expr) {
                (Some(alias), _) => Some(alias.0.as_str()),
                (None, ast::Expr::Variable(name)) => Some(name.0.as_str()),
                (None, _) => None,
            }),
    );
    let rest = projection
        .order_by
        .iter()
        .map(|item| &item.expr)
        .chain(&projection.skip)
        .chain(&projection.limit)
        .chain(filter);
    for expr in rest {
        collect_free(expr, bound, free);
    }
    keeps_scope
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

fn calls_rand(expr: &ast::Expr) -> bool {
    matches!(expr, ast::Expr::Function(call) if Function::lookup(&qualified_name(call)) == Some(Function::Rand))
        || children(expr).into_iter().any(calls_rand)
}

fn children(expr: &ast::Expr) -> Vec<&ast::Expr> {
    match expr {
        ast::Expr::Literal(_)
        | ast::Expr::Parameter(_)
        | ast::Expr::Variable(_)
        | ast::Expr::CountStar
        | ast::Expr::Pattern(_)
        | ast::Expr::Exists(_)
        | ast::Expr::Count(_)
        | ast::Expr::Collect(_) => Vec::new(),
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
            .chain(case.branches.iter().flat_map(|branch| {
                branch
                    .when
                    .iter()
                    .filter_map(ast::WhenOperand::expr)
                    .chain(std::iter::once(&branch.then))
            }))
            .chain(case.otherwise.as_deref())
            .collect(),
        ast::Expr::ListComprehension(comprehension) => vec![&comprehension.source],
        ast::Expr::ListPredicate(predicate) => vec![&predicate.source],
        ast::Expr::Reduce(reduce) => vec![&reduce.init, &reduce.source],
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
        | ast::Expr::PatternComprehension(_)
        | ast::Expr::Collect(_) => Kind::List,
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
        | ast::Expr::Count(_)
        | ast::Expr::CountStar => Kind::Value,
        _ => Kind::Unknown,
    }
}

fn literal_value(literal: &ast::Literal) -> Datum {
    match literal {
        ast::Literal::Integer(value) => Datum::Integer(*value),
        ast::Literal::Float(value) => Datum::Float(*value),
        ast::Literal::String(value) => Datum::String(value.clone()),
        ast::Literal::Boolean(value) => Datum::Boolean(*value),
        ast::Literal::Null => Datum::Null,
    }
}

#[cfg(test)]
#[path = "bind.test.rs"]
mod tests;
