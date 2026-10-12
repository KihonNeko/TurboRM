//! `turborm review`: send a PDF to the tablet for handwritten review, check
//! that the tablet has synced, and fetch the annotated pages.
//!
//! `send` adds exactly one new document through the same import, upload and
//! verification path as the app's Import and Verify import, plus the folders
//! of `--folder` that do not exist yet. `status` and
//! `fetch` only read. Nothing here moves, renames or deletes cloud entries.
//! This runs before, and instead of, any window or toolkit initialisation.

use anyhow::{anyhow, ensure, Context, Result};
use clap::Parser;
use std::ffi::OsString;
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};
use turborm::review::{self, Cli, CloudDocument, FetchContext, ReviewCommand};
use turborm_core::{
    auth, cloud, folders, hash_tree, import,
    job::{self, Job, State},
    library::{self, Library},
};
use zeroize::Zeroizing;

struct Session {
    http: cloud::Http,
    token: Zeroizing<String>,
    host: String,
}

impl Session {
    fn connect() -> Result<Self> {
        let (http, token, host) = auth::connect()
            .context("Not connected to reMarkable Cloud. Run `turborm` once and pair it")?;
        Ok(Self {
            http,
            token: Zeroizing::new(token),
            host,
        })
    }
    fn library(&self) -> Result<Library<'_, cloud::Http>> {
        Ok(Library::new(
            &self.http,
            &self.host,
            &self.token,
            Some(library::default_cache_dir()?),
        ))
    }
}

fn state_dir(given: Option<PathBuf>) -> Result<PathBuf> {
    given.map_or_else(review::default_state_dir, Ok)
}

fn send(
    file: &Path,
    title: &str,
    folder: Option<&str>,
    source: Option<PathBuf>,
    state_dir: &Path,
) -> Result<()> {
    ensure!(file.is_file(), "{} is not a file", file.display());
    let source = source
        .map(|path| {
            std::fs::canonicalize(&path)
                .with_context(|| format!("Source file {} is not readable", path.display()))
        })
        .transpose()?;
    let session = Session::connect()?;
    let (parent, folder_name) = match folder {
        Some(path) => {
            // Reuse each existing folder on the path; create only the missing
            // ones. This only ever adds folders.
            eprintln!("Finding or creating the folder \"{path}\"...");
            let steps = folders::ensure_folder_path(
                &session.http,
                &session.host,
                &session.token,
                Some(library::default_cache_dir()?),
                "",
                path,
            )?;
            for step in &steps {
                eprintln!(
                    "{} folder \"{}\" ({})",
                    if step.created { "Created" } else { "Using" },
                    step.name,
                    step.id
                );
            }
            let id = steps
                .last()
                .map(|step| step.id.clone())
                .ok_or_else(|| anyhow!("The folder path is empty"))?;
            let names: Vec<&str> = steps.iter().map(|step| step.name.as_str()).collect();
            (id, Some(names.join("/")))
        }
        None => default_destination(&session)?,
    };
    let job_dir = job::default_job_dir()?;
    import::prepare_import(file, &job_dir, Some(title), &parent)?;
    eprintln!("Upload journal: {}", job_dir.display());
    let recover = || {
        format!(
            "Upload journal retained at {}. Check it with Verify import in TurboRM (or `turborm-cli verify {0}`) before sending again",
            job_dir.display()
        )
    };
    let mut progress = |line: &str| eprintln!("{line}");
    hash_tree::send_with_progress(
        &session.http,
        &session.host,
        &session.token,
        &job_dir,
        &mut progress,
    )
    .with_context(recover)?;
    let mut uploaded = Job::load(&job_dir)?;
    if uploaded.state != State::Verified {
        // The same read-back check as Verify import.
        hash_tree::verify_with_progress(
            &session.http,
            &session.host,
            &session.token,
            &job_dir,
            &mut progress,
        )
        .with_context(recover)?;
        uploaded = Job::load(&job_dir)?;
    }
    let tree = uploaded.hash_tree.as_ref().ok_or_else(|| {
        anyhow!("The upload used the legacy protocol; no document hash is recorded")
    })?;
    let state = review::UploadState {
        id: uploaded.uuid.to_string(),
        title: title.to_owned(),
        parent,
        folder: folder_name,
        uploaded_at: review::utc_timestamp(SystemTime::now()),
        document_hash: tree.document_hash.clone(),
        source,
        job: Some(job_dir.clone()),
    };
    let saved = review::save_state(state_dir, &state)?;
    eprintln!("Review state saved to {}", saved.display());
    println!("{}", serde_json::to_string_pretty(&state)?);
    Ok(())
}

/// Without --folder: the existing top-level "Claude Review" folder, or the
/// library root. Nothing is created.
fn default_destination(session: &Session) -> Result<(String, Option<String>)> {
    let folder = review::DEFAULT_FOLDER;
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    eprintln!(
        "Looking for a top-level folder named \"{folder}\" among {} cloud entries...",
        snapshot.manifest.rows.len()
    );
    let listing = library.list(&snapshot, 4)?;
    let (destination, matches) = review::find_folder(&listing.entries, folder);
    match destination {
        Some(entry) if matches > 1 => eprintln!(
            "{matches} folders are named \"{folder}\"; using the one with ID {}.",
            entry.id
        ),
        Some(_) => {}
        None => eprintln!(
            "No top-level folder named \"{folder}\" exists, so the document goes to the library root. \
Pass --folder \"{folder}/<project>\" to create one."
        ),
    }
    Ok(destination.map_or_else(
        || (String::new(), None),
        |entry| (entry.id.clone(), Some(entry.title.clone())),
    ))
}

