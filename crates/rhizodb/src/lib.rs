//! An embedded, schemaless property graph database queried with GQL.
//!
//! ```
//! use rhizodb::Database;
//! use rhizodb::Parameters;
//! use rhizodb::Value;
//!
//! let db = Database::in_memory()?;
//! db.execute(
//!     "INSERT (:Person {name: 'Ada'})-[:KNOWS]->(:Person {name: 'Grace'})",
//!     &Parameters::new(),
//! )?;
//!
//! let mut parameters = Parameters::new();
//! parameters.insert("name".to_owned(), Value::String("Ada".to_owned()));
//! let result = db.execute(
//!     "MATCH (a:Person {name: $name})-[:KNOWS]->(b) RETURN b.name AS friend",
//!     &parameters,
//! )?;
//! assert_eq!(result.columns(), ["friend"]);
//! assert_eq!(result.rows(), [vec![Value::String("Grace".to_owned())]]);
//! # Ok::<(), rhizodb::Error>(())
//! ```
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod database;
mod error;
pub mod gql;
mod graph;
mod query;
mod result;
mod storage;
mod value;

pub use database::Database;
pub use error::Error;
pub use error::ErrorKind;
pub use gql::error::SyntaxErrorKind;
pub use graph::NodeId;
pub use graph::RelationshipId;
pub use result::Node;
pub use result::Parameters;
pub use result::Path;
pub use result::QueryResult;
pub use result::Relationship;
pub use result::Value;
