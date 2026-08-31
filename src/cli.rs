use clap::{Parser, Subcommand};

use crate::commands::account::AccountArgs;
use crate::commands::ari::AriArgs;
use crate::commands::chat::ChatArgs;
use crate::commands::price::PriceArgs;
use crate::commands::sessions::SessionsArgs;

#[derive(Parser, Debug)]
#[command(name = "agul", version, about = "Minimal agent runtime")]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Connect a ChatGPT account and inspect Codex quota.
    Account(AccountArgs),
    /// Serve the ARI integration interface.
    Ari(AriArgs),
    /// Chat with an external model, four core tools, and launch plugins.
    Chat(Box<ChatArgs>),
    /// Inspect or refresh versioned price catalogs.
    Price(PriceArgs),
    /// Read local visible-history sessions and usage ledgers.
    Sessions(SessionsArgs),
}
