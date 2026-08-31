use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::json;

use crate::runtime::{SESSION_SCHEMA, SessionStore, format_femto_amount_3dp};

#[derive(Args, Debug)]
pub(crate) struct SessionsArgs {
    #[arg(long)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: SessionsCommand,
}

#[derive(Subcommand, Debug)]
enum SessionsCommand {
    /// List recent visible-history sessions.
    List,
    /// Show one session and its per-response usage ledger.
    Show {
        id: String,
        /// Include the append-only event trace.
        #[arg(long)]
        trace: bool,
    },
}

pub(crate) struct SessionsCommandResult {
    pub(crate) exit_code: u8,
}

pub(crate) fn run(args: &SessionsArgs) -> SessionsCommandResult {
    match run_inner(args) {
        Ok(()) => SessionsCommandResult { exit_code: 0 },
        Err(error) => {
            eprintln!("! {error}");
            SessionsCommandResult { exit_code: 1 }
        }
    }
}

fn run_inner(args: &SessionsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = SessionStore::discover(args.state_dir.as_deref())?;
    match &args.command {
        SessionsCommand::List => {
            for session in store.list()? {
                let cost = session
                    .usage
                    .as_ref()
                    .expect("session list loads usage")
                    .total_cost
                    .as_ref()
                    .map(|cost| {
                        format!(
                            "{} {}",
                            cost.currency,
                            format_femto_amount_3dp(cost.femto_units())
                        )
                    })
                    .unwrap_or_else(|| "—".to_string());
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}+{}\t{} child\t{}\t{}",
                    session.id,
                    session.source,
                    session.status,
                    session.model,
                    session.engine,
                    session.turns,
                    session.summarized_turns,
                    session.related_sessions,
                    cost,
                    session.workspace.display()
                );
            }
        }
        SessionsCommand::Show { id, trace } => {
            let session = store.load(id, None)?;
            let aggregate_usage = store.aggregate_usage(&session)?;
            let trace = if *trace {
                Some(
                    store
                        .read_trace(id)?
                        .lines()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect::<Vec<serde_json::Value>>(),
                )
            } else {
                None
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "schema": SESSION_SCHEMA,
                    "id": session.id,
                    "workspace": session.workspace,
                    "model": session.model,
                    "engine": session.engine,
                    "upstream_thread_id": session.upstream_thread_id,
                    "source": session.source,
                    "status": session.status,
                    "owner_pid": session.owner_pid,
                    "attribution": session.attribution,
                    "related_sessions": session.related_sessions,
                    "handoff": session.handoff,
                    "created_at": session.created_at,
                    "updated_at": session.updated_at,
                    "summarized_turns": session.summarized_turns,
                    "summary": session.summary,
                    "turns": session.turns,
                    "pending_user": session.pending_visible_user(),
                    "usage": {
                        "summary": session.usage.summary(),
                        "aggregate": aggregate_usage,
                        "entries": session.usage.entries(),
                    },
                    "trace": trace,
                }))?
            );
        }
    }
    Ok(())
}
