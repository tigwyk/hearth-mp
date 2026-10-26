//! The `hearth_interp_*` exports against the shared `conformance/vectors/interp.json`
//! (v0.4 Task 5, the Unity half).

use std::ffi::CString;

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

fn c(v: &Value) -> CString {
    CString::new(v.to_string()).unwrap()
}

fn close(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            (a.as_f64().unwrap() - b.as_f64().unwrap()).abs() < 1e-6
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| close(a, b))
        }
        _ => a == b,
    }
}

#[test]
fn interp_through_ffi_matches_fixture() {
    for case in &hearth_mp_protocol::vectors::load("interp.json") {
        let name = &case["name"];
        let track = case.get("track").cloned().unwrap_or(json!(["value"]));
        let sample = case.get("sample").cloned().unwrap_or_else(|| track.clone());
        let p = hearth_interp_new(case["tick_hz"].as_u64().unwrap() as u8);
        assert_eq!(unsafe { hearth_interp_track(p, c(&track).as_ptr()) }, 1);
        for pair in case["snapshots"].as_array().unwrap() {
            let state = json!({ "value": pair[1] });
            let t = pair[0].as_u64().unwrap();
            assert_eq!(
                unsafe { hearth_interp_on_patch(p, c(&state).as_ptr(), t, t) },
                1
            );
        }
        let render_t = case["render_t"].as_u64().unwrap();
        let actual = take(unsafe { hearth_interp_sample_at(p, c(&sample).as_ptr(), render_t) });
        if case["expect"].is_null() {
            assert_eq!(actual, "", "case {name}");
        } else {
            let actual: Value = serde_json::from_str(&actual)
                .unwrap_or_else(|_| panic!("case {name}: got {actual:?}"));
            assert!(
                close(&actual, &case["expect"]),
                "case {name}: {actual} vs {}",
                case["expect"]
            );
        }
        unsafe { hearth_interp_free(p) };
    }
}

#[test]
fn sample_uses_clock_two_periods_behind() {
    let p = hearth_interp_new(10);
    unsafe {
        hearth_interp_track(p, c(&json!(["x"])).as_ptr());
        // Server time runs 10_000 ms ahead of the local clock.
        hearth_interp_on_patch(p, c(&json!({"x": 0})).as_ptr(), 11_000, 1_000);
        hearth_interp_on_patch(p, c(&json!({"x": 10})).as_ptr(), 11_100, 1_100);
        // Local 1_250 -> server 11_250 -> render 11_050 (two 100 ms periods back).
        let v: Value = serde_json::from_str(&take(hearth_interp_sample(
            p,
            c(&json!(["x"])).as_ptr(),
            1_250,
        )))
        .unwrap();
        assert!(close(&v, &json!(5)), "{v}");
        hearth_interp_reset(p);
        assert_eq!(
            take(hearth_interp_sample(p, c(&json!(["x"])).as_ptr(), 1_250)),
            ""
        );
        hearth_interp_free(p);
    }
}

#[test]
fn null_and_bad_path_are_safe() {
    let null = std::ptr::null_mut();
    unsafe {
        hearth_interp_free(null);
        assert_eq!(hearth_interp_track(null, c(&json!(["x"])).as_ptr()), 0);
        assert_eq!(
            take(hearth_interp_sample(null, c(&json!(["x"])).as_ptr(), 0)),
            ""
        );
        let p = hearth_interp_new(20);
        assert_eq!(
            hearth_interp_track(p, CString::new("{\"no\":1}").unwrap().as_ptr()),
            0
        );
        assert!(take(hearth_last_error()).contains("path"));
        hearth_interp_free(p);
    }
}
