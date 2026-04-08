use crate::swarm::PeerRegistry;
use axum::extract::State;
use axum::http::header;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<PeerRegistry>,
    pub sse_tx: broadcast::Sender<String>,
}

/// Start the web server and return the broadcast sender for SSE pushes.
pub fn start_web_server(
    addr: SocketAddr,
    registry: Arc<PeerRegistry>,
) -> broadcast::Sender<String> {
    let (sse_tx, _) = broadcast::channel::<String>(256);

    let state = AppState {
        registry,
        sse_tx: sse_tx.clone(),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/js/monitor.js", get(js_handler))
        .route("/css/style.css", get(css_handler))
        .route("/api/swarm", get(swarm_handler))
        .route("/api/events", get(events_handler))
        .with_state(state);

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    sse_tx
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
}

async fn js_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        include_str!("static/monitor.js"),
    )
}

async fn css_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css")],
        include_str!("static/style.css"),
    )
}

async fn swarm_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.registry.snapshot())
}

async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.sse_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(data) => Some(Ok(Event::default().event("swarm").data(data))),
        Err(_) => None,
    });
    Sse::new(stream)
}
