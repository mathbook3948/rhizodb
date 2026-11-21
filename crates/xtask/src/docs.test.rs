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
