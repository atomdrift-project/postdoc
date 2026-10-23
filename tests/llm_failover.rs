//! Exercise the same config resolver and HTTP client postdoc uses for both
//! its file interpreter and isomer's differential interpreter.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use scan::cli::{GlobalArgs, LlmEnv};
use scan::interpret::{InterpretConfig, chat};
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    global: GlobalArgs,
}

#[derive(Debug)]
struct Request {
    endpoint: &'static str,
    authorization: String,
    body: Value,
}

type Requests = Arc<Mutex<Vec<Request>>>;

struct Endpoint {
    url: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Endpoint {
    fn start(
        name: &'static str,
        models_status: StatusCode,
        chat_status: StatusCode,
        delay: Duration,
        requests: Requests,
    ) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let expected_auth = if name == "openrouter" {
                    "Bearer router-test-token"
                } else {
                    "Bearer internal-test-token"
                };
                let model = format!("{name}-8B");
                let app = Router::new()
                    .route(
                        "/v1/models",
                        get(move |headers: HeaderMap| async move {
                            let status = if headers.get("authorization").unwrap() == expected_auth {
                                models_status
                            } else {
                                StatusCode::UNAUTHORIZED
                            };
                            (status, Json(json!({"data": [{"id": model}]})))
                        }),
                    )
                    .route(
                        "/v1/chat/completions",
                        post(move |headers: HeaderMap, Json(body): Json<Value>| {
                            let requests = Arc::clone(&requests);
                            async move {
                                let authorization = headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .unwrap_or_default()
                                    .to_owned();
                                let status = if authorization == expected_auth {
                                    chat_status
                                } else {
                                    StatusCode::UNAUTHORIZED
                                };
                                requests.lock().unwrap().push(Request {
                                    endpoint: name,
                                    authorization,
                                    body,
                                });
                                tokio::time::sleep(delay).await;
                                (
                                    status,
                                    Json(json!({"choices": [{"message": {
                                        "role": "assistant",
                                        "content": format!("answered by {name}")
                                    }}]})),
                                )
                            }
                        }),
                    );
                axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app)
                    .with_graceful_shutdown(async {
                        let _ = stopped.await;
                    })
                    .await
                    .unwrap();
            });
        });
        Self {
            url,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Chain {
    endpoints: [Endpoint; 3],
    requests: Requests,
    env: LlmEnv,
    _tokens: tempfile::TempDir,
}

impl Chain {
    fn new(models_status: StatusCode, statuses: [StatusCode; 3]) -> Self {
        let requests = Requests::default();
        let endpoints: [Endpoint; 3] = ["primary", "backup", "openrouter"]
            .into_iter()
            .zip(statuses)
            .map(|(name, status)| {
                Endpoint::start(
                    name,
                    if name == "primary" {
                        models_status
                    } else {
                        StatusCode::OK
                    },
                    status,
                    Duration::ZERO,
                    Arc::clone(&requests),
                )
            })
            .collect::<Vec<_>>()
            .try_into()
            .unwrap_or_else(|_| unreachable!());
        let tokens = tempfile::tempdir().unwrap();
        let internal = tokens.path().join("llm");
        let router = tokens.path().join("openrouter");
        std::fs::write(&internal, "internal-test-token\n").unwrap();
        std::fs::write(&router, "router-test-token\n").unwrap();
        let env = LlmEnv {
            target: Some(format!(
                "{},{},openrouter",
                endpoints[0].url, endpoints[1].url
            )),
            llm_token_file: Some(internal),
            openrouter_token_file: Some(router),
            ..Default::default()
        };
        Self {
            endpoints,
            requests,
            env,
            _tokens: tokens,
        }
    }

    fn config(&self) -> InterpretConfig {
        let mut config = Args::parse_from(["postdoc"])
            .global
            .interpret_config_with(&self.env)
            .unwrap()
            .unwrap();
        // Resolve the real named target (and its separate credentials/default
        // model), then route its HTTP calls to our OpenRouter stand-in.
        let router = config.fallbacks.last_mut().unwrap();
        assert_eq!(router.base_url, scan::interpret::OPENROUTER_BASE_URL);
        assert_eq!(router.model, "openrouter/auto");
        router.base_url.clone_from(&self.endpoints[2].url);
        config.timeout = Duration::from_secs(2);
        config
    }

    fn assert_attempts(&self, expected: &[&str]) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(
            requests.iter().map(|r| r.endpoint).collect::<Vec<_>>(),
            expected
        );
        for request in requests.iter() {
            let (key, model) = if request.endpoint == "openrouter" {
                ("router-test-token", "openrouter/auto".to_owned())
            } else {
                ("internal-test-token", format!("{}-8B", request.endpoint))
            };
            assert_eq!(request.authorization, format!("Bearer {key}"));
            assert_eq!(request.body["model"], model);
            assert_eq!(request.body["messages"][1]["content"], "Reply OK.");
        }
    }
}

