# TurboRM

TurboRM is an independent, unofficial project. It is not made, endorsed or supported by reMarkable AS. "reMarkable" is used only to describe the tablet and cloud service it works with.

TurboRM is a Rust desktop client for Linux built on the [`turborm-core`](../turborm-core/README.md) cloud library. No delete or trash action is exposed.

The interface uses Slint 1.18.1, the Cosmic widget style, and Skia rendering. The library browser, the pairing screen and the document viewer are all Slint in a single `turborm` executable.

Screenshots use offline demo data: [library](screenshots/library-slint.png), [list view](screenshots/library-slint-list.png), [dark theme](screenshots/library-slint-dark.png), [narrow window](screenshots/library-slint-narrow.png), [page viewer](screenshots/viewer.png), [long canvas](screenshots/long-canvas.png), [landscape](screenshots/landscape.png), [first-launch pairing](screenshots/pairing.png), [new folder dialog](screenshots/new-folder.png).

## Run

Build with Rust 1.92 or newer, a C++ toolchain, CMake, pkg-config, and Linux graphics development libraries. Skia downloads a matching prebuilt archive when available; a source build additionally needs Clang and Python. On Debian/Ubuntu:

```sh
sudo apt install build-essential cmake pkgconf libfontconfig-dev libfreetype-dev libgl-dev libegl-dev libwayland-dev libxkbcommon-dev poppler-utils util-linux
```

Slint's Winit backend includes Wayland and X11 support; Qt is not required. The PDF viewer uses Poppler's `pdftoppm` under `prlimit`; on Debian/Ubuntu install `poppler-utils` and `util-linux`. File dialogs require your desktop's XDG portal and a file chooser backend. Pairing requires an unlocked Secret Service provider, such as GNOME Keyring or KWallet with Secret Service enabled.

```sh
./run-desktop.sh
```

Or run `./run-desktop.sh` from the repository. It builds/runs the optimized release profile when Cargo is available, otherwise a previously built binary. For an optimized binary:

```sh
cargo build -p turborm --release --locked
./target/release/turborm
```

`turborm.desktop` is an optional application-menu entry; its `Exec` expects the binary on your PATH. The release download's `install.sh` installs the binaries, this entry (with an absolute `Exec`) and the icons into `~/.local`; building from source installs nothing. The menu icon is `icons/turborm.svg`; to use it, copy it to `~/.local/share/icons/hicolor/scalable/apps/turborm.svg`. `icons/turborm-small.svg` drops the small "RM" for 16–24 px icons; render it to PNG in `hicolor/16x16`, `22x22` and `24x24`. The window icon is built into the binary from `icons/turborm.png`.

## Build environment

The repository is a **Rust 1.92+ workspace** with a shared root `Cargo.toml` and shared `target/` directory. The build cache can grow large (several GB); run `cargo clean` at the repository root to clear it. One release build uses about 2.1 GB during compilation.

## Use

On first launch, follow the pairing-page link and enter the eight-character code in the app. The one-time code is exchanged for credentials; it is not saved. Device and renewable user tokens are stored in Linux Secret Service, under the `remarkable-send` service name shared with `turborm-cli`. A locked/unavailable keyring produces an error and a retry action, never a plaintext fallback or an automatic replacement pairing.

