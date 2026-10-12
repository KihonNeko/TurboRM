#![forbid(unsafe_code)]

mod library_cli;
use turborm_core::{auth, cloud, hash_tree, job};

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(
    version,
    about = "Browse, download and upload reMarkable Cloud documents"
)]
struct Cli {
    /// Override the verified metadata cache directory (contains document names, no tokens).
    #[arg(long, global = true, conflicts_with = "no_cache")]
    cache_dir: Option<PathBuf>,
    /// Bypass the library metadata cache.
    #[arg(long, global = true)]
    no_cache: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// List all cloud documents/folders; aliases: ls. Cloud read-only.
    #[command(alias = "ls")]
    List {
        /// Folder UUID, or root. Without this option, lists the whole library.
        #[arg(long)]
        folder: Option<String>,
        /// Include descendants when filtering by folder.
        #[arg(long, requires = "folder")]
        recursive: bool,
        #[arg(long)]
        include_trash: bool,
        #[arg(long)]
        json: bool,
        /// Save JSON to a new file instead of printing document names.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=8))]
        jobs: u8,
    },
    /// Show a document's metadata, content descriptor and cloud component inventory.
    Info {
        id: String,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Download all native components of one document/folder into a new directory.
    Pull {
        id: String,
        #[arg(long)]
        out: PathBuf,
        /// Maximum bytes per component (default 512 MiB).
        #[arg(long, default_value_t = turborm_core::library::DEFAULT_MAX_BYTES)]
        max_bytes: usize,
    },
    /// Export the original PDF/EPUB. Handwritten overlays are retained by pull, not rendered.
    Export {
        id: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = turborm_core::library::DEFAULT_MAX_BYTES)]
        max_bytes: usize,
    },
    /// Create a NEW folder and print its ID. Only adds; nothing is changed or moved.
    Mkdir {
        /// Folder name, or with -p a path such as "Claude Review/TurboRM".
        name: String,
        /// Parent folder UUID. Without it the folder goes to the library root.
        #[arg(long)]
        parent: Option<String>,
        /// Treat NAME as a path: reuse each existing folder (ignoring case) and
        /// create only the missing ones.
        #[arg(short = 'p', long = "parents")]
        parents: bool,
    },
    /// Pair using a hidden local code; store this app's credential in the system keyring.
    Pair {
        /// Replace this app's credential only after new registration succeeds.
        #[arg(long)]
        replace: bool,
        /// Check mapped cloud routes immediately with the new user token.
        #[arg(long)]
        diagnose: bool,
    },
    /// Check authentication and read-only cloud access.
    Status,
    /// Read-only diagnostics for mapped cloud protocols; prints no private response data.
    Diagnose,
    /// Read the root manifest and print only structural information.
    InspectTree,
    /// Prepare a PDF bundle locally. No credentials or network needed.
    Prepare {
        file: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        title: Option<String>,
    },
    /// Prepare and upload one PDF as a NEW document in the root folder.
    Upload {
        file: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
    },
    /// Send a previously prepared job. Attempted jobs cannot be replayed.
    Send { job: PathBuf },
    /// Read the cloud copy and verify every component; never uploads or deletes.
    Verify { job: PathBuf },
}
fn run() -> Result<()> {
    let cli = Cli::parse();
    let cache = || -> Result<Option<PathBuf>> {
        if cli.no_cache {
            Ok(None)
        } else {
            Ok(Some(match &cli.cache_dir {
                Some(path) => path.clone(),
                None => turborm_core::library::default_cache_dir()?,
            }))
        }
    };
    match cli.command {
        Command::List {
            folder,
            recursive,
            include_trash,
            json,
            out,
            jobs,
        } => {
            let (http, token, host) = auth::connect()?;
            let token = Zeroizing::new(token);
            let library = turborm_core::library::Library::new(&http, &host, &token, cache()?);
            let snapshot = library.snapshot()?;
            eprintln!(
                "Reading {} cloud entries (verified metadata cache enabled: {})...",
                snapshot.manifest.rows.len(),
                !cli.no_cache
            );
            let mut listing = library.list(&snapshot, jobs as usize)?;
            turborm_core::library::filter_listing(
                &mut listing,
                folder.as_deref(),
                recursive,
                include_trash,
            )?;
            library_cli::listing(&listing, json, out.as_deref())
        }
        Command::Info { id, out } => {
            let (http, token, host) = auth::connect()?;
            let token = Zeroizing::new(token);
            let library = turborm_core::library::Library::new(&http, &host, &token, cache()?);
            library_cli::json(&library.info(&library.snapshot()?, &id)?, out.as_deref())
        }
        Command::Pull { id, out, max_bytes } => {
            let (http, token, host) = auth::connect()?;
            let token = Zeroizing::new(token);
            let library = turborm_core::library::Library::new(&http, &host, &token, cache()?);
            let count = library.pull(&library.snapshot()?, &id, &out, max_bytes)?;
            println!(
                "Downloaded and verified {count} native files into {}.",
                out.display()
            );
            Ok(())
        }
        Command::Export { id, out, max_bytes } => {
            let (http, token, host) = auth::connect()?;
            let token = Zeroizing::new(token);
            let library = turborm_core::library::Library::new(&http, &host, &token, cache()?);
            let format = library.export(&library.snapshot()?, &id, &out, max_bytes)?;
            println!("Exported and verified original {format} to {}. Handwritten overlays are available through pull.",out.display());
            Ok(())
        }
        Command::Mkdir {
            name,
            parent,
            parents,
        } => {
            let (http, token, host) = auth::connect()?;
            let token = Zeroizing::new(token);
            let parent = parent.unwrap_or_default();
            let id = if parents {
                let steps = turborm_core::folders::ensure_folder_path(
                    &http,
                    &host,
                    &token,
                    cache()?,
                    &parent,
                    &name,
                )?;
                for step in &steps {
                    eprintln!(
                        "{} {} \"{}\"",
                        if step.created { "Created" } else { "Reused " },
                        step.id,
                        step.name.escape_debug()
                    );
                }
                steps
                    .last()
                    .map(|step| step.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("The folder path is empty"))?
            } else {
                let id = turborm_core::folders::create_folder(
                    &http,
                    &host,
                    &token,
                    cache()?,
                    &parent,
                    &name,
                )?;
                eprintln!("Created and verified folder {id}.");
                id
            };
            println!("{id}");
            Ok(())
        }
        Command::Pair { replace, diagnose } => auth::pair(replace, diagnose),
        Command::Prepare { file, out, title } => {
            job::prepare(&file, &out, title.as_deref())?;
            println!(
                "Prepared PDF bundle at {}. No cloud request was made.",
                out.display()
            );
            Ok(())
        }
        Command::Diagnose => {
            let (http, token, host) = auth::connect()?;
            http.diagnose(&host, &Zeroizing::new(token))
        }
        Command::Status => {
            let (http, token, host) = auth::connect()?;
            hash_tree::status(&http, &host, &Zeroizing::new(token))
        }
        Command::InspectTree => {
            let (http, token, host) = auth::connect()?;
            cloud::inspect_tree(&http, &host, &Zeroizing::new(token))
        }
        Command::Upload { file, out, title } => {
            // Authenticate before staging, so a missing pairing does not create abandoned jobs.
            let (http, token, host) = auth::connect()?;
            let token = Zeroizing::new(token);
            let out = match out {
                Some(p) => p,
                None => job::default_job_dir()?,
            };
            job::prepare(&file, &out, title.as_deref())?;
            println!("Upload job: {}", out.display());
            hash_tree::send(&http, &host, &token, &out)
        }
        Command::Send { job } => {
            let (http, token, host) = auth::connect()?;
            hash_tree::send(&http, &host, &Zeroizing::new(token), &job)
        }
        Command::Verify { job } => {
            let (http, token, host) = auth::connect()?;
            hash_tree::verify(&http, &host, &Zeroizing::new(token), &job)
        }
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}
