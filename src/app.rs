use crate::cli::{Cli, Command};
use crate::commands::{account, ari, chat, price, sessions};

pub fn run(cli: Cli) -> Result<u8, Box<dyn std::error::Error>> {
    match cli.command {
        None => Ok(chat::run(&chat::ChatArgs::default()).exit_code),
        Some(Command::Account(args)) => account::run(&args),
        Some(Command::Ari(args)) => {
            ari::run(&args)?;
            Ok(0)
        }
        Some(Command::Chat(args)) => Ok(chat::run(&args).exit_code),
        Some(Command::Price(args)) => Ok(price::run(&args).exit_code),
        Some(Command::Sessions(args)) => Ok(sessions::run(&args).exit_code),
    }
}
