use anyhow::{Context, Result as AnyhowResult};
use std::cmp::Reverse;
use std::fs;
use std::io::Write;
use std::os::unix::fs as unix_fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::context::{DotsymContext, DotsymizeCandidate, SymlinkMapping};

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

pub fn apply_command(context: &DotsymContext, dry_run: bool, no_backup_existing_symlinks: bool, filter_path: Option<String>) -> AnyhowResult<()> {
    if dry_run {
        println!("DRY RUN - showing what would be done:");
        println!();
    }

    if let Some(ref path) = filter_path {
        println!("Filtering to path: {}", path);
        println!();
    }

    let operations = context.apply_symlinks(dry_run, no_backup_existing_symlinks, filter_path.as_deref())
        .context("Failed to apply symlink operations")?;

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

    if dry_run {
        println!("Dry run complete:");
        println!("  {} symlinks already exist (nothing to do)", exists_count);
        println!("  {} symlinks would be created", create_count);
        if backup_count > 0 {
            println!("  {} files/directories would be backed up", backup_count);
        }
        println!();
        println!("Run without --dry-run to apply these changes.");
    } else {
        println!("Complete:");
        println!("  {} symlinks already existed (nothing done)", exists_count);
        println!("  {} symlinks created", create_count);
        if backup_count > 0 {
            println!("  {} files/directories backed up", backup_count);
        }
    }

    Ok(())
}

/// Decide which candidates to present and in what order. Existing literal
/// directories are preferred (deepest first), then host-specific over generic.
/// For locations that don't exist yet, only offer the canonical "new directory"
/// choice (mirror the full parent path) in the two primary host directories, to
/// keep the list focused.
pub(crate) fn select_dotsymize_candidates(
    context: &DotsymContext,
    candidates: Vec<DotsymizeCandidate>,
) -> Vec<DotsymizeCandidate> {
    let max_depth = candidates.iter().map(|c| c.literal_depth).max().unwrap_or(0);

    let mut kept: Vec<DotsymizeCandidate> = candidates
        .into_iter()
        .filter(|c| {
            c.literal_dir_exists
                || (c.literal_depth == max_depth
                    && (c.host_dir == context.hostname || c.host_dir == "dotsym"))
        })
        .collect();

    kept.sort_by(|a, b| {
        // existing literal dirs first, then deeper literal dirs, then
        // host-specific before generic, then by host name for stability.
        (!a.literal_dir_exists, Reverse(a.literal_depth), !a.host_specific, a.host_dir.clone()).cmp(
            &(!b.literal_dir_exists, Reverse(b.literal_depth), !b.host_specific, b.host_dir.clone()),
        )
    });

    // Drop duplicate destinations (different splits can collapse to the same path).
    let mut seen = std::collections::HashSet::new();
    kept.retain(|c| seen.insert(c.repo_dest.clone()));

    kept
}

pub fn dotsymize_command(
    context: &DotsymContext,
    path: String,
    dry_run: bool,
    yes: bool,
) -> AnyhowResult<()> {
    let cwd = std::env::current_dir().context("Failed to determine current directory")?;
    let target = context.resolve_target(&path, &cwd);

    if !target.exists() && !target.is_symlink() {
        return Err(anyhow::anyhow!("Path does not exist: {}", target.display()));
    }

    if target.is_symlink() {
        return Err(anyhow::anyhow!(
            "{} is already a symlink; it may already be managed. Refusing to dotsymize it.",
            target.display()
        ));
    }

    let config_dir = context.config_dir();
    if target.starts_with(&config_dir) {
        return Err(anyhow::anyhow!(
            "{} is inside the dotfiles repo ({}); nothing to do.",
            target.display(),
            config_dir.display()
        ));
    }

    if target
        .components()
        .any(|c| c.as_os_str().to_string_lossy().contains(&context.config.separator))
    {
        eprintln!(
            "Warning: a path component contains the separator '{}'; the generated symlink may not round-trip correctly.",
            context.config.separator
        );
    }

    let candidates = select_dotsymize_candidates(context, context.dotsymize_candidates(&target)?);

    if candidates.is_empty() {
        return Err(anyhow::anyhow!("Could not determine any dotsym destination for {}", target.display()));
    }

    println!("Dotsymize: {}", target.display());
    println!();
    println!("Candidate locations in the dotfiles repo ({}):", config_dir.display());
    println!();

    for (i, c) in candidates.iter().enumerate() {
        let rel = c.repo_dest.strip_prefix(&config_dir).unwrap_or(&c.repo_dest);
        let mut tags = Vec::new();
        if c.literal_dir_exists {
            tags.push("existing dir".to_string());
        } else if !c.host_dir_exists {
            tags.push("new host dir".to_string());
        } else {
            tags.push("new dir".to_string());
        }
        if i == 0 {
            tags.push("recommended".to_string());
        }
        println!("  {}) {}  [{}]", i + 1, rel.display(), tags.join(", "));
    }
    println!();

    let choice = if yes || dry_run {
        Some(0)
    } else {
        prompt_choice(candidates.len())?
    };

    let Some(selected) = choice else {
        println!("Cancelled.");
        return Ok(());
    };

    let chosen = &candidates[selected];

    println!();
    println!("Will create:");
    println!("  {} -> {}", target.display(), chosen.repo_dest.display());

    if dry_run {
        println!();
        println!("Dry run - nothing was moved. Re-run without --dry-run to apply.");
        return Ok(());
    }

    context
        .dotsymize_apply(&target, &chosen.repo_dest)
        .with_context(|| format!("Failed to dotsymize {}", target.display()))?;

    println!();
    println!("Done. {} now points into the dotfiles repo.", target.display());

    Ok(())
}

