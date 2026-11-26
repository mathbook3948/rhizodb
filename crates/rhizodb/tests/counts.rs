mod support;

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use proptest::prelude::*;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const LABELS: [&str; 3] = ["A", "B", "C"];

#[derive(Clone, Debug)]
enum Op {
    Create(u8, Option<i8>),
    CreateMany(u8, u16),
    Delete(usize),
    AddLabel(usize, usize),
    RemoveLabel(usize, usize),
    Check,
    Commit,
    Rollback,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u8..8, any::<Option<i8>>()).prop_map(|(labels, x)| Op::Create(labels, x)),
        1 => (0u8..8, 1u16..300).prop_map(|(labels, count)| Op::CreateMany(labels, count)),
        3 => any::<usize>().prop_map(Op::Delete),
        3 => (any::<usize>(), 0..LABELS.len()).prop_map(|(node, label)| Op::AddLabel(node, label)),
        3 => (any::<usize>(), 0..LABELS.len()).prop_map(|(node, label)| Op::RemoveLabel(node, label)),
        2 => Just(Op::Check),
        3 => Just(Op::Commit),
        1 => Just(Op::Rollback),
    ]
}

#[derive(Clone, Debug, Default)]
struct Model {
    nodes: BTreeMap<i64, (BTreeSet<&'static str>, bool)>,
    next: i64,
}

impl Model {
    fn pick(&self, index: usize) -> Option<i64> {
        let len = self.nodes.len();
        (len > 0).then(|| *self.nodes.keys().nth(index % len).unwrap_or(&0))
    }

    fn count(&self, label: Option<&str>) -> i64 {
        self.nodes
            .values()
            .filter(|(labels, _)| label.is_none_or(|label| labels.contains(label)))
            .count() as i64
    }
}

fn labels_of(mask: u8) -> BTreeSet<&'static str> {
    LABELS
        .iter()
        .enumerate()
        .filter(|(bit, _)| mask & (1 << bit) != 0)
        .map(|(_, name)| *name)
        .collect()
}

fn label_text(labels: &BTreeSet<&'static str>) -> String {
    labels.iter().map(|label| format!(":{label}")).collect()
}

fn apply(execute: &mut dyn FnMut(&str) -> Vec<Vec<Value>>, model: &mut Model, op: &Op) {
    match op {
        Op::Create(mask, x) => {
            let labels = labels_of(*mask);
            let k = model.next;
            model.next += 1;
            let x = x.map_or(String::new(), |x| format!(", x: {x}"));
            execute(&format!("CREATE ({} {{k: {k}{x}}})", label_text(&labels)));
            model.nodes.insert(k, (labels, !x.is_empty()));
        }
        Op::CreateMany(mask, count) => {
            let labels = labels_of(*mask);
            let first = model.next;
            model.next += i64::from(*count);
            execute(&format!(
                "UNWIND range({first}, {}) AS k CREATE ({} {{k: k}})",
                model.next - 1,
                label_text(&labels)
            ));
            for k in first..model.next {
                model.nodes.insert(k, (labels.clone(), false));
            }
        }
        Op::Delete(index) => {
            if let Some(k) = model.pick(*index) {
                execute(&format!("MATCH (n {{k: {k}}}) DELETE n"));
                model.nodes.remove(&k);
            }
        }
        Op::AddLabel(index, label) => {
            if let Some(k) = model.pick(*index) {
                execute(&format!("MATCH (n {{k: {k}}}) SET n:{}", LABELS[*label]));
                if let Some((labels, _)) = model.nodes.get_mut(&k) {
                    labels.insert(LABELS[*label]);
                }
            }
        }
        Op::RemoveLabel(index, label) => {
            if let Some(k) = model.pick(*index) {
                execute(&format!("MATCH (n {{k: {k}}}) REMOVE n:{}", LABELS[*label]));
                if let Some((labels, _)) = model.nodes.get_mut(&k) {
                    labels.remove(LABELS[*label]);
                }
            }
        }
        Op::Check | Op::Commit | Op::Rollback => {}
    }
}

