use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use super::plan::Plan;

/// How many bound queries a [`PlanCache`] keeps.
pub(crate) const CAPACITY: usize = 64;

/// The bound plans of recently run query texts. When full, a new text replaces the one used
/// longest ago.
#[derive(Debug, Default)]
pub(crate) struct PlanCache {
    entries: Mutex<Entries>,
}

#[derive(Debug, Default)]
struct Entries {
    plans: HashMap<String, (Arc<Plan>, u64)>,
    clock: u64,
}

impl PlanCache {
    /// The plan bound for `query`, if it is kept.
    pub(crate) fn get(&self, query: &str) -> Option<Arc<Plan>> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.clock += 1;
        let clock = entries.clock;
        let (plan, used) = entries.plans.get_mut(query)?;
        *used = clock;
        Some(Arc::clone(plan))
    }

    /// Keeps `plan` as the plan bound for `query`.
    pub(crate) fn insert(&self, query: &str, plan: Arc<Plan>) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        entries.clock += 1;
        let clock = entries.clock;
        if entries.plans.len() >= CAPACITY && !entries.plans.contains_key(query) {
            let oldest = entries
                .plans
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(text, _)| text.clone());
            if let Some(oldest) = oldest {
                entries.plans.remove(&oldest);
            }
        }
        entries.plans.insert(query.to_owned(), (plan, clock));
    }

    /// Forgets every plan.
    pub(crate) fn clear(&mut self) {
        self.entries
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .plans
            .clear();
    }

    /// How many plans are kept.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .plans
            .len()
    }
}

#[cfg(test)]
#[path = "cache.test.rs"]
mod tests;
