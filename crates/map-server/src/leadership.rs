//! Broker-backed leader election for imagery repository management.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{header, Method, Request};
use hyper_util::{
    client::legacy::{connect::HttpConnector, Client},
    rt::TokioExecutor,
};
use map_imagery::{
    ChunkResultEvent, ChunkWorkEvent, ImagerySnapshot, ImageryStore, InboxEvent, InboxEventInput,
    JobCommandEvent, JobStatus, OutboxEvent, TileStoredEvent,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const DEFAULT_ELECTION: &str = "cangling-map/imagery-repository-master";
const DEFAULT_TTL_SECONDS: u64 = 15;
const DEFAULT_PEER_URL_TEMPLATE: &str = "http://{leader}.cangling-map-headless:3000";
const DEFAULT_CHUNK_TILES: u32 = 512;
const DEFAULT_HYBRID_TAKEOVER_SECONDS: u64 = 120;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LeadershipSnapshot {
    pub mode: &'static str,
    pub election: String,
    pub candidate_id: String,
    pub is_leader: bool,
    pub leader_id: Option<String>,
    pub term: u64,
    pub expires_in_ms: u64,
    pub worker: WorkerSnapshot,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkerSnapshot {
    pub enabled: bool,
    /// Configured upper bound retained for API compatibility.
    pub concurrency: u64,
    pub min_concurrency: u64,
    pub effective_concurrency: u64,
    pub average_tile_ms: u64,
    pub active_chunks: u64,
    pub completed_chunks: u64,
    pub failed_attempts: u64,
    pub processed_tiles: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkerMetricSample {
    pub recorded_at_unix_ms: u64,
    pub effective_concurrency: u64,
    pub average_tile_ms: u64,
    pub active_chunks: u64,
    pub completed_chunks: u64,
    pub failed_attempts: u64,
    pub processed_tiles: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WorkerMetricsResponse {
    pub current: WorkerSnapshot,
    pub samples: Vec<WorkerMetricSample>,
}

#[derive(Default)]
struct WorkerHistory {
    last_recorded: Option<Instant>,
    samples: VecDeque<WorkerMetricSample>,
}

#[derive(Default)]
struct WorkerMetrics {
    enabled: AtomicBool,
    concurrency: AtomicU64,
    min_concurrency: AtomicU64,
    effective_concurrency: AtomicU64,
    average_tile_micros: AtomicU64,
    active_chunks: AtomicU64,
    completed_chunks: AtomicU64,
    failed_attempts: AtomicU64,
    processed_tiles: AtomicU64,
    history: Mutex<WorkerHistory>,
}

impl WorkerMetrics {
    fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            enabled: self.enabled.load(Ordering::Relaxed),
            concurrency: self.concurrency.load(Ordering::Relaxed),
            min_concurrency: self.min_concurrency.load(Ordering::Relaxed),
            effective_concurrency: self.effective_concurrency.load(Ordering::Relaxed),
            average_tile_ms: self.average_tile_micros.load(Ordering::Relaxed) / 1_000,
            active_chunks: self.active_chunks.load(Ordering::Relaxed),
            completed_chunks: self.completed_chunks.load(Ordering::Relaxed),
            failed_attempts: self.failed_attempts.load(Ordering::Relaxed),
            processed_tiles: self.processed_tiles.load(Ordering::Relaxed),
        }
    }

    fn update_adaptive(&self, adaptive: &AdaptiveTileConcurrency) {
        self.effective_concurrency
            .store(adaptive.current() as u64, Ordering::Relaxed);
        self.average_tile_micros
            .store(adaptive.average_micros(), Ordering::Relaxed);
        self.record_history(false);
    }

    fn record_history(&self, force: bool) {
        let now = Instant::now();
        let mut history = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !force
            && history
                .last_recorded
                .is_some_and(|last| now.duration_since(last) < Duration::from_secs(5))
        {
            return;
        }
        history.last_recorded = Some(now);
        history.samples.push_back(WorkerMetricSample {
            recorded_at_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            effective_concurrency: self.effective_concurrency.load(Ordering::Relaxed),
            average_tile_ms: self.average_tile_micros.load(Ordering::Relaxed) / 1_000,
            active_chunks: self.active_chunks.load(Ordering::Relaxed),
            completed_chunks: self.completed_chunks.load(Ordering::Relaxed),
            failed_attempts: self.failed_attempts.load(Ordering::Relaxed),
            processed_tiles: self.processed_tiles.load(Ordering::Relaxed),
        });
        while history.samples.len() > 120 {
            history.samples.pop_front();
        }
    }

    fn history(&self) -> WorkerMetricsResponse {
        let samples = self
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .samples
            .iter()
            .cloned()
            .collect();
        WorkerMetricsResponse {
            current: self.snapshot(),
            samples,
        }
    }
}

struct AdaptiveTileConcurrency {
    min: u64,
    max: u64,
    target_micros: u64,
    current: AtomicU64,
    average_micros: AtomicU64,
    samples: AtomicU64,
}

impl AdaptiveTileConcurrency {
    fn new(min: usize, max: usize, target: Duration) -> Self {
        let max = max.max(1) as u64;
        let min = (min.max(1) as u64).min(max);
        Self {
            min,
            max,
            target_micros: target.as_micros().max(1) as u64,
            current: AtomicU64::new(min),
            average_micros: AtomicU64::new(0),
            samples: AtomicU64::new(0),
        }
    }

    fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed) as usize
    }

    fn average_micros(&self) -> u64 {
        self.average_micros.load(Ordering::Relaxed)
    }

    fn observe_success(&self, elapsed: Duration) {
        let sample = elapsed.as_micros().max(1) as u64;
        let previous = self.average_micros.load(Ordering::Relaxed);
        let average = if previous == 0 {
            sample
        } else {
            previous.saturating_mul(7).saturating_add(sample) / 8
        };
        self.average_micros.store(average, Ordering::Relaxed);
        let samples = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        if samples % 16 != 0 {
            return;
        }
        let current = self.current.load(Ordering::Relaxed);
        let next = if average <= self.target_micros && current < self.max {
            current + 1
        } else if average > self.target_micros.saturating_mul(2) && current > self.min {
            current - 1
        } else {
            current
        };
        self.current.store(next, Ordering::Relaxed);
    }

    fn observe_failure(&self) {
        let current = self.current.load(Ordering::Relaxed);
        self.current
            .store((current / 2).max(self.min), Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
struct LeadershipState {
    leader_id: Option<String>,
    term: u64,
    lease_id: Option<String>,
    valid_until: Option<Instant>,
}

impl LeadershipState {
    fn follower() -> Self {
        Self {
            leader_id: None,
            term: 0,
            lease_id: None,
            valid_until: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct MapLeadership {
    election: String,
    candidate_id: String,
    ttl: Duration,
    broker: Option<BrokerElectionClient>,
    state: Arc<RwLock<LeadershipState>>,
    worker_metrics: Arc<WorkerMetrics>,
}

impl MapLeadership {
    pub(crate) fn from_env() -> Result<Self, String> {
        let election =
            std::env::var("CANGLING_MAP_ELECTION").unwrap_or_else(|_| DEFAULT_ELECTION.into());
        let candidate_id = std::env::var("CANGLING_MAP_INSTANCE_ID")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| format!("cangling-map-{}", std::process::id()));
        let ttl_seconds = std::env::var("CANGLING_MAP_ELECTION_TTL_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TTL_SECONDS)
            .clamp(3, 3600);
        let broker = BrokerElectionClient::from_env()?;
        let state = if broker.is_some() {
            LeadershipState::follower()
        } else {
            tracing::warn!(
                "CANGLING_BROKER_HTTP_URL is not configured; imagery management runs in standalone leader mode"
            );
            LeadershipState {
                leader_id: Some(candidate_id.clone()),
                term: 1,
                lease_id: Some("standalone".into()),
                valid_until: None,
            }
        };
        Ok(Self {
            election,
            candidate_id,
            ttl: Duration::from_secs(ttl_seconds),
            broker,
            state: Arc::new(RwLock::new(state)),
            worker_metrics: Arc::new(WorkerMetrics::default()),
        })
    }

    pub(crate) fn start(&self) {
        if self.broker.is_some() {
            let leadership = self.clone();
            tokio::spawn(async move { leadership.run().await });
        }
    }

    pub(crate) fn candidate_id(&self) -> &str {
        &self.candidate_id
    }

    pub(crate) fn start_outbox_publisher(&self, imagery: ImageryStore) {
        let Some(broker) = self.broker.clone() else {
            return;
        };
        let source = self.candidate_id.clone();
        tokio::spawn(async move {
            let mut configured_once = false;
            loop {
                if !configured_once {
                    match broker.configure_imagery_topics().await {
                        Ok(()) => configured_once = true,
                        Err(error) => {
                            tracing::warn!(%error, "could not configure imagery broker topics");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                    }
                }
                let events = match imagery.pending_outbox(100).await {
                    Ok(events) => events,
                    Err(error) => {
                        tracing::error!(%error, "could not read imagery outbox");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                if events.is_empty() {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                // Reassert topic durability before every non-empty batch. This
                // also covers a broker restart with a newly initialized DB;
                // publishing first would otherwise create an implicit
                // ephemeral topic and lose a control-plane event.
                if let Err(error) = broker.configure_imagery_topics().await {
                    tracing::warn!(%error, "could not refresh imagery broker topics");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
                for event in events {
                    let id = event.id.clone();
                    match broker.publish_outbox(&source, event).await {
                        Ok(()) => {
                            if let Err(error) = imagery.mark_outbox_published(&id).await {
                                tracing::error!(%error, event_id = %id, "could not complete imagery outbox event");
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, event_id = %id, "could not publish imagery outbox event");
                            let _ = imagery.mark_outbox_failed(&id).await;
                        }
                    }
                }
            }
        });
    }

    pub(crate) fn start_inbox_consumer(&self, imagery: ImageryStore) {
        let Some(broker) = self.broker.clone() else {
            return;
        };
        spawn_inbox_topic_consumer(
            self.clone(),
            broker.clone(),
            imagery.clone(),
            "imagery.tile.stored",
            "imagery.tile.stored",
            "imagery-tile-results",
        );
        spawn_inbox_topic_consumer(
            self.clone(),
            broker.clone(),
            imagery.clone(),
            "imagery.job.command",
            "imagery.job.created",
            "imagery-job-commands",
        );
        spawn_inbox_topic_consumer(
            self.clone(),
            broker,
            imagery,
            "imagery.chunk.result",
            "imagery.chunk.result",
            "imagery-chunk-results",
        );
    }

    pub(crate) fn start_chunk_worker(
        &self,
        imagery: ImageryStore,
        tile_client: super::imagery::ImageryTileClient,
    ) {
        let Some(broker) = self.broker.clone() else {
            return;
        };
        let allowed_hosts = std::env::var("CANGLING_MAP_IMAGERY_SOURCE_HOSTS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<std::collections::HashSet<_>>();
        if allowed_hosts.is_empty() {
            tracing::warn!(
                "server imagery worker is disabled; configure CANGLING_MAP_IMAGERY_SOURCE_HOSTS"
            );
            return;
        }
        let http = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
        {
            Ok(http) => http,
            Err(error) => {
                tracing::error!(%error, "could not initialize imagery source HTTP client");
                return;
            }
        };
        let instance_id = self.candidate_id.clone();
        let max_concurrency = configured_worker_concurrency();
        let min_concurrency = configured_worker_min_concurrency(max_concurrency);
        let adaptive = Arc::new(AdaptiveTileConcurrency::new(
            min_concurrency,
            max_concurrency,
            configured_worker_target_latency(),
        ));
        let metrics = self.worker_metrics.clone();
        metrics.enabled.store(true, Ordering::Relaxed);
        metrics
            .concurrency
            .store(max_concurrency as u64, Ordering::Relaxed);
        metrics
            .min_concurrency
            .store(min_concurrency as u64, Ordering::Relaxed);
        metrics.update_adaptive(&adaptive);
        tokio::spawn(async move {
            let consumer_id = format!("{instance_id}:imagery-chunk-worker");
            loop {
                let message = match broker
                    .poll("imagery.chunk.work", &consumer_id, &instance_id)
                    .await
                {
                    Ok(Some(message)) => message,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(%error, "could not poll imagery chunk work");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                metrics.active_chunks.fetch_add(1, Ordering::Relaxed);
                let result = process_chunk_work(
                    &broker,
                    &imagery,
                    &tile_client,
                    &http,
                    &allowed_hosts,
                    &adaptive,
                    &metrics,
                    &message,
                )
                .await;
                metrics.active_chunks.fetch_sub(1, Ordering::Relaxed);
                let (success, error) = match result {
                    Ok(processed_tiles) => {
                        metrics.completed_chunks.fetch_add(1, Ordering::Relaxed);
                        metrics
                            .processed_tiles
                            .fetch_add(processed_tiles, Ordering::Relaxed);
                        (true, String::new())
                    }
                    Err(error) => {
                        adaptive.observe_failure();
                        metrics.update_adaptive(&adaptive);
                        metrics.failed_attempts.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%error, message_id = %message.id, "imagery chunk failed");
                        if message.delivery_attempt >= message.max_delivery_attempts.max(1) {
                            if let Err(report_error) =
                                report_terminal_chunk_result(&broker, &imagery, &message, &error)
                                    .await
                            {
                                tracing::warn!(%report_error, message_id = %message.id, "could not publish terminal imagery chunk result");
                                // Leave the lease unacknowledged. Expiry will
                                // redeliver the idempotent chunk so its final
                                // state is not silently lost.
                                continue;
                            }
                        }
                        (false, error)
                    }
                };
                metrics.record_history(true);
                if let Err(ack_error) = broker
                    .ack(&message.id, &message.lease, success, &error)
                    .await
                {
                    tracing::warn!(%ack_error, message_id = %message.id, "could not ACK imagery chunk");
                }
            }
        });
    }

    pub(crate) fn start_snapshot_sync(&self, imagery: ImageryStore) {
        let Some(broker) = self.broker.clone() else {
            return;
        };
        let template = std::env::var("CANGLING_MAP_PEER_URL_TEMPLATE")
            .unwrap_or_else(|_| DEFAULT_PEER_URL_TEMPLATE.into());
        if !template.starts_with("http://") || !template.contains("{leader}") {
            tracing::error!(
                template,
                "CANGLING_MAP_PEER_URL_TEMPLATE must be an http:// URL containing {{leader}}"
            );
            return;
        }
        let interval = std::env::var("CANGLING_MAP_SNAPSHOT_INTERVAL_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(10)
            .clamp(2, 3600);
        let leadership = self.clone();
        tokio::spawn(async move {
            loop {
                let observed = leadership.snapshot().await;
                let Some(leader_id) = observed.leader_id.clone() else {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                };
                if observed.is_leader || leader_id == leadership.candidate_id {
                    tokio::time::sleep(Duration::from_secs(interval)).await;
                    continue;
                }
                let base_url = template.replace("{leader}", &leader_id);
                match broker.fetch_snapshot(&base_url).await {
                    Ok(snapshot)
                        if snapshot.leader_id.as_deref() == Some(leader_id.as_str())
                            && snapshot.leader_term == observed.term =>
                    {
                        // Hold the read guard while importing so an election
                        // update cannot promote this instance midway through a
                        // follower-only snapshot transaction.
                        let state = leadership.state.read().await;
                        let still_follower = !leadership.is_current_leader(&state)
                            && state.leader_id.as_deref() == Some(leader_id.as_str())
                            && state.term == observed.term;
                        if still_follower {
                            match imagery.apply_snapshot(&snapshot).await {
                                Ok(()) => tracing::debug!(
                                    leader = %leader_id,
                                    term = observed.term,
                                    repositories = snapshot.repositories.len(),
                                    jobs = snapshot.jobs.len(),
                                    "imagery follower snapshot synchronized"
                                ),
                                Err(error) => {
                                    tracing::warn!(%error, leader = %leader_id, "could not apply imagery snapshot")
                                }
                            }
                        }
                    }
                    Ok(snapshot) => tracing::warn!(
                        expected_leader = %leader_id,
                        expected_term = observed.term,
                        snapshot_leader = ?snapshot.leader_id,
                        snapshot_term = snapshot.leader_term,
                        "discarded stale imagery snapshot"
                    ),
                    Err(error) => {
                        tracing::warn!(%error, leader = %leader_id, "could not fetch imagery leader snapshot")
                    }
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        });
    }

    pub(crate) fn start_hybrid_takeover(&self, imagery: ImageryStore) {
        if self.broker.is_none() {
            return;
        }
        let timeout_seconds = std::env::var("CANGLING_MAP_HYBRID_TAKEOVER_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_HYBRID_TAKEOVER_SECONDS);
        if timeout_seconds == 0 {
            tracing::info!("automatic hybrid imagery takeover is disabled");
            return;
        }
        let timeout_seconds = timeout_seconds.clamp(15, 86_400);
        let interval = Duration::from_secs((timeout_seconds / 3).clamp(2, 30));
        let leadership = self.clone();
        tokio::spawn(async move {
            loop {
                if leadership.leader_term().await.is_some() {
                    match imagery.stale_hybrid_job_ids(timeout_seconds, 100).await {
                        Ok(job_ids) => {
                            for job_id in job_ids {
                                let result = async {
                                    imagery
                                        .ensure_job_chunks(&job_id, configured_chunk_tiles())
                                        .await?;
                                    imagery.takeover_hybrid_job(&job_id).await
                                }
                                .await;
                                match result {
                                    Ok(queued) if queued > 0 => tracing::info!(
                                        %job_id,
                                        queued_chunks = queued,
                                        idle_seconds = timeout_seconds,
                                        "automatically took over stale hybrid imagery job"
                                    ),
                                    Ok(_) => {}
                                    Err(error) => tracing::warn!(
                                        %error,
                                        %job_id,
                                        "could not take over stale hybrid imagery job"
                                    ),
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "could not scan stale hybrid imagery jobs")
                        }
                    }
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    pub(crate) async fn leader_term(&self) -> Option<u64> {
        let state = self.state.read().await;
        self.is_current_leader(&state).then_some(state.term)
    }

    pub(crate) fn worker_metrics(&self) -> WorkerMetricsResponse {
        self.worker_metrics.history()
    }

    pub(crate) async fn snapshot(&self) -> LeadershipSnapshot {
        let state = self.state.read().await;
        let standalone = self.broker.is_none();
        let is_leader = self.is_current_leader(&state);
        LeadershipSnapshot {
            mode: if standalone { "standalone" } else { "broker" },
            election: self.election.clone(),
            candidate_id: self.candidate_id.clone(),
            is_leader,
            leader_id: state.leader_id.clone(),
            term: state.term,
            expires_in_ms: state
                .valid_until
                .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
                .unwrap_or_default()
                .as_millis() as u64,
            worker: self.worker_metrics.snapshot(),
        }
    }

    fn is_current_leader(&self, state: &LeadershipState) -> bool {
        let owns_term =
            state.leader_id.as_deref() == Some(self.candidate_id.as_str()) && state.term > 0;
        if self.broker.is_none() {
            return owns_term;
        }
        owns_term
            && state.lease_id.is_some()
            && state
                .valid_until
                .is_some_and(|deadline| deadline > Instant::now())
    }

    async fn run(self) {
        let broker = self.broker.clone().expect("broker checked by start");
        let refresh = (self.ttl / 3).max(Duration::from_secs(1));
        loop {
            let lease = {
                let state = self.state.read().await;
                if self.is_current_leader(&state) {
                    state.lease_id.clone().map(|lease_id| ActiveLease {
                        term: state.term,
                        lease_id,
                    })
                } else {
                    None
                }
            };
            let result = match lease {
                Some(lease) => broker
                    .renew(
                        &self.election,
                        &self.candidate_id,
                        lease.term,
                        &lease.lease_id,
                        self.ttl,
                    )
                    .await
                    .map(ElectionResult::Renew),
                None => broker
                    .campaign(&self.election, &self.candidate_id, self.ttl)
                    .await
                    .map(ElectionResult::Campaign),
            };
            match result {
                Ok(ElectionResult::Campaign(response)) => {
                    self.apply_leader(response.leader, response.won).await;
                }
                Ok(ElectionResult::Renew(response)) => {
                    if let Some(leader) = response.leader {
                        self.apply_leader(leader, response.renewed).await;
                    } else {
                        self.clear_leadership().await;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "cangling-map leader election request failed");
                    // Keep a previously acquired lease only until its local safety deadline.
                    let expired = self
                        .state
                        .read()
                        .await
                        .valid_until
                        .is_some_and(|deadline| deadline <= Instant::now());
                    if expired {
                        self.clear_leadership().await;
                    }
                }
            }
            tokio::time::sleep(refresh).await;
        }
    }

    async fn apply_leader(&self, leader: LeaderWire, acquired: bool) {
        let safety = Duration::from_secs(1).min(self.ttl / 3);
        let valid_for = Duration::from_millis(leader.remaining_ttl_ms).saturating_sub(safety);
        let mut state = self.state.write().await;
        let was_leader = self.is_current_leader(&state);
        state.leader_id = Some(leader.leader_id.clone());
        state.term = leader.term;
        let locally_valid = acquired && !valid_for.is_zero();
        state.lease_id = locally_valid.then_some(leader.lease_id);
        state.valid_until = locally_valid.then_some(Instant::now() + valid_for);
        let is_leader = self.is_current_leader(&state);
        if is_leader && !was_leader {
            tracing::info!(term = state.term, candidate = %self.candidate_id, "cangling-map became imagery master");
        } else if was_leader && !is_leader {
            tracing::warn!(leader = %leader.leader_id, term = leader.term, "cangling-map lost imagery leadership");
        }
    }

    async fn clear_leadership(&self) {
        let mut state = self.state.write().await;
        let was_leader = self.is_current_leader(&state);
        state.lease_id = None;
        state.valid_until = None;
        if was_leader {
            tracing::warn!(candidate = %self.candidate_id, "cangling-map imagery leadership expired");
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveLease {
    term: u64,
    lease_id: String,
}

enum ElectionResult {
    Campaign(CampaignResponse),
    Renew(RenewResponse),
}

#[derive(Clone)]
struct BrokerElectionClient {
    base_url: String,
    token: String,
    client: Client<HttpConnector, Full<Bytes>>,
}

impl BrokerElectionClient {
    fn from_env() -> Result<Option<Self>, String> {
        let Ok(base_url) = std::env::var("CANGLING_BROKER_HTTP_URL") else {
            return Ok(None);
        };
        let base_url = base_url.trim().trim_end_matches('/').to_owned();
        if !base_url.starts_with("http://") {
            return Err("CANGLING_BROKER_HTTP_URL currently requires an http:// URL".into());
        }
        Ok(Some(Self {
            base_url,
            token: std::env::var("CANGLING_BROKER_AUTH_TOKEN")
                .or_else(|_| std::env::var("CL_BROKER_AUTH_TOKEN"))
                .unwrap_or_default(),
            client: Client::builder(TokioExecutor::new()).build_http(),
        }))
    }

    async fn campaign(
        &self,
        election: &str,
        candidate_id: &str,
        ttl: Duration,
    ) -> Result<CampaignResponse, String> {
        self.post(
            "/election/campaign",
            &CampaignRequest {
                election,
                candidate_id,
                ttl_seconds: ttl.as_secs(),
            },
        )
        .await
    }

    async fn renew(
        &self,
        election: &str,
        leader_id: &str,
        term: u64,
        lease_id: &str,
        ttl: Duration,
    ) -> Result<RenewResponse, String> {
        self.post(
            "/election/renew",
            &RenewRequest {
                election,
                leader_id,
                term,
                lease_id,
                ttl_seconds: ttl.as_secs(),
            },
        )
        .await
    }

    async fn configure_imagery_topics(&self) -> Result<(), String> {
        let _: TopicsResponse = self
            .post(
                "/topics",
                &TopicsRequest {
                    topics: vec![
                        TopicConfiguration::persistent_single("imagery.job.command"),
                        TopicConfiguration::persistent_single("imagery.chunk.work"),
                        TopicConfiguration::persistent_single("imagery.chunk.result"),
                        TopicConfiguration::persistent_single("imagery.tile.stored"),
                        TopicConfiguration::ephemeral_broadcast("imagery.job.events"),
                        TopicConfiguration::persistent_broadcast("imagery.repository.changed"),
                        TopicConfiguration::ephemeral_broadcast("system.component.heartbeat"),
                        TopicConfiguration::persistent_broadcast("system.election.changed"),
                    ],
                },
            )
            .await?;
        Ok(())
    }

    async fn publish_outbox(&self, source: &str, event: OutboxEvent) -> Result<(), String> {
        let topic = outbox_topic(&event);
        let event_id = event.id.clone();
        let mut attributes = std::collections::HashMap::new();
        attributes.insert("event_type".into(), event.event_type.clone());
        attributes.insert("aggregate_type".into(), event.aggregate_type.clone());
        attributes.insert("aggregate_id".into(), event.aggregate_id.clone());
        attributes.insert("source".into(), source.to_owned());
        let payload = serde_json::json!({
            "event_id": event.id,
            "event_type": event.event_type,
            "aggregate_type": event.aggregate_type,
            "aggregate_id": event.aggregate_id,
            "payload": event.payload,
            "attempt": event.attempts,
            "source": source,
            "occurred_at": event.created_at,
        });
        let response: PublishResponse = self
            .post(
                "/publish",
                &PublishRequest {
                    topic,
                    payload,
                    attributes,
                    idempotency_key: &event_id,
                },
            )
            .await?;
        if response.message_id.is_empty() {
            return Err("broker returned an empty message_id".into());
        }
        Ok(())
    }

    async fn poll(
        &self,
        topic: &str,
        consumer_id: &str,
        consumer_name: &str,
    ) -> Result<Option<BrokerMessage>, String> {
        let bytes = serde_json::to_vec(&PollRequest {
            topic,
            consumer_id,
            consumer_name,
            wait_ms: 20_000,
            attributes: std::collections::HashMap::from([
                ("component".to_string(), "cangling-map".to_string()),
                ("role".to_string(), "imagery-master".to_string()),
            ]),
        })
        .map_err(|error| error.to_string())?;
        let response = tokio::time::timeout(
            Duration::from_secs(25),
            self.request(Method::POST, "/consume/poll", bytes),
        )
        .await
        .map_err(|_| "broker consume poll timed out".to_string())??;
        if response.0 == hyper::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !response.0.is_success() {
            return Err(format!(
                "broker returned {}: {}",
                response.0,
                String::from_utf8_lossy(&response.1).trim()
            ));
        }
        serde_json::from_slice(&response.1)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    async fn ack(
        &self,
        message_id: &str,
        lease: &str,
        success: bool,
        error: &str,
    ) -> Result<(), String> {
        let response: AckResponse = self
            .post(
                "/consume/ack",
                &AckRequest {
                    message_id,
                    lease,
                    success,
                    error,
                },
            )
            .await?;
        if response.accepted {
            Ok(())
        } else {
            Err("broker rejected imagery ACK".into())
        }
    }

    async fn renew_message(&self, message_id: &str, lease: &str) -> Result<(), String> {
        let response: ConsumeRenewResponse = self
            .post("/consume/renew", &ConsumeRenewRequest { message_id, lease })
            .await?;
        if response.renewed {
            Ok(())
        } else {
            Err("broker rejected imagery message lease renewal".into())
        }
    }

    async fn fetch_snapshot(&self, base_url: &str) -> Result<ImagerySnapshot, String> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!(
                "{}/api/imagery/snapshot",
                base_url.trim_end_matches('/')
            ))
            .body(Full::new(Bytes::new()))
            .map_err(|error| error.to_string())?;
        let response = tokio::time::timeout(Duration::from_secs(10), self.client.request(request))
            .await
            .map_err(|_| "imagery snapshot request timed out".to_string())?
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| error.to_string())?
            .to_bytes();
        if !status.is_success() {
            return Err(format!(
                "imagery leader returned {status}: {}",
                String::from_utf8_lossy(&body).trim()
            ));
        }
        serde_json::from_slice(&body).map_err(|error| error.to_string())
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        bytes: Vec<u8>,
    ) -> Result<(hyper::StatusCode, Bytes), String> {
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("{}{}", self.base_url, path))
            .header(header::CONTENT_TYPE, "application/json");
        if !self.token.is_empty() {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {}", self.token));
        }
        let request = builder
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map_err(|error| error.to_string())?
            .to_bytes();
        Ok((status, body))
    }

    async fn post<T: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, String> {
        let bytes = serde_json::to_vec(body).map_err(|error| error.to_string())?;
        let (status, body) = self.request(Method::POST, path, bytes).await?;
        if !status.is_success() {
            return Err(format!(
                "broker returned {status}: {}",
                String::from_utf8_lossy(&body).trim()
            ));
        }
        serde_json::from_slice(&body).map_err(|error| error.to_string())
    }
}

fn outbox_topic(event: &OutboxEvent) -> &'static str {
    match event.event_type.as_str() {
        "imagery.tile.stored" => "imagery.tile.stored",
        "imagery.job.created"
            if event
                .payload
                .get("mode")
                .and_then(serde_json::Value::as_str)
                != Some("browser") =>
        {
            "imagery.job.command"
        }
        "imagery.chunk.created" => "imagery.chunk.work",
        "imagery.chunk.completed" | "imagery.chunk.failed" => "imagery.chunk.result",
        event_type if event_type.starts_with("imagery.repository.") => "imagery.repository.changed",
        _ => "imagery.job.events",
    }
}

fn spawn_inbox_topic_consumer(
    leadership: MapLeadership,
    broker: BrokerElectionClient,
    imagery: ImageryStore,
    topic: &'static str,
    expected_event_type: &'static str,
    consumer_suffix: &'static str,
) {
    tokio::spawn(async move {
        let consumer_id = format!("{}:{consumer_suffix}", leadership.candidate_id);
        loop {
            if leadership.leader_term().await.is_none() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            if let Err(error) = apply_pending_inbox(&imagery).await {
                tracing::warn!(%error, "could not apply pending imagery Inbox events");
            }
            let message = match broker
                .poll(topic, &consumer_id, &leadership.candidate_id)
                .await
            {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(%error, %topic, "could not poll imagery topic");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            };
            let result = receive_broker_event(&imagery, &message, expected_event_type).await;
            let still_leader = leadership.leader_term().await.is_some();
            let success = result.is_ok() && still_leader;
            let error = match result {
                Ok(()) if still_leader => String::new(),
                Ok(()) => "imagery leadership changed before ACK".to_string(),
                Err(error) => error,
            };
            if let Err(ack_error) = broker
                .ack(&message.id, &message.lease, success, &error)
                .await
            {
                tracing::warn!(%ack_error, message_id = %message.id, %topic, "could not ACK imagery message");
            }
        }
    });
}

pub(crate) fn configured_chunk_tiles() -> u32 {
    std::env::var("CANGLING_MAP_CHUNK_TILES")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(DEFAULT_CHUNK_TILES)
        .clamp(1, 65_536)
}

fn configured_worker_concurrency() -> usize {
    std::env::var("CANGLING_MAP_IMAGERY_WORKER_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8)
        .clamp(1, 64)
}

fn configured_worker_min_concurrency(max: usize) -> usize {
    std::env::var("CANGLING_MAP_IMAGERY_WORKER_MIN_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, max)
}

fn configured_worker_target_latency() -> Duration {
    let milliseconds = std::env::var("CANGLING_MAP_IMAGERY_TARGET_TILE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(500)
        .clamp(50, 60_000);
    Duration::from_millis(milliseconds)
}

async fn process_chunk_work(
    broker: &BrokerElectionClient,
    imagery: &ImageryStore,
    tile_client: &super::imagery::ImageryTileClient,
    http: &reqwest::Client,
    allowed_hosts: &std::collections::HashSet<String>,
    adaptive: &AdaptiveTileConcurrency,
    metrics: &WorkerMetrics,
    message: &BrokerMessage,
) -> Result<u64, String> {
    if message.payload_encoding != "utf-8" {
        return Err(format!(
            "unsupported Broker payload encoding: {}",
            message.payload_encoding
        ));
    }
    let envelope: BrokerEventEnvelope =
        serde_json::from_str(&message.payload).map_err(|error| error.to_string())?;
    if envelope.event_type != "imagery.chunk.created" {
        return Err(format!(
            "unexpected imagery chunk event: {}",
            envelope.event_type
        ));
    }
    let work: ChunkWorkEvent =
        serde_json::from_value(envelope.payload).map_err(|error| error.to_string())?;
    let renew_interval = Duration::from_millis(message.visibility_timeout_ms.max(3_000) / 3);
    let mut last_renewal = Instant::now();
    let job = loop {
        let Some(job) = imagery
            .get_job(&work.job_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            // A deleted task makes already published Broker work obsolete.
            // Returning success lets the consumer ACK it instead of retrying
            // a task which the operator deliberately removed.
            return Ok(0);
        };
        match job.status {
            JobStatus::Running => break job,
            JobStatus::Paused => {
                // Keep the durable Broker lease alive while an operator has
                // paused the job. This avoids consuming retry attempts and
                // resumes from the same idempotent chunk after the status is
                // changed back to running.
                if last_renewal.elapsed() >= renew_interval {
                    broker.renew_message(&message.id, &message.lease).await?;
                    last_renewal = Instant::now();
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            JobStatus::Cancelled | JobStatus::Completed | JobStatus::Failed => {
                // ACK obsolete work without downloading or writing tiles.
                return Ok(0);
            }
            JobStatus::Pending => {
                return Err("imagery job has not entered the running state".into());
            }
        }
    };
    let repository = imagery
        .get_repository(&work.repository_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("imagery repository not found: {}", work.repository_id))?;
    if job.repository_id != repository.id || job.leader_term != work.leader_term {
        return Err("imagery chunk fencing term or repository does not match the job".into());
    }
    for template in repository.source_url_templates() {
        let source = reqwest::Url::parse(template)
            .map_err(|error| format!("invalid imagery source URL: {error}"))?;
        let source_host = source
            .host_str()
            .ok_or_else(|| "imagery source URL has no host".to_string())?
            .to_ascii_lowercase();
        if !allowed_hosts.contains(&source_host) {
            return Err(format!(
                "imagery source host {source_host} is not in CANGLING_MAP_IMAGERY_SOURCE_HOSTS"
            ));
        }
    }
    let completed_tiles = if work.planned_tiles > 0 {
        work.planned_tiles
    } else {
        (u64::from(work.max_x - work.min_x) + 1) * (u64::from(work.max_y - work.min_y) + 1)
    };
    let mut coordinates = std::collections::VecDeque::new();
    for y in work.min_y..=work.max_y {
        for x in work.min_x..=work.max_x {
            coordinates.push_back((x, y));
        }
    }
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        let Some(current_job) = imagery
            .get_job(&work.job_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            tasks.abort_all();
            return Ok(0);
        };
        let current_status = current_job.status;
        match current_status {
            JobStatus::Running => {}
            JobStatus::Paused => {
                if last_renewal.elapsed() >= renew_interval {
                    broker.renew_message(&message.id, &message.lease).await?;
                    last_renewal = Instant::now();
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            JobStatus::Cancelled | JobStatus::Completed | JobStatus::Failed => {
                tasks.abort_all();
                return Ok(0);
            }
            JobStatus::Pending => {
                tasks.abort_all();
                return Err("imagery job returned to the pending state".into());
            }
        }
        while tasks.len() < adaptive.current() {
            let Some((x, y)) = coordinates.pop_front() else {
                break;
            };
            let imagery = imagery.clone();
            let tile_client = tile_client.clone();
            let http = http.clone();
            let repository = repository.clone();
            let job_id = job.id.clone();
            let zoom = work.z;
            tasks.spawn(async move {
                let started = Instant::now();
                let result = process_one_tile(
                    &imagery,
                    &tile_client,
                    &http,
                    &repository,
                    &job_id,
                    zoom,
                    x,
                    y,
                )
                .await;
                (result, started.elapsed())
            });
        }
        if tasks.is_empty() {
            break;
        }
        let renew_after = renew_interval.saturating_sub(last_renewal.elapsed());
        tokio::select! {
            result = tasks.join_next() => {
                match result {
                    Some(Ok((Ok(()), elapsed))) => {
                        adaptive.observe_success(elapsed);
                        metrics.update_adaptive(adaptive);
                    }
                    Some(Ok((Err(error), _))) => {
                        tasks.abort_all();
                        return Err(error);
                    }
                    Some(Err(error)) => {
                        tasks.abort_all();
                        return Err(format!("imagery tile worker task failed: {error}"));
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep(renew_after) => {
                broker.renew_message(&message.id, &message.lease).await?;
                last_renewal = Instant::now();
            }
        }
    }
    imagery
        .emit_chunk_result(&ChunkResultEvent {
            chunk_id: work.chunk_id.clone(),
            job_id: work.job_id.clone(),
            repository_id: work.repository_id,
            completed_tiles,
            failed_tiles: 0,
            leader_term: work.leader_term,
            error: None,
        })
        .await
        .map_err(|error| error.to_string())?;
    // Do not ACK the work item until the local transactional Outbox has been
    // accepted by Broker. If this process dies before then, the durable work
    // message is redelivered and tile receipts make the retry inexpensive.
    while imagery
        .has_unpublished_outbox("job", &work.job_id)
        .await
        .map_err(|error| error.to_string())?
        || imagery
            .has_unpublished_outbox("chunk", &work.chunk_id)
            .await
            .map_err(|error| error.to_string())?
    {
        if last_renewal.elapsed() >= renew_interval {
            broker.renew_message(&message.id, &message.lease).await?;
            last_renewal = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(completed_tiles)
}

#[allow(clippy::too_many_arguments)]
async fn process_one_tile(
    imagery: &ImageryStore,
    tile_client: &super::imagery::ImageryTileClient,
    http: &reqwest::Client,
    repository: &map_imagery::Repository,
    job_id: &str,
    zoom: u8,
    x: u32,
    y: u32,
) -> Result<(), String> {
    if imagery
        .repository_tile_exists(&repository.id, zoom, x, y)
        .await
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }
    let templates = repository.source_url_templates();
    if templates.is_empty() {
        return Err("imagery repository has no source URL template".into());
    }
    let start = (usize::from(zoom)
        .wrapping_mul(31)
        .wrapping_add(x as usize)
        .wrapping_mul(31)
        .wrapping_add(y as usize))
        % templates.len();
    let mut source_bytes = None;
    let mut last_error = String::new();
    for offset in 0..templates.len() {
        let url = templates[(start + offset) % templates.len()]
            .replace("{z}", &zoom.to_string())
            .replace("{x}", &x.to_string())
            .replace("{y}", &y.to_string());
        let response = match http.get(&url).send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("source GET {url} failed: {error}");
                continue;
            }
        };
        if !response.status().is_success() {
            last_error = format!("source GET {url} returned {}", response.status());
            continue;
        }
        if response
            .content_length()
            .is_some_and(|size| size > super::imagery::MAX_TILE_BYTES as u64)
        {
            last_error = format!("source tile {url} exceeds the size limit");
            continue;
        }
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                last_error = format!("source GET {url} body failed: {error}");
                continue;
            }
        };
        if bytes.is_empty() || bytes.len() > super::imagery::MAX_TILE_BYTES {
            last_error = format!("source tile {url} has an invalid size");
            continue;
        }
        if let Err(error) = super::imagery::validate_tile_bytes(repository.format, &bytes) {
            last_error = format!("source tile {url} is invalid: {error}");
            continue;
        }
        source_bytes = Some(bytes);
        break;
    }
    let bytes = source_bytes.ok_or_else(|| {
        if last_error.is_empty() {
            "imagery repository has no source URL template".to_string()
        } else {
            format!("all imagery source URL templates failed; {last_error}")
        }
    })?;
    tile_client
        .put(repository, zoom, x, y, repository.format, &bytes)
        .await?;
    imagery
        .record_tile_stored(job_id, zoom, x, y, repository.format, &bytes)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

async fn report_terminal_chunk_result(
    broker: &BrokerElectionClient,
    imagery: &ImageryStore,
    message: &BrokerMessage,
    error: &str,
) -> Result<(), String> {
    let envelope: BrokerEventEnvelope =
        serde_json::from_str(&message.payload).map_err(|failure| failure.to_string())?;
    let work: ChunkWorkEvent =
        serde_json::from_value(envelope.payload).map_err(|failure| failure.to_string())?;
    let _repository = imagery
        .get_repository(&work.repository_id)
        .await
        .map_err(|failure| failure.to_string())?
        .ok_or_else(|| format!("imagery repository not found: {}", work.repository_id))?;
    let total_tiles = if work.planned_tiles > 0 {
        work.planned_tiles
    } else {
        (u64::from(work.max_x - work.min_x) + 1) * (u64::from(work.max_y - work.min_y) + 1)
    };
    let missing_tiles = imagery
        .count_missing_repository_tiles_in_range(
            &work.repository_id,
            work.z,
            work.min_x,
            work.max_x,
            work.min_y,
            work.max_y,
        )
        .await
        .map_err(|failure| failure.to_string())?;
    let completed_tiles = total_tiles.saturating_sub(missing_tiles.min(total_tiles));
    let failed_tiles = total_tiles.saturating_sub(completed_tiles);
    let error = (failed_tiles > 0).then(|| error.chars().take(2_048).collect::<String>());
    imagery
        .emit_chunk_result(&ChunkResultEvent {
            chunk_id: work.chunk_id.clone(),
            job_id: work.job_id.clone(),
            repository_id: work.repository_id,
            completed_tiles,
            failed_tiles,
            leader_term: work.leader_term,
            error,
        })
        .await
        .map_err(|failure| failure.to_string())?;
    let renew_interval = Duration::from_millis(message.visibility_timeout_ms.max(3_000) / 3);
    let mut last_renewal = Instant::now();
    while imagery
        .has_unpublished_outbox("chunk", &work.chunk_id)
        .await
        .map_err(|failure| failure.to_string())?
    {
        if last_renewal.elapsed() >= renew_interval {
            broker.renew_message(&message.id, &message.lease).await?;
            last_renewal = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

async fn apply_pending_inbox(imagery: &ImageryStore) -> Result<(), String> {
    let events = imagery
        .pending_inbox(100)
        .await
        .map_err(|error| error.to_string())?;
    for event in events {
        if let Err(error) = apply_inbox_event(imagery, &event).await {
            let _ = imagery.mark_inbox_failed(&event.event_id, &error).await;
            return Err(error);
        }
        imagery
            .mark_inbox_applied(&event.event_id)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn receive_broker_event(
    imagery: &ImageryStore,
    message: &BrokerMessage,
    expected_event_type: &str,
) -> Result<(), String> {
    if message.payload_encoding != "utf-8" {
        return Err(format!(
            "unsupported Broker payload encoding: {}",
            message.payload_encoding
        ));
    }
    let envelope: BrokerEventEnvelope =
        serde_json::from_str(&message.payload).map_err(|error| error.to_string())?;
    let expected = envelope.event_type == expected_event_type
        || (expected_event_type == "imagery.chunk.result"
            && matches!(
                envelope.event_type.as_str(),
                "imagery.chunk.completed" | "imagery.chunk.failed"
            ));
    if !expected {
        return Err(format!(
            "unexpected imagery event {} on {expected_event_type}",
            envelope.event_type,
        ));
    }
    let leader_term = event_leader_term(&envelope.event_type, &envelope.payload)?;
    imagery
        .record_inbox_event(&InboxEventInput {
            event_id: envelope.event_id.clone(),
            event_type: envelope.event_type.clone(),
            source: envelope.source,
            leader_term,
            payload: envelope.payload.clone(),
        })
        .await
        .map_err(|error| error.to_string())?;
    let event = InboxEvent {
        event_id: envelope.event_id,
        event_type: envelope.event_type,
        source: None,
        leader_term,
        payload: envelope.payload,
        attempts: 0,
        received_at: message.created_at.clone(),
    };
    if let Err(error) = apply_inbox_event(imagery, &event).await {
        let _ = imagery.mark_inbox_failed(&event.event_id, &error).await;
        return Err(error);
    }
    imagery
        .mark_inbox_applied(&event.event_id)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn event_leader_term(event_type: &str, payload: &serde_json::Value) -> Result<u64, String> {
    match event_type {
        "imagery.tile.stored" => serde_json::from_value::<TileStoredEvent>(payload.clone())
            .map(|event| event.leader_term)
            .map_err(|error| error.to_string()),
        "imagery.job.created" => serde_json::from_value::<JobCommandEvent>(payload.clone())
            .map(|event| event.leader_term)
            .map_err(|error| error.to_string()),
        "imagery.chunk.completed" | "imagery.chunk.failed" => {
            serde_json::from_value::<ChunkResultEvent>(payload.clone())
                .map(|event| event.leader_term)
                .map_err(|error| error.to_string())
        }
        _ => Err(format!("unsupported imagery event: {event_type}")),
    }
}

async fn apply_inbox_event(imagery: &ImageryStore, event: &InboxEvent) -> Result<(), String> {
    match event.event_type.as_str() {
        "imagery.tile.stored" => {
            let tile: TileStoredEvent =
                serde_json::from_value(event.payload.clone()).map_err(|error| error.to_string())?;
            if imagery
                .get_job(&tile.job_id)
                .await
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(());
            }
            imagery
                .apply_tile_stored_event(&tile)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        "imagery.job.created" => {
            let command: JobCommandEvent =
                serde_json::from_value(event.payload.clone()).map_err(|error| error.to_string())?;
            let Some(job) = imagery
                .get_job(&command.job_id)
                .await
                .map_err(|error| error.to_string())?
            else {
                return Ok(());
            };
            if job.repository_id != command.repository_id
                || job.mode != command.mode
                || job.leader_term != command.leader_term
            {
                return Err(format!(
                    "imagery job command does not match canonical job {}",
                    command.job_id
                ));
            }
            imagery
                .ensure_job_chunks(&command.job_id, configured_chunk_tiles())
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        "imagery.chunk.completed" | "imagery.chunk.failed" => {
            let result: ChunkResultEvent =
                serde_json::from_value(event.payload.clone()).map_err(|error| error.to_string())?;
            if imagery
                .get_job(&result.job_id)
                .await
                .map_err(|error| error.to_string())?
                .is_none()
            {
                return Ok(());
            }
            imagery
                .apply_chunk_result(&result)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        event_type => Err(format!("unsupported Inbox event: {event_type}")),
    }
}

#[derive(Serialize)]
struct CampaignRequest<'a> {
    election: &'a str,
    candidate_id: &'a str,
    ttl_seconds: u64,
}

#[derive(Serialize)]
struct RenewRequest<'a> {
    election: &'a str,
    leader_id: &'a str,
    term: u64,
    lease_id: &'a str,
    ttl_seconds: u64,
}

#[derive(Serialize)]
struct PollRequest<'a> {
    topic: &'a str,
    consumer_id: &'a str,
    consumer_name: &'a str,
    wait_ms: u64,
    attributes: std::collections::HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct BrokerMessage {
    id: String,
    #[allow(dead_code)]
    topic: String,
    payload: String,
    payload_encoding: String,
    #[allow(dead_code)]
    attributes: serde_json::Value,
    created_at: String,
    lease: String,
    #[allow(dead_code)]
    visibility_timeout_ms: u64,
    #[serde(default = "default_delivery_attempt")]
    delivery_attempt: i64,
    #[serde(default = "default_max_delivery_attempts")]
    max_delivery_attempts: i64,
}

fn default_delivery_attempt() -> i64 {
    1
}

fn default_max_delivery_attempts() -> i64 {
    10
}

#[derive(Serialize)]
struct AckRequest<'a> {
    message_id: &'a str,
    lease: &'a str,
    success: bool,
    error: &'a str,
}

#[derive(Deserialize)]
struct AckResponse {
    accepted: bool,
}

#[derive(Serialize)]
struct ConsumeRenewRequest<'a> {
    message_id: &'a str,
    lease: &'a str,
}

#[derive(Deserialize)]
struct ConsumeRenewResponse {
    renewed: bool,
    #[allow(dead_code)]
    visibility_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct BrokerEventEnvelope {
    event_id: String,
    event_type: String,
    #[serde(default)]
    source: Option<String>,
    payload: serde_json::Value,
}

#[derive(Serialize)]
struct TopicsRequest {
    topics: Vec<TopicConfiguration>,
}

#[derive(Serialize)]
struct TopicConfiguration {
    topic: &'static str,
    delivery: &'static str,
    persistence: &'static str,
}

impl TopicConfiguration {
    fn persistent_single(topic: &'static str) -> Self {
        Self {
            topic,
            delivery: "single",
            persistence: "persistent",
        }
    }

    fn persistent_broadcast(topic: &'static str) -> Self {
        Self {
            topic,
            delivery: "broadcast",
            persistence: "persistent",
        }
    }

    fn ephemeral_broadcast(topic: &'static str) -> Self {
        Self {
            topic,
            delivery: "broadcast",
            persistence: "ephemeral",
        }
    }
}

#[derive(Deserialize)]
struct TopicsResponse {
    #[allow(dead_code)]
    topics: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct PublishRequest<'a> {
    topic: &'a str,
    payload: serde_json::Value,
    attributes: std::collections::HashMap<String, String>,
    idempotency_key: &'a str,
}

#[derive(Deserialize)]
struct PublishResponse {
    message_id: String,
    #[allow(dead_code)]
    duplicate: bool,
    #[allow(dead_code)]
    dropped: bool,
}

#[derive(Debug, Deserialize)]
struct LeaderWire {
    leader_id: String,
    term: u64,
    lease_id: String,
    #[serde(default)]
    remaining_ttl_ms: u64,
}

#[derive(Debug, Deserialize)]
struct CampaignResponse {
    won: bool,
    leader: LeaderWire,
}

#[derive(Debug, Deserialize)]
struct RenewResponse {
    renewed: bool,
    leader: Option<LeaderWire>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use map_imagery::{JobInput, JobMode, RepositoryInput, TileFormat};
    use sha2::{Digest, Sha256};

    #[test]
    fn adaptive_concurrency_uses_additive_increase_and_failure_backoff() {
        let adaptive = AdaptiveTileConcurrency::new(1, 8, Duration::from_millis(500));
        assert_eq!(adaptive.current(), 1);
        for _ in 0..16 {
            adaptive.observe_success(Duration::from_millis(100));
        }
        assert_eq!(adaptive.current(), 2);
        assert!(adaptive.average_micros() <= 100_000);

        for _ in 0..16 {
            adaptive.observe_success(Duration::from_secs(2));
        }
        assert_eq!(adaptive.current(), 1);

        for _ in 0..64 {
            adaptive.observe_success(Duration::from_millis(50));
        }
        assert!(adaptive.current() > 1);
        adaptive.observe_failure();
        assert!(adaptive.current() >= 1);
        assert!(adaptive.current() < 8);
    }

    #[test]
    fn worker_metric_history_is_bounded_and_includes_current_snapshot() {
        let metrics = WorkerMetrics::default();
        let adaptive = AdaptiveTileConcurrency::new(1, 8, Duration::from_millis(500));
        metrics.enabled.store(true, Ordering::Relaxed);
        metrics.concurrency.store(8, Ordering::Relaxed);
        metrics.min_concurrency.store(1, Ordering::Relaxed);
        metrics.update_adaptive(&adaptive);
        for value in 1..=130 {
            metrics.processed_tiles.store(value, Ordering::Relaxed);
            metrics.record_history(true);
        }
        let history = metrics.history();
        assert!(history.current.enabled);
        assert_eq!(history.current.concurrency, 8);
        assert_eq!(history.current.processed_tiles, 130);
        assert_eq!(history.samples.len(), 120);
        assert_eq!(history.samples.last().unwrap().processed_tiles, 130);
    }

    #[tokio::test]
    async fn standalone_mode_is_an_immediate_leader() {
        let leadership = MapLeadership {
            election: DEFAULT_ELECTION.into(),
            candidate_id: "map-0".into(),
            ttl: Duration::from_secs(15),
            broker: None,
            state: Arc::new(RwLock::new(LeadershipState {
                leader_id: Some("map-0".into()),
                term: 1,
                lease_id: Some("standalone".into()),
                valid_until: None,
            })),
            worker_metrics: Arc::new(WorkerMetrics::default()),
        };
        assert_eq!(leadership.leader_term().await, Some(1));
        let snapshot = leadership.snapshot().await;
        assert!(snapshot.is_leader);
        assert!(!snapshot.worker.enabled);
        assert_eq!(snapshot.worker.active_chunks, 0);
    }

    #[tokio::test]
    async fn expired_local_lease_fails_closed() {
        let leadership = MapLeadership {
            election: DEFAULT_ELECTION.into(),
            candidate_id: "map-0".into(),
            ttl: Duration::from_secs(15),
            broker: Some(BrokerElectionClient {
                base_url: "http://127.0.0.1:1".into(),
                token: String::new(),
                client: Client::builder(TokioExecutor::new()).build_http(),
            }),
            state: Arc::new(RwLock::new(LeadershipState {
                leader_id: Some("map-0".into()),
                term: 7,
                lease_id: Some("lease".into()),
                valid_until: Some(Instant::now() - Duration::from_millis(1)),
            })),
            worker_metrics: Arc::new(WorkerMetrics::default()),
        };
        assert_eq!(leadership.leader_term().await, None);
    }

    #[tokio::test]
    async fn broker_remaining_ttl_drives_local_safety_deadline() {
        let leadership = MapLeadership {
            election: DEFAULT_ELECTION.into(),
            candidate_id: "map-0".into(),
            ttl: Duration::from_secs(15),
            broker: Some(BrokerElectionClient {
                base_url: "http://127.0.0.1:1".into(),
                token: String::new(),
                client: Client::builder(TokioExecutor::new()).build_http(),
            }),
            state: Arc::new(RwLock::new(LeadershipState::follower())),
            worker_metrics: Arc::new(WorkerMetrics::default()),
        };
        leadership
            .apply_leader(
                LeaderWire {
                    leader_id: "map-0".into(),
                    term: 11,
                    lease_id: "lease-11".into(),
                    remaining_ttl_ms: 8_000,
                },
                true,
            )
            .await;
        assert_eq!(leadership.leader_term().await, Some(11));

        leadership
            .apply_leader(
                LeaderWire {
                    leader_id: "map-0".into(),
                    term: 11,
                    lease_id: "lease-11".into(),
                    remaining_ttl_ms: 500,
                },
                true,
            )
            .await;
        assert_eq!(leadership.leader_term().await, None);
    }

    #[tokio::test]
    async fn broker_tile_result_is_inboxed_and_applied_once() {
        let imagery = ImageryStore::in_memory().await.unwrap();
        let repository = imagery
            .create_repository(&RepositoryInput {
                name: "broker-result".into(),
                description: None,
                source_url_template: "https://tiles.example/{z}/{x}/{y}.png".into(),
                min_zoom: 0,
                max_zoom: 10,
                bounds: [-180.0, -85.0, 180.0, 85.0],
                format: TileFormat::Png,
                revocable: false,
            })
            .await
            .unwrap();
        let job = imagery
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Browser,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 2,
                leader_term: 5,
            })
            .await
            .unwrap();
        let digest = format!("{:x}", Sha256::digest(b"tile"));
        let payload = serde_json::json!({
            "event_id": "event-tile-1",
            "event_type": "imagery.tile.stored",
            "source": "map-1",
            "payload": {
                "job_id": job.id.clone(),
                "repository_id": repository.id.clone(),
                "z": 2,
                "x": 1,
                "y": 1,
                "format": "png",
                "size_bytes": 4,
                "sha256": digest,
                "leader_term": 5
            }
        });
        let message = BrokerMessage {
            id: "message-1".into(),
            topic: "imagery.tile.stored".into(),
            payload: payload.to_string(),
            payload_encoding: "utf-8".into(),
            attributes: serde_json::json!({}),
            created_at: "2026-11-02T00:00:00Z".into(),
            lease: "lease-1".into(),
            visibility_timeout_ms: 30_000,
            delivery_attempt: 1,
            max_delivery_attempts: 10,
        };
        receive_broker_event(&imagery, &message, "imagery.tile.stored")
            .await
            .unwrap();
        receive_broker_event(&imagery, &message, "imagery.tile.stored")
            .await
            .unwrap();
        assert_eq!(
            imagery
                .get_job(&job.id)
                .await
                .unwrap()
                .unwrap()
                .completed_tiles,
            1
        );
        assert!(imagery.pending_inbox(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn broker_job_command_creates_server_chunks_once() {
        let imagery = ImageryStore::in_memory().await.unwrap();
        let repository = imagery
            .create_repository(&RepositoryInput {
                name: "server-command".into(),
                description: None,
                source_url_template: "https://tiles.example/{z}/{x}/{y}.png".into(),
                min_zoom: 0,
                max_zoom: 1,
                bounds: [-180.0, -85.0, 180.0, 85.0],
                format: TileFormat::Png,
                revocable: false,
            })
            .await
            .unwrap();
        let job = imagery
            .create_job(&JobInput {
                repository_id: repository.id.clone(),
                mode: JobMode::Server,
                min_zoom: None,
                max_zoom: None,
                bounds: None,
                total_tiles: 0,
                leader_term: 21,
            })
            .await
            .unwrap();
        let payload = serde_json::json!({
            "event_id": "event-job-1",
            "event_type": "imagery.job.created",
            "source": "map-0",
            "payload": {
                "job_id": job.id.clone(),
                "repository_id": repository.id.clone(),
                "mode": "server",
                "leader_term": 21
            }
        });
        let message = BrokerMessage {
            id: "message-job-1".into(),
            topic: "imagery.job.command".into(),
            payload: payload.to_string(),
            payload_encoding: "utf-8".into(),
            attributes: serde_json::json!({}),
            created_at: "2026-11-02T00:00:00Z".into(),
            lease: "lease-job-1".into(),
            visibility_timeout_ms: 30_000,
            delivery_attempt: 1,
            max_delivery_attempts: 10,
        };
        receive_broker_event(&imagery, &message, "imagery.job.created")
            .await
            .unwrap();
        receive_broker_event(&imagery, &message, "imagery.job.created")
            .await
            .unwrap();
        let chunks = imagery.list_job_chunks(&job.id).await.unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            imagery.get_job(&job.id).await.unwrap().unwrap().total_tiles,
            5
        );
        assert!(imagery.pending_inbox(10).await.unwrap().is_empty());

        let result_payload = serde_json::json!({
            "event_id": "event-chunk-result-1",
            "event_type": "imagery.chunk.completed",
            "source": "map-1",
            "payload": {
                "chunk_id": chunks[0].id.clone(),
                "job_id": job.id.clone(),
                "repository_id": repository.id.clone(),
                "completed_tiles": 1,
                "failed_tiles": 0,
                "leader_term": 21
            }
        });
        let result_message = BrokerMessage {
            id: "message-chunk-result-1".into(),
            topic: "imagery.chunk.result".into(),
            payload: result_payload.to_string(),
            payload_encoding: "utf-8".into(),
            attributes: serde_json::json!({}),
            created_at: "2026-11-02T00:00:01Z".into(),
            lease: "lease-chunk-result-1".into(),
            visibility_timeout_ms: 30_000,
            delivery_attempt: 1,
            max_delivery_attempts: 10,
        };
        receive_broker_event(&imagery, &result_message, "imagery.chunk.completed")
            .await
            .unwrap();
        assert_eq!(
            imagery.list_job_chunks(&job.id).await.unwrap()[0].status,
            "completed"
        );
    }

    #[test]
    fn server_jobs_and_chunks_use_durable_work_topics() {
        let event = |event_type: &str, payload: serde_json::Value| OutboxEvent {
            id: "event".into(),
            event_type: event_type.into(),
            aggregate_type: "job".into(),
            aggregate_id: "job-1".into(),
            payload,
            attempts: 0,
            created_at: String::new(),
        };
        assert_eq!(
            outbox_topic(&event(
                "imagery.job.created",
                serde_json::json!({"mode":"server"})
            )),
            "imagery.job.command"
        );
        assert_eq!(
            outbox_topic(&event(
                "imagery.job.created",
                serde_json::json!({"mode":"browser"})
            )),
            "imagery.job.events"
        );
        assert_eq!(
            outbox_topic(&event("imagery.chunk.created", serde_json::json!({}))),
            "imagery.chunk.work"
        );
    }
}
