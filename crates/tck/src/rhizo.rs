use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rhizodb::Database;
use rhizodb::ErrorKind;
use rhizodb::SyntaxErrorKind;
use rhizodb::ValueType;

use crate::engine::Engine;
use crate::engine::Graph;
use crate::engine::QueryError;
use crate::engine::Rows;
use crate::scenario::Procedure;
use crate::side_effects::Entity;
use crate::side_effects::GraphSnapshot;
use crate::side_effects::PropertyEntry;
use crate::signature::parse_signature;
use crate::value::Direction;
use crate::value::Node;
use crate::value::Path;
use crate::value::PathStep;
use crate::value::Properties;
use crate::value::Relationship;
use crate::value::Value;

/// Runs every scenario against a fresh in-memory RhizoDB.
#[derive(Clone, Copy, Debug, Default)]
pub struct RhizoDb;

impl Engine for RhizoDb {
    fn fresh_graph(&self) -> Box<dyn Graph> {
        match Database::in_memory() {
            Ok(database) => Box::new(RhizoGraph { database }),
            Err(_) => Box::new(BrokenGraph),
        }
    }
}

#[derive(Debug)]
struct RhizoGraph {
    database: Database,
}

#[derive(Debug)]
struct BrokenGraph;

impl Graph for BrokenGraph {
    fn register_procedure(&mut self, _procedure: &Procedure) -> Result<(), String> {
        Ok(())
    }

    fn execute(
        &mut self,
        _query: &str,
        _parameters: &BTreeMap<String, Value>,
    ) -> Result<Rows, QueryError> {
        Err(QueryError::Unsupported)
    }

    fn snapshot(&self) -> Result<GraphSnapshot, String> {
        Ok(GraphSnapshot::default())
    }
}

impl Graph for RhizoGraph {
    fn register_procedure(&mut self, procedure: &Procedure) -> Result<(), String> {
        self.database
            .register_procedure(table_procedure(procedure)?);
        Ok(())
    }

    fn execute(
        &mut self,
        query: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Rows, QueryError> {
        let parameters = parameters
            .iter()
            .map(|(name, value)| Ok((name.clone(), to_rhizo(value)?)))
            .collect::<Result<_, QueryError>>()?;
        let result = self
            .database
            .execute(query, &parameters)
            .map_err(|error| query_error(error.kind()))?;
        let rows = result
            .rows()
            .iter()
            .map(|row| row.iter().map(from_rhizo).collect())
            .collect::<Result<_, _>>()?;
        Ok(Rows {
            columns: result.columns().to_vec(),
            rows,
        })
    }

    fn snapshot(&self) -> Result<GraphSnapshot, String> {
        let mut snapshot = GraphSnapshot::default();
        let mut labels = BTreeSet::new();
        for value in self.read_all("MATCH (n) RETURN n")? {
            if let rhizodb::Value::Node(node) = value {
                let id = node.id.as_u64();
                snapshot.nodes.push(id);
                labels.extend(node.labels.iter().cloned());
                record_properties(&mut snapshot, Entity::Node(id), &node.properties)?;
            }
        }
        for value in self.read_all("MATCH ()-[r]->() RETURN r")? {
            if let rhizodb::Value::Relationship(relationship) = value {
                let id = relationship.id.as_u64();
                snapshot.relationships.push(id);
                record_properties(
                    &mut snapshot,
                    Entity::Relationship(id),
                    &relationship.properties,
                )?;
            }
        }
        snapshot.labels = labels.into_iter().collect();
        Ok(snapshot)
    }
}

impl RhizoGraph {
    fn read_all(&self, query: &str) -> Result<Vec<rhizodb::Value>, String> {
        let result = self
            .database
            .execute(query, &rhizodb::Parameters::new())
            .map_err(|error| format!("{query}: {error}"))?;
        Ok(result.rows().iter().flatten().cloned().collect())
    }
}

fn table_procedure(procedure: &Procedure) -> Result<rhizodb::Procedure, String> {
    let signature = parse_signature(&procedure.signature)?;
    let inputs = signature.parameters.len();
    let rows = procedure.rows.clone();
    let mut registered = rhizodb::Procedure::new(signature.name, move |args| {
        let args = args.iter().map(from_rhizo).collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        for row in &rows {
            let (row_inputs, row_outputs) = row.split_at(inputs.min(row.len()));
            if row_inputs == args.as_slice() {
                out.push(
                    row_outputs
                        .iter()
                        .map(to_rhizo)
                        .collect::<Result<Vec<_>, _>>()?,
                );
            }
        }
        Ok(out)
    });
    for (name, kind) in signature.parameters {
        registered = registered.parameter(name, value_type(&kind));
    }
    for (name, _) in signature.outputs {
        registered = registered.output(name);
    }
    Ok(registered)
}

fn value_type(kind: &str) -> ValueType {
    match kind.trim_end_matches('?') {
        "BOOLEAN" => ValueType::Boolean,
        "INTEGER" => ValueType::Integer,
        "FLOAT" => ValueType::Float,
        "NUMBER" => ValueType::Number,
        "STRING" => ValueType::String,
        "MAP" => ValueType::Map,
        "NODE" => ValueType::Node,
        "RELATIONSHIP" => ValueType::Relationship,
        "PATH" => ValueType::Path,
        kind if kind.starts_with("LIST") => ValueType::List,
        _ => ValueType::Any,
    }
}

fn record_properties(
    snapshot: &mut GraphSnapshot,
    entity: Entity,
    properties: &BTreeMap<String, rhizodb::Value>,
) -> Result<(), String> {
    for (key, value) in properties {
        let value = from_rhizo(value).map_err(|error| format!("property `{key}`: {error}"))?;
        snapshot.properties.push(PropertyEntry {
            entity,
            key: key.clone(),
            value,
        });
    }
    Ok(())
}

fn query_error(kind: ErrorKind) -> QueryError {
    let (kind, detail) = match kind {
        ErrorKind::Syntax(syntax) => match syntax {
            SyntaxErrorKind::InvalidNumberLiteral => ("SyntaxError", "InvalidNumberLiteral"),
            SyntaxErrorKind::IntegerOverflow => ("SyntaxError", "IntegerOverflow"),
            SyntaxErrorKind::FloatOverflow => ("SyntaxError", "FloatingPointOverflow"),
            SyntaxErrorKind::InvalidUnicodeCharacter => ("SyntaxError", "InvalidUnicodeCharacter"),
            SyntaxErrorKind::InvalidUnicodeEscape => ("SyntaxError", "InvalidUnicodeLiteral"),
            SyntaxErrorKind::MixedUnion => ("SyntaxError", "InvalidClauseComposition"),
            SyntaxErrorKind::InvalidRelationshipPattern => {
                ("SyntaxError", "InvalidRelationshipPattern")
            }
            SyntaxErrorKind::UnexpectedToken
            | SyntaxErrorKind::UnexpectedEnd
            | SyntaxErrorKind::UnexpectedCharacter
            | SyntaxErrorKind::InvalidEscape
            | SyntaxErrorKind::UnterminatedString
            | SyntaxErrorKind::UnterminatedQuotedName
            | SyntaxErrorKind::UnterminatedComment => ("SyntaxError", "UnexpectedSyntax"),
            _ => return QueryError::Unsupported,
        },
        ErrorKind::UndefinedVariable => ("SyntaxError", "UndefinedVariable"),
        ErrorKind::VariableAlreadyBound => ("SyntaxError", "VariableAlreadyBound"),
        ErrorKind::VariableTypeConflict => ("SyntaxError", "VariableTypeConflict"),
        ErrorKind::ColumnNameConflict => ("SyntaxError", "ColumnNameConflict"),
        ErrorKind::NoExpressionAlias => ("SyntaxError", "NoExpressionAlias"),
        ErrorKind::NoVariablesInScope => ("SyntaxError", "NoVariablesInScope"),
        ErrorKind::InvalidClauseComposition => ("SyntaxError", "InvalidClauseComposition"),
        ErrorKind::DifferentColumnsInUnion => ("SyntaxError", "DifferentColumnsInUnion"),
        ErrorKind::InvalidAggregation => ("SyntaxError", "InvalidAggregation"),
        ErrorKind::NestedAggregation => ("SyntaxError", "NestedAggregation"),
        ErrorKind::AmbiguousAggregationExpression => {
            ("SyntaxError", "AmbiguousAggregationExpression")
        }
        ErrorKind::NonConstantExpression => ("SyntaxError", "NonConstantExpression"),
        ErrorKind::NegativeIntegerArgument => ("SyntaxError", "NegativeIntegerArgument"),
        ErrorKind::NoSingleRelationshipType => ("SyntaxError", "NoSingleRelationshipType"),
        ErrorKind::RequiresDirectedRelationship => ("SyntaxError", "RequiresDirectedRelationship"),
        ErrorKind::CreatingVarLength => ("SyntaxError", "CreatingVarLength"),
        ErrorKind::InvalidParameterUse => ("SyntaxError", "InvalidParameterUse"),
        ErrorKind::InvalidDelete => ("SyntaxError", "InvalidDelete"),
        ErrorKind::NullMergeProperty => ("SemanticError", "MergeReadOwnWrites"),
        ErrorKind::RelationshipUniquenessViolation => {
            ("SyntaxError", "RelationshipUniquenessViolation")
        }
        ErrorKind::UnknownFunction => ("SyntaxError", "UnknownFunction"),
        ErrorKind::InvalidNumberOfArguments => ("SyntaxError", "InvalidNumberOfArguments"),
        ErrorKind::InvalidArgumentType => ("SyntaxError", "InvalidArgumentType"),
        ErrorKind::ProcedureNotFound => ("ProcedureError", "ProcedureNotFound"),
        ErrorKind::InvalidArgumentPassingMode => ("SyntaxError", "InvalidArgumentPassingMode"),
        ErrorKind::MissingParameter => ("ParameterMissing", "MissingParameter"),
        ErrorKind::InvalidArgumentValue => ("TypeError", "InvalidArgumentValue"),
        ErrorKind::TypeMismatch
        | ErrorKind::PropertyAccessOnNonEntity
        | ErrorKind::ListElementAccessByNonInteger => ("TypeError", "InvalidArgumentType"),
        ErrorKind::MapElementAccessByNonString => ("TypeError", "MapElementAccessByNonString"),
        ErrorKind::InvalidPropertyType => ("TypeError", "InvalidPropertyType"),
        ErrorKind::NumberOutOfRange => ("ArgumentError", "NumberOutOfRange"),
        ErrorKind::NonIntegerArgument => ("ArgumentError", "InvalidArgumentType"),
        ErrorKind::DeleteConnectedNode => ("ConstraintVerificationFailed", "DeleteConnectedNode"),
        ErrorKind::DeletedEntityAccess => ("EntityNotFound", "DeletedEntityAccess"),
        _ => return QueryError::Unsupported,
    };
    QueryError::Raised {
        kind: kind.to_owned(),
        detail: detail.to_owned(),
    }
}

fn to_rhizo(value: &Value) -> Result<rhizodb::Value, QueryError> {
    Ok(match value {
        Value::Null => rhizodb::Value::Null,
        Value::Boolean(value) => rhizodb::Value::Boolean(*value),
        Value::Integer(value) => rhizodb::Value::Integer(*value),
        Value::Float(value) => rhizodb::Value::Float(*value),
        Value::String(value) => rhizodb::Value::String(value.clone()),
        Value::List(items) => {
            rhizodb::Value::List(items.iter().map(to_rhizo).collect::<Result<_, _>>()?)
        }
        Value::Map(entries) => rhizodb::Value::Map(
            entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), to_rhizo(value)?)))
                .collect::<Result<_, QueryError>>()?,
        ),
        Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
            return Err(QueryError::Unsupported);
        }
    })
}

