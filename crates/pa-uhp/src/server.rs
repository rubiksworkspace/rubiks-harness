//! The HTTP surface: version negotiation, auth, routing and SSE.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures::stream;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::time::timeout;

use crate::catalog::ModelCatalog;
use crate::protocol::{self, new_id, ApiError, HARNESS_ID, VERSION, VERSION_HEADER};
use crate::request::{self, TaskRequest};
use crate::runner::{self, RunSpec};
use crate::store::{NewTask, SessionRecord, Store, Task};
use crate::ServerConfig;

type Body = BoxBody<Bytes, Infallible>;

const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// A comment on an otherwise silent stream; the spec's ceiling is 30 seconds.
const KEEP_ALIVE: Duration = Duration::from_secs(15);
/// Cancel answers within the spec's one second even if the child takes longer.
const CANCEL_WAIT: Duration = Duration::from_millis(800);
/// How long a stopping server lets running tasks settle.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

struct State {
    config: ServerConfig,
    store: Arc<Store>,
    harness_created_at_ms: u64,
}

fn json_response(status: StatusCode, body: &Value) -> Response<Body> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_string())).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

#[allow(clippy::unnecessary_wraps)] // Reads as the tail of a route that can fail.
fn ok(body: &Value) -> Result<Response<Body>, ApiError> {
    Ok(json_response(StatusCode::OK, body))
}

fn error_response(error: &ApiError) -> Response<Body> {
    let mut response = json_response(error.status, &error.envelope());
    if error.status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            WWW_AUTHENTICATE,
            hyper::header::HeaderValue::from_static("Bearer"),
        );
    }
    response
}

fn authorize(state: &State, request: &Request<Incoming>) -> Result<(), ApiError> {
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .ok_or_else(ApiError::missing_credential)?;
    let token = header
        .to_str()
        .ok()
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(ApiError::invalid_credential)?;
    if bool::from(token.as_bytes().ct_eq(state.config.token.as_bytes())) {
        Ok(())
    } else {
        Err(ApiError::invalid_credential())
    }
}

/// A fresh catalog, off the async threads: reading it touches files.
async fn load_catalog(state: &State) -> Result<ModelCatalog, ApiError> {
    let source = Arc::clone(&state.config.catalog);
    tokio::task::spawn_blocking(move || source())
        .await
        .map_err(|_| ApiError::internal())
}

async fn harness_json(state: &State) -> Result<Value, ApiError> {
    let catalog = load_catalog(state).await?;
    Ok(protocol::harness(
        state.harness_created_at_ms,
        catalog.default_selector().as_deref(),
    ))
}

fn require_harness(id: &str) -> Result<(), ApiError> {
    if id == HARNESS_ID {
        Ok(())
    } else {
        Err(ApiError::harness_not_found())
    }
}

/// One batch of events as SSE frames; events go out as soon as they exist.
fn sse_frames(events: &[Value]) -> Bytes {
    let mut frames = String::new();
    for event in events {
        frames.push_str("data: ");
        frames.push_str(&event.to_string());
        frames.push_str("\n\n");
    }
    Bytes::from(frames)
}

struct Follower {
    task: Arc<Task>,
    progress: watch::Receiver<u64>,
    next_event: usize,
    ended: bool,
}

/// The next chunk of a task's stream. Dropping the stream, which is what a
/// closed connection does, only ends this follower: the runner is independent.
async fn next_chunk(
    mut follower: Follower,
) -> Option<(Result<Frame<Bytes>, Infallible>, Follower)> {
    if follower.ended {
        return None;
    }
    loop {
        // Subscribed before this read, so an event published right after it
        // still wakes the wait below.
        let (events, terminal) = follower.task.events_from(follower.next_event);
        if !events.is_empty() {
            follower.next_event += events.len();
            follower.ended = terminal;
            return Some((Ok(Frame::data(sse_frames(&events))), follower));
        }
        if terminal {
            return None;
        }
        match timeout(KEEP_ALIVE, follower.progress.changed()).await {
            Err(_) => {
                let comment = Bytes::from_static(b": keep-alive\n\n");
                return Some((Ok(Frame::data(comment)), follower));
            }
            Ok(Err(_)) => return None,
            Ok(Ok(())) => {}
        }
    }
}

