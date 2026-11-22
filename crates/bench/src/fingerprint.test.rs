use super::*;

fn row(build: impl FnOnce(&mut RowHasher)) -> RowHasher {
    let mut hasher = RowHasher::default();
    build(&mut hasher);
    hasher
}

fn of(rows: Vec<RowHasher>) -> Fingerprint {
    let mut fingerprint = Fingerprint::default();
    for row in rows {
        fingerprint.absorb(row);
    }
    fingerprint
}

#[test]
fn row_order_does_not_change_the_fingerprint() {
    let a = || row(|h| h.int(1));
    let b = || row(|h| h.text("x"));

    assert_eq!(of(vec![a(), b()]), of(vec![b(), a()]));
}

#[test]
fn a_different_value_or_row_count_changes_the_fingerprint() {
    let one = of(vec![row(|h| h.int(1))]);

    assert_ne!(one, of(vec![row(|h| h.int(2))]));
    assert_ne!(one, of(vec![row(|h| h.int(1)), row(|h| h.int(1))]));
    assert_ne!(one, of(vec![row(|h| h.float(1.0))]));
    assert_ne!(one, of(vec![row(|h| h.text("1"))]));
}

#[test]
fn floats_that_differ_below_six_decimals_hash_alike() {
    let a = of(vec![row(|h| h.float(2.4999999))]);
    let b = of(vec![row(|h| h.float(2.5))]);

    assert_eq!(a, b);
    assert_ne!(a, of(vec![row(|h| h.float(2.51))]));
}

#[test]
fn list_boundaries_are_part_of_the_hash() {
    let flat = row(|h| {
        h.int(1);
        h.int(2);
    });
    let nested = row(|h| {
        h.open_list();
        h.int(1);
        h.int(2);
        h.close_list();
    });

    assert_ne!(of(vec![flat]), of(vec![nested]));
}

#[test]
fn merging_adds_rows_and_checksums() {
    let a = of(vec![row(|h| h.int(1))]);
    let b = of(vec![row(|h| h.int(2))]);

    assert_eq!(a.merge(b), of(vec![row(|h| h.int(1)), row(|h| h.int(2))]));
}
