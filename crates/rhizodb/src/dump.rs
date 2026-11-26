use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;
use std::io::BufRead;
use std::io::BufWriter;
use std::io::Write;

use crate::error::Error;
use crate::error::ErrorKind;
use crate::graph::Graph;
use crate::graph::GraphError;
use crate::graph::GraphTx;
use crate::graph::IndexKind;
use crate::graph::IndexTarget;
use crate::graph::Label;
use crate::graph::NodeId;
use crate::graph::NodeRecord;
use crate::graph::Properties;
use crate::graph::PropertyKey;
use crate::graph::RelationshipId;
use crate::graph::RelationshipRecord;
use crate::graph::RelationshipType;
use crate::graph::Target;
use crate::graph::Token;
use crate::graph::VectorMetric;
use crate::storage::ReadTransaction;
use crate::storage::Storage;
use crate::storage::WriteTransaction;
use crate::temporal::Date;
use crate::temporal::DateTime;
use crate::temporal::Duration;
use crate::temporal::LocalDateTime;
use crate::temporal::LocalTime;
use crate::temporal::Temporal;
use crate::temporal::Time;
use crate::value::MAX_DEPTH;
use crate::value::PropertyValue;

use self::json::Json;

pub(crate) mod json;

pub(crate) const VERSION: i64 = 1;
const FORMAT: &str = "rhizodb-dump";
const BATCH_LINES: usize = 10_000;
const BATCH_BYTES: usize = 16 << 20;

fn io_error(error: io::Error) -> Error {
    Error::caused_by(
        ErrorKind::Io,
        format!("reading or writing the dump failed: {error}"),
        Box::new(error),
    )
}

fn invalid(line: u64, message: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::InvalidDump,
        format!("dump line {line}: {message}"),
    )
}

/// Writes every index definition, node, and relationship that `tx` sees to `out` as a dump.
pub(crate) fn export<T: ReadTransaction>(tx: &GraphTx<T>, out: impl Write) -> Result<(), Error> {
    let mut out = BufWriter::new(out);
    let mut line = String::new();
    let (next_node, next_relationship) = tx.next_ids()?;
    let _ = write!(
        line,
        "{{\"type\":\"header\",\"format\":\"{FORMAT}\",\"version\":{VERSION},\
         \"next_node_id\":{next_node},\"next_relationship_id\":{next_relationship}}}"
    );
    emit(&mut out, &mut line).map_err(io_error)?;
    for (name, index) in tx.indexes()? {
        write_index(&mut line, tx, &name, &index.definition)?;
        emit(&mut out, &mut line).map_err(io_error)?;
    }
    let mut failed: Option<io::Error> = None;
    let mut nodes = 0u64;
    let visited = tx.visit_nodes(|id, labels, properties| {
        line.push_str("{\"type\":\"node\",\"id\":");
        let _ = write!(line, "{}", id.as_u64());
        line.push_str(",\"labels\":[");
        let mut sorted = labels
            .iter()
            .map(|label| tx.token_name(*label))
            .collect::<Result<Vec<_>, _>>()?;
        sorted.sort();
        for (at, label) in sorted.iter().enumerate() {
            if at > 0 {
                line.push(',');
            }
            json::write_string(&mut line, label);
        }
        line.push_str("],\"properties\":");
        write_properties(&mut line, tx, &properties)?;
        line.push('}');
        nodes += 1;
        emit(&mut out, &mut line).map_err(|error| {
            failed = Some(error);
            GraphError::Corrupt("writing the dump failed")
        })
    });
    if let Some(error) = failed.take() {
        return Err(io_error(error));
    }
    visited?;
    let mut relationships = 0u64;
    let visited = tx.visit_relationships(|id, record| {
        line.push_str("{\"type\":\"relationship\",\"id\":");
        let _ = write!(line, "{}", id.as_u64());
        line.push_str(",\"relationship_type\":");
        json::write_string(&mut line, &tx.token_name(record.rel_type)?);
        let _ = write!(
            line,
            ",\"start\":{},\"end\":{},\"properties\":",
            record.source.as_u64(),
            record.target.as_u64()
        );
        write_properties(&mut line, tx, &record.properties)?;
        line.push('}');
        relationships += 1;
        emit(&mut out, &mut line).map_err(|error| {
            failed = Some(error);
            GraphError::Corrupt("writing the dump failed")
        })
    });
    if let Some(error) = failed.take() {
        return Err(io_error(error));
    }
    visited?;
    let _ = write!(
        line,
        "{{\"type\":\"end\",\"nodes\":{nodes},\"relationships\":{relationships}}}"
    );
    emit(&mut out, &mut line).map_err(io_error)?;
    out.flush().map_err(io_error)
}

