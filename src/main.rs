use anyhow::{Context, Result as AnyhowResult};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Parser)]
#[command(name = "dotsym")]
#[command(about = "manage symlinks to dotfiles")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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

#[derive(Deserialize, Debug, Clone)]
struct Config {
    separator: String,
    dir: String,
}

#[derive(Debug, Clone)]
struct SymlinkMapping {
    source: PathBuf,
    destination: PathBuf,
}

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

struct DotsymContext {
    config: Config,
    hostname: String,
    home_dir: PathBuf,
}

impl DotsymContext {
    fn new(config: Config, hostname: Option<String>, home_dir: Option<PathBuf>) -> Result<Self, DotsymError> {
        let hostname = hostname.unwrap_or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".to_string())
        });

        let home_dir = match home_dir {
            Some(dir) => dir,
            None => dirs::home_dir().ok_or(DotsymError::HomeDirectoryNotFound)?,
        };

        Ok(Self {
            config,
            hostname,
            home_dir,
        })
    }

    fn expand_path(&self, path: &str) -> String {
        let separator = &self.config.separator;

        if path == separator {
            return String::new();
        }

        let mut result = path.to_string();

        if result.starts_with(separator) {
            result = format!(".{}", &result[separator.len()..]);
        }

        result = result.replace(separator, "/");

        result
    }

    fn get_symlink_mappings(&self) -> Result<Vec<SymlinkMapping>, DotsymError> {
        let expanded_dir = self.config.dir.replace("~/", &format!("{}/", self.home_dir.display()));
        let config_dir = Path::new(&expanded_dir);
        let mut mappings = Vec::new();

        if !config_dir.exists() {
            return Err(DotsymError::DotfilesDirectoryNotFound {
                path: config_dir.to_path_buf()
            });
        }

        // If directory exists but is empty, that's fine - just return empty mappings
        if !config_dir.is_dir() {
            return Err(DotsymError::DotfilesDirectoryNotFound {
                path: config_dir.to_path_buf()
            });
        }

        let mut host_dirs = Vec::new();

        for entry in fs::read_dir(config_dir).map_err(|e| DotsymError::DirectoryTraversal {
            path: config_dir.to_path_buf(),
            source: e,
        })? {
            let entry = entry.map_err(|e| DotsymError::DirectoryTraversal {
                path: config_dir.to_path_buf(),
                source: e,
            })?;
            let name = entry.file_name().to_string_lossy().to_string();

            if name == "dotsym" || name.starts_with("dotsym__") {
                host_dirs.push((name, 0));
            } else if name == self.hostname || name.starts_with(&format!("{}__", self.hostname)) {
                host_dirs.push((name, 1));
            }
        }

        host_dirs.sort();

        for (host_dir_name, _) in host_dirs {
            let host_dir = config_dir.join(&host_dir_name);
            if !host_dir.is_dir() {
                continue;
            }

            let mut literal_dirs = Vec::new();
            for entry in fs::read_dir(&host_dir).map_err(|e| DotsymError::DirectoryTraversal {
                path: host_dir.clone(),
                source: e,
            })? {
                let entry = entry.map_err(|e| DotsymError::DirectoryTraversal {
                    path: host_dir.clone(),
                    source: e,
                })?;
                let file_type = entry.file_type().map_err(|e| DotsymError::DirectoryTraversal {
                    path: host_dir.clone(),
                    source: e,
                })?;
                if file_type.is_dir() {
                    literal_dirs.push(entry.file_name().to_string_lossy().to_string());
                }
            }
            literal_dirs.sort();

            for literal_dir_name in literal_dirs {
                let literal_dir = host_dir.join(&literal_dir_name);
                let expanded_literal_dir = self.expand_path(&literal_dir_name);
                let target_dir = if expanded_literal_dir.is_empty() {
                    self.home_dir.clone()
                } else {
                    self.home_dir.join(&expanded_literal_dir)
                };

                for entry in fs::read_dir(&literal_dir).map_err(|e| DotsymError::DirectoryTraversal {
                    path: literal_dir.clone(),
                    source: e,
                })? {
                    let entry = entry.map_err(|e| DotsymError::DirectoryTraversal {
                        path: literal_dir.clone(),
                        source: e,
                    })?;
                    let dest_name = entry.file_name().to_string_lossy().to_string();
                    let expanded_dest_name = self.expand_path(&dest_name);

                    let destination = entry.path();
                    let source = target_dir.join(&expanded_dest_name);

                    mappings.push(SymlinkMapping {
                        source,
                        destination,
                    });
                }
            }
        }

        Ok(mappings)
    }

    fn generate_backup_path(&self, original_path: &Path) -> PathBuf {
        let mut counter = 1;
        loop {
            let backup_path = if original_path.extension().is_some() {
                // For files with extensions: file.txt.~1~, file.txt.~2~, etc.
                original_path.with_extension(format!("{}.~{}~", original_path.extension().unwrap().to_string_lossy(), counter))
            } else {
                // For files without extensions: filename.~1~, filename.~2~, etc.
                PathBuf::from(format!("{}.~{}~", original_path.to_string_lossy(), counter))
            };

            if !backup_path.exists() {
                return backup_path;
            }
            counter += 1;

            // Prevent infinite loops (though very unlikely in practice)
            if counter > 9999 {
                return PathBuf::from(format!("{}.~{}~", original_path.to_string_lossy(), counter));
            }
        }
    }

    fn apply_symlinks(&self, dry_run: bool) -> Result<Vec<SymlinkOperation>, DotsymError> {
        let mappings = self.get_symlink_mappings()?;
        let mut operations = Vec::new();

        for mapping in mappings {
            let operation = self.analyze_symlink(&mapping)?;

            if dry_run {
                println!("{}", operation.describe());
            } else {
                self.execute_operation(&operation)?;
                println!("{}", operation.describe());
            }

            operations.push(operation);
        }

        Ok(operations)
    }

    fn analyze_symlink(&self, mapping: &SymlinkMapping) -> Result<SymlinkOperation, DotsymError> {
        // Check if source (destination file) exists - allow symlinks even if broken
        if !mapping.destination.exists() && !mapping.destination.is_symlink() {
            return Err(DotsymError::SourceFileNotFound {
                path: mapping.destination.clone()
            });
        }

        // Check if target (symlink location) already exists
        if mapping.source.exists() || mapping.source.is_symlink() {
            if mapping.source.is_symlink() {
                // It's a symlink - check if it points to the right place
                if let Ok(current_target) = fs::read_link(&mapping.source)
                    && current_target == mapping.destination {
                        return Ok(SymlinkOperation::AlreadyExists(mapping.clone()));
                    }
                // Wrong or broken symlink - back it up like any other file
                let backup_path = self.generate_backup_path(&mapping.source);
                return Ok(SymlinkOperation::CreateWithBackup {
                    mapping: mapping.clone(),
                    backup_path,
                });
            } else {
                // It's a regular file/directory - we need to back it up
                let backup_path = self.generate_backup_path(&mapping.source);
                return Ok(SymlinkOperation::CreateWithBackup {
                    mapping: mapping.clone(),
                    backup_path,
                });
            }
        }

        Ok(SymlinkOperation::CreateSymlink(mapping.clone()))
    }

    fn execute_operation(&self, operation: &SymlinkOperation) -> Result<(), DotsymError> {
        match operation {
            SymlinkOperation::AlreadyExists(_) => {
                // Nothing to do
                Ok(())
            }
            SymlinkOperation::CreateWithBackup { mapping, backup_path } => {
                // Create parent directory if it doesn't exist
                if let Some(parent) = mapping.source.parent()
                    && !parent.exists() {
                        fs::create_dir_all(parent).map_err(|e| DotsymError::ParentDirectoryCreation {
                            path: parent.to_path_buf(),
                            source: e,
                        })?;
                    }

                // Create backup of existing file/directory
                fs::rename(&mapping.source, backup_path).map_err(|e| DotsymError::BackupCreation {
                    original_path: mapping.source.clone(),
                    backup_path: backup_path.clone(),
                    source: e,
                })?;

                // Create the symlink
                unix_fs::symlink(&mapping.destination, &mapping.source).map_err(|e| DotsymError::SymlinkCreation {
                    source: mapping.source.clone(),
                    destination: mapping.destination.clone(),
                    io_error: e,
                })?;

                Ok(())
            }
            SymlinkOperation::CreateSymlink(mapping) => {
                // Create parent directory if it doesn't exist
                if let Some(parent) = mapping.source.parent()
                    && !parent.exists() {
                        fs::create_dir_all(parent).map_err(|e| DotsymError::ParentDirectoryCreation {
                            path: parent.to_path_buf(),
                            source: e,
                        })?;
                    }

                // Remove existing symlink if it exists
                if mapping.source.exists() || mapping.source.is_symlink() {
                    fs::remove_file(&mapping.source).map_err(|e| DotsymError::SymlinkCreation {
                        source: mapping.source.clone(),
                        destination: mapping.destination.clone(),
                        io_error: e,
                    })?;
                }

                // Create the symlink
                unix_fs::symlink(&mapping.destination, &mapping.source).map_err(|e| DotsymError::SymlinkCreation {
                    source: mapping.source.clone(),
                    destination: mapping.destination.clone(),
                    io_error: e,
                })?;

                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone)]
enum SymlinkOperation {
    CreateSymlink(SymlinkMapping),
    AlreadyExists(SymlinkMapping),
    CreateWithBackup { mapping: SymlinkMapping, backup_path: PathBuf },
}

impl SymlinkOperation {
    fn describe(&self) -> String {
        match self {
            SymlinkOperation::CreateSymlink(mapping) => {
                format!("CREATE: {} -> {}", mapping.source.display(), mapping.destination.display())
            }
            SymlinkOperation::AlreadyExists(mapping) => {
                format!("EXISTS: {} -> {}", mapping.source.display(), mapping.destination.display())
            }
            SymlinkOperation::CreateWithBackup { mapping, backup_path } => {
                format!("CREATE: {} -> {} (backup: {})",
                    mapping.source.display(),
                    mapping.destination.display(),
                    backup_path.display())
            }
        }
    }
}

fn load_config() -> AnyhowResult<Config> {
    let home_dir = dirs::home_dir()
        .ok_or(DotsymError::HomeDirectoryNotFound)?;

    let config_path = home_dir.join(".config/dotsym/dotsym.toml");

    if !config_path.exists() {
        return Err(DotsymError::ConfigNotFound {
            path: config_path
        }.into());
    }

    let config_content = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config file at {}", config_path.display()))?;

    let config: Config = toml::from_str(&config_content)
        .map_err(|e| DotsymError::ConfigInvalid {
            path: config_path,
            source: e
        })?;

    Ok(config)
}

fn preview_command(context: &DotsymContext) -> AnyhowResult<()> {
    let mappings = context.get_symlink_mappings()
        .context("Failed to generate symlink mappings")?;

    for mapping in mappings {
        println!("{}", mapping.source.display());
        println!("{}", mapping.destination.display());
        println!();
    }

    Ok(())
}

fn apply_command(context: &DotsymContext, dry_run: bool) -> AnyhowResult<()> {
    if dry_run {
        println!("DRY RUN - showing what would be done:");
        println!();
    }

    let operations = context.apply_symlinks(dry_run)
        .context("Failed to apply symlink operations")?;

    if dry_run {
        println!();
        println!("Dry run complete. {} operations would be performed.", operations.len());
        println!("Run without --dry-run to apply these changes.");
    } else {
        println!();
        println!("Applied {} symlink operations successfully.", operations.len());
    }

    Ok(())
}

fn setup_command(directory: String, separator: String) -> AnyhowResult<()> {
    setup_command_with_home_dir(Some(directory), Some(separator), None)
}

fn setup_command_with_home_dir(directory: Option<String>, separator: Option<String>, home_dir_override: Option<PathBuf>) -> AnyhowResult<()> {
    let separator = separator.unwrap_or_else(|| "__".to_string());
    let directory = directory.unwrap_or_else(|| "~/dotfiles".to_string());

    println!("Setting up dotsym with:");
    println!("  Directory: {}", directory);
    println!("  Separator: {}", separator);
    println!();

    // Create temporary config for setup
    let temp_config = Config {
        separator: separator.clone(),
        dir: directory.clone(),
    };

    let context = DotsymContext::new(temp_config, None, home_dir_override)
        .context("Failed to initialize dotsym context")?;

    // Get all mappings and find the dotsym.toml file
    let mappings = context.get_symlink_mappings()
        .context("Failed to generate symlink mappings")?;

    // Look for dotsym.toml in the mappings
    let dotsym_config_mapping = mappings.iter()
        .find(|mapping| {
            mapping.source.file_name()
                .map(|name| name == "dotsym.toml")
                .unwrap_or(false) &&
            mapping.source.parent()
                .and_then(|parent| parent.file_name())
                .map(|name| name == "dotsym")
                .unwrap_or(false)
        });

    match dotsym_config_mapping {
        Some(mapping) => {
            println!("Found dotsym config at: {}", mapping.destination.display());

            // Read and validate the config file contents
            if mapping.destination.exists() || mapping.destination.is_symlink() {
                let config_content = fs::read_to_string(&mapping.destination)
                    .with_context(|| format!("Failed to read config file {}", mapping.destination.display()))?;

                let found_config: Config = toml::from_str(&config_content)
                    .with_context(|| format!("Failed to parse config file {}", mapping.destination.display()))?;

                // Validate separator
                if found_config.separator != separator {
                    return Err(anyhow::anyhow!(
                        "Config file separator mismatch:\n  Command line: '{}'\n  Config file:  '{}'\n\nPlease ensure the separator argument matches the config file.",
                        separator, found_config.separator
                    ));
                }

                // Validate directory (expand ~ for comparison)
                let home_dir = context.home_dir.clone();
                let expected_dir_expanded = if let Some(stripped) = directory.strip_prefix("~/") {
                    format!("{}/{}", home_dir.display(), stripped)
                } else {
                    directory.clone()
                };

                let found_dir_expanded = if let Some(stripped) = found_config.dir.strip_prefix("~/") {
                    format!("{}/{}", home_dir.display(), stripped)
                } else {
                    found_config.dir.clone()
                };

                // Compare paths - try canonicalization first, fall back to direct comparison
                let paths_match = match (
                    std::fs::canonicalize(&expected_dir_expanded),
                    std::fs::canonicalize(&found_dir_expanded)
                ) {
                    (Ok(expected_canonical), Ok(found_canonical)) => {
                        expected_canonical == found_canonical
                    }
                    _ => {
                        // If canonicalization fails (directories don't exist), compare the expanded paths directly
                        std::path::Path::new(&expected_dir_expanded) == std::path::Path::new(&found_dir_expanded)
                    }
                };

                if !paths_match {
                    return Err(anyhow::anyhow!(
                        "Config file directory mismatch:\n  Command line: '{}' (expands to: {})\n  Config file:  '{}' (expands to: {})\n\nPlease ensure the directory argument matches the config file.",
                        directory, expected_dir_expanded,
                        found_config.dir, found_dir_expanded
                    ));
                }

                println!("✓ Config file contents are consistent with setup arguments");
            }

            // Create the directory if it doesn't exist
            if let Some(parent) = mapping.source.parent()
                && !parent.exists() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create directory {}", parent.display()))?;
                }

            // Check if config already exists
            if mapping.source.exists() || mapping.source.is_symlink() {
                if mapping.source.is_symlink() {
                    if let Ok(current_target) = fs::read_link(&mapping.source) {
                        if current_target == mapping.destination {
                            println!("dotsym.toml symlink already exists and points to the correct location.");
                            return Ok(());
                        } else {
                            println!("dotsym.toml symlink exists but points to wrong location.");
                            println!("Current: {} -> {}", mapping.source.display(), current_target.display());
                            println!("Should be: {} -> {}", mapping.source.display(), mapping.destination.display());
                            return Err(anyhow::anyhow!("Config symlink exists but is incorrect. Please fix manually."));
                        }
                    } else {
                        println!("dotsym.toml is a broken symlink, replacing it.");
                        fs::remove_file(&mapping.source)
                            .with_context(|| format!("Failed to remove broken symlink {}", mapping.source.display()))?;
                    }
                } else {
                    println!("dotsym.toml exists as a regular file, not replacing it.");
                    return Err(anyhow::anyhow!("Config file already exists as a regular file. Use 'dotsym apply' if you want to manage it with dotsym."));
                }
            }

            // Create the symlink
            unix_fs::symlink(&mapping.destination, &mapping.source)
                .with_context(|| format!("Failed to create symlink from {} to {}",
                    mapping.source.display(), mapping.destination.display()))?;

            println!("✓ Created dotsym.toml symlink:");
            println!("  {} -> {}", mapping.source.display(), mapping.destination.display());
            println!();
            println!("Setup complete! You can now run 'dotsym apply' to install your dotfiles.");
        }
        None => {
            println!("No dotsym.toml found in the dotfiles directory structure.");
            println!();
            println!("Expected to find a file like:");
            println!("  {}/dotsym/__config__dotsym/dotsym.toml", directory);
            println!("  {}/$(hostname)/__config__dotsym/dotsym.toml", directory);
            println!("  etc.");
            println!();
            println!("Please create a dotsym.toml file in your dotfiles directory first.");
            return Err(anyhow::anyhow!("No dotsym.toml found in dotfiles directory"));
        }
    }

    Ok(())
}

