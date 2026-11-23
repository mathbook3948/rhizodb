//! Kills a child process that writes through the public API at varied points, then reopens the
//! file and checks that every transaction it reported as committed is there in full, that
//! nothing it had not committed is, and that the indexes agree with the data they cover.

mod support;

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::thread::sleep;
use std::time::Duration;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use rhizodb::VectorMetric;
use support::*;

const DATABASE_ENV: &str = "RHIZODB_KILL_DATABASE";
const HOLD_ENV: &str = "RHIZODB_KILL_HOLD_AFTER";
const ITEMS_PER_BATCH: i64 = 12;
const PRUNE_EVERY: i64 = 5;
const PRUNE_LAG: i64 = 3;
const WRITER_CAP: i64 = 100_000;
const FAST_TRIALS: u64 = 12;

fn parameters(entries: &[(&str, Value)]) -> Parameters {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn create_indexes(db: &Database) {
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    db.create_keyword_index("item_text", IndexTarget::Label("Item"), &["text"])
        .unwrap();
    db.create_vector_index(
        "item_vec",
        IndexTarget::Label("Item"),
        "vec",
        3,
        VectorMetric::Euclidean,
    )
    .unwrap();
}

fn is_pruned(batch: i64, committed: i64) -> bool {
    let pruner = batch + PRUNE_LAG;
    pruner < committed && pruner % PRUNE_EVERY == 0
}

/// Iteration `i` as one write transaction of several queries.
fn write_iteration(db: &Database, i: i64) {
    let mut tx = db.begin_write().unwrap();
    let at = parameters(&[("i", Value::Integer(i))]);
    tx.execute("INSERT (:Batch {i: $i, payload: $payload})", &{
        let mut p = at.clone();
        p.insert("payload".to_owned(), Value::String("x".repeat(3_000)));
        p
    })
    .unwrap();
    tx.execute(
        "MATCH (b:Batch {i: $i}) UNWIND range(1, $n) AS j \
         INSERT (b)-[:HAS {j: j}]->(:Item {i: $i, j: j, text: 'word batch' + toString($i), \
         vec: [toFloat($i), toFloat(j), 1.0]})",
        &{
            let mut p = at.clone();
            p.insert("n".to_owned(), Value::Integer(ITEMS_PER_BATCH));
            p
        },
    )
    .unwrap();
    tx.execute(
        "MATCH (c:Counter) SET c.n = c.n + 1, c.sum = c.sum + $i",
        &at,
    )
    .unwrap();
    if i % PRUNE_EVERY == 0 && i >= PRUNE_LAG {
        tx.execute(
            "MATCH (b:Batch {i: $old})-[:HAS]->(item:Item) DETACH DELETE item SET b:Pruned",
            &parameters(&[("old", Value::Integer(i - PRUNE_LAG))]),
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn open_for_writing(path: &Path) -> Database {
    let db = Database::open(path).unwrap();
    if run(&db, "MATCH (c:Counter) RETURN c").is_empty() {
        run(&db, "INSERT (:Counter {n: 0, sum: 0})");
    }
    create_indexes(&db);
    db
}

#[test]
#[ignore = "child process of the kill tests, selected by RHIZODB_KILL_DATABASE"]
fn kill_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let hold_after: Option<i64> = std::env::var(HOLD_ENV).ok().map(|n| n.parse().unwrap());
    let db = open_for_writing(Path::new(&path));
    let start = ints(&run(&db, "MATCH (c:Counter) RETURN c.n"))[0];
    let mut stdout = std::io::stdout();
    writeln!(stdout, "ready").unwrap();
    stdout.flush().unwrap();
    for i in start..WRITER_CAP {
        if hold_after == Some(i) {
            let mut tx = db.begin_write().unwrap();
            tx.execute(
                "UNWIND range(1, 500) AS g INSERT (:Ghost {g: g})",
                &Parameters::new(),
            )
            .unwrap();
            tx.execute("MATCH (c:Counter) SET c.n = -1", &Parameters::new())
                .unwrap();
            writeln!(stdout, "holding").unwrap();
            stdout.flush().unwrap();
            loop {
                sleep(Duration::from_secs(60));
            }
        }
        write_iteration(&db, i);
        writeln!(stdout, "committed {i}").unwrap();
        stdout.flush().unwrap();
    }
}

struct Writer {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Writer {
    fn spawn(path: &Path, hold_after: Option<i64>) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["kill_child", "--exact", "--ignored", "--nocapture"])
            .env(DATABASE_ENV, path)
            .stdout(Stdio::piped());
        if let Some(hold_after) = hold_after {
            command.env(HOLD_ENV, hold_after.to_string());
        }
        let mut child = command.spawn().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdout }
    }

    fn next_line(&mut self) -> Option<String> {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).unwrap_or(0);
        (read > 0 && line.ends_with('\n')).then(|| line.trim_end().to_owned())
    }

    fn wait_for(&mut self, wanted: &str) {
        while let Some(line) = self.next_line() {
            if line == wanted {
                return;
            }
        }
        panic!("the writer exited before printing {wanted:?}");
    }

    fn next_committed(&mut self) -> Option<i64> {
        loop {
            let line = self.next_line()?;
            if let Some(number) = line.strip_prefix("committed ") {
                return Some(number.parse().unwrap());
            }
        }
    }

    fn kill_and_collect_committed(mut self) -> Vec<i64> {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        std::iter::from_fn(|| self.next_committed()).collect()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn count(db: &Database, query: &str, parameters: &Parameters) -> i64 {
    let rows = db.execute(query, parameters).unwrap().rows().to_vec();
    ints(&rows)[0]
}

/// Checks the reopened database against the iterations it holds and returns how many it holds.
fn verify(db: &Database) -> Result<i64, String> {
    let none = Parameters::new();
    let counter = run(db, "MATCH (c:Counter) RETURN c.n, c.sum");
    let [row] = counter.as_slice() else {
        return Err(format!("counter rows {counter:?}"));
    };
    let (Value::Integer(n), Value::Integer(sum)) = (&row[0], &row[1]) else {
        return Err(format!("counter {row:?}"));
    };
    let n = *n;
    if *sum != (0..n).sum::<i64>() {
        return Err(format!("counter sum {sum} for {n} iterations"));
    }
    let ghosts = count(db, "MATCH (g:Ghost) RETURN count(g)", &none);
    if ghosts != 0 {
        return Err(format!(
            "{ghosts} nodes of an uncommitted transaction survived"
        ));
    }
    let batches = ints(&run(db, "MATCH (b:Batch) RETURN b.i ORDER BY b.i"));
    if batches != (0..n).collect::<Vec<_>>() {
        return Err(format!("batches {batches:?} for {n} iterations"));
    }
    let mut live_items = 0;
    for i in 0..n {
        let at = parameters(&[("i", Value::Integer(i))]);
        let expected = if is_pruned(i, n) { 0 } else { ITEMS_PER_BATCH };
        live_items += expected;
        let through_edges = count(
            db,
            "MATCH (:Batch {i: $i})-[:HAS]->(item) RETURN count(item)",
            &at,
        );
        let through_index = count(db, "MATCH (item:Item {i: $i}) RETURN count(item)", &at);
        let through_scan = count(
            db,
            "MATCH (item:Item) WHERE item.i + 0 = $i RETURN count(item)",
            &at,
        );
        if [through_edges, through_index, through_scan] != [expected; 3] {
            return Err(format!(
                "batch {i}: expected {expected} items, found {through_edges} by edges, \
                 {through_index} by the property index, {through_scan} by a scan"
            ));
        }
        let pruned = count(db, "MATCH (b:Batch:Pruned {i: $i}) RETURN count(b)", &at);
        if pruned != i64::from(is_pruned(i, n)) {
            return Err(format!("batch {i}: Pruned label count {pruned}"));
        }
    }
    let keyword = count(
        db,
        "CALL db.index.keyword.queryNodes('item_text', 'word', 1000000) YIELD node \
         RETURN count(node)",
        &none,
    );
    let vector = count(
        db,
        "CALL db.index.vector.queryNodes('item_vec', [0.0, 0.0, 0.0], 1000000) YIELD node \
         RETURN count(node)",
        &none,
    );
    let relationships = count(db, "MATCH ()-[r:HAS]->() RETURN count(r)", &none);
    if [keyword, vector, relationships] != [live_items; 3] {
        return Err(format!(
            "{live_items} live items, but the keyword index finds {keyword}, the vector index \
             {vector}, and {relationships} HAS relationships remain"
        ));
    }
    Ok(n)
}

fn kill_point(rng: &mut Rng) -> (u64, u64) {
    (rng.below(40), rng.below(6_000))
}

#[test]
fn a_database_survives_a_writer_killed_between_and_inside_transactions() {
    let seed = seed();
    let mut rng = Rng::new(seed);
    let directory = tempfile::tempdir().unwrap();
    for trial in 0..cases(FAST_TRIALS) {
        let (lines_before_kill, delay_micros) = kill_point(&mut rng);
        let path = directory.path().join(format!("trial-{trial}.rhizodb"));
        let context = format!(
            "seed {seed} (replay with RHIZODB_TEST_SEED={seed}), trial {trial}, killed after \
             {lines_before_kill} lines and {delay_micros}us"
        );
        let mut writer = Writer::spawn(&path, None);
        writer.wait_for("ready");
        let mut reported: Vec<i64> = (0..lines_before_kill)
            .map(|_| writer.next_committed().expect("the writer exited early"))
            .collect();
        sleep(Duration::from_micros(delay_micros));
        reported.extend(writer.kill_and_collect_committed());
        let reported = i64::try_from(reported.len()).unwrap();
        let db = Database::open(&path).unwrap();
        let recovered = verify(&db).unwrap_or_else(|reason| panic!("{context}: {reason}"));
        assert!(
            recovered == reported || recovered == reported + 1,
            "{context}: {reported} reported committed, {recovered} recovered"
        );
        drop(db);

        let db = open_for_writing(&path);
        write_iteration(&db, recovered);
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(
            verify(&db),
            Ok(recovered + 1),
            "{context}: after a write on the recovered file and a reopen"
        );
    }
}

#[test]
fn an_open_write_transaction_killed_before_commit_leaves_no_trace() {
    let directory = tempfile::tempdir().unwrap();
    for hold_after in [0, 3] {
        let path = directory.path().join(format!("hold-{hold_after}.rhizodb"));
        let mut writer = Writer::spawn(&path, Some(hold_after));
        writer.wait_for("holding");
        writer.kill_and_collect_committed();
        let db = Database::open(&path).unwrap();
        assert_eq!(
            verify(&db),
            Ok(hold_after),
            "killed while holding after {hold_after}"
        );
    }
}

#[test]
fn the_verifier_accepts_whole_iterations_and_rejects_partial_ones() {
    let damages = [
        "MATCH (item:Item {i: 4, j: 2}) DETACH DELETE item",
        "MATCH (b:Batch {i: 6}) DETACH DELETE b",
        "MATCH (c:Counter) SET c.n = 6",
        "MATCH (c:Counter) SET c.sum = 0",
        "MATCH (:Item {i: 5})-[r:HAS]-() DELETE r",
        "MATCH (b:Batch {i: 2}) REMOVE b:Pruned",
        "INSERT (:Ghost {g: 1})",
    ];
    for damage in damages {
        let directory = tempfile::tempdir().unwrap();
        let db = open_for_writing(&directory.path().join("verifier.rhizodb"));
        for i in 0..7 {
            write_iteration(&db, i);
            assert_eq!(verify(&db), Ok(i + 1));
        }
        run(&db, damage);
        assert!(verify(&db).is_err(), "the verifier accepted {damage}");
    }
}
