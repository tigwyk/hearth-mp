//! v0.3b Task 3: the Steam, itch.io and Discord providers.
//!
//! Every provider is tested through its Workers-free inner function against a
//! [`CannedFetcher`], so no Workers `Env` or network is needed. The tests pin the
//! security rules from Review Focus 4: an identity is only produced from a
//! verified OK, and a provider outage is a 502 "`<provider>` unavailable".

use futures::executor::block_on;
use hearth_mp_gateway::providers::discord::{self, DiscordConfig};
use hearth_mp_gateway::providers::itch;
use hearth_mp_gateway::providers::steam::{self, SteamConfig};
use hearth_mp_gateway::{CannedFetcher, Identity, ProviderError};
use serde_json::{json, Value};

const STEAM_PREFIX: &str =
    "https://partner.steam-api.com/ISteamUserAuth/AuthenticateUserTicket/v1/";
const ITCH_ME: &str = "https://itch.io/api/1/jwt/me";
const DISCORD_TOKEN: &str = "https://discord.com/api/oauth2/token";
const DISCORD_ME: &str = "https://discord.com/api/users/@me";

const STEAM_KEY: &str = "steam-web-api-key-SECRET-0123456789";
const ITCH_KEY: &str = "itch-api-key-123";
const DISCORD_SECRET: &str = "discord-client-secret-456";
const STEAM_ID: &str = "76561198000000000";
const DISCORD_ID: &str = "80351110224678912";
const ACCESS_TOKEN: &str = "discord-access-token-value";

fn steam_config() -> SteamConfig {
    SteamConfig::new(480, "HEARTH_STEAM_KEY")
}

fn discord_config(redirect_uri: Option<&str>) -> DiscordConfig {
    let mut config = DiscordConfig::new("client-id", "HEARTH_DISCORD_SECRET");
    config.redirect_uri = redirect_uri.map(str::to_owned);
    config
}

fn steam_body() -> Value {
    json!({ "ticket": "deadbeef" })
}

fn discord_body() -> Value {
    json!({ "code": "oauth-code" })
}

fn steam_response(params: Value) -> String {
    json!({ "response": { "params": params } }).to_string()
}

fn steam_ok_body() -> String {
    steam_response(json!({
        "result": "OK",
        "steamid": STEAM_ID,
        "ownersteamid": STEAM_ID,
        "vacbanned": false,
        "publisherbanned": false,
    }))
}

fn call_steam(
    cfg: &SteamConfig,
    body: &Value,
    fetcher: &CannedFetcher,
) -> Result<Identity, ProviderError> {
    block_on(steam::verify_with_key(cfg, body, STEAM_KEY, fetcher))
}

fn call_itch(fetcher: &CannedFetcher) -> Result<Identity, ProviderError> {
    block_on(itch::verify(&json!({ "api_key": ITCH_KEY }), fetcher))
}

fn call_discord(cfg: &DiscordConfig, fetcher: &CannedFetcher) -> Result<Identity, ProviderError> {
    block_on(discord::verify_with_secret(
        cfg,
        &discord_body(),
        DISCORD_SECRET,
        fetcher,
    ))
}

/// One call path per provider for the shared upstream-failure tests.
fn call(provider: &str, fetcher: &CannedFetcher) -> Result<Identity, ProviderError> {
    match provider {
        "steam" => call_steam(&steam_config(), &steam_body(), fetcher),
        "itch" => call_itch(fetcher),
        "discord" => call_discord(&discord_config(None), fetcher),
        other => panic!("unknown provider {other}"),
    }
}

/// A fetcher whose Discord calls both succeed, for the redirect-URI test.
fn discord_ok_fetcher() -> CannedFetcher {
    CannedFetcher::new()
        .route(
            DISCORD_TOKEN,
            200,
            &json!({ "access_token": ACCESS_TOKEN, "token_type": "Bearer" }).to_string(),
        )
        .route(
            DISCORD_ME,
            200,
            &json!({
                "id": DISCORD_ID,
                "username": "cooluser",
                "global_name": "Cool User",
                "discriminator": "0",
            })
            .to_string(),
        )
}

