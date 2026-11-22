use super::*;

#[test]
fn every_scale_name_reads_back() {
    for scale in [Scale::Tiny, Scale::Small, Scale::Medium, Scale::Large] {
        assert_eq!(Scale::parse(scale.name()), Some(scale));
    }
    assert_eq!(Scale::parse("huge"), None);
}

#[test]
fn person_counts_follow_the_documented_sizes() {
    assert_eq!(Scale::Tiny.plan().persons, 200);
    assert_eq!(Scale::Small.plan().persons, 2_000);
    assert_eq!(Scale::Medium.plan().persons, 20_000);
    assert_eq!(Scale::Large.plan().persons, 200_000);
}
