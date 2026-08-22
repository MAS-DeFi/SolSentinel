#[allow(dead_code)]
#[path = "../src/cli.rs"]
mod cli;

use std::io;

use clap::CommandFactory;
use clap_complete::{generate, shells::Bash};

use cli::Cli;

fn main() {
    let mut command = Cli::command();
    generate(Bash, &mut command, "val", &mut io::stdout());
}