#[test]
fn steam_ok() {
    let fetcher = CannedFetcher::new().route(STEAM_PREFIX, 200, &steam_ok_body());
    let identity = call_steam(&steam_config(), &steam_body(), &fetcher).expect("steam verifies");
    assert_eq!(identity.player_id.0, format!("steam_{STEAM_ID}"));
    assert_eq!(identity.provider, "steam");
    assert_eq!(identity.display_name, None);

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert!(request.url.starts_with(STEAM_PREFIX), "{}", request.url);
    assert!(request.url.contains("appid=480"), "{}", request.url);
    assert!(request.url.contains("ticket=deadbeef"), "{}", request.url);
    assert!(request.url.contains("identity=hearth"), "{}", request.url);
    assert!(request.body.is_none());
}

#[test]
fn steam_error_desc_rejected() {
    let fetch = json!({
        "response": { "error": { "errorcode": 101, "errordesc": "Invalid ticket" } }
    })
    .to_string();
    let fetcher = CannedFetcher::new().route(STEAM_PREFIX, 200, &fetch);
    let err = call_steam(&steam_config(), &steam_body(), &fetcher).expect_err("invalid ticket");
    assert_eq!(err, ProviderError::Rejected("Invalid ticket".to_owned()));
    assert_eq!(err.status(), 401);
}

#[test]
fn steam_banned_rejected() {
    for field in ["vacbanned", "publisherbanned"] {
        let mut params = json!({ "result": "OK", "steamid": STEAM_ID });
        params[field] = Value::Bool(true);
        let fetcher = CannedFetcher::new().route(STEAM_PREFIX, 200, &steam_response(params));
        let err = call_steam(&steam_config(), &steam_body(), &fetcher).expect_err("banned");
        assert_eq!(err, ProviderError::Rejected("banned".to_owned()), "{field}");
        assert_eq!(err.status(), 401, "{field}");
    }
}

#[test]
fn steam_banned_allowed_when_configured() {
    let mut params = json!({ "result": "OK", "steamid": STEAM_ID });
    params["publisherbanned"] = Value::Bool(true);
    params["vacbanned"] = Value::Bool(true);
    let fetcher = CannedFetcher::new().route(STEAM_PREFIX, 200, &steam_response(params));
    let mut config = steam_config();
    config.reject_banned = false;
    let identity = call_steam(&config, &steam_body(), &fetcher).expect("bans are allowed");
    assert_eq!(identity.player_id.0, format!("steam_{STEAM_ID}"));
}

#[test]
fn steam_ticket_too_long_400() {
    let ticket = "a".repeat(4097);
    let fetcher = CannedFetcher::new().route(STEAM_PREFIX, 200, &steam_ok_body());
    let err = call_steam(&steam_config(), &json!({ "ticket": ticket }), &fetcher)
        .expect_err("oversized ticket");
    assert!(matches!(err, ProviderError::BadRequest(_)), "{err:?}");
    assert_eq!(err.status(), 400);
    assert!(
        fetcher.requests().is_empty(),
        "no upstream call for a bad body"
    );
}

#[test]
fn steam_key_not_in_error_text() {
    // A secret must never reach an error message or a log line: errors are built
    // from response fields only.
    for fetcher in [
        CannedFetcher::new().failing("timeout"),
        CannedFetcher::new().route(STEAM_PREFIX, 500, "upstream exploded"),
        CannedFetcher::new().route(STEAM_PREFIX, 200, "<html>not json</html>"),
    ] {
        let err = call_steam(&steam_config(), &steam_body(), &fetcher).expect_err("upstream down");
        assert_eq!(err, ProviderError::Upstream("steam unavailable".to_owned()));
        assert!(
            !format!("{err:?}").contains(STEAM_KEY),
            "the key leaked into {err:?}"
        );
        assert!(
            !format!("{err:?}").contains(&format!("key={STEAM_KEY}")),
            "the key leaked into {err:?}"
        );
    }
}

