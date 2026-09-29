//! OAuth2 loopback + PKCE flow against a mock token endpoint, with a fake browser.
#![cfg(feature = "oauth")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::ctx;
use ssx_upload::oauth::{BrowserOpener, ClientAuth, OAuthClient, OAuthConfig, Pkce};
use ssx_upload::{InMemorySecretStore, SecretStore, UploadError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy)]
enum Browser {
    /// Redirect back with the right state and a code.
    Good,
    /// First a forged callback (wrong state), then the genuine one.
    ForgedThenGood,
    /// The user clicks "deny".
    Denies,
    /// Never comes back.
    Silent,
    /// Cannot start at all.
    Broken,
}

struct FakeBrowser {
    behaviour: Browser,
    urls: Mutex<Vec<String>>,
    replies: Arc<Mutex<Vec<(u16, String)>>>,
}

impl FakeBrowser {
    fn new(behaviour: Browser) -> Arc<Self> {
        Arc::new(Self { behaviour, urls: Mutex::default(), replies: Arc::default() })
    }

    fn param(&self, name: &str) -> String {
        let url = url::Url::parse(&self.urls.lock().unwrap()[0]).unwrap();
        url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).unwrap_or_default()
    }
}

impl BrowserOpener for FakeBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        self.urls.lock().unwrap().push(url.to_owned());
        if matches!(self.behaviour, Browser::Broken) {
            return Err("no display".into());
        }
        let parsed = url::Url::parse(url).unwrap();
        let q = |n: &str| parsed.query_pairs().find(|(k, _)| k == n).map(|(_, v)| v.into_owned()).unwrap();
        let redirect = q("redirect_uri");
        let state = q("state");
        let behaviour = self.behaviour;
        let replies = self.replies.clone();
        tokio::spawn(async move {
            let client = ctx().http;
            let hit = |query: String| {
                let client = client.clone();
                let target = format!("{redirect}?{query}");
                let replies = replies.clone();
                async move {
                    let r = client.get(target).send().await.unwrap();
                    let status = r.status().as_u16();
                    let text = r.text().await.unwrap();
                    replies.lock().unwrap().push((status, text));
                }
            };
            match behaviour {
                Browser::Good => hit(format!("code=the-code&state={state}")).await,
                Browser::ForgedThenGood => {
                    hit("code=evil&state=forged".into()).await;
                    hit(format!("code=the-code&state={state}")).await;
                }
                Browser::Denies => hit(format!("error=access_denied&error_description=User+said+no&state={state}")).await,
                Browser::Silent | Browser::Broken => {}
            }
        });
        Ok(())
    }
}

fn config(server: &MockServer) -> OAuthConfig {
    let mut c = OAuthConfig::new("testprov", "client-abc", "https://auth.example.com/authorize?foo=bar", &format!("{}/token", server.uri()));
    c.scopes = vec!["read".into(), "write files".into()];
    c.extra_auth_params = vec![("access_type".into(), "offline".into())];
    c.timeout = Duration::from_secs(5);
    c
}

fn token_body(access: &str, refresh: Option<&str>, expires_in: u64) -> String {
    let refresh = refresh.map(|r| format!(r#","refresh_token":"{r}""#)).unwrap_or_default();
    format!(r#"{{"access_token":"{access}","token_type":"Bearer","expires_in":{expires_in},"scope":"read"{refresh}}}"#)
}

fn form(req: &wiremock::Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&req.body).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn get(f: &[(String, String)], k: &str) -> Option<String> {
    f.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

#[tokio::test]
async fn full_flow_with_pkce_state_and_token_persistence() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_string(token_body("acc-1", Some("ref-1"), 3600))).mount(&server).await;
    let browser = FakeBrowser::new(Browser::Good);
    let store = Arc::new(InMemorySecretStore::new());
    let client = OAuthClient::new(config(&server), ctx().with_secrets(store.clone()), browser.clone()).with_clock(Arc::new(|| 1_000));

    let tokens = client.authorize().await.expect("authorize");
    assert_eq!(tokens.access_token, "acc-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("ref-1"));
    assert_eq!(tokens.expires_at, Some(4_600));

    // The authorization URL.
    let url = url::Url::parse(&browser.urls.lock().unwrap()[0]).unwrap();
    assert_eq!(url.host_str(), Some("auth.example.com"));
    assert_eq!(browser.param("foo"), "bar", "existing query is preserved");
    assert_eq!(browser.param("response_type"), "code");
    assert_eq!(browser.param("client_id"), "client-abc");
    assert_eq!(browser.param("scope"), "read write files");
    assert_eq!(browser.param("access_type"), "offline");
    assert_eq!(browser.param("code_challenge_method"), "S256");
    assert_eq!(browser.param("state").len(), 22);
    let redirect = url::Url::parse(&browser.param("redirect_uri")).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    assert_eq!(redirect.path(), "/callback");
    assert!(redirect.port().is_some_and(|p| p > 1024), "ephemeral port");

    // The token request.
    let req = &server.received_requests().await.unwrap()[0];
    let f = form(req);
    assert_eq!(get(&f, "grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(get(&f, "code").as_deref(), Some("the-code"));
    assert_eq!(get(&f, "client_id").as_deref(), Some("client-abc"));
    assert_eq!(get(&f, "redirect_uri"), Some(browser.param("redirect_uri")));
    let verifier = get(&f, "code_verifier").expect("PKCE verifier");
    assert_eq!(Pkce::challenge_for(&verifier), browser.param("code_challenge"), "verifier hashes to the challenge sent earlier");
    assert!(get(&f, "client_secret").is_none());

    // The browser saw a friendly page.
    let replies = browser.replies.lock().unwrap().clone();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].0, 200);
    assert!(replies[0].1.contains("Signed in"));

    // Persisted and reusable without another round trip.
    assert_eq!(client.stored_tokens().unwrap(), Some(tokens));
    assert_eq!(client.access_token().await.unwrap(), "acc-1");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    client.sign_out().unwrap();
    assert!(store.get("oauth.testprov.tokens").unwrap().is_none());
    assert!(matches!(client.access_token().await, Err(UploadError::Auth { .. })));
}

#[tokio::test]
async fn forged_state_is_rejected_without_consuming_the_flow() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_string(token_body("acc", None, 60))).mount(&server).await;
    let browser = FakeBrowser::new(Browser::ForgedThenGood);
    let client = OAuthClient::new(config(&server), ctx(), browser.clone());
    let tokens = client.authorize().await.expect("genuine callback still completes the flow");
    assert_eq!(tokens.access_token, "acc");
    let replies = browser.replies.lock().unwrap().clone();
    assert_eq!(replies[0].0, 400, "forged callback answered with 400");
    assert_eq!(replies[1].0, 200);
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "the forged code was never exchanged");
    assert_eq!(get(&form(&reqs[0]), "code").as_deref(), Some("the-code"));
}

