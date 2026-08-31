use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand};

use crate::runtime::{NativeProvider, PriceCatalogStore};
use crate::terminal::plain_text;

#[derive(Args, Debug)]
pub(crate) struct PriceArgs {
    #[command(subcommand)]
    command: PriceCommand,
}

#[derive(Subcommand, Debug)]
enum PriceCommand {
    /// Show the selected catalog, cache, source, and next check.
    Status(PriceLocationArgs),
    /// Download and cache a configured Agul JSON price catalog now.
    Sync(PriceLocationArgs),
}

#[derive(Args, Debug)]
struct PriceLocationArgs {
    /// Embedded provider catalog used when no downloaded catalog applies.
    #[arg(long, env = "AGUL_PROVIDER", default_value_t)]
    provider: NativeProvider,

    /// Remote Agul JSON catalog. Otherwise uses AGUL_PRICE_CATALOG_URL or the last sync source.
    #[arg(long)]
    url: Option<String>,

    /// Override the local state directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,
}

pub(crate) struct PriceCommandResult {
    pub(crate) exit_code: u8,
}

pub(crate) fn run(args: &PriceArgs) -> PriceCommandResult {
    match run_inner(args) {
        Ok(()) => PriceCommandResult { exit_code: 0 },
        Err(error) => {
            eprintln!("! {}", plain_text(&error.to_string()));
            PriceCommandResult { exit_code: 1 }
        }
    }
}

fn run_inner(args: &PriceArgs) -> Result<(), Box<dyn std::error::Error>> {
    let location = match &args.command {
        PriceCommand::Status(location) | PriceCommand::Sync(location) => location,
    };
    let builtin = location.provider.catalog();
    let store = PriceCatalogStore::discover(location.state_dir.as_deref(), Some(&builtin))?;
    match &args.command {
        PriceCommand::Status(location) => {
            let status = store.status(location.url.as_deref(), Some(&builtin))?;
            let catalog = match (&status.catalog_id, &status.catalog_version) {
                (Some(id), Some(version)) => format!("{id}@{version}"),
                _ => "none".to_string(),
            };
            let location = if status.using_embedded {
                "embedded".to_string()
            } else {
                status
                    .catalog_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "none".to_string())
            };
            let stale = if status.stale { " · review due" } else { "" };
            println!("{catalog} · {location}{stale}");
            println!(
                "source {}",
                status.configured_url.as_deref().unwrap_or("not configured")
            );
            println!(
                "checked {} · synced {} · next {}",
                relative_time(status.last_attempt_at, now()),
                relative_time(status.last_success_at, now()),
                next_check_time(status.next_check_at, now())
            );
            if let Some(error) = status.last_error {
                println!("last error {}", plain_text(&error));
            }
            println!("cache {}", status.cache_root.display());
        }
        PriceCommand::Sync(location) => {
            let url = store
                .configured_url(location.url.as_deref())?
                .ok_or("no price catalog URL; use --url or AGUL_PRICE_CATALOG_URL")?;
            let result = store.sync(&url, Some(&builtin))?;
            let marker = if result.changed { "updated" } else { "current" };
            println!(
                "{}@{} · {marker} · {}",
                result.catalog.id,
                result.catalog.version,
                result.cache_path.display()
            );
        }
    }
    Ok(())
}

fn relative_time(value: Option<u64>, now: u64) -> String {
    let Some(value) = value else {
        return "never".to_string();
    };
    if value == now {
        return "now".to_string();
    }
    if value > now {
        return format!("in {}", duration_label(value - now));
    }
    format!("{} ago", duration_label(now - value))
}

fn next_check_time(value: Option<u64>, now: u64) -> String {
    value.map_or_else(
        || "not scheduled".to_string(),
        |value| relative_time(Some(value), now),
    )
}

fn duration_label(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_stay_compact() {
        assert_eq!(relative_time(None, 100), "never");
        assert_eq!(relative_time(Some(99), 100), "1s ago");
        assert_eq!(relative_time(Some(100), 100), "now");
        assert_eq!(relative_time(Some(3_700), 100), "in 1h");
        assert_eq!(next_check_time(None, 100), "not scheduled");
        assert_eq!(next_check_time(Some(100), 100), "now");
    }
}
