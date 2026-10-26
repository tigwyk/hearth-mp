//! The WebGL bodies: [`plugin!`](crate::plugin) wraps each in `#[wasm_bindgen]`
//! when the plugin crate builds with `--features webgl`.
//!
//! The same bodies as the C ABI ([`crate::ops`]); only the types change. A
//! client is a `u32` handle into a thread-local table (0 is never handed out),
//! and calls on an unknown or freed handle return empty values. Byte results
//! are `Vec<u8>`; their text flag is read afterwards with [`last_is_text`].
//! `hearth.jslib` wraps these for C#.

use std::cell::{Cell, RefCell};

use hearth_client::ClientCore;

use crate::{guard, ops, HearthInterp, HearthPredictor, LogicCtor};

thread_local! {
    static CLIENTS: RefCell<Vec<Option<ClientCore>>> = const { RefCell::new(Vec::new()) };
    static LAST_IS_TEXT: Cell<bool> = const { Cell::new(false) };
    static PREDICTORS: RefCell<Vec<Option<HearthPredictor>>> = const { RefCell::new(Vec::new()) };
    static INTERPS: RefCell<Vec<Option<HearthInterp>>> = const { RefCell::new(Vec::new()) };
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

pub fn client_hello(h: u32, opts_json: &str) -> Vec<u8> {
    with(h, Vec::new(), |core| {
        Ok(bytes(ops::hello(core, opts_json)?))
    })
}

pub fn client_handle(h: u32, data: &[u8], is_text: bool) -> String {
    with(h, String::new(), |core| {
        Ok(ops::handle(core, data, is_text))
    })
}

pub fn client_queue_intent(h: u32, intent_json: &str) -> u32 {
    with(h, 0, |core| ops::queue_intent(core, intent_json))
}

pub fn client_poll_send(h: u32, now_ms: f64) -> Vec<u8> {
    with(h, Vec::new(), |core| {
        Ok(core
            .poll_send(now_ms as u64)
            .map(|msg| bytes(ops::wire(msg)))
            .unwrap_or_default())
    })
}

pub fn client_leave(h: u32) -> Vec<u8> {
    with(h, Vec::new(), |core| Ok(bytes(ops::wire(core.leave()))))
}

pub fn client_on_disconnected(h: u32) -> String {
    with(h, String::new(), |core| Ok(ops::on_disconnected(core)))
}

pub fn client_state(h: u32) -> String {
    with(h, String::new(), |core| Ok(ops::state(core)))
}

pub fn client_last_tick_t(h: u32) -> f64 {
    with(h, 0.0, |core| {
        Ok(core.last_tick().map_or(0.0, |(_, t)| t as f64))
    })
}

pub fn client_seq(h: u32) -> f64 {
    with(h, 0.0, |core| Ok(core.seq() as f64))
}

/// The retry clock value, or -1 to give up.
pub fn reconnect_decide(code: u16, first_drop_ms: f64, attempt: u32, now_ms: f64) -> f64 {
    ops::reconnect_decide(code, first_drop_ms as u64, attempt, now_ms as u64) as f64
}

/// The text flag of the last `client_hello`/`client_poll_send`/`client_leave`.
pub fn last_is_text() -> bool {
    LAST_IS_TEXT.with(Cell::get)
}

/// This thread's last error message (empty if none).
pub fn last_error() -> String {
    crate::last_error()
}

// ---- predictors -------------------------------------------------------------

/// A slot-reusing insert; handles start at 1 so 0 can mean "error".
fn insert<T>(table: &mut Vec<Option<T>>, value: T) -> u32 {
    if let Some(i) = table.iter().position(Option::is_none) {
        table[i] = Some(value);
        i as u32 + 1
    } else {
        table.push(Some(value));
        table.len() as u32
    }
}

fn with_predictor<T>(h: u32, fallback: T, f: impl FnOnce(&mut HearthPredictor) -> T) -> T {
    guard(fallback, || {
        PREDICTORS.with(|table| {
            let mut table = table.borrow_mut();
            let p = (h as usize)
                .checked_sub(1)
                .and_then(|i| table.get_mut(i))
                .and_then(Option::as_mut)
                .ok_or_else(|| format!("unknown predictor handle {h}"))?;
            Ok(f(p))
        })
    })
}

/// A predictor handle, or 0 on an unknown logic name (see [`last_error`]).
pub fn predictor_new(registry: &[(&str, LogicCtor)], logic: &str, me: &str, tick_hz: u8) -> u32 {
    guard(0, || {
        let p = HearthPredictor::new(registry, logic, me, tick_hz)?;
        Ok(PREDICTORS.with(|table| insert(&mut table.borrow_mut(), p)))
    })
}

pub fn predictor_free(h: u32) {
    PREDICTORS.with(|table| {
        if let Some(slot) = (h as usize)
            .checked_sub(1)
            .and_then(|i| table.borrow_mut().get_mut(i).map(Option::take))
        {
            drop(slot);
        }
    });
}

pub fn predictor_input(h: u32, seq: u32, input_json: &str) -> bool {
    with_predictor(h, false, |p| p.input(seq, input_json))
}

pub fn predictor_on_authoritative(h: u32, state_json: &str, acked_json: &str) -> bool {
    with_predictor(h, false, |p| p.on_authoritative(state_json, acked_json))
}

pub fn predictor_on_unconfirmed(h: u32, seqs_json: &str) {
    with_predictor(h, (), |p| p.on_unconfirmed(seqs_json))
}

pub fn predictor_state(h: u32) -> String {
    with_predictor(h, String::new(), |p| p.state())
}

pub fn predictor_names(registry: &[(&str, LogicCtor)]) -> String {
    crate::predict::names(registry)
}

// ---- interpolators ----------------------------------------------------------

fn with_interp<T>(
    h: u32,
    fallback: T,
    f: impl FnOnce(&mut HearthInterp) -> Result<T, String>,
) -> T {
    guard(fallback, || {
        INTERPS.with(|table| {
            let mut table = table.borrow_mut();
            let p = (h as usize)
                .checked_sub(1)
                .and_then(|i| table.get_mut(i))
                .and_then(Option::as_mut)
                .ok_or_else(|| format!("unknown interpolator handle {h}"))?;
            f(p)
        })
    })
}

pub fn interp_new(tick_hz: u8) -> u32 {
    INTERPS.with(|table| insert(&mut table.borrow_mut(), HearthInterp::new(tick_hz)))
}

pub fn interp_free(h: u32) {
    INTERPS.with(|table| {
        if let Some(slot) = (h as usize)
            .checked_sub(1)
            .and_then(|i| table.borrow_mut().get_mut(i).map(Option::take))
        {
            drop(slot);
        }
    });
}

pub fn interp_track(h: u32, path_json: &str) -> bool {
    with_interp(h, false, |p| p.track(path_json).map(|()| true))
}

pub fn interp_on_patch(h: u32, state_json: &str, server_t: f64, local_ms: f64) -> bool {
    with_interp(h, false, |p| {
        p.on_patch(state_json, server_t as u64, local_ms as u64)
            .map(|()| true)
    })
}

pub fn interp_reset(h: u32) {
    with_interp(h, (), |p| {
        p.reset();
        Ok(())
    })
}

pub fn interp_sample(h: u32, path_json: &str, local_ms: f64) -> String {
    with_interp(h, String::new(), |p| p.sample(path_json, local_ms as u64))
}

pub fn interp_sample_at(h: u32, path_json: &str, render_t: f64) -> String {
    with_interp(h, String::new(), |p| {
        p.sample_at(path_json, render_t as u64)
    })
}
