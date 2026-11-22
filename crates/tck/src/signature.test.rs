use super::*;

fn pairs(entries: &[(&str, &str)]) -> Vec<(String, String)> {
    entries
        .iter()
        .map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned()))
        .collect()
}

#[test]
fn a_signature_names_its_parameters_and_outputs_in_order() {
    assert_eq!(
        parse_signature("test.my.proc(name :: STRING?, id :: INTEGER?) :: (city :: STRING?)"),
        Ok(Signature {
            name: "test.my.proc".to_owned(),
            parameters: pairs(&[("name", "STRING?"), ("id", "INTEGER?")]),
            outputs: pairs(&[("city", "STRING?")]),
        })
    );
}

#[test]
fn empty_lists_and_trailing_space_are_accepted() {
    assert_eq!(
        parse_signature("test.doNothing() :: ()"),
        Ok(Signature {
            name: "test.doNothing".to_owned(),
            parameters: Vec::new(),
            outputs: Vec::new(),
        })
    );
    assert_eq!(
        parse_signature("test.my.proc(in :: INTEGER?) :: (a :: INTEGER?, b :: INTEGER?) ")
            .map(|signature| signature.outputs.len()),
        Ok(2)
    );
}

#[test]
fn a_signature_without_outputs_is_malformed() {
    assert!(parse_signature("test.my.proc(in :: INTEGER?)").is_err());
    assert!(parse_signature("test.my.proc(in) :: ()").is_err());
}
