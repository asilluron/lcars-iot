//! One TLS session: KV watches, command requests, and screen heartbeats.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_nats::jetstream::kv::Operation;
use futures::StreamExt;
use home_proto::v1::{
    BackendHeartbeat, Command, CommandResult, Event, ScreenError, ScreenHeartbeat, ScreenLayout,
};
use home_proto::{decode, encode};
use tokio::sync::mpsc;

use crate::config::SyncConfig;
use crate::intent::UiIntent;
use crate::layout::{baked_layout, validate_layout};
use crate::policy::{
    DragCoalescer, FRAME_MS, FrameCoalescer, LinkPhase, jitter_unit, link_phase, reconnect_delay,
};
use crate::project::project_layout;
use crate::store::{ReplyEffect, StateStore, TickAction};
use crate::subjects::{
    BACKEND_KEY, KV_CONFIG, KV_STATE, STREAM_EVENTS, command_subject, error_subject,
    heartbeat_subject, layout_key,
};
use crate::update::UiUpdate;

enum ConnEvent {
    Connected,
    Disconnected,
    Client(String),
}

enum KvKind {
    State,
    Layout,
}

struct KvRecord {
    key: String,
    value: Vec<u8>,
    revision: u64,
    deleted: bool,
    caught_up: bool,
}

enum WatchItem {
    State { generation: u64, record: KvRecord },
    Layout { generation: u64, record: KvRecord },
    Event { generation: u64, bytes: Vec<u8> },
    Notice { generation: u64, error: String },
    Failed { generation: u64, error: String },
}

enum SessionStop {
    Retry(String),
    Shutdown,
}

