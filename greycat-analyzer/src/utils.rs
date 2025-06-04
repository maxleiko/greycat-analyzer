use std::{error::Error, fs, path::Path};

pub type AnyError = Box<dyn Error + Sync + Send>;

/// Walks a directory and calls `on_entry` for each entry that satisfies `is_valid`.
pub(crate) fn for_each_valid_entry(
    path: &Path,
    is_valid: &dyn Fn(&Path) -> bool,
    on_entry: &dyn Fn(&Path) -> Result<(), AnyError>,
) -> Result<(), AnyError> {
    // Read entries in this directory
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            // Full path of the entry
            let entry_path = entry.path();

            // If valid, call the callback
            if is_valid(&entry_path) {
                on_entry(&entry_path)?;
            }

            // If it's a dir, recurse
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    for_each_valid_entry(&entry_path, is_valid, on_entry)?;
                }
            }
        }
    }
    Ok(())
}
