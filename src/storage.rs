use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .context("create private temporary file")?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    // Keep the new data recoverable if replacing the destination fails.
    std::fs::rename(&temporary, path).with_context(|| {
        format!(
            "persist {}; new data retained at {}",
            path.display(),
            temporary.display()
        )
    })?;
    Ok(())
}
