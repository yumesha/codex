use codex_backend_client::Client as BackendClient;
use codex_common::CliConfigOverrides;
use codex_common::create_config_summary_entries;
use codex_core::CodexAuth;
use codex_core::INTERACTIVE_SESSION_SOURCES;
use codex_core::RolloutRecorder;
use codex_core::ThreadSortKey;
use codex_core::config::Config;
use codex_core::project_doc::discover_project_doc_paths;
use codex_core::protocol::NetworkAccess;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::TokenUsage;
use std::path::Path;

const CODEX_CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

pub async fn run_status(cli_config_overrides: CliConfigOverrides) -> ! {
    let config = load_config_or_exit(cli_config_overrides).await;

    // Header like TUI
    println!();
    println!(">_ OpenAI Codex (v{})", CODEX_CLI_VERSION);
    println!();
    println!("Visit https://chatgpt.com/codex/settings/usage for up-to-date");
    println!("information on rate limits and credits");
    println!();

    // Build list of labels to calculate alignment
    let model_name = config.model.as_deref().unwrap_or("<default>");
    let config_entries = create_config_summary_entries(&config, model_name);

    let mut labels = vec!["Model", "Directory", "Approval", "Sandbox", "Agents.md"];

    // Check if we need Model provider
    let model_provider = format_model_provider(&config);
    if model_provider.is_some() {
        labels.push("Model provider");
    }

    // Check auth for fetching live rate limits
    let auth_info =
        CodexAuth::from_auth_storage(&config.codex_home, config.cli_auth_credentials_store_mode)
            .ok()
            .flatten();

    // Session will be added if found
    labels.push("Session");

    // Rate limit labels
    labels.push("5h limit");
    labels.push("Weekly limit");

    let label_width = labels.iter().map(|l| l.len()).max().unwrap_or(0);

    // Model with details
    let (model_display, model_details) = compose_model_display(model_name, &config_entries);
    if model_details.is_empty() {
        print_field("Model", &model_display, label_width);
    } else {
        print_field(
            "Model",
            &format!("{} ({})", model_display, model_details.join(", ")),
            label_width,
        );
    }

    // Model provider (only if not default OpenAI)
    if let Some(provider) = model_provider {
        print_field("Model provider", &provider, label_width);
    }

    // Directory (relativized to home)
    let directory = format_directory_display(&config.cwd);
    print_field("Directory", &directory, label_width);

    // Approval
    let approval = config_entries
        .iter()
        .find(|(k, _)| *k == "approval")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "<unknown>".to_string());
    print_field("Approval", &approval, label_width);

    // Sandbox
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
    print_field("Sandbox", sandbox_str, label_width);

    // Agents.md
    let agents_summary = compose_agents_summary(&config);
    print_field("Agents.md", &agents_summary, label_width);

    // Session from most recent session
    let session_id = match RolloutRecorder::list_threads(
        &config.codex_home,
        1,
        None,
        ThreadSortKey::UpdatedAt,
        INTERACTIVE_SESSION_SOURCES,
        Some(&[config.model_provider_id.clone()]),
        &config.model_provider_id,
    )
    .await
    {
        Ok(page) if !page.items.is_empty() => {
            if let Some(thread) = page.items.first() {
                if let Some(filename) = thread.path.file_stem() {
                    let session_id = filename.to_string_lossy().to_string();
                    print_field("Session", &session_id, label_width);
                    Some(session_id)
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };

    println!();

    // Try to fetch real-time rate limits from API
    let rate_limits = if let Some(auth) = &auth_info {
        fetch_rate_limits_from_api(auth).await
    } else {
        None
    };

    // Display rate limits (real-time or fallback message)
    if let Some(limits) = rate_limits {
        display_rate_limits(&limits, label_width);
    } else {
        // Fall back to session file if API call failed
        if let Some(_session) = session_id {
            if let Ok(page) = RolloutRecorder::list_threads(
                &config.codex_home,
                1,
                None,
                ThreadSortKey::UpdatedAt,
                INTERACTIVE_SESSION_SOURCES,
                Some(&[config.model_provider_id.clone()]),
                &config.model_provider_id,
            )
            .await
            {
                if let Some(thread) = page.items.first() {
                    if let Ok(data) = extract_session_data(&thread.path).await {
                        if let Some(limits) = data.rate_limits {
                            display_rate_limits(&limits, label_width);
                        } else {
                            print_field("5h limit", "data not available yet", label_width);
                        }
                    } else {
                        print_field("5h limit", "data not available yet", label_width);
                    }
                } else {
                    print_field("5h limit", "data not available yet", label_width);
                }
            } else {
                print_field("5h limit", "data not available yet", label_width);
            }
        } else {
            print_field("5h limit", "data not available yet", label_width);
        }
    }

    std::process::exit(0);
}

async fn fetch_rate_limits_from_api(auth: &CodexAuth) -> Option<RateLimitSnapshot> {
    let base_url = "https://chatgpt.com";
    let client = BackendClient::from_auth(base_url, auth).ok()?;
    client.get_rate_limits().await.ok()
}

fn print_field(label: &str, value: &str, label_width: usize) {
    println!(
        " {:width$}   {}",
        format!("{}:", label),
        value,
        width = label_width + 1
    );
}

fn compose_model_display(model_name: &str, entries: &[(&str, String)]) -> (String, Vec<String>) {
    let mut details: Vec<String> = Vec::new();
    if let Some((_, effort)) = entries.iter().find(|(k, _)| *k == "reasoning effort") {
        details.push(format!("reasoning {}", effort.to_ascii_lowercase()));
    }
    if let Some((_, summary)) = entries.iter().find(|(k, _)| *k == "reasoning summaries") {
        let summary = summary.trim();
        if summary.eq_ignore_ascii_case("none") || summary.eq_ignore_ascii_case("off") {
            details.push("summaries off".to_string());
        } else if !summary.is_empty() {
            details.push(format!("summaries {}", summary.to_ascii_lowercase()));
        }
    }
    (model_name.to_string(), details)
}

fn compose_agents_summary(config: &Config) -> String {
    match discover_project_doc_paths(config) {
        Ok(paths) => {
            let mut rels: Vec<String> = Vec::new();
            for p in paths {
                let file_name = p
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                let display = if let Some(parent) = p.parent() {
                    if parent == config.cwd {
                        file_name.clone()
                    } else {
                        let mut cur = config.cwd.as_path();
                        let mut ups = 0usize;
                        let mut reached = false;
                        while let Some(c) = cur.parent() {
                            if cur == parent {
                                reached = true;
                                break;
                            }
                            cur = c;
                            ups += 1;
                        }
                        if reached {
                            let up = format!("..{}", std::path::MAIN_SEPARATOR);
                            format!("{}{}", up.repeat(ups), file_name)
                        } else if let Ok(stripped) = p.strip_prefix(&config.cwd) {
                            dunce::simplified(stripped).display().to_string()
                        } else {
                            dunce::simplified(&p).display().to_string()
                        }
                    }
                } else {
                    dunce::simplified(&p).display().to_string()
                };
                rels.push(display);
            }
            if rels.is_empty() {
                "<none>".to_string()
            } else {
                rels.join(", ")
            }
        }
        Err(_) => "<none>".to_string(),
    }
}

fn format_model_provider(config: &Config) -> Option<String> {
    let provider = &config.model_provider;
    let name = provider.name.trim();
    let provider_name = if name.is_empty() {
        config.model_provider_id.as_str()
    } else {
        name
    };

    let is_default_openai = provider.is_openai() && provider.base_url.is_none();
    if is_default_openai {
        return None;
    }

    Some(provider_name.to_string())
}

fn format_directory_display(directory: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = directory.strip_prefix(&home) {
            if rel.as_os_str().is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rel.display());
        }
    }
    directory.display().to_string()
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
    #[allow(dead_code)]
    token_usage: Option<TokenUsage>,
    rate_limits: Option<RateLimitSnapshot>,
}