#[test]
fn itch_ok_uses_display_name() {
    let fetch = json!({
        "user": { "id": 123456, "username": "itchy", "display_name": "Itchy Player" }
    })
    .to_string();
    let fetcher = CannedFetcher::new().route(ITCH_ME, 200, &fetch);
    let identity = call_itch(&fetcher).expect("itch verifies");
    assert_eq!(identity.player_id.0, "itch_123456");
    assert_eq!(identity.provider, "itch");
    assert_eq!(identity.display_name.as_deref(), Some("Itchy Player"));

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(request.url, ITCH_ME);
    assert_eq!(
        request.headers,
        vec![("Authorization".to_owned(), format!("Bearer {ITCH_KEY}"))]
    );
    assert!(request.body.is_none());
}

#[test]
fn itch_falls_back_to_username() {
    for fetch in [
        json!({ "user": { "id": "42", "username": "itchy", "display_name": null } }).to_string(),
        json!({ "user": { "id": "42", "username": "itchy" } }).to_string(),
        // A name that sanitises away counts as absent, so the username wins.
        json!({ "user": { "id": "42", "username": "itchy", "display_name": " \r\n" } }).to_string(),
    ] {
        let fetcher = CannedFetcher::new().route(ITCH_ME, 200, &fetch);
        let identity = call_itch(&fetcher).expect("itch verifies");
        assert_eq!(identity.player_id.0, "itch_42");
        assert_eq!(identity.display_name.as_deref(), Some("itchy"));
    }
}

#[test]
fn itch_401_rejected() {
    // A 401 is a rejected key.
    let fetcher =
        CannedFetcher::new().route(ITCH_ME, 401, &json!({ "errors": ["bad"] }).to_string());
    let err = call_itch(&fetcher).expect_err("bad key");
    assert_eq!(err, ProviderError::Rejected("itch rejected".to_owned()));
    assert_eq!(err.status(), 401);

    // itch also reports a bad key as a 200 with an `errors` array.
    let fetch = json!({ "errors": ["invalid api key"] }).to_string();
    let fetcher = CannedFetcher::new().route(ITCH_ME, 200, &fetch);
    let err = call_itch(&fetcher).expect_err("bad key");
    assert_eq!(err, ProviderError::Rejected("invalid api key".to_owned()));
    assert_eq!(err.status(), 401);
}

#[test]
fn discord_ok_two_calls_in_order() {
    let fetcher = discord_ok_fetcher();
    let identity = call_discord(&discord_config(None), &fetcher).expect("discord verifies");
    assert_eq!(identity.player_id.0, format!("discord_{DISCORD_ID}"));
    assert_eq!(identity.provider, "discord");
    assert_eq!(identity.display_name.as_deref(), Some("Cool User"));

    let requests = fetcher.requests();
    assert_eq!(requests.len(), 2);

    let token_request = &requests[0];
    assert_eq!(token_request.method, "POST");
    assert_eq!(token_request.url, DISCORD_TOKEN);
    assert_eq!(
        token_request.headers,
        vec![(
            "Content-Type".to_owned(),
            "application/x-www-form-urlencoded".to_owned()
        )]
    );
    let body = token_request.body.as_deref().expect("a form body");
    assert!(body.contains("client_id=client-id"), "{body}");
    assert!(
        body.contains(&format!("client_secret={DISCORD_SECRET}")),
        "{body}"
    );
    assert!(body.contains("grant_type=authorization_code"), "{body}");
    assert!(body.contains("code=oauth-code"), "{body}");
    assert!(!body.contains("redirect_uri"), "{body}");

    let user_request = &requests[1];
    assert_eq!(user_request.method, "GET");
    assert_eq!(user_request.url, DISCORD_ME);
    assert_eq!(
        user_request.headers,
        vec![("Authorization".to_owned(), format!("Bearer {ACCESS_TOKEN}"))]
    );
    assert!(user_request.body.is_none());
}

