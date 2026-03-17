use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DotsymError {
    #[error("Config file not found at {path}. Run 'dotsym setup' to create one.")]
    ConfigNotFound { path: PathBuf },

    #[error("Invalid configuration in {path}: {source}")]
    ConfigInvalid { path: PathBuf, source: toml::de::Error },

    #[error("Home directory could not be determined")]
    HomeDirectoryNotFound,

    #[error("Dotfiles directory '{path}' does not exist")]
    DotfilesDirectoryNotFound { path: PathBuf },

    #[error("IO error while traversing directory '{path}': {source}")]
    DirectoryTraversal { path: PathBuf, source: std::io::Error },

    #[error("Failed to create parent directory '{path}': {source}")]
    ParentDirectoryCreation { path: PathBuf, source: std::io::Error },

    #[error("Failed to create symlink from '{source}' to '{destination}': {io_error}")]
    SymlinkCreation { source: PathBuf, destination: PathBuf, #[source] io_error: std::io::Error },

    #[error("Source file '{path}' does not exist")]
    SourceFileNotFound { path: PathBuf },

    #[error("Destination '{path}' already exists and is not a symlink")]
    DestinationExists { path: PathBuf },

    #[error("Destination symlink '{path}' points to '{current_target}' but should point to '{expected_target}'")]
    SymlinkMismatch { path: PathBuf, current_target: PathBuf, expected_target: PathBuf },

    #[error("Failed to create backup '{backup_path}' for '{original_path}': {source}")]
    BackupCreation { original_path: PathBuf, backup_path: PathBuf, source: std::io::Error },
}