fn sse_response(task: Arc<Task>) -> Response<Body> {
    let follower = Follower {
        progress: task.follow(),
        task,
        next_event: 0,
        ended: false,
    };
    let body = StreamBody::new(stream::unfold(follower, next_chunk));
    let mut response = Response::new(BodyExt::boxed(body));
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-cache"),
    );
    response
}

/// The session a task runs in: the one it continues, or a new one.
fn resolve_session(state: &State, task: &TaskRequest) -> Result<SessionRecord, ApiError> {
    if let Some(previous) = &task.previous_response_id {
        let response = state
            .store
            .read_response(previous)
            .ok_or_else(ApiError::response_not_found)?;
        let session_id = response["metadata"]["session_id"]
            .as_str()
            .ok_or_else(ApiError::session_not_found)?;
        return state
            .store
            .load_session(session_id)
            .ok_or_else(ApiError::session_not_found);
    }
    let id = new_id("hsess");
    let workspace = state
        .config
        .workspace
        .clone()
        .unwrap_or_else(|| state.store.session_workspace(&id));
    Ok(SessionRecord {
        id,
        prime_session_id: None,
        workspace,
    })
}

async fn create_response(state: &Arc<State>, body: &[u8]) -> Result<Response<Body>, ApiError> {
    let request = request::parse(body)?;
    if let Some(harness_id) = &request.harness_id {
        require_harness(harness_id)?;
    }
    let session = resolve_session(state, &request)?;
    let catalog = load_catalog(state).await?;
    let entry = match &request.model {
        Some(model) => catalog.resolve(model).ok_or_else(|| {
            ApiError::model_unavailable(format!("Model {model:?} cannot run here."))
        })?,
        None => catalog.default_entry().ok_or_else(|| {
            ApiError::model_unavailable("No model is available. Configure a provider first.")
        })?,
    };

    let ephemeral = !request.store && request.previous_response_id.is_none();
    let mut metadata = request.metadata;
    metadata.insert("harness_id".to_string(), json!(HARNESS_ID));
    if !request.ignored_fields.is_empty() {
        metadata.insert("ignored_fields".to_string(), json!(request.ignored_fields));
    }
    let task = state.store.begin(NewTask {
        session_id: session.id.clone(),
        previous_response_id: request.previous_response_id.clone(),
        model: entry.id.clone(),
        store: request.store,
        metadata,
        input_items: request.input_items,
    })?;

    let prepared = std::fs::create_dir_all(&session.workspace)
        .context("workspace")
        .and_then(|()| {
            if ephemeral {
                Ok(())
            } else {
                state.store.save_session(&session)
            }
        });
    if let Err(error) = prepared {
        eprintln!("uhp: could not prepare {}: {error:#}", session.id);
    }
    tokio::spawn(runner::run(RunSpec {
        task: Arc::clone(&task),
        store: Arc::clone(&state.store),
        executable: state.config.executable.clone(),
        token_env: state.config.token_env.clone(),
        session,
        prompt: request.prompt,
        instructions: request.instructions,
        provider: entry.provider.clone(),
        model: entry.id.clone(),
        ephemeral,
        max_step: request.max_step,
        timeout: request.timeout_seconds.map(Duration::from_secs),
    }));

    if request.stream {
        return Ok(sse_response(task));
    }
    if !request.background {
        task.wait_terminal().await;
    }
    ok(&task.response())
}

async fn cancel_task(task: &Task) {
    task.cancel.cancel();
    let _ = timeout(CANCEL_WAIT, task.wait_terminal()).await;
}

