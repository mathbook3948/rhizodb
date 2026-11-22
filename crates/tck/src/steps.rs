use std::collections::BTreeMap;

use crate::literal::parse_value;
use crate::scenario::Control;
use crate::scenario::Expected;
use crate::scenario::GraphSetup;
use crate::scenario::Ordering;
use crate::scenario::Phase;
use crate::scenario::Procedure;
use crate::side_effects::Counts;
use crate::side_effects::SideEffects;
use crate::value::Lists;
use crate::value::Value;

#[derive(Clone, Copy, Debug)]
enum Kind {
    EmptyGraph,
    AnyGraph,
    NamedGraph,
    Setup,
    Parameters,
    Procedure,
    Query,
    ControlQuery,
    Rows(Ordering, Lists),
    EmptyResult,
    Error,
    SideEffects,
    NoSideEffects,
}

const STEP_PATTERNS: [(&str, Kind); 17] = [
    ("an empty graph", Kind::EmptyGraph),
    ("any graph", Kind::AnyGraph),
    ("the {} graph", Kind::NamedGraph),
    ("having executed:", Kind::Setup),
    ("parameters are:", Kind::Parameters),
    ("there exists a procedure {}:", Kind::Procedure),
    ("executing query:", Kind::Query),
    ("executing control query:", Kind::ControlQuery),
    (
        "the result should be, in any order:",
        Kind::Rows(Ordering::AnyOrder, Lists::Strict),
    ),
    (
        "the result should be, in order:",
        Kind::Rows(Ordering::InOrder, Lists::Strict),
    ),
    (
        "the result should be (ignoring element order for lists):",
        Kind::Rows(Ordering::AnyOrder, Lists::IgnoreElementOrder),
    ),
    (
        "the result should be, in order (ignoring element order for lists):",
        Kind::Rows(Ordering::InOrder, Lists::IgnoreElementOrder),
    ),
    ("the result should be empty", Kind::EmptyResult),
    ("a {} should be raised at {}: {}", Kind::Error),
    ("an {} should be raised at {}: {}", Kind::Error),
    ("the side effects should be:", Kind::SideEffects),
    ("no side effects", Kind::NoSideEffects),
];

fn capture<'a>(template: &str, text: &'a str) -> Option<Vec<&'a str>> {
    let segments: Vec<&str> = template.split("{}").collect();
    let (first, rest) = segments.split_first()?;
    let (last, middle) = match rest.split_last() {
        Some(split) => split,
        None => return (text == *first).then(Vec::new),
    };
    let mut remaining = text.strip_prefix(first)?.strip_suffix(last)?;
    let mut captures = Vec::new();
    for segment in middle {
        let at = remaining.find(segment)?;
        captures.push(&remaining[..at]);
        remaining = &remaining[at + segment.len()..];
    }
    captures.push(remaining);
    Some(captures)
}

/// One step of a scenario as parsed from a feature file, with an outline already substituted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawStep {
    /// Step text after the keyword.
    pub text: String,
    /// Doc string argument, if any.
    pub docstring: Option<String>,
    /// Data table argument, if any.
    pub table: Option<Vec<Vec<String>>>,
    /// One-based line of the step in the feature file.
    pub line: usize,
}

/// The parts of a scenario that its steps describe.
#[derive(Clone, Debug, PartialEq)]
pub struct Parts {
    /// Starting graph.
    pub graph: GraphSetup,
    /// Registered procedures.
    pub procedures: Vec<Procedure>,
    /// Setup queries.
    pub setup: Vec<String>,
    /// Query parameters.
    pub parameters: BTreeMap<String, Value>,
    /// Query under test.
    pub query: String,
    /// Expected outcome of the query.
    pub expected: Expected,
    /// Expected side effects.
    pub side_effects: Option<SideEffects>,
    /// Control query.
    pub control: Option<Control>,
}

#[derive(Default)]
struct Builder {
    graph: Option<GraphSetup>,
    procedures: Vec<Procedure>,
    setup: Vec<String>,
    parameters: Option<BTreeMap<String, Value>>,
    query: Option<String>,
    expected: Option<Expected>,
    side_effects: Option<SideEffects>,
    control_query: Option<String>,
    control_expected: Option<Expected>,
}

/// Compiles the steps of one scenario, background first, into its parts.
pub fn compile_steps(steps: &[RawStep]) -> Result<Parts, String> {
    let mut builder = Builder::default();
    for step in steps {
        builder
            .apply(step)
            .map_err(|message| format!("line {}: {message}", step.line))?;
    }
    builder.finish()
}

