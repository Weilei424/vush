use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod validate;

#[derive(Parser)]
#[command(name = "vush", version, about, arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check test files without sending any traffic
    #[command(after_help = validate::AFTER_HELP)]
    Validate {
        /// YAML or JSON test files (.yaml, .yml or .json)
        #[arg(required = true, value_name = "FILE")]
        files: Vec<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Validate { files } => {
            if validate::run(&files, &mut io::stdout(), &mut io::stderr()) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
    }
}
