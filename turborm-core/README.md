# turborm-core

The reMarkable Cloud library behind [TurboRM](../README.md), plus a small command-line client, `turborm-cli`. It browses the cloud library, downloads native documents, exports original PDFs/EPUBs, uploads new PDFs and creates folders. The [desktop app](../turborm/README.md) adds browsing with covers, a page viewer, moves, EPUB/image import and rendered exports; the CLI stays useful for scripting, diagnostics and upload-journal recovery.

This is experimental software that talks to an undocumented cloud protocol. Keep backups of anything important.

## Build

Requires Rust 1.89 or newer. From the repository root:

```sh
cargo build --release --locked -p turborm-core
./target/release/turborm-cli --help
```

Pairing needs an unlocked Secret Service keyring (GNOME Keyring, or KWallet with Secret Service enabled).

## Use

### Pair

```sh
turborm-cli pair
turborm-cli status
```

`pair` prints the reMarkable pairing-page URL and reads a fresh one-time code locally, without echo. It registers a new software client and stores its registration token in the system keyring (service `remarkable-send`, account `device-token`; the name is kept for compatibility with earlier builds). The desktop app shares this credential. Replacing an existing pairing requires `pair --replace`; a failed registration keeps the previous credentials. No code or token belongs in chat or command arguments.

### Browse and download (cloud read-only)

```sh
turborm-cli list
turborm-cli list --folder root
turborm-cli list --include-trash --out library.json
turborm-cli info <document-UUID>
turborm-cli export <document-UUID> --out original.pdf
turborm-cli pull <document-UUID> --out native-document
```

- `list` (alias `ls`) shows the library with document IDs, type, size and folder path. Trashed entries are hidden unless `--include-trash` is given. `--folder <folder-UUID> --recursive` includes descendants. `--json` writes structured output to stdout; `--out` saves JSON to a **new** file.
- `info` shows metadata, content settings and every cloud component's path, size and checksum.
- `export` downloads the **original PDF or EPUB** with checksum and length verification. It does not render handwriting into the file; the desktop app does that.
- `pull` downloads **all native components** of one entry (metadata, content, PDF/EPUB source and `.rm` handwriting files) plus a `download.json` inventory into a new directory.

These commands run through a GET-only transport guard: they cannot upload, delete or publish. Local output never overwrites an existing file or directory. Pull/export default to a 512 MiB limit per component (`--max-bytes`).

Verified document manifests and metadata are cached under `~/.cache/remarkable-send/cache` (or `$XDG_CACHE_HOME`). The cache holds document titles and metadata, never credentials. `--no-cache` bypasses it and `--cache-dir <directory>` moves it.

### Upload new PDFs

```sh
turborm-cli upload ~/path/document.pdf
```

Or prepare the job offline first and send it explicitly:

```sh
turborm-cli prepare ~/path/document.pdf --out my-upload --title "My document"
turborm-cli send my-upload
```

`prepare` makes no network requests. The job holds the unchanged PDF, generated metadata/content files and `job.json`. Default `upload` jobs are kept under `~/.local/state/remarkable-send/jobs`. If an upload stops or its result is uncertain, check it with:

```sh
turborm-cli verify <job-directory>
```

Verification only reads. It compares the exact entry, component paths, sizes and hashes. A verified cloud copy does not prove the tablet has synced it yet.

### Create folders

```sh
turborm-cli mkdir "Reading list"
turborm-cli mkdir "Chapter 2" --parent <folder-UUID>
turborm-cli mkdir -p "Claude Review/TurboRM"
```

`mkdir` adds one folder and prints its ID. A live folder with the same name under the same parent (ignoring case) is refused. `mkdir -p` reuses existing folders and creates only the missing ones.

### Diagnostics

`diagnose` and `inspect-tree` print only structural information (hosts, status codes, schema), never document data or tokens.

## How writes stay safe

Every write uploads content-addressed blobs, reads each one back and checks its hash, then publishes a new root with the generation it started from. If someone else (usually the tablet) published in between, the server refuses with HTTP 412 and nothing is overwritten. Every existing root entry is carried over unchanged. Only folder creation retries a conflict, from a fresh snapshot, at most three times; uploads are journaled as `sending` before the first write and are never replayed automatically.

## Limits

- Uploads are PDF only, up to 100 MiB and 10,000 pages; encrypted PDFs are rejected. Uploads from the CLI go to the root folder.
- No overwrite, delete, rename, notebook editing or device-direct (USB/Wi-Fi) transfer.
- Schema-4 cloud roots only; older schemas fail explicitly.
- Replacing local credentials does not revoke the old registered client; remove it from your reMarkable account settings if needed.

## Tests

```sh
cargo test --locked -p turborm-core
```

The tests use synthetic PDFs and mocked responses. They never pair, read saved credentials or contact an account. A one-page synthetic smoke-test PDF is in `examples/transfer-test.pdf`.
