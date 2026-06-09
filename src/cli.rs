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
    #[command(about = "Remove broken dotsym symlinks whose target no longer exists in the dotfiles repo")]
    Clean {
        #[arg(short = 'n', long, help = "Show what would be removed without deleting anything")]
        dry_run: bool,
        #[arg(short = 'y', long, help = "Don't prompt; delete all dangling symlinks found")]
        yes: bool,
    },
    #[command(about = "Move a file/dir into the dotfiles repo and symlink it back, so it can be managed by dotsym")]
    Dotsymize {
        #[arg(help = "Path of the file or directory to bring under dotsym management")]
        path: String,
        #[arg(short = 'n', long, help = "Show the candidate locations and chosen action without moving anything")]
        dry_run: bool,
        #[arg(short = 'y', long, help = "Don't prompt; use the recommended (first) candidate")]
        yes: bool,
    },
}