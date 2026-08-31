use std::process::Command;

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::runtime::{CodexAppServer, CodexLogin};

#[derive(Args, Debug)]
pub(crate) struct AccountArgs {
    #[command(subcommand)]
    command: AccountCommand,

    /// Codex executable used for the supported account bridge.
    #[arg(long, global = true, env = "AGUL_CODEX_COMMAND")]
    codex_command: Option<String>,
}

#[derive(Subcommand, Debug)]
enum AccountCommand {
    /// Show the active account, plan, quota windows, and token activity.
    Status {
        /// Emit the full account snapshot as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Sign in with ChatGPT and use the account's Codex allowance.
    Login {
        /// Show a device code instead of using the browser callback flow.
        #[arg(long)]
        device_code: bool,

        /// Print the login URL without opening the default browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Clear the shared Codex credential used by Agul, Codex CLI, and its extensions.
    Logout,
}

pub(crate) fn run(args: &AccountArgs) -> Result<u8, Box<dyn std::error::Error>> {
    let mut server = CodexAppServer::start(args.codex_command.as_deref())?;
    match &args.command {
        AccountCommand::Status { json } => show_status(&mut server, *json)?,
        AccountCommand::Login {
            device_code,
            no_open,
        } => login(&mut server, *device_code, *no_open)?,
        AccountCommand::Logout => {
            let account = server.account()?;
            let name = account_name(&account);
            server.logout()?;
            println!("○ {name} · signed out");
        }
    }
    Ok(0)
}

fn show_status(
    server: &mut CodexAppServer,
    as_json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let account = server.account()?;
    let chatgpt = account
        .pointer("/account/type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "chatgpt");
    let (rate_limits, rate_limits_error) = optional_call(chatgpt, || server.rate_limits());
    let (usage, usage_error) = optional_call(chatgpt, || server.usage());
    let warnings = [rate_limits_error, usage_error]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    if as_json {
        println!(
            "{}",
            serde_json::to_string(&json!({
                "ok": true,
                "account": account,
                "rate_limits": rate_limits,
                "usage": usage,
                "warnings": warnings,
            }))?
        );
        return Ok(());
    }

    println!(
        "{}",
        status_line(&account, rate_limits.as_ref(), usage.as_ref())
    );
    for warning in warnings {
        eprintln!("· {warning}");
    }
    Ok(())
}

fn optional_call<F, E>(enabled: bool, call: F) -> (Option<Value>, Option<String>)
where
    F: FnOnce() -> Result<Value, E>,
    E: std::fmt::Display,
{
    if !enabled {
        return (None, None);
    }
    match call() {
        Ok(value) => (Some(value), None),
        Err(error) => {
            let message = error.to_string();
            if message.to_ascii_lowercase().contains("method not found") {
                (None, None)
            } else {
                (None, Some(message))
            }
        }
    }
}

fn login(
    server: &mut CodexAppServer,
    device_code: bool,
    no_open: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let login = server.login(device_code)?;
    match &login {
        CodexLogin::Browser { auth_url, .. } => {
            println!("↗ {auth_url}");
            if !no_open && !open_browser(auth_url) {
                eprintln!("· open the link above to continue");
            }
        }
        CodexLogin::DeviceCode {
            verification_url,
            user_code,
            ..
        } => println!("↗ {verification_url} · {user_code}"),
    }
    println!("… waiting for ChatGPT");
    server.wait_for_login(login.login_id())?;
    let account = server.account()?;
    println!("{}", identity_label(&account));
    Ok(())
}

fn status_line(account: &Value, rate_limits: Option<&Value>, usage: Option<&Value>) -> String {
    let mut parts = vec![identity_label(account)];
    if let Some(limits) = rate_limits {
        let labels = quota_labels(limits);
        if !labels.is_empty() {
            parts.push(format!("◒ {}", labels.join(", ")));
        }
    }
    if let Some(tokens) = usage
        .and_then(|value| value.pointer("/summary/lifetimeTokens"))
        .and_then(Value::as_u64)
    {
        parts.push(format!("Σ {} tokens", compact_number(tokens)));
    }
    parts.join(" │ ")
}

fn identity_label(account: &Value) -> String {
    let Some(details) = account.get("account").filter(|value| !value.is_null()) else {
        return "○ ChatGPT · signed out".to_string();
    };
    let kind = details
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if kind == "chatgpt" {
        let mut label = String::from("● ChatGPT");
        if let Some(plan) = details.get("planType").and_then(Value::as_str) {
            label.push_str(" · ");
            label.push_str(plan);
        }
        if let Some(email) = details.get("email").and_then(Value::as_str) {
            label.push_str(" · ");
            label.push_str(email);
        }
        return label;
    }
    if kind == "apiKey" {
        return "● OpenAI API key · API billed".to_string();
    }
    format!("● {kind}")
}

fn account_name(account: &Value) -> &'static str {
    match account.pointer("/account/type").and_then(Value::as_str) {
        Some("chatgpt") => "ChatGPT",
        Some("apiKey") => "OpenAI API key",
        _ => "Codex account",
    }
}

fn quota_labels(value: &Value) -> Vec<String> {
    if let Some(buckets) = value.get("rateLimitsByLimitId").and_then(Value::as_object) {
        let mut labels = buckets
            .iter()
            .filter_map(|(id, bucket)| quota_label(id, bucket))
            .collect::<Vec<_>>();
        labels.sort();
        return labels;
    }
    value
        .get("rateLimits")
        .and_then(|bucket| {
            let id = bucket
                .get("limitId")
                .and_then(Value::as_str)
                .unwrap_or("codex");
            quota_label(id, bucket)
        })
        .into_iter()
        .collect()
}

fn quota_label(id: &str, bucket: &Value) -> Option<String> {
    let windows = ["primary", "secondary"]
        .into_iter()
        .filter_map(|name| bucket.get(name).and_then(quota_window_label))
        .collect::<Vec<_>>();
    (!windows.is_empty()).then(|| format!("{id} {}", windows.join(" + ")))
}

fn quota_window_label(window: &Value) -> Option<String> {
    let used = window.get("usedPercent")?.as_f64()?;
    let duration = window.get("windowDurationMins").and_then(Value::as_u64);
    let used = if used.fract() == 0.0 {
        format!("{used:.0}%")
    } else {
        format!("{used:.1}%")
    };
    Some(match duration {
        Some(minutes) => format!("{used}/{minutes}m"),
        None => used,
    })
}

fn compact_number(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.2}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "windows")]
    let result = Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn();
    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = Command::new("xdg-open").arg(url).spawn();
    result.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_one_compact_line() {
        let account = json!({
            "account": {"type": "chatgpt", "email": "a@example.com", "planType": "plus"},
            "requiresOpenaiAuth": true
        });
        let limits = json!({
            "rateLimitsByLimitId": {
                "codex": {
                    "primary": {"usedPercent": 25, "windowDurationMins": 15},
                    "secondary": {"usedPercent": 10.5, "windowDurationMins": 10080}
                }
            }
        });
        let usage = json!({"summary": {"lifetimeTokens": 1_234_567}});

        assert_eq!(
            status_line(&account, Some(&limits), Some(&usage)),
            "● ChatGPT · plus · a@example.com │ ◒ codex 25%/15m + 10.5%/10080m │ Σ 1.23M tokens"
        );
    }

    #[test]
    fn api_key_is_not_mislabeled_as_subscription_usage() {
        let account = json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true});

        assert_eq!(
            status_line(&account, None, None),
            "● OpenAI API key · API billed"
        );
    }

    #[test]
    fn signed_out_status_stays_short() {
        assert_eq!(
            status_line(&json!({"account": null}), None, None),
            "○ ChatGPT · signed out"
        );
    }
}
