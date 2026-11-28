use super::*;

#[test]
fn a_file_of_the_operating_system_reaches_its_identity_and_locks_through_any_fs() {
    let directory = tempfile::tempdir().unwrap();
    let fs = AnyFs::Os(OsFs::new(directory.path()));
    let size = BlockSize::new(4096).unwrap();
    drop(fs.create("one", size).unwrap());
    drop(fs.create("other", size).unwrap());
    std::fs::hard_link(directory.path().join("one"), directory.path().join("link")).unwrap();
    let one = fs.open("one", size).unwrap();
    let identity = one.identity().unwrap();
    assert_ne!(identity, (0, 0));
    assert_eq!(fs.open("link", size).unwrap().identity().unwrap(), identity);
    assert_ne!(
        fs.open("other", size).unwrap().identity().unwrap(),
        identity
    );
    assert!(!one.held_elsewhere().unwrap());
}

const RACE_DIRECTORY: &str = "RHIZODB_RACE_DIRECTORY";
const RACE_NAME: &str = "RHIZODB_RACE_NAME";

#[test]
#[ignore = "child process of the race of first opens, selected by RHIZODB_RACE_DIRECTORY"]
fn racing_first_open_child() {
    use std::io::BufRead as _;
    use std::io::Read as _;
    use std::io::Write as _;
    let (Ok(directory), Ok(name)) = (std::env::var(RACE_DIRECTORY), std::env::var(RACE_NAME))
    else {
        return;
    };
    let mut stdin = std::io::stdin().lock();
    let mut go = String::new();
    stdin.read_line(&mut go).unwrap();
    let opened = OsFs::read_only(&directory, &name)
        .map_err(EngineError::from)
        .and_then(|fs| {
            Engine::open_read_only(Arc::new(AnyFs::Os(fs)), &name, EngineOptions::default())
        });
    let mut stdout = std::io::stdout();
    writeln!(
        stdout,
        "{}",
        if opened.is_ok() { "opened" } else { "refused" }
    )
    .unwrap();
    stdout.flush().unwrap();
    let mut rest = Vec::new();
    stdin.read_to_end(&mut rest).unwrap();
    drop(opened);
}

struct Racer {
    child: std::process::Child,
    outcome: std::sync::mpsc::Receiver<String>,
}

impl Racer {
    fn spawn(directory: &Path, name: &str) -> Self {
        use std::io::BufRead as _;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "engine::any_fs::tests::racing_first_open_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env(RACE_DIRECTORY, directory)
            .env(RACE_NAME, name)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let (send, outcome) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                let line = line.trim().to_owned();
                if (line == "opened" || line == "refused") && send.send(line).is_err() {
                    return;
                }
            }
        });
        Self { child, outcome }
    }

    fn go(&mut self) {
        use std::io::Write as _;
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "go").unwrap();
        stdin.flush().unwrap();
    }

    fn opened(&self) -> bool {
        self.outcome
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("a racing open reported within a minute")
            == "opened"
    }

    fn close(mut self) {
        drop(self.child.stdin.take());
        assert!(self.child.wait().unwrap().success());
    }
}

#[test]
fn first_opens_through_two_names_of_one_file_never_both_start_a_session() {
    let mut once = 0;
    for round in 0..24 {
        let directory = tempfile::tempdir().unwrap();
        let fs = OsFs::new(directory.path());
        Engine::open(
            Arc::new(AnyFs::Os(fs)),
            "a.rhizodb",
            EngineOptions::default(),
        )
        .unwrap()
        .close()
        .unwrap();
        std::fs::hard_link(
            directory.path().join("a.rhizodb"),
            directory.path().join("b.rhizodb"),
        )
        .unwrap();
        let mut a = Racer::spawn(directory.path(), "a.rhizodb");
        let mut b = Racer::spawn(directory.path(), "b.rhizodb");
        a.go();
        b.go();
        let (a_opened, b_opened) = (a.opened(), b.opened());
        assert!(
            !(a_opened && b_opened),
            "round {round}: two sessions hold one file through two names"
        );
        once += usize::from(a_opened || b_opened);
        a.close();
        b.close();
    }
    assert!(once > 0, "no round opened the file at all");
}
