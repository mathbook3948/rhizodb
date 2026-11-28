//! Tests that spawn this test binary as child processes, since a byte-range lock never conflicts
//! with another lock of its own process on most platforms, and a mapping is only shared when a
//! second process sees it.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use rhizodb_shm::Lock;
use rhizodb_shm::Mapping;

const ROLE_ENV: &str = "RHIZODB_SHM_CHILD";
const FILE_ENV: &str = "RHIZODB_SHM_FILE";
const RANGE: (u64, u64) = (1 << 40, 3);
const WORDS: usize = 512;
const ADDS: u64 = 10_000;

fn open(path: &Path) -> File {
    File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

fn mapped(path: &Path) -> (File, Mapping) {
    let file = open(path);
    rhizodb_shm::reserve(&file, (WORDS * 8) as u64).unwrap();
    let mapping = Mapping::new(&file, WORDS).unwrap();
    (file, mapping)
}

fn say(line: &str) {
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{line}").unwrap();
    stdout.flush().unwrap();
}

fn hang() -> ! {
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[test]
#[ignore = "child process of the tests in this file, selected by RHIZODB_SHM_CHILD"]
fn child() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let path = std::env::var(FILE_ENV).unwrap();
    let path = Path::new(&path);
    let (offset, len) = RANGE;
    match role.as_str() {
        "exclusive" | "shared" => {
            let file = open(path);
            let kind = if role == "shared" {
                Lock::Shared
            } else {
                Lock::Exclusive
            };
            rhizodb_shm::lock(&file, offset, len, kind).unwrap();
            say("locked");
            hang();
        }
        "downgrade" => {
            let file = open(path);
            rhizodb_shm::lock(&file, offset, len, Lock::Exclusive).unwrap();
            rhizodb_shm::downgrade(&file, offset, len).unwrap();
            say("downgraded");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            rhizodb_shm::unlock(&file, offset, len).unwrap();
            say("unlocked");
            hang();
        }
        "adder" => {
            let (_file, mapping) = mapped(path);
            assert_eq!(mapping.words()[1].load(Ordering::Acquire), 42);
            for _ in 0..ADDS {
                mapping.words()[3].fetch_add(1, Ordering::Relaxed);
            }
            mapping.words()[0].store(1, Ordering::Release);
            say("added");
        }
        role => panic!("unknown child role {role}"),
    }
}

const PATIENCE: Duration = Duration::from_secs(60);

struct Peer {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Peer {
    fn spawn(role: &str, path: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["child", "--exact", "--ignored", "--nocapture", "--quiet"])
            .env(ROLE_ENV, role)
            .env(FILE_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self { child, lines }
    }

    fn wait_for(&mut self, wanted: &str) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.trim_end() == wanted => return,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => panic!("no {wanted:?} within {PATIENCE:?}"),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("the child exited before printing {wanted:?}")
                }
            }
        }
    }

    fn tell(&mut self) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "go").unwrap();
        stdin.flush().unwrap();
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn can_take(path: &Path, kind: Lock) -> bool {
    let (sender, tried) = mpsc::channel();
    let path = path.to_owned();
    thread::spawn(move || {
        let file = open(&path);
        let (offset, len) = RANGE;
        let got = rhizodb_shm::try_lock(&file, offset, len, kind).unwrap();
        if got {
            rhizodb_shm::unlock(&file, offset, len).unwrap();
        }
        let _ = sender.send(got);
    });
    tried
        .recv_timeout(PATIENCE)
        .expect("a try of a lock returned at once")
}

fn exits_in_time(child: &mut Child) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.success();
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the child did not exit within {PATIENCE:?}");
}

// Windows releases the locks of a killed process soon after it exits, not at once.
fn eventually_can_take(path: &Path, kind: Lock) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !can_take(path, kind) {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

#[test]
fn an_exclusive_lock_keeps_out_other_processes_until_its_holder_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let mut holder = Peer::spawn("exclusive", &path);
    holder.wait_for("locked");
    assert!(
        !can_take(&path, Lock::Exclusive),
        "exclusive beside exclusive"
    );
    assert!(!can_take(&path, Lock::Shared), "shared beside exclusive");
    holder.kill();
    assert!(
        eventually_can_take(&path, Lock::Exclusive),
        "freed by the holder's death"
    );
}

