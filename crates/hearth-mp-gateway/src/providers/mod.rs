//! Provider dispatch: every provider turns an external credential into a verified
//! [`Identity`], and only then does the gateway mint a Hearth token for it.
//!
//! Provider HTTP calls go through [`Fetcher`], so each provider can be tested
//! natively against canned upstream responses ([`CannedFetcher`]).

pub mod anonymous;
pub mod discord;
pub mod itch;
pub mod jwt;
pub mod steam;

use std::time::Duration;

use serde_json::Value;
use worker::{Delay, Env, Fetch, Headers, Method, Request, RequestInit};

use crate::auth::Identity;

/// The HTTP seam lives in `hearth-mp-server` so the gateway's auth providers and the
/// server's handoff allocator share one definition. The Worker implementation
/// stays here: it is the only part that needs the Workers API.
pub use hearth_mp_server::fetcher::{CannedFetcher, Fetcher, HttpReq, HttpResp};

/// The secret every Hearth token is signed with.
const SECRET_BINDING: &str = "HEARTH_SECRET";
/// Upstream provider calls are abandoned after this long.
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a provider could not produce an identity. Each case maps to one status.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider is not enabled in this deployment.
    Disabled,
    /// The request body is missing a field or is malformed.
    BadRequest(String),
    /// The credential was verified as invalid.
    Rejected(String),
    /// The provider's upstream is down, slow or answered nonsense.
    Upstream(String),
}

impl ProviderError {
    /// The HTTP status `POST /auth/{provider}` answers with.
    pub fn status(&self) -> u16 {
        match self {
            ProviderError::Disabled => 404,
            ProviderError::BadRequest(_) => 400,
            ProviderError::Rejected(_) => 401,
            ProviderError::Upstream(_) => 502,
        }
    }

    /// The message in the JSON error body.
    pub fn message(&self) -> &str {
        match self {
            ProviderError::Disabled => "provider not enabled",
            ProviderError::BadRequest(message)
            | ProviderError::Rejected(message)
            | ProviderError::Upstream(message) => message,
        }
    }
}

/// The real fetcher: a Worker subrequest that gives up after five seconds.
pub struct WorkerFetcher;

impl Fetcher for WorkerFetcher {
    async fn send(&self, req: HttpReq) -> Result<HttpResp, String> {
        let mut init = RequestInit::new();
        let headers = Headers::new();
        for (name, value) in &req.headers {
            headers.set(name, value).map_err(|err| err.to_string())?;
        }
        init.with_headers(headers);
        init.with_method(match req.method {
            "POST" => Method::Post,
            _ => Method::Get,
        });
        if let Some(body) = req.body {
            init.with_body(Some(body.into()));
        }
        let request = Request::new_with_init(&req.url, &init)
            .map_err(|err| format!("request build failed: {err}"))?;
        let fetch = Fetch::Request(request);
        let send = fetch.send();
        let timeout = Delay::from(UPSTREAM_TIMEOUT);
        // The Workers API has no way to attach an AbortSignal to a subrequest, so
        // a call that overruns is abandoned: the consumer gets `timeout` at once.
        futures::pin_mut!(send);
        futures::pin_mut!(timeout);
        match futures::future::select(send, timeout).await {
            futures::future::Either::Left((result, _)) => {
                let mut response = result.map_err(|err| format!("subrequest failed: {err}"))?;
                let status = response.status_code();
                let body = response
                    .text()
                    .await
                    .map_err(|err| format!("reading the response failed: {err}"))?;
                Ok(HttpResp { status, body })
            }
            futures::future::Either::Right((_, _)) => Err("timeout".to_owned()),
        }
    }
}

/// Which providers a deployment enables. The default enables anonymous only.
#[non_exhaustive]
pub struct AuthConfig {
    /// `POST /auth/anonymous` is enabled.
    pub anonymous: bool,
    /// `POST /auth/jwt`, when configured.
    pub jwt: Option<jwt::JwtConfig>,
    /// `POST /auth/steam`, when configured.
    pub steam: Option<steam::SteamConfig>,
    /// `POST /auth/itch` is enabled.
    pub itch: bool,
    /// `POST /auth/discord`, when configured.
    pub discord: Option<discord::DiscordConfig>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            anonymous: true,
            jwt: None,
            steam: None,
            itch: false,
            discord: None,
        }
    }
}

