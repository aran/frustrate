//! A patchable library built for release is the plain library.

#[test]
fn a_release_build_has_no_patch_loader_and_no_slots() {
    let root = std::env::var("RUNFILES_DIR")
        .or_else(|_| std::env::var("TEST_SRCDIR"))
        .expect("runfiles");
    let paths = std::env::var("RELEASE_LIBRARY").expect("RELEASE_LIBRARY");
    let path = paths.split_whitespace().find(|p| p.ends_with(".dylib")).expect("the library");
    let bytes = std::fs::read(std::path::Path::new(&root).join(path)).unwrap();
    let image = frustrate_hotpatch::image::Image::read(&bytes).unwrap();

    assert!(image.exported.contains("frustrate_call_sync"), "the entry points are exported");
    let patch_exports: Vec<&String> = image
        .exported
        .iter()
        .filter(|s| s.starts_with("flutter_hot_patch_") || s.starts_with("frustrate_hot_"))
        .collect();
    assert!(patch_exports.is_empty(), "release exports {patch_exports:?}");
    let slots = image.symbols.keys().filter(|s| s.contains("frustrate_hot_slot_")).count();
    assert_eq!(slots, 0, "release defines hot patch slots");
}
