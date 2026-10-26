//! `wasm-bindgen` exports for Unity WebGL builds (`--features webgl`).
//!
//! The same bodies as the C ABI ([`crate::ops`]); only the types change. A
//! client is a `u32` handle into a thread-local table (0 is never handed out),
//! and calls on an unknown or freed handle return empty values. Byte results
//! are `Vec<u8>`; their text flag is read afterwards with [`last_is_text`].
//! `hearth.jslib` wraps these for C#.

use std::cell::{Cell, RefCell};

use hearth_client::ClientCore;
use wasm_bindgen::prelude::*;

use crate::{guard, ops};

thread_local! {
    static CLIENTS: RefCell<Vec<Option<ClientCore>>> = const { RefCell::new(Vec::new()) };
    static LAST_IS_TEXT: Cell<bool> = const { Cell::new(false) };
}

fn with<T>(h: u32, fallback: T, f: impl FnOnce(&mut ClientCore) -> Result<T, String>) -> T {
    guard(fallback, || {
        CLIENTS.with(|clients| {
            let mut clients = clients.borrow_mut();
            let core = (h as usize)
                .checked_sub(1)
                .and_then(|i| clients.get_mut(i))
                .and_then(Option::as_mut)
                .ok_or_else(|| format!("unknown client handle {h}"))?;
            f(core)
        })
    })
}

fn bytes((bytes, is_text): (Vec<u8>, bool)) -> Vec<u8> {
    LAST_IS_TEXT.with(|cell| cell.set(is_text));
    bytes
}

/// A new client handle, or 0 on error.
#[wasm_bindgen]
pub fn client_new(schema_hash: &str, encoding: u8) -> u32 {
    guard(0, || {
        let core = ops::new_client(schema_hash, encoding)?;
        Ok(CLIENTS.with(|clients| {
            let mut clients = clients.borrow_mut();
            // Reuse a freed slot so a long session does not grow the table.
            if let Some(i) = clients.iter().position(Option::is_none) {
                clients[i] = Some(core);
                i as u32 + 1
            } else {
                clients.push(Some(core));
                clients.len() as u32
            }
        }))
    })
}

#[wasm_bindgen]
pub fn client_free(h: u32) {
    CLIENTS.with(|clients| {
        if let Some(slot) = (h as usize)
            .checked_sub(1)
            .and_then(|i| clients.borrow_mut().get_mut(i).map(Option::take))
        {
            drop(slot);
        }
    });
}

#[wasm_bindgen]
pub fn client_hello(h: u32, opts_json: &str) -> Vec<u8> {
    with(h, Vec::new(), |core| {
        Ok(bytes(ops::hello(core, opts_json)?))
    })
}

#[wasm_bindgen]
pub fn client_handle(h: u32, data: &[u8], is_text: bool) -> String {
    with(h, String::new(), |core| {
        Ok(ops::handle(core, data, is_text))
    })
}

#[wasm_bindgen]
pub fn client_queue_intent(h: u32, intent_json: &str) -> u32 {
    with(h, 0, |core| ops::queue_intent(core, intent_json))
}

#[wasm_bindgen]
pub fn client_poll_send(h: u32, now_ms: f64) -> Vec<u8> {
    with(h, Vec::new(), |core| {
        Ok(core
            .poll_send(now_ms as u64)
            .map(|msg| bytes(ops::wire(msg)))
            .unwrap_or_default())
    })
}

#[wasm_bindgen]
pub fn client_leave(h: u32) -> Vec<u8> {
    with(h, Vec::new(), |core| Ok(bytes(ops::wire(core.leave()))))
}

#[wasm_bindgen]
pub fn client_on_disconnected(h: u32) -> String {
    with(h, String::new(), |core| Ok(ops::on_disconnected(core)))
}

#[wasm_bindgen]
pub fn client_state(h: u32) -> String {
    with(h, String::new(), |core| Ok(ops::state(core)))
}

#[wasm_bindgen]
pub fn client_seq(h: u32) -> f64 {
    with(h, 0.0, |core| Ok(core.seq() as f64))
}

/// The retry clock value, or -1 to give up.
#[wasm_bindgen]
pub fn reconnect_decide(code: u16, first_drop_ms: f64, attempt: u32, now_ms: f64) -> f64 {
    ops::reconnect_decide(code, first_drop_ms as u64, attempt, now_ms as u64) as f64
}

/// The text flag of the last `client_hello`/`client_poll_send`/`client_leave`.
#[wasm_bindgen]
pub fn last_is_text() -> bool {
    LAST_IS_TEXT.with(Cell::get)
}

/// This thread's last error message (empty if none).
#[wasm_bindgen]
pub fn last_error() -> String {
    crate::last_error()
}
