//! Fleet federation commands: view status, diagnostics, and health.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::api::client::ApiClient;
use crate::api::schema::{EmptyParams, Method, Request};

pub(crate) fn run_fleet_command(args: &[String]) -> io::Result<i32> {
    match args.first().map(|arg| arg.as_str()) {
        Some("status") => run_fleet_status(&args[1..]),
        Some("doctor") => run_fleet_doctor(&args[1..]),
        Some("help" | "--help" | "-h") => {
            print_fleet_help();
            Ok(0)
        }
        _ => {
            print_fleet_help();
            Ok(2)
        }
    }
}

fn run_fleet_status(args: &[String]) -> io::Result<i32> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: herdr fleet status [--json]");
        println!();
        println!("Show federation fleet status: which origins are reachable, latency, errors.");
        println!();
        println!("Options:");
        println!("  --json    Output as JSON instead of human-readable text");
        return Ok(0);
    }

    let use_json = args.iter().any(|arg| arg == "--json");

    let client = ApiClient::local();
    let request_id = format!("cli:fleet:status:{}", current_unix_time());
    let request = Request {
        id: request_id,
        method: Method::FederationStatus(EmptyParams::default()),
    };

    match client.request(request) {
        Ok(response) => {
            match response.result {
                crate::api::schema::ResponseResult::FederationStatus { status } => {
                    if use_json {
                        if let Ok(json) = serde_json::to_string(&status) {
                            println!("{}", json);
                        } else {
                            eprintln!("Failed to serialize federation status");
                            return Ok(1);
                        }
                    } else {
                        print_federation_status_human_readable(&status);
                    }
                    Ok(0)
                }
                _ => {
                    eprintln!("Unexpected response type from federation.status");
                    Ok(1)
                }
            }
        }
        Err(err) => {
            if use_json {
                eprintln!("{{\"error\":\"{err}\"}}");
            } else {
                eprintln!("Failed to query federation status: {err}");
            }
            Ok(1)
        }
    }
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn print_federation_status_human_readable(status: &crate::api::schema::FederationStatusResponse) {
    if !status.enabled {
        println!("Federation is disabled.");
        println!("Enable it in your config: set experimental.federation = true");
        return;
    }

    println!("Federation Status");
    println!("─────────────────");
    println!("Total origins:     {}", status.total_origins);
    println!("  Reachable:       {}", status.reachable);
    println!("  Unreachable:     {}", status.unreachable);
    println!("  Unknown:         {}", status.unknown);

    if !status.origins.is_empty() {
        println!();
        println!("Origin Details:");
        for origin in &status.origins {
            let status_str = match origin.status {
                crate::api::schema::OriginStatusKind::Reachable => "✓ reachable",
                crate::api::schema::OriginStatusKind::Unreachable => "✗ unreachable",
                crate::api::schema::OriginStatusKind::Unknown => "? unknown",
            };
            println!();
            println!("  {} ({}) {}", origin.label, origin.key, status_str);

            if let Some(latency_ms) = origin.latency_ms {
                println!("    Latency: {} ms", latency_ms);
            }
            if let Some(error) = &origin.error {
                println!("    Error: {}", error);
            }
            if origin.failure_count > 0 {
                println!("    Failures: {}", origin.failure_count);
            }
        }
    } else {
        println!();
        println!("No origins configured.");
    }
}

fn run_fleet_doctor(args: &[String]) -> io::Result<i32> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: herdr fleet doctor");
        println!();
        println!("Run federation health diagnostics: check connectivity, versioning,");
        println!("and configuration for all federated origins.");
        return Ok(0);
    }

    // TODO: Run diagnostics via the federation API endpoint
    eprintln!("Federation diagnostics are not yet available.");
    eprintln!("To see fleet diagnostics, enable experimental.federation in your config.");

    Ok(0)
}

fn print_fleet_help() {
    println!("usage: herdr fleet <command> [options]");
    println!();
    println!("Federation fleet commands:");
    println!();
    println!("  status     Show federation status (origins, reachability, latency)");
    println!("  doctor     Run federation health diagnostics");
    println!("  help       Show this help message");
    println!();
    println!("Use 'herdr fleet <command> --help' for more information.");
}
