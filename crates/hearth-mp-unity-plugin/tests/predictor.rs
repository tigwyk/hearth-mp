//! The `hearth_predictor_*` exports (plan v0.4b Task 2).

use std::ffi::CString;
use std::time::Duration;

use hearth_mp_client::{boxed, Echo1dState, Logic};
use hearth_mp_protocol::PlayerId;
use hearth_unity::*;
use serde_json::{json, Value};

fn take(buf: HearthBuf) -> String {
    let text = if buf.ptr.is_null() {
        String::new()
    } else {
        String::from_utf8(unsafe { std::slice::from_raw_parts(buf.ptr, buf.len) }.to_vec()).unwrap()
    };
    unsafe { hearth_buf_free(buf) };
    text
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn state(p: *mut HearthPredictor) -> Value {
    serde_json::from_str(&take(unsafe { hearth_predictor_state(p) })).unwrap()
}

fn input(p: *mut HearthPredictor, seq: u32, value: i64) -> u8 {
    unsafe { hearth_predictor_input(p, seq, c(&value.to_string()).as_ptr()) }
}

fn authoritative(p: *mut HearthPredictor, state: &str, acked: &str) -> u8 {
    unsafe { hearth_predictor_on_authoritative(p, c(state).as_ptr(), c(acked).as_ptr()) }
}

#[test]
fn echo1d_through_ffi() {
    let p = unsafe { hearth_predictor_new(c("hearth.echo1d").as_ptr(), c("me").as_ptr(), 20) };
    assert!(!p.is_null());
    assert_eq!(input(p, 1, 1), 1);
    assert_eq!(state(p), json!({"x": 1}));
    assert_eq!(authoritative(p, r#"{"x":0}"#, "[1]"), 1);
    assert_eq!(state(p), json!({"x": 0}));
    input(p, 2, 1);
    input(p, 3, 1);
    assert_eq!(authoritative(p, r#"{"x":1}"#, "[2]"), 1);
    assert_eq!(state(p), json!({"x": 2}));
    unsafe { hearth_predictor_on_unconfirmed(p, c("[3]").as_ptr()) };
    authoritative(p, r#"{"x":1}"#, "[]");
    assert_eq!(state(p), json!({"x": 1}));
    unsafe { hearth_predictor_free(p) };
}

#[test]
fn unknown_logic_null_and_error_lists_names() {
    let p = unsafe { hearth_predictor_new(c("arnea").as_ptr(), c("me").as_ptr(), 20) };
    assert!(p.is_null());
    let error = take(hearth_last_error());
    assert!(error.contains("unknown logic \"arnea\""), "{error}");
    assert!(error.contains("hearth.echo1d"), "{error}");
}

#[test]
fn names_lists_registered() {
    assert_eq!(take(hearth_predictor_names()), r#"["hearth.echo1d"]"#);
}

struct PanicLogic;

impl Logic for PanicLogic {
    type State = Echo1dState;
    type Input = i64;

    fn step(_: &mut Echo1dState, _: &[(PlayerId, i64)], _: Duration) {
        panic!("dev logic blew up");
    }
}

#[test]
fn panicking_logic_poisons_not_aborts() {
    let registry: &[(&str, LogicCtor)] = &[("panic", boxed::<PanicLogic> as LogicCtor)];
    let p = unsafe {
        ffi::__impl_hearth_predictor_new(registry, c("panic").as_ptr(), c("me").as_ptr(), 20)
    };
    assert!(!p.is_null());
    assert_eq!(
        input(p, 1, 1),
        0,
        "the panic is caught and reported as failure"
    );
    let error = take(hearth_last_error());
    assert!(error.contains("dev logic blew up"), "{error}");
    unsafe { hearth_predictor_free(p) };
}

#[test]
fn poisoned_calls_return_zero_with_message() {
    let p = unsafe { hearth_predictor_new(c("hearth.echo1d").as_ptr(), c("me").as_ptr(), 20) };
    // Review Focus 2: the room state does not match the logic state.
    assert_eq!(authoritative(p, r#"{"y":1}"#, "[]"), 0);
    let first = take(hearth_last_error());
    assert!(first.contains("predictor disabled"), "{first}");
    assert_eq!(input(p, 1, 1), 0);
    assert_eq!(authoritative(p, r#"{"x":1}"#, "[]"), 0, "stays poisoned");
    assert_eq!(take(unsafe { hearth_predictor_state(p) }), "");
    assert_eq!(take(hearth_last_error()), first);
    unsafe { hearth_predictor_free(p) };
}

#[test]
fn null_predictor_is_safe() {
    let null = std::ptr::null_mut();
    unsafe {
        hearth_predictor_free(null);
        assert_eq!(hearth_predictor_input(null, 1, c("1").as_ptr()), 0);
        assert_eq!(
            hearth_predictor_on_authoritative(null, c("{}").as_ptr(), c("[]").as_ptr()),
            0
        );
        hearth_predictor_on_unconfirmed(null, c("[]").as_ptr());
        assert_eq!(take(hearth_predictor_state(null)), "");
    }
}
