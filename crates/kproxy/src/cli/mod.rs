//! CLI navigation, guides, and completion generation.

pub mod completions;
pub mod guide;
pub mod help;

use anyhow::Result;
use clap::Parser;
use std::ffi::OsString;

use crate::{Cli, Command, ConfigCommand};

/// Parse one CLI invocation and preserve Clap's stdout/stderr and exit-code contract.
pub fn parse_or_exit(args: impl IntoIterator<Item = OsString>) -> Cli {
    Cli::try_parse_from(args).unwrap_or_else(|error| error.exit())
}

/// Handle commands that do not need a running daemon.
pub async fn handle_local(cli: &Cli) -> Result<bool> {
    match &cli.command {
        None => {
            help::print_command_help(&[])?;
            Ok(true)
        }
        Some(Command::Help { all, path }) => {
            if *all {
                help::print_all_commands();
            } else {
                help::print_command_help(path)?;
            }
            Ok(true)
        }
        Some(Command::Guide { topic }) => {
            guide::print(topic.map(|topic| topic.as_str()))?;
            Ok(true)
        }
        Some(Command::Completions { shell }) => {
            completions::print(*shell)?;
            Ok(true)
        }
        Some(Command::Version) => {
            print_version(cli.json)?;
            Ok(true)
        }
        Some(Command::Config {
            command: Some(ConfigCommand::List),
        }) => {
            crate::commands::runtime::list_config_modules(cli.json)?;
            Ok(true)
        }
        Some(Command::Config {
            command: Some(ConfigCommand::Validate { file }),
        }) => {
            if file.is_none() {
                kproxy_store::environment::load_dotenv()?;
            }
            crate::commands::runtime::validate_config(file.as_deref(), cli.json).await?;
            Ok(true)
        }
        Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Config),
        }) => {
            crate::commands::runtime::show_alert_config(cli.json)?;
            Ok(true)
        }
        Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Events),
        }) => {
            crate::commands::runtime::show_alert_events(cli.json)?;
            Ok(true)
        }
        Some(Command::Alert {
            command: Some(crate::commands::runtime::AlertCommand::Platforms),
        }) => {
            crate::commands::runtime::show_alert_platforms(cli.json)?;
            Ok(true)
        }
        Some(command) => {
            if let Some(path) = help::empty_group_path(command) {
                if cli.json {
                    help::exit_missing_action(path);
                }
                help::print_command_help(&[path.to_owned()])?;
                return Ok(true);
            }
            if wrapper_local_only() {
                eprintln!(
                    "kproxy: kproxyd 未运行；当前可使用 help、guide、completions、version、config list/validate、alert config/events/platforms 或无参命令组帮助"
                );
                std::process::exit(1);
            }
            Ok(false)
        }
    }
}

fn wrapper_local_only() -> bool {
    std::env::var_os("KPROXY_WRAPPER_LOCAL_ONLY").is_some_and(|value| value == "1")
}

fn print_version(json: bool) -> Result<()> {
    let value = serde_json::json!({
        "version":env!("CARGO_PKG_VERSION"),
        "rust":env!("CARGO_PKG_RUST_VERSION"),
        "codewhisperer":kproxy_kiro::endpoint::CODEWHISPERER_URL,
        "amazonq":kproxy_kiro::endpoint::AMAZONQ_URL
    });
    if json {
        crate::output::print_json(&value)
    } else {
        println!(
            "kproxy {} (Rust {})",
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_RUST_VERSION")
        );
        println!("CodeWhisperer {}", kproxy_kiro::endpoint::CODEWHISPERER_URL);
        println!("AmazonQ        {}", kproxy_kiro::endpoint::AMAZONQ_URL);
        Ok(())
    }
}