#[test]
fn a_shared_lock_admits_shared_locks_of_other_processes_and_keeps_out_exclusive_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let mut holder = Peer::spawn("shared", &path);
    holder.wait_for("locked");
    assert!(can_take(&path, Lock::Shared), "shared beside shared");
    assert!(!can_take(&path, Lock::Exclusive), "exclusive beside shared");
    holder.kill();
    assert!(
        eventually_can_take(&path, Lock::Exclusive),
        "freed by the holder's death"
    );
}

#[test]
fn a_downgraded_lock_is_one_shared_lock_that_one_unlock_releases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let mut holder = Peer::spawn("downgrade", &path);
    holder.wait_for("downgraded");
    assert!(
        can_take(&path, Lock::Shared),
        "shared beside the downgraded lock"
    );
    assert!(
        !can_take(&path, Lock::Exclusive),
        "exclusive beside the downgraded lock"
    );
    holder.tell();
    holder.wait_for("unlocked");
    assert!(can_take(&path, Lock::Exclusive), "freed by one unlock");
}

#[test]
fn a_waiting_lock_is_granted_once_the_holder_dies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let mut holder = Peer::spawn("exclusive", &path);
    holder.wait_for("locked");
    let (granted, waiter) = mpsc::channel();
    {
        let path = path.clone();
        thread::spawn(move || {
            let file = open(&path);
            let (offset, len) = RANGE;
            rhizodb_shm::lock(&file, offset, len, Lock::Exclusive).unwrap();
            let _ = granted.send(Instant::now());
            thread::sleep(PATIENCE);
        });
    }
    assert!(
        waiter.recv_timeout(Duration::from_millis(200)).is_err(),
        "the waiter got the lock while the holder lived"
    );
    let killed = Instant::now();
    holder.kill();
    let at = waiter
        .recv_timeout(PATIENCE)
        .expect("the waiter got the lock once the holder died");
    assert!(at >= killed, "granted only after the kill");
}

#[test]
fn stores_into_a_mapping_show_in_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("words");
    let (_file, mapping) = mapped(&path);
    for word in &mapping.words()[..4] {
        word.store(0, Ordering::Relaxed);
    }
    mapping.words()[1].store(42, Ordering::Release);
    for _ in 0..ADDS {
        mapping.words()[3].fetch_add(1, Ordering::Relaxed);
    }
    let mut adder = Peer::spawn("adder", &path);
    adder.wait_for("added");
    assert_eq!(mapping.words()[0].load(Ordering::Acquire), 1);
    assert_eq!(mapping.words()[3].load(Ordering::Relaxed), 2 * ADDS);
    assert!(exits_in_time(&mut adder.child));
}

#[test]
fn concurrent_adds_from_two_processes_lose_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("words");
    let (_file, mapping) = mapped(&path);
    for word in &mapping.words()[..4] {
        word.store(0, Ordering::Relaxed);
    }
    mapping.words()[1].store(42, Ordering::Release);
    let mut adder = Peer::spawn("adder", &path);
    for _ in 0..ADDS {
        mapping.words()[3].fetch_add(1, Ordering::Relaxed);
    }
    adder.wait_for("added");
    assert_eq!(mapping.words()[3].load(Ordering::Relaxed), 2 * ADDS);
}

#[test]
fn a_test_through_a_read_only_handle_sees_a_shared_lock_of_another_process() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locks");
    let mut holder = Peer::spawn("shared", &path);
    holder.wait_for("locked");
    let reading = File::open(&path).unwrap();
    let (offset, len) = RANGE;
    assert!(rhizodb_shm::held_elsewhere(&reading, offset, len, Lock::Exclusive).unwrap());
    assert!(!rhizodb_shm::held_elsewhere(&reading, offset, len, Lock::Shared).unwrap());
    holder.kill();
    let deadline = Instant::now() + PATIENCE;
    while rhizodb_shm::held_elsewhere(&reading, offset, len, Lock::Exclusive).unwrap() {
        assert!(Instant::now() < deadline, "the killed holder's lock stayed");
        thread::sleep(Duration::from_millis(10));
    }
}
