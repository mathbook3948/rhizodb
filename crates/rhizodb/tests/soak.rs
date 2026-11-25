//! A long soak: a child process writes, overwrites, and deletes through the public API, with
//! values large enough that RhizoDB's own engine checkpoints in the background, while the parent
//! kills it with SIGKILL at random points, reads the file at random moments in between, and
//! sometimes lets it close cleanly. After every kill the reopened file must hold exactly the
//! iterations the child reported as committed, or one more, with every index agreeing with the
//! data. It runs only when `RHIZODB_SOAK_SECS` gives its length, as `mise run soak` does.

mod support;

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use rhizodb::Database;
use rhizodb::IndexTarget;
use rhizodb::Parameters;
use rhizodb::Value;
use support::*;

const SECONDS_ENV: &str = "RHIZODB_SOAK_SECS";
const DATABASE_ENV: &str = "RHIZODB_SOAK_DATABASE";
const ITEMS: i64 = 8;
const KEEP: i64 = 40;

fn parameters(entries: &[(&str, Value)]) -> Parameters {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

/// The payload iteration `i` writes, and the one it rewrites iteration `i - KEEP / 2` to.
fn payload(i: i64, round: i64) -> String {
    let length = ((i * 7919 + round * 104_729) % 300_000) as usize;
    let letter = char::from(b'a' + ((i + round) % 26) as u8);
    letter.to_string().repeat(length)
}

fn write_iteration(db: &Database, i: i64) {
    let mut tx = db.begin_write().unwrap();
    let at = parameters(&[("i", Value::Integer(i))]);
    tx.execute(
        "INSERT (:Batch {i: $i, payload: $payload, round: 0})",
        &parameters(&[
            ("i", Value::Integer(i)),
            ("payload", Value::String(payload(i, 0))),
        ]),
    )
    .unwrap();
    tx.execute(
        "MATCH (b:Batch {i: $i}) UNWIND range(1, $n) AS j \
         INSERT (b)-[:HAS {j: j}]->(:Item {i: $i, j: j, text: 'word w' + toString($i)})",
        &parameters(&[("i", Value::Integer(i)), ("n", Value::Integer(ITEMS))]),
    )
    .unwrap();
    if i >= KEEP / 2 {
        let old = i - KEEP / 2;
        tx.execute(
            "MATCH (b:Batch {i: $old}) SET b.payload = $payload, b.round = 1",
            &parameters(&[
                ("old", Value::Integer(old)),
                ("payload", Value::String(payload(old, 1))),
            ]),
        )
        .unwrap();
    }
    if i >= KEEP {
        tx.execute(
            "MATCH (b:Batch {i: $old}) OPTIONAL MATCH (b)-[:HAS]->(item) DETACH DELETE b, item",
            &parameters(&[("old", Value::Integer(i - KEEP))]),
        )
        .unwrap();
    }
    tx.execute("MATCH (c:Counter) SET c.n = $i + 1", &at)
        .unwrap();
    tx.commit().unwrap();
}

fn open_for_writing(path: &Path) -> Database {
    let db = Database::open(path).unwrap();
    if run(&db, "MATCH (c:Counter) RETURN c").is_empty() {
        run(&db, "INSERT (:Counter {n: 0})");
    }
    db.create_property_index("item_i", IndexTarget::Label("Item"), "i")
        .unwrap();
    db.create_property_index("batch_i", IndexTarget::Label("Batch"), "i")
        .unwrap();
    db.create_keyword_index("item_text", IndexTarget::Label("Item"), &["text"])
        .unwrap();
    db
}

#[test]
#[ignore = "child process of the soak test, selected by RHIZODB_SOAK_DATABASE"]
fn soak_child() {
    let Ok(path) = std::env::var(DATABASE_ENV) else {
        return;
    };
    let db = open_for_writing(Path::new(&path));
    let start = ints(&run(&db, "MATCH (c:Counter) RETURN c.n"))[0];
    let mut stdout = std::io::stdout();
    writeln!(stdout, "ready {start}").unwrap();
    stdout.flush().unwrap();
    let stop: Option<i64> = std::env::var("RHIZODB_SOAK_STOP")
        .ok()
        .map(|n| n.parse().unwrap());
    for i in start.. {
        if stop == Some(i) {
            break;
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
    fn spawn(path: &Path, stop: Option<i64>) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["soak_child", "--exact", "--ignored", "--nocapture"])
            .env(DATABASE_ENV, path)
            .stdout(Stdio::piped());
        if let Some(stop) = stop {
            command.env("RHIZODB_SOAK_STOP", stop.to_string());
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

    fn last_committed(&mut self, mut last: Option<i64>) -> Option<i64> {
        while let Some(line) = self.next_line() {
            if let Some(number) = line.strip_prefix("committed ") {
                last = Some(number.parse().unwrap());
            }
        }
        last
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn count(db: &Database, query: &str, parameters: &Parameters) -> i64 {
    ints(db.execute(query, parameters).unwrap().rows())[0]
}

/// Checks a database against the iterations it holds and returns how many it holds.
fn verify(db: &Database) -> Result<i64, String> {
    let none = Parameters::new();
    let read = db.begin_read().map_err(|error| error.to_string())?;
    let n = match read.execute("MATCH (c:Counter) RETURN c.n", &none) {
        Ok(result) => match result.rows() {
            [row] => match row.as_slice() {
                [Value::Integer(n)] => *n,
                other => return Err(format!("counter {other:?}")),
            },
            other => return Err(format!("counter rows {other:?}")),
        },
        Err(error) => return Err(error.to_string()),
    };
    let first = (n - KEEP).max(0);
    let rows = read
        .execute("MATCH (b:Batch) RETURN b.i, b.round, size(b.payload), substring(b.payload, 0, 1) ORDER BY b.i", &none)
        .map_err(|error| error.to_string())?;
    let batches: Vec<i64> = rows
        .rows()
        .iter()
        .map(|row| match &row[0] {
            Value::Integer(i) => *i,
            _ => -1,
        })
        .collect();
    if batches != (first..n).collect::<Vec<_>>() {
        return Err(format!("batches {batches:?} for {n} iterations"));
    }
    for row in rows.rows() {
        let [
            Value::Integer(i),
            Value::Integer(round),
            Value::Integer(size),
            Value::String(head),
        ] = row.as_slice()
        else {
            return Err(format!("batch row {row:?}"));
        };
        let want_round = i64::from(*i < n - KEEP / 2);
        let want = payload(*i, want_round);
        if *round != want_round
            || *size != want.len() as i64
            || (!want.is_empty() && *head != want[..1])
        {
            return Err(format!(
                "batch {i} of {n}: round {round}, payload of {size} starting {head:?}"
            ));
        }
    }
    let items = (n - first) * ITEMS;
    let through_edges = count_in(
        &read,
        "MATCH (:Batch)-[:HAS]->(item:Item) RETURN count(item)",
    )?;
    let through_scan = count_in(&read, "MATCH (item:Item) RETURN count(item)")?;
    let through_index = count_in(
        &read,
        &format!("MATCH (item:Item) WHERE item.i >= {first} AND item.i < {n} RETURN count(item)"),
    )?;
    let through_keyword = count_in(
        &read,
        "CALL db.index.keyword.queryNodes('item_text', 'word', 100000000) YIELD node RETURN count(node)",
    )?;
    if [through_edges, through_scan, through_index, through_keyword] != [items; 4] {
        return Err(format!(
            "{items} items expected for {n} iterations, found {through_edges} by edges, {through_scan} by a scan, \
             {through_index} by the property index, {through_keyword} by the keyword index"
        ));
    }
    if n > 0 {
        let last = count(
            db,
            "MATCH (item:Item {i: $i}) RETURN count(item)",
            &parameters(&[("i", Value::Integer(n - 1))]),
        );
        if last != ITEMS {
            return Err(format!("the last iteration has {last} items"));
        }
    }
    Ok(n)
}

fn count_in(read: &rhizodb::ReadTransaction<'_>, query: &str) -> Result<i64, String> {
    match read.execute(query, &Parameters::new()) {
        Ok(result) => match result.rows() {
            [row] => match row.as_slice() {
                [Value::Integer(n)] => Ok(*n),
                other => Err(format!("{query}: {other:?}")),
            },
            other => Err(format!("{query}: {other:?}")),
        },
        Err(error) => Err(format!("{query}: {error}")),
    }
}

#[test]
fn a_database_survives_hours_of_writes_killed_at_random_points() {
    let Some(seconds) = std::env::var(SECONDS_ENV)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return;
    };
    let seed = seed();
    let mut rng = Rng::new(seed);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("soak.rhizodb");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut reported = 0;
    let mut cycles = 0u64;
    while Instant::now() < deadline {
        cycles += 1;
        let context = format!("seed {seed} (replay with RHIZODB_TEST_SEED={seed}), cycle {cycles}");
        let clean = rng.chance(1, 8);
        let stop = clean.then(|| reported + 1 + rng.below(30) as i64);
        let mut writer = Writer::spawn(&path, stop);
        let ready =
            std::iter::from_fn(|| writer.next_line()).find(|line| line.starts_with("ready "));
        assert_eq!(ready, Some(format!("ready {reported}")), "{context}");
        let mut last = None;
        if !clean {
            let deadline = Instant::now() + Duration::from_millis(rng.below(4_000));
            while Instant::now() < deadline {
                match writer.next_line() {
                    Some(line) => {
                        if let Some(number) = line.strip_prefix("committed ") {
                            last = Some(number.parse::<i64>().unwrap());
                        }
                    }
                    None => panic!("{context}: the writer exited early"),
                }
            }
            std::thread::sleep(Duration::from_micros(rng.below(20_000)));
            writer.child.kill().unwrap();
        }
        let status = writer.child.wait().unwrap();
        assert!(
            !clean || status.success(),
            "{context}: the writer failed with {status}"
        );
        let last = writer.last_committed(last);
        let committed = last.map_or(reported, |last| last + 1);
        let db = Database::open(&path)
            .unwrap_or_else(|error| panic!("{context}: reopen failed: {error}"));
        let recovered = verify(&db).unwrap_or_else(|reason| panic!("{context}: {reason}"));
        assert!(
            recovered == committed || (!clean && recovered == committed + 1),
            "{context}: {committed} reported committed, {recovered} recovered"
        );
        reported = recovered;
        drop(db);
    }
    eprintln!("{cycles} cycles, {reported} iterations");
}
