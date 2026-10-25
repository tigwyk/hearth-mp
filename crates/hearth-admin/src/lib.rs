//! Hearth v0.2c admin: a room registry Durable Object and the token-gated
//! `/admin/*` router.
//!
//! This is a library, not a Worker. The dev's Worker mounts [`serve_admin`]
//! under `/admin/*` only when `HEARTH_ADMIN_TOKEN` is set, and declares the
//! registry Durable Object class with [`registry_object!`]. Rooms report to the
//! registry through `WorkerHost::report`; the registry is optional, so a
//! missing binding only disables `GET /admin/rooms`.

pub mod registry;
pub mod routes;

pub use registry::{Registry, RegistryObject, RegistryService, RegistryState, RoomRow};
pub use routes::{admin_token, check_bearer, serve_admin, token_from};

/// Declare the `HearthRegistry` Durable Object class in the calling crate.
///
/// A Durable Object class must be exported from the user's Worker, so it cannot
/// live in this library; the macro emits it in the caller's crate. Call it once
/// at the crate root:
///
/// ```ignore
/// hearth::admin::registry_object!();
/// ```
///
/// Then bind the class in `wrangler.toml`:
///
/// ```toml
/// [[durable_objects.bindings]]
/// name = "HEARTH_REGISTRY"
/// class_name = "HearthRegistry"
///
/// [[migrations]]
/// tag = "v2"
/// new_sqlite_classes = ["HearthRegistry"]
/// ```
#[macro_export]
macro_rules! registry_object {
    () => {
        // `#[worker::durable_object]`'s wasm-bindgen struct-marker expansion
        // spells the crate as bare `wasm_bindgen`, so the wrapper lives in a
        // hidden module that puts that name in scope.
        #[doc(hidden)]
        pub mod __hearth_registry_object {
            #[allow(unused_imports)]
            use ::worker::wasm_bindgen;

            #[::worker::durable_object]
            pub struct HearthRegistry($crate::RegistryObject);

            impl ::worker::DurableObject for HearthRegistry {
                fn new(state: ::worker::State, _env: ::worker::Env) -> Self {
                    Self($crate::RegistryObject::new(state))
                }

                async fn fetch(
                    &self,
                    req: ::worker::Request,
                ) -> ::worker::Result<::worker::Response> {
                    self.0.fetch(req).await
                }
            }
        }

        pub use self::__hearth_registry_object::HearthRegistry;
    };
}
