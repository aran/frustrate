//! A bridge whose Rust calls JavaScript the app supplies, through an import
//! namespace of its own (tests/dart_integration/test/host_imports_web_test.dart).
//! `test_host.js` implements the namespace.

use frustrate::bridge;

#[link(wasm_import_module = "frustrate_test_host")]
extern "C" {
    /// Writes the `actor` this realm's `attach` was given into `buf` as JSON,
    /// and answers its length, or -1 when it was null.
    fn actor_json(buf: *mut u8, cap: usize) -> i32;
    /// 1 once this realm's `attach` has finished, 0 before, and 0 if the
    /// realm posted to the page while `attach` ran.
    fn attached() -> i32;
    /// Replaces the realm's file with `len` bytes at `ptr`; answers how many
    /// were written, or -2 where the realm has no file.
    fn store(ptr: *const u8, len: usize) -> i32;
    /// Reads the realm's file into `buf`; answers how many bytes, or -2 where
    /// the realm has no file.
    fn load(buf: *mut u8, cap: usize) -> i32;
}

fn realm_actor() -> Option<String> {
    let mut buf = vec![0u8; 256];
    let n = unsafe { actor_json(buf.as_mut_ptr(), buf.len()) };
    if n < 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8(buf).expect("UTF-8 from the host"))
}

/// The `actor` the page instance's `attach` was given, as JSON.
#[bridge(sync)]
pub fn page_actor() -> Option<String> {
    realm_actor()
}

/// The `actor` seen by the instance that runs an async fn: a pool worker's on
/// threaded wasm, the page's otherwise.
#[bridge]
pub async fn async_actor() -> Option<String> {
    realm_actor()
}

/// An actor whose host functions reach a file in its Worker.
#[bridge(actor)]
pub struct Files {
    attached_at_construction: bool,
}

#[bridge]
impl Files {
    pub fn new() -> Self {
        Files {
            attached_at_construction: unsafe { attached() } == 1,
        }
    }

    /// Whether `attach` had finished before the constructor ran, with nothing
    /// posted to the page while it ran.
    pub fn attached_at_construction(&self) -> bool {
        self.attached_at_construction
    }

    /// The `actor` this instance's `attach` was given, as JSON.
    pub fn actor(&self) -> Option<String> {
        realm_actor()
    }

    /// Write `data` to the file and read it back, both synchronously, within
    /// this one call. Answers what was read.
    pub fn round_trip(&mut self, data: Vec<u8>) -> Vec<u8> {
        let written = unsafe { store(data.as_ptr(), data.len()) };
        assert_eq!(written, data.len() as i32, "store answered {written}");
        let mut buf = vec![0u8; data.len() + 16];
        let read = unsafe { load(buf.as_mut_ptr(), buf.len()) };
        assert!(read >= 0, "load answered {read}");
        buf.truncate(read as usize);
        buf
    }
}
