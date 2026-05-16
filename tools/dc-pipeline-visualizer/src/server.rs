use crate::{
    graph::GraphConfig,
    metrics::{Store, now_ts, tail_metrics_file},
    protocol::{MetricSeries, ServerMessage},
};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, State, WebSocketUpgrade, ws::Message},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use futures::{SinkExt, StreamExt};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::{RwLock, broadcast};

#[derive(Clone)]
pub struct AppState {
    pub graph: Arc<GraphConfig>,
    pub store: Arc<RwLock<Store>>,
    pub tx: broadcast::Sender<Arc<ServerEvent>>,
}

#[derive(Debug, Clone)]
pub enum ServerEvent {
    Metrics(MetricSeries),
    Health { status: String, message: String },
    State { state: String, detail: String },
}

pub async fn run(
    bind: SocketAddr,
    graph: GraphConfig,
    store: Store,
    metrics_file: PathBuf,
    ui_dist: PathBuf,
) -> Result<()> {
    let (tx, _rx) = broadcast::channel(256);
    let state = AppState {
        graph: Arc::new(graph),
        store: Arc::new(RwLock::new(store)),
        tx,
    };
    let _ = state.tx.send(Arc::new(ServerEvent::State {
        state: "starting".into(),
        detail: "initializing metrics tail loop".into(),
    }));

    let tail_state = state.clone();
    tokio::spawn(async move {
        let error_state = tail_state.clone();
        let _ = tail_state.tx.send(Arc::new(ServerEvent::State {
            state: "tailing".into(),
            detail: "waiting for metrics rows".into(),
        }));
        let metrics_file_for_error = metrics_file.clone();
        let tail_result = tail_metrics_file(metrics_file, move |metric| {
            let tail_state = tail_state.clone();
            tokio::spawn(async move {
                let mut store = tail_state.store.write().await;
                let update = store.ingest(metric);
                drop(store);
                let _ = tail_state
                    .tx
                    .send(Arc::new(ServerEvent::Metrics(update.clone())));
            });
        })
        .await;

        if let Err(error) = tail_result {
            tracing::error!(?error, "metrics tail loop exited");
            let _ = error_state.tx.send(Arc::new(ServerEvent::Health {
                status: "error".into(),
                message: format!(
                    "failed tailing metrics file {}",
                    metrics_file_for_error.display()
                ),
            }));
        }
    });

    let api_router = Router::new()
        .route("/ws", get(ws_handler))
        .route("/api/bootstrap", get(bootstrap_handler))
        .route("/api/graph", get(graph_handler))
        .route("/api/health", get(health_handler))
        .route("/", get(index_handler))
        .route("/{*path}", get(static_handler))
        .with_state((state, ui_dist));

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(addr = %bind, "pipeline visualizer listening");
    axum::serve(listener, api_router).await?;

    Ok(())
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State((state, _)): State<(AppState, PathBuf)>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_connection(socket, state))
}

async fn ws_connection(socket: axum::extract::ws::WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    let snapshot = {
        let store = state.store.read().await;
        store.snapshot()
    };

    let bootstrap = ServerMessage::Bootstrap {
        graph: (*state.graph).clone(),
        snapshot,
        now_ts: now_ts(),
    };
    if sender
        .send(Message::Text(
            serde_json::to_string(&bootstrap)
                .unwrap_or_else(|_| {
                    "{\"type\":\"health\",\"status\":\"error\",\"message\":\"serialize failed\"}"
                        .into()
                })
                .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            message = rx.recv() => {
                match message {
                    Ok(event) => {
                        let outbound = match event.as_ref() {
                            ServerEvent::Metrics(update) => ServerMessage::Metrics { updates: vec![update.clone()] },
                            ServerEvent::Health { status, message } => ServerMessage::Health { status: status.clone(), message: message.clone() },
                            ServerEvent::State { state, detail } => ServerMessage::State { state: state.clone(), detail: detail.clone() },
                        };
                        let text = match serde_json::to_string(&outbound) {
                            Ok(v) => v,
                            Err(error) => {
                                tracing::warn!(?error, "failed to serialize ws message");
                                continue;
                            }
                        };
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let lag = ServerMessage::Health {
                            status: "warning".into(),
                            message: format!("client lagged and skipped {skipped} updates"),
                        };
                        let text = serde_json::to_string(&lag).unwrap_or_else(|_| "{\"type\":\"health\",\"status\":\"error\",\"message\":\"serialize failed\"}".into());
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn bootstrap_handler(State((state, _)): State<(AppState, PathBuf)>) -> impl IntoResponse {
    let snapshot = state.store.read().await.snapshot();
    Json(ServerMessage::Bootstrap {
        graph: (*state.graph).clone(),
        snapshot,
        now_ts: now_ts(),
    })
}

async fn graph_handler(State((state, _)): State<(AppState, PathBuf)>) -> impl IntoResponse {
    Json((*state.graph).clone())
}

async fn health_handler() -> impl IntoResponse {
    Json(ServerMessage::Health {
        status: "ok".into(),
        message: "tailing".into(),
    })
}

async fn index_handler(State((_, ui_dist)): State<(AppState, PathBuf)>) -> Response {
    let index = ui_dist.join("index.html");
    match tokio::fs::read_to_string(index).await {
        Ok(contents) => Html(contents).into_response(),
        Err(_) => Html("<h1>UI build not found</h1><p>Run: npm run build in tools/dc-pipeline-visualizer/ui</p>".to_string()).into_response(),
    }
}

async fn static_handler(
    State((_, ui_dist)): State<(AppState, PathBuf)>,
    Path(path): Path<String>,
) -> Response {
    let file = ui_dist.join(path);
    match tokio::fs::read(file).await {
        Ok(bytes) => bytes.into_response(),
        Err(_) => Html("not found".to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_health_contract_serializes() {
        let msg = ServerMessage::Health {
            status: "warning".into(),
            message: "client lagged and skipped 10 updates".into(),
        };
        let value = serde_json::to_value(msg).expect("serialize");
        assert_eq!(value["type"], "health");
        assert_eq!(value["status"], "warning");
    }

    #[tokio::test]
    async fn broadcast_channel_surfaces_backpressure_lag() {
        let (tx, mut rx) = broadcast::channel::<Arc<ServerEvent>>(1);
        let _ = tx.send(Arc::new(ServerEvent::State {
            state: "tailing".into(),
            detail: "first".into(),
        }));
        let _ = tx.send(Arc::new(ServerEvent::State {
            state: "tailing".into(),
            detail: "second".into(),
        }));

        let result = rx.recv().await;
        assert!(matches!(
            result,
            Err(broadcast::error::RecvError::Lagged(skipped)) if skipped >= 1
        ));
    }
}