struct Watches {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Watches {
    fn abort(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

struct CpuSampler {
    last_ticks: Option<u64>,
    last_at: Option<Instant>,
}

impl CpuSampler {
    fn new() -> Self {
        Self {
            last_ticks: None,
            last_at: None,
        }
    }

    fn sample(&mut self) -> (f32, u32) {
        let rss = rss_mb();
        let Some(ticks) = proc_ticks() else {
            return (0.0, rss);
        };
        let now = Instant::now();
        let pct = match (self.last_ticks, self.last_at) {
            (Some(previous), Some(at)) => {
                let elapsed = now.duration_since(at).as_secs_f32().max(0.001);
                let delta = ticks.saturating_sub(previous) as f32;
                delta / 100.0 / elapsed * 100.0
            }
            _ => 0.0,
        };
        self.last_ticks = Some(ticks);
        self.last_at = Some(now);
        (pct, rss)
    }
}

pub async fn run(
    config: SyncConfig,
    mut intents: mpsc::Receiver<UiIntent>,
    updates: mpsc::Sender<UiUpdate>,
) {
    let mut attempt = 0_u32;
    loop {
        let _ = updates.send(link_update(LinkPhase::Connecting)).await;
        match connect_client(&config).await {
            Ok((client, events)) => {
                attempt = 0;
                match session(
                    config.clone(),
                    client,
                    events,
                    &mut intents,
                    updates.clone(),
                )
                .await
                {
                    SessionStop::Shutdown => break,
                    SessionStop::Retry(error) => {
                        tracing::warn!(error = %error, "nats session ended");
                        let _ = updates.send(link_update(LinkPhase::LinkLost)).await;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "nats connect failed");
                let _ = updates.send(link_update(LinkPhase::NoLink)).await;
            }
        }
        let delay = reconnect_delay(attempt, jitter_unit());
        attempt = attempt.saturating_add(1);
        tokio::time::sleep(delay).await;
    }
}

fn link_update(phase: LinkPhase) -> UiUpdate {
    UiUpdate::Link {
        label: phase.label(),
        controls_enabled: phase.controls_enabled(),
        values_dimmed: phase.values_dimmed(),
    }
}

async fn connect_client(
    config: &SyncConfig,
) -> Result<(async_nats::Client, mpsc::Receiver<ConnEvent>), String> {
    let (tx, rx) = mpsc::channel(32);
    let mut options = async_nats::ConnectOptions::new()
        .name(format!("lcars-{}", config.screen_id))
        .connection_timeout(Duration::from_secs(2))
        .request_timeout(Some(Duration::from_millis(1_000)))
        .max_reconnects(None)
        .reconnect_delay_callback(|attempts| {
            reconnect_delay(
                u32::try_from(attempts.saturating_sub(1)).unwrap_or(u32::MAX),
                jitter_unit(),
            )
        })
        .event_callback(move |event| {
            let tx = tx.clone();
            async move {
                let mapped = match event {
                    async_nats::Event::Connected => Some(ConnEvent::Connected),
                    async_nats::Event::Disconnected => Some(ConnEvent::Disconnected),
                    async_nats::Event::ClientError(error) => {
                        Some(ConnEvent::Client(error.to_string()))
                    }
                    _ => None,
                };
                if let Some(mapped) = mapped {
                    let _ = tx.send(mapped).await;
                }
            }
        });
    if config.require_tls {
        options = options.require_tls(true);
    }
    if let Some(path) = &config.ca_path {
        options = options.add_root_certificates(path.clone());
    }
    if let Some(creds) = &config.creds {
        options = options.credentials(creds).map_err(|err| err.to_string())?;
    }
    let addrs = config
        .nats_urls
        .iter()
        .map(|url| url.parse::<async_nats::ServerAddr>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let client = options
        .connect(addrs)
        .await
        .map_err(|err| err.to_string())?;
    Ok((client, rx))
}

async fn session(
    config: SyncConfig,
    client: async_nats::Client,
    mut events: mpsc::Receiver<ConnEvent>,
    intents: &mut mpsc::Receiver<UiIntent>,
    updates: mpsc::Sender<UiUpdate>,
) -> SessionStop {
    let (watch_tx, mut watch_rx) = mpsc::channel(256);
    let (reply_tx, mut reply_rx) = mpsc::channel(64);
    let mut generation = 1_u64;
    let mut watches = spawn_watches(&client, &config, generation, watch_tx.clone());
    let mut state = Live {
        config,
        client,
        store: StateStore::new(),
        layout: baked_layout(),
        drags: DragCoalescer::new(),
        coalescer: FrameCoalescer::new(FRAME_MS),
        updates,
        connected: true,
        disconnected_at: None,
        backend_unix_ms: None,
        started: Instant::now(),
        watch_started: Instant::now(),
        state_ready: false,
        layout_ready: false,
        saw_state: false,
        saw_layout: false,
        last_phase: None,
        last_heartbeat: Instant::now(),
        cpu: CpuSampler::new(),
        generation,
    };
    let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MS));

    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Some(ConnEvent::Disconnected) => {
                        state.connected = false;
                        if state.disconnected_at.is_none() {
                            state.disconnected_at = Some(Instant::now());
                        }
                        watches.abort();
                    }
                    Some(ConnEvent::Connected) => {
                        let resumed = !state.connected;
                        state.connected = true;
                        state.disconnected_at = None;
                        if resumed {
                            generation = generation.saturating_add(1);
                            state.generation = generation;
                            state.watch_started = Instant::now();
                            state.state_ready = false;
                            state.layout_ready = false;
                            state.saw_state = false;
                            state.saw_layout = false;
                            watches.abort();
                            watches = spawn_watches(&state.client, &state.config, generation, watch_tx.clone());
                        }
                    }
                    Some(ConnEvent::Client(error)) => {
                        tracing::warn!(error = %error, "nats client error");
                    }
                    None => return SessionStop::Retry("connection events ended".into()),
                }
            }
            item = watch_rx.recv() => {
                match item {
                    Some(item) => {
                        if let Some(stop) = state.on_watch(item).await {
                            watches.abort();
                            return stop;
                        }
                    }
                    None => return SessionStop::Retry("watch channel closed".into()),
                }
            }
            reply = reply_rx.recv() => {
                if let Some(bytes) = reply {
                    state.on_reply(bytes);
                }
            }
            intent = intents.recv() => {
                match intent {
                    Some(intent) => state.on_intent(intent, &reply_tx),
                    None => {
                        watches.abort();
                        return SessionStop::Shutdown;
                    }
                }
            }
            _ = tick.tick() => {
                state.on_tick(&reply_tx);
            }
        }
    }
}

struct Live {
    config: SyncConfig,
    client: async_nats::Client,
    store: StateStore,
    layout: ScreenLayout,
    drags: DragCoalescer,
    coalescer: FrameCoalescer<()>,
    updates: mpsc::Sender<UiUpdate>,
    connected: bool,
    disconnected_at: Option<Instant>,
    backend_unix_ms: Option<i64>,
    started: Instant,
    watch_started: Instant,
    state_ready: bool,
    layout_ready: bool,
    saw_state: bool,
    saw_layout: bool,
    last_phase: Option<LinkPhase>,
    last_heartbeat: Instant,
    cpu: CpuSampler,
    generation: u64,
}