fn status(id: &str, state_dir: &Path) -> Result<()> {
    let saved = review::load_state(state_dir, id)?;
    let session = Session::connect()?;
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    let current = match snapshot.manifest.rows.iter().find(|row| row.name == id) {
        None => None,
        Some(row) => {
            let info = library.info(&snapshot, id)?;
            Some(CloudDocument {
                title: info.entry.title.clone(),
                parent: info.entry.parent.clone(),
                deleted: info.entry.deleted,
                last_modified: info.metadata["lastModified"].as_str().map(str::to_owned),
                hash: row.hash.clone(),
                page_count: info.entry.page_count,
                scene_pages: info
                    .files
                    .iter()
                    .filter(|file| file.path.ends_with(".rm"))
                    .count(),
            })
        }
    };
    let mut output = review::status_json(id, current.as_ref(), saved.as_ref());
    output["generation"] = snapshot.generation.into();
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn fetch(id: &str, out: &Path, longest_side: u32, state_dir: &Path) -> Result<()> {
    ensure!(
        !out.exists(),
        "{} already exists; choose a new output directory",
        out.display()
    );
    let saved = review::load_state(state_dir, id)?;
    let session = Session::connect()?;
    let library = session.library()?;
    let snapshot = library.snapshot()?;
    let row = snapshot
        .manifest
        .rows
        .iter()
        .find(|row| row.name == id)
        .ok_or_else(|| anyhow!("Document {id} is not in the cloud library"))?;
    let document_hash = row.hash.clone();
    let info = library.info(&snapshot, id)?;
    ensure!(
        info.entry.kind == "document",
        "{id} is a folder, not a document"
    );
    if info.entry.deleted {
        eprintln!("Note: the document is in the trash on the tablet.");
    }
    let directory = tempfile::Builder::new()
        .prefix("turborm-review-")
        .tempdir()?;
    let count = library.pull(
        &snapshot,
        id,
        &directory.path().join("bundle"),
        library::DEFAULT_MAX_BYTES,
    )?;
    eprintln!("Downloaded and verified {count} files; rendering pages...");
    let report = review::render_review(
        directory,
        id,
        longest_side,
        out,
        FetchContext {
            document_hash: Some(document_hash),
            generation: Some(snapshot.generation),
            saved,
        },
    )?;
    let out = std::fs::canonicalize(out)?;
    let pages = report["pages"].as_array().cloned().unwrap_or_default();
    let annotated: Vec<_> = pages
        .iter()
        .filter(|page| page["annotated"] == true)
        .map(|page| page["page"].clone())
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "id": id,
            "out": out,
            "pages": pages.len(),
            "annotated_pages": annotated,
            "summary": out.join("summary.md"),
            "annotations": out.join("annotations.json"),
        }))?
    );
    Ok(())
}

/// Whether the command line asks for the review CLI: `turborm review ...`.
pub fn requested(args: &[OsString]) -> bool {
    args.get(1).is_some_and(|arg| arg == "review")
}

/// Run `turborm review ...` from the full command line and return the exit
/// code. Help and argument errors are printed by clap.
pub fn main(args: Vec<OsString>) -> i32 {
    // clap names the program from the first item, so usage reads
    // `turborm review <COMMAND>`.
    let cli = Cli::parse_from(
        std::iter::once(OsString::from("turborm review")).chain(args.into_iter().skip(2)),
    );
    match run(cli) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        ReviewCommand::Send {
            file,
            title,
            folder,
            source,
            state_dir: dir,
        } => send(&file, &title, folder.as_deref(), source, &state_dir(dir)?),
        ReviewCommand::Status { id, state_dir: dir } => status(&id, &state_dir(dir)?),
        ReviewCommand::Fetch {
            id,
            out,
            longest_side,
            state_dir: dir,
        } => fetch(&id, &out, longest_side, &state_dir(dir)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_a_leading_review_argument_selects_the_cli() {
        assert!(requested(&args(&["turborm", "review"])));
        assert!(requested(&args(&["turborm", "review", "status", "x"])));
        assert!(requested(&args(&["/usr/bin/turborm", "review", "--help"])));
        assert!(!requested(&args(&["turborm"])));
        assert!(!requested(&args(&["turborm", "--demo"])));
        assert!(!requested(&args(&["turborm", "--screenshot", "review"])));
        assert!(!requested(&args(&["turborm", "reviews"])));
    }

    #[test]
    fn review_arguments_reach_the_review_parser() {
        let id = "3f2c6d1e-8a4b-4c5d-9e6f-7a8b9c0d1e2f";
        let parsed = Cli::try_parse_from(
            std::iter::once(OsString::from("turborm review")).chain(
                args(&["turborm", "review", "status", id])
                    .into_iter()
                    .skip(2),
            ),
        )
        .unwrap();
        assert_eq!(
            parsed.command,
            ReviewCommand::Status {
                id: id.into(),
                state_dir: None
            }
        );
    }
}
