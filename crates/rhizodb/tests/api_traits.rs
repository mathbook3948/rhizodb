//! The auto traits and standard traits the public types promise, checked at compile time so a
//! change of an internal type cannot drop one unnoticed.

use std::error::Error as StdError;
use std::fmt::Debug;

use rhizodb::Database;
use rhizodb::Date;
use rhizodb::DateTime;
use rhizodb::Duration;
use rhizodb::Error;
use rhizodb::ErrorKind;
use rhizodb::IndexTarget;
use rhizodb::LocalDateTime;
use rhizodb::LocalTime;
use rhizodb::Node;
use rhizodb::NodeId;
use rhizodb::OpenOptions;
use rhizodb::Parameters;
use rhizodb::Path;
use rhizodb::Procedure;
use rhizodb::ProcedureOutput;
use rhizodb::QueryResult;
use rhizodb::ReadTransaction;
use rhizodb::Relationship;
use rhizodb::RelationshipId;
use rhizodb::SyntaxErrorKind;
use rhizodb::Temporal;
use rhizodb::Time;
use rhizodb::Value;
use rhizodb::ValueType;
use rhizodb::VectorMetric;

fn shared<T: Send + Sync + Debug>() {}

fn moved<T: Send + Debug>() {}

fn error<T: StdError + Send + Sync + 'static>() {}

#[test]
fn values_and_handles_move_and_share_across_threads() {
    shared::<Database>();
    shared::<OpenOptions>();
    shared::<Parameters>();
    shared::<QueryResult>();
    shared::<Value>();
    shared::<Node>();
    shared::<Relationship>();
    shared::<Path>();
    shared::<NodeId>();
    shared::<RelationshipId>();
    shared::<Temporal>();
    shared::<Date>();
    shared::<LocalTime>();
    shared::<Time>();
    shared::<LocalDateTime>();
    shared::<DateTime>();
    shared::<Duration>();
    shared::<Procedure>();
    shared::<ProcedureOutput>();
    shared::<ValueType>();
    shared::<IndexTarget<'static>>();
    shared::<VectorMetric>();
    shared::<ErrorKind>();
    shared::<SyntaxErrorKind>();
    moved::<ReadTransaction<'static>>();
}

#[test]
fn the_error_is_a_standard_error_that_crosses_threads() {
    error::<Error>();
}
