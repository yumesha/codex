use codex_app_server_protocol::AuthMode;
use codex_common::{CliConfigOverrides, create_config_summary_entries};
use codex_core::{CodexAuth, INTERACTIVE_SESSION_SOURCES, RolloutRecorder};
use codex_core::config::Config;
use codex_core::protocol::{NetworkAccess, RateLimitSnapshot, SandboxPolicy, TokenUsage};

pub async fn run_status(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    // Print status information
    println!("Codex Status");
    println!("============");
    println!();

    // Model information
    let model_name = config.model.as_deref().unwrap_or("<default>");
    println!("Model: {}", model_name);

    // Provider information
    let provider = &config.model_provider;
    let provider_name = if provider.name.trim().is_empty() {
        &config.model_provider_id
    } else {
        provider.name.trim()
    };
    println!("Provider: {}", provider_name);

    // Working directory
    println!("Directory: {}", config.cwd.display());

    // Approval policy
    let config_entries = create_config_summary_entries(&config, model_name);
    let approval = config_entries
        .iter()
        .find(|(k, _)| *k == "approval")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "<unknown>".to_string());
    println!("Approval: {}", approval);

    // Sandbox policy
    let sandbox_str = match config.sandbox_policy.get() {
        SandboxPolicy::DangerFullAccess => "danger-full-access",
        SandboxPolicy::ReadOnly => "read-only",
        SandboxPolicy::WorkspaceWrite { .. } => "workspace-write",
        SandboxPolicy::ExternalSandbox { network_access } => {
            if matches!(network_access, NetworkAccess::Enabled) {
                "external-sandbox (network access enabled)"
            } else {
                "external-sandbox"
            }
        }
    };
    println!("Sandbox: {}", sandbox_str);
    println!();

    // Session information
    println!("Session Configuration");
    println!("---------------------");

    // Try to load the most recent session's token usage
    match RolloutRecorder::list_threads(
        &config.codex_home,
        1, // Get just the most recent
        None,
        INTERACTIVE_SESSION_SOURCES,
        Some(&[config.model_provider_id.clone()]),
        &config.model_provider_id,
    )
    .await
    {
        Ok(page) if !page.items.is_empty() => {
            // Found a recent session
            if let Some(thread) = page.items.first() {
                if let Some(created_at) = &thread.created_at {
                    println!("Most Recent Session: {}", created_at);
                }

                // Try to extract token usage and rate limits from the session file
                match extract_session_data(&thread.path).await {
                    Ok(data) => {
                        if let Some(usage) = data.token_usage {
                            println!("Total Tokens: {}", format_tokens(usage.blended_total()));
                            println!("Input Tokens: {}", format_tokens(usage.non_cached_input()));
                            if usage.cached_input() > 0 {
                                println!("Cached Input: {}", format_tokens(usage.cached_input()));
                            }
                            println!("Output Tokens: {}", format_tokens(usage.output_tokens));
                            if usage.reasoning_output_tokens > 0 {
                                println!("Reasoning Tokens: {}", format_tokens(usage.reasoning_output_tokens));
                            }
                        } else {
                            println!("Token Usage: No data available");
                        }

                        // Display rate limits
                        if let Some(limits) = data.rate_limits {
                            println!();
                            display_rate_limits(&limits);
                        }
                    }
                    Err(_) => {
                        println!("Token Usage: Error reading session");
                    }
                }
            }
        }
        _ => {
            println!("Session: No recent sessions found");
            println!("Token Usage: N/A");
        }
    }
    println!();

    // Authentication status
    println!("Authentication");
    println!("--------------");
    match CodexAuth::from_auth_storage(&config.codex_home, config.cli_auth_credentials_store_mode) {
        Ok(Some(auth)) => match auth.mode {
            AuthMode::ApiKey => {
                println!("Status: Logged in (API key)");
            }
            AuthMode::ChatGPT => {
                println!("Status: Logged in (ChatGPT)");
            }
        },
        Ok(None) => {
            println!("Status: Not logged in");
        }
        Err(e) => {
            eprintln!("Error: {}", e);
        }
    }

    std::process::exit(0);
}

async fn load_config_or_exit(cli_config_overrides: CliConfigOverrides) -> Config {
    let cli_overrides = match cli_config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {}", e);
            std::process::exit(1);
        }
    };

    match Config::load_with_cli_overrides(cli_overrides).await {
        Ok(config) => config,
        Err(e) => {
            eprintln!("Error loading configuration: {}", e);
            std::process::exit(1);
        }
    }
}

struct SessionData {
    token_usage: Option<TokenUsage>,
    rate_limits: Option<RateLimitSnapshot>,
}

