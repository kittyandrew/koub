mod client;
mod commands;
mod format;

use clap::Parser;
use client::Client;
use commands::*;
use format::*;

#[derive(Parser)]
#[command(name = "oubot-cli")]
#[command(about = "CLI tool for Open Uptime Bot management")]
struct Cli {
    /// Server URL (e.g., http://localhost:8080)
    #[arg(short, long, env = "OUBOT_SERVER")]
    server: String,

    /// Authentication token (not required for 'init' command)
    #[arg(short, long, env = "OUBOT_TOKEN")]
    token: Option<String>,

    /// Output raw JSON instead of formatted view
    #[arg(long, global = true)]
    raw: bool,

    #[command(subcommand)]
    command: Commands,
}

fn require_token(token: &Option<String>) {
    if token.is_none() {
        eprintln!("Error: This command requires authentication. Please provide --token or set OUBOT_TOKEN.");
        std::process::exit(1);
    }
}

fn main() {
    let cli = Cli::parse();
    let client = Client::new(cli.server, cli.token.clone());

    match cli.command {
        Commands::Init { invite, language, invites } => {
            // If invite is provided, create regular user; otherwise create admin (first user)
            let is_admin = invite.is_none();
            let user_type = if is_admin { "Admin" } else { "Normal" };
            let mut body = serde_json::json!({
                "user_type": user_type,
                "invites_limit": invites,
                "ntfy_enabled": true,
                "language_code": language
            });
            if let Some(inv) = invite {
                body["invite"] = serde_json::json!(inv);
            }
            handle_response_with(client.post("/api/v1/users", &body), cli.raw, |json| {
                if let Some(token) =
                    json.get("state").and_then(|s| s.get("user")).and_then(|u| u.get("access_token")).and_then(|t| t.as_str())
                {
                    let label = if is_admin { "Admin" } else { "User" };
                    println!("{} created successfully!", label);
                    println!("Your access token: {}", token);
                    println!("\nUse this token with the CLI:");
                    println!("  export OUBOT_TOKEN={}", token);
                    println!("  oubot-cli me");
                } else {
                    print_json(json);
                }
            });
        }

        Commands::Me => {
            require_token(&cli.token);
            handle_response_with(client.get("/api/v1/me"), cli.raw, format_me);
        }

        Commands::Token(cmd) => {
            require_token(&cli.token);
            match cmd {
                TokenCommands::Show => match client.get("/api/v1/me") {
                    Ok(json) => {
                        if let Some(token) = json
                            .get("user")
                            .and_then(|u| u.get("user"))
                            .and_then(|u| u.get("access_token"))
                            .and_then(|t| t.as_str())
                        {
                            println!("{}", token);
                        } else {
                            eprintln!("Error: Could not extract token from response");
                            std::process::exit(1);
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                },
                TokenCommands::Regenerate => {
                    handle_response(client.post_empty("/api/v1/me/regenerate-token"), cli.raw);
                }
            }
        }

        Commands::Ntfy(cmd) => {
            require_token(&cli.token);
            match cmd {
                NtfyCommands::Show => {
                    handle_response_with(client.get("/api/v1/me/ntfy"), cli.raw, format_ntfy);
                }
                NtfyCommands::Enable | NtfyCommands::Disable => {
                    let enabled = matches!(cmd, NtfyCommands::Enable);
                    let body = serde_json::json!({"enabled": enabled});
                    handle_response_with(client.patch("/api/v1/me/ntfy", &body), cli.raw, |json| {
                        println!("Ntfy notifications {}", if get_bool(json, "enabled") { "enabled" } else { "disabled" });
                    });
                }
            }
        }

        Commands::Language { code } => {
            require_token(&cli.token);
            let body = serde_json::json!({"language_code": code});
            handle_response_with(client.patch("/api/v1/me/language", &body), cli.raw, |json| {
                if let Some(lang) = json.get("language_code").and_then(|l| l.as_str()) {
                    println!("Language set to: {}", lang);
                } else {
                    print_json(json);
                }
            });
        }

        Commands::Pause => {
            require_token(&cli.token);
            handle_response_with(client.post_empty("/api/v1/me/pause"), cli.raw, |json| {
                if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
                    println!("{}", msg);
                } else {
                    print_json(json);
                }
            });
        }

        Commands::Unpause => {
            require_token(&cli.token);
            handle_response_with(client.post_empty("/api/v1/me/unpause"), cli.raw, |json| {
                if let Some(msg) = json.get("message").and_then(|m| m.as_str()) {
                    println!("{}", msg);
                } else {
                    print_json(json);
                }
            });
        }

        Commands::Settings(cmd) => {
            require_token(&cli.token);
            match cmd {
                SettingsCommands::Show => {
                    handle_response_with(client.get("/api/v1/me/settings"), cli.raw, |json| {
                        println!("Settings");
                        println!("========");
                        println!("Up delay:     {}s", get_i64(json, "up_delay"));
                        let start = json.get("maint_window_start_utc").and_then(|v| v.as_i64());
                        let end = json.get("maint_window_end_utc").and_then(|v| v.as_i64());
                        match (start, end) {
                            (Some(s), Some(e)) => {
                                println!("Maintenance:  {:02}:{:02}-{:02}:{:02} UTC", s / 60, s % 60, e / 60, e % 60);
                            }
                            _ => println!("Maintenance:  [not set]"),
                        }
                    });
                }
                SettingsCommands::Delay { seconds } => {
                    let body = serde_json::json!({"up_delay": seconds});
                    handle_response_with(client.patch("/api/v1/me/settings", &body), cli.raw, format_settings_update);
                }
                SettingsCommands::Maintenance { start, end } => {
                    let body = match (start, end) {
                        (Some(s), Some(e)) => {
                            let parse_time = |t: &str| -> Result<i16, String> {
                                let parts: Vec<&str> = t.split(':').collect();
                                if parts.len() != 2 {
                                    return Err(format!("Invalid time format '{}', expected HH:MM", t));
                                }
                                let h: i16 = parts[0].parse().map_err(|_| format!("Invalid hour in '{}'", t))?;
                                let m: i16 = parts[1].parse().map_err(|_| format!("Invalid minute in '{}'", t))?;
                                if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                                    return Err(format!("Time '{}' out of range (00:00-23:59)", t));
                                }
                                Ok(h * 60 + m)
                            };
                            let start_min = match parse_time(&s) {
                                Ok(v) => v,
                                Err(e) => {
                                    eprintln!("Error: {}", e);
                                    std::process::exit(1);
                                }
                            };
                            let end_min = match parse_time(&e) {
                                Ok(v) => v,
                                Err(e) => {
                                    eprintln!("Error: {}", e);
                                    std::process::exit(1);
                                }
                            };
                            serde_json::json!({
                                "maint_window_start_utc": start_min,
                                "maint_window_end_utc": end_min
                            })
                        }
                        (None, None) => {
                            serde_json::json!({
                                "maint_window_start_utc": null,
                                "maint_window_end_utc": null
                            })
                        }
                        _ => {
                            eprintln!("Error: Provide both start and end times, or neither to clear");
                            std::process::exit(1);
                        }
                    };
                    handle_response_with(client.patch("/api/v1/me/settings", &body), cli.raw, format_settings_update);
                }
            }
        }

        Commands::Admin(cmd) => {
            require_token(&cli.token);
            match cmd {
                AdminCommands::Users => {
                    handle_response_with(client.get("/api/v1/admin/users"), cli.raw, format_users_list);
                }
                AdminCommands::User { id } => {
                    handle_response_with(client.get(&format!("/api/v1/admin/users/{}", id)), cli.raw, format_me);
                }
                AdminCommands::Invites => {
                    handle_response_with(client.get("/api/v1/invites"), cli.raw, format_invites_list);
                }
                AdminCommands::CreateInvite => {
                    handle_response_with(client.post_empty("/api/v1/invites"), cli.raw, |json| {
                        if let Some(token) = json.get("invite").and_then(|i| i.get("token")).and_then(|t| t.as_str()) {
                            println!("Invite created successfully!");
                            println!("Invite token: {}", token);
                        } else {
                            print_json(json);
                        }
                    });
                }
                AdminCommands::DeleteInvite { id } => {
                    handle_response(client.delete(&format!("/api/v1/invites/{}", id)), cli.raw);
                }
                AdminCommands::DeleteUser { id } => {
                    handle_response(client.delete(&format!("/api/v1/admin/users/{}", id)), cli.raw);
                }
            }
        }
    }
}
