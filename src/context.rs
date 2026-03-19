use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::DotsymError;

#[derive(Debug, Clone)]
pub struct SymlinkMapping {
    pub source: PathBuf,
    pub destination: PathBuf,
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