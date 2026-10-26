/// Emit every Hearth Unity export into the calling crate.
///
/// The stock plugin (`crates/hearth-unity-plugin`) is `hearth_unity_core::plugin! {}`.
/// A game that predicts with its own shared logic builds its own cdylib, also
/// with `[lib] name = "hearth_unity"`, so it drops in for the stock binaries:
///
/// ```ignore
/// hearth_unity_core::plugin! { logic: { "arena" => arena_logic::Arena } }
/// ```
///
/// Each name becomes a logic C# can open with
/// `new HearthPredictor<TState>(room, "arena", 20)`. The built-in
/// `"hearth.echo1d"` is always registered. With the crate's `webgl` feature (and
/// a direct `wasm-bindgen` dependency), the same functions are also emitted as
/// `#[wasm_bindgen]` exports for the WebGL core.
///
/// The `#[no_mangle]` functions are emitted here, in the plugin crate, rather
/// than re-exported from this rlib: a cdylib does not reliably keep an rlib
/// dependency's exported symbols under LTO.
#[macro_export]
macro_rules! plugin {
    () => {
        $crate::__plugin_impl! {}
    };
    (logic: { $($name:literal => $ty:ty),* $(,)? }) => {
        $crate::__plugin_impl! { $($name => $ty),* }
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __plugin_impl {
    ($($name:literal => $ty:ty),*) => {
        pub use $crate::{ffi, HearthBuf, HearthClient, HearthInterp, HearthPredictor, LogicCtor};

        /// The logic types this plugin can predict with, by name.
        pub static HEARTH_LOGIC: &[(&str, $crate::LogicCtor)] = &[
            (
                "hearth.echo1d",
                $crate::hearth_client::boxed::<$crate::hearth_client::Echo1d> as $crate::LogicCtor,
            ),
            $(($name, $crate::hearth_client::boxed::<$ty> as $crate::LogicCtor),)*
        ];

        #[cfg(not(target_arch = "wasm32"))]
        pub use __hearth_c_exports::*;

        #[cfg(not(target_arch = "wasm32"))]
        mod __hearth_c_exports {
            use ::std::ffi::c_char;

            use $crate::{ffi, HearthBuf, HearthClient, HearthInterp, HearthPredictor};

            /// A new client, or null on error. `encoding`: 0 MsgPack, 1 JSON.
            ///
            /// # Safety
            /// `schema_hash` is null or a NUL-terminated string.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_new(schema_hash: *const c_char, encoding: u8) -> *mut HearthClient {
                ffi::__impl_hearth_client_new(schema_hash, encoding)
            }

            /// Free a client. Null is a no-op.
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_free(c: *mut HearthClient) {
                ffi::__impl_hearth_client_free(c)
            }

            /// The `HELLO` frame. `opts_json` may be null or empty for `{}`.
            ///
            /// # Safety
            /// `c` is null or a live handle; `opts_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_hello(
                c: *mut HearthClient,
                opts_json: *const c_char,
                out_is_text: *mut u8,
            ) -> HearthBuf {
                ffi::__impl_hearth_client_hello(c, opts_json, out_is_text)
            }

            /// Feed one received frame; returns the events as a JSON array.
            ///
            /// # Safety
            /// `c` is null or a live handle; `data` points to `len` readable bytes.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_handle(
                c: *mut HearthClient,
                data: *const u8,
                len: usize,
                is_text: u8,
            ) -> HearthBuf {
                ffi::__impl_hearth_client_handle(c, data, len, is_text)
            }

            /// Queue an intent; returns its seq (>= 1), or 0 on invalid JSON.
            ///
            /// # Safety
            /// `c` is null or a live handle; `intent_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_queue_intent(c: *mut HearthClient, intent_json: *const c_char) -> u32 {
                ffi::__impl_hearth_client_queue_intent(c, intent_json)
            }

            /// The next `INTENT` frame, or an empty buffer when nothing may be sent now.
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_poll_send(c: *mut HearthClient, now_ms: u64, out_is_text: *mut u8) -> HearthBuf {
                ffi::__impl_hearth_client_poll_send(c, now_ms, out_is_text)
            }

            /// The `LEAVE` frame.
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_leave(c: *mut HearthClient, out_is_text: *mut u8) -> HearthBuf {
                ffi::__impl_hearth_client_leave(c, out_is_text)
            }

            /// Record a socket drop; returns the events as a JSON array.
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_on_disconnected(c: *mut HearthClient) -> HearthBuf {
                ffi::__impl_hearth_client_on_disconnected(c)
            }

            /// The current state as JSON (`null` before the first `FULL_STATE`).
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_state(c: *mut HearthClient) -> HearthBuf {
                ffi::__impl_hearth_client_state(c)
            }

            /// The last state sequence applied.
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_seq(c: *mut HearthClient) -> u64 {
                ffi::__impl_hearth_client_seq(c)
            }

            /// The server time of the newest ticked `PATCH`; 0 before one (or in event rooms).
            ///
            /// # Safety
            /// `c` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_client_last_tick_t(c: *mut HearthClient) -> u64 {
                ffi::__impl_hearth_client_last_tick_t(c)
            }

            /// The reconnect decision: the absolute retry clock value, or -1 to give up.
            #[no_mangle]
            pub extern "C" fn hearth_reconnect_decide(code: u16, first_drop_ms: u64, attempt: u32, now_ms: u64) -> i64 {
                ffi::__impl_hearth_reconnect_decide(code, first_drop_ms, attempt, now_ms)
            }

            /// Free a buffer returned by any export. An empty buffer is a no-op.
            ///
            /// # Safety
            /// `b` came from this library and was not freed before.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_buf_free(b: HearthBuf) {
                ffi::__impl_hearth_buf_free(b)
            }

            /// This thread's last error message, or an empty buffer if none.
            #[no_mangle]
            pub extern "C" fn hearth_last_error() -> HearthBuf {
                ffi::__impl_hearth_last_error()
            }

            /// A snapshot interpolator for a room ticking at `tick_hz`.
            #[no_mangle]
            pub extern "C" fn hearth_interp_new(tick_hz: u8) -> *mut HearthInterp {
                ffi::__impl_hearth_interp_new(tick_hz)
            }

            /// Free an interpolator. Null is a no-op.
            ///
            /// # Safety
            /// `p` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_interp_free(p: *mut HearthInterp) {
                ffi::__impl_hearth_interp_free(p)
            }

            /// Track a JSON path (`"*"` matches every key at its level); 1 = ok.
            ///
            /// # Safety
            /// `p` is null or a live handle; `path_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_interp_track(p: *mut HearthInterp, path_json: *const c_char) -> u8 {
                ffi::__impl_hearth_interp_track(p, path_json)
            }

            /// Record the state after a ticked `PATCH` with server time `server_t`, received at `local_ms`.
            ///
            /// # Safety
            /// `p` is null or a live handle; `state_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_interp_on_patch(
                p: *mut HearthInterp,
                state_json: *const c_char,
                server_t: u64,
                local_ms: u64,
            ) -> u8 {
                ffi::__impl_hearth_interp_on_patch(p, state_json, server_t, local_ms)
            }

            /// Drop snapshots and clock samples (after a `FULL_STATE`).
            ///
            /// # Safety
            /// `p` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_interp_reset(p: *mut HearthInterp) {
                ffi::__impl_hearth_interp_reset(p)
            }

            /// The interpolated value at `local_ms` (two periods behind the server), as JSON; empty when unknown.
            ///
            /// # Safety
            /// `p` is null or a live handle; `path_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_interp_sample(p: *mut HearthInterp, path_json: *const c_char, local_ms: u64) -> HearthBuf {
                ffi::__impl_hearth_interp_sample(p, path_json, local_ms)
            }

            /// The interpolated value at an explicit server render time, as JSON; empty when unknown.
            ///
            /// # Safety
            /// `p` is null or a live handle; `path_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_interp_sample_at(p: *mut HearthInterp, path_json: *const c_char, render_t: u64) -> HearthBuf {
                ffi::__impl_hearth_interp_sample_at(p, path_json, render_t)
            }

            /// A predictor for the logic registered as `logic`, or null (the last
            /// error then lists the registered names).
            ///
            /// # Safety
            /// `logic` and `me` are null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_predictor_new(
                logic: *const c_char,
                me: *const c_char,
                tick_hz: u8,
            ) -> *mut HearthPredictor {
                ffi::__impl_hearth_predictor_new(super::HEARTH_LOGIC, logic, me, tick_hz)
            }

            /// Free a predictor. Null is a no-op.
            ///
            /// # Safety
            /// `p` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_predictor_free(p: *mut HearthPredictor) {
                ffi::__impl_hearth_predictor_free(p)
            }

            /// Record input `seq` and step; 1 = ok, 0 = poisoned (see the last error).
            ///
            /// # Safety
            /// `p` is null or a live handle; `input_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_predictor_input(p: *mut HearthPredictor, seq: u32, input_json: *const c_char) -> u8 {
                ffi::__impl_hearth_predictor_input(p, seq, input_json)
            }

            /// Reconcile with the authoritative state; `acked_json` is a JSON `u32` array.
            ///
            /// # Safety
            /// `p` is null or a live handle; the strings are null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_predictor_on_authoritative(
                p: *mut HearthPredictor,
                state_json: *const c_char,
                acked_json: *const c_char,
            ) -> u8 {
                ffi::__impl_hearth_predictor_on_authoritative(p, state_json, acked_json)
            }

            /// Drop inputs the server reported as unconfirmed.
            ///
            /// # Safety
            /// `p` is null or a live handle; `seqs_json` is null or NUL-terminated.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_predictor_on_unconfirmed(p: *mut HearthPredictor, seqs_json: *const c_char) {
                ffi::__impl_hearth_predictor_on_unconfirmed(p, seqs_json)
            }

            /// The predicted state as JSON; empty once poisoned.
            ///
            /// # Safety
            /// `p` is null or a live handle.
            #[no_mangle]
            pub unsafe extern "C" fn hearth_predictor_state(p: *mut HearthPredictor) -> HearthBuf {
                ffi::__impl_hearth_predictor_state(p)
            }

            /// The registered logic names as a JSON array.
            #[no_mangle]
            pub extern "C" fn hearth_predictor_names() -> HearthBuf {
                ffi::__impl_hearth_predictor_names(super::HEARTH_LOGIC)
            }
        }

        #[cfg(feature = "webgl")]
        mod __hearth_web_exports {
            use ::wasm_bindgen::prelude::wasm_bindgen;

            use $crate::web;

            #[wasm_bindgen]
            pub fn client_new(schema_hash: &str, encoding: u8) -> u32 { web::client_new(schema_hash, encoding) }
            #[wasm_bindgen]
            pub fn client_free(h: u32) { web::client_free(h) }
            #[wasm_bindgen]
            pub fn client_hello(h: u32, opts_json: &str) -> Vec<u8> { web::client_hello(h, opts_json) }
            #[wasm_bindgen]
            pub fn client_handle(h: u32, data: &[u8], is_text: bool) -> String { web::client_handle(h, data, is_text) }
            #[wasm_bindgen]
            pub fn client_queue_intent(h: u32, intent_json: &str) -> u32 { web::client_queue_intent(h, intent_json) }
            #[wasm_bindgen]
            pub fn client_poll_send(h: u32, now_ms: f64) -> Vec<u8> { web::client_poll_send(h, now_ms) }
            #[wasm_bindgen]
            pub fn client_leave(h: u32) -> Vec<u8> { web::client_leave(h) }
            #[wasm_bindgen]
            pub fn client_on_disconnected(h: u32) -> String { web::client_on_disconnected(h) }
            #[wasm_bindgen]
            pub fn client_state(h: u32) -> String { web::client_state(h) }
            #[wasm_bindgen]
            pub fn client_seq(h: u32) -> f64 { web::client_seq(h) }
            #[wasm_bindgen]
            pub fn client_last_tick_t(h: u32) -> f64 { web::client_last_tick_t(h) }
            #[wasm_bindgen]
            pub fn interp_new(tick_hz: u8) -> u32 { web::interp_new(tick_hz) }
            #[wasm_bindgen]
            pub fn interp_free(h: u32) { web::interp_free(h) }
            #[wasm_bindgen]
            pub fn interp_track(h: u32, path_json: &str) -> bool { web::interp_track(h, path_json) }
            #[wasm_bindgen]
            pub fn interp_on_patch(h: u32, state_json: &str, server_t: f64, local_ms: f64) -> bool {
                web::interp_on_patch(h, state_json, server_t, local_ms)
            }
            #[wasm_bindgen]
            pub fn interp_reset(h: u32) { web::interp_reset(h) }
            #[wasm_bindgen]
            pub fn interp_sample(h: u32, path_json: &str, local_ms: f64) -> String { web::interp_sample(h, path_json, local_ms) }
            #[wasm_bindgen]
            pub fn interp_sample_at(h: u32, path_json: &str, render_t: f64) -> String {
                web::interp_sample_at(h, path_json, render_t)
            }
            #[wasm_bindgen]
            pub fn reconnect_decide(code: u16, first_drop_ms: f64, attempt: u32, now_ms: f64) -> f64 {
                web::reconnect_decide(code, first_drop_ms, attempt, now_ms)
            }
            #[wasm_bindgen]
            pub fn last_is_text() -> bool { web::last_is_text() }
            #[wasm_bindgen]
            pub fn last_error() -> String { web::last_error() }
            #[wasm_bindgen]
            pub fn predictor_new(logic: &str, me: &str, tick_hz: u8) -> u32 {
                web::predictor_new(super::HEARTH_LOGIC, logic, me, tick_hz)
            }
            #[wasm_bindgen]
            pub fn predictor_free(h: u32) { web::predictor_free(h) }
            #[wasm_bindgen]
            pub fn predictor_input(h: u32, seq: u32, input_json: &str) -> bool { web::predictor_input(h, seq, input_json) }
            #[wasm_bindgen]
            pub fn predictor_on_authoritative(h: u32, state_json: &str, acked_json: &str) -> bool {
                web::predictor_on_authoritative(h, state_json, acked_json)
            }
            #[wasm_bindgen]
            pub fn predictor_on_unconfirmed(h: u32, seqs_json: &str) { web::predictor_on_unconfirmed(h, seqs_json) }
            #[wasm_bindgen]
            pub fn predictor_state(h: u32) -> String { web::predictor_state(h) }
            #[wasm_bindgen]
            pub fn predictor_names() -> String { web::predictor_names(super::HEARTH_LOGIC) }
        }

        /// The plain-Rust WebGL bodies, for wasm tests of this plugin.
        #[doc(hidden)]
        pub use $crate::web;
    };
}
