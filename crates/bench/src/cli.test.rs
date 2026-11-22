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
            out: None,
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
            out: Some(PathBuf::from("r.json")),
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
