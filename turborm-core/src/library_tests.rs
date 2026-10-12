use super::*;
use serde_json::json;

const DOC: &str = "11111111-1111-4111-8111-111111111111";
const FOLDER: &str = "22222222-2222-4222-8222-222222222222";
const OTHER: &str = "33333333-3333-4333-8333-333333333333";

struct Mock {
    root: Value,
    blobs: Mutex<HashMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
}
impl Transport for Mock {
    fn request(
        &self,
        _host: &str,
        method: &str,
        path: &str,
        _token: Option<&str>,
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Vec<u8>> {
        assert_eq!(
            method, "GET",
            "Library commands must never mutate cloud state"
        );
        assert!(body.is_none());
        self.calls.lock().unwrap().push(path.to_owned());
        if path == "/sync/v4/root" {
            return Ok(serde_json::to_vec(&self.root)?);
        }
        let hash = path.strip_prefix("/sync/v3/files/").unwrap();
        let bytes = self
            .blobs
            .lock()
            .unwrap()
            .get(hash)
            .ok_or_else(|| anyhow!("Missing mock blob"))?
            .clone();
        if hash == self.root["hash"].as_str().unwrap() {
            assert!(headers
                .iter()
                .any(|(n, v)| *n == "rm-filename" && v == "root.docSchema"));
        }
        ensure!(bytes.len() <= limit, "Mock size limit");
        Ok(bytes)
    }
}
fn server(kind: &str) -> Mock {
    let mut blobs = HashMap::new();
    let mut rows = Vec::new();
    let components=vec![
        (format!("{DOC}.metadata"),serde_json::to_vec(&json!({"type":"DocumentType","parent":FOLDER,"visibleName":"Same title","pinned":true})).unwrap()),
        (format!("{DOC}.content"),serde_json::to_vec(&json!({"fileType":kind,"pageCount":12})).unwrap()),
        (format!("{DOC}/{OTHER}.rm"),b"native handwriting bytes".to_vec()),
        (format!("{DOC}.pagedata"),b"Blank\n".to_vec()),
    ];
    for (name, bytes) in components.into_iter().chain(if kind == "notebook" {
        None
    } else {
        Some((
            format!("{DOC}.{kind}"),
            format!("original {kind} bytes").into_bytes(),
        ))
    }) {
        let hash = job::hash(&bytes);
        rows.push(Row {
            hash: hash.clone(),
            name,
            flags: 0,
            count: 0,
            size: bytes.len() as u64,
        });
        blobs.insert(hash, bytes);
    }
    let doc = Manifest {
        name: DOC.into(),
        flags: 0,
        rows,
    };
    let hash = doc.hash().unwrap();
    blobs.insert(hash.clone(), doc.serialize().unwrap());
    let doc_row = Row {
        hash,
        name: DOC.into(),
        flags: 0,
        count: doc.rows.len() as u32,
        size: doc.total_size().unwrap(),
    };
    let folder_meta = serde_json::to_vec(
        &json!({"type":"CollectionType","parent":"","visibleName":"Same title"}),
    )
    .unwrap();
    let hash = job::hash(&folder_meta);
    let size = folder_meta.len() as u64;
    blobs.insert(hash.clone(), folder_meta);
    let folder = Manifest {
        name: FOLDER.into(),
        flags: 0,
        rows: vec![Row {
            hash,
            name: format!("{FOLDER}.metadata"),
            flags: 0,
            count: 0,
            size,
        }],
    };
    let hash = folder.hash().unwrap();
    blobs.insert(hash.clone(), folder.serialize().unwrap());
    let root = Manifest {
        name: ".".into(),
        flags: 0,
        rows: vec![
            doc_row,
            Row {
                hash,
                name: FOLDER.into(),
                flags: 0,
                count: 1,
                size,
            },
        ],
    };
    let hash = root.hash().unwrap();
    blobs.insert(hash.clone(), root.serialize().unwrap());
    Mock {
        root: json!({"hash":hash,"generation":44,"schemaVersion":4}),
        blobs: Mutex::new(blobs),
        calls: Mutex::new(Vec::new()),
    }
}

fn listing_snapshot(http: &Mock, count: usize, invalid: Option<usize>) -> Snapshot {
    let mut rows = Vec::new();
    for index in 0..count {
        let id = Uuid::from_u128(index as u128 + 1).to_string();
        let metadata = if invalid == Some(index) {
            serde_json::to_vec(&json!({"type":"DocumentType","parent":""})).unwrap()
        } else {
            serde_json::to_vec(&json!({
                "type":"DocumentType",
                "parent":"",
                "visibleName":format!("Entry {index}")
            }))
            .unwrap()
        };
        let metadata_hash = job::hash(&metadata);
        http.blobs
            .lock()
            .unwrap()
            .insert(metadata_hash.clone(), metadata.clone());
        let document = Manifest {
            name: id.clone(),
            flags: 0,
            rows: vec![Row {
                hash: metadata_hash,
                name: format!("{id}.metadata"),
                flags: 0,
                count: 0,
                size: metadata.len() as u64,
            }],
        };
        let document_bytes = document.serialize().unwrap();
        let document_hash = job::hash(&document_bytes);
        http.blobs
            .lock()
            .unwrap()
            .insert(document_hash.clone(), document_bytes);
        rows.push(Row {
            hash: document_hash,
            name: id,
            flags: 0,
            count: 1,
            size: metadata.len() as u64,
        });
    }
    Snapshot {
        generation: 45,
        manifest: Manifest {
            name: ".".into(),
            flags: 0,
            rows,
        },
    }
}

#[test]
fn listing_processes_a_non_multiple_of_the_progress_interval() {
    let http = server("pdf");
    let snapshot = listing_snapshot(&http, 137, None);
    let listing = Library::new(&http, "host", "token", None)
        .list(&snapshot, 4)
        .unwrap();
    assert_eq!(listing.entries.len(), 137);
    assert!(listing.failures.is_empty());
}

#[test]
fn listing_keeps_valid_entries_and_identifies_each_unreadable_entry() {
    let http = server("pdf");
    let snapshot = listing_snapshot(&http, 137, Some(70));
    let failed_id = snapshot.manifest.rows[70].name.clone();
    let listing = Library::new(&http, "host", "token", None)
        .list(&snapshot, 4)
        .unwrap();
    assert_eq!(listing.entries.len(), 136);
    assert_eq!(listing.failures.len(), 1);
    assert_eq!(listing.failures[0].id, failed_id);
    assert!(listing.failures[0].error.contains("Metadata title missing"));
    assert!(listing
        .entries
        .iter()
        .any(|entry| entry.title == "Entry 136"));
}

#[test]
fn lists_folder_paths_and_duplicate_titles_with_stable_ids_and_verified_cache() {
    let http = server("pdf");
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(&http, "host", "token", Some(temp.path().join("cache")));
    let snapshot = library.snapshot().unwrap();
    let mut listing = library.list(&snapshot, 4).unwrap();
    assert_eq!(listing.entries.len(), 2);
    let entry = listing.entries.iter().find(|e| e.id == DOC).unwrap();
    assert_eq!(entry.path, "/Same title/Same title");
    assert!(entry.pinned);
    // The `.content` descriptor rides the same cached, verified blob path as
    // `.metadata`, so the listing knows the file type and page count.
    assert_eq!(entry.file_type.as_deref(), Some("pdf"));
    assert_eq!(entry.page_count, Some(12));
    let folder = listing.entries.iter().find(|e| e.id == FOLDER).unwrap();
    assert_eq!(folder.file_type, None);
    assert_eq!(folder.page_count, None);
    let calls = http.calls.lock().unwrap().len();
    library.list(&snapshot, 2).unwrap();
    assert_eq!(http.calls.lock().unwrap().len(), calls);
    let info = library.info(&snapshot, DOC).unwrap();
    assert_eq!(info.content.unwrap()["fileType"], "pdf");
    filter_listing(&mut listing, Some(FOLDER), false, false).unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].id, DOC);
    // Corrupt a cache object: refetch and verify, do not trust cached data.
    let row = find_row(&snapshot, DOC).unwrap();
    fs::write(temp.path().join("cache").join(&row.hash), b"corrupt").unwrap();
    library.document(row).unwrap();
    assert!(http.calls.lock().unwrap().len() > calls);
}
#[test]
fn original_export_and_native_pull_preserve_every_byte_and_never_overwrite() {
    for kind in ["pdf", "epub"] {
        let http = server(kind);
        let temp = tempfile::tempdir().unwrap();
        let library = Library::new(&http, "host", "token", None);
        let snapshot = library.snapshot().unwrap();
        let out = temp.path().join(format!("source.{kind}"));
        assert_eq!(library.export(&snapshot, DOC, &out, 10000).unwrap(), kind);
        assert_eq!(
            fs::read(&out).unwrap(),
            format!("original {kind} bytes").as_bytes()
        );
        assert!(library.export(&snapshot, DOC, &out, 10000).is_err());
        let bundle = temp.path().join("native");
        assert_eq!(library.pull(&snapshot, DOC, &bundle, 10000).unwrap(), 5);
        assert_eq!(
            fs::read(bundle.join(DOC).join(format!("{OTHER}.rm"))).unwrap(),
            b"native handwriting bytes"
        );
        assert!(bundle.join("download.json").exists());
        assert!(bundle.join(format!("{DOC}.docSchema")).exists());
        assert!(library.pull(&snapshot, DOC, &bundle, 10000).is_err());
        assert!(library
            .export(&snapshot, DOC, &temp.path().join("wrong.txt"), 10000)
            .is_err());
    }
}