impl Live {
    async fn on_watch(&mut self, item: WatchItem) -> Option<SessionStop> {
        match item {
            WatchItem::State { generation, record } if generation == self.generation => {
                self.on_state(record)
            }
            WatchItem::Layout { generation, record } if generation == self.generation => {
                self.on_layout(record).await
            }
            WatchItem::Event { generation, bytes } if generation == self.generation => {
                self.on_event(&bytes)
            }
            WatchItem::Notice { generation, error } if generation == self.generation => {
                tracing::warn!(error = %error, "event stream unavailable");
                let _ = self.updates.try_send(UiUpdate::Error(error));
            }
            WatchItem::Failed { generation, error } if generation == self.generation => {
                return Some(SessionStop::Retry(error));
            }
            _ => {}
        }
        None
    }

    fn on_state(&mut self, record: KvRecord) {
        self.saw_state = true;
        if record.caught_up {
            self.state_ready = true;
        }
        if record.key == BACKEND_KEY {
            if !record.deleted
                && let Ok(beat) = decode::<BackendHeartbeat>(&record.value)
            {
                self.backend_unix_ms = Some(beat.unix_ms);
            }
            return;
        }
        if record.deleted {
            self.store.apply_delete(&record.key, record.revision);
        } else {
            match decode::<home_proto::v1::EntityState>(&record.value) {
                Ok(state) => {
                    self.store.apply_kv(record.revision, state);
                }
                Err(error) => {
                    tracing::warn!(error = %error, key = %record.key, "entity decode");
                    return;
                }
            }
        }
        self.mark_tiles();
    }

    async fn on_layout(&mut self, record: KvRecord) {
        self.saw_layout = true;
        if record.caught_up {
            self.layout_ready = true;
        }
        if record.deleted {
            return;
        }
        let layout = match decode::<ScreenLayout>(&record.value) {
            Ok(layout) => layout,
            Err(error) => {
                self.report_error("layout_decode", &error.to_string()).await;
                return;
            }
        };
        if let Err(error) = validate_layout(&layout) {
            self.report_error("layout", &error.to_string()).await;
            return;
        }
        self.layout = layout.clone();
        let _ = self.updates.try_send(UiUpdate::Layout(layout));
        self.mark_tiles();
    }

    fn on_event(&self, bytes: &[u8]) {
        let event = match decode::<Event>(bytes) {
            Ok(event) => event,
            Err(error) => {
                tracing::warn!(error = %error, "event decode");
                return;
            }
        };
        let critical = event.severity == home_proto::v1::event::Severity::Critical as i32;
        let age_ms = unix_ms().saturating_sub(u64::try_from(event.unix_ms.max(0)).unwrap_or(0));
        if critical || age_ms < 60_000 {
            let _ = self.updates.try_send(UiUpdate::Alert {
                title: event.title,
                body: event.body,
                critical,
            });
        }
    }

    fn on_intent(&mut self, intent: UiIntent, reply_tx: &mpsc::Sender<Vec<u8>>) {
        let now = unix_ms();
        if let UiIntent::SubmitPin { command_id, pin } = intent {
            if let Some(command) = self.store.attach_pin(&command_id, pin) {
                self.dispatch(command, reply_tx);
            }
            return;
        }
        let intent = if intent.is_slider() {
            match self.drags.push(intent, now) {
                Some(intent) => intent,
                None => return,
            }
        } else {
            intent
        };
        self.start_command(intent, reply_tx);
    }

    fn on_tick(&mut self, reply_tx: &mpsc::Sender<Vec<u8>>) {
        let now = unix_ms();
        self.note_init_timeout();
        for intent in self.drags.flush_due(now) {
            self.start_command(intent, reply_tx);
        }
        for action in self.store.tick(now) {
            match action {
                TickAction::Retry(command) => self.dispatch(command, reply_tx),
                TickAction::Revert { entity_id, message } => {
                    let _ = self
                        .updates
                        .try_send(UiUpdate::CommandFailed { entity_id, message });
                    self.send_tiles_now();
                }
            }
        }
        self.flush_tiles(now);
        self.publish_phase();
        if self.last_heartbeat.elapsed() >= Duration::from_secs(10) {
            self.last_heartbeat = Instant::now();
            self.spawn_heartbeat();
        }
    }

    fn start_command(&mut self, intent: UiIntent, reply_tx: &mpsc::Sender<Vec<u8>>) {
        let entity = intent.entity_id().unwrap_or("").to_string();
        let action = intent.action_name();
        match self.store.begin(&intent, &self.config.screen_id, unix_ms()) {
            Ok(command) => {
                tracing::debug!(entity = %entity, action, command_id = %command.command_id, "command");
                self.dispatch(command, reply_tx);
                self.send_tiles_now();
            }
            Err(error) => {
                tracing::debug!(entity = %entity, action, ?error, "command dropped");
            }
        }
    }

