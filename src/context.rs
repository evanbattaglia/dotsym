use std::collections::HashSet;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::error::DotsymError;

/// A possible location in the dotfiles repo where a target path could be moved
/// so that a symlink back to the original location can be managed by dotsym.
#[derive(Debug, Clone)]
#[allow(dead_code)] // some fields are descriptive metadata not consumed internally
pub struct DotsymizeCandidate {
    /// The host directory name (e.g. "dotsym", "myhostname", "myhostname__a").
    pub host_dir: String,
    /// The collapsed literal-directory name (the dir in which the link is created).
    pub literal_dir: String,
    /// The collapsed destination name (the file/dir directly inside the literal dir).
    pub dest_name: String,
    /// Full path to where the file/dir would live inside the repo.
    pub repo_dest: PathBuf,
    /// Full path to the literal directory inside the repo.
    pub literal_dir_path: PathBuf,
    /// Whether the literal directory already exists in the repo.
    pub literal_dir_exists: bool,
    /// Whether the host directory already exists in the repo.
    pub host_dir_exists: bool,
    /// Number of original path components that went into the literal directory.
    pub literal_depth: usize,
    /// Whether this host directory is host-specific (vs. the generic "dotsym").
    pub host_specific: bool,
}

#[derive(Debug, Clone)]
pub struct SymlinkMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
}

/// A broken symlink found in a directory the current dotsym structure references,
/// pointing somewhere inside the dotfiles repo whose target no longer exists.
/// These are the symlinks the `clean` command offers to remove.
#[derive(Debug, Clone)]
pub struct DanglingSymlink {
    /// The symlink itself (the path that would be removed).
    pub link_path: PathBuf,
    /// The (absolute, lexically-normalized) target it points to, now missing.
    pub target: PathBuf,
}

#[derive(Debug, Clone)]
pub enum SymlinkOperation {
    CreateSymlink(SymlinkMapping),
    AlreadyExists(SymlinkMapping),
    CreateWithBackup { mapping: SymlinkMapping, backup_path: PathBuf },
}

