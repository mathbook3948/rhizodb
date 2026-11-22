use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::engine::Engine;
use crate::engine::Graph;
use crate::engine::QueryError;
use crate::engine::Rows;
use crate::scenario::Procedure;
use crate::side_effects::GraphSnapshot;
use crate::value::Value;

#[derive(Clone, Debug)]
pub struct Answer {
    pub query: String,
    pub result: Result<Rows, QueryError>,
    pub snapshot: Option<Result<GraphSnapshot, String>>,
}

impl Answer {
    pub fn rows(query: &str, columns: &[&str], rows: Vec<Vec<Value>>) -> Answer {
        let columns = columns.iter().map(|column| column.to_string()).collect();
        Answer {
            query: query.into(),
            result: Ok(Rows { columns, rows }),
            snapshot: None,
        }
    }

    pub fn raised(query: &str, kind: &str, detail: &str) -> Answer {
        Answer {
            query: query.into(),
            result: Err(QueryError::Raised {
                kind: kind.into(),
                detail: detail.into(),
            }),
            snapshot: None,
        }
    }

    pub fn then_graph(mut self, snapshot: GraphSnapshot) -> Answer {
        self.snapshot = Some(Ok(snapshot));
        self
    }

    pub fn then_unreadable_graph(mut self, reason: &str) -> Answer {
        self.snapshot = Some(Err(reason.into()));
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Registered(String),
    Executed(String, BTreeMap<String, Value>),
}

#[derive(Clone, Debug, Default)]
pub struct FakeEngine {
    answers: Arc<Vec<Answer>>,
    pub events: Arc<Mutex<Vec<Event>>>,
}

impl FakeEngine {
    pub fn new(answers: Vec<Answer>) -> FakeEngine {
        FakeEngine {
            answers: Arc::new(answers),
            events: Arc::default(),
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

impl Engine for FakeEngine {
    fn fresh_graph(&self) -> Box<dyn Graph> {
        Box::new(FakeGraph {
            answers: Arc::clone(&self.answers),
            events: Arc::clone(&self.events),
            state: Ok(GraphSnapshot::default()),
        })
    }
}

struct FakeGraph {
    answers: Arc<Vec<Answer>>,
    events: Arc<Mutex<Vec<Event>>>,
    state: Result<GraphSnapshot, String>,
}

impl Graph for FakeGraph {
    fn register_procedure(&mut self, procedure: &Procedure) -> Result<(), String> {
        if let Ok(mut events) = self.events.lock() {
            events.push(Event::Registered(procedure.signature.clone()));
        }
        Ok(())
    }

    fn execute(
        &mut self,
        query: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Rows, QueryError> {
        if let Ok(mut events) = self.events.lock() {
            events.push(Event::Executed(query.to_string(), parameters.clone()));
        }
        let answer = self.answers.iter().find(|answer| answer.query == query);
        let Some(answer) = answer else {
            return Err(QueryError::Unsupported);
        };
        if let Some(snapshot) = &answer.snapshot {
            self.state = snapshot.clone();
        }
        answer.result.clone()
    }

    fn snapshot(&self) -> Result<GraphSnapshot, String> {
        self.state.clone()
    }
}
