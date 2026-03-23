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
        #[arg(short = 'n', long)]
        dry_run: bool,
        #[arg(long, help = "Skip backing up existing symlinks (files and directories are still backed up)")]
        no_backup_existing_symlinks: bool,
        #[arg(help = "Optional filter path: host_dir, host_dir/literal_dir, or host_dir/literal_dir/symlink")]
        path: Option<String>,
    },
    Setup {
        directory: String,
        separator: String,
    },
}