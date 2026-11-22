use lbug::Connection;
use tck::engine::QueryError;
use tck::engine::Rows;
use tck::value::Node;
use tck::value::Properties;
use tck::value::Relationship;
use tck::value::Value;

/// The error kind a LadybugDB failure is reported under.
pub const LADYBUG_ERROR: &str = "LadybugError";

/// The error kind of a LadybugDB value the harness cannot compare.
pub const UNCONVERTIBLE: &str = "Unconvertible";

/// Runs `query` on LadybugDB and converts its rows to TCK values.
pub fn execute(connection: &Connection<'_>, query: &str) -> Result<Rows, QueryError> {
    let result = connection
        .query(query)
        .map_err(|error| QueryError::Raised {
            kind: LADYBUG_ERROR.into(),
            detail: without_source_paths(&error.to_string()),
        })?;
    let columns = result.get_column_names();
    let rows = result
        .map(|row| row.iter().map(convert).collect::<Result<Vec<_>, _>>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|detail| QueryError::Raised {
            kind: UNCONVERTIBLE.into(),
            detail,
        })?;
    Ok(Rows { columns, rows })
}

/// Drops the machine-specific directories from the source paths a failed LadybugDB assertion
/// names, so the message reads the same on every machine.
pub fn without_source_paths(message: &str) -> String {
    const SOURCE_ROOT: &str = "lbug-src/";
    message
        .split('"')
        .map(|part| match part.find(SOURCE_ROOT) {
            Some(start) => &part[start..],
            None => part,
        })
        .collect::<Vec<_>>()
        .join("\"")
}

/// Converts a LadybugDB value to the TCK value it stands for.
///
/// Every integer width becomes an integer and both float widths a float. A struct becomes a
/// map, since LadybugDB returns map literals as structs. A node or relationship drops its null
/// properties, since LadybugDB gives every column of the table to every element, and RhizoDB
/// keeps only the properties an element has.
pub fn convert(value: &lbug::Value) -> Result<Value, String> {
    let integer = |value: i128| {
        i64::try_from(value)
            .map(Value::Integer)
            .map_err(|_| format!("integer {value} does not fit in 64 bits"))
    };
    Ok(match value {
        lbug::Value::Null(_) => Value::Null,
        lbug::Value::Bool(value) => Value::Boolean(*value),
        lbug::Value::Int8(value) => Value::Integer(i64::from(*value)),
        lbug::Value::Int16(value) => Value::Integer(i64::from(*value)),
        lbug::Value::Int32(value) => Value::Integer(i64::from(*value)),
        lbug::Value::Int64(value) => Value::Integer(*value),
        lbug::Value::UInt8(value) => Value::Integer(i64::from(*value)),
        lbug::Value::UInt16(value) => Value::Integer(i64::from(*value)),
        lbug::Value::UInt32(value) => Value::Integer(i64::from(*value)),
        lbug::Value::UInt64(value) => integer(i128::from(*value))?,
        lbug::Value::Int128(value) => integer(*value)?,
        lbug::Value::Double(value) => Value::Float(*value),
        lbug::Value::Float(value) => Value::Float(f64::from(*value)),
        lbug::Value::String(value) => Value::String(value.clone()),
        lbug::Value::Date(value) => Value::String(value.to_string()),
        lbug::Value::List(_, items) | lbug::Value::Array(_, items) => {
            Value::List(items.iter().map(convert).collect::<Result<_, _>>()?)
        }
        lbug::Value::Struct(fields) => Value::Map(properties(fields, false)?),
        lbug::Value::Map(_, entries) => Value::Map(
            entries
                .iter()
                .map(|(key, value)| match key {
                    lbug::Value::String(key) => Ok((key.clone(), convert(value)?)),
                    key => Err(format!("map key {key} is not a string")),
                })
                .collect::<Result<_, _>>()?,
        ),
        lbug::Value::Node(node) => Value::Node(Node {
            labels: [node.get_label_name().clone()].into_iter().collect(),
            properties: properties(node.get_properties(), true)?,
        }),
        lbug::Value::Rel(relationship) => Value::Relationship(Relationship {
            rel_type: relationship.get_label_name().clone(),
            properties: properties(relationship.get_properties(), true)?,
        }),
        other => return Err(format!("cannot compare LadybugDB value {other}")),
    })
}

fn properties(fields: &[(String, lbug::Value)], drop_nulls: bool) -> Result<Properties, String> {
    fields
        .iter()
        .filter(|(_, value)| !(drop_nulls && matches!(value, lbug::Value::Null(_))))
        .map(|(key, value)| Ok((key.clone(), convert(value)?)))
        .collect()
}

#[cfg(test)]
#[path = "ladybug.test.rs"]
mod tests;