async fn extract_session_data(session_path: &std::path::Path) -> std::io::Result<SessionData> {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader;

    let file = tokio::fs::File::open(session_path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();

    let mut total_usage = TokenUsage::default();
    let mut found_usage = false;
    let mut rate_limits = None;

    while let Some(line) = lines.next_line().await? {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value.get("type").and_then(|v| v.as_str()) == Some("event_msg") {
                if let Some(payload) = value.get("payload") {
                    if payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                        if let Some(info_obj) = payload.get("info") {
                            if let Some(total_obj) = info_obj.get("total_token_usage") {
                                if let Ok(usage) =
                                    serde_json::from_value::<TokenUsage>(total_obj.clone())
                                {
                                    total_usage = usage;
                                    found_usage = true;
                                }
                            }
                        }

                        if let Some(limits_obj) = payload.get("rate_limits") {
                            if let Ok(limits) =
                                serde_json::from_value::<RateLimitSnapshot>(limits_obj.clone())
                            {
                                rate_limits = Some(limits);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(SessionData {
        token_usage: if found_usage { Some(total_usage) } else { None },
        rate_limits,
    })
}

fn display_rate_limits(limits: &RateLimitSnapshot, label_width: usize) {
    if let Some(primary) = &limits.primary {
        let label = if let Some(minutes) = primary.window_minutes {
            format_duration_label(minutes)
        } else {
            "5h".to_string()
        };
        display_rate_limit_bar(
            &format!("{} limit", label),
            primary.used_percent,
            primary.resets_at,
            label_width,
        );
    }

    if let Some(secondary) = &limits.secondary {
        let label = if let Some(minutes) = secondary.window_minutes {
            format_duration_label(minutes)
        } else {
            "Weekly".to_string()
        };
        display_rate_limit_bar(
            &format!("{} limit", label),
            secondary.used_percent,
            secondary.resets_at,
            label_width,
        );
    }

    if let Some(credits) = &limits.credits {
        if credits.has_credits {
            if credits.unlimited {
                print_field("Credits", "Unlimited", label_width);
            } else if let Some(balance) = &credits.balance {
                print_field("Credits", &format!("{} credits", balance), label_width);
            }
        }
    }
}

fn display_rate_limit_bar(
    label: &str,
    used_percent: f64,
    resets_at: Option<i64>,
    label_width: usize,
) {
    const BAR_WIDTH: usize = 20;
    let percent_left = 100.0 - used_percent;
    let filled = ((percent_left / 100.0) * BAR_WIDTH as f64).round() as usize;
    let empty = BAR_WIDTH.saturating_sub(filled);

    let bar = format!(
        "[{}{}]",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty)
    );

    let reset_str = if let Some(ts) = resets_at {
        format!(" (resets {})", format_reset_time(ts))
    } else {
        String::new()
    };

    println!(
        " {:width$}   {} {:.0}% left{}",
        format!("{}:", label),
        bar,
        percent_left,
        reset_str,
        width = label_width + 1
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
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

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
        0 => "Jan",
        1 => "Feb",
        2 => "Mar",
        3 => "Apr",
        4 => "May",
        5 => "Jun",
        6 => "Jul",
        7 => "Aug",
        8 => "Sep",
        9 => "Oct",
        10 => "Nov",
        11 => "Dec",
        _ => "???",
    };

    let now_local = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };

    if local_time.tm_year == now_local.tm_year
        && local_time.tm_mon == now_local.tm_mon
        && local_time.tm_mday == now_local.tm_mday
    {
        format!("{:02}:{:02}", hour, min)
    } else {
        format!("{:02}:{:02} on {} {}", hour, min, day, month)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

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
        use std::time::SystemTime;
        use std::time::UNIX_EPOCH;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Past time should still show time format
        let past = now - 3600;
        let result = format_reset_time(past);
        assert!(result.contains(':'));

        // Future time should show time format
        let future = now + 7200;
        let result = format_reset_time(future);
        assert!(result.contains(':'));
    }

    #[tokio::test]
    async fn test_extract_session_data_empty_file() {
        let temp_file = NamedTempFile::new().unwrap();
        let result = extract_session_data(temp_file.path()).await.unwrap();
        assert!(result.token_usage.is_none());
        assert!(result.rate_limits.is_none());
    }

    #[tokio::test]
    async fn test_extract_session_data_with_rate_limits() {
        let mut temp_file = NamedTempFile::new().unwrap();
        let event = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":100,"input_tokens":60,"output_tokens":40,"cached_input_tokens":0,"reasoning_output_tokens":0}},"rate_limits":{"primary":{"used_percent":1.0,"window_minutes":300,"resets_at":1736789074},"secondary":{"used_percent":5.0,"window_minutes":10080,"resets_at":1737393874}}}}"#;
        writeln!(temp_file, "{}", event).unwrap();
        temp_file.flush().unwrap();

        let result = extract_session_data(temp_file.path()).await.unwrap();
        assert!(result.rate_limits.is_some());
        let limits = result.rate_limits.unwrap();
        assert!(limits.primary.is_some());
        assert!(limits.secondary.is_some());
    }
}
