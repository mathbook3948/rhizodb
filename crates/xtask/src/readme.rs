use std::error::Error;
use std::fs;
use std::path::Path;

const ROOT_README: &str = "README.md";
const CRATE_README: &str = "crates/rhizodb/README.md";

pub fn copy_problems(root: &[u8], copy: &[u8], copy_is_symlink: bool) -> Vec<String> {
    if copy_is_symlink {
        return vec![format!(
            "{CRATE_README} is a symlink, which a checkout without symlinks turns into a text file, so make it a copy of {ROOT_README}"
        )];
    }
    if root != copy {
        return vec![format!(
            "{CRATE_README} differs from {ROOT_README}, copy {ROOT_README} over it"
        )];
    }
    Vec::new()
}

pub fn check(workspace_root: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let copy = workspace_root.join(CRATE_README);
    let copy_is_symlink = fs::symlink_metadata(&copy)?.file_type().is_symlink();
    Ok(copy_problems(
        &fs::read(workspace_root.join(ROOT_README))?,
        &fs::read(&copy)?,
        copy_is_symlink,
    ))
}

#[cfg(test)]
#[path = "readme.test.rs"]
mod tests;
