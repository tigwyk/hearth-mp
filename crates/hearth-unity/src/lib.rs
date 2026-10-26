//! Hearth Unity native plugin: a C ABI over [`hearth_client::ClientCore`].
//!
//! Every structured value crosses the boundary as UTF-8 JSON, and every byte
//! result comes back as a [`HearthBuf`] that the caller frees with
//! [`hearth_buf_free`]. C# (`unity/com.hearth.client/Runtime/Core/NativeCore.cs`)
//! owns the socket, the main-thread dispatch and the reconnect loop.
//!
//! No export may abort the Unity process: each body runs inside
//! `catch_unwind`, and a panic, a null pointer, invalid UTF-8 or invalid JSON
//! returns the empty/0/-1 value and sets [`hearth_last_error`].
//!
//! The header `include/hearth_unity.h` is generated with
//! `cbindgen --config cbindgen.toml --output include/hearth_unity.h`.
//!
//! The `webgl` feature adds the same functions as `wasm-bindgen` exports (see
//! `web`); both builds share the bodies in [`ops`].

use std::cell::RefCell;
use std::ffi::{c_char, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};

use hearth_client::{ClientConfig, ClientCore, Reconnect, ReconnectPolicy};
use hearth_protocol::{Encoding, WireMsg};

#[cfg(feature = "webgl")]
pub mod web;

/// An owned byte buffer handed to C. An empty buffer is `{null, 0, 0}`; every
/// non-empty one must be passed to [`hearth_buf_free`] exactly once.
#[repr(C)]
pub struct HearthBuf {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl HearthBuf {
    const EMPTY: HearthBuf = HearthBuf {
        ptr: std::ptr::null_mut(),
        len: 0,
        cap: 0,
    };

    fn from_vec(bytes: Vec<u8>) -> HearthBuf {
        if bytes.is_empty() {
            return HearthBuf::EMPTY;
        }
        let mut bytes = std::mem::ManuallyDrop::new(bytes);
        HearthBuf {
            ptr: bytes.as_mut_ptr(),
            len: bytes.len(),
            cap: bytes.capacity(),
        }
    }
}

/// Opaque client handle: one room session.
pub struct HearthClient {
    core: ClientCore,
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn set_error(message: impl Into<String>) {
    LAST_ERROR.with(|cell| *cell.borrow_mut() = message.into());
}

/// This thread's last error message (empty if none).
pub fn last_error() -> String {
    LAST_ERROR.with(|cell| cell.borrow().clone())
}

/// Run `body`, turning a panic or an `Err` into `fallback` plus a last error.
pub fn guard<T>(fallback: T, body: impl FnOnce() -> Result<T, String>) -> T {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(value)) => value,
        Ok(Err(message)) => {
            set_error(message);
            fallback
        }
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            set_error(format!("panic in hearth_unity: {message}"));
            fallback
        }
    }
}

/// Shared bodies for the C ABI and the wasm exports.
pub mod ops {
    use super::*;
    use hearth_protocol::Value;

    pub fn new_client(schema_hash: &str, encoding: u8) -> Result<ClientCore, String> {
        let encoding = match encoding {
            0 => Encoding::MsgPack,
            1 => Encoding::Json,
            other => return Err(format!("unknown encoding {other} (0 msgpack, 1 json)")),
        };
        Ok(ClientCore::new(ClientConfig::new(schema_hash, encoding)))
    }

    /// A frame as `(bytes, is_text)`.
    pub fn wire(msg: WireMsg) -> (Vec<u8>, bool) {
        match msg {
            WireMsg::Binary(bytes) => (bytes, false),
            WireMsg::Text(text) => (text.into_bytes(), true),
        }
    }

