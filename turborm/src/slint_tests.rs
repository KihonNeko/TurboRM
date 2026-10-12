use crate::slint_generated::{AppWindow, FolderTarget, LibraryItem};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

// The testing backend owns process-global Slint state, so callback/model and
// cross-thread delivery checks intentionally share one test.
#[test]
fn generated_library_contract_and_event_loop_delivery() {
    i_slint_backend_testing::init_integration_test_with_system_time();

    let window = AppWindow::new().expect("create AppWindow with the headless backend");

    let selected = Rc::new(RefCell::new(String::new()));
    let selected_from_callback = selected.clone();
    window.on_select_entry(move |id| {
        *selected_from_callback.borrow_mut() = id.into();
    });

    let entries = Rc::new(VecModel::from(vec![LibraryItem {
        entry_id: "document-1".into(),
        title: "First notebook".into(),
        subtitle: "Document".into(),
        is_folder: false,
        has_content: false,
        pinned: true,
        selected: false,
        cover: Default::default(),
        cover_ready: false,
        warning: SharedString::default(),
        type_label: "Notebook".into(),
        tile_width: 0.0,
    }]));
    window.set_entries(ModelRc::from(entries.clone()));

    assert_eq!(window.get_entries().row_count(), 1);
    assert_eq!(
        window
            .get_entries()
            .row_data(0)
            .expect("first projected entry")
            .entry_id,
        "document-1"
    );
    window.invoke_select_entry("document-1".into());
    assert_eq!(&*selected.borrow(), "document-1");

    entries.push(LibraryItem {
        entry_id: "folder-1".into(),
        title: "Folder".into(),
        subtitle: "Folder".into(),
        is_folder: true,
        has_content: false,
        pinned: false,
        selected: false,
        cover: Default::default(),
        cover_ready: false,
        warning: SharedString::default(),
        type_label: "Folder".into(),
        tile_width: 120.0,
    });
    assert_eq!(window.get_entries().row_count(), 2);

    let breadcrumbs = Rc::new(VecModel::from(vec![FolderTarget {
        entry_id: SharedString::default(),
        title: "Library".into(),
    }]));
    window.set_breadcrumbs(ModelRc::from(breadcrumbs));
    assert_eq!(window.get_breadcrumbs().row_count(), 1);

    crate::library_ui::test_controller_contract(&window);
    crate::library_ui::test_structure_contract(&window);
    crate::library_ui::test_folder_layout_contract(&window);
    crate::library_ui::test_pairing_contract(&window);
    crate::library_ui::test_viewer_contract(&window);
    crate::library_ui::test_drag_and_export_contract(&window);
    crate::library_ui::test_new_folder_contract(&window);
    crate::library_ui::test_settings_contract(&window);

    let ui_thread = std::thread::current().id();
    let ran_on_ui_thread = Arc::new(AtomicBool::new(false));
    let ran_from_worker = ran_on_ui_thread.clone();
    let weak = window.as_weak();
    let worker = std::thread::spawn(move || {
        weak.upgrade_in_event_loop(move |window| {
            ran_from_worker.store(std::thread::current().id() == ui_thread, Ordering::SeqCst);
            window.set_status_text("worker result delivered".into());
            let _ = slint::quit_event_loop();
        })
        .expect("queue worker result on the Slint event loop");
    });

    slint::run_event_loop().expect("run headless Slint event loop");
    worker.join().expect("worker thread completes");

    assert!(ran_on_ui_thread.load(Ordering::SeqCst));
    assert_eq!(window.get_status_text(), "worker result delivered");
}
