//! rsclient — a native Linux launcher for RuneScape 3, Old School RuneScape and RuneLite.
//!
//! Two modes live in this one binary:
//!
//! * `rsclient` — the launcher window (egui).
//! * `rsclient login` — the login window (WebKitGTK), run as a child process, which
//!   prints the resulting session to stdout as JSON.
//!
//! They are split because egui drives a `winit` event loop on the main thread while the
//! webview needs a GTK main loop on *its* main thread; the two cannot share a process.
//! Re-execing also means a WebKitGTK crash takes down only the login window.

mod auth;
mod clients;
mod log;
mod login;
mod paths;
mod store;
mod ui;

use anyhow::Result;

const USAGE: &str = "\
rsclient — launcher for RuneScape 3, Old School RuneScape and RuneLite

Usage:
  rsclient           Open the launcher
  rsclient login     Run the sign-in window alone, printing the session as JSON
  rsclient --help    Show this message";

fn main() {
    let result = match std::env::args().nth(1).as_deref() {
        Some("login") => run_login(),
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        Some("--version" | "-V") => {
            println!("rsclient {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown argument: {other}\n\n{USAGE}");
            std::process::exit(2);
        }
        None => ui::run(),
    };

    if let Err(e) = result {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}

/// The `login` subcommand: sign in, then print the session as JSON on stdout.
fn run_login() -> Result<()> {
    let session = login::run()?;
    println!("{}", serde_json::to_string(&session)?);
    Ok(())
}
