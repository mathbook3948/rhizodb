use super::*;

#[test]
fn a_seed_always_gives_the_same_sequence() {
    let mut first = Random::new(7);
    let mut second = Random::new(7);

    let a: Vec<u64> = (0..5).map(|_| first.next_u64()).collect();
    let b: Vec<u64> = (0..5).map(|_| second.next_u64()).collect();

    assert_eq!(a, b);
    assert_ne!(Random::new(8).next_u64(), a[0]);
}

#[test]
fn between_stays_inside_its_bounds() {
    let mut random = Random::new(1);

    for _ in 0..1000 {
        let value = random.between(-3, 4);
        assert!((-3..=4).contains(&value), "{value}");
    }
    assert_eq!(random.between(5, 5), 5);
}

#[test]
fn skewed_values_stay_below_the_bound_and_favor_small_ones() {
    let mut random = Random::new(2);
    let values: Vec<usize> = (0..10_000).map(|_| random.skewed(100, 3)).collect();

    assert!(values.iter().all(|&value| value < 100));
    let low = values.iter().filter(|&&value| value < 10).count();
    let high = values.iter().filter(|&&value| value >= 90).count();
    assert!(low > 10 * high, "{low} {high}");
}

#[test]
fn pick_returns_none_only_for_an_empty_slice() {
    let mut random = Random::new(1);

    assert_eq!(random.pick::<u8>(&[]), None);
    assert_eq!(random.pick(&[9]), Some(&9));
}
