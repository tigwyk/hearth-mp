//! Predictors behind one handle type, for the `hearth_predictor_*` exports.
//!
//! A plugin registers its logic types by name ([`plugin!`](crate::plugin)); C#
//! creates a predictor by that name. Any `Err` or panic from the logic poisons
//! the handle: every later call returns 0 or empty and the last error is the
//! poison message, so C# reports it once and falls back to the authoritative
//! state instead of failing every frame.

use std::panic::{catch_unwind, AssertUnwindSafe};

use hearth_client::DynPredictor;
use hearth_protocol::{PlayerId, Value};

use crate::{panic_message, set_error};

/// Builds a boxed predictor for one logic type: `hearth_client::boxed::<L>`.
pub type LogicCtor = fn(PlayerId, u8) -> Box<dyn DynPredictor>;

/// Opaque predictor handle.
pub struct HearthPredictor {
    inner: Box<dyn DynPredictor>,
    poisoned: Option<String>,
}

impl HearthPredictor {
    /// A predictor for the logic registered as `logic`.
    pub fn new(
        registry: &[(&str, LogicCtor)],
        logic: &str,
        me: &str,
        tick_hz: u8,
    ) -> Result<Self, String> {
        let ctor = registry
            .iter()
            .find(|(name, _)| *name == logic)
            .map(|(_, ctor)| *ctor)
            .ok_or_else(|| format!("unknown logic \"{logic}\"; registered: {}", names(registry)))?;
        Ok(Self {
            inner: ctor(PlayerId::from(me), tick_hz),
            poisoned: None,
        })
    }

    /// Run `f` on the logic unless poisoned; an `Err` or a panic poisons.
    fn run<T>(
        &mut self,
        fallback: T,
        f: impl FnOnce(&mut dyn DynPredictor) -> Result<T, String>,
    ) -> T {
        if let Some(message) = &self.poisoned {
            set_error(message.clone());
            return fallback;
        }
        let result = catch_unwind(AssertUnwindSafe(|| f(&mut *self.inner)));
        let message = match result {
            Ok(Ok(value)) => return value,
            Ok(Err(message)) => message,
            Err(panic) => format!("panic in prediction logic: {}", panic_message(panic)),
        };
        let message = format!("predictor disabled: {message}");
        set_error(message.clone());
        self.poisoned = Some(message);
        fallback
    }

    pub fn input(&mut self, seq: u32, input_json: &str) -> bool {
        self.run(false, |p| {
            let input: Value =
                serde_json::from_str(input_json).map_err(|e| format!("bad input JSON: {e}"))?;
            p.input(seq, &input).map(|()| true)
        })
    }

    pub fn on_authoritative(&mut self, state_json: &str, acked_json: &str) -> bool {
        self.run(false, |p| {
            let state: Value =
                serde_json::from_str(state_json).map_err(|e| format!("bad state JSON: {e}"))?;
            let acked = seqs(acked_json)?;
            p.on_authoritative(&state, &acked).map(|()| true)
        })
    }

    pub fn on_unconfirmed(&mut self, seqs_json: &str) {
        self.run((), |p| {
            p.on_unconfirmed(&seqs(seqs_json)?);
            Ok(())
        })
    }

    /// The predicted state as JSON; empty once poisoned.
    pub fn state(&mut self) -> String {
        self.run(String::new(), |p| p.predicted().map(|v| v.to_string()))
    }
}

fn seqs(json: &str) -> Result<Vec<u32>, String> {
    if json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(json).map_err(|e| format!("bad seq array JSON: {e}"))
}

/// The registered logic names as a JSON array.
pub fn names(registry: &[(&str, LogicCtor)]) -> String {
    serde_json::to_string(&registry.iter().map(|(name, _)| *name).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into())
}

/// The C ABI bodies; the `registry` argument is the plugin's static table.
pub mod ffi {
    use std::ffi::c_char;

    use super::*;
    use crate::ffi::str_arg;
    use crate::{guard, HearthBuf};

    unsafe fn predictor<'a>(p: *mut HearthPredictor) -> Result<&'a mut HearthPredictor, String> {
        p.as_mut()
            .ok_or_else(|| "predictor handle is null".to_string())
    }

    /// # Safety
    /// `logic` and `me` are null or NUL-terminated.
    pub unsafe fn __impl_hearth_predictor_new(
        registry: &[(&str, LogicCtor)],
        logic: *const c_char,
        me: *const c_char,
        tick_hz: u8,
    ) -> *mut HearthPredictor {
        guard(std::ptr::null_mut(), || {
            let predictor = HearthPredictor::new(
                registry,
                str_arg(logic, "logic")?,
                str_arg(me, "me")?,
                tick_hz,
            )?;
            Ok(Box::into_raw(Box::new(predictor)))
        })
    }

    /// # Safety
    /// `p` is null or a live handle.
    pub unsafe fn __impl_hearth_predictor_free(p: *mut HearthPredictor) {
        guard((), || {
            if !p.is_null() {
                drop(Box::from_raw(p));
            }
            Ok(())
        })
    }

    /// # Safety
    /// `p` is null or a live handle; `input_json` is null or NUL-terminated.
    pub unsafe fn __impl_hearth_predictor_input(
        p: *mut HearthPredictor,
        seq: u32,
        input_json: *const c_char,
    ) -> u8 {
        guard(0, || {
            Ok(u8::from(
                predictor(p)?.input(seq, str_arg(input_json, "input_json")?),
            ))
        })
    }

    /// # Safety
    /// `p` is null or a live handle; the strings are null or NUL-terminated.
    pub unsafe fn __impl_hearth_predictor_on_authoritative(
        p: *mut HearthPredictor,
        state_json: *const c_char,
        acked_json: *const c_char,
    ) -> u8 {
        guard(0, || {
            let p = predictor(p)?;
            Ok(u8::from(p.on_authoritative(
                str_arg(state_json, "state_json")?,
                str_arg(acked_json, "acked_json")?,
            )))
        })
    }

    /// # Safety
    /// `p` is null or a live handle; `seqs_json` is null or NUL-terminated.
    pub unsafe fn __impl_hearth_predictor_on_unconfirmed(
        p: *mut HearthPredictor,
        seqs_json: *const c_char,
    ) {
        guard((), || {
            predictor(p)?.on_unconfirmed(str_arg(seqs_json, "seqs_json")?);
            Ok(())
        })
    }

    /// # Safety
    /// `p` is null or a live handle.
    pub unsafe fn __impl_hearth_predictor_state(p: *mut HearthPredictor) -> HearthBuf {
        guard(HearthBuf::EMPTY, || {
            Ok(HearthBuf::from_vec(predictor(p)?.state().into_bytes()))
        })
    }

    pub fn __impl_hearth_predictor_names(registry: &[(&str, LogicCtor)]) -> HearthBuf {
        guard(HearthBuf::EMPTY, || {
            Ok(HearthBuf::from_vec(names(registry).into_bytes()))
        })
    }
}