#[test]
fn component_reads_only_the_selected_verified_leaf_within_its_document() {
    let http = server("pdf");
    let library = Library::new(&http, "host", "token", None);
    let snapshot = library.snapshot().unwrap();
    let path = format!("{DOC}/{OTHER}.rm");

    assert_eq!(
        library.component(&snapshot, DOC, &path, 100).unwrap(),
        b"native handwriting bytes"
    );
    assert!(library
        .component(&snapshot, DOC, &format!("{DOC}.missing"), 100)
        .is_err());
    assert!(library
        .component(&snapshot, DOC, &format!("{OTHER}.metadata"), 100)
        .is_err());
    assert!(library.component(&snapshot, DOC, &path, 1).is_err());

    let document = library.document(find_row(&snapshot, DOC).unwrap()).unwrap();
    let component = document.rows.iter().find(|row| row.name == path).unwrap();
    http.blobs
        .lock()
        .unwrap()
        .insert(component.hash.clone(), b"tampered component bytes".to_vec());
    assert!(library.component(&snapshot, DOC, &path, 100).is_err());
}
#[test]
fn cached_components_are_reused_verified_and_bounded() {
    let http = server("pdf");
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(&http, "host", "token", Some(temp.path().join("cache")));
    let snapshot = library.snapshot().unwrap();
    let pdf = format!("{DOC}.pdf");
    let scene = format!("{DOC}/{OTHER}.rm");
    let downloads = |http: &Mock| {
        http.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.starts_with("/sync/v3/files/"))
            .count()
    };
    let fetch = |path: &str, budget| {
        library
            .cached_component(&snapshot, DOC, path, 100, budget)
            .unwrap()
    };

    assert_eq!(fetch(&pdf, 1000), b"original pdf bytes");
    let before = downloads(&http);
    // The second read comes from disk, not from the cloud.
    assert_eq!(fetch(&pdf, 1000), b"original pdf bytes");
    assert_eq!(downloads(&http), before);

    // A damaged cache file is never returned; it is downloaded again.
    let components = temp.path().join("cache").join("components");
    for entry in fs::read_dir(&components).unwrap() {
        fs::write(entry.unwrap().path(), b"damaged").unwrap();
    }
    assert_eq!(fetch(&pdf, 1000), b"original pdf bytes");
    assert!(downloads(&http) > before);

    // Past the budget, the least recently used component makes room. Each
    // one fits in half the budget, so each is stored; all three do not fit.
    fs::remove_dir_all(&components).unwrap();
    let content = format!("{DOC}.content");
    let sizes: Vec<u64> = [&pdf, &scene, &content]
        .iter()
        .map(|path| fetch(path, 1000).len() as u64)
        .collect();
    let budget = sizes.iter().sum::<u64>() - 1;
    assert!(sizes.iter().all(|size| *size <= budget / 2));
    fs::remove_dir_all(&components).unwrap();
    for path in [&pdf, &scene, &content] {
        fetch(path, budget);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let kept: u64 = fs::read_dir(&components)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    assert!(kept <= budget, "{kept} > {budget}");
    assert_eq!(kept, sizes[1] + sizes[2], "the oldest, the PDF, went first");
    // Something larger than half the budget is returned but never stored.
    fs::remove_dir_all(&components).unwrap();
    fetch(&scene, sizes[1] * 2 - 1);
    assert!(!components.exists() || fs::read_dir(&components).unwrap().count() == 0);

    // Without a cache directory it is the plain verified download.
    let uncached = Library::new(&http, "host", "token", None);
    assert_eq!(
        uncached
            .cached_component(&snapshot, DOC, &pdf, 100, 1000)
            .unwrap(),
        b"original pdf bytes"
    );
}
#[test]
fn notebook_export_is_explicitly_unsupported_but_native_pull_works() {
    let http = server("notebook");
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(&http, "host", "token", None);
    let snapshot = library.snapshot().unwrap();
    let out = temp.path().join("notebook.pdf");
    let error = library.export(&snapshot, DOC, &out, 10000).unwrap_err();
    assert!(error.to_string().contains("native files"));
    assert!(!out.exists());
    assert_eq!(
        library
            .pull(&snapshot, DOC, &temp.path().join("native"), 10000)
            .unwrap(),
        4
    );
}
#[test]
fn corruption_and_limits_leave_no_completed_download_or_partial_export() {
    let http = server("pdf");
    let temp = tempfile::tempdir().unwrap();
    let library = Library::new(&http, "host", "token", None);
    let snapshot = library.snapshot().unwrap();
    let bundle = temp.path().join("bundle");
    assert!(library.pull(&snapshot, DOC, &bundle, 1).is_err());
    assert!(!bundle.exists());
    let hash = job::hash(b"original pdf bytes");
    http.blobs
        .lock()
        .unwrap()
        .insert(hash, b"corrupted pdf data".to_vec());
    let out = temp.path().join("bad.pdf");
    assert!(library.export(&snapshot, DOC, &out, 10000).is_err());
    assert!(!out.exists());
    assert!(library.pull(&snapshot, DOC, &bundle, 10000).is_err());
    assert!(!bundle.exists());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}