fn emit(out: &mut impl Write, line: &mut String) -> io::Result<()> {
    line.push('\n');
    let written = out.write_all(line.as_bytes());
    line.clear();
    written
}

fn write_index<T: ReadTransaction>(
    line: &mut String,
    tx: &GraphTx<T>,
    name: &str,
    definition: &crate::graph::IndexDefinition,
) -> Result<(), GraphError> {
    line.push_str("{\"type\":\"index\",\"name\":");
    json::write_string(line, name);
    line.push_str(",\"index\":");
    line.push_str(match definition.kind {
        IndexKind::Keyword { .. } => "\"keyword\"",
        IndexKind::Vector { .. } => "\"vector\"",
        IndexKind::Property { .. } => "\"property\"",
    });
    match definition.target {
        Target::Label(label) => {
            line.push_str(",\"label\":");
            json::write_string(line, &tx.token_name(label)?);
        }
        Target::RelationshipType(rel_type) => {
            line.push_str(",\"relationship_type\":");
            json::write_string(line, &tx.token_name(rel_type)?);
        }
    }
    match &definition.kind {
        IndexKind::Keyword { properties } => {
            let mut sorted = properties
                .iter()
                .map(|key| tx.token_name(*key))
                .collect::<Result<Vec<_>, _>>()?;
            sorted.sort();
            line.push_str(",\"properties\":[");
            for (at, key) in sorted.iter().enumerate() {
                if at > 0 {
                    line.push(',');
                }
                json::write_string(line, key);
            }
            line.push(']');
        }
        IndexKind::Vector {
            property,
            dimension,
            metric,
        } => {
            line.push_str(",\"property\":");
            json::write_string(line, &tx.token_name(*property)?);
            let _ = write!(
                line,
                ",\"dimension\":{dimension},\"metric\":\"{}\"",
                metric_name(*metric)
            );
        }
        IndexKind::Property { property } => {
            line.push_str(",\"property\":");
            json::write_string(line, &tx.token_name(*property)?);
        }
    }
    line.push('}');
    Ok(())
}

fn metric_name(metric: VectorMetric) -> &'static str {
    match metric {
        VectorMetric::Cosine => "cosine",
        VectorMetric::Euclidean => "euclidean",
        VectorMetric::DotProduct => "dot_product",
    }
}

fn write_properties<T: ReadTransaction>(
    line: &mut String,
    tx: &GraphTx<T>,
    properties: &Properties,
) -> Result<(), GraphError> {
    let mut sorted = properties
        .iter()
        .map(|(key, value)| Ok((tx.token_name(*key)?, value)))
        .collect::<Result<Vec<_>, GraphError>>()?;
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    line.push('{');
    for (at, (key, value)) in sorted.iter().enumerate() {
        if at > 0 {
            line.push(',');
        }
        json::write_string(line, key);
        line.push(':');
        write_value(line, value);
    }
    line.push('}');
    Ok(())
}

