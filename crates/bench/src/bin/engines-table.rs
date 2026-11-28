use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    match bench::engines_table::main(env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
