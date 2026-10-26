//! RS256/ES256 JWS signature verification through the Workers WebCrypto API.
//!
//! The runtime already ships WebCrypto, so asymmetric JWT verification costs no
//! extra Wasm size. `worker` 0.8 does not enable the `web-sys`
//! `SubtleCrypto`/`CryptoKey` features, so the two calls go through
//! `js_sys::Reflect`; the ABI is the standard
//! `crypto.subtle.importKey("jwk", ...)` then `verify(...)` pair.
//!
//! Natively (tests, `cargo check` on the host) there is no JS runtime, so
//! [`verify`] answers `Err`. Callers treat that as an upstream failure, never as
//! a valid signature.

/// Verify one JWS signature against a JWK. `Ok(false)` is a bad signature;
/// `Err` means the check itself could not be performed.
#[cfg(not(target_arch = "wasm32"))]
pub async fn verify(
    _alg: &str,
    _jwk: &serde_json::Value,
    _signing_input: &[u8],
    _sig: &[u8],
) -> Result<bool, String> {
    Err("WebCrypto is only available in the Workers runtime".to_owned())
}

/// Verify one JWS signature against a JWK. `Ok(false)` is a bad signature;
/// `Err` means the check itself could not be performed.
#[cfg(target_arch = "wasm32")]
pub async fn verify(
    alg: &str,
    jwk: &serde_json::Value,
    signing_input: &[u8],
    sig: &[u8],
) -> Result<bool, String> {
    use worker::js_sys::{self, Array, Reflect, Uint8Array, JSON};
    use worker::wasm_bindgen::{JsCast, JsValue};
    use worker::wasm_bindgen_futures::JsFuture;

    // Importing an EC key names the curve; verifying names the hash. The JWS
    // ES256 signature is already the raw r‖s form WebCrypto expects.
    let (import_parameters, verify_parameters) = match alg {
        "RS256" => (
            r#"{"name":"RSASSA-PKCS1-v1_5","hash":{"name":"SHA-256"}}"#,
            r#"{"name":"RSASSA-PKCS1-v1_5"}"#,
        ),
        "ES256" => (
            r#"{"name":"ECDSA","namedCurve":"P-256"}"#,
            r#"{"name":"ECDSA","hash":{"name":"SHA-256"}}"#,
        ),
        other => return Err(format!("unsupported JWT algorithm `{other}`")),
    };

    let global = js_sys::global();
    let crypto = Reflect::get(&global, &JsValue::from_str("crypto"))
        .map_err(|_| "crypto is unavailable".to_owned())?;
    let subtle = Reflect::get(&crypto, &JsValue::from_str("subtle"))
        .map_err(|_| "crypto.subtle is unavailable".to_owned())?;
    let function = |name: &str| -> Result<js_sys::Function, String> {
        Reflect::get(&subtle, &JsValue::from_str(name))
            .map_err(|_| format!("crypto.subtle.{name} is unavailable"))?
            .dyn_into::<js_sys::Function>()
            .map_err(|_| format!("crypto.subtle.{name} is not a function"))
    };
    let parameters = |json: &str| -> Result<JsValue, String> {
        JSON::parse(json).map_err(|_| "the built-in WebCrypto parameters are invalid".to_owned())
    };

    // importKey("jwk", jwk, algorithm, extractable, keyUsages)
    let key_data =
        JSON::parse(&jwk.to_string()).map_err(|_| "the JWK is not valid JSON".to_owned())?;
    let usages: JsValue = Array::of1(&JsValue::from_str("verify")).into();
    let arguments = Array::new();
    arguments.push(&JsValue::from_str("jwk"));
    arguments.push(&key_data);
    arguments.push(&parameters(import_parameters)?);
    arguments.push(&JsValue::FALSE);
    arguments.push(&usages);
    let imported = function("importKey")?
        .apply(&subtle, &arguments)
        .map_err(|_| "WebCrypto importKey failed".to_owned())?;
    let key = JsFuture::from(imported.unchecked_into::<js_sys::Promise>())
        .await
        .map_err(|_| "WebCrypto importKey failed".to_owned())?;

    // verify(algorithm, key, signature, data)
    let signature: JsValue = Uint8Array::from(sig).into();
    let data: JsValue = Uint8Array::from(signing_input).into();
    let arguments = Array::new();
    arguments.push(&parameters(verify_parameters)?);
    arguments.push(&key);
    arguments.push(&signature);
    arguments.push(&data);
    let verified = function("verify")?
        .apply(&subtle, &arguments)
        .map_err(|_| "WebCrypto verify failed".to_owned())?;
    let verified = JsFuture::from(verified.unchecked_into::<js_sys::Promise>())
        .await
        .map_err(|_| "WebCrypto verify failed".to_owned())?;
    Ok(verified.as_bool().unwrap_or(false))
}
