use super::*;

#[test]
fn an_identical_copy_passes() {
    assert!(copy_problems(b"# RhizoDB\n", b"# RhizoDB\n", false).is_empty());
}

#[test]
fn a_symlink_left_as_a_text_file_fails() {
    assert_eq!(
        copy_problems(b"# RhizoDB\n", b"../../README.md", false).len(),
        1
    );
}

#[test]
fn a_symlink_fails_even_where_it_reads_as_the_readme() {
    assert_eq!(copy_problems(b"# RhizoDB\n", b"# RhizoDB\n", true).len(), 1);
}

#[test]
fn line_ending_changes_fail() {
    assert_eq!(
        copy_problems(b"# RhizoDB\n", b"# RhizoDB\r\n", false).len(),
        1
    );
}