fn check(
    execute: &mut dyn FnMut(&str) -> Vec<Vec<Value>>,
    model: &Model,
) -> Result<(), TestCaseError> {
    let total = model.count(None);
    prop_assert_eq!(
        execute("MATCH (n) RETURN count(*), count(n), count(n) + 1"),
        vec![vec![int(total), int(total), int(total + 1)]]
    );
    prop_assert_eq!(
        execute("MATCH (n) RETURN count(DISTINCT n)"),
        vec![vec![int(total)]]
    );
    prop_assert_eq!(
        execute("MATCH (n) WHERE n.k >= 0 RETURN count(n)"),
        vec![vec![int(total)]]
    );
    let with_x = model.nodes.values().filter(|(_, x)| *x).count() as i64;
    prop_assert_eq!(
        execute("MATCH (n) RETURN count(n.x)"),
        vec![vec![int(with_x)]]
    );
    for label in LABELS {
        let carrying = model.count(Some(label));
        prop_assert_eq!(
            execute(&format!("MATCH (n:{label}) RETURN count(*), count(n)")),
            vec![vec![int(carrying), int(carrying)]],
            "count of {}",
            label
        );
        prop_assert_eq!(
            execute(&format!("MATCH (n:{label}) WHERE n.k >= 0 RETURN count(n)")),
            vec![vec![int(carrying)]],
            "filtered count of {}",
            label
        );
        prop_assert_eq!(
            execute(&format!(
                "UNWIND [1, 2, 3] AS i MATCH (n:{label}) RETURN count(n) AS c"
            )),
            vec![vec![int(3 * carrying)]],
            "count of {} per row",
            label
        );
    }
    prop_assert_eq!(
        execute("MATCH (n:Missing) RETURN count(n), count(*)"),
        vec![vec![int(0), int(0)]]
    );
    Ok(())
}

fn run_ops(ops: &[Op]) -> Result<(), TestCaseError> {
    let db = db();
    let mut committed = Model::default();
    let mut rest = ops.iter().peekable();
    while rest.peek().is_some() {
        let mut pending = committed.clone();
        let mut tx = db.begin_write().unwrap();
        let mut execute = |query: &str| match tx.execute(query, &Parameters::new()) {
            Ok(result) => result.rows().to_vec(),
            Err(error) => panic!("{query}: {error}"),
        };
        let mut end = Op::Commit;
        for op in rest.by_ref() {
            match op {
                Op::Commit | Op::Rollback => {
                    end = op.clone();
                    break;
                }
                Op::Check => check(&mut execute, &pending)?,
                op => apply(&mut execute, &mut pending, op),
            }
        }
        check(&mut execute, &pending)?;
        if matches!(end, Op::Rollback) {
            tx.rollback().unwrap();
        } else {
            tx.commit().unwrap();
            committed = pending;
        }
        check(&mut |query: &str| run(&db, query), &committed)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    #[ignore = "left out of mise run check, run by mise run random"]
    fn counts_from_node_meta_agree_with_counting_each_match(
        ops in prop::collection::vec(op(), 1..60),
    ) {
        run_ops(&ops)?;
    }
}

#[test]
fn counts_agree_across_groups_after_many_one_node_commits() {
    let db = db();
    let mut model = Model::default();
    apply(
        &mut |query: &str| run(&db, query),
        &mut model,
        &Op::CreateMany(0b011, 4200),
    );
    for round in 0..150usize {
        let op = match round % 3 {
            0 => Op::AddLabel(round * 31, 2),
            1 => Op::Delete(round * 17),
            _ => Op::Create((round % 8) as u8, None),
        };
        apply(&mut |query: &str| run(&db, query), &mut model, &op);
    }
    check(&mut |query: &str| run(&db, query), &model).unwrap();
}
