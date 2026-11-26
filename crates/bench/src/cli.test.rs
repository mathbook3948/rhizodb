use super::*;

fn args(text: &[&str]) -> Vec<String> {
    text.iter().map(|part| (*part).to_owned()).collect()
}

#[test]
fn no_arguments_give_the_defaults() {
    assert_eq!(
        parse(args(&[])),
        Ok(Arguments {
            scale: Scale::Small,
            reps: 5,
            only: None,
            skip: None,
            out: None,
            engines: None,
            servers: Servers::default(),
            load: Load::default(),
            memory: Memory::default(),
        })
    );
}

#[test]
fn every_flag_is_read() {
    let parsed = parse(args(&[
        "--scale", "medium", "--reps", "3", "--only", "load", "--out", "r.json",
    ]));

    assert_eq!(
        parsed,
        Ok(Arguments {
            scale: Scale::Medium,
            reps: 3,
            only: Some("load".to_owned()),
            skip: None,
            out: Some(PathBuf::from("r.json")),
            engines: None,
            servers: Servers::default(),
            load: Load::default(),
            memory: Memory::default(),
        })
    );
}

#[test]
fn bad_input_is_a_usage_error() {
    assert!(parse(args(&["--scale", "huge"])).is_err());
    assert!(parse(args(&["--reps", "0"])).is_err());
    assert!(parse(args(&["--reps"])).is_err());
    assert!(parse(args(&["--nope"])).is_err());
}

#[test]
fn engines_and_server_urls_are_read() {
    let parsed = parse(args(&[
        "--engines",
        "rhizodb,sqlite",
        "--neo4j",
        "bolt://127.0.0.1:7687",
        "--memgraph",
        "bolt://127.0.0.1:7688",
        "--falkordb",
        "redis://127.0.0.1:6379",
    ]))
    .unwrap();

    assert_eq!(
        parsed.engines,
        Some(vec![EngineKind::Rhizodb, EngineKind::Sqlite])
    );
    assert_eq!(
        parsed.servers.neo4j.as_deref(),
        Some("bolt://127.0.0.1:7687")
    );
    assert_eq!(
        parsed.servers.memgraph.as_deref(),
        Some("bolt://127.0.0.1:7688")
    );
    assert_eq!(
        parsed.servers.falkordb.as_deref(),
        Some("redis://127.0.0.1:6379")
    );
    assert!(parse(args(&["--engines", "oracle"])).is_err());
}

#[test]
fn fewer_than_five_repetitions_warn_loudly() {
    assert!(warnings(5).is_empty());
    assert!(warnings(3)[0].starts_with("WARNING"));
}

#[test]
fn skip_is_read() {
    assert_eq!(
        parse(args(&["--skip", "index/vector"]))
            .unwrap()
            .skip
            .as_deref(),
        Some("index/vector")
    );
}

#[test]
fn the_cache_size_and_cold_runs_are_read() {
    let parsed = parse(args(&["--cache-mib", "32", "--cold"])).unwrap();

    assert_eq!(
        parsed.memory,
        Memory {
            cache_bytes: Some(32 << 20),
            cold: true,
        }
    );
    assert!(parse(args(&["--cache-mib", "0"])).is_err());
}