#[test]
fn healthy_primary_leaves_both_backups_unused() {
    let chain = Chain::new(StatusCode::OK, [StatusCode::OK; 3]);
    assert_eq!(
        chat(&chain.config(), "test", "Reply OK.", 8).unwrap(),
        "answered by primary"
    );
    chain.assert_attempts(&["primary"]);
}

#[test]
fn rejected_primary_uses_private_backup_before_openrouter() {
    let chain = Chain::new(
        StatusCode::OK,
        [StatusCode::UNAUTHORIZED, StatusCode::OK, StatusCode::OK],
    );
    assert_eq!(
        chat(&chain.config(), "test", "Reply OK.", 8).unwrap(),
        "answered by backup"
    );
    chain.assert_attempts(&["primary", "backup"]);
}

#[test]
fn both_internal_endpoints_failing_reaches_openrouter_with_its_own_key() {
    let chain = Chain::new(
        StatusCode::OK,
        [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::BAD_GATEWAY,
            StatusCode::OK,
        ],
    );
    assert_eq!(
        chat(&chain.config(), "test", "Reply OK.", 8).unwrap(),
        "answered by openrouter"
    );
    chain.assert_attempts(&["primary", "backup", "openrouter"]);
}

#[test]
fn failed_model_discovery_retains_the_remaining_chain() {
    let chain = Chain::new(StatusCode::SERVICE_UNAVAILABLE, [StatusCode::OK; 3]);
    assert_eq!(
        chat(&chain.config(), "test", "Reply OK.", 8).unwrap(),
        "answered by backup"
    );
    chain.assert_attempts(&["backup"]);
}

#[test]
fn missing_openrouter_key_keeps_internal_failover_available() {
    let mut chain = Chain::new(
        StatusCode::OK,
        [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::OK,
            StatusCode::OK,
        ],
    );
    chain.env.openrouter_token_file = None;
    let config = Args::parse_from(["postdoc"])
        .global
        .interpret_config_with(&chain.env)
        .unwrap()
        .unwrap();
    assert_eq!(config.fallbacks.len(), 1);
    assert_eq!(config.fallbacks[0].base_url, chain.endpoints[1].url);
    assert_eq!(
        chat(&config, "test", "Reply OK.", 8).unwrap(),
        "answered by backup"
    );
    chain.assert_attempts(&["primary", "backup"]);
}

#[test]
fn connection_failure_after_startup_uses_backup() {
    let chain = Chain::new(StatusCode::OK, [StatusCode::OK; 3]);
    let mut config = chain.config();
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.base_url = format!("http://{}/v1", unavailable.local_addr().unwrap());
    drop(unavailable);
    assert_eq!(
        chat(&config, "test", "Reply OK.", 8).unwrap(),
        "answered by backup"
    );
    chain.assert_attempts(&["backup"]);
}

