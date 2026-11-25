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
    assert!(
        data.people
            .iter()
            .all(|p| (20..=24).contains(&p.bio.split(' ').count()))
    );
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

#[test]
fn no_pair_of_persons_has_two_knows_edges_the_same_way() {
    let data = Dataset::generate(3, Scale::Small.plan());
    let pairs: std::collections::HashSet<(i64, i64)> =
        data.knows.iter().map(|edge| (edge.from, edge.to)).collect();

    assert_eq!(pairs.len(), data.knows.len());
}

#[test]
fn rare_terms_have_fewer_than_ten_hits_and_the_truth_lists_every_one() {
    let data = Dataset::generate(3, Scale::Small.plan());

    for (index, term) in data.terms.iter().enumerate() {
        let hits: Vec<i64> = data
            .people
            .iter()
            .filter(|person| person.bio.split(' ').any(|word| word == term))
            .map(|person| person.id)
            .collect();
        assert_eq!(data.keyword_truth[index], hits, "{term}");
    }
    let rare = data
        .keyword_truth
        .iter()
        .filter(|hits| !hits.is_empty() && hits.len() < SEARCH_LIMIT)
        .count();
    assert!(rare >= 4, "{rare}");
    assert!(
        data.keyword_truth
            .iter()
            .any(|hits| hits.len() > SEARCH_LIMIT)
    );
}

#[test]
fn the_vector_truth_is_the_nearest_persons_by_cosine_distance_nearest_first() {
    let data = dataset();
    let vector = &data.query_vectors[0];
    let norm = |values: &[f64]| values.iter().map(|value| value * value).sum::<f64>().sqrt();
    let distance = |id: i64| {
        let person = &data.people[usize::try_from(id - 1).unwrap()];
        let dot: f64 = person
            .embedding
            .iter()
            .zip(vector)
            .map(|(a, b)| a * b)
            .sum();
        1.0 - dot / (norm(&person.embedding) * norm(vector))
    };
    let truth = &data.vector_truth[0];

    assert_eq!(truth.len(), SEARCH_LIMIT);
    assert!(
        truth
            .windows(2)
            .all(|pair| distance(pair[0]) <= distance(pair[1]))
    );
    let worst = distance(*truth.last().unwrap());
    assert!(
        data.people
            .iter()
            .filter(|person| !truth.contains(&person.id))
            .all(|person| distance(person.id) >= worst)
    );
}

#[test]
fn a_csv_file_with_its_final_name_is_always_complete() {
    let data = dataset();
    let dir = std::env::temp_dir().join(format!("bench-csv-atomic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    data.write_csv(&dir).unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}
