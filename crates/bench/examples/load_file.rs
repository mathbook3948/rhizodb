//! Loads the benchmark graph into RhizoDB at a scale, as the bulk load workload does, and leaves
//! the database in a directory, for `mise run size-report`, which reports where its bytes go.

use std::path::PathBuf;

use bench::cli::SEED;
use bench::data::Dataset;
use bench::engine::Engine;
use bench::engine::directory_size;
use bench::rhizo::RhizoEngine;
use bench::scale::Scale;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let scale = arguments.next().unwrap_or_else(|| "medium".to_owned());
    let dir = PathBuf::from(arguments.next().ok_or("give a directory")?);
    let scale = Scale::parse(&scale).ok_or("unknown scale")?;
    let data = Dataset::generate(SEED, scale.plan());
    let scratch = dir.join("input");
    let graph = dir.join("graph");
    std::fs::create_dir_all(&scratch)?;
    std::fs::create_dir_all(&graph)?;
    let instance = RhizoEngine
        .open(&graph)
        .map_err(|error| format!("{error:?}"))?;
    let run = || -> Result<(), bench::engine::EngineError> {
        instance.create_schema()?;
        instance.prepare_bulk_load(&data, &scratch)?;
        instance.bulk_load(&data, &scratch)?;
        Ok(())
    };
    run().map_err(|error| format!("{error:?}"))?;
    instance.close().map_err(|error| format!("{error:?}"))?;
    println!("{} bytes", directory_size(&graph)?);
    Ok(())
}