#[tokio::test]
async fn denial_timeout_cancellation_and_broken_browser() {
    let server = MockServer::start().await;

    let denied = OAuthClient::new(config(&server), ctx(), FakeBrowser::new(Browser::Denies));
    match denied.authorize().await.unwrap_err() {
        UploadError::Auth { message } => assert!(message.contains("access_denied") && message.contains("User said no"), "{message}"),
        other => panic!("{other:?}"),
    }

    let mut quick = config(&server);
    quick.timeout = Duration::from_millis(300);
    let slow = OAuthClient::new(quick, ctx(), FakeBrowser::new(Browser::Silent));
    assert!(matches!(slow.authorize().await.unwrap_err(), UploadError::Auth { message } if message.contains("timed out")));

    let c = ctx();
    let token = c.cancel.clone();
    let cancelled = OAuthClient::new(config(&server), c, FakeBrowser::new(Browser::Silent));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        token.cancel();
    });
    assert!(cancelled.authorize().await.unwrap_err().is_cancelled());

    let broken = OAuthClient::new(config(&server), ctx(), FakeBrowser::new(Browser::Broken));
    match broken.authorize().await.unwrap_err() {
        UploadError::Auth { message } => assert!(message.contains("no display") && message.contains("https://auth.example.com/authorize"), "URL is included for manual use: {message}"),
        other => panic!("{other:?}"),
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn loopback_listener_ignores_strays_and_rejects_foreign_hosts() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_string(token_body("acc", None, 60))).mount(&server).await;

    struct Prober(Mutex<Vec<(u16, u16)>>);
    impl BrowserOpener for Prober {
        fn open(&self, url: &str) -> Result<(), String> {
            let parsed = url::Url::parse(url).unwrap();
            let q = |n: &str| parsed.query_pairs().find(|(k, _)| k == n).map(|(_, v)| v.into_owned()).unwrap();
            let redirect = url::Url::parse(&q("redirect_uri")).unwrap();
            let (port, state) = (redirect.port().unwrap(), q("state"));
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                let raw = |req: String| async move {
                    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                    s.write_all(req.as_bytes()).await.unwrap();
                    let mut out = String::new();
                    s.read_to_string(&mut out).await.unwrap();
                    out.split(' ').nth(1).unwrap().parse::<u16>().unwrap()
                };
                // favicon: 404, flow continues
                assert_eq!(raw(format!("GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")).await, 404);
                // rebinding attempt: wrong Host header
                assert_eq!(raw(format!("GET /callback?code=x&state={state} HTTP/1.1\r\nHost: evil.example.com\r\n\r\n")).await, 400);
                // wrong method
                assert_eq!(raw(format!("POST /callback?code=x&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 0\r\n\r\n")).await, 404);
                // missing code
                assert_eq!(raw(format!("GET /callback?state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")).await, 400);
                // garbage
                assert_eq!(raw("\r\n\r\n".to_owned()).await, 400);
                assert_eq!(raw(format!("GET /callback?code=real&state={state} HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n")).await, 200);
            });
            Ok(())
        }
    }
    let client = OAuthClient::new(config(&server), ctx(), Arc::new(Prober(Mutex::default())));
    client.authorize().await.expect("flow completes after the noise");
    assert_eq!(get(&form(&server.received_requests().await.unwrap()[0]), "code").as_deref(), Some("real"));
}

