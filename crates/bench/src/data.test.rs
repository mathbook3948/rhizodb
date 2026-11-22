use super::*;
use crate::scale::Scale;

fn dataset() -> Dataset {
    Dataset::generate(3, Scale::Tiny.plan())
}

#[test]
fn a_seed_always_gives_the_same_dataset() {
    let plan = Scale::Tiny.plan();

    assert_eq!(Dataset::generate(3, plan), Dataset::generate(3, plan));
    assert_ne!(Dataset::generate(3, plan), Dataset::generate(4, plan));
}

#[test]
fn counts_follow_the_plan() {
    let plan = Scale::Tiny.plan();
    let data = dataset();

    assert_eq!(data.people.len(), plan.persons);
    assert_eq!(data.cities.len(), plan.cities);
    assert_eq!(data.knows.len(), plan.persons * plan.knows_per_person);
    assert!(data.lives_in.len() < plan.persons);
    assert!(data.lives_in.len() > plan.persons / 2);
}

#[test]
fn edges_only_join_nodes_that_exist_and_never_loop() {
    let plan = Scale::Tiny.plan();
    let data = dataset();
    let persons = 1..=plan.persons as i64;
    let cities = 1..=plan.cities as i64;

    assert!(data.knows.iter().all(|edge| {
        persons.contains(&edge.from) && persons.contains(&edge.to) && edge.from != edge.to
    }));
    assert!(
        data.lives_in
            .iter()
            .all(|edge| persons.contains(&edge.person) && cities.contains(&edge.city))
    );
}

#[test]
fn in_degree_is_skewed_toward_hubs() {
    let plan = Scale::Small.plan();
    let data = Dataset::generate(3, plan);
    let mut degree = vec![0usize; plan.persons + 1];
    for edge in &data.knows {
        degree[edge.to as usize] += 1;
    }
    let hub = degree.iter().copied().max().unwrap_or(0);

    assert!(hub > 10 * plan.knows_per_person, "{hub}");
}

#[test]
fn persons_have_embeddings_bios_and_sometimes_no_age() {
    let data = dataset();

    assert!(data.people.iter().all(|p| p.embedding.len() == 64));
    assert!(data.people.iter().all(|p| p.bio.split(' ').count() == 20));
    assert!(data.people.iter().any(|p| p.age.is_none()));
    assert!(data.people.iter().any(|p| p.age.is_some()));
    assert!(
        data.people
            .iter()
            .any(|p| p.bio.contains("서울") || p.bio.contains("데이터"))
    );
}

#[test]
fn facts_count_what_a_load_of_the_prefix_holds() {
    let data = dataset();
    let facts = data.facts(data.loaded_in_full());

    assert_eq!(facts.persons, 200);
    assert_eq!(facts.knows, 2000);
    assert_eq!(
        facts.aged,
        data.people.iter().filter(|p| p.age.is_some()).count() as u64
    );
    let prefix = data.facts(Loaded {
        persons: 10,
        cities: 0,
        knows: 5,
        lives_in: 7,
    });
    assert_eq!((prefix.persons, prefix.cities), (10, 0));
    assert_eq!((prefix.knows, prefix.lives_in), (5, 7));
}

#[test]
fn csv_export_has_a_header_and_one_line_per_record() {
    let data = dataset();
    let dir = std::env::temp_dir().join(format!("bench-csv-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    data.write_csv(&dir).unwrap();
    let lines = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .unwrap()
            .lines()
            .count()
    };

    assert_eq!(lines(PERSONS_CSV), 1 + data.people.len());
    assert_eq!(lines(CITIES_CSV), 1 + data.cities.len());
    assert_eq!(lines(KNOWS_CSV), 1 + data.knows.len());
    assert_eq!(lines(LIVES_IN_CSV), 1 + data.lives_in.len());
    let _ = std::fs::remove_dir_all(dir);
}