#[test]
fn timed_out_primary_uses_backup() {
    let chain = Chain::new(StatusCode::OK, [StatusCode::OK; 3]);
    let mut config = chain.config();
    let slow = Endpoint::start(
        "primary",
        StatusCode::OK,
        StatusCode::OK,
        Duration::from_secs(1),
        Arc::clone(&chain.requests),
    );
    config.base_url.clone_from(&slow.url);
    config.timeout = Duration::from_millis(100);
    assert_eq!(
        chat(&config, "test", "Reply OK.", 8).unwrap(),
        "answered by backup"
    );
    chain.assert_attempts(&["primary", "backup"]);
}

#[test]
fn all_endpoints_failing_returns_an_error() {
    let chain = Chain::new(
        StatusCode::OK,
        [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::BAD_GATEWAY,
            StatusCode::PAYMENT_REQUIRED,
        ],
    );
    let error = chat(&chain.config(), "test", "Reply OK.", 8).unwrap_err();
    assert!(format!("{error:#}").contains("402"), "{error:#}");
    chain.assert_attempts(&["primary", "backup", "openrouter"]);
}

fn live_endpoint(target: &str) -> anyhow::Result<()> {
    let mut env = LlmEnv::from_process()?;
    env.target = Some(target.to_owned());
    env.model = None;
    let mut config = Args::parse_from(["postdoc"])
        .global
        .interpret_config_with(&env)?
        .unwrap();
    config.timeout = Duration::from_secs(30);
    let reply = chat(
        &config,
        "Follow the user's instructions.",
        "Reply exactly OK.",
        16,
    )?;
    anyhow::ensure!(!reply.trim().is_empty(), "empty completion from {target}");
    println!("PASS {target}: model={}, reply={reply:?}", config.model);
    Ok(())
}

#[test]
#[ignore = "contacts the live isotope13 LLM using local credentials"]
fn live_isotope13() -> anyhow::Result<()> {
    live_endpoint("https://llm.isotope13.ai/v1")
}

#[test]
#[ignore = "contacts the live private LLM using local credentials"]
fn live_private_backup() -> anyhow::Result<()> {
    live_endpoint("http://10.9.8.14:8000/v1")
}

#[test]
#[ignore = "forces primary failure, then contacts the live private LLM"]
fn live_failover_to_private_backup() -> anyhow::Result<()> {
    let requests = Requests::default();
    let primary = Endpoint::start(
        "primary",
        StatusCode::OK,
        StatusCode::SERVICE_UNAVAILABLE,
        Duration::ZERO,
        Arc::clone(&requests),
    );
    let mut env = LlmEnv::from_process()?;
    env.target = Some(format!(
        "{},http://10.9.8.14:8000/v1,openrouter",
        primary.url
    ));
    // Pin only the refused primary, so the real backup still discovers its model.
    env.model = Some("primary-8B,,".to_owned());
    let mut config = Args::parse_from(["postdoc"])
        .global
        .interpret_config_with(&env)?
        .unwrap();
    // The stand-in uses a fixture credential; the real endpoints keep their own.
    config.api_key = Some("internal-test-token".to_owned());
    config.timeout = Duration::from_secs(30);
    assert_eq!(config.fallbacks[0].base_url, "http://10.9.8.14:8000/v1");
    let reply = chat(
        &config,
        "Follow the user's instructions.",
        "Reply exactly OK.",
        16,
    )?;
    anyhow::ensure!(reply.trim() == "OK", "unexpected completion: {reply:?}");
    assert_eq!(requests.lock().unwrap().len(), 1);
    println!("PASS forced primary failure -> http://10.9.8.14:8000/v1: reply={reply:?}");
    Ok(())
}

#[test]
#[ignore = "sends a billed completion to OpenRouter using local credentials"]
fn live_openrouter() -> anyhow::Result<()> {
    live_endpoint("openrouter")
}
