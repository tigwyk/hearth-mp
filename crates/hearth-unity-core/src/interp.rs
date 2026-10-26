//! Snapshot interpolation for ticked rooms behind one handle: the
//! `hearth_interp_*` exports (v0.4 Task 7, the Unity port of Godot's
//! `HearthInterp`). Paths and values are JSON; a `"*"` path segment matches every
//! key at its level.

use hearth_client::{ClockSync, Interpolator};
use hearth_protocol::Value;

/// Opaque interpolator handle: a clock estimate plus the snapshot buffer.
pub struct HearthInterp {
    clock: ClockSync,
    interp: Interpolator,
}

fn path(json: &str) -> Result<Vec<Value>, String> {
    match serde_json::from_str(json).map_err(|e| format!("bad path JSON: {e}"))? {
        Value::Array(path) => Ok(path),
        _ => Err("a path is a JSON array".into()),
    }
}

impl HearthInterp {
    pub fn new(tick_hz: u8) -> Self {
        Self {
            clock: ClockSync::new(),
            interp: Interpolator::new(tick_hz),
        }
    }

    pub fn track(&mut self, path_json: &str) -> Result<(), String> {
        self.interp.track(path(path_json)?);
        Ok(())
    }

    /// Record one ticked snapshot received at `local_ms`.
    pub fn on_patch(
        &mut self,
        state_json: &str,
        server_t: u64,
        local_ms: u64,
    ) -> Result<(), String> {
        let state: Value =
            serde_json::from_str(state_json).map_err(|e| format!("bad state JSON: {e}"))?;
        self.clock.observe(server_t, local_ms);
        self.interp.on_patch(&state, server_t);
        Ok(())
    }

    /// Drop snapshots and clock samples (a `FULL_STATE` replaced the state).
    pub fn reset(&mut self) {
        self.clock = ClockSync::new();
        self.interp.clear();
    }

    /// The value at the render time for `local_ms`, as JSON; empty when unknown.
    pub fn sample(&self, path_json: &str, local_ms: u64) -> Result<String, String> {
        let Some(render_t) = self.interp.render_time(&self.clock, local_ms) else {
            return Ok(String::new());
        };
        self.sample_at(path_json, render_t)
    }

    /// The value at an explicit server render time (the fixture tests).
    pub fn sample_at(&self, path_json: &str, render_t: u64) -> Result<String, String> {
        Ok(self
            .interp
            .sample(&path(path_json)?, render_t)
            .map(|v| v.to_string())
            .unwrap_or_default())
    }
}

/// The C ABI bodies.
pub mod ffi {
    use std::ffi::c_char;

    use super::*;
    use crate::ffi::str_arg;
    use crate::{guard, HearthBuf};

    unsafe fn interp<'a>(p: *mut HearthInterp) -> Result<&'a mut HearthInterp, String> {
        p.as_mut()
            .ok_or_else(|| "interpolator handle is null".to_string())
    }

    pub fn __impl_hearth_interp_new(tick_hz: u8) -> *mut HearthInterp {
        guard(std::ptr::null_mut(), || {
            Ok(Box::into_raw(Box::new(HearthInterp::new(tick_hz))))
        })
    }

    /// # Safety
    /// `p` is null or a live handle.
    pub unsafe fn __impl_hearth_interp_free(p: *mut HearthInterp) {
        guard((), || {
            if !p.is_null() {
                drop(Box::from_raw(p));
            }
            Ok(())
        })
    }

    /// # Safety
    /// `p` is null or a live handle; `path_json` is null or NUL-terminated.
    pub unsafe fn __impl_hearth_interp_track(p: *mut HearthInterp, path_json: *const c_char) -> u8 {
        guard(0, || {
            interp(p)?.track(str_arg(path_json, "path_json")?)?;
            Ok(1)
        })
    }

    /// # Safety
    /// `p` is null or a live handle; `state_json` is null or NUL-terminated.
    pub unsafe fn __impl_hearth_interp_on_patch(
        p: *mut HearthInterp,
        state_json: *const c_char,
        server_t: u64,
        local_ms: u64,
    ) -> u8 {
        guard(0, || {
            interp(p)?.on_patch(str_arg(state_json, "state_json")?, server_t, local_ms)?;
            Ok(1)
        })
    }

    /// # Safety
    /// `p` is null or a live handle.
    pub unsafe fn __impl_hearth_interp_reset(p: *mut HearthInterp) {
        guard((), || {
            interp(p)?.reset();
            Ok(())
        })
    }

    /// # Safety
    /// `p` is null or a live handle; `path_json` is null or NUL-terminated.
    pub unsafe fn __impl_hearth_interp_sample(
        p: *mut HearthInterp,
        path_json: *const c_char,
        local_ms: u64,
    ) -> HearthBuf {
        guard(HearthBuf::EMPTY, || {
            Ok(HearthBuf::from_vec(
                interp(p)?
                    .sample(str_arg(path_json, "path_json")?, local_ms)?
                    .into_bytes(),
            ))
        })
    }

    /// # Safety
    /// `p` is null or a live handle; `path_json` is null or NUL-terminated.
    pub unsafe fn __impl_hearth_interp_sample_at(
        p: *mut HearthInterp,
        path_json: *const c_char,
        render_t: u64,
    ) -> HearthBuf {
        guard(HearthBuf::EMPTY, || {
            Ok(HearthBuf::from_vec(
                interp(p)?
                    .sample_at(str_arg(path_json, "path_json")?, render_t)?
                    .into_bytes(),
            ))
        })
    }
}
