mod bootstrap;
mod bootstrap_web;
mod checks;
mod checks_data;
mod cluster;
mod database;
mod db_bootstrap;
mod doctor;
mod install;
mod install_access;
mod install_events;
mod loadbalancer_probe;
mod observability;
mod storage;
mod storage_probe;
mod storage_secret;
mod style;
mod targets;
mod types;
mod users;
mod util;

use std::env;
use std::process::ExitCode;

use bootstrap::bootstrap;
use doctor::doctor;
use style::{status_label, RED};
use users::users;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("{} {err}", status_label("error", RED));
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        return Ok(());
    };

    match command.as_str() {
        "help" | "--help" | "-h" => print_help(),
        "version" | "--version" | "-V" => println!("kubarr {VERSION}"),
        "bootstrap" => bootstrap(args.collect())?,
        "doctor" => doctor(args.collect()),
        "users" => users(args.collect()),
        other => return Err(format!("unknown command '{other}'. Run 'kubarr help'.")),
    }

    Ok(())
}

fn print_help() {
    println!(
        "kubarr {VERSION}\n\nUSAGE:\n    kubarr <COMMAND> [OPTIONS]\n\nCOMMANDS:\n    bootstrap   Set up Kubarr in the web interface\n    users       Manage Kubarr users\n    doctor      Check local tools and cluster access\n    version     Print CLI version\n    help        Print this help\n\nRun 'kubarr <COMMAND> --help' for command-specific help."
    );
}
