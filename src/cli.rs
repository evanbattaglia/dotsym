use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "dotsym")]
#[command(about = "manage symlinks to dotfiles")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Preview,
    Apply {
        #[arg(short, long)]
        dry_run: bool,
    },
    Setup {
        directory: String,
        separator: String,
    },
}