    fn dispatch(&self, command: Command, reply_tx: &mpsc::Sender<Vec<u8>>) {
        let client = self.client.clone();
        let reply_tx = reply_tx.clone();
        let subject = command_subject(&command.entity_id);
        tokio::spawn(async move {
            let payload = encode(&command);
            if let Ok(bytes) = request_protobuf(&client, &subject, "Command", payload).await {
                let _ = reply_tx.send(bytes).await;
            }
        });
    }

    fn note_init_timeout(&mut self) {
        let elapsed = self.watch_started.elapsed();
        if !self.state_ready
            && ((elapsed > Duration::from_millis(500) && !self.saw_state)
                || elapsed > Duration::from_secs(2))
        {
            self.state_ready = true;
        }
        if !self.layout_ready
            && ((elapsed > Duration::from_millis(500) && !self.saw_layout)
                || elapsed > Duration::from_secs(2))
        {
            self.layout_ready = true;
        }
    }

    fn phase(&self) -> LinkPhase {
        if !self.connected {
            let elapsed = self
                .disconnected_at
                .map(|at| at.elapsed())
                .unwrap_or(Duration::ZERO);
            return link_phase(true, false, elapsed, false);
        }
        if !self.state_ready || !self.layout_ready {
            return LinkPhase::Connecting;
        }
        link_phase(true, true, Duration::ZERO, self.backend_stale())
    }

    fn backend_stale(&self) -> bool {
        match self.backend_unix_ms {
            Some(stamp) => unix_ms() as i64 - stamp > crate::policy::BACKEND_STALE_MS,
            None => self.started.elapsed() >= Duration::from_secs(15),
        }
    }

    fn publish_phase(&mut self) {
        let phase = self.phase();
        self.store.set_online(phase.controls_enabled());
        if self.last_phase == Some(phase) {
            return;
        }
        self.last_phase = Some(phase);
        let _ = self.updates.try_send(link_update(phase));
        if !phase.controls_enabled() {
            self.send_tiles_now();
        }
    }

    fn mark_tiles(&mut self) {
        self.coalescer.push("tiles", (), unix_ms());
    }

    fn flush_tiles(&mut self, now_ms: u64) {
        if self.coalescer.take_due(now_ms).is_some() {
            self.send_tiles_now();
        }
    }

    fn send_tiles_now(&self) {
        let tiles = project_layout(&self.layout, &self.store, unix_ms());
        let _ = self.updates.try_send(UiUpdate::Tiles(tiles));
    }

    fn spawn_heartbeat(&mut self) {
        let (cpu_pct, rss_mb) = self.cpu.sample();
        let beat = ScreenHeartbeat {
            screen_id: self.config.screen_id.clone(),
            version: self.config.version.clone(),
            uptime_s: self.started.elapsed().as_secs(),
            connected: self.connected,
            frame_ms_p99: 0.0,
            cmd_latency_ms_p95: self.store.latency_ms_p95(),
            cpu_pct,
            rss_mb,
        };
        let client = self.client.clone();
        let subject = heartbeat_subject(&self.config.screen_id);
        tokio::spawn(async move {
            let payload = encode(&beat);
            if let Err(error) =
                publish_protobuf(&client, &subject, "ScreenHeartbeat", payload).await
            {
                tracing::debug!(error = %error, "heartbeat publish failed");
            }
        });
    }

    async fn report_error(&self, code: &str, detail: &str) {
        tracing::warn!(code, detail, "screen error");
        let _ = self.updates.try_send(UiUpdate::Error(detail.to_string()));
        let error = ScreenError {
            screen_id: self.config.screen_id.clone(),
            code: code.to_string(),
            detail: detail.to_string(),
        };
        let client = self.client.clone();
        let subject = error_subject(&self.config.screen_id);
        if let Err(error) = publish_protobuf(&client, &subject, "ScreenError", encode(&error)).await
        {
            tracing::warn!(error = %error, "error publish failed");
        }
    }

    fn on_reply(&mut self, bytes: Vec<u8>) {
        let result = match decode::<CommandResult>(&bytes) {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(error = %error, "command result decode");
                return;
            }
        };
        match self.store.observe_reply(&result) {
            ReplyEffect::Reverted { entity_id, message } => {
                let _ = self
                    .updates
                    .try_send(UiUpdate::CommandFailed { entity_id, message });
                self.send_tiles_now();
            }
            ReplyEffect::PinRequired {
                entity_id,
                command_id,
            } => {
                let _ = self.updates.try_send(UiUpdate::PinRequired {
                    entity_id,
                    command_id,
                });
            }
            ReplyEffect::WaitingForState | ReplyEffect::Ignored => {}
        }
    }
}

