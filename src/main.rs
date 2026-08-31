use clap::Parser;

use agul::cli::Cli;

fn main() {
    let cli = Cli::parse();

    match agul::app::run(cli) {
        Ok(0) => {}
        Ok(exit_code) => std::process::exit(i32::from(exit_code)),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