#[test]
fn native_extraction_rejects_traversal_reserved_names_and_collisions() {
    let base = Manifest {
        name: DOC.into(),
        flags: 0,
        rows: vec![Row {
            name: format!("{DOC}.metadata"),
            hash: "ab".repeat(32),
            flags: 0,
            count: 0,
            size: 2,
        }],
    };
    for name in [
        format!("{DOC}/../escape"),
        format!("{DOC}/CON.txt"),
        format!("{DOC}/bad:ads"),
        format!("{DOC}/foo\\bar"),
        format!("{DOC}/tail."),
        format!("{DOC}//x"),
        format!("{OTHER}.pdf"),
        format!("{DOC}.docSchema"),
    ] {
        let mut doc = base.clone();
        doc.rows[0].name = name;
        assert!(validate_paths(&doc, DOC).is_err());
    }
    let mut doc = base.clone();
    let mut row = doc.rows[0].clone();
    row.name = row.name.to_uppercase();
    doc.rows.push(row);
    assert!(validate_paths(&doc, DOC).is_err());
    let mut doc = base;
    doc.rows[0].name = format!("{DOC}/folder");
    let mut row = doc.rows[0].clone();
    row.name.push_str("/file.rm");
    doc.rows.push(row);
    assert!(validate_paths(&doc, DOC).is_err());
}
#[test]
fn hierarchy_handles_trashed_ancestors_missing_parents_and_cycles() {
    fn entry(id: &str, parent: &str) -> Entry {
        Entry {
            id: id.into(),
            parent: parent.into(),
            title: id.into(),
            kind: "folder".into(),
            deleted: false,
            pinned: false,
            modified: None,
            files: 1,
            size: 1,
            path: String::new(),
            hierarchy_warning: None,
            file_type: None,
            page_count: None,
        }
    }
    let mut entries = vec![
        entry(DOC, FOLDER),
        entry(FOLDER, "trash"),
        entry(OTHER, "missing"),
    ];
    resolve_paths(&mut entries);
    assert!(entries[0].deleted);
    assert!(entries[0].path.starts_with("/[Trash]/"));
    assert!(entries[2].hierarchy_warning.is_some());
    let mut listing = Listing {
        generation: 1,
        entries,
        failures: Vec::new(),
    };
    filter_listing(&mut listing, None, false, false).unwrap();
    assert_eq!(listing.entries.len(), 1);
    let mut entries = vec![entry(DOC, FOLDER), entry(FOLDER, DOC)];
    resolve_paths(&mut entries);
    assert!(entries.iter().all(|e| e.hierarchy_warning.is_some()));
}
#[test]
fn descriptor_and_manifest_hash_mismatches_are_rejected() {
    let http = server("pdf");
    let library = Library::new(&http, "host", "token", None);
    let snapshot = library.snapshot().unwrap();
    let row = find_row(&snapshot, DOC).unwrap();
    let mut wrong = row.clone();
    wrong.size += 1;
    assert!(library.document(&wrong).is_err());
    http.blobs
        .lock()
        .unwrap()
        .get_mut(&row.hash)
        .unwrap()
        .push(b'\n');
    assert!(library.document(row).is_err());
}

