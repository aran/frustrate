//! A bridge whose actor reaches browser APIs through wasm-bindgen, web-sys and
//! js-sys from inside its Worker
//! (tests/dart_integration/test/bindgen_web_test.dart).
//!
//! The members that await a JS promise run it on a `runtime::spawn_local`
//! task, because a `JsFuture` is not `Send`. `spawn_local` exists only on
//! single-threaded wasm, so on the threaded build those members panic and the
//! test skips them.

use frustrate::{bridge, Addr, StreamSink};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// The actor's Worker global, where `setTimeout` lives (a Worker has no
/// `window`).
fn scope() -> web_sys::WorkerGlobalScope {
    js_sys::global().unchecked_into()
}

/// Run `task` on this instance's executor, where a non-`Send` future can run.
#[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
fn spawn_local(task: impl std::future::Future<Output = ()> + 'static) {
    frustrate::runtime::spawn_local(task);
}

#[cfg(not(all(target_family = "wasm", not(target_feature = "atomics"))))]
fn spawn_local(_task: impl std::future::Future<Output = ()> + 'static) {
    panic!("runtime::spawn_local exists only on single-threaded wasm");
}

/// A promise that resolves after `ms` milliseconds, by `setTimeout`.
fn timeout(ms: i32) -> js_sys::Promise {
    js_sys::Promise::new(&mut |resolve, _reject| {
        scope()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .expect("setTimeout");
    })
}

#[bridge(actor)]
pub struct Browser {
    me: Addr<Browser>,
    total: i64,
}

#[bridge]
impl Browser {
    pub fn new(me: Addr<Browser>) -> Self {
        Browser { me, total: 0 }
    }

    /// Await a JS promise resolving to `x`, add what it resolved to to the
    /// running total through `Addr::call`, and send the new total.
    pub fn add_from_promise(&mut self, x: i64, sink: StreamSink<i64>) {
        let me = self.me.clone();
        spawn_local(async move {
            let resolved = js_sys::Promise::resolve(&JsValue::from_f64(x as f64));
            let value = wasm_bindgen_futures::JsFuture::from(resolved)
                .await
                .expect("a resolved promise")
                .as_f64()
                .expect("a number") as i64;
            if let Ok(total) = me.call(move |s| s.add(value)).await {
                sink.add(total);
            }
        });
    }

    /// Wait `ms` milliseconds on `setTimeout`, then add `x` to the running
    /// total through `Addr::call` and send the new total.
    pub fn add_after_timeout(&mut self, ms: i32, x: i64, sink: StreamSink<i64>) {
        let me = self.me.clone();
        spawn_local(async move {
            wasm_bindgen_futures::JsFuture::from(timeout(ms))
                .await
                .expect("a timer resolves");
            if let Ok(total) = me.call(move |s| s.add(x)).await {
                sink.add(total);
            }
        });
    }

    /// The running total.
    pub fn total(&self) -> i64 {
        self.total
    }

    /// Have the browser call a Rust closure that panics with `message`: a
    /// wasm-bindgen closure handed to `setTimeout`, run by the event loop
    /// outside any frustrate entry point.
    pub fn panic_in_callback(&mut self, message: String) {
        let callback = Closure::once_into_js(move || -> () { panic!("{message}") });
        scope()
            .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), 0)
            .expect("setTimeout");
    }

    /// Make a promise rejected with `reason`, and handle it nowhere.
    pub fn reject_unhandled(&mut self, reason: String) {
        let _ = js_sys::Promise::reject(&JsValue::from_str(&reason));
    }
}

impl Browser {
    fn add(&mut self, n: i64) -> i64 {
        self.total += n;
        self.total
    }
}
