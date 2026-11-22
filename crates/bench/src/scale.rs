/// A named size of the benchmark graph and of the work each workload does on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Tiny,
    Small,
    Medium,
    Large,
}

/// The counts that follow from a [`Scale`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    pub persons: usize,
    pub cities: usize,
    pub knows_per_person: usize,
    pub lookups: usize,
    pub traversals: usize,
    pub small_commits: usize,
    pub per_row_loads: usize,
    pub statement_edges: usize,
    pub searches: usize,
}

impl Scale {
    /// Reads `tiny`, `small`, `medium`, or `large`.
    pub fn parse(text: &str) -> Option<Scale> {
        match text {
            "tiny" => Some(Scale::Tiny),
            "small" => Some(Scale::Small),
            "medium" => Some(Scale::Medium),
            "large" => Some(Scale::Large),
            _ => None,
        }
    }

    /// The name `--scale` takes.
    pub fn name(self) -> &'static str {
        match self {
            Scale::Tiny => "tiny",
            Scale::Small => "small",
            Scale::Medium => "medium",
            Scale::Large => "large",
        }
    }

    /// The counts for this scale.
    ///
    /// RhizoDB has no property index, so a lookup by property scans every node with the label.
    /// The operation counts keep one run of each workload to seconds on RhizoDB at every scale.
    pub fn plan(self) -> Plan {
        let (persons, cities, lookups, traversals, small_commits, per_row_loads, statement_edges) =
            match self {
                Scale::Tiny => (200, 10, 20, 10, 50, 50, 100),
                Scale::Small => (2_000, 50, 100, 30, 500, 500, 100),
                Scale::Medium => (20_000, 100, 100, 20, 500, 500, 100),
                Scale::Large => (200_000, 500, 20, 5, 500, 500, 100),
            };
        Plan {
            persons,
            cities,
            knows_per_person: 10,
            lookups,
            traversals,
            small_commits,
            per_row_loads,
            statement_edges,
            searches: lookups,
        }
    }
}

#[cfg(test)]
#[path = "scale.test.rs"]
mod tests;