impl SymlinkOperation {
    pub fn describe(&self) -> String {
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

pub struct DotsymContext {
    pub config: Config,
    pub hostname: String,
    pub home_dir: PathBuf,
}

impl DotsymContext {
    pub fn new(config: Config, hostname: Option<String>, home_dir: Option<PathBuf>) -> Result<Self, DotsymError> {
        let hostname = hostname
            .or_else(|| config.hostname.clone())
            .unwrap_or_else(|| {
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

    /// Inverse of `expand_path`: turn a path relative to the home directory into
    /// a collapsed literal-directory or destination name (a leading "." becomes the
    /// separator, and "/" path separators become the separator). An empty string
    /// (the home directory itself) collapses to the bare separator.
    fn collapse_path(&self, rel: &str) -> String {
        let separator = &self.config.separator;

        if rel.is_empty() {
            return separator.clone();
        }

        let mut result = rel.to_string();

        if let Some(stripped) = result.strip_prefix('.') {
            result = format!("{}{}", separator, stripped);
        }

        result.replace('/', separator)
    }

    /// The configured dotfiles directory, with a leading `~/` expanded.
    pub fn config_dir(&self) -> PathBuf {
        let expanded = self.config.dir.replace("~/", &format!("{}/", self.home_dir.display()));
        PathBuf::from(expanded)
    }

    /// Determine the candidate host directories to consider when dotsymizing.
    /// Always includes the canonical generic ("dotsym") and host-specific
    /// (hostname) directories, plus any existing `dotsym__*` / `hostname__*`
    /// variants found in the repo. Returns (name, host_specific) pairs.
    fn dotsymize_host_dirs(&self, config_dir: &Path) -> Vec<(String, bool)> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();

        for (name, specific) in [(self.hostname.clone(), true), ("dotsym".to_string(), false)] {
            if seen.insert(name.clone()) {
                result.push((name, specific));
            }
        }

        if let Ok(entries) = fs::read_dir(config_dir) {
            for entry in entries.flatten() {
                if !entry.path().is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let specific = if name == self.hostname
                    || name.starts_with(&format!("{}__", self.hostname))
                {
                    true
                } else if name == "dotsym" || name.starts_with("dotsym__") {
                    false
                } else {
                    continue;
                };
                if seen.insert(name.clone()) {
                    result.push((name, specific));
                }
            }
        }

        result
    }

    /// Normalize a path logically (resolving `.` and `..` without touching the
    /// filesystem or following symlinks), making it absolute relative to `cwd`
    /// if necessary and expanding a leading `~`.
    pub fn resolve_target(&self, raw: &str, cwd: &Path) -> PathBuf {
        let expanded = if raw == "~" {
            self.home_dir.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            self.home_dir.join(rest)
        } else {
            PathBuf::from(raw)
        };

        let absolute = if expanded.is_absolute() {
            expanded
        } else {
            cwd.join(expanded)
        };

        logical_normalize(&absolute)
    }

    /// Given the (already resolved, absolute) path of a file or directory the
    /// user wants to manage, enumerate the possible places it could be moved to
    /// inside the dotfiles repo so a symlink back to it can be managed.
    pub fn dotsymize_candidates(&self, target: &Path) -> Result<Vec<DotsymizeCandidate>, DotsymError> {
        let config_dir = self.config_dir();

        let rel = target.strip_prefix(&self.home_dir).map_err(|_| DotsymError::NotUnderHome {
            path: target.to_path_buf(),
        })?;

        let components: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();

        if components.is_empty() {
            return Err(DotsymError::NotUnderHome {
                path: target.to_path_buf(),
            });
        }

        let host_dirs = self.dotsymize_host_dirs(&config_dir);
        let mut candidates = Vec::new();

        // Split the relative path at every component boundary: the prefix becomes
        // the literal directory, the remainder becomes the destination name.
        for k in 0..components.len() {
            let literal_rel = components[..k].join("/");
            let dest_rel = components[k..].join("/");
            let literal_dir = self.collapse_path(&literal_rel);
            let dest_name = self.collapse_path(&dest_rel);

            for (host_dir, host_specific) in &host_dirs {
                let host_path = config_dir.join(host_dir);
                let literal_dir_path = host_path.join(&literal_dir);
                let repo_dest = literal_dir_path.join(&dest_name);

                candidates.push(DotsymizeCandidate {
                    host_dir: host_dir.clone(),
                    literal_dir: literal_dir.clone(),
                    dest_name: dest_name.clone(),
                    literal_dir_exists: literal_dir_path.is_dir(),
                    host_dir_exists: host_path.is_dir(),
                    literal_dir_path,
                    repo_dest,
                    literal_depth: k,
                    host_specific: *host_specific,
                });
            }
        }

        Ok(candidates)
    }

    /// Move `target` into the repo at `repo_dest` and replace it with a symlink
    /// pointing at the new location.
    pub fn dotsymize_apply(&self, target: &Path, repo_dest: &Path) -> Result<(), DotsymError> {
        if repo_dest.exists() || repo_dest.is_symlink() {
            return Err(DotsymError::DestinationExists {
                path: repo_dest.to_path_buf(),
            });
        }

        if let Some(parent) = repo_dest.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent).map_err(|e| DotsymError::ParentDirectoryCreation {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        fs::rename(target, repo_dest).map_err(|e| DotsymError::BackupCreation {
            original_path: target.to_path_buf(),
            backup_path: repo_dest.to_path_buf(),
            source: e,
        })?;

        unix_fs::symlink(repo_dest, target).map_err(|e| DotsymError::SymlinkCreation {
            source: target.to_path_buf(),
            destination: repo_dest.to_path_buf(),
            io_error: e,
        })?;

        Ok(())
    }

    pub fn get_symlink_mappings(&self) -> Result<Vec<SymlinkMapping>, DotsymError> {
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
                // Use entry.path().is_dir() to follow symlinks - literal directories can be symlinks to directories
                if entry.path().is_dir() {
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

    /// Find symlinks that dotsym most likely created but which are now broken.
    ///
    /// It scans the directories where the current dotsym structure makes links
    /// (the parents of every symlink mapping for this host) and returns the
    /// symlinks there that:
    ///   * are actual symlinks (regular files/directories are never considered),
    ///   * point somewhere inside the dotfiles repo, and
    ///   * have a target that no longer exists.
    ///
    /// Symlinks pointing outside the dotfiles repo, and symlinks whose target
    /// still resolves, are deliberately left alone.
    pub fn find_dangling_symlinks(&self) -> Result<Vec<DanglingSymlink>, DotsymError> {
        let mappings = self.get_symlink_mappings()?;
        let config_dir = logical_normalize(&self.config_dir());

        // The directories where dotsym creates links are the parents of every
        // mapping source. Collect the unique, existing ones.
        let mut link_dirs: Vec<PathBuf> = Vec::new();
        let mut seen = HashSet::new();
        for mapping in &mappings {
            if let Some(parent) = mapping.source.parent() {
                let parent = parent.to_path_buf();
                if seen.insert(parent.clone()) {
                    link_dirs.push(parent);
                }
            }
        }
        link_dirs.sort();

        let mut dangling = Vec::new();
        for dir in link_dirs {
            if !dir.is_dir() {
                continue;
            }

            for entry in fs::read_dir(&dir).map_err(|e| DotsymError::DirectoryTraversal {
                path: dir.clone(),
                source: e,
            })? {
                let entry = entry.map_err(|e| DotsymError::DirectoryTraversal {
                    path: dir.clone(),
                    source: e,
                })?;
                let link_path = entry.path();

                // Only consider symlinks; never touch regular files or directories.
                let Ok(metadata) = fs::symlink_metadata(&link_path) else {
                    continue;
                };
                if !metadata.file_type().is_symlink() {
                    continue;
                }

                // Read the target and make it absolute relative to the link's dir.
                let Ok(raw_target) = fs::read_link(&link_path) else {
                    continue;
                };
                let abs_target = if raw_target.is_absolute() {
                    raw_target
                } else {
                    dir.join(raw_target)
                };
                let target = logical_normalize(&abs_target);

                // Only symlinks pointing into the dotfiles repo are ours to clean.
                if !target.starts_with(&config_dir) {
                    continue;
                }

                // Only broken ones. `Path::exists` follows the link, so it is
                // false exactly when the target no longer resolves.
                if link_path.exists() {
                    continue;
                }

                dangling.push(DanglingSymlink { link_path, target });
            }
        }

        dangling.sort_by(|a, b| a.link_path.cmp(&b.link_path));
        Ok(dangling)
    }

    pub fn generate_backup_path(&self, original_path: &Path) -> PathBuf {
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

    pub fn filter_mappings(&self, mappings: Vec<SymlinkMapping>, filter_path: Option<&str>) -> Result<Vec<SymlinkMapping>, DotsymError> {
        let Some(filter) = filter_path else {
            return Ok(mappings);
        };

        // Expand the filter path if it starts with ~/
        let expanded_filter = filter.replace("~/", &format!("{}/", self.home_dir.display()));

        // Determine if this is an absolute or relative path
        let filter_path_buf = PathBuf::from(&expanded_filter);
        let config_dir_expanded = self.config.dir.replace("~/", &format!("{}/", self.home_dir.display()));
        let config_dir = Path::new(&config_dir_expanded);

        // Convert to absolute path if it's relative
        let absolute_filter = if filter_path_buf.is_absolute() {
            filter_path_buf
        } else {
            config_dir.join(&filter_path_buf)
        };

        // Normalize the filter path (canonicalize if it exists, otherwise just clean it up)
        let normalized_filter = if absolute_filter.exists() {
            absolute_filter.canonicalize().unwrap_or(absolute_filter)
        } else {
            absolute_filter
        };

        // Filter mappings based on whether their destination starts with the filter path
        let filtered: Vec<SymlinkMapping> = mappings.into_iter()
            .filter(|mapping| {
                // Try to canonicalize the destination, fall back to the original path
                let dest_path = if mapping.destination.exists() {
                    mapping.destination.canonicalize().unwrap_or_else(|_| mapping.destination.clone())
                } else {
                    mapping.destination.clone()
                };

                // Check if destination starts with the filter path
                dest_path.starts_with(&normalized_filter)
            })
            .collect();

        Ok(filtered)
    }

    pub fn apply_symlinks(&self, dry_run: bool, no_backup_existing_symlinks: bool, filter_path: Option<&str>) -> Result<Vec<SymlinkOperation>, DotsymError> {
        let mappings = self.get_symlink_mappings()?;
        let filtered_mappings = self.filter_mappings(mappings, filter_path)?;
        let mut operations = Vec::new();

        for mapping in filtered_mappings {
            let operation = self.analyze_symlink(&mapping, no_backup_existing_symlinks)?;

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

    pub fn analyze_symlink(&self, mapping: &SymlinkMapping, no_backup_existing_symlinks: bool) -> Result<SymlinkOperation, DotsymError> {
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
                // Wrong or broken symlink - check if we should skip backup
                if no_backup_existing_symlinks {
                    // Skip backup, just replace the symlink
                    return Ok(SymlinkOperation::CreateSymlink(mapping.clone()));
                } else {
                    // Back it up like any other file
                    let backup_path = self.generate_backup_path(&mapping.source);
                    return Ok(SymlinkOperation::CreateWithBackup {
                        mapping: mapping.clone(),
                        backup_path,
                    });
                }
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

    pub fn execute_operation(&self, operation: &SymlinkOperation) -> Result<(), DotsymError> {
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

/// Lexically normalize a path (resolve `.` and `..` without touching the
/// filesystem or following symlinks). Used to compare paths and to resolve
/// the targets of (possibly broken) symlinks.
fn logical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}