#[test]
fn file_type_and_page_count_come_from_the_content_descriptor() {
    for kind in ["notebook", "pdf", "epub"] {
        let http = server(kind);
        let library = Library::new(&http, "host", "token", None);
        let snapshot = library.snapshot().unwrap();
        let listing = library.list(&snapshot, 2).unwrap();
        let entry = listing.entries.iter().find(|e| e.id == DOC).unwrap();
        assert_eq!(entry.file_type.as_deref(), Some(kind));
        assert_eq!(entry.page_count, Some(12));
        assert_eq!(
            library.info(&snapshot, DOC).unwrap().entry.page_count,
            Some(12)
        );
    }
}

#[test]
fn content_variants_map_to_known_types_or_stay_unknown() {
    let mut entry = Entry {
        id: DOC.into(),
        title: "x".into(),
        kind: "document".into(),
        parent: String::new(),
        deleted: false,
        pinned: false,
        modified: None,
        files: 0,
        size: 0,
        path: String::new(),
        hierarchy_warning: None,
        file_type: None,
        page_count: None,
    };
    // `cPages.pages` is the newer page list; it is used when pageCount is absent.
    apply_content(
        &mut entry,
        &json!({"fileType": "notebook", "cPages": {"pages": [{}, {}, {}]}}),
    );
    assert_eq!(entry.file_type.as_deref(), Some("notebook"));
    assert_eq!(entry.page_count, Some(3));
    // pageCount wins when both are present.
    apply_content(
        &mut entry,
        &json!({"fileType": "pdf", "pageCount": 9, "cPages": {"pages": [{}]}}),
    );
    assert_eq!(entry.page_count, Some(9));
    // An unexpected file type never reaches the interface as free-form text.
    apply_content(&mut entry, &json!({"fileType": "something-else"}));
    assert_eq!(entry.file_type, None);
    apply_content(&mut entry, &json!({}));
    assert_eq!(entry.file_type, None);
    assert_eq!(entry.page_count, None);
}

