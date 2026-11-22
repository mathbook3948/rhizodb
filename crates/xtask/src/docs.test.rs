use super::*;

const VALID: &str = "# Storage\n\nDecides how data reaches disk.\n\n## Background\n\nReason.\n\n## Decisions\n\n- A decision.\n\n## Open\n\nNone\n";

#[test]
fn parses_title_summary_and_domain_of_a_valid_doc() {
    assert_eq!(
        parse_doc("architecture/storage.md", VALID),
        Ok(Doc {
            domain: "architecture".into(),
            path: "architecture/storage.md".into(),
            title: "Storage".into(),
            summary: "Decides how data reaches disk.".into(),
        })
    );
}

#[test]
fn reports_a_missing_section() {
    let text = VALID.replace("## Open\n\nNone\n", "");

    assert_eq!(
        parse_doc("architecture/storage.md", &text),
        Err(vec![
            "sections must be exactly ## Background, ## Decisions, ## Open".into()
        ])
    );
}

#[test]
fn reports_a_path_outside_the_domain_layout() {
    assert_eq!(
        parse_doc("Storage.md", VALID),
        Err(vec![
            "path must be <domain>/<title>.md in lowercase letters and hyphens".into()
        ])
    );
}

#[test]
fn index_groups_docs_by_domain_in_path_order() {
    let doc = |domain: &str, name: &str| Doc {
        domain: domain.into(),
        path: format!("{domain}/{name}.md"),
        title: name.into(),
        summary: format!("{name} summary."),
    };

    let index = render_index(&[
        doc("product", "scope"),
        doc("architecture", "storage"),
        doc("architecture", "query"),
    ]);

    assert!(index.ends_with(
        "\n## architecture\n\n- [query](architecture/query.md): query summary.\n- [storage](architecture/storage.md): storage summary.\n\n## product\n\n- [scope](product/scope.md): scope summary.\n"
    ));
}

#[test]
fn reports_entries_of_a_domain_folder_that_are_not_docs() {
    let root = std::env::temp_dir().join(format!("xtask-docs-{}", std::process::id()));
    let domain = root.join(DOCS_DIR).join("architecture");
    fs::create_dir_all(domain.join("nested")).unwrap();
    fs::write(domain.join("storage.md"), VALID).unwrap();
    fs::write(domain.join("diagram.png"), [0x89, 0xff, 0xfe]).unwrap();
    fs::write(domain.join("broken.md"), [0x89, 0xff, 0xfe]).unwrap();

    let result = collect(&root).map_err(|error| error.to_string());
    fs::remove_dir_all(&root).unwrap();

    let (docs, mut violations) = result.unwrap();
    violations.sort();
    assert_eq!(docs.len(), 1);
    assert_eq!(
        violations,
        [
            "docs/architecture/broken.md: a doc must be UTF-8 text",
            "docs/architecture/diagram.png: a domain folder holds only .md docs",
            "docs/architecture/nested: a domain folder holds only .md docs",
        ]
    );
}

#[test]
fn reports_a_decision_line_with_two_sentences() {
    let text = VALID.replace(
        "- A decision.\n",
        "- A decision.\n- IDs only grow. A deleted ID is never reused.\n",
    );

    assert_eq!(
        parse_doc("architecture/storage.md", &text),
        Err(vec![
            "line 12: a Decisions line holds one decision in one sentence".into()
        ])
    );
}

#[test]
fn reports_decisions_joined_by_a_semicolon() {
    let text = VALID.replace(
        "- A decision.\n",
        "- IDs only grow; IDs are never reused.\n",
    );

    assert_eq!(
        parse_doc("architecture/storage.md", &text),
        Err(vec![
            "line 11: a Decisions line holds one decision in one sentence".into()
        ])
    );
}

#[test]
fn reports_reasoning_in_a_decision_line() {
    let text = VALID.replace(
        "- A decision.\n",
        "- Durations keep months apart, because months have no fixed length.\n",
    );

    assert_eq!(
        parse_doc("architecture/storage.md", &text),
        Err(vec![
            "line 11: a Decisions line leaves its reasoning to the commit message".into()
        ])
    );
}

#[test]
fn accepts_code_spans_and_abbreviations_inside_one_sentence() {
    let text = VALID.replace(
        "- A decision.\n",
        "- `n.name` reads a property, and `a.b.c` reads nested ones.\n",
    );

    assert!(parse_doc("architecture/storage.md", &text).is_ok());
}

#[test]
fn leaves_background_prose_alone() {
    let text = VALID.replace("Reason.\n", "Reason one. Reason two, because it matters.\n");

    assert!(parse_doc("architecture/storage.md", &text).is_ok());
}
