//! `datawal` — read-only inspector binary for datawal stores.
//!
//! See `README.md` in this crate for usage and the
//! `datawal.cli.v1` JSON output schema.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

mod cli;
mod commands;
mod output;

use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match commands::dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            // User-facing error path (bad args, IO, store-locked, etc.).
            // Always emit to stderr in human form; the structured JSON
            // streams stay on stdout per subcommand contract.
            eprintln!("datawal: error: {:#}", e);
            ExitCode::from(1)
        }
    }
}
