//! `println!` / `eprintln!` for wasm32-unknown-unknown, routed to the console.
//!
//! Installed by toolchain/custom_std as `sys/stdio/frustrate.rs`. The stub it
//! replaces (`sys/stdio/unsupported.rs`) *accepts* every write and returns the
//! byte count, so a `println!` succeeds and vanishes — the quiet failure the
//! std-facility check exists to turn into a build error.
//!
//! `Stdin` keeps the stub's behaviour: a browser has no stdin, and reporting
//! immediate EOF is the honest answer rather than an error.
//!
//! Writes are handed to the host per call, tagged by stream. The host buffers
//! to whole lines before reaching `console.log`/`console.error`, because Rust's
//! formatting machinery emits a line in several writes and one console entry
//! per fragment is unreadable.

use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};

#[link(wasm_import_module = "frustrate")]
unsafe extern "C" {
    /// `stream` is 1 for stdout, 2 for stderr.
    safe fn write_stdio(stream: u32, ptr: *const u8, len: usize);
}

pub struct Stdin;
pub struct Stdout;
pub struct Stderr;

impl Stdin {
    pub const fn new() -> Stdin {
        Stdin
    }
}

impl io::Read for Stdin {
    #[inline]
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        Ok(0)
    }

    #[inline]
    fn read_buf(&mut self, _cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        Ok(())
    }

    #[inline]
    fn read_vectored(&mut self, _bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        Ok(0)
    }

    #[inline]
    fn is_read_vectored(&self) -> bool {
        false
    }

    #[inline]
    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        if !buf.is_empty() { Err(io::Error::READ_EXACT_EOF) } else { Ok(()) }
    }

    #[inline]
    fn read_buf_exact(&mut self, cursor: BorrowedCursor<'_, u8>) -> io::Result<()> {
        if cursor.capacity() != 0 { Err(io::Error::READ_EXACT_EOF) } else { Ok(()) }
    }

    #[inline]
    fn read_to_end(&mut self, _buf: &mut Vec<u8>) -> io::Result<usize> {
        Ok(0)
    }

    #[inline]
    fn read_to_string(&mut self, _buf: &mut String) -> io::Result<usize> {
        Ok(0)
    }
}

fn emit(stream: u32, buf: &[u8]) -> io::Result<usize> {
    if !buf.is_empty() {
        write_stdio(stream, buf.as_ptr(), buf.len());
    }
    Ok(buf.len())
}

impl Stdout {
    pub const fn new() -> Stdout {
        Stdout
    }
}

impl io::Write for Stdout {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        emit(1, buf)
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for b in bufs {
            total += emit(1, b)?;
        }
        Ok(total)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Stderr {
    pub const fn new() -> Stderr {
        Stderr
    }
}

impl io::Write for Stderr {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        emit(2, buf)
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for b in bufs {
            total += emit(2, b)?;
        }
        Ok(total)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub const STDIN_BUF_SIZE: usize = 0;

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}

/// Panic output goes through the same console path as `eprintln!`, so a panic
/// message is visible rather than swallowed.
pub fn panic_output() -> Option<Stderr> {
    Some(Stderr::new())
}