async fn extract_session_data(session_path: &std::path::Path) -> std::io::Result<SessionData> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let file = tokio::fs::File::open(session_path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let mut total_usage = TokenUsage::default();
    let mut found_usage = false;
    let mut rate_limits = None;

    while let Some(line) = lines.next_line().await? {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            // Look for event_msg events with token_count payload
            if value.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
                if let Some(payload) = value.get("payload") {
                    if payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                        // Extract token usage info
                        if let Some(info_obj) = payload.get("info") {
                            if let Some(total_obj) = info_obj.get("total_token_usage") {
                                if let Ok(usage) = serde_json::from_value::<TokenUsage>(total_obj.clone()) {
                                    total_usage = usage;
                                    found_usage = true;
                                }
                            }
                        }

                        // Extract rate limits
                        if let Some(limits_obj) = payload.get("rate_limits") {
                            if let Ok(limits) = serde_json::from_value::<RateLimitSnapshot>(limits_obj.clone()) {
                                rate_limits = Some(limits);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(SessionData {
        token_usage: if found_usage {
            Some(total_usage)
        } else {
            None
        },
        rate_limits,
    })
}

fn display_rate_limits(limits: &RateLimitSnapshot) {
    println!("Rate Limits");
    println!("-----------");

    if let Some(primary) = &limits.primary {
        let label = if let Some(minutes) = primary.window_minutes {
            format_duration_label(minutes)
        } else {
            "5h".to_string()
        };
        display_rate_limit_bar(&label, primary.used_percent, primary.resets_at);
    }

    if let Some(secondary) = &limits.secondary {
        let label = if let Some(minutes) = secondary.window_minutes {
            format_duration_label(minutes)
        } else {
            "Weekly".to_string()
        };
        display_rate_limit_bar(&label, secondary.used_percent, secondary.resets_at);
    }

    if let Some(credits) = &limits.credits {
        if credits.has_credits {
            if credits.unlimited {
                println!("\nCredits: Unlimited");
            } else if let Some(balance) = &credits.balance {
                println!("\nCredits: {}", balance);
            }
        }
    }
}

fn display_rate_limit_bar(label: &str, used_percent: f64, resets_at: Option<i64>) {
    const BAR_WIDTH: usize = 30;
    let percent_left = 100.0 - used_percent;
    let filled = ((used_percent / 100.0) * BAR_WIDTH as f64).round() as usize;
    let empty = BAR_WIDTH.saturating_sub(filled);

    let bar = format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(empty)
    );

    let reset_str = if let Some(ts) = resets_at {
        format!(" (resets {})", format_reset_time(ts))
    } else {
        String::new()
    };

    println!(
        "{:13} [{}] {:.0}% left{}",
        format!("{} limit:", label),
        bar,
        percent_left,
        reset_str
    );
}

fn format_duration_label(minutes: i64) -> String {
    if minutes < 60 {
        format!("{}m", minutes)
    } else if minutes < 1440 {
        format!("{}h", minutes / 60)
    } else if minutes < 10080 {
        format!("{}d", minutes / 1440)
    } else {
        "Weekly".to_string()
    }
}

fn format_reset_time(unix_timestamp: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let diff = unix_timestamp - now;

    if diff < 0 {
        return "recently".to_string();
    }

    // Convert to local time using libc (available from workspace deps)
    let local_time = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let timestamp = unix_timestamp as libc::time_t;
        libc::localtime_r(&timestamp, &mut tm);
        tm
    };

    let hour = local_time.tm_hour;
    let min = local_time.tm_min;
    let day = local_time.tm_mday;
    let month = match local_time.tm_mon {
        0 => "Jan", 1 => "Feb", 2 => "Mar", 3 => "Apr",
        4 => "May", 5 => "Jun", 6 => "Jul", 7 => "Aug",
        8 => "Sep", 9 => "Oct", 10 => "Nov", 11 => "Dec",
        _ => "???",
    };

    // Check if it's today
    let now_local = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };

    if local_time.tm_year == now_local.tm_year
        && local_time.tm_mon == now_local.tm_mon
        && local_time.tm_mday == now_local.tm_mday
    {
        // Today - just show time
        format!("{:02}:{:02}", hour, min)
    } else {
        // Different day - show time and date
        format!("{:02}:{:02} on {} {}", hour, min, day, month)
    }
}

fn format_tokens(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_format_tokens_small() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(999), "999");
    }

    #[test]
    fn test_format_tokens_thousands() {
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(45_200), "45.2k");
    }

    #[test]
    fn test_format_tokens_millions() {
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(2_300_000), "2.3M");
    }

    #[test]
    fn test_format_duration_label() {
        assert_eq!(format_duration_label(30), "30m");
        assert_eq!(format_duration_label(60), "1h");
        assert_eq!(format_duration_label(300), "5h");
        assert_eq!(format_duration_label(1440), "1d");
        assert_eq!(format_duration_label(10080), "Weekly");
    }

    #[test]
    fn test_format_reset_time() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Past time
        let past = now - 3600;
        assert_eq!(format_reset_time(past), "recently");

        // Future time (same day) - should return HH:MM format
        let future = now + 7200; // 2 hours from now
        let result = format_reset_time(future);
        assert!(result.contains(':'));
        assert!(result.len() == 5 || result.contains("on")); // Either HH:MM or HH:MM on DD Mon
    }

    #[tokio::test]
    async fn test_extract_session_data_empty_file() {
        let mut temp_file = NamedTempFile::new().unwrap();

        let result = extract_session_data(temp_file.path()).await.unwrap();

        assert!(result.token_usage.is_none());
        assert!(result.rate_limits.is_none());
    }

    #[tokio::test]
    async fn test_extract_session_data_with_token_count() {
        let mut temp_file = NamedTempFile::new().unwrap();

        // Write a token_count event
        let event = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1500,"input_tokens":1000,"output_tokens":500,"cached_input_tokens":200,"reasoning_output_tokens":100}}}}"#;
        writeln!(temp_file, "{}", event).unwrap();
        temp_file.flush().unwrap();

        let result = extract_session_data(temp_file.path()).await.unwrap();

        assert!(result.token_usage.is_some());
        let usage = result.token_usage.unwrap();
        assert_eq!(usage.total_tokens, 1500);
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
    }

    #[tokio::test]
    async fn test_extract_session_data_with_rate_limits() {
        let mut temp_file = NamedTempFile::new().unwrap();

        // Write a token_count event with rate limits
        let event = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100,"input_tokens":60,"output_tokens":40,"cached_input_tokens":0,"reasoning_output_tokens":0}},"rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1736789074},"secondary":{"used_percent":5.0,"window_minutes":10080,"resets_at":1737393874}}}}"#;
        writeln!(temp_file, "{}", event).unwrap();
        temp_file.flush().unwrap();

        let result = extract_session_data(temp_file.path()).await.unwrap();

        assert!(result.rate_limits.is_some());
        let limits = result.rate_limits.unwrap();
        assert!(limits.primary.is_some());
        assert!(limits.secondary.is_some());

        let primary = limits.primary.unwrap();
        assert_eq!(primary.used_percent, 1.0);
        assert_eq!(primary.window_minutes, Some(300));

        let secondary = limits.secondary.unwrap();
        assert_eq!(secondary.used_percent, 5.0);
        assert_eq!(secondary.window_minutes, Some(10080));
    }

    #[tokio::test]
    async fn test_extract_session_data_multiple_events() {
        let mut temp_file = NamedTempFile::new().unwrap();

        // Write multiple events, last token_count should win
        writeln!(temp_file, r#"{{"type":"session_meta","payload":{{"id":"test"}}}}"#).unwrap();
        writeln!(temp_file, r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":1000,"input_tokens":600,"output_tokens":400,"cached_input_tokens":0,"reasoning_output_tokens":0}}}}}}}}"#).unwrap();
        writeln!(temp_file, r#"{{"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"total_tokens":2000,"input_tokens":1200,"output_tokens":800,"cached_input_tokens":0,"reasoning_output_tokens":0}}}}}}}}"#).unwrap();
        temp_file.flush().unwrap();

        let result = extract_session_data(temp_file.path()).await.unwrap();

        assert!(result.token_usage.is_some());
        let usage = result.token_usage.unwrap();
        // Should use the last token_count event
        assert_eq!(usage.total_tokens, 2000);
    }

    #[test]
    fn test_display_rate_limit_bar_calculation() {
        // Test that the bar calculation works correctly
        const BAR_WIDTH: usize = 30;

        // 1% used = 1 filled char
        let used_1_percent = 1.0;
        let filled = ((used_1_percent / 100.0) * BAR_WIDTH as f64).round() as usize;
        assert_eq!(filled, 0); // Rounds to 0

        // 50% used = 15 filled chars
        let used_50_percent = 50.0;
        let filled = ((used_50_percent / 100.0) * BAR_WIDTH as f64).round() as usize;
        assert_eq!(filled, 15);

        // 100% used = 30 filled chars
        let used_100_percent = 100.0;
        let filled = ((used_100_percent / 100.0) * BAR_WIDTH as f64).round() as usize;
        assert_eq!(filled, 30);
    }
}
