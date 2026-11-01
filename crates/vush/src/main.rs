use clap::Parser;

#[derive(Parser)]
#[command(name = "vush", version, about, arg_required_else_help = true)]
struct Cli {}

fn main() {
    Cli::parse();
}
