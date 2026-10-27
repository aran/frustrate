//! A patchable library built for release is the plain library, whether it holds
//! one bridge or several.

#[test]
fn a_release_build_has_no_patch_loader_and_no_slots() {
    let root = std::env::var("RUNFILES_DIR")
        .or_else(|_| std::env::var("TEST_SRCDIR"))
        .expect("runfiles");
    let paths = std::env::var("RELEASE_LIBRARY").expect("RELEASE_LIBRARY");
    let libraries: Vec<&str> = paths.split_whitespace().filter(|p| p.ends_with(".dylib")).collect();
    let cases = [
        ("libhotpatch_fixture.dylib", &["frustrate_hotpatch_fixture_call_sync"][..]),
        (
            "libcomposed.dylib",
            &["frustrate_hotpatch_alpha_call_sync", "frustrate_hotpatch_beta_call_sync"][..],
        ),
    ];
    for (file, entry_points) in cases {
        let path = libraries
            .iter()
            .find(|p| p.ends_with(&format!("/{file}")))
            .unwrap_or_else(|| panic!("{file} among {libraries:?}"));
        let bytes = std::fs::read(std::path::Path::new(&root).join(path)).unwrap();
        let image = frustrate_hotpatch::image::Image::read(&bytes).unwrap();

        for e in entry_points {
            assert!(image.exported.contains(*e), "{file}: {e} is exported");
        }
        let patch_exports: Vec<&String> = image
            .exported
            .iter()
            .filter(|s| s.starts_with("flutter_hot_patch_") || s.starts_with("frustrate_hot_"))
            .collect();
        assert!(patch_exports.is_empty(), "{file}: release exports {patch_exports:?}");
        let slots = image.symbols.keys().filter(|s| s.contains("frustrate_hot_slot_")).count();
        assert_eq!(slots, 0, "{file}: release defines hot patch slots");
    }
}