fn spawn_watches(
    client: &async_nats::Client,
    config: &SyncConfig,
    generation: u64,
    tx: mpsc::Sender<WatchItem>,
) -> Watches {
    let state = tokio::spawn(watch_kv(
        client.clone(),
        KV_STATE,
        ">".into(),
        generation,
        KvKind::State,
        tx.clone(),
    ));
    let layout = tokio::spawn(watch_kv(
        client.clone(),
        KV_CONFIG,
        layout_key(&config.screen_id),
        generation,
        KvKind::Layout,
        tx.clone(),
    ));
    let events = tokio::spawn(watch_events(client.clone(), generation, tx));
    Watches {
        tasks: vec![state, layout, events],
    }
}

async fn watch_kv(
    client: async_nats::Client,
    bucket: &str,
    pattern: String,
    generation: u64,
    kind: KvKind,
    tx: mpsc::Sender<WatchItem>,
) {
    let failed = async {
        let context = async_nats::jetstream::new(client);
        let store = context
            .get_key_value(bucket)
            .await
            .map_err(|err| err.to_string())?;
        let mut watcher = store.watch(pattern).await.map_err(|err| err.to_string())?;
        while let Some(next) = watcher.next().await {
            let entry = next.map_err(|err| err.to_string())?;
            let record = KvRecord {
                key: entry.key,
                value: entry.value.to_vec(),
                revision: entry.revision,
                deleted: matches!(entry.operation, Operation::Delete | Operation::Purge),
                caught_up: entry.delta == 0,
            };
            let item = match kind {
                KvKind::State => WatchItem::State { generation, record },
                KvKind::Layout => WatchItem::Layout { generation, record },
            };
            if tx.send(item).await.is_err() {
                return Ok(());
            }
        }
        Err("kv watch ended".to_string())
    };
    if let Err(error) = failed.await {
        let _ = tx.send(WatchItem::Failed { generation, error }).await;
    }
}

async fn watch_events(client: async_nats::Client, generation: u64, tx: mpsc::Sender<WatchItem>) {
    let failed = async {
        let context = async_nats::jetstream::new(client);
        let stream = context
            .get_stream(STREAM_EVENTS)
            .await
            .map_err(|err| err.to_string())?;
        let config = async_nats::jetstream::consumer::pull::Config {
            filter_subject: "evt.>".into(),
            deliver_policy: async_nats::jetstream::consumer::DeliverPolicy::All,
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            ..Default::default()
        };
        let consumer = stream
            .create_consumer(config)
            .await
            .map_err(|err| err.to_string())?;
        let mut messages = consumer.messages().await.map_err(|err| err.to_string())?;
        while let Some(next) = messages.next().await {
            let message = next.map_err(|err| err.to_string())?;
            let bytes = message.payload.to_vec();
            if tx
                .send(WatchItem::Event { generation, bytes })
                .await
                .is_err()
            {
                return Ok(());
            }
            if let Err(error) = message.ack().await {
                tracing::debug!(error = %error, "event ack failed");
            }
        }
        Ok(())
    };
    if let Err(error) = failed.await {
        let _ = tx.send(WatchItem::Notice { generation, error }).await;
    }
}

fn protobuf_headers(message: &str) -> Result<async_nats::HeaderMap, String> {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Content-Type", home_proto::content_type(message));
    Ok(headers)
}

async fn publish_protobuf(
    client: &async_nats::Client,
    subject: &str,
    message: &str,
    payload: Vec<u8>,
) -> Result<(), String> {
    let headers = protobuf_headers(message)?;
    client
        .publish_with_headers(subject.to_string(), headers, payload.into())
        .await
        .map_err(|err| err.to_string())
}

async fn request_protobuf(
    client: &async_nats::Client,
    subject: &str,
    message: &str,
    payload: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let headers = protobuf_headers(message)?;
    let reply = client
        .request_with_headers(subject.to_string(), headers, payload.into())
        .await
        .map_err(|err| err.to_string())?;
    Ok(reply.payload.to_vec())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn rss_mb() -> u32 {
    #[cfg(target_os = "linux")]
    {
        let Ok(text) = std::fs::read_to_string("/proc/self/statm") else {
            return 0;
        };
        let pages: u64 = text
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        u32::try_from(pages * 4 / 1024).unwrap_or(u32::MAX)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

fn proc_ticks() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/stat").ok()?;
        let end = text.rfind(')')?;
        let fields: Vec<&str> = text[end + 2..].split_whitespace().collect();
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        Some(utime + stime)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
