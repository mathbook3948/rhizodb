use std::path::Path;

use crate::engine::Dialect;
use crate::engine::Engine;
use crate::engine::EngineError;
use crate::engine::EngineKind;
use crate::engine::Instance;

/// An engine whose cargo feature is off. It opens nothing and says which feature to enable, so
/// the report shows it as unavailable instead of leaving it out.
#[derive(Clone, Copy, Debug)]
pub struct Absent {
    kind: EngineKind,
    dialect: Dialect,
    feature: &'static str,
}

impl Absent {
    /// The stand-in for `kind`, which the cargo feature `feature` builds.
    pub fn new(kind: EngineKind, dialect: Dialect, feature: &'static str) -> Absent {
        Absent {
            kind,
            dialect,
            feature,
        }
    }
}

impl Engine for Absent {
    fn kind(&self) -> EngineKind {
        self.kind
    }

    fn dialect(&self) -> Dialect {
        self.dialect
    }

    fn open(&self, _dir: &Path) -> Result<Box<dyn Instance>, EngineError> {
        Err(EngineError::Unavailable(format!(
            "built without the `{}` cargo feature, run with `--features {}`",
            self.feature, self.feature
        )))
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({ "built": false, "feature": self.feature })
    }
}