- Browse compact folder chips and a grid of document covers, or switch to the list. Covers follow the configured page or last-opened page, falling back to the first active page. Very long pages show their top section in the cover. Double-click to open; select an item for move/export actions.
- Opening and page changes paint immediately. `Command::Open` switches the viewer before anything is downloaded, showing the title, the page count and the library's cached cover as a placeholder, with "Opening…", "Downloading N MB…" and "Rendering page N…" in the status bar. A page change keeps the previous page on screen, dimmed, until the replacement arrives — the canvas is never blank. The first paint is a cheap 600-pixel overview; tiles supply the detail, rendered centre-out across the worker pool. The last four pages stay cached, so going back is instant, and the page after the one shown is fetched and rendered ahead on an idle step that always yields to user actions.
- The viewer opens in the same window at fit-width, with horizontal and vertical scrolling, fit-page, 100% and stepped zoom. Ctrl and the mouse wheel zoom; Left/Right and PageUp/PageDown change pages; `+`/`-` step the zoom, `0` fits the page and `1` sets 100%. "Library" returns with the selection and folder intact. Long, wide, and negative-origin ink canvases keep their supported content; visible tiles provide detail without allocating one enormous image.
- Import a PDF, EPUB, PNG, or JPEG into the current folder. Images become one-page PDFs. A new document identity is created; existing documents are never replaced.
- Move a selected document or folder to another folder or the library root.
- Create a folder with **New folder**, the first toolbar action on the right, in the folder being browsed; or right-click a folder tile and choose **New folder inside…**. The action is available while browsing "My files" (not under a sidebar filter), when idle and paired. The dialog checks the name as you type, in Rust: it must not be empty, longer than 255 characters or contain "/", and no folder with the same name (ignoring case) may exist in that place. Enter creates, Escape cancels. The status line shows "Creating folder…" and then "Folder created", and the new folder is selected. Creation only adds one entry. In the offline demo the toolbar action is off; `--demo-new-folder` opens the dialog, and a folder created there exists only in memory.
- **Settings**, at the foot of the sidebar, chooses how much of the library is kept on disk: more space, faster opening. **Small** keeps up to 100 MB of recently opened documents; **Standard** (the default) up to 500 MB; **Full library** downloads every document in the background after each listing and keeps them up to date, so nothing waits for a download once the first pass is done. The full download stands aside while the app is busy with anything you asked for, stops as soon as another mode is chosen, never leaves less than 2 GB of the disk free, and removes old versions of edited pages and deleted documents. Its progress shows in the dialog and next to "Connected" in the sidebar. The dialog also shows the disk used and, once known, the library's size, and **Clear** removes the downloaded documents (not in Full library mode). The choice is saved in `~/.config/turborm/settings.json` (or under `$XDG_CONFIG_HOME`) and takes effect at once; a smaller mode trims the cache straight away. `--demo-settings` opens the dialog in the offline demo, which never reads or writes the file.
- Export the untouched original PDF/EPUB, or all native notebook components to a new bundle directory.
- From an open document, export the displayed page as SVG or the document as a **preview PDF**. Preview PDFs contain rasterized pages; native notebook files remain unchanged.
- Use **Check import** with a retained job directory to reconcile an interrupted upload before attempting another import.

Local exports refuse to overwrite existing files. Cloud mutations are initiated only by import, move or New folder; browsing and opening use the existing read-only transport. Rename, delete and trash are not available.

## Review on the tablet

`turborm review` is a command-line mode of the same executable. It never opens a window and needs no display. It sends a PDF to the tablet for handwritten review, then brings the annotations back as images and text. A Claude Code skill (`~/.claude/skills/review-on-remarkable`) drives it, but it works on its own too:

```sh
turborm review send plan.pdf --title "Plan (review 2026-09-23)" --source plan.md
turborm review status <id>
turborm review fetch <id> --out ./review-<id>
```