/// Prompt for a 1-based choice; returns the 0-based index, or None if cancelled.
/// In `dry_run`/`yes` paths this is not called. Defaults to the first (recommended)
/// candidate on an empty line.
fn prompt_choice(count: usize) -> AnyhowResult<Option<usize>> {
    loop {
        print!("Choose a destination [1] (q to cancel): ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        let n = std::io::stdin().read_line(&mut line).context("Failed to read input")?;
        if n == 0 {
            // EOF
            return Ok(None);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(Some(0));
        }
        if trimmed.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        match trimmed.parse::<usize>() {
            Ok(i) if i >= 1 && i <= count => return Ok(Some(i - 1)),
            _ => println!("Please enter a number between 1 and {} (or q to cancel).", count),
        }
    }
}

pub fn clean_command(context: &DotsymContext, dry_run: bool, yes: bool) -> AnyhowResult<()> {
    let config_dir = context.config_dir();

    let dangling = context.find_dangling_symlinks()
        .context("Failed to scan for dangling symlinks")?;

    if dangling.is_empty() {
        println!("No dangling dotsym symlinks found.");
        print_clean_caveat();
        return Ok(());
    }

    println!(
        "Found {} dangling symlink(s) pointing into the dotfiles repo ({}):",
        dangling.len(),
        config_dir.display()
    );
    println!();
    for d in &dangling {
        println!("  {} -> {} (target missing)", d.link_path.display(), d.target.display());
    }
    println!();

    if dry_run {
        println!("Dry run - nothing was deleted. Re-run without --dry-run to remove these symlinks.");
        print_clean_caveat();
        return Ok(());
    }

    let proceed = if yes {
        true
    } else {
        prompt_yes_no(&format!("Delete {} symlink(s)?", dangling.len()))?
    };

    if !proceed {
        println!("Cancelled. No symlinks were deleted.");
        print_clean_caveat();
        return Ok(());
    }

    let mut deleted = 0;
    for d in &dangling {
        // Re-verify right before removing: it must still be a symlink and still
        // be broken, so we never delete something that changed underneath us and
        // never remove a regular file.
        let still_dangling = fs::symlink_metadata(&d.link_path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
            && !d.link_path.exists();

        if !still_dangling {
            println!("Skipped (changed since scan): {}", d.link_path.display());
            continue;
        }

        match fs::remove_file(&d.link_path) {
            Ok(()) => {
                println!("Deleted: {}", d.link_path.display());
                deleted += 1;
            }
            Err(e) => {
                eprintln!("Failed to delete {}: {}", d.link_path.display(), e);
            }
        }
    }

    println!();
    println!("Deleted {} of {} symlink(s).", deleted, dangling.len());
    print_clean_caveat();

    Ok(())
}

/// Prompt for a yes/no answer, defaulting to "no" on an empty line or EOF.
fn prompt_yes_no(question: &str) -> AnyhowResult<bool> {
    loop {
        print!("{} [y/N]: ", question);
        std::io::stdout().flush().ok();

        let mut line = String::new();
        let n = std::io::stdin().read_line(&mut line).context("Failed to read input")?;
        if n == 0 {
            // EOF
            return Ok(false);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("n") || trimmed.eq_ignore_ascii_case("no") {
            return Ok(false);
        }
        if trimmed.eq_ignore_ascii_case("y") || trimmed.eq_ignore_ascii_case("yes") {
            return Ok(true);
        }
        println!("Please answer y or n.");
    }
}

/// Remind the user that `clean` only inspects locations the *current* dotsym
/// structure points at, so links orphaned by deleting whole repo directories
/// won't be discovered here.
fn print_clean_caveat() {
    println!();
    println!("Note: clean only looks in directories referenced by the current dotsym");
    println!("directory structure. If you deleted whole directories from that structure,");
    println!("there may still be dangling symlinks in locations that are no longer");
    println!("referenced; those are not detected here and must be removed manually.");
}

pub fn setup_command(directory: String, separator: String, hostname: Option<String>) -> AnyhowResult<()> {
    setup_command_with_home_dir(Some(directory), Some(separator), hostname, None)
}

pub fn setup_command_with_home_dir(directory: Option<String>, separator: Option<String>, hostname: Option<String>, home_dir_override: Option<PathBuf>) -> AnyhowResult<()> {
    let separator = separator.unwrap_or_else(|| "__".to_string());
    let directory = directory.unwrap_or_else(|| "~/dotfiles".to_string());

    println!("Setting up dotsym with:");
    println!("  Directory: {}", directory);
    println!("  Separator: {}", separator);
    if let Some(ref h) = hostname {
        println!("  Hostname:  {}", h);
    }
    println!();

    // Create temporary config for setup
    let temp_config = Config {
        separator: separator.clone(),
        dir: directory.clone(),
        hostname: None,
    };

    let context = DotsymContext::new(temp_config, hostname.clone(), home_dir_override)
        .context("Failed to initialize dotsym context")?;

    if hostname.is_none() {
        match context.hostname.to_lowercase().as_str() {
            "localhost" | "localhost.localdomain" | "(none)" | "unknown" => {
                println!("⚠  Detected hostname is '{}' — this appears to be a generic/default value.", context.hostname);
                println!("   If you manage dotfiles across multiple machines, consider creating a");
                println!("   host-specific config, then run 'dotsym setup --hostname <name>' to link it.");
                println!("   For example:");
                println!("     mkdir -p {}/<name>/__config__dotsym", directory);
                println!("     # create dotsym.toml inside it, then:");
                println!("     dotsym setup --hostname <name>");
                println!();
            }
            _ => {}
        }
    }

    // Get all mappings and find the dotsym.toml file
    let mappings = context.get_symlink_mappings()
        .context("Failed to generate symlink mappings")?;

    // Look for dotsym.toml in the mappings — prefer a host-specific one if given
    let is_dotsym_config = |m: &&SymlinkMapping| -> bool {
        m.source.file_name()
            .map(|name| name == "dotsym.toml")
            .unwrap_or(false) &&
        m.source.parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name == "dotsym")
            .unwrap_or(false)
    };

    let dotsym_config_mapping = hostname.as_ref()
        .and_then(|h| {
            let pattern = format!("/{}/", h);
            mappings.iter().find(|m| is_dotsym_config(m) && m.destination.to_string_lossy().contains(&pattern))
        })
        .or_else(|| mappings.iter().find(is_dotsym_config));

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

                // Validate hostname if both CLI and config specify one
                if let (Some(cli_hostname), Some(config_hostname)) = (&hostname, &found_config.hostname) {
                    if cli_hostname != config_hostname {
                        return Err(anyhow::anyhow!(
                            "Config file hostname mismatch:\n  Command line: '{}'\n  Config file:  '{}'\n\nPlease ensure the --hostname argument matches the config file.",
                            cli_hostname, config_hostname
                        ));
                    }
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
            match &hostname {
                Some(h) => println!("  {}/{}/__config__dotsym/dotsym.toml", directory, h),
                None => println!("  {}/$(hostname)/__config__dotsym/dotsym.toml", directory),
            }
            println!("  etc.");
            println!();
            println!("Please create a dotsym.toml file in your dotfiles directory first.");
            return Err(anyhow::anyhow!("No dotsym.toml found in dotfiles directory"));
        }
    }

    Ok(())
}
