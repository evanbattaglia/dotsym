use anyhow::{Context, Result as AnyhowResult};
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::context::DotsymContext;

pub fn preview_command(context: &DotsymContext) -> AnyhowResult<()> {
    let mappings = context.get_symlink_mappings()
        .context("Failed to generate symlink mappings")?;

    for mapping in mappings {
        println!("{}", mapping.source.display());
        println!("{}", mapping.destination.display());
        println!();
    }

    Ok(())
}

pub fn apply_command(context: &DotsymContext, dry_run: bool, no_backup_existing_symlinks: bool) -> AnyhowResult<()> {
    if dry_run {
        println!("DRY RUN - showing what would be done:");
        println!();
    }

    let operations = context.apply_symlinks(dry_run, no_backup_existing_symlinks)
        .context("Failed to apply symlink operations")?;

    if dry_run {
        println!();

        // Count operation types
        let mut exists_count = 0;
        let mut create_count = 0;
        let mut backup_count = 0;

        for operation in &operations {
            match operation {
                crate::context::SymlinkOperation::AlreadyExists(_) => exists_count += 1,
                crate::context::SymlinkOperation::CreateSymlink(_) => create_count += 1,
                crate::context::SymlinkOperation::CreateWithBackup { .. } => {
                    create_count += 1;
                    backup_count += 1;
                }
            }
        }

        println!("Dry run complete:");
        println!("  {} symlinks already exist (nothing to do)", exists_count);
        println!("  {} symlinks would be created", create_count);
        if backup_count > 0 {
            println!("  {} files/directories would be backed up", backup_count);
        }
        println!();
        println!("Run without --dry-run to apply these changes.");
    } else {
        println!();
        println!("Applied {} symlink operations successfully.", operations.len());
    }

    Ok(())
}

pub fn setup_command(directory: String, separator: String) -> AnyhowResult<()> {
    setup_command_with_home_dir(Some(directory), Some(separator), None)
}

pub fn setup_command_with_home_dir(directory: Option<String>, separator: Option<String>, home_dir_override: Option<PathBuf>) -> AnyhowResult<()> {
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
