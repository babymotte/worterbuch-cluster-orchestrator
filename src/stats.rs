use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
#[cfg(feature = "jemalloc")]
use axum::{
    body::Body,
    http::{HeaderValue, Response, StatusCode},
};
use miette::{Context, IntoDiagnostic, Result};
use serde_json::json;
use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
};
use tokio::{
    net::TcpListener,
    select,
    sync::{mpsc, oneshot},
};
use tokio_graceful_shutdown::{SubsystemBuilder, SubsystemHandle};
use tracing::{info, instrument};

pub struct StatsSender(mpsc::Sender<StatsEvent>);

impl StatsSender {
    pub async fn candidate(&self) {
        self.0
            .send(StatsEvent::Election(ElectionState::Candidate))
            .await
            .ok();
    }

    pub async fn follower(&self) {
        self.0
            .send(StatsEvent::Election(ElectionState::Follower))
            .await
            .ok();
    }

    pub async fn leader(&self) {
        self.0
            .send(StatsEvent::Election(ElectionState::Leader))
            .await
            .ok();
    }
}

#[derive(Debug, Clone)]
enum ElectionState {
    Candidate,
    Leader,
    Follower,
}

#[derive(Debug)]
enum StatsEvent {
    Election(ElectionState),
}

#[derive(Debug)]
enum StatsApiMessage {
    ElectionState(oneshot::Sender<ElectionState>),
}

#[derive(Clone)]
struct Server {
    api_tx: mpsc::Sender<StatsApiMessage>,
}

impl Server {
    #[instrument(skip(server), ret)]
    async fn ready(State(server): State<Server>) -> impl IntoResponse {
        let (tx, rx) = oneshot::channel();
        server
            .api_tx
            .send(StatsApiMessage::ElectionState(tx))
            .await
            .ok();
        let state = if let Ok(it) = rx.await {
            it
        } else {
            ElectionState::Candidate
        };

        match state {
            ElectionState::Candidate => (StatusCode::OK, Json(json!("candidate"))),
            ElectionState::Leader => (StatusCode::OK, Json(json!("leader"))),
            ElectionState::Follower => (StatusCode::SERVICE_UNAVAILABLE, Json(json!("follower"))),
        }
    }

    #[cfg(feature = "jemalloc")]
    async fn get_heap() -> Result<Response<Body>, (StatusCode, String)> {
        let prof_ctl = if let Some(it) = jemalloc_pprof::PROF_CTL.as_ref() {
            it
        } else {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "jemalloc profiling is not enabled".to_owned(),
            ));
        };
        let mut prof_ctl = prof_ctl.lock().await;
        Self::require_profiling_activated(&prof_ctl)?;
        let pprof = match prof_ctl.dump_pprof() {
            Ok(it) => it,
            Err(e) => {
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("error generating heap dump: {e}"),
                ));
            }
        };

        let mut response = pprof.into_response();

        response.headers_mut().insert(
            "Content-Disposition",
            HeaderValue::from_static("attachment; filename=heap.pb.gz"),
        );

        Ok(response)
    }

    #[cfg(feature = "jemalloc")]
    fn require_profiling_activated(
        prof_ctl: &jemalloc_pprof::JemallocProfCtl,
    ) -> Result<(), (StatusCode, String)> {
        if prof_ctl.activated() {
            Ok(())
        } else {
            Err((StatusCode::FORBIDDEN, "heap profiling not activated".into()))
        }
    }
}

pub async fn start_stats_endpoint(subsys: &SubsystemHandle, port: u16) -> Result<StatsSender> {
    let (evt_tx, evt_rx) = mpsc::channel(1);
    let (api_tx, api_rx) = mpsc::channel(1);

    StatsActor::start_stats_actor(subsys, evt_rx, api_rx);

    subsys.start(SubsystemBuilder::new("stats-endpoint", move |s| {
        run_server(s, api_tx, port)
    }));

    Ok(StatsSender(evt_tx))
}

async fn run_server(
    subsys: SubsystemHandle,
    api_tx: mpsc::Sender<StatsApiMessage>,
    port: u16,
) -> Result<()> {
    let server = Server { api_tx };

    let mut app = Router::new().route("/ready", get(Server::ready));

    #[cfg(feature = "jemalloc")]
    if env::var("MALLOC_CONF")
        .unwrap_or("".into())
        .contains("prof_active:true")
    {
        app = app.route("/debug/heap", get(Server::get_heap))
    }

    let ip = IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0));
    let addr = SocketAddr::new(ip, port);

    info!("Starting stats endpoint at {addr} …");

    let listener = TcpListener::bind(addr)
        .await
        .into_diagnostic()
        .wrap_err_with(|| format!("stats endpoint could not bind to socket address {addr}"))?;

    axum::serve(listener, app.with_state(server))
        .with_graceful_shutdown(async move {
            subsys.on_shutdown_requested().await;
        })
        .await
        .into_diagnostic()
        .wrap_err("error starting stats endpoint web server")?;

    Ok(())
}

struct StatsActor {
    subsys: SubsystemHandle,
    stats_rx: mpsc::Receiver<StatsEvent>,
    api_rx: mpsc::Receiver<StatsApiMessage>,
    election_state: ElectionState,
}

impl StatsActor {
    fn start_stats_actor(
        subsys: &SubsystemHandle,
        stats_rx: mpsc::Receiver<StatsEvent>,
        api_rx: mpsc::Receiver<StatsApiMessage>,
    ) {
        subsys.start(SubsystemBuilder::new("stats-actor", |s| async {
            let actor = StatsActor {
                subsys: s,
                stats_rx,
                api_rx,
                election_state: ElectionState::Candidate,
            };
            actor.run_stats_actor().await
        }));
    }

    async fn run_stats_actor(mut self) -> Result<()> {
        loop {
            select! {
                recv = self.stats_rx.recv() => match recv {
                    Some(it) => self.stats_event(it),
                    None => break,
                },
                recv = self.api_rx.recv() => match recv {
                    Some(it) => self.api_request(it),
                    None => break,
                },
                _ = self.subsys.on_shutdown_requested() => break,
            }
        }

        Ok(())
    }

    fn stats_event(&mut self, e: StatsEvent) {
        match e {
            StatsEvent::Election(state) => self.election_state = state,
        }
    }

    fn api_request(&mut self, e: StatsApiMessage) {
        match e {
            StatsApiMessage::ElectionState(sender) => {
                sender.send(self.election_state.clone()).ok();
            }
        }
    }
}