fn from_rhizo(value: &rhizodb::Value) -> Result<Value, QueryError> {
    Ok(match value {
        rhizodb::Value::Null => Value::Null,
        rhizodb::Value::Boolean(value) => Value::Boolean(*value),
        rhizodb::Value::Integer(value) => Value::Integer(*value),
        rhizodb::Value::Float(value) => Value::Float(*value),
        rhizodb::Value::String(value) => Value::String(value.clone()),
        rhizodb::Value::List(items) => {
            Value::List(items.iter().map(from_rhizo).collect::<Result<_, _>>()?)
        }
        rhizodb::Value::Map(entries) => Value::Map(properties(entries)?),
        rhizodb::Value::Node(node) => Value::Node(from_node(node)?),
        rhizodb::Value::Relationship(relationship) => {
            Value::Relationship(from_relationship(relationship)?)
        }
        rhizodb::Value::Path(path) => Value::Path(from_path(path)?),
        rhizodb::Value::Temporal(value) => Value::String(value.to_string()),
        _ => return Err(QueryError::Unsupported),
    })
}

fn properties(entries: &BTreeMap<String, rhizodb::Value>) -> Result<Properties, QueryError> {
    entries
        .iter()
        .map(|(key, value)| Ok((key.clone(), from_rhizo(value)?)))
        .collect()
}

fn from_node(node: &rhizodb::Node) -> Result<Node, QueryError> {
    Ok(Node {
        labels: node.labels.clone(),
        properties: properties(&node.properties)?,
    })
}

fn from_relationship(relationship: &rhizodb::Relationship) -> Result<Relationship, QueryError> {
    Ok(Relationship {
        rel_type: relationship.rel_type.clone(),
        properties: properties(&relationship.properties)?,
    })
}

fn from_path(path: &rhizodb::Path) -> Result<Path, QueryError> {
    let (start, rest) = path.nodes().split_first().ok_or(QueryError::Unsupported)?;
    let mut previous = start.id;
    let mut steps = Vec::new();
    for (relationship, node) in path.relationships().iter().zip(rest) {
        let direction = if relationship.start == previous {
            Direction::Forward
        } else {
            Direction::Backward
        };
        steps.push(PathStep {
            relationship: from_relationship(relationship)?,
            direction,
            node: from_node(node)?,
        });
        previous = node.id;
    }
    Ok(Path {
        start: from_node(start)?,
        steps,
    })
}

#[cfg(test)]
#[path = "rhizo.test.rs"]
mod tests;