#[tokio::test]
async fn token_endpoint_errors_and_client_auth_modes() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(400).set_body_string(r#"{"error":"invalid_grant","error_description":"code expired"}"#)).up_to_n_times(1).mount(&server).await;
    let client = OAuthClient::new(config(&server), ctx(), FakeBrowser::new(Browser::Good));
    match client.authorize().await.unwrap_err() {
        UploadError::Auth { message } => assert!(message.contains("invalid_grant") && message.contains("code expired") && message.contains("sign in again"), "{message}"),
        other => panic!("{other:?}"),
    }
    // Providers that answer 200 with an error body (GitHub style).
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_string(r#"{"error":"bad_verification_code"}"#)).up_to_n_times(1).mount(&server).await;
    assert!(matches!(client.authorize().await, Err(UploadError::Auth { .. })));
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_string("<html>")).up_to_n_times(1).mount(&server).await;
    assert!(matches!(client.authorize().await, Err(UploadError::InvalidResponse { .. })));
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(500)).up_to_n_times(1).mount(&server).await;
    assert!(client.authorize().await.unwrap_err().is_retryable());

    server.reset().await;
    Mock::given(method("POST")).and(path("/token")).respond_with(ResponseTemplate::new(200).set_body_string(r#"{"access_token":"a","expires_in":"120"}"#)).mount(&server).await;
    let mut basic = config(&server);
    basic.client_auth = ClientAuth::BasicHeader;
    basic.client_secret = Some("s3cret".into());
    basic.use_pkce = false;
    let browser = FakeBrowser::new(Browser::Good);
    let t = OAuthClient::new(basic, ctx(), browser.clone()).with_clock(Arc::new(|| 10)).authorize().await.unwrap();
    assert_eq!(t.expires_at, Some(130), "string expires_in is accepted");
    assert_eq!(t.token_type, "Bearer", "defaults when omitted");
    let req = &server.received_requests().await.unwrap()[0];
    let f = form(req);
    assert!(get(&f, "client_secret").is_none() && get(&f, "client_id").is_none() && get(&f, "code_verifier").is_none());
    assert_eq!(req.headers.get("authorization").unwrap().to_str().unwrap(), "Basic Y2xpZW50LWFiYzpzM2NyZXQ=");
    assert!(!browser.urls.lock().unwrap()[0].contains("code_challenge"), "PKCE can be disabled");
}

#[tokio::test]
async fn refresh_rotates_persists_and_is_single_flight() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicU64::new(0));
    let c2 = calls.clone();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |_: &wiremock::Request| {
            let n = c2.fetch_add(1, Ordering::SeqCst) + 1;
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_string(token_body(&format!("acc-{}", n + 1), if n == 1 { Some("ref-2") } else { None }, 3600))
        })
        .mount(&server)
        .await;
    let store = Arc::new(InMemorySecretStore::new());
    let now = Arc::new(AtomicU64::new(1_000));
    let n2 = now.clone();
    let client = OAuthClient::new(config(&server), ctx().with_secrets(store.clone()), FakeBrowser::new(Browser::Good))
        .with_clock(Arc::new(move || n2.load(Ordering::SeqCst)));
    store.set("oauth.testprov.tokens", r#"{"access_token":"acc-1","refresh_token":"ref-1","token_type":"Bearer","scope":null,"expires_at":1030}"#).unwrap();

    // Within the 60 s skew: refresh needed. Five concurrent callers, exactly one refresh.
    let handles: Vec<_> = (0..5).map(|_| { let c = client.clone(); tokio::spawn(async move { c.access_token().await }) }).collect();
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), "acc-2");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let reqs = server.received_requests().await.unwrap();
    let f = form(&reqs[0]);
    assert_eq!(get(&f, "grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(get(&f, "refresh_token").as_deref(), Some("ref-1"));
    let stored = client.stored_tokens().unwrap().unwrap();
    assert_eq!(stored.refresh_token.as_deref(), Some("ref-2"), "rotated refresh token is persisted");
    assert_eq!(stored.expires_at, Some(4_600));

    // A response without refresh_token keeps the old one.
    now.store(10_000, Ordering::SeqCst);
    assert_eq!(client.access_token().await.unwrap(), "acc-3");
    assert_eq!(client.stored_tokens().unwrap().unwrap().refresh_token.as_deref(), Some("ref-2"));
    assert_eq!(form(&server.received_requests().await.unwrap()[1]).iter().find(|(k, _)| k == "refresh_token").unwrap().1, "ref-2");

    // Expired without a refresh token: actionable error, no request.
    store.set("oauth.testprov.tokens", r#"{"access_token":"x","refresh_token":null,"token_type":"Bearer","scope":null,"expires_at":5}"#).unwrap();
    assert!(matches!(client.access_token().await, Err(UploadError::Auth { message }) if message.contains("no refresh token")));
    // Corrupt storage is an Auth error, not a panic.
    store.set("oauth.testprov.tokens", "{not json").unwrap();
    assert!(matches!(client.access_token().await, Err(UploadError::Auth { message }) if message.contains("corrupt")));
}