async fn read_body(request: Request<Incoming>) -> Result<Bytes, ApiError> {
    Limited::new(request.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(|_| ApiError::body_too_large(MAX_BODY_BYTES))
}

async fn route(state: &Arc<State>, request: Request<Incoming>) -> Result<Response<Body>, ApiError> {
    let method = request.method().clone();
    let path = request.uri().path().trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    if method == Method::GET && segments == ["v1", "uhp"] {
        return ok(&protocol::discovery(&state.config.version));
    }
    authorize(state, &request)?;
    match (&method, segments.as_slice()) {
        (&Method::GET, ["v1", "harnesses"]) => {
            ok(&json!({ "harnesses": [harness_json(state).await?] }))
        }
        (&Method::GET, ["v1", "harnesses", id]) => {
            require_harness(id)?;
            ok(&harness_json(state).await?)
        }
        (&Method::GET, ["v1", "harnesses", id, "models"]) => {
            require_harness(id)?;
            let catalog = load_catalog(state).await?;
            ok(&protocol::harness_models(
                catalog.default_selector().as_deref(),
                &catalog.harness_models_json(),
            ))
        }
        (&Method::GET, ["v1", "models"]) => ok(&load_catalog(state).await?.backends_json()),
        (&Method::POST, ["v1", "responses"]) => {
            let body = read_body(request).await?;
            create_response(state, &body).await
        }
        (&Method::GET, ["v1", "responses", id]) => ok(&state
            .store
            .read_response(id)
            .ok_or_else(ApiError::response_not_found)?),
        (&Method::DELETE, ["v1", "responses", id]) => {
            if state.store.delete_response(id) {
                ok(&json!({ "id": id, "deleted": true }))
            } else {
                Err(ApiError::response_not_found())
            }
        }
        (&Method::GET, ["v1", "responses", id, "input_items"]) => {
            let items = state
                .store
                .read_input_items(id)
                .ok_or_else(ApiError::response_not_found)?;
            ok(&json!({ "object": "list", "data": items }))
        }
        (&Method::POST, ["v1", "responses", id, "cancel"]) => {
            if let Some(task) = state.store.live_task(id) {
                cancel_task(&task).await;
                return ok(&task.response());
            }
            ok(&state
                .store
                .read_response(id)
                .ok_or_else(ApiError::response_not_found)?)
        }
        (&Method::POST, ["v1", "sessions", id, "cancel"]) => {
            let running = state.store.running_in_session(id);
            if running.is_none() && state.store.load_session(id).is_none() {
                return Err(ApiError::session_not_found());
            }
            if let Some(task) = &running {
                cancel_task(task).await;
            }
            ok(&json!({
                "id": id,
                "object": "session",
                "cancelled": running.is_some(),
            }))
        }
        _ => Err(ApiError::route_not_found()),
    }
}

async fn handle(
    state: Arc<State>,
    request: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let requested = request
        .headers()
        .get(VERSION_HEADER)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
    let result = match requested.as_deref() {
        Some(version) if version != VERSION => Err(ApiError::unsupported_version(version)),
        _ => route(&state, request).await,
    };
    let mut response = result.unwrap_or_else(|error| error_response(&error));
    response.headers_mut().insert(
        VERSION_HEADER,
        hyper::header::HeaderValue::from_static(VERSION),
    );
    Ok(response)
}

/// When the harness was first served, stable across restarts.
fn harness_created_at_ms(config: &ServerConfig) -> Result<u64> {
    let path = config.state_dir.join("harness_created_at");
    if let Some(stored) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
    {
        return Ok(stored);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(0));
    std::fs::write(&path, now.to_string())
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(now)
}

async fn bind(config: &ServerConfig) -> Result<(TcpListener, SocketAddr)> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((config.host.as_str(), config.port))
        .await
        .with_context(|| format!("could not resolve {}", config.host))?
        .collect();
    if addresses.is_empty() {
        bail!("{} resolves to no address", config.host);
    }
    if !config.allow_remote && addresses.iter().any(|address| !address.ip().is_loopback()) {
        bail!(
            "{} is not a loopback address. UHP requires TLS off loopback, which this server \
             leaves to a reverse proxy; put one in front and pass --allow-remote.",
            config.host
        );
    }
    let listener = TcpListener::bind(addresses.as_slice())
        .await
        .with_context(|| format!("could not bind {}:{}", config.host, config.port))?;
    let local = listener.local_addr()?;
    Ok((listener, local))
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Serves until SIGINT or SIGTERM, then settles running tasks.
///
/// # Errors
/// The state directory cannot be opened or the address cannot be bound.
pub async fn serve(config: ServerConfig) -> Result<()> {
    let store = Arc::new(Store::open(&config.state_dir)?);
    let (listener, local) = bind(&config).await?;
    let state = Arc::new(State {
        harness_created_at_ms: harness_created_at_ms(&config)?,
        store: Arc::clone(&store),
        config,
    });
    eprintln!("UHP server listening on http://{local}");

    let stopping = shutdown_signal();
    tokio::pin!(stopping);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let state = Arc::clone(&state);
                tokio::spawn(async move {
                    let service = service_fn(move |request| handle(Arc::clone(&state), request));
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
            () = &mut stopping => break,
        }
    }

    store.interrupt_all();
    let settled = timeout(SHUTDOWN_WAIT, async {
        while store.live_count() > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let _ = settled.await;
    Ok(())
}