impl Builder {
    fn apply(&mut self, step: &RawStep) -> Result<(), String> {
        let (kind, captures) = STEP_PATTERNS
            .iter()
            .find_map(|(template, kind)| capture(template, &step.text).map(|c| (*kind, c)))
            .ok_or_else(|| format!("unrecognized step `{}`", step.text))?;
        match kind {
            Kind::EmptyGraph => self.set_graph(GraphSetup::Empty),
            Kind::AnyGraph => self.set_graph(GraphSetup::Any),
            Kind::NamedGraph => self.set_graph(GraphSetup::Named(captures[0].to_string())),
            Kind::Setup => {
                if self.parameters.is_some() || self.query.is_some() {
                    return Err("setup query after parameters or the query".into());
                }
                self.setup.push(docstring(step)?);
                Ok(())
            }
            Kind::Parameters => {
                if self.parameters.is_some() {
                    return Err("parameters given twice".into());
                }
                let mut parameters = BTreeMap::new();
                for row in table(step)? {
                    let [name, value] = pair(row)?;
                    parameters.insert(name.clone(), parse_value(value).map_err(|e| e.to_string())?);
                }
                self.parameters = Some(parameters);
                Ok(())
            }
            Kind::Procedure => {
                let (columns, rows) = typed_table(step)?;
                self.procedures.push(Procedure {
                    signature: captures[0].to_string(),
                    columns,
                    rows,
                });
                Ok(())
            }
            Kind::Query => {
                if self.query.is_some() {
                    return Err("more than one query".into());
                }
                self.query = Some(docstring(step)?);
                Ok(())
            }
            Kind::ControlQuery => {
                if self.expected.is_none() || self.control_query.is_some() {
                    return Err("control query must follow one checked query".into());
                }
                self.control_query = Some(docstring(step)?);
                Ok(())
            }
            Kind::Rows(ordering, lists) => {
                let (columns, rows) = typed_table(step)?;
                self.expect(Expected::Rows {
                    columns,
                    rows,
                    ordering,
                    lists,
                })
            }
            Kind::EmptyResult => self.expect(Expected::Empty),
            Kind::Error => self.expect(Expected::Error {
                kind: captures[0].to_string(),
                phase: phase(captures[1])?,
                detail: captures[2].to_string(),
            }),
            Kind::SideEffects => self.set_side_effects(side_effects_from(table(step)?)?),
            Kind::NoSideEffects => self.set_side_effects(SideEffects::default()),
        }
    }

    fn set_graph(&mut self, graph: GraphSetup) -> Result<(), String> {
        if self.graph.is_some() {
            return Err("graph given twice".into());
        }
        self.graph = Some(graph);
        Ok(())
    }

    fn set_side_effects(&mut self, side_effects: SideEffects) -> Result<(), String> {
        if self.query.is_none() || self.side_effects.is_some() {
            return Err("side effects must follow the query, once".into());
        }
        self.side_effects = Some(side_effects);
        Ok(())
    }

    fn expect(&mut self, expected: Expected) -> Result<(), String> {
        let slot = if self.control_query.is_some() {
            &mut self.control_expected
        } else if self.query.is_some() {
            &mut self.expected
        } else {
            return Err("expectation before the query".into());
        };
        if slot.is_some() {
            return Err("expectation given twice".into());
        }
        *slot = Some(expected);
        Ok(())
    }

    fn finish(self) -> Result<Parts, String> {
        let graph = self.graph.ok_or("scenario has no graph step")?;
        let query = self.query.ok_or("scenario has no query")?;
        let expected = self.expected.ok_or("scenario does not expect anything")?;
        let control = match (self.control_query, self.control_expected) {
            (Some(query), Some(expected)) => Some(Control { query, expected }),
            (None, _) => None,
            (Some(_), None) => return Err("control query does not expect anything".into()),
        };
        let side_effects = match (&expected, self.side_effects) {
            (Expected::Error { .. }, None) => Some(SideEffects::default()),
            (_, side_effects) => side_effects,
        };
        Ok(Parts {
            graph,
            procedures: self.procedures,
            setup: self.setup,
            parameters: self.parameters.unwrap_or_default(),
            query,
            expected,
            side_effects,
            control,
        })
    }
}

fn docstring(step: &RawStep) -> Result<String, String> {
    step.docstring
        .clone()
        .ok_or_else(|| format!("step `{}` needs a doc string", step.text))
}

fn table(step: &RawStep) -> Result<&[Vec<String>], String> {
    step.table
        .as_deref()
        .ok_or_else(|| format!("step `{}` needs a table", step.text))
}

fn pair(row: &[String]) -> Result<[&String; 2], String> {
    match row {
        [name, value] => Ok([name, value]),
        _ => Err(format!("expected two columns, found {}", row.len())),
    }
}

type ValueTable = (Vec<String>, Vec<Vec<Value>>);

fn typed_table(step: &RawStep) -> Result<ValueTable, String> {
    let (columns, body) = table(step)?
        .split_first()
        .ok_or("table has no header row")?;
    let mut rows = Vec::new();
    for row in body {
        if row.len() != columns.len() {
            return Err(format!(
                "row has {} cells but the header has {}",
                row.len(),
                columns.len()
            ));
        }
        rows.push(
            row.iter()
                .map(|cell| parse_value(cell).map_err(|e| e.to_string()))
                .collect::<Result<_, _>>()?,
        );
    }
    Ok((columns.clone(), rows))
}

fn phase(text: &str) -> Result<Phase, String> {
    match text {
        "compile time" => Ok(Phase::CompileTime),
        "runtime" => Ok(Phase::Runtime),
        "any time" => Ok(Phase::AnyTime),
        other => Err(format!("unknown error phase `{other}`")),
    }
}

fn side_effects_from(rows: &[Vec<String>]) -> Result<SideEffects, String> {
    let mut effects = SideEffects::default();
    for row in rows {
        let [name, count] = pair(row)?;
        let count: u64 = count
            .parse()
            .map_err(|_| format!("side effect count `{count}` is not a number"))?;
        let (counts, kind) = match name.split_at_checked(1) {
            Some(("+", kind)) => (&mut effects.added, kind),
            Some(("-", kind)) => (&mut effects.removed, kind),
            _ => return Err(format!("unknown side effect `{name}`")),
        };
        set_count(counts, kind, count).ok_or_else(|| format!("unknown side effect `{name}`"))?;
    }
    Ok(effects)
}

fn set_count(counts: &mut Counts, kind: &str, count: u64) -> Option<()> {
    let slot = match kind {
        "nodes" => &mut counts.nodes,
        "relationships" => &mut counts.relationships,
        "labels" => &mut counts.labels,
        "properties" => &mut counts.properties,
        _ => return None,
    };
    *slot = count;
    Some(())
}

#[cfg(test)]
#[path = "steps.test.rs"]
mod tests;