#[test]
fn an_unreadable_content_blob_still_yields_the_entry() {
    let http = server("pdf");
    let library = Library::new(&http, "host", "token", None);
    let snapshot = library.snapshot().unwrap();
    let document = library.document(find_row(&snapshot, DOC).unwrap()).unwrap();
    let content = document
        .rows
        .iter()
        .find(|row| row.name == format!("{DOC}.content"))
        .unwrap();
    http.blobs
        .lock()
        .unwrap()
        .insert(content.hash.clone(), b"not json at all".to_vec());
    let listing = library.list(&snapshot, 2).unwrap();
    assert!(
        listing.failures.is_empty(),
        "a bad .content is not a listing failure"
    );
    let entry = listing.entries.iter().find(|e| e.id == DOC).unwrap();
    assert_eq!(entry.file_type, None);
    assert_eq!(entry.page_count, None);
}
#[test]
fn the_component_cache_can_be_measured_trimmed_and_pruned() {
    let http = server("pdf");
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let library = Library::new(&http, "host", "token", Some(cache.clone()));
    let snapshot = library.snapshot().unwrap();
    let rows = library.document_rows(&snapshot, DOC).unwrap();
    let pdf = rows
        .iter()
        .find(|row| row.name == format!("{DOC}.pdf"))
        .unwrap()
        .clone();
    let scene = rows
        .iter()
        .find(|row| row.name.ends_with(".rm"))
        .unwrap()
        .clone();
    assert_eq!(component_cache_bytes(&cache), 0);
    assert!(!is_component_cached(&cache, &pdf.hash, pdf.size));

    for row in [&pdf, &scene] {
        library
            .cached_component(&snapshot, DOC, &row.name, 100, 1000)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        is_component_cached(&cache, &pdf.hash, pdf.size)
            && is_component_cached(&cache, &scene.hash, scene.size)
    );
    assert_eq!(component_cache_bytes(&cache), pdf.size + scene.size);

    // Trimming to a smaller budget drops the least recently used first.
    trim_component_cache(&cache, scene.size).unwrap();
    assert!(!is_component_cached(&cache, &pdf.hash, pdf.size));
    assert!(is_component_cached(&cache, &scene.hash, scene.size));

    // Pruning keeps exactly the hashes it is given; an empty set clears all.
    library
        .cached_component(&snapshot, DOC, &pdf.name, 100, 1000)
        .unwrap();
    let keep = HashSet::from([pdf.hash.clone()]);
    assert_eq!(prune_component_cache(&cache, &keep).unwrap(), scene.size);
    assert!(
        is_component_cached(&cache, &pdf.hash, pdf.size)
            && !is_component_cached(&cache, &scene.hash, scene.size)
    );
    assert_eq!(
        prune_component_cache(&cache, &HashSet::new()).unwrap(),
        pdf.size
    );
    assert_eq!(component_cache_bytes(&cache), 0);
}
