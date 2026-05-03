//! Reeve binary entry point.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "reeve",
    version,
    about = "Reeve — runtime that supervises AI coding agents"
)]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
