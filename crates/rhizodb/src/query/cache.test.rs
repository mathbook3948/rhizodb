use std::sync::Arc;

use crate::gql::parser::parse;
use crate::procedure::Procedures;
use crate::result::Parameters;

use super::super::bind::bind;
use super::super::plan::Plan;
use super::CAPACITY;
use super::PlanCache;

fn plan() -> Arc<Plan> {
    let query = "RETURN 1";
    Arc::new(
        bind(
            &parse(query).unwrap(),
            query,
            &Parameters::new(),
            &Procedures::new(),
        )
        .unwrap(),
    )
}

#[test]
fn a_full_cache_replaces_the_query_used_longest_ago() {
    let cache = PlanCache::default();
    for index in 0..CAPACITY {
        cache.insert(&format!("query {index}"), plan());
    }
    assert!(cache.get("query 0").is_some());
    cache.insert("one more", plan());
    assert_eq!(cache.len(), CAPACITY);
    assert!(cache.get("query 0").is_some());
    assert!(cache.get("query 1").is_none());
    assert!(cache.get("one more").is_some());
}

#[test]
fn clearing_forgets_every_plan() {
    let cache = PlanCache::default();
    cache.insert("RETURN 1", plan());
    cache.clear();
    assert!(cache.get("RETURN 1").is_none());
}