    pub fn hello(core: &ClientCore, opts_json: &str) -> Result<(Vec<u8>, bool), String> {
        let opts: Value = if opts_json.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(opts_json).map_err(|e| format!("bad opts JSON: {e}"))?
        };
        Ok(wire(core.hello(opts)))
    }

    /// Feed one frame; returns the events as a JSON array. A protocol error is
    /// an `error` event, never a failure.
    pub fn handle(core: &mut ClientCore, data: &[u8], is_text: bool) -> String {
        let msg = if is_text {
            WireMsg::Text(String::from_utf8_lossy(data).into_owned())
        } else {
            WireMsg::Binary(data.to_vec())
        };
        let events = match core.handle(&msg) {
            Ok(events) => serde_json::to_value(events).unwrap_or(Value::Array(Vec::new())),
            Err(error) => serde_json::json!([{
                "type": "error",
                "code": "BAD_FRAME",
                "message": format!("bad server frame: {error:?}"),
                "ref_seq": null,
            }]),
        };
        events.to_string()
    }

    pub fn queue_intent(core: &mut ClientCore, intent_json: &str) -> Result<u32, String> {
        let intent: Value =
            serde_json::from_str(intent_json).map_err(|e| format!("bad intent JSON: {e}"))?;
        Ok(core.queue_intent(intent))
    }

    pub fn on_disconnected(core: &mut ClientCore) -> String {
        serde_json::to_string(&core.on_disconnected()).unwrap_or_else(|_| "[]".into())
    }

    pub fn state(core: &ClientCore) -> String {
        core.state().to_string()
    }

    /// The retry clock value, or -1 to give up.
    pub fn reconnect_decide(code: u16, first_drop_ms: u64, attempt: u32, now_ms: u64) -> i64 {
        match ReconnectPolicy::default().decide(code, first_drop_ms, attempt, now_ms) {
            Reconnect::RetryAt(at) => i64::try_from(at).unwrap_or(i64::MAX),
            Reconnect::GiveUp => -1,
        }
    }
}

unsafe fn str_arg<'a>(ptr: *const c_char, name: &str) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err(format!("{name} is null"));
    }
    CStr::from_ptr(ptr)
        .to_str()
        .map_err(|_| format!("{name} is not UTF-8"))
}

unsafe fn client<'a>(c: *mut HearthClient) -> Result<&'a mut HearthClient, String> {
    c.as_mut()
        .ok_or_else(|| "client handle is null".to_string())
}

unsafe fn set_text(out_is_text: *mut u8, is_text: bool) {
    if !out_is_text.is_null() {
        *out_is_text = u8::from(is_text);
    }
}

/// A new client, or null on error. `encoding`: 0 MsgPack, 1 JSON.
///
/// # Safety
/// `schema_hash` is null or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_new(
    schema_hash: *const c_char,
    encoding: u8,
) -> *mut HearthClient {
    guard(std::ptr::null_mut(), || {
        let hash = str_arg(schema_hash, "schema_hash")?;
        let core = ops::new_client(hash, encoding)?;
        Ok(Box::into_raw(Box::new(HearthClient { core })))
    })
}

/// Free a client. Null is a no-op.
///
/// # Safety
/// `c` is null or a handle from [`hearth_client_new`] not yet freed.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_free(c: *mut HearthClient) {
    guard((), || {
        if !c.is_null() {
            drop(Box::from_raw(c));
        }
        Ok(())
    })
}

/// The `HELLO` frame. `opts_json` may be null or empty for `{}`.
///
/// # Safety
/// `c` is null or a live handle; `opts_json` is null or NUL-terminated;
/// `out_is_text` is null or writable.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_hello(
    c: *mut HearthClient,
    opts_json: *const c_char,
    out_is_text: *mut u8,
) -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        let client = client(c)?;
        let opts = if opts_json.is_null() {
            ""
        } else {
            str_arg(opts_json, "opts_json")?
        };
        let (bytes, is_text) = ops::hello(&client.core, opts)?;
        set_text(out_is_text, is_text);
        Ok(HearthBuf::from_vec(bytes))
    })
}

