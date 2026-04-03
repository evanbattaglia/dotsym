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

    #[error("Failed to create backup '{backup_path}' for '{original_path}': {source}")]
    BackupCreation { original_path: PathBuf, backup_path: PathBuf, source: std::io::Error },

    #[error("Path '{path}' is not located under the home directory and cannot be managed by dotsym")]
    NotUnderHome { path: PathBuf },

    #[error("Destination '{path}' already exists in the dotfiles repo; refusing to overwrite it")]
    DestinationExists { path: PathBuf },
}