impl AuthConfig {
    /// Build the configuration from the Worker's variables and secrets.
    ///
    /// A misconfiguration that would otherwise silently disable a provider (an
    /// unknown provider, a short HS256 secret) is an error: the gateway answers
    /// 500 instead of pretending the provider is off.
    pub fn from_env(env: &Env) -> Result<Self, String> {
        let var = |name: &str| -> Option<String> {
            env.var(name)
                .ok()
                .map(|value| value.to_string())
                .filter(|value| !value.is_empty())
        };
        let flag = |name: &str, default: bool| -> bool {
            match var(name).as_deref() {
                None => default,
                Some(value) => matches!(value, "1" | "true" | "yes" | "on"),
            }
        };

        let jwt = if let Some(url) = var("HEARTH_JWT_JWKS_URL") {
            Some(jwt::JwtConfig {
                issuer: required(&var, "HEARTH_JWT_ISS")?,
                audience: required(&var, "HEARTH_JWT_AUD")?,
                keys: jwt::JwtKeys::Jwks { url },
                name_claim: var("HEARTH_JWT_NAME_CLAIM"),
            })
        } else if var("HEARTH_JWT_SECRET").is_some() {
            let secret = env
                .secret("HEARTH_JWT_SECRET")
                .map(|value| value.to_string())
                .unwrap_or_default();
            if secret.len() < 32 {
                return Err("HEARTH_JWT_SECRET must be at least 32 bytes".to_owned());
            }
            Some(jwt::JwtConfig {
                issuer: required(&var, "HEARTH_JWT_ISS")?,
                audience: required(&var, "HEARTH_JWT_AUD")?,
                keys: jwt::JwtKeys::Hs256 {
                    secret_env: "HEARTH_JWT_SECRET",
                },
                name_claim: var("HEARTH_JWT_NAME_CLAIM"),
            })
        } else {
            None
        };

        let steam = match var("HEARTH_STEAM_APP_ID") {
            None => None,
            Some(app_id) => Some(steam::SteamConfig {
                app_id: app_id
                    .parse()
                    .map_err(|_| "HEARTH_STEAM_APP_ID must be a number".to_owned())?,
                key_env: "HEARTH_STEAM_KEY",
                identity: var("HEARTH_STEAM_IDENTITY").unwrap_or_else(|| "hearth".to_owned()),
                reject_banned: !flag("HEARTH_STEAM_ALLOW_BANNED", false),
            }),
        };

        let discord =
            match (
                var("HEARTH_DISCORD_CLIENT_ID"),
                var("HEARTH_DISCORD_SECRET"),
            ) {
                (Some(client_id), Some(_)) => Some(discord::DiscordConfig {
                    client_id,
                    secret_env: "HEARTH_DISCORD_SECRET",
                    redirect_uri: var("HEARTH_DISCORD_REDIRECT_URI"),
                }),
                (None, None) => None,
                _ => return Err(
                    "Discord auth needs both HEARTH_DISCORD_CLIENT_ID and HEARTH_DISCORD_SECRET"
                        .to_owned(),
                ),
            };

        Ok(Self {
            anonymous: flag("HEARTH_AUTH_ANONYMOUS", true),
            jwt,
            steam,
            itch: flag("HEARTH_AUTH_ITCH", false),
            discord,
        })
    }
}

fn required(var: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    var(name).ok_or_else(|| format!("{name} is required when the JWT provider is enabled"))
}

#[doc(hidden)]
/// Whether a provider is enabled, without touching the network or the Worker env.
pub fn check_enabled(cfg: &AuthConfig, provider: &str) -> Result<(), ProviderError> {
    let enabled = match provider {
        "anonymous" => cfg.anonymous,
        "jwt" => cfg.jwt.is_some(),
        "steam" => cfg.steam.is_some(),
        "itch" => cfg.itch,
        "discord" => cfg.discord.is_some(),
        _ => false,
    };
    if enabled {
        Ok(())
    } else {
        Err(ProviderError::Disabled)
    }
}

/// Verify one credential and return its identity. Only `Ok` may mint a token.
pub async fn verify(
    cfg: &AuthConfig,
    provider: &str,
    body: &Value,
    env: &Env,
    fetcher: &impl Fetcher,
) -> Result<Identity, ProviderError> {
    check_enabled(cfg, provider)?;
    match provider {
        "anonymous" => {
            let secret = env
                .secret(SECRET_BINDING)
                .map(|secret| secret.to_string())
                .unwrap_or_default();
            anonymous::verify(secret.as_bytes(), body)
        }
        "jwt" => {
            let config = cfg.jwt.as_ref().expect("checked above");
            jwt::verify(config, body, env, fetcher).await
        }
        "steam" => {
            let config = cfg.steam.as_ref().expect("checked above");
            steam::verify(config, body, env, fetcher).await
        }
        "itch" => itch::verify(body, fetcher).await,
        "discord" => {
            let config = cfg.discord.as_ref().expect("checked above");
            discord::verify(config, body, env, fetcher).await
        }
        _ => Err(ProviderError::Disabled),
    }
}