pub(crate) fn write_value(out: &mut String, value: &PropertyValue) {
    match value {
        PropertyValue::Boolean(value) => out.push_str(if *value { "true" } else { "false" }),
        PropertyValue::Integer(value) => {
            let _ = write!(out, "{value}");
        }
        PropertyValue::Float(value) if value.is_finite() => json::write_finite_float(out, *value),
        PropertyValue::Float(value) => {
            let _ = write!(out, "{{\"$float\":\"{:016x}\"}}", value.to_bits());
        }
        PropertyValue::String(value) => json::write_string(out, value),
        PropertyValue::Bytes(value) => {
            out.push_str("{\"$bytes\":\"");
            for byte in value {
                let _ = write!(out, "{byte:02x}");
            }
            out.push_str("\"}");
        }
        PropertyValue::List(items) => {
            out.push('[');
            for (at, item) in items.iter().enumerate() {
                if at > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        PropertyValue::Temporal(value) => write_temporal(out, value),
    }
}

fn write_date(out: &mut String, date: Date) {
    let year = date.year();
    if year < 0 {
        let _ = write!(out, "-{:04}", year.unsigned_abs());
    } else {
        let _ = write!(out, "{year:04}");
    }
    let _ = write!(out, "-{:02}-{:02}", date.month(), date.day());
}

fn write_local_time(out: &mut String, time: LocalTime) {
    let _ = write!(
        out,
        "{:02}:{:02}:{:02}.{:09}",
        time.hour(),
        time.minute(),
        time.second(),
        time.nanosecond()
    );
}

fn write_offset(out: &mut String, offset: i32) {
    let sign = if offset < 0 { '-' } else { '+' };
    let seconds = offset.unsigned_abs();
    let _ = write!(
        out,
        "{sign}{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
}

fn write_temporal(out: &mut String, value: &Temporal) {
    match value {
        Temporal::Date(date) => {
            out.push_str("{\"$date\":\"");
            write_date(out, *date);
        }
        Temporal::LocalTime(time) => {
            out.push_str("{\"$localtime\":\"");
            write_local_time(out, *time);
        }
        Temporal::Time(time) => {
            out.push_str("{\"$time\":\"");
            write_local_time(out, time.local_time());
            write_offset(out, time.offset_seconds());
        }
        Temporal::LocalDateTime(local) => {
            out.push_str("{\"$localdatetime\":\"");
            write_date(out, local.date());
            out.push('T');
            write_local_time(out, local.time());
        }
        Temporal::DateTime(datetime) => {
            out.push_str("{\"$datetime\":\"");
            let local = datetime.local();
            write_date(out, local.date());
            out.push('T');
            write_local_time(out, local.time());
            write_offset(out, datetime.offset_seconds());
            if let Some(zone) = datetime.zone_name() {
                out.push('[');
                out.push_str(zone);
                out.push(']');
            }
        }
        Temporal::Duration(duration) => {
            let _ = write!(
                out,
                "{{\"$duration\":{{\"months\":{},\"days\":{},\"seconds\":{},\"nanoseconds\":{}}}}}",
                duration.months(),
                duration.days(),
                duration.seconds(),
                duration.nanoseconds()
            );
            return;
        }
    }
    out.push_str("\"}");
}

struct Fields {
    entries: Vec<(String, Json)>,
}

impl Fields {
    fn new(json: Json, what: &str) -> Result<Self, String> {
        let Json::Object(entries) = json else {
            return Err(format!("{what} is not a JSON object"));
        };
        let mut seen = BTreeSet::new();
        for (key, _) in &entries {
            if !seen.insert(key.as_str()) {
                return Err(format!("{what} repeats the field `{key}`"));
            }
        }
        Ok(Self { entries })
    }

    fn take(&mut self, key: &str) -> Option<Json> {
        let at = self.entries.iter().position(|(held, _)| held == key)?;
        Some(self.entries.swap_remove(at).1)
    }

    fn required(&mut self, key: &str) -> Result<Json, String> {
        self.take(key)
            .ok_or_else(|| format!("the field `{key}` is missing"))
    }

    fn string(&mut self, key: &str) -> Result<String, String> {
        match self.required(key)? {
            Json::String(text) => Ok(text),
            _ => Err(format!("the field `{key}` is not a string")),
        }
    }

    fn id(&mut self, key: &str) -> Result<u64, String> {
        match self.required(key)? {
            Json::Integer(value) => {
                u64::try_from(value).map_err(|_| format!("the field `{key}` is negative"))
            }
            _ => Err(format!("the field `{key}` is not an integer")),
        }
    }

    fn finish(self) -> Result<(), String> {
        match self.entries.first() {
            Some((key, _)) => Err(format!("the field `{key}` is unknown")),
            None => Ok(()),
        }
    }
}

fn strings(json: Json, what: &str) -> Result<Vec<String>, String> {
    let Json::Array(items) = json else {
        return Err(format!("{what} is not an array"));
    };
    let mut seen = BTreeSet::new();
    items
        .into_iter()
        .map(|item| match item {
            Json::String(text) if seen.insert(text.clone()) => Ok(text),
            Json::String(text) => Err(format!("{what} repeats `{text}`")),
            _ => Err(format!("{what} holds something other than a string")),
        })
        .collect()
}

pub(crate) fn read_value(json: Json, depth: usize) -> Result<PropertyValue, String> {
    match json {
        Json::Null => Err("a property value is null".to_owned()),
        Json::Boolean(value) => Ok(PropertyValue::Boolean(value)),
        Json::Integer(value) => Ok(PropertyValue::Integer(value)),
        Json::Float(value) => Ok(PropertyValue::Float(value)),
        Json::String(value) => Ok(PropertyValue::String(value)),
        Json::Array(items) => {
            if depth >= MAX_DEPTH {
                return Err(format!("a list nests deeper than {MAX_DEPTH} levels"));
            }
            items
                .into_iter()
                .map(|item| read_value(item, depth + 1))
                .collect::<Result<_, _>>()
                .map(PropertyValue::List)
        }
        Json::Object(mut entries) => {
            let Some((tag, inner)) = entries.pop().filter(|_| entries.is_empty()) else {
                return Err("a tagged value has other than one field".to_owned());
            };
            read_tagged(&tag, inner)
        }
    }
}

fn read_tagged(tag: &str, inner: Json) -> Result<PropertyValue, String> {
    if tag == "$duration" {
        return read_duration(inner)
            .map(|duration| PropertyValue::Temporal(Temporal::Duration(duration)));
    }
    let Json::String(text) = inner else {
        return Err(format!("the value of `{tag}` is not a string"));
    };
    let malformed = || format!("`{text}` is not a valid `{tag}`");
    let temporal = match tag {
        "$float" => {
            let value = parse_hex(&text)
                .filter(|bytes| bytes.len() == 8)
                .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
                .map(|bytes| f64::from_bits(u64::from_be_bytes(bytes)))
                .filter(|value| !value.is_finite())
                .ok_or_else(malformed)?;
            return Ok(PropertyValue::Float(value));
        }
        "$bytes" => {
            return parse_hex(&text)
                .map(PropertyValue::Bytes)
                .ok_or_else(malformed);
        }
        "$date" => parse_date(&text).map(Temporal::Date),
        "$localtime" => parse_local_time(&text).map(Temporal::LocalTime),
        "$time" => parse_time(&text).map(Temporal::Time),
        "$localdatetime" => parse_local_datetime(&text).map(Temporal::LocalDateTime),
        "$datetime" => parse_datetime(&text).map(Temporal::DateTime),
        _ => return Err(format!("the value tag `{tag}` is unknown")),
    };
    temporal.map(PropertyValue::Temporal).ok_or_else(malformed)
}

fn read_duration(inner: Json) -> Result<Duration, String> {
    let mut fields = Fields::new(inner, "a duration")?;
    let mut part = |key: &str| match fields.required(key)? {
        Json::Integer(value) => Ok(value),
        _ => Err(format!("the duration field `{key}` is not an integer")),
    };
    let months = part("months")?;
    let days = part("days")?;
    let seconds = part("seconds")?;
    let nanoseconds = part("nanoseconds")?;
    fields.finish()?;
    u32::try_from(nanoseconds)
        .ok()
        .and_then(|nanoseconds| Duration::from_stored(months, days, seconds, nanoseconds))
        .ok_or_else(|| "a duration's nanoseconds are not below one second".to_owned())
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

fn digits<T: std::str::FromStr>(text: &str, count: usize) -> Option<T> {
    (text.len() == count && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn parse_date(text: &str) -> Option<Date> {
    let (negative, unsigned) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut parts = unsigned.split('-');
    let year = parts.next()?;
    let month = digits(parts.next()?, 2)?;
    let day = digits(parts.next()?, 2)?;
    if parts.next().is_some() || year.len() < 4 || !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let year: i64 = year.parse().ok()?;
    Date::from_ymd(if negative { -year } else { year }, month, day)
}

fn parse_local_time(text: &str) -> Option<LocalTime> {
    let bytes = text.as_bytes();
    if text.len() != 18 || bytes.get(2) != Some(&b':') || bytes.get(5) != Some(&b':') {
        return None;
    }
    if bytes.get(8) != Some(&b'.') {
        return None;
    }
    LocalTime::from_hms_nano(
        digits(text.get(0..2)?, 2)?,
        digits(text.get(3..5)?, 2)?,
        digits(text.get(6..8)?, 2)?,
        digits(text.get(9..18)?, 9)?,
    )
}

fn parse_offset(text: &str) -> Option<i32> {
    let bytes = text.as_bytes();
    if text.len() != 9 || bytes.get(3) != Some(&b':') || bytes.get(6) != Some(&b':') {
        return None;
    }
    let sign = match bytes.first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    let hours: i32 = digits(text.get(1..3)?, 2)?;
    let minutes: i32 = digits(text.get(4..6)?, 2)?;
    let seconds: i32 = digits(text.get(7..9)?, 2)?;
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60 + seconds))
}

fn parse_time(text: &str) -> Option<Time> {
    let split = text.len().checked_sub(9)?;
    Time::new(
        parse_local_time(text.get(..split)?)?,
        parse_offset(text.get(split..)?)?,
    )
}

fn parse_local_datetime(text: &str) -> Option<LocalDateTime> {
    let (date, time) = text.split_once('T')?;
    Some(LocalDateTime::new(
        parse_date(date)?,
        parse_local_time(time)?,
    ))
}

fn parse_datetime(text: &str) -> Option<DateTime> {
    let (rest, zone) = match text.strip_suffix(']') {
        Some(rest) => {
            let (rest, zone) = rest.split_once('[')?;
            (rest, Some(zone))
        }
        None => (text, None),
    };
    let split = rest.len().checked_sub(9)?;
    let local = parse_local_datetime(rest.get(..split)?)?;
    let offset = parse_offset(rest.get(split..)?)?;
    let instant = local.nanos() - i128::from(offset) * 1_000_000_000;
    let epoch_second = i64::try_from(instant.div_euclid(1_000_000_000)).ok()?;
    let nanosecond = u32::try_from(instant.rem_euclid(1_000_000_000)).ok()?;
    DateTime::from_stored(epoch_second, nanosecond, offset, zone)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Header,
    Indexes,
    Nodes,
    Relationships,
    Ended,
}

struct Import {
    stage: Stage,
    next_node: u64,
    next_relationship: u64,
    last_node: Option<u64>,
    last_relationship: Option<u64>,
    nodes: u64,
    relationships: u64,
}

fn line_error(line: u64, error: GraphError) -> Error {
    match error {
        GraphError::NodeNotFound(_) | GraphError::IndexExists(_) | GraphError::InvalidIndex(_) => {
            invalid(line, error)
        }
        other => Error::from(other),
    }
}

impl Import {
    fn apply<T: WriteTransaction>(
        &mut self,
        tx: &mut GraphTx<T>,
        json: Json,
        line: u64,
    ) -> Result<(), Error> {
        let mut fields = Fields::new(json, "the line").map_err(|error| invalid(line, error))?;
        let kind = fields
            .string("type")
            .map_err(|error| invalid(line, error))?;
        match (self.stage, kind.as_str()) {
            (Stage::Header, "header") => self.header(fields, line),
            (Stage::Header, _) => Err(invalid(line, "the dump does not start with a header")),
            (Stage::Indexes, "index") => self.index(tx, fields, line),
            (Stage::Indexes | Stage::Nodes, "node") => {
                self.stage = Stage::Nodes;
                self.node(tx, fields, line)
            }
            (Stage::Indexes | Stage::Nodes | Stage::Relationships, "relationship") => {
                self.stage = Stage::Relationships;
                self.relationship(tx, fields, line)
            }
            (Stage::Indexes | Stage::Nodes | Stage::Relationships, "end") => self.end(fields, line),
            (_, "header" | "index" | "node" | "relationship" | "end") => {
                Err(invalid(line, format!("a `{kind}` line is out of order")))
            }
            _ => Err(invalid(line, format!("the line type `{kind}` is unknown"))),
        }
    }

    fn header(&mut self, mut fields: Fields, line: u64) -> Result<(), Error> {
        let fail = |error| invalid(line, error);
        if fields.string("format").map_err(fail)? != FORMAT {
            return Err(invalid(line, "the file is not a RhizoDB dump"));
        }
        match fields.required("version").map_err(fail)? {
            Json::Integer(version) if version > VERSION => {
                return Err(Error::new(
                    ErrorKind::UnsupportedDumpVersion,
                    format!(
                        "the dump has format version {version}, and this build reads versions up to {VERSION}"
                    ),
                ));
            }
            Json::Integer(version) if version >= 1 => {}
            _ => return Err(invalid(line, "the version is not a positive integer")),
        }
        self.next_node = fields.id("next_node_id").map_err(fail)?;
        self.next_relationship = fields.id("next_relationship_id").map_err(fail)?;
        fields.finish().map_err(fail)?;
        self.stage = Stage::Indexes;
        Ok(())
    }

    fn index<T: WriteTransaction>(
        &mut self,
        tx: &mut GraphTx<T>,
        mut fields: Fields,
        line: u64,
    ) -> Result<(), Error> {
        let fail = |error| invalid(line, error);
        let name = fields.string("name").map_err(fail)?;
        let kind = fields.string("index").map_err(fail)?;
        let label = fields.take("label");
        let rel_type = fields.take("relationship_type");
        let (target_name, over_label) = match (label, rel_type) {
            (Some(Json::String(label)), None) => (label, true),
            (None, Some(Json::String(rel_type))) => (rel_type, false),
            _ => {
                return Err(invalid(
                    line,
                    "an index names neither one label nor one relationship type",
                ));
            }
        };
        let target = if over_label {
            IndexTarget::Label(&target_name)
        } else {
            IndexTarget::RelationshipType(&target_name)
        };
        let created = match kind.as_str() {
            "keyword" => {
                let properties = strings(
                    fields.required("properties").map_err(fail)?,
                    "the properties",
                )
                .map_err(fail)?;
                fields.finish().map_err(fail)?;
                let properties: Vec<&str> = properties.iter().map(String::as_str).collect();
                tx.create_keyword_index(&name, target, &properties)
            }
            "vector" => {
                let property = fields.string("property").map_err(fail)?;
                let dimension = fields.id("dimension").map_err(fail)?;
                let metric = match fields.string("metric").map_err(fail)?.as_str() {
                    "cosine" => VectorMetric::Cosine,
                    "euclidean" => VectorMetric::Euclidean,
                    "dot_product" => VectorMetric::DotProduct,
                    other => return Err(invalid(line, format!("the metric `{other}` is unknown"))),
                };
                fields.finish().map_err(fail)?;
                let dimension = usize::try_from(dimension)
                    .map_err(|_| invalid(line, "the dimension is too large"))?;
                tx.create_vector_index(&name, target, &property, dimension, metric)
            }
            "property" => {
                let property = fields.string("property").map_err(fail)?;
                fields.finish().map_err(fail)?;
                tx.create_property_index(&name, target, &property)
            }
            other => {
                return Err(invalid(
                    line,
                    format!("the index kind `{other}` is unknown"),
                ));
            }
        };
        created.map_err(|error| line_error(line, error))
    }

    fn node<T: WriteTransaction>(
        &mut self,
        tx: &mut GraphTx<T>,
        mut fields: Fields,
        line: u64,
    ) -> Result<(), Error> {
        let fail = |error| invalid(line, error);
        let id = fields.id("id").map_err(fail)?;
        if self.last_node.is_some_and(|last| id <= last) || id >= self.next_node {
            return Err(invalid(
                line,
                format!(
                    "node {id} is not above the node before it and below the header's next node ID"
                ),
            ));
        }
        let labels =
            strings(fields.required("labels").map_err(fail)?, "the labels").map_err(fail)?;
        let properties = fields.required("properties").map_err(fail)?;
        fields.finish().map_err(fail)?;
        let mut record = NodeRecord::default();
        for label in labels {
            record
                .labels
                .insert(tx.intern_token::<Label>(&label).map_err(Error::from)?);
        }
        record.properties = read_properties(tx, properties).map_err(|error| match error {
            Read::Invalid(message) => invalid(line, message),
            Read::Graph(error) => Error::from(error),
        })?;
        tx.create_node_at(NodeId(id), record)
            .map_err(|error| line_error(line, error))?;
        self.last_node = Some(id);
        self.nodes += 1;
        Ok(())
    }

    fn relationship<T: WriteTransaction>(
        &mut self,
        tx: &mut GraphTx<T>,
        mut fields: Fields,
        line: u64,
    ) -> Result<(), Error> {
        let fail = |error| invalid(line, error);
        let id = fields.id("id").map_err(fail)?;
        if self.last_relationship.is_some_and(|last| id <= last) || id >= self.next_relationship {
            return Err(invalid(
                line,
                format!(
                    "relationship {id} is not above the relationship before it and below the header's next relationship ID"
                ),
            ));
        }
        let rel_type = fields.string("relationship_type").map_err(fail)?;
        let source = NodeId(fields.id("start").map_err(fail)?);
        let target = NodeId(fields.id("end").map_err(fail)?);
        let properties = fields.required("properties").map_err(fail)?;
        fields.finish().map_err(fail)?;
        let record = RelationshipRecord {
            rel_type: tx
                .intern_token::<RelationshipType>(&rel_type)
                .map_err(Error::from)?,
            source,
            target,
            properties: read_properties(tx, properties).map_err(|error| match error {
                Read::Invalid(message) => invalid(line, message),
                Read::Graph(error) => Error::from(error),
            })?,
        };
        tx.create_relationship_at(RelationshipId(id), &record)
            .map_err(|error| line_error(line, error))?;
        self.last_relationship = Some(id);
        self.relationships += 1;
        Ok(())
    }

    fn end(&mut self, mut fields: Fields, line: u64) -> Result<(), Error> {
        let fail = |error| invalid(line, error);
        let nodes = fields.id("nodes").map_err(fail)?;
        let relationships = fields.id("relationships").map_err(fail)?;
        fields.finish().map_err(fail)?;
        if (nodes, relationships) != (self.nodes, self.relationships) {
            return Err(invalid(
                line,
                format!(
                    "the end line counts {nodes} nodes and {relationships} relationships, but the dump holds {} and {}",
                    self.nodes, self.relationships
                ),
            ));
        }
        self.stage = Stage::Ended;
        Ok(())
    }
}

enum Read {
    Invalid(String),
    Graph(GraphError),
}

fn read_properties<T: WriteTransaction>(
    tx: &mut GraphTx<T>,
    json: Json,
) -> Result<Properties, Read> {
    let fields = Fields::new(json, "the properties").map_err(Read::Invalid)?;
    let mut properties = Properties::new();
    for (key, value) in fields.entries {
        let value = read_value(value, 0).map_err(Read::Invalid)?;
        let token: Token<PropertyKey> = tx.intern_token(&key).map_err(Read::Graph)?;
        properties.insert(token, value);
    }
    Ok(properties)
}

/// Reads a dump from `input` into `graph`, which must hold no node, relationship, or index and
/// never have held a node or relationship, in write transactions that each commit a bounded
/// batch of lines. Write transactions of other threads wait until the import ends.
pub(crate) fn import<S: Storage>(graph: &Graph<S>, mut input: impl BufRead) -> Result<(), Error> {
    let _exclusive = graph.exclusive_writes()?;
    let mut tx = graph.write()?;
    if tx.next_ids()? != (0, 0) || !tx.indexes()?.is_empty() {
        return Err(Error::new(
            ErrorKind::DatabaseNotEmpty,
            "a dump imports only into a database that never held a node, a relationship, or an index",
        ));
    }
    let mut state = Import {
        stage: Stage::Header,
        next_node: 0,
        next_relationship: 0,
        last_node: None,
        last_relationship: None,
        nodes: 0,
        relationships: 0,
    };
    let mut buffer = Vec::new();
    let mut line = 0u64;
    let mut pending_lines = 0usize;
    let mut pending_bytes = 0usize;
    loop {
        buffer.clear();
        let read = input.read_until(b'\n', &mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        line += 1;
        if state.stage == Stage::Ended {
            return Err(invalid(line, "text follows the end line"));
        }
        let mut bytes = buffer.as_slice();
        if let Some(rest) = bytes.strip_suffix(b"\n") {
            bytes = rest.strip_suffix(b"\r").unwrap_or(rest);
        }
        let text =
            std::str::from_utf8(bytes).map_err(|_| invalid(line, "the line is not UTF-8"))?;
        let json = json::parse(text).map_err(|error| invalid(line, error))?;
        state.apply(&mut tx, json, line)?;
        pending_lines += 1;
        pending_bytes = pending_bytes.saturating_add(read);
        if pending_lines >= BATCH_LINES || pending_bytes >= BATCH_BYTES {
            tx.commit()?;
            tx = graph.write()?;
            pending_lines = 0;
            pending_bytes = 0;
        }
    }
    if state.stage != Stage::Ended {
        return Err(invalid(line, "the dump ends before its end line"));
    }
    tx.advance_ids(state.next_node, state.next_relationship)?;
    Ok(tx.commit()?)
}

#[cfg(test)]
#[path = "dump.test.rs"]
mod tests;
