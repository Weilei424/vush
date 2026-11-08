use std::env;
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
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => exit_with_usage(error),
    };
    match cli.command {
        Command::Validate { files } => {
            if validate::run(&files, &mut io::stdout(), &mut io::stderr()) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
    }
}

/// Prints clap's help or usage error and exits.
///
/// clap echoes arguments as they are, so the message comes from a second parse
/// with control characters escaped. Escaping the finished message wouldn't
/// work, because a newline in an argument looks like one of clap's own.
fn exit_with_usage(error: clap::Error) -> ! {
    let args = env::args_os().map(|arg| validate::printable(&arg.to_string_lossy()));
    Cli::try_parse_from(args).err().unwrap_or(error).exit()
}