#[test]
fn discord_falls_back_to_username() {
    for fetch in [
        json!({ "id": DISCORD_ID, "username": "cooluser", "global_name": null }).to_string(),
        json!({ "id": DISCORD_ID, "username": "cooluser" }).to_string(),
        // A name that sanitises away counts as absent, so the username wins.
        json!({ "id": DISCORD_ID, "username": "cooluser", "global_name": " \r\n" }).to_string(),
    ] {
        let fetcher = CannedFetcher::new()
            .route(
                DISCORD_TOKEN,
                200,
                &json!({ "access_token": ACCESS_TOKEN }).to_string(),
            )
            .route(DISCORD_ME, 200, &fetch);
        let identity = call_discord(&discord_config(None), &fetcher).expect("discord verifies");
        assert_eq!(identity.display_name.as_deref(), Some("cooluser"));
    }
}

#[test]
fn discord_token_exchange_400_rejected() {
    let fetch = json!({ "error": "invalid_grant" }).to_string();
    let fetcher = CannedFetcher::new()
        .route(DISCORD_TOKEN, 400, &fetch)
        .route(
            DISCORD_ME,
            200,
            &json!({ "id": DISCORD_ID, "username": "u" }).to_string(),
        );
    let err = call_discord(&discord_config(None), &fetcher).expect_err("bad code");
    assert_eq!(err, ProviderError::Rejected("discord rejected".to_owned()));
    assert_eq!(err.status(), 401);
    assert_eq!(
        fetcher.requests().len(),
        1,
        "a failed exchange must not call /users/@me"
    );
}

#[test]
fn discord_user_call_401_rejected() {
    let fetcher = CannedFetcher::new()
        .route(
            DISCORD_TOKEN,
            200,
            &json!({ "access_token": ACCESS_TOKEN }).to_string(),
        )
        .route(DISCORD_ME, 401, "{}");
    let err = call_discord(&discord_config(None), &fetcher).expect_err("bad token");
    assert_eq!(err, ProviderError::Rejected("discord rejected".to_owned()));
    assert_eq!(fetcher.requests().len(), 2);
}

#[test]
fn discord_redirect_uri_sent_only_when_configured() {
    let redirect = "https://example.com/hearth/callback";
    let fetcher = discord_ok_fetcher();
    call_discord(&discord_config(Some(redirect)), &fetcher).expect("discord verifies");
    let body = fetcher.requests()[0].body.clone().expect("a form body");
    assert!(
        body.contains("redirect_uri=https%3A%2F%2Fexample.com%2Fhearth%2Fcallback"),
        "{body}"
    );

    let fetcher = discord_ok_fetcher();
    call_discord(&discord_config(None), &fetcher).expect("discord verifies");
    let body = fetcher.requests()[0].body.clone().expect("a form body");
    assert!(!body.contains("redirect_uri"), "{body}");
}

#[test]
fn upstream_500_is_502() {
    for provider in ["steam", "itch", "discord"] {
        let fetcher = CannedFetcher::new().route("", 500, "upstream broke");
        let err = call(provider, &fetcher).expect_err("a 500 cannot verify");
        assert_eq!(
            err,
            ProviderError::Upstream(format!("{provider} unavailable")),
            "{provider}"
        );
        assert_eq!(err.status(), 502, "{provider}");
        assert_eq!(fetcher.requests().len(), 1, "{provider}");
    }
}

#[test]
fn upstream_html_is_502() {
    for provider in ["steam", "itch", "discord"] {
        let fetcher = CannedFetcher::new().route("", 200, "<html>maintenance</html>");
        let err = call(provider, &fetcher).expect_err("HTML cannot verify");
        assert_eq!(
            err,
            ProviderError::Upstream(format!("{provider} unavailable")),
            "{provider}"
        );
        assert_eq!(err.status(), 502, "{provider}");
    }
}

#[test]
fn upstream_timeout_is_502() {
    for provider in ["steam", "itch", "discord"] {
        let fetcher = CannedFetcher::new().failing("timeout");
        let err = call(provider, &fetcher).expect_err("a timeout cannot verify");
        assert_eq!(
            err,
            ProviderError::Upstream(format!("{provider} unavailable")),
            "{provider}"
        );
        assert_eq!(err.status(), 502, "{provider}");
    }
}
