use frustrate::bridge;
use std::sync::Mutex;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// A logger that takes a lock for every record.
struct Locking;

impl log::Log for Locking {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        LINES.lock().unwrap().push(record.args().to_string());
    }
    fn flush(&self) {}
}

static LOGGER: Locking = Locking;

/// Installs the logger. Unclaimed: nothing a claimed member runs.
#[bridge(sync)]
pub fn install() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(log::LevelFilter::Info);
}

/// Claims `no_block` and logs. The logger it reaches is whatever `install`
/// put in `log`'s slot, which takes a lock.
#[bridge(sync, no_block)]
pub fn work(x: i32) -> i32 {
    log::info!("work {x}");
    x
}
