use anyhow::{anyhow, Result};
use serde::Serialize;
use std::{io::Write, path::Path};
use turborm_core::library::Listing;

pub fn json(value: &impl Serialize, out: Option<&Path>) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if let Some(out) = out {
        let parent = out
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(out)
            .map_err(|_| anyhow!("Cannot save JSON output without overwriting an existing file"))?;
        eprintln!("Saved JSON to {}.", out.display());
    } else {
        std::io::stdout().lock().write_all(&bytes)?;
    }
    Ok(())
}
pub fn listing(listing: &Listing, as_json: bool, out: Option<&Path>) -> Result<()> {
    if as_json || out.is_some() {
        return json(listing, out);
    }
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "ID                                    TYPE      BYTES         PATH"
    )?;
    for entry in &listing.entries {
        // Names are untrusted text: never send terminal controls to the user's shell.
        let path = entry
            .path
            .chars()
            .flat_map(|c| c.escape_debug())
            .collect::<String>();
        writeln!(
            stdout,
            "{}  {:8}  {:12}  {}{}",
            entry.id,
            entry.kind,
            entry.size,
            path,
            if entry.deleted { " [trashed]" } else { "" }
        )?;
    }
    eprintln!(
        "{} entries at cloud generation {}.",
        listing.entries.len(),
        listing.generation
    );
    if !listing.failures.is_empty() {
        eprintln!(
            "Skipped {} unreadable cloud entr{}; refresh to retry:",
            listing.failures.len(),
            if listing.failures.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
        for failure in &listing.failures {
            // Cloud errors are also untrusted text and may contain remote filenames.
            let error = failure
                .error
                .chars()
                .flat_map(|character| character.escape_debug())
                .collect::<String>();
            eprintln!("  {}: {error}", failure.id);
        }
    }
    Ok(())
}
