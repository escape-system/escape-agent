//! OAuth discovery, PKCE exchange and refresh against a fake authorization server.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::extract::{Form, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use oc_mcp::config::OAuthConfig;
use oc_mcp::oauth::{CredentialStore, MemoryCredentialStore, OAuthSession, code_challenge};
use serde_json::{Value, json};
use tokio::net::TcpListener;

struct OAuthState {
    base: String,
    token_requests: std::sync::Mutex<Vec<HashMap<String, String>>>,
    refreshes: AtomicU32,
}

async fn metadata(State(state): State<Arc<OAuthState>>) -> Json<Value> {
    Json(json!({
        "issuer": state.base,
        "authorization_endpoint": format!("{}/authorize", state.base),
        "token_endpoint": format!("{}/token", state.base),
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"]
    }))
}

async fn token(
    State(state): State<Arc<OAuthState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Json<Value> {
    let grant = form.get("grant_type").cloned().unwrap_or_default();
    if grant == "refresh_token" {
        state.refreshes.fetch_add(1, Ordering::SeqCst);
    }
    state
        .token_requests
        .lock()
        .expect("token requests")
        .push(form);
    if grant == "refresh_token" {
        Json(json!({
            "access_token": "access-2",
            "token_type": "Bearer",
            "expires_in": 3600
        }))
    } else {
        Json(json!({
            "access_token": "access-1",
            "token_type": "Bearer",
            "expires_in": 3600,
            "refresh_token": "refresh-1",
            "scope": "repo"
        }))
    }
}

#[tokio::test]
async fn oauth_metadata_pkce_exchange_and_refresh() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let base = format!("http://{address}");
    let state = Arc::new(OAuthState {
        base: base.clone(),
        token_requests: std::sync::Mutex::new(Vec::new()),
        refreshes: AtomicU32::new(0),
    });
    let app = Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/token", post(token))
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let store = Arc::new(MemoryCredentialStore::new());
    let session = OAuthSession::new(
        "remote",
        url::Url::parse(&format!("{base}/mcp")).expect("url"),
        OAuthConfig {
            client_id: Some("client-id".to_owned()),
            client_secret: None,
            scope: Some("repo".to_owned()),
            callback_port: None,
            redirect_uri: None,
            auth_server_metadata_url: Some(format!(
                "{base}/.well-known/oauth-authorization-server"
            )),
        },
        store.clone(),
    )
    .expect("session");

    let authorization = session
        .begin("http://127.0.0.1:43123/callback")
        .await
        .expect("begin");
    assert_eq!(authorization.url.path(), "/authorize");
    let query: HashMap<String, String> = authorization.url.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], "client-id");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["resource"], format!("{base}/mcp"));
    assert_eq!(query["state"], authorization.state);
    let challenge = query["code_challenge"].clone();

    let tokens = session.complete("code-123").await.expect("complete");
    assert_eq!(tokens.access_token, "access-1");
    assert_eq!(session.access_token().await.expect("token"), "access-1");

    {
        let requests = state.token_requests.lock().expect("requests");
        let exchange = &requests[0];
        assert_eq!(exchange["grant_type"], "authorization_code");
        assert_eq!(exchange["code"], "code-123");
        assert_eq!(
            code_challenge(&exchange["code_verifier"]),
            challenge,
            "verifier must match the advertised challenge"
        );
        assert_eq!(exchange["redirect_uri"], "http://127.0.0.1:43123/callback");
        assert_eq!(exchange["resource"], format!("{base}/mcp"));
    }

    // Force expiry inside the 60 s skew and refresh.
    let mut stored = store.load("remote").await.expect("load").expect("stored");
    stored.expires_at = Some(0);
    store.save("remote", &stored).await.expect("save");
    assert_eq!(session.access_token().await.expect("refreshed"), "access-2");
    assert_eq!(state.refreshes.load(Ordering::SeqCst), 1);
    {
        let requests = state.token_requests.lock().expect("requests");
        let refresh = &requests[1];
        assert_eq!(refresh["grant_type"], "refresh_token");
        assert_eq!(refresh["refresh_token"], "refresh-1");
    }

    // The refreshed credential is persisted with a new expiry.
    let stored = store.load("remote").await.expect("load").expect("stored");
    assert_eq!(stored.tokens.expect("tokens").access_token, "access-2");
    assert!(stored.expires_at.is_some_and(|expires| expires > 0));
}
