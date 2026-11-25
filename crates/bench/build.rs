//! Exports the version each pinned dependency is declared at, and exports the executable's
//! symbols so LadybugDB extensions built from source can resolve the engine's own symbols.

use std::env;
use std::fs;

const PINNED: [(&str, &str, &str); 7] = [
    ("lbug", "lbug", "PIN_LBUG"),
    ("rusqlite", "rusqlite", "PIN_RUSQLITE"),
    ("duckdb", "duckdb", "PIN_DUCKDB"),
    ("cozo", "cozo", "PIN_COZO"),
    ("neo4rs", "neo4rs", "PIN_NEO4RS"),
    ("falkordb", "falkordb", "PIN_FALKORDB"),
    ("rhizodb", "rhizodb", "PIN_RHIZODB"),
];

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../rhizodb/Cargo.toml");
    println!("cargo:rustc-link-arg=-rdynamic");
    if env::var_os("CARGO_FEATURE_COZO").is_some() {
        println!("cargo:rustc-link-arg=-Wl,--allow-multiple-definition");
    }
    let manifest = fs::read_to_string("Cargo.toml").unwrap_or_default();
    for (crate_name, _, variable) in PINNED {
        let version = if crate_name == "rhizodb" {
            workspace_version()
        } else {
            declared_version(&manifest, crate_name)
        };
        println!("cargo:rustc-env={variable}={}", version.unwrap_or_default());
    }
}

fn declared_version(manifest: &str, name: &str) -> Option<String> {
    let line = manifest
        .lines()
        .find(|line| line.trim_start().starts_with(&format!("{name} =")))?;
    let after = line
        .split("version = \"")
        .nth(1)
        .or_else(|| line.split("= \"").nth(1))?;
    let version = after.split('"').next()?;
    Some(version.trim_start_matches('=').to_owned())
}

fn workspace_version() -> Option<String> {
    let manifest = fs::read_to_string("../../Cargo.toml").ok()?;
    let line = manifest
        .lines()
        .find(|line| line.trim_start().starts_with("version ="))?;
    Some(line.split('"').nth(1)?.to_owned())
}