/// Feed one received frame; returns the events as a JSON array.
///
/// # Safety
/// `c` is null or a live handle; `data` points to `len` readable bytes (or is
/// null with `len == 0`).
#[no_mangle]
pub unsafe extern "C" fn hearth_client_handle(
    c: *mut HearthClient,
    data: *const u8,
    len: usize,
    is_text: u8,
) -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        let client = client(c)?;
        let bytes: &[u8] = if len == 0 {
            &[]
        } else if data.is_null() {
            return Err("data is null".into());
        } else {
            std::slice::from_raw_parts(data, len)
        };
        let events = ops::handle(&mut client.core, bytes, is_text != 0);
        Ok(HearthBuf::from_vec(events.into_bytes()))
    })
}

/// Queue an intent; returns its seq (>= 1), or 0 on invalid JSON.
///
/// # Safety
/// `c` is null or a live handle; `intent_json` is null or NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_queue_intent(
    c: *mut HearthClient,
    intent_json: *const c_char,
) -> u32 {
    guard(0, || {
        let client = client(c)?;
        ops::queue_intent(&mut client.core, str_arg(intent_json, "intent_json")?)
    })
}

/// The next `INTENT` frame, or an empty buffer when nothing may be sent now.
///
/// # Safety
/// `c` is null or a live handle; `out_is_text` is null or writable.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_poll_send(
    c: *mut HearthClient,
    now_ms: u64,
    out_is_text: *mut u8,
) -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        let client = client(c)?;
        Ok(match client.core.poll_send(now_ms) {
            Some(msg) => {
                let (bytes, is_text) = ops::wire(msg);
                set_text(out_is_text, is_text);
                HearthBuf::from_vec(bytes)
            }
            None => HearthBuf::EMPTY,
        })
    })
}

/// The `LEAVE` frame.
///
/// # Safety
/// `c` is null or a live handle; `out_is_text` is null or writable.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_leave(
    c: *mut HearthClient,
    out_is_text: *mut u8,
) -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        let client = client(c)?;
        let (bytes, is_text) = ops::wire(client.core.leave());
        set_text(out_is_text, is_text);
        Ok(HearthBuf::from_vec(bytes))
    })
}

/// Record a socket drop; returns the events as a JSON array.
///
/// # Safety
/// `c` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_on_disconnected(c: *mut HearthClient) -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        let client = client(c)?;
        Ok(HearthBuf::from_vec(
            ops::on_disconnected(&mut client.core).into_bytes(),
        ))
    })
}

/// The current state as JSON (`null` before the first `FULL_STATE`).
///
/// # Safety
/// `c` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_state(c: *mut HearthClient) -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        let client = client(c)?;
        Ok(HearthBuf::from_vec(ops::state(&client.core).into_bytes()))
    })
}

/// The last state sequence applied (0 before the first `FULL_STATE`). The
/// editor window shows it next to each patch.
///
/// # Safety
/// `c` is null or a live handle.
#[no_mangle]
pub unsafe extern "C" fn hearth_client_seq(c: *mut HearthClient) -> u64 {
    guard(0, || Ok(client(c)?.core.seq()))
}

/// The reconnect decision: the absolute retry clock value, or -1 to give up.
#[no_mangle]
pub extern "C" fn hearth_reconnect_decide(
    code: u16,
    first_drop_ms: u64,
    attempt: u32,
    now_ms: u64,
) -> i64 {
    guard(-1, || {
        Ok(ops::reconnect_decide(code, first_drop_ms, attempt, now_ms))
    })
}

/// Free a buffer returned by any export. An empty buffer is a no-op.
///
/// # Safety
/// `b` came from this library and was not freed before.
#[no_mangle]
pub unsafe extern "C" fn hearth_buf_free(b: HearthBuf) {
    guard((), || {
        if !b.ptr.is_null() {
            drop(Vec::from_raw_parts(b.ptr, b.len, b.cap));
        }
        Ok(())
    })
}

/// This thread's last error message, or an empty buffer if none.
#[no_mangle]
pub extern "C" fn hearth_last_error() -> HearthBuf {
    guard(HearthBuf::EMPTY, || {
        Ok(HearthBuf::from_vec(last_error().into_bytes()))
    })
}
