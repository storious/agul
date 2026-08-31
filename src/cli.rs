use clap::{Parser, Subcommand};

use crate::commands::account::AccountArgs;
use crate::commands::ari::AriArgs;
use crate::commands::chat::ChatArgs;
use crate::commands::price::PriceArgs;
use crate::commands::sessions::SessionsArgs;

#[derive(Parser, Debug)]
#[command(
    name = "agul",
    bin_name = "agul",
    version,
    about = "Small terminal coding agent with four core tools and optional extensions",
    long_about = "Agul is a small terminal coding agent. Run it without a command to open the full-screen workbench in the current directory. It can read, write, edit, and run commands using DeepSeek by default, GLM Coding Plan, a ChatGPT/Codex account, or a local OpenAI-compatible model.",
    after_help = "Quick start:\n  agul                              Open the workbench in this directory\n  agul chat --provider glm          Use GLM Coding Plan\n  agul account login               Connect a ChatGPT account\n  agul chat --engine codex          Use ChatGPT/Codex quota\n  agul chat --continue              Continue the latest chat here\n\nRun `agul <command> --help` for command details."
)]
pub struct Cli {
    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Connect a ChatGPT account and inspect Codex quota.
    #[command(
        after_help = "Examples:\n  agul account login\n  agul account status\n  agul account logout"
    )]
    Account(AccountArgs),
    /// Serve the ARI interface used by Agulater and AgentKube.
    #[command(after_help = "Example:\n  agul ari serve")]
    Ari(AriArgs),
    /// Open the workbench or run one model turn.
    #[command(
        long_about = "Open Agul's full-screen workbench, or use --prompt for one non-interactive model turn. The native engine provides four core tools and can load Skills, context, and Plugins prepared by Agulater.",
        after_help = "Examples:\n  agul                                      Open the workbench with DeepSeek\n  agul chat --provider glm                  Use GLM Coding Plan (GLM_API_KEY)\n  agul chat --engine codex                  Use a connected ChatGPT account\n  agul chat --base-url <url>/v1 --model <model>\n                                            Use a local or compatible model\n  agul chat --prompt \"summarize this project\"\n                                            Run one turn and exit\n  agul chat --continue                      Continue the latest chat here"
    )]
    Chat(Box<ChatArgs>),
    /// Inspect or update model price catalogs.
    #[command(after_help = "Examples:\n  agul price status\n  agul price sync --url <catalog-url>")]
    Price(PriceArgs),
    /// List saved chats or inspect a session and its usage ledger.
    #[command(
        after_help = "Examples:\n  agul sessions list\n  agul sessions show <id>\n  agul sessions show <id> --trace"
    )]
    Sessions(SessionsArgs),
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn help(args: &[&str]) -> String {
        let argv = std::iter::once("agul.exe")
            .chain(args.iter().copied())
            .collect::<Vec<_>>();
        let error = Cli::try_parse_from(argv).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        error.to_string()
    }

    #[test]
    fn top_level_help_explains_the_default_and_quick_paths() {
        let help = help(&["--help"]);

        assert!(help.contains("Usage: agul [COMMAND]"));
        assert!(!help.contains("agul.exe"));
        assert!(help.contains("Run it without a command to open the full-screen workbench"));
        assert!(help.contains("agul chat --provider glm"));
        assert!(help.contains("agul account login"));
        assert!(help.contains("agul chat --continue"));
    }

    #[test]
    fn chat_help_groups_choices_and_shows_working_examples() {
        let help = help(&["chat", "--help"]);

        for heading in ["Workspace:", "Model:", "Sessions:", "Limits:", "Output:"] {
            assert!(help.contains(heading), "missing help heading {heading}");
        }
        assert!(help.contains("GLM Coding Plan (GLM_API_KEY)"));
        assert!(help.contains("agul chat --engine codex"));
        assert!(help.contains("agul chat --base-url <url>/v1 --model <model>"));
    }

    #[test]
    fn every_help_route_uses_the_installed_command_name() {
        let routes: &[&[&str]] = &[
            &["account", "--help"],
            &["account", "status", "--help"],
            &["account", "login", "--help"],
            &["account", "logout", "--help"],
            &["ari", "--help"],
            &["ari", "serve", "--help"],
            &["price", "--help"],
            &["price", "status", "--help"],
            &["price", "sync", "--help"],
            &["sessions", "--help"],
            &["sessions", "list", "--help"],
            &["sessions", "show", "--help"],
        ];

        for route in routes {
            let help = help(route);
            assert!(help.contains("Usage: agul "), "route {route:?}");
            assert!(!help.contains("agul.exe"), "route {route:?}");
        }
    }
}