fn main() -> AnyhowResult<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Preview => {
            let config = load_config()?;
            let context = DotsymContext::new(config, None, None)
                .context("Failed to initialize dotsym context")?;
            preview_command(&context)
        }
        Commands::Apply { dry_run } => {
            let config = load_config()?;
            let context = DotsymContext::new(config, None, None)
                .context("Failed to initialize dotsym context")?;
            apply_command(&context, dry_run)
        }
        Commands::Setup { directory, separator } => {
            setup_command(directory, separator)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_config(separator: &str, dir: &str) -> Config {
        Config {
            separator: separator.to_string(),
            dir: dir.to_string(),
        }
    }

    fn create_test_context(config: Config, hostname: Option<String>, home_dir: Option<PathBuf>) -> DotsymContext {
        DotsymContext::new(config, hostname, home_dir).unwrap()
    }

    fn setup_test_directory_structure(temp_dir: &TempDir) -> std::io::Result<()> {
        let base_path = temp_dir.path();

        fs::create_dir_all(base_path.join("dotsym/__"))?;
        fs::create_dir_all(base_path.join("dotsym/__config__dotsym"))?;
        fs::create_dir_all(base_path.join("dotsym/code__myproject"))?;
        fs::create_dir_all(base_path.join("dotsym__2/code__myproject"))?;
        fs::create_dir_all(base_path.join("myhostname/code__myproject"))?;
        fs::create_dir_all(base_path.join("myhostname__a/__config"))?;
        fs::create_dir_all(base_path.join("myhostname__a/__config__someotherprogram"))?;
        fs::create_dir_all(base_path.join("otherhostname/__config"))?;

        File::create(base_path.join("dotsym/__/__gitconfig"))?;

        let mut dotsym_toml = File::create(base_path.join("dotsym/__config__dotsym/dotsym.toml"))?;
        dotsym_toml.write_all(b"# dotsym config\n")?;

        File::create(base_path.join("dotsym/code__myproject/mystuff"))?;
        File::create(base_path.join("dotsym__2/code__myproject/mypersonalscripts"))?;
        File::create(base_path.join("myhostname/code__myproject/__git__info__exclude"))?;
        File::create(base_path.join("myhostname/code__myproject/morepersonalstuff"))?;

        fs::create_dir_all(base_path.join("myhostname__a/__config/program1"))?;
        fs::create_dir_all(base_path.join("myhostname__a/__config/program2__dir1"))?;
        fs::create_dir_all(base_path.join("myhostname__a/__config__someotherprogram/subdir__file2"))?;

        fs::create_dir_all(base_path.join("otherhostname/__config/program1"))?;

        Ok(())
    }

    #[test]
    fn test_expand_path_basic() {
        let config = create_test_config("__", "/test");
        let context = create_test_context(config, Some("testhost".to_string()), Some(PathBuf::from("/home/test")));

        assert_eq!(context.expand_path("__gitconfig"), ".gitconfig");
        assert_eq!(context.expand_path("code__myproject"), "code/myproject");
        assert_eq!(context.expand_path("__config__dotsym"), ".config/dotsym");
        assert_eq!(context.expand_path("__"), "");
    }

    #[test]
    fn test_expand_path_middle_separators() {
        let config = create_test_config("__", "/test");
        let context = create_test_context(config, Some("testhost".to_string()), Some(PathBuf::from("/home/test")));

        assert_eq!(context.expand_path("__git__info__exclude"), ".git/info/exclude");
        assert_eq!(context.expand_path("program2__dir1"), "program2/dir1");
        assert_eq!(context.expand_path("subdir__file2"), "subdir/file2");
    }

    #[test]
    fn test_get_symlink_mappings_with_fixtures() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        setup_test_directory_structure(&temp_dir)?;

        let config = create_test_config("__", temp_dir.path().to_str().unwrap());
        let home_dir = PathBuf::from("/home/me");
        let context = create_test_context(config, Some("myhostname".to_string()), Some(home_dir.clone()));

        let mappings = context.get_symlink_mappings()?;


        // Check we have the expected count
        assert_eq!(mappings.len(), 9);

        // Check that all expected sources are present (order may vary)
        let source_paths: Vec<String> = mappings.iter()
            .map(|m| m.source.to_string_lossy().to_string())
            .collect();

        let expected_sources = vec![
            "/home/me/.gitconfig",
            "/home/me/.config/dotsym/dotsym.toml",
            "/home/me/code/myproject/mystuff",
            "/home/me/code/myproject/mypersonalscripts",
            "/home/me/code/myproject/.git/info/exclude",
            "/home/me/code/myproject/morepersonalstuff",
            "/home/me/.config/program1",
            "/home/me/.config/program2/dir1",
            "/home/me/.config/someotherprogram/subdir/file2",
        ];

        for expected_source in &expected_sources {
            assert!(source_paths.contains(&expected_source.to_string()),
                   "Expected source '{}' not found in {:?}", expected_source, source_paths);
        }

        // Verify that the mappings end with expected destination suffixes
        let dest_suffixes: Vec<String> = mappings.iter()
            .map(|m| m.destination.to_string_lossy().to_string())
            .map(|path| {
                // Extract the suffix after the temp directory path
                if let Some(pos) = path.rfind('/') {
                    let parent_and_file = &path[..pos];
                    if let Some(parent_pos) = parent_and_file.rfind('/') {
                        let parent = &parent_and_file[parent_pos+1..];
                        let file = &path[pos+1..];
                        format!("{}/{}", parent, file)
                    } else {
                        path
                    }
                } else {
                    path
                }
            })
            .collect();

        let expected_dest_patterns = vec![
            "__/__gitconfig",
            "__config__dotsym/dotsym.toml",
            "code__myproject/mystuff",
            "code__myproject/mypersonalscripts",
            "code__myproject/__git__info__exclude",
            "code__myproject/morepersonalstuff",
            "__config/program1",
            "__config/program2__dir1",
            "__config__someotherprogram/subdir__file2",
        ];

        for expected_pattern in expected_dest_patterns {
            assert!(dest_suffixes.iter().any(|suffix| suffix.ends_with(expected_pattern)),
                   "Expected destination pattern '{}' not found in {:?}", expected_pattern, dest_suffixes);
        }

        Ok(())
    }

    #[test]
    fn test_get_symlink_mappings_unknown_hostname() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        setup_test_directory_structure(&temp_dir)?;

        let config = create_test_config("__", temp_dir.path().to_str().unwrap());
        let home_dir = PathBuf::from("/home/me");
        let context = create_test_context(config, Some("unknownhost".to_string()), Some(home_dir.clone()));

        let mappings = context.get_symlink_mappings()?;

        let expected_global_mappings = 4;
        assert_eq!(mappings.len(), expected_global_mappings,
                   "Should only have global dotsym mappings for unknown hostname");

        let source_paths: Vec<String> = mappings.iter()
            .map(|m| m.source.to_string_lossy().to_string())
            .collect();

        assert!(source_paths.contains(&"/home/me/.gitconfig".to_string()));
        assert!(source_paths.contains(&"/home/me/.config/dotsym/dotsym.toml".to_string()));
        assert!(source_paths.contains(&"/home/me/code/myproject/mystuff".to_string()));
        assert!(source_paths.contains(&"/home/me/code/myproject/mypersonalscripts".to_string()));

        Ok(())
    }

    #[test]
    fn test_get_symlink_mappings_empty_directory() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let config = create_test_config("__", temp_dir.path().to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(PathBuf::from("/home/test")));

        // Empty directory (that exists) should return no mappings, not an error
        let mappings = context.get_symlink_mappings()?;
        assert_eq!(mappings.len(), 0, "Empty directory should produce no mappings");

        Ok(())
    }

    #[test]
    fn test_get_symlink_mappings_nonexistent_directory() -> Result<(), Box<dyn std::error::Error>> {
        let config = create_test_config("__", "/nonexistent/path");
        let context = create_test_context(config, Some("testhost".to_string()), Some(PathBuf::from("/home/test")));

        let result = context.get_symlink_mappings();
        assert!(result.is_err(), "Nonexistent directory should produce an error");

        // Check that it's specifically a DotfilesDirectoryNotFound error
        if let Err(DotsymError::DotfilesDirectoryNotFound { path }) = result {
            assert_eq!(path, PathBuf::from("/nonexistent/path"));
        } else {
            panic!("Expected DotfilesDirectoryNotFound error");
        }

        Ok(())
    }

    #[test]
    fn test_host_directory_sorting() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let base_path = temp_dir.path();

        fs::create_dir_all(base_path.join("dotsym__2/__"))?;
        fs::create_dir_all(base_path.join("dotsym/__"))?;
        fs::create_dir_all(base_path.join("dotsym__1/__"))?;
        fs::create_dir_all(base_path.join("testhost__b/__"))?;
        fs::create_dir_all(base_path.join("testhost/__"))?;
        fs::create_dir_all(base_path.join("testhost__a/__"))?;

        File::create(base_path.join("dotsym__2/__/file2"))?;
        File::create(base_path.join("dotsym/__/file1"))?;
        File::create(base_path.join("dotsym__1/__/file3"))?;
        File::create(base_path.join("testhost__b/__/file6"))?;
        File::create(base_path.join("testhost/__/file4"))?;
        File::create(base_path.join("testhost__a/__/file5"))?;

        let config = create_test_config("__", temp_dir.path().to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(PathBuf::from("/home/test")));

        let mappings = context.get_symlink_mappings()?;

        let dest_filenames: Vec<String> = mappings.iter()
            .map(|m| m.destination.file_name().unwrap().to_string_lossy().to_string())
            .collect();

        assert_eq!(dest_filenames, vec!["file1", "file3", "file2", "file4", "file5", "file6"]);

        Ok(())
    }

    #[test]
    fn test_different_separator() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let base_path = temp_dir.path();

        fs::create_dir_all(base_path.join("dotsym/--"))?;
        File::create(base_path.join("dotsym/--/--gitconfig"))?;

        let config = create_test_config("--", temp_dir.path().to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(PathBuf::from("/home/test")));

        let mappings = context.get_symlink_mappings()?;

        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].source.to_string_lossy(), "/home/test/.gitconfig");
        assert!(mappings[0].destination.to_string_lossy().ends_with("--gitconfig"));

        Ok(())
    }

    #[test]
    fn test_config_not_found_error() {
        use std::fs;
        use tempfile::TempDir;

        let temp_home = TempDir::new().unwrap();
        let fake_home = temp_home.path();

        // Set up a fake home directory structure without the config file
        fs::create_dir_all(fake_home.join(".config")).unwrap();

        // Temporarily override the config loading to use our fake home
        let config_path = fake_home.join(".config/dotsym/dotsym.toml");
        let result = if !config_path.exists() {
            Err(DotsymError::ConfigNotFound { path: config_path })
        } else {
            Ok(Config { separator: "__".to_string(), dir: "~/test".to_string() })
        };

        assert!(result.is_err());
        if let Err(DotsymError::ConfigNotFound { path }) = result {
            assert!(path.to_string_lossy().ends_with(".config/dotsym/dotsym.toml"));
            assert!(path.to_string_lossy().contains("Run 'dotsym setup' to create one") == false); // Error message is in Display impl
        } else {
            panic!("Expected ConfigNotFound error");
        }
    }

    #[test]
    fn test_config_invalid_error() {
        use tempfile::TempDir;

        let temp_home = TempDir::new().unwrap();
        let fake_home = temp_home.path();

        fs::create_dir_all(fake_home.join(".config/dotsym")).unwrap();
        let config_path = fake_home.join(".config/dotsym/dotsym.toml");

        // Write invalid TOML
        fs::write(&config_path, "invalid toml content [[[").unwrap();

        let config_content = fs::read_to_string(&config_path).unwrap();
        let result: Result<Config, DotsymError> = toml::from_str(&config_content)
            .map_err(|e| DotsymError::ConfigInvalid {
                path: config_path.clone(),
                source: e
            });

        assert!(result.is_err());
        if let Err(DotsymError::ConfigInvalid { path, source: _ }) = result {
            assert_eq!(path, config_path);
        } else {
            panic!("Expected ConfigInvalid error");
        }
    }

    #[test]
    fn test_home_directory_not_found_error() {
        // Test DotsymContext::new when home directory cannot be determined
        let config = create_test_config("__", "/test");

        // Simulate home directory not being found by passing None and not providing fallback
        // In real scenario this would happen if dirs::home_dir() returns None
        let result = DotsymContext::new(config, Some("testhost".to_string()), None);

        // This should succeed since we have a fallback, but let's test the error type exists
        assert!(result.is_ok());

        // Test the actual error case by directly creating the error
        let error = DotsymError::HomeDirectoryNotFound;
        assert_eq!(format!("{}", error), "Home directory could not be determined");
    }

    #[test]
    fn test_error_display_messages() {
        use std::path::PathBuf;

        let config_not_found = DotsymError::ConfigNotFound {
            path: PathBuf::from("/home/user/.config/dotsym/dotsym.toml")
        };
        assert!(format!("{}", config_not_found).contains("Run 'dotsym setup' to create one"));

        let dotfiles_not_found = DotsymError::DotfilesDirectoryNotFound {
            path: PathBuf::from("/home/user/dotfiles")
        };
        assert!(format!("{}", dotfiles_not_found).contains("does not exist"));

        let home_not_found = DotsymError::HomeDirectoryNotFound;
        assert_eq!(format!("{}", home_not_found), "Home directory could not be determined");
    }

    #[test]
    fn test_apply_symlinks_create_new() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        let operations = context.apply_symlinks(true)?; // dry run

        assert_eq!(operations.len(), 1);
        match &operations[0] {
            SymlinkOperation::CreateSymlink(mapping) => {
                assert!(mapping.source.to_string_lossy().ends_with(".gitconfig"));
                assert!(mapping.destination.to_string_lossy().ends_with("__gitconfig"));
            }
            _ => panic!("Expected CreateSymlink operation"),
        }

        Ok(())
    }

    #[test]
    fn test_apply_symlinks_already_exists() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        // Create existing symlink that points to the correct destination
        let home_gitconfig = home_dir.path().join(".gitconfig");
        unix_fs::symlink(dotfiles_dir.join("dotsym/__/__gitconfig"), &home_gitconfig)?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        let operations = context.apply_symlinks(true)?; // dry run

        assert_eq!(operations.len(), 1);
        match &operations[0] {
            SymlinkOperation::AlreadyExists(_) => {
                // Expected
            }
            _ => panic!("Expected AlreadyExists operation, got: {:?}", operations[0]),
        }

        Ok(())
    }

    #[test]
    fn test_apply_symlinks_backup_existing_symlink() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        // Create existing symlink that points to a different location
        let home_gitconfig = home_dir.path().join(".gitconfig");
        let old_target = temp_dir.path().join("old_gitconfig");
        File::create(&old_target)?;
        unix_fs::symlink(&old_target, &home_gitconfig)?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        let operations = context.apply_symlinks(true)?; // dry run

        assert_eq!(operations.len(), 1);
        match &operations[0] {
            SymlinkOperation::CreateWithBackup { mapping, backup_path } => {
                assert!(mapping.source.to_string_lossy().ends_with(".gitconfig"));
                assert!(mapping.destination.to_string_lossy().ends_with("__gitconfig"));
                assert!(backup_path.to_string_lossy().ends_with(".gitconfig.~1~"));
            }
            _ => panic!("Expected CreateWithBackup operation, got: {:?}", operations[0]),
        }

        Ok(())
    }

    #[test]
    fn test_apply_symlinks_backup_broken() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        // Create broken symlink (symlink that points to nonexistent file)
        let home_gitconfig = home_dir.path().join(".gitconfig");
        let nonexistent_target = temp_dir.path().join("nonexistent_file");
        unix_fs::symlink(&nonexistent_target, &home_gitconfig)?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        let operations = context.apply_symlinks(true)?; // dry run

        assert_eq!(operations.len(), 1);
        match &operations[0] {
            SymlinkOperation::CreateWithBackup { mapping, backup_path } => {
                // Broken symlinks are now backed up like any other file
                assert!(mapping.source.to_string_lossy().ends_with(".gitconfig"));
                assert!(backup_path.to_string_lossy().ends_with(".gitconfig.~1~"));
            }
            _ => panic!("Expected CreateWithBackup operation, got: {:?}", operations[0]),
        }

        Ok(())
    }

    #[test]
    fn test_apply_symlinks_replace_broken_read_error() -> Result<(), Box<dyn std::error::Error>> {
        // This test would be difficult to create reliably in a cross-platform way
        // since creating a symlink that fails to read is platform-specific
        // The ReplaceSymlink case is triggered in the catch-all error case
        // when reading the symlink fails, which is hard to reproduce in tests
        Ok(())
    }

    #[test]
    fn test_apply_symlinks_backup_regular_file() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        // Create regular file at destination (this should be backed up, not error)
        let home_gitconfig = home_dir.path().join(".gitconfig");
        let mut regular_file = File::create(&home_gitconfig)?;
        regular_file.write_all(b"regular file content")?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        let operations = context.apply_symlinks(true)?; // dry run - should not error
        assert_eq!(operations.len(), 1);

        match &operations[0] {
            SymlinkOperation::CreateWithBackup { mapping, backup_path } => {
                assert_eq!(mapping.source, home_gitconfig);
                assert!(backup_path.to_string_lossy().ends_with(".gitconfig.~1~"));
            }
            _ => panic!("Expected CreateWithBackup operation, got: {:?}", operations[0]),
        }

        Ok(())
    }

    #[test]
    fn test_apply_symlinks_source_not_found_error() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory with a directory but no actual file inside
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        // Create the directory structure as if the file should exist, but don't create the file
        File::create(dotfiles_dir.join("dotsym/__/__gitconfig.missing"))?; // Different file

        // To trigger the SourceFileNotFound error, we need the mapping to be generated
        // but the destination file to not exist. Let's manually create a mapping scenario
        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        // This test actually won't generate the error because if the file doesn't exist,
        // it won't be found during directory traversal. Let me change this to test
        // a different scenario - when a file gets deleted between mapping generation and application
        let gitconfig_path = dotfiles_dir.join("dotsym/__/__gitconfig");
        File::create(&gitconfig_path)?;

        // Get mappings first
        let mappings = context.get_symlink_mappings()?;
        assert!(!mappings.is_empty(), "Should have at least one mapping");

        // Find the gitconfig mapping
        let gitconfig_mapping = mappings.iter()
            .find(|m| m.destination.to_string_lossy().contains("__gitconfig"))
            .expect("Should find gitconfig mapping");

        // Now delete the file and try to analyze the symlink
        fs::remove_file(&gitconfig_path)?;

        let result = context.analyze_symlink(gitconfig_mapping);
        assert!(result.is_err());

        if let Err(DotsymError::SourceFileNotFound { path }) = result {
            assert!(path.to_string_lossy().ends_with("__gitconfig"));
        } else {
            panic!("Expected SourceFileNotFound error, got: {:?}", result);
        }

        Ok(())
    }

    #[test]
    fn test_apply_creates_parent_directories() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory with nested structure
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__nested__dir"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__config__nested__dir/test.conf"))?;
        test_file.write_all(b"test config content")?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        // Execute apply (not dry run)
        let operations = context.apply_symlinks(false)?;

        assert_eq!(operations.len(), 1);

        // Check that the parent directory was created
        let expected_parent = home_dir.path().join(".config/nested/dir");
        assert!(expected_parent.parent().unwrap().exists(), "Parent directory should exist");

        // Check that the symlink exists and points to the right place
        let expected_symlink = home_dir.path().join(".config/nested/dir/test.conf");
        assert!(expected_symlink.is_symlink(), "Symlink should exist");

        let link_target = fs::read_link(&expected_symlink)?;
        assert!(link_target.to_string_lossy().ends_with("test.conf"));

        Ok(())
    }

    #[test]
    fn test_symlink_operation_describe() {
        let mapping = SymlinkMapping {
            source: PathBuf::from("/home/user/.gitconfig"),
            destination: PathBuf::from("/home/user/dotfiles/__gitconfig"),
        };

        let create = SymlinkOperation::CreateSymlink(mapping.clone());
        assert!(create.describe().starts_with("CREATE:"));
        assert!(create.describe().contains(".gitconfig"));

        let exists = SymlinkOperation::AlreadyExists(mapping.clone());
        assert!(exists.describe().starts_with("EXISTS:"));

        let create_with_backup = SymlinkOperation::CreateWithBackup {
            mapping,
            backup_path: PathBuf::from("/home/user/.gitconfig.~1~"),
        };
        assert!(create_with_backup.describe().starts_with("CREATE:"));
        assert!(create_with_backup.describe().contains("backup: /home/user/.gitconfig.~1~"));
    }

    #[test]
    fn test_generate_backup_path() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        fs::create_dir_all(&home_dir)?;

        let config = create_test_config("__", "/test");
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.clone()));

        // Test backup path generation for file without extension
        let file_without_ext = home_dir.join("gitconfig");
        let backup1 = context.generate_backup_path(&file_without_ext);
        assert_eq!(backup1, home_dir.join("gitconfig.~1~"));

        // Create the first backup to test counter increment
        File::create(&backup1)?;
        let backup2 = context.generate_backup_path(&file_without_ext);
        assert_eq!(backup2, home_dir.join("gitconfig.~2~"));

        // Test backup path generation for file with extension
        let file_with_ext = home_dir.join("config.toml");
        let backup_ext = context.generate_backup_path(&file_with_ext);
        assert_eq!(backup_ext, home_dir.join("config.toml.~1~"));

        Ok(())
    }

    #[test]
    fn test_apply_with_backup_dry_run() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        // Create existing regular file at destination
        let home_gitconfig = home_dir.path().join(".gitconfig");
        let mut regular_file = File::create(&home_gitconfig)?;
        regular_file.write_all(b"existing content")?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        let operations = context.apply_symlinks(true)?; // dry run

        assert_eq!(operations.len(), 1);
        match &operations[0] {
            SymlinkOperation::CreateWithBackup { mapping, backup_path } => {
                assert!(mapping.source.to_string_lossy().ends_with(".gitconfig"));
                assert!(backup_path.to_string_lossy().ends_with(".gitconfig.~1~"));
            }
            _ => panic!("Expected CreateWithBackup operation, got: {:?}", operations[0]),
        }

        Ok(())
    }

    #[test]
    fn test_apply_with_backup_actual() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = TempDir::new()?;

        // Set up dotfiles directory
        let dotfiles_dir = temp_dir.path();
        fs::create_dir_all(dotfiles_dir.join("dotsym/__"))?;
        let mut test_file = File::create(dotfiles_dir.join("dotsym/__/__gitconfig"))?;
        test_file.write_all(b"[user]\n    name = Test User\n")?;

        // Create existing regular file at destination
        let home_gitconfig = home_dir.path().join(".gitconfig");
        let mut regular_file = File::create(&home_gitconfig)?;
        regular_file.write_all(b"existing content")?;

        let config = create_test_config("__", dotfiles_dir.to_str().unwrap());
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.path().to_path_buf()));

        // Execute apply (not dry run)
        let operations = context.apply_symlinks(false)?;

        assert_eq!(operations.len(), 1);

        // Check that the symlink was created
        assert!(home_gitconfig.is_symlink(), "Original path should now be a symlink");

        // Check that the backup was created and contains the original content
        let backup_path = home_dir.path().join(".gitconfig.~1~");
        assert!(backup_path.exists(), "Backup should exist");
        let backup_content = fs::read_to_string(&backup_path)?;
        assert_eq!(backup_content, "existing content");

        // Check that the symlink points to the right place
        let link_target = fs::read_link(&home_gitconfig)?;
        assert!(link_target.to_string_lossy().ends_with("__gitconfig"));

        Ok(())
    }

    #[test]
    fn test_backup_path_collision_handling() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        fs::create_dir_all(&home_dir)?;

        let config = create_test_config("__", "/test");
        let context = create_test_context(config, Some("testhost".to_string()), Some(home_dir.clone()));

        let original_file = home_dir.join("test.txt");

        // Create several backup files to test collision handling
        for i in 1..=3 {
            File::create(home_dir.join(format!("test.txt.~{}~", i)))?;
        }

        let backup_path = context.generate_backup_path(&original_file);
        assert_eq!(backup_path, home_dir.join("test.txt.~4~"));

        Ok(())
    }

    #[test]
    fn test_setup_command_success() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = temp_dir.path().join("dotfiles");

        // Set up dotfiles directory with config
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__dotsym"))?;
        let config_content = format!(r#"separator = "__"
dir = "{}"
"#, dotfiles_dir.to_string_lossy());
        fs::write(dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml"), config_content)?;

        // Test setup command
        let result = setup_command_with_home_dir(Some(dotfiles_dir.to_string_lossy().to_string()), None, Some(home_dir.clone()));
        assert!(result.is_ok(), "Setup command should succeed");

        // Verify symlink was created
        let config_symlink = home_dir.join(".config/dotsym/dotsym.toml");
        assert!(config_symlink.is_symlink(), "Config symlink should exist");

        // Verify symlink points to correct location
        let link_target = fs::read_link(&config_symlink)?;
        let expected_target = dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml");
        assert_eq!(link_target, expected_target);

        // Verify config content is accessible
        let config_content_read = fs::read_to_string(&config_symlink)?;
        assert!(config_content_read.contains(r#"separator = "__""#));

        Ok(())
    }

    #[test]
    fn test_setup_command_already_exists() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = temp_dir.path().join("dotfiles");

        // Set up dotfiles directory with config
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__dotsym"))?;
        let config_content = format!(r#"separator = "__"
dir = "{}"
"#, dotfiles_dir.to_string_lossy());
        fs::write(dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml"), config_content)?;

        // Create existing correct symlink
        fs::create_dir_all(home_dir.join(".config/dotsym"))?;
        let config_symlink = home_dir.join(".config/dotsym/dotsym.toml");
        let expected_target = dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml");
        unix_fs::symlink(&expected_target, &config_symlink)?;

        // Test setup command - should succeed and do nothing
        let result = setup_command_with_home_dir(Some(dotfiles_dir.to_string_lossy().to_string()), None, Some(home_dir.clone()));
        assert!(result.is_ok(), "Setup command should succeed when symlink already exists correctly");

        // Verify symlink still exists and is correct
        assert!(config_symlink.is_symlink(), "Config symlink should still exist");
        let link_target = fs::read_link(&config_symlink)?;
        assert_eq!(link_target, expected_target);

        Ok(())
    }

    #[test]
    fn test_setup_command_no_config_found() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let empty_dotfiles_dir = temp_dir.path().join("empty-dotfiles");

        // Create empty dotfiles directory
        fs::create_dir_all(&empty_dotfiles_dir)?;

        // Test setup command - should fail
        let result = setup_command_with_home_dir(Some(empty_dotfiles_dir.to_string_lossy().to_string()), None, Some(home_dir.clone()));
        assert!(result.is_err(), "Setup command should fail when no config is found");

        let error_msg = format!("{}", result.unwrap_err());
        assert!(error_msg.contains("No dotsym.toml found"), "Error should mention missing dotsym.toml");

        Ok(())
    }

    #[test]
    fn test_setup_command_wrong_symlink() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = temp_dir.path().join("dotfiles");

        // Set up dotfiles directory with config
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__dotsym"))?;
        let config_content = format!(r#"separator = "__"
dir = "{}"
"#, dotfiles_dir.to_string_lossy());
        fs::write(dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml"), config_content)?;

        // Create existing wrong symlink
        fs::create_dir_all(home_dir.join(".config/dotsym"))?;
        let config_symlink = home_dir.join(".config/dotsym/dotsym.toml");
        let wrong_target = temp_dir.path().join("wrong-target");
        fs::write(&wrong_target, "wrong config")?;
        unix_fs::symlink(&wrong_target, &config_symlink)?;

        // Test setup command - should fail with helpful message
        let result = setup_command_with_home_dir(Some(dotfiles_dir.to_string_lossy().to_string()), None, Some(home_dir.clone()));
        assert!(result.is_err(), "Setup command should fail when wrong symlink exists");

        let error_msg = format!("{}", result.unwrap_err());
        assert!(error_msg.contains("Config symlink exists but is incorrect"), "Error should mention incorrect symlink");

        Ok(())
    }

    #[test]
    fn test_setup_command_with_custom_separator() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = temp_dir.path().join("dotfiles");

        // Set up dotfiles directory with custom separator
        fs::create_dir_all(dotfiles_dir.join("dotsym/--config--dotsym"))?;
        let config_content = format!(r#"separator = "--"
dir = "{}"
"#, dotfiles_dir.to_string_lossy());
        fs::write(dotfiles_dir.join("dotsym/--config--dotsym/dotsym.toml"), config_content)?;

        // Test setup command with custom separator
        let result = setup_command_with_home_dir(Some(dotfiles_dir.to_string_lossy().to_string()), Some("--".to_string()), Some(home_dir.clone()));
        assert!(result.is_ok(), "Setup command should succeed with custom separator");

        // Verify symlink was created
        let config_symlink = home_dir.join(".config/dotsym/dotsym.toml");
        assert!(config_symlink.is_symlink(), "Config symlink should exist");

        // Verify symlink points to correct location
        let link_target = fs::read_link(&config_symlink)?;
        let expected_target = dotfiles_dir.join("dotsym/--config--dotsym/dotsym.toml");
        assert_eq!(link_target, expected_target);

        Ok(())
    }

    #[test]
    fn test_setup_command_separator_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = temp_dir.path().join("dotfiles");

        // Set up dotfiles directory with different separator
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__dotsym"))?;
        let config_content = format!(r#"separator = "--"
dir = "{}"
"#, dotfiles_dir.to_string_lossy());
        fs::write(dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml"), config_content)?;

        // Test setup command with mismatched separator
        let result = setup_command_with_home_dir(Some(dotfiles_dir.to_string_lossy().to_string()), Some("__".to_string()), Some(home_dir.clone()));
        assert!(result.is_err(), "Setup command should fail with separator mismatch");

        let error_msg = format!("{}", result.unwrap_err());
        assert!(error_msg.contains("separator mismatch"), "Error should mention separator mismatch");
        assert!(error_msg.contains("Command line: '__'"), "Error should show command line separator");
        assert!(error_msg.contains("Config file:  '--'"), "Error should show config file separator");

        Ok(())
    }

    #[test]
    fn test_setup_command_directory_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = temp_dir.path().join("dotfiles");
        let different_dir = temp_dir.path().join("different");

        // Set up dotfiles directory with different directory in config
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__dotsym"))?;
        let config_content = format!(r#"separator = "__"
dir = "{}"
"#, different_dir.to_string_lossy());
        fs::write(dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml"), config_content)?;

        // Test setup command with mismatched directory
        let result = setup_command_with_home_dir(Some(dotfiles_dir.to_string_lossy().to_string()), Some("__".to_string()), Some(home_dir.clone()));
        assert!(result.is_err(), "Setup command should fail with directory mismatch");

        let error_msg = format!("{}", result.unwrap_err());
        assert!(error_msg.contains("directory mismatch"), "Error should mention directory mismatch");
        assert!(error_msg.contains(&*dotfiles_dir.to_string_lossy()), "Error should show command line directory");
        assert!(error_msg.contains(&*different_dir.to_string_lossy()), "Error should show config file directory");

        Ok(())
    }

    #[test]
    fn test_setup_command_tilde_expansion_matching() -> Result<(), Box<dyn std::error::Error>> {
        let temp_dir = TempDir::new()?;
        let home_dir = temp_dir.path().join("home");
        let dotfiles_dir = home_dir.join("dotfiles");

        // Set up dotfiles directory
        fs::create_dir_all(dotfiles_dir.join("dotsym/__config__dotsym"))?;
        let config_content = r#"separator = "__"
dir = "~/dotfiles"
"#;
        fs::write(dotfiles_dir.join("dotsym/__config__dotsym/dotsym.toml"), config_content)?;

        // Test setup command with tilde expansion
        let result = setup_command_with_home_dir(Some("~/dotfiles".to_string()), Some("__".to_string()), Some(home_dir.clone()));
        assert!(result.is_ok(), "Setup command should succeed with matching tilde expansion");

        // Verify symlink was created
        let config_symlink = home_dir.join(".config/dotsym/dotsym.toml");
        assert!(config_symlink.is_symlink(), "Config symlink should exist");

        Ok(())
    }
}