- `send` adds one new document through the same path as Import: `prepare_import`, the schema-4 upload, and the byte-for-byte read-back that Verify import runs. It never changes, moves or deletes an entry. `--folder "Claude Review/TurboRM"` names the destination as a path: each existing folder on it is reused (matched without case) and the missing ones are created, which is the only other thing `send` can add. Without `--folder` the document goes into an existing top-level folder named "Claude Review" (matched without case), or to the library root when there is none; nothing is created then. The command prints JSON (`id`, `title`, `parent`, `folder`, `uploaded_at`, `document_hash`, `source`, `job`) and saves the same JSON as `~/.local/state/turborm-review/<id>.json` (`--state-dir` changes the directory). Progress goes to stderr, so stdout stays pure JSON.
- `status` only reads. It prints whether the document exists, its `lastModified` and hash, how many handwriting pages (`.rm`) it has, and `changed_since_upload`: true when the hash moved or any handwriting page exists. Use it to tell whether the tablet has synced.
- `fetch` only reads. It pulls the document (bounded and hash-verified, like native export) and renders every page with the viewer's renderer: the PDF background, ink, highlights, typed text, inserted images and templates. Pages the user added on the tablet are included. The output is a **new** directory; an existing one is never touched. For each page N it writes `page-NNN.png` (default longest side 2200 px, `--longest-side` 400–4000). When the page has annotations, it also writes `page-NNN-ink.png`, the annotations alone on white in the same geometry, so handwriting is readable without the printed text under it. `annotations.json` lists, per page: `annotated`; `highlights` (the exact highlighted strings, with their colour); `typed_text`; `colors` (stroke counts by pen colour, e.g. `red`, `highlighter yellow`); `ink_bounds`, one `[x, y, w, h]` box in page-image pixels for each group of nearby marks, plus `ink_bbox` around them all; `inserted_page`; and the page's warnings and notes. `summary.md` gives the same information in reading order. A long page (more than 1.5 times taller than wide, such as the skill's single scrolling review page) is rendered by width instead: as wide as a 3:4 page of the chosen size and as tall as the page needs. It is also cut into screen-sized parts that overlap by an eighth, `page-NNN-part-KK.png` and, for parts with marks, `page-NNN-ink-part-KK.png`; each entry in the page's `parts` gives its `top`, `height` and `ink_bounds` in that part's pixels.

The annotations-only image comes from `Document::render_annotations`, which composes the page without the PDF background and template but keeps the background's placement in the canvas bounds. `Document::still` rasterizes a whole page at up to 4000 px, uncached. The viewer, covers and exports do not use either path.

## Rendering scope

The notebook parser is independently implemented in Rust, with bounded v3/v5 decoding and v6 scene decoding, including both point layouts. All **107 locally available real pages** render with zero failures. Typed text is decoded exactly and laid out approximately; paragraph styles, bullets, checkboxes, and text highlights are implemented. Brush approximations (highlighter multiply, pencil pressure opacity, marker, paintbrush, calligraphy) are implemented. Anchored transform offsets and group-local eraser masks are implemented. It is a first viewer, **not full Windows/macOS rendering parity**. Text wrapping can differ by 1–2 characters from the tablet because reMarkable Sans is not redistributable. Anchored offsets are the stored values only. Scene image items are not drawn (every copied asset stores empty image tables). Group references and left/right constraints determine paint order; unresolved concurrent ties are reported. Tool 6 uses a mask within its group, and tool 8 is recognized as non-geometric eraser control. Legacy eraser behavior remains approximate. Canvas bounds include supported visible ink beyond every edge of the base page, including negative coordinates. Landscape notebook orientation and actual PDF aspect ratios are preserved. Unsupported versions fail explicitly. The viewer shows rendering limitations rather than claiming a lossless visual export. Field interpretations were also cross-checked against [rmscene's published format reader](https://github.com/ricklupton/rmscene/blob/main/src/rmscene/scene_stream.py); no external reMarkable renderer or Python runtime is used by the app.

**PDF background geometry** is resolved. The PDF page is placed at physical size (227/72 scene units per point), horizontally centred on scene x=0, with its top at y=0. The same helper is used by covers, viewer tiles, SVG, and preview PDF exports. Rotated/cropped/UserUnit/landscape cases are unit-tested but not tablet-verified; the background raster budget was not raised so backgrounds are softer at high zoom on large pages.

PDF backgrounds use Poppler in a local subprocess with fixed arguments, no shell, a cleared environment, a timeout, and memory/CPU/output limits. SVG rasterization and notebook parsing run in Rust. SVG exports embed any PDF background as a PNG and preserve supported ink as vector strokes.

EPUB imports preserve the original book but require the tablet to generate and sync a PDF/page representation before this viewer can display it. EPUB upload acceptance and the new move flow still need live account/tablet validation. No live mutations are made by tests.

Preview downloads are limited to 512 MiB per document. Raster PDF export retains the complete supported canvas using 512-pixel tiles, with limits of 200 pages, 4,096 tiles, and 256 MiB of compressed images. Very long PDF pages are proportionally scaled to fit portable PDF dimensions. Original-file and native exports remain available for larger documents. Native previews use private temporary directories removed when the document is replaced or the app exits normally; a crash may leave a private temporary directory. Verified metadata caches, private cover PNG caches, and import journals contain document data, not credentials. Opening downloads the first scene and any shared PDF background; subsequent pages are downloaded when visited, plus at most one page of read-ahead. Downloaded document files are kept in a checksum-verified cache under `~/.cache/remarkable-send/cache/components`, limited by the storage setting (100 MB, 500 MB by default, or the whole library) with the least recently used files removed first, so reopening a document does not download it again (a file larger than half the limit is not kept). Where the next page follows the current one in the source PDF, both backgrounds come from a single `pdftoppm` invocation inside the unchanged prlimit sandbox (cleared environment, address/CPU/file-size/descriptor limits, per-page timeout and 32 MiB read bound). The viewer and covers take `pdftoppm`'s raw PPM output, which is several times faster than PNG. PDF backgrounds are rasterized at 1800 pixels on the longest side; when a tile needs more detail than that (a long page shown at full width, or any page zoomed in), the tile renders its part of the PDF page again at its own resolution, in 4096 x 2048 bands made in the same sandbox and kept in memory (4 bands at most). A Poppler run costs about the same whatever its size, because it decodes the page's whole scanned image, so bands are large, and the bands a screen needs start together as soon as its tiles are requested. PDF page sizes come from `pdfinfo -box` in the same sandbox, which reads only the page tree (0.02 s for a 368 MB scan that took 10 s to parse in full); a PDF that declares `/UserUnit`, which Poppler does not report, or that `pdfinfo` cannot read, is parsed in full with lopdf instead. A preview PDF export fetches every active page before rendering. Covers are keyed by the immutable document hash and only the selected scene/background is downloaded for rendering. Local template configuration bypasses the cover cache so changed paper assets take effect.

## Local paper templates

Paper templates render without any configuration. Template asset files are
discovered automatically, in this order, and the first directory holding
`<name>.template` wins:

1. `REMARKABLE_TEMPLATES_DIR`, when it is set and absolute
2. `$XDG_DATA_HOME/remarkable-desktop/templates`
3. `~/.local/share/remarkable-desktop/templates`

No template asset is bundled with this application. To use the assets from your
own firmware copy, put them in one of those directories:

```sh
mkdir -p ~/.local/share/remarkable-desktop/templates
cp /path/to/usr/share/remarkable/templates/* ~/.local/share/remarkable-desktop/templates/
```

`REMARKABLE_TEMPLATES_DIR` still works for an existing setup:

```sh
REMARKABLE_TEMPLATES_DIR=/absolute/path/to/templates ./run-desktop.sh
```

When no asset file is found, the standard geometric papers are drawn
procedurally instead: Blank, `P Dots S` (and its top/bottom variants),
`P Dots large`, `P Lines small/medium/large`, `P Grid small/medium/large`,
`P Margin small/medium/large`, and the `LS Dots`, `LS Lines`, `LS Grid` and
`LS Margin` variants. Their spacings, offsets and dot sizes were measured from
the firmware's own template JSON; no firmware file is copied into this
repository. A template with no generator and no asset file reports a single
note, `Template "X" is not available locally`, and the page renders without
paper rather than failing.

Every safety check is unchanged: the directory must be absolute, the asset must
be a regular non-symlink file within 2 MiB, and the bounded interpreter caps
items, instances, path parts and output size. The bounded interpreter supports
path templates (55 of the 65 distinct assets in the copied firmware); the ten
containing text items are reported rather than partially drawn.

The selected page template comes from the document's `cPages` register or its
legacy `.pagedata`. Blank pages need no asset. A page with a PDF background
draws the PDF page instead of a paper template.

## Rendering notes and warnings

The viewer's strip separates two kinds of entry:

- **Warnings** are content problems: something in the file could not be
  rendered faithfully (an undrawable image item, an unsupported tool, a
  template that could not be produced, overlapping items whose order could not
  be resolved). Only these are counted, and the count is shown in the warn
  colour.
- **Notes** are approximations and standing facts about this reader: the brush
  texture simulation, approximate typed-text layout, and stored anchor offsets.
  They are listed under a muted `Notes` heading and never counted. When a page
  has only notes, the collapsed strip reads `Notes` with no coloured number.

Identical entries are collapsed, so a page where forty groups share one problem
shows it once as `… (×40)`. Export advice is not a page note: the rasterized
preview-PDF caveat is the export button's description and the status-bar message
shown after the export completes.

## Structure

```text
turborm/ui/              Slint library, pairing and viewer screens
turborm/ui/theme.slint   light/dark design system, palettes, components
turborm/src/library_state.rs Rust library state and navigation rules
turborm/src/library_ui.rs Slint model projection and callbacks
turborm/src/viewer_state.rs Rust page, zoom, visible tiles and tile cache
turborm/src/pdf_geometry.rs PDF placement transform, shared by all renders
turborm/src/service.rs   typed commands, foreground/cover/offline workers, OS dialogs
turborm/src/settings.rs  saved user settings (storage mode)
turborm/src/renderer.rs  page rendering, covers, tiles, SVG, preview PDF
turborm/src/review.rs    review CLI arguments, upload state, fetch rendering
turborm/src/review_cli.rs `turborm review` cloud session (send/status/fetch)
turborm/src/templates.rs bounded local paper-template interpreter
turborm-core/src/scene_text.rs   typed text decoding, layout, styling
turborm-core/src/auth.rs         pairing and system credential storage
turborm-core/src/library.rs      immutable snapshots and verified downloads
turborm-core/src/moves.rs        validated copy-on-write cloud moves
turborm-core/src/import.rs       offline PDF/EPUB/image job preparation
turborm-core/src/notebook.rs     bounded v6 scene decoding, group masks
turborm-core/src/legacy_notebook.rs bounded v3/v5 notebook reader
turborm-core/src/cover.rs        cover/last-opened page selection
turborm-core/src/hash_tree.rs    verified uploads and generation-guarded publication
```

Rust owns library state and `Rc<VecModel<T>>` models; Slint exposes properties and callbacks. Worker notifications use `Weak::upgrade_in_event_loop`. The UI passes commands and receives plain results through channels. It has no HTTP client or tokens. A foreground worker keeps network calls, dialogs, downloads, and page rendering off the UI thread; a separate bounded worker fetches covers; and a small pool (at most four threads, never more than the machine's parallelism) rasterizes viewer tiles, so a refresh or an export can no longer stall the visible canvas. The viewer keeps at most 64 tile textures, the document keeps its last four rendered pages, and the library keeps at most 128 cover textures. The existing CLI remains separately buildable and shares the core library. Mutations preserve unknown document metadata and unrelated root rows; conflicting root generations fail without automatic write retries.

## Check

```sh
cargo test -p turborm-core
cargo test -p turborm
./run-desktop.sh --demo
./run-desktop.sh --demo-stress
./run-desktop.sh --demo-pairing
./run-desktop.sh --demo-viewer
./run-desktop.sh --demo-long
./run-desktop.sh --demo-landscape
./run-desktop.sh --demo-new-folder [--demo-folder-name <text>]
./run-desktop.sh --demo-settings
```

Add `--demo-narrow`, `--demo-list`, `--demo-folders` or `--demo-dark` to a demo, or `--screenshot <path>` to capture the window and exit.

Desktop rendering tests use Poppler and synthetic local PDFs. Demo mode uses synthetic library entries and never pairs or contacts the cloud. Tests cover core protocol preservation, credential error handling, import validation, folder cycles, stale destinations, scene/page parsing, PDF page redirection, and refusing export overwrites.

On Debian 13 with Rust 1.98.1: **275 tests (171 core, 73 desktop lib, 31 desktop bin), all passing**, with Clippy clean. No hand-written unsafe code; Slint-generated bindings have a narrowly scoped exception for toolkit internals. A release build produces a ~40 MB executable. Library, viewer and pairing windows were checked on GNOME Wayland and X11/XWayland; other desktops are untested. A read-only live check loaded a library of 1,370 entries with zero failures. Rendering parity for rotated/cropped/landscape PDFs still needs wider testing.

Standard dependency API references: [slint](https://docs.rs/slint/1.18.1/slint/), [resvg](https://docs.rs/resvg/0.45.1/resvg/), and [keyring](https://docs.rs/keyring/3.6.3/keyring/).
