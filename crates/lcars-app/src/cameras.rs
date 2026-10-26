//! Camera catalog and live view for the Security page.
//!
//! The catalog comes from `lcars.cameras.list`. Stills and the live Annex-B
//! stream are HTTP on the same lcars-api server, at the paths in that reply.
//! Names, secrets, and frame bytes are not logged.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use futures::StreamExt;
use lcars_api_proto::prost::Message;
use lcars_api_proto::v1::{ListCamerasRequest, ListCamerasResponse};
use lcars_api_proto::{CAMERAS_LIST, ERROR_CODE};
use lcars_ui::slint::{Image, ModelRc, Rgb8Pixel, SharedPixelBuffer, VecModel, Weak};
use lcars_ui::{AppWindow, CameraCard};
use tokio::sync::{mpsc, watch};

const LIVE_MAX_W: u32 = 852;
const LIVE_MAX_H: u32 = 250;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_ATTEMPTS: u32 = 4;
const FRAME_INTERVAL: Duration = Duration::from_millis(66);

#[derive(Clone)]
pub struct HttpConfig {
    pub base: String,
    pub user: Option<String>,
    pub secret: Option<String>,
}

pub enum Command {
    Open(String),
    Close,
}

/// Wire the Security page callbacks. Empty serials never reach here: the
/// mock pills only set the on-screen notice.
pub fn attach(app: &AppWindow, commands: mpsc::UnboundedSender<Command>) {
    let open = commands.clone();
    app.on_open_camera(move |sn| {
        let _ = open.send(Command::Open(sn.to_string()));
    });
    app.on_close_live(move || {
        let _ = commands.send(Command::Close);
    });
}

pub async fn post_status(weak: &Weak<AppWindow>, status: &str) {
    let _ = post(
        weak,
        UiUpdate {
            status: Some(status.to_string()),
            cards: None,
            live_open: None,
            live_name: None,
            live_image: None,
            live_frame: None,
        },
    )
    .await;
}

pub async fn run(
    clients: watch::Receiver<Option<async_nats::Client>>,
    http: Option<HttpConfig>,
    commands: mpsc::UnboundedReceiver<Command>,
    weak: Weak<AppWindow>,
) {
    let media = match http.clone().map(Media::new) {
        Some(Ok(media)) => Some(media),
        Some(Err(error)) => {
            tracing::warn!(error = %error, "camera http client failed");
            None
        }
        None => None,
    };
    let mut session = Session {
        clients,
        media,
        commands,
        weak,
        shown: Vec::new(),
        had_catalog: false,
        catalog_status: "LINKING".into(),
        live_sn: None,
        live: None,
        generation: Arc::new(AtomicU64::new(0)),
    };
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let wake = {
            let Session {
                commands, clients, ..
            } = &mut session;
            tokio::select! {
                biased;
                command = commands.recv() => Wake::Command(command),
                changed = clients.changed() => Wake::Client(changed),
                _ = interval.tick() => Wake::Tick,
            }
        };
        match wake {
            Wake::Command(None) => return,
            Wake::Command(Some(command)) => session.handle(command).await,
            Wake::Client(Err(_)) => return,
            Wake::Client(Ok(())) => session.on_client().await,
            Wake::Tick => session.refresh().await,
            Wake::Listed(_) | Wake::Still(_) => {}
        }
    }
}

enum Wake {
    Command(Option<Command>),
    Client(Result<(), watch::error::RecvError>),
    Tick,
    Listed(Result<Vec<ListedCamera>, ListFailure>),
    Still(Result<Option<Vec<u8>>, String>),
}

struct Session {
    clients: watch::Receiver<Option<async_nats::Client>>,
    media: Option<Media>,
    commands: mpsc::UnboundedReceiver<Command>,
    weak: Weak<AppWindow>,
    shown: Vec<Shown>,
    had_catalog: bool,
    catalog_status: String,
    live_sn: Option<String>,
    live: Option<LiveTask>,
    generation: Arc<AtomicU64>,
}

#[derive(Clone)]
struct Shown {
    camera: ListedCamera,
    still: Picture,
    has_still: bool,
}

struct LiveTask {
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for LiveTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Session {
    fn client(&self) -> Option<async_nats::Client> {
        self.clients.borrow().clone()
    }

    async fn on_client(&mut self) {
        if self.client().is_none() {
            if self.live_sn.is_none() {
                self.catalog_status = "NO LINK".into();
                self.publish(false).await;
            }
            return;
        }
        if !self.had_catalog && self.live_sn.is_none() {
            self.catalog_status = "LINKING".into();
            self.publish(false).await;
        }
        self.refresh().await;
    }

    async fn refresh(&mut self) {
        let Some(client) = self.client() else {
            return;
        };
        let listed = {
            let commands = &mut self.commands;
            tokio::select! {
                biased;
                command = commands.recv() => Wake::Command(command),
                listed = fetch_list(&client) => Wake::Listed(listed),
            }
        };
        let listed = match listed {
            Wake::Command(command) => {
                if let Some(command) = command {
                    self.handle(command).await;
                }
                return;
            }
            Wake::Listed(listed) => listed,
            Wake::Client(_) | Wake::Tick | Wake::Still(_) => return,
        };
        match listed {
            Ok(listed) => {
                self.had_catalog = true;
                let previous = std::mem::take(&mut self.shown);
                self.shown = listed
                    .into_iter()
                    .map(|camera| {
                        let kept = previous.iter().find(|shown| shown.camera.sn == camera.sn);
                        let (still, has_still) = kept
                            .map(|shown| (shown.still.clone(), shown.has_still))
                            .unwrap_or_else(|| (empty_picture(), false));
                        Shown {
                            camera,
                            still,
                            has_still,
                        }
                    })
                    .collect();
                if self.live_sn.is_none() {
                    self.catalog_status = if self.shown.is_empty() {
                        "NO CAMERAS".into()
                    } else {
                        count_label(self.shown.len())
                    };
                }
                self.publish(true).await;
                self.load_stills().await;
            }
            Err(ListFailure::Unavailable) => {
                self.had_catalog = true;
                self.shown.clear();
                if self.live_sn.is_none() {
                    self.catalog_status = "NO CAMERAS".into();
                }
                tracing::warn!("camera list unavailable");
                self.publish(true).await;
            }
            Err(ListFailure::Down(code)) => {
                tracing::warn!(code = %code, "camera list failed");
                if !self.had_catalog && self.live_sn.is_none() {
                    self.catalog_status = "NO LINK".into();
                    self.publish(false).await;
                }
            }
        }
    }

    async fn load_stills(&mut self) {
        let Some(media) = self.media.clone() else {
            return;
        };
        let paths: Vec<(String, String)> = self
            .shown
            .iter()
            .filter(|shown| self.live_sn.as_deref() != Some(shown.camera.sn.as_str()))
            .map(|shown| (shown.camera.sn.clone(), shown.camera.snapshot_path.clone()))
            .collect();
        for (sn, path) in paths {
            let shot = {
                let commands = &mut self.commands;
                tokio::select! {
                    biased;
                    command = commands.recv() => Wake::Command(command),
                    shot = media.snapshot(&path) => Wake::Still(shot),
                }
            };
            let shot = match shot {
                Wake::Command(command) => {
                    if let Some(command) = command {
                        self.handle(command).await;
                    }
                    return;
                }
                Wake::Still(shot) => shot,
                Wake::Client(_) | Wake::Tick | Wake::Listed(_) => return,
            };
            match shot {
                Ok(Some(bytes)) => {
                    if let Some(image) = jpeg_picture(&bytes) {
                        if let Some(shown) =
                            self.shown.iter_mut().find(|shown| shown.camera.sn == sn)
                        {
                            shown.still = image;
                            shown.has_still = true;
                        }
                        self.publish(true).await;
                    }
                }
                Ok(None) => {
                    if let Some(shown) = self.shown.iter_mut().find(|shown| shown.camera.sn == sn) {
                        shown.has_still = false;
                    }
                    self.publish(true).await;
                }
                Err(error) => {
                    tracing::warn!(sn = %sn, error = %error, "camera still failed");
                }
            }
        }
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Close => self.close_live().await,
            Command::Open(sn) => self.open_live(sn).await,
        }
    }

    async fn open_live(&mut self, sn: String) {
        if sn.is_empty() {
            return;
        }
        let Some(shown) = self.shown.iter().find(|shown| shown.camera.sn == sn) else {
            return;
        };
        let Some(media) = self.media.clone() else {
            self.catalog_status = "NO PICTURE".into();
            self.publish(false).await;
            return;
        };
        let camera = shown.camera.clone();
        let name = display_name(&camera);
        let still = shown.still.clone();
        let has_still = shown.has_still;
        let stamp = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.live = None;
        self.live_sn = Some(camera.sn.clone());
        let _ = post(
            &self.weak,
            UiUpdate {
                status: Some("LINKING".into()),
                cards: None,
                live_open: Some(true),
                live_name: Some(name.clone()),
                live_image: has_still.then(|| still.clone()),
                live_frame: Some(false),
            },
        )
        .await;
        let weak = self.weak.clone();
        let generation = Arc::clone(&self.generation);
        self.live = Some(LiveTask {
            handle: tokio::spawn(async move {
                play(Play {
                    stamp,
                    generation,
                    weak,
                    media,
                    sn: camera.sn,
                    path: camera.stream_path,
                    name,
                    still,
                    has_still,
                })
                .await;
            }),
        });
    }

    async fn close_live(&mut self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.live = None;
        self.live_sn = None;
        self.catalog_status = if !self.had_catalog {
            "LINKING".into()
        } else if self.shown.is_empty() {
            "NO CAMERAS".into()
        } else {
            count_label(self.shown.len())
        };
        let _ = post(
            &self.weak,
            UiUpdate {
                status: Some(self.catalog_status.clone()),
                cards: None,
                live_open: Some(false),
                live_name: None,
                live_image: None,
                live_frame: Some(false),
            },
        )
        .await;
    }

    async fn publish(&self, cards: bool) {
        // The play task owns the status line while a stream is open.
        let _ = post(
            &self.weak,
            UiUpdate {
                status: self.live_sn.is_none().then(|| self.catalog_status.clone()),
                cards: cards.then(|| self.shown.clone()),
                live_open: None,
                live_name: None,
                live_image: None,
                live_frame: None,
            },
        )
        .await;
    }
}

struct Play {
    stamp: u64,
    generation: Arc<AtomicU64>,
    weak: Weak<AppWindow>,
    media: Media,
    sn: String,
    path: String,
    name: String,
    still: Picture,
    has_still: bool,
}

impl Play {
    fn stale(&self) -> bool {
        self.generation.load(Ordering::SeqCst) != self.stamp
    }
}

async fn play(play: Play) {
    let mut decoder = AnnexDecoder::new();
    for attempt in 0..STREAM_ATTEMPTS {
        if play.stale() {
            return;
        }
        match stream_once(&play, &mut decoder).await {
            Ok(StreamEnd::Closed) | Err(_) if attempt + 1 < STREAM_ATTEMPTS && !play.stale() => {
                let _ = post(
                    &play.weak,
                    UiUpdate {
                        status: Some("RECONNECTING".into()),
                        cards: None,
                        live_open: None,
                        live_name: None,
                        live_image: None,
                        live_frame: Some(false),
                    },
                )
                .await;
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            Ok(StreamEnd::Closed) => break,
            Err(error) => {
                tracing::warn!(sn = %play.sn, error = %error, "camera stream failed");
                break;
            }
        }
    }
    if !play.stale() {
        let _ = post(
            &play.weak,
            UiUpdate {
                status: Some("NO PICTURE".into()),
                cards: None,
                live_open: Some(true),
                live_name: Some(play.name.clone()),
                live_image: None,
                live_frame: Some(false),
            },
        )
        .await;
    }
}

enum StreamEnd {
    Closed,
}

async fn stream_once(play: &Play, decoder: &mut AnnexDecoder) -> Result<StreamEnd, String> {
    let response = play.media.open_stream(&play.path).await?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(sn = %play.sn, status = %status, "camera stream rejected");
        return Err(status.as_str().to_string());
    }
    tracing::info!(sn = %play.sn, "camera stream open");
    if !play.stale() {
        let _ = post(
            &play.weak,
            UiUpdate {
                status: Some("LIVE".into()),
                cards: None,
                live_open: Some(true),
                live_name: Some(play.name.clone()),
                live_image: play.has_still.then(|| play.still.clone()),
                live_frame: Some(false),
            },
        )
        .await;
    }
    let mut body = response.bytes_stream();
    let mut next_paint = Instant::now();
    let mut saw_frame = false;
    while let Some(chunk) = body.next().await {
        if play.stale() {
            return Ok(StreamEnd::Closed);
        }
        let chunk = chunk.map_err(|error| error.to_string())?;
        let frames = decoder.push(&chunk).map_err(|error| error.to_string())?;
        if Instant::now() < next_paint {
            continue;
        }
        if let Some(frame) = frames.last() {
            next_paint = Instant::now() + FRAME_INTERVAL;
            let first = !saw_frame;
            saw_frame = true;
            let _ = post(
                &play.weak,
                UiUpdate {
                    status: None,
                    cards: None,
                    live_open: None,
                    live_name: None,
                    live_image: Some(frame.clone()),
                    live_frame: first.then_some(true),
                },
            )
            .await;
        }
    }
    tracing::info!(sn = %play.sn, "camera stream closed");
    Ok(StreamEnd::Closed)
}

struct UiUpdate {
    status: Option<String>,
    cards: Option<Vec<Shown>>,
    live_open: Option<bool>,
    live_name: Option<String>,
    live_image: Option<Picture>,
    live_frame: Option<bool>,
}

async fn post(weak: &Weak<AppWindow>, update: UiUpdate) -> bool {
    for _ in 0..100 {
        let weak = weak.clone();
        let update = UiUpdate {
            status: update.status.clone(),
            cards: update.cards.clone(),
            live_open: update.live_open,
            live_name: update.live_name.clone(),
            live_image: update.live_image.clone(),
            live_frame: update.live_frame,
        };
        if lcars_ui::slint::invoke_from_event_loop(move || apply(&weak, update)).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracing::warn!("ui event loop did not accept a camera update");
    false
}

fn apply(weak: &Weak<AppWindow>, update: UiUpdate) {
    let Some(app) = weak.upgrade() else {
        return;
    };
    if let Some(status) = update.status {
        app.set_camera_status(status.into());
    }
    if let Some(live_open) = update.live_open {
        app.set_live_open(live_open);
    }
    if let Some(name) = update.live_name {
        app.set_live_name(name.into());
    }
    if let Some(image) = update.live_image {
        app.set_live_image(slint_image(&image));
    }
    if let Some(frame) = update.live_frame {
        app.set_live_frame(frame);
    }
    if let Some(cards) = update.cards {
        app.set_cameras(ModelRc::new(VecModel::from(
            cards.into_iter().map(card_item).collect::<Vec<_>>(),
        )));
    }
}

fn card_item(shown: Shown) -> CameraCard {
    CameraCard {
        sn: shown.camera.sn.clone().into(),
        name: display_name(&shown.camera).into(),
        still: slint_image(&shown.still),
        has_still: shown.has_still,
    }
}

fn display_name(camera: &ListedCamera) -> String {
    if camera.name.is_empty() {
        camera.sn.clone()
    } else {
        camera.name.clone()
    }
}

fn count_label(count: usize) -> String {
    if count == 1 {
        "1 CAMERA".into()
    } else {
        format!("{count} CAMERAS")
    }
}

#[derive(Clone)]
struct ListedCamera {
    sn: String,
    name: String,
    stream_path: String,
    snapshot_path: String,
}

enum ListFailure {
    Unavailable,
    Down(String),
}

fn project(response: &ListCamerasResponse) -> Vec<ListedCamera> {
    response
        .cameras
        .iter()
        .filter(|camera| camera.has_camera)
        .map(|camera| ListedCamera {
            sn: camera.sn.clone(),
            name: camera.name.clone(),
            stream_path: camera.stream_path.clone(),
            snapshot_path: camera.snapshot_path.clone(),
        })
        .collect()
}

async fn fetch_list(client: &async_nats::Client) -> Result<Vec<ListedCamera>, ListFailure> {
    let reply = client
        .request(CAMERAS_LIST, ListCamerasRequest {}.encode_to_vec().into())
        .await
        .map_err(|error| ListFailure::Down(error.to_string()))?;
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(ERROR_CODE))
    {
        let code = code.as_str();
        if code == "503" {
            return Err(ListFailure::Unavailable);
        }
        return Err(ListFailure::Down(code.to_string()));
    }
    let response = ListCamerasResponse::decode(reply.payload)
        .map_err(|error| ListFailure::Down(error.to_string()))?;
    let count = response.cameras.len();
    tracing::info!(count, "cameras listed");
    Ok(project(&response))
}

fn join_url(base: &str, path: &str) -> Result<String, String> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains("://") {
        return Err("camera path is not on this server".into());
    }
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

fn authorization_value(user: &str, secret: &str) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"));
    format!("Basic {token}")
}

#[derive(Clone)]
struct Media {
    client: reqwest::Client,
    config: HttpConfig,
}

impl Media {
    fn new(config: HttpConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client, config })
    }

    fn request(&self, path: &str) -> Result<reqwest::RequestBuilder, String> {
        let url = join_url(&self.config.base, path)?;
        let mut request = self.client.get(url);
        if let (Some(user), Some(secret)) = (&self.config.user, &self.config.secret) {
            request = request.header(
                reqwest::header::AUTHORIZATION,
                authorization_value(user, secret),
            );
        }
        Ok(request)
    }

    async fn snapshot(&self, path: &str) -> Result<Option<Vec<u8>>, String> {
        let response = self
            .request(path)?
            .timeout(SNAPSHOT_TIMEOUT)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(response.status().as_str().to_string());
        }
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        if bytes.len() > 10 * 1024 * 1024 {
            return Err("still is too large".into());
        }
        Ok(Some(bytes.to_vec()))
    }

    async fn open_stream(&self, path: &str) -> Result<reqwest::Response, String> {
        self.request(path)?
            .send()
            .await
            .map_err(|error| error.to_string())
    }
}

#[derive(Clone)]
struct Picture {
    width: u32,
    height: u32,
    pixels: Vec<Rgb8Pixel>,
}

fn slint_image(picture: &Picture) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(picture.width, picture.height);
    buffer.make_mut_slice().copy_from_slice(&picture.pixels);
    Image::from_rgb8(buffer)
}

fn empty_picture() -> Picture {
    Picture {
        width: 1,
        height: 1,
        pixels: vec![Rgb8Pixel { r: 0, g: 0, b: 0 }],
    }
}

fn jpeg_picture(bytes: &[u8]) -> Option<Picture> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()?;
    let rgb = decoded.to_rgb8();
    let pixels = rgb
        .pixels()
        .map(|pixel| Rgb8Pixel {
            r: pixel[0],
            g: pixel[1],
            b: pixel[2],
        })
        .collect();
    Some(Picture {
        width: rgb.width(),
        height: rgb.height(),
        pixels,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    H264,
    H265,
}

fn sniff(bytes: &[u8]) -> Option<Codec> {
    let mut index = 0;
    while index + 3 < bytes.len() {
        let header = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
            index + 4
        } else if bytes[index..].starts_with(&[0, 0, 1]) {
            index + 3
        } else {
            index += 1;
            continue;
        };
        if header >= bytes.len() {
            break;
        }
        let nal = bytes[header];
        if nal & 0x1f == 7 {
            return Some(Codec::H264);
        }
        if (nal >> 1) & 0x3f == 32 {
            return Some(Codec::H265);
        }
        index = header + 1;
    }
    None
}

type RgbFrame = Picture;

struct AnnexDecoder {
    inner: Option<OpenDecoder>,
    pending: Vec<u8>,
}

struct OpenDecoder {
    decoder: ffmpeg_next::decoder::Video,
    parser: *mut ffmpeg_next::ffi::AVCodecParserContext,
    scaler: Option<ffmpeg_next::software::scaling::context::Context>,
    source: (u32, u32, ffmpeg_next::format::Pixel),
}

unsafe impl Send for OpenDecoder {}

impl Drop for OpenDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.parser.is_null() {
                ffmpeg_next::ffi::av_parser_close(self.parser);
                self.parser = std::ptr::null_mut();
            }
        }
    }
}

impl AnnexDecoder {
    fn new() -> Self {
        Self {
            inner: None,
            pending: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Result<Vec<RgbFrame>, String> {
        self.pending.extend_from_slice(chunk);
        if self.pending.len() > 4 * 1024 * 1024 {
            let extra = self.pending.len() - 4 * 1024 * 1024;
            self.pending.drain(..extra);
        }
        if self.inner.is_none() {
            let Some(codec) = sniff(&self.pending) else {
                return Ok(Vec::new());
            };
            self.open(codec)?;
        }
        let buffered = std::mem::take(&mut self.pending);
        let mut input = buffered.as_slice();
        let mut frames = Vec::new();
        while !input.is_empty() {
            let consumed = self.feed(input, &mut frames)?;
            if consumed == 0 {
                self.pending.extend_from_slice(input);
                break;
            }
            input = &input[consumed.min(input.len())..];
        }
        Ok(frames)
    }

    fn open(&mut self, codec: Codec) -> Result<(), String> {
        ffmpeg_next::init().map_err(|error| error.to_string())?;
        unsafe {
            ffmpeg_next::ffi::av_log_set_level(ffmpeg_next::ffi::AV_LOG_ERROR);
        }
        let id = match codec {
            Codec::H264 => ffmpeg_next::codec::Id::H264,
            Codec::H265 => ffmpeg_next::codec::Id::HEVC,
        };
        let found = ffmpeg_next::decoder::find(id).ok_or("no decoder for this stream")?;
        let mut context = ffmpeg_next::codec::context::Context::new_with_codec(found);
        unsafe {
            (*context.as_mut_ptr()).thread_count = 1;
        }
        let decoder = context
            .decoder()
            .video()
            .map_err(|error| error.to_string())?;
        let parser_id = match codec {
            Codec::H264 => ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_H264,
            Codec::H265 => ffmpeg_next::ffi::AVCodecID::AV_CODEC_ID_HEVC,
        };
        let parser = unsafe { ffmpeg_next::ffi::av_parser_init(parser_id as i32) };
        if parser.is_null() {
            return Err("annex-b parser is unavailable".into());
        }
        self.inner = Some(OpenDecoder {
            decoder,
            parser,
            scaler: None,
            source: (0, 0, ffmpeg_next::format::Pixel::None),
        });
        Ok(())
    }

    fn feed(&mut self, input: &[u8], frames: &mut Vec<RgbFrame>) -> Result<usize, String> {
        let Some(open) = self.inner.as_mut() else {
            return Err("decoder is not open".into());
        };
        let mut data = std::ptr::null_mut();
        let mut size = 0_i32;
        let consumed = unsafe {
            ffmpeg_next::ffi::av_parser_parse2(
                open.parser,
                open.decoder.as_mut_ptr(),
                &mut data,
                &mut size,
                input.as_ptr(),
                i32::try_from(input.len()).unwrap_or(i32::MAX),
                ffmpeg_next::ffi::AV_NOPTS_VALUE,
                ffmpeg_next::ffi::AV_NOPTS_VALUE,
                0,
            )
        };
        if consumed < 0 {
            return Err("annex-b parser failed".into());
        }
        if size > 0 && !data.is_null() {
            let slice = unsafe { std::slice::from_raw_parts(data, size as usize) };
            let packet = ffmpeg_next::packet::Packet::copy(slice);
            if open.decoder.send_packet(&packet).is_ok() {
                let mut frame = ffmpeg_next::frame::Video::empty();
                while open.decoder.receive_frame(&mut frame).is_ok() {
                    if let Some(rgb) = open.painted(&frame) {
                        frames.push(rgb);
                    }
                }
            }
        }
        Ok(usize::try_from(consumed).unwrap_or(0))
    }
}

impl OpenDecoder {
    fn painted(&mut self, frame: &ffmpeg_next::frame::Video) -> Option<RgbFrame> {
        let (width, height) = fit(frame.width(), frame.height());
        let source = (frame.width(), frame.height(), frame.format());
        if self.scaler.is_none() || self.source != source {
            self.scaler = ffmpeg_next::software::scaling::context::Context::get(
                frame.format(),
                frame.width(),
                frame.height(),
                ffmpeg_next::format::Pixel::RGB24,
                width,
                height,
                ffmpeg_next::software::scaling::flag::Flags::BILINEAR,
            )
            .ok();
            self.source = source;
        }
        let scaler = self.scaler.as_mut()?;
        let mut rgb = ffmpeg_next::frame::Video::empty();
        scaler.run(frame, &mut rgb).ok()?;
        let stride = rgb.stride(0);
        let row_bytes = rgb.width() as usize * 3;
        let data = rgb.data(0);
        let mut pixels = Vec::with_capacity(rgb.width() as usize * rgb.height() as usize);
        for y in 0..rgb.height() as usize {
            let start = y * stride;
            let end = start + row_bytes;
            if end > data.len() {
                return None;
            }
            for [r, g, b] in data[start..end].as_chunks::<3>().0 {
                pixels.push(Rgb8Pixel {
                    r: *r,
                    g: *g,
                    b: *b,
                });
            }
        }
        Some(RgbFrame {
            width: rgb.width(),
            height: rgb.height(),
            pixels,
        })
    }
}

fn fit(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (2, 2);
    }
    let scale = (LIVE_MAX_W as f32 / width as f32)
        .min(LIVE_MAX_H as f32 / height as f32)
        .min(1.0);
    let fitted_w = ((width as f32 * scale).round() as u32).max(2) & !1;
    let fitted_h = ((height as f32 * scale).round() as u32).max(2) & !1;
    (fitted_w, fitted_h)
}

#[cfg(test)]
mod tests {
    use super::{Codec, authorization_value, join_url, project, sniff};
    use lcars_api_proto::v1::{Camera, ListCamerasResponse};

    #[test]
    fn project_drops_devices_without_a_camera() {
        let response = ListCamerasResponse {
            cameras: vec![
                Camera {
                    sn: "HUB1".into(),
                    name: "Base".into(),
                    has_camera: false,
                    stream_path: "/cameras/HUB1/stream".into(),
                    snapshot_path: "/cameras/HUB1/snapshot".into(),
                },
                Camera {
                    sn: "CAM1".into(),
                    name: "Front door".into(),
                    has_camera: true,
                    stream_path: "/cameras/CAM1/stream".into(),
                    snapshot_path: "/cameras/CAM1/snapshot".into(),
                },
            ],
        };
        let listed = project(&response);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].sn, "CAM1");
        assert_eq!(listed[0].name, "Front door");
        assert_eq!(listed[0].stream_path, "/cameras/CAM1/stream");
    }

    #[test]
    fn joins_the_server_origin_and_the_returned_path() {
        assert_eq!(
            join_url("http://192.0.2.10:8080", "/cameras/CAM1/stream").unwrap(),
            "http://192.0.2.10:8080/cameras/CAM1/stream"
        );
        assert_eq!(
            join_url("http://192.0.2.10:8080/", "/cameras/CAM1/snapshot").unwrap(),
            "http://192.0.2.10:8080/cameras/CAM1/snapshot"
        );
        assert!(join_url("http://192.0.2.10:8080", "http://192.0.2.20/x").is_err());
        assert!(join_url("http://192.0.2.10:8080", "//192.0.2.20/x").is_err());
    }

    #[test]
    fn authorization_header_is_basic() {
        assert_eq!(
            authorization_value("device", "secret"),
            "Basic ZGV2aWNlOnNlY3JldA=="
        );
    }

    #[test]
    fn sniff_reads_the_parameter_set() {
        assert_eq!(sniff(&[0, 0, 0, 1, 0x67, 0x42]), Some(Codec::H264));
        assert_eq!(sniff(&[0, 0, 1, 0x40, 0x01]), Some(Codec::H265));
        assert_eq!(sniff(b"FRAMEBYTES"), None);
    }

    #[tokio::test]
    async fn http_still_uses_basic_auth_and_closing_stops_the_stream() {
        let gate = serve(None).await;
        let media = super::Media::new(super::HttpConfig {
            base: gate.base.clone(),
            user: Some("device".into()),
            secret: Some("secret".into()),
        })
        .unwrap();
        let still = media
            .snapshot("/cameras/CAM1/snapshot")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(still, jpeg_bytes());
        assert_eq!(
            gate.authorization.lock().unwrap().as_deref(),
            Some(authorization_value("device", "secret")).as_deref()
        );
        let response = media.open_stream("/cameras/CAM1/stream").await.unwrap();
        assert!(response.status().is_success());
        drop(response);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gate.closed.load(std::sync::atomic::Ordering::SeqCst) {
            if std::time::Instant::now() > deadline {
                panic!("closing the stream did not disconnect");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn security_page_opens_and_closes_a_stream() {
        use lcars_ui::slint::ComponentHandle;
        use tracing_subscriber::layer::SubscriberExt;
        let logs = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let subscriber =
            tracing_subscriber::registry().with(LogCapture(std::sync::Arc::clone(&logs)));
        let _ = tracing::subscriber::set_global_default(subscriber);

        i_slint_backend_testing::init_integration_test_with_system_time();
        let app = lcars_ui::AppWindow::new().expect("headless window");
        let weak = app.as_weak();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        lcars_ui::slint::spawn_local(async move {
            let _ = ready_tx.send(());
        })
        .expect("schedule the ready signal");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            ready_rx.recv().expect("event loop");
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(panel_scenarios(weak))
            }));
            let _ = lcars_ui::slint::invoke_from_event_loop(|| {
                let _ = lcars_ui::slint::quit_event_loop();
            });
            let _ = done_tx.send(result);
        });
        app.run().expect("event loop");
        match done_rx.recv().expect("scenario result") {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("{error}"),
            Err(_) => panic!("camera session panicked"),
        }
        let text = logs.lock().expect("logs").clone();
        assert!(!text.contains("Front door"), "{text}");
        assert!(!text.contains("FRAMEBYTES"), "{text}");
        assert!(!text.contains("panel-secret"), "{text}");
    }

    struct LogCapture(std::sync::Arc<std::sync::Mutex<String>>);

    impl<S> tracing_subscriber::Layer<S> for LogCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if !event.metadata().target().starts_with("lcars_app") {
                return;
            }
            let mut visit = Visit(String::new());
            event.record(&mut visit);
            let mut buf = self.0.lock().expect("logs");
            buf.push_str(&visit.0);
            buf.push('\n');
        }
    }

    struct Visit(String);

    impl tracing::field::Visit for Visit {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.push_str(field.name());
            self.0.push('=');
            self.0.push_str(&format!("{value:?}"));
            self.0.push(' ');
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push_str(field.name());
            self.0.push('=');
            self.0.push_str(value);
            self.0.push(' ');
        }
    }

    struct Gate {
        base: String,
        authorization: std::sync::Mutex<Option<String>>,
        streams: std::sync::atomic::AtomicU32,
        closed: std::sync::atomic::AtomicBool,
        expect: Option<String>,
    }

    async fn serve(expect: Option<String>) -> std::sync::Arc<Gate> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let gate = std::sync::Arc::new(Gate {
            base: format!("http://127.0.0.1:{port}"),
            authorization: std::sync::Mutex::new(None),
            streams: std::sync::atomic::AtomicU32::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            expect,
        });
        let accepted = std::sync::Arc::clone(&gate);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let gate = std::sync::Arc::clone(&accepted);
                tokio::spawn(handle_camera(socket, gate));
            }
        });
        gate
    }

    async fn handle_camera(mut socket: tokio::net::TcpStream, gate: std::sync::Arc<Gate>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::new();
        let mut byte = [0; 1];
        loop {
            if socket.read(&mut byte).await.unwrap_or(0) == 0 || buf.len() > 8192 {
                return;
            }
            buf.push(byte[0]);
            if buf.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buf);
        let path = request.split_whitespace().nth(1).unwrap_or("");
        let authorization = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        });
        *gate.authorization.lock().expect("auth") = authorization.clone();
        if gate
            .expect
            .as_ref()
            .is_some_and(|expected| authorization.as_deref() != Some(expected))
        {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            return;
        }
        if path.ends_with("/snapshot") {
            let jpeg = jpeg_bytes();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                jpeg.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(&jpeg).await;
            return;
        }
        if path.ends_with("/stream") {
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\nA\r\nFRAMEBYTES\r\n").await;
            let _ = socket.flush().await;
            gate.streams
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut tmp = [0; 32];
            loop {
                match socket.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            gate.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn jpeg_bytes() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30]));
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut bytes);
        encoder
            .encode(image.as_raw(), 2, 2, image::ExtendedColorType::Rgb8)
            .expect("jpeg");
        bytes
    }

    async fn panel_scenarios(
        weak: lcars_ui::slint::Weak<lcars_ui::AppWindow>,
    ) -> Result<(), String> {
        let nats = start_nats()?;
        let _nats = KillChild(nats.0);
        let client = wait_for_nats(nats.1).await?;
        drive(&weak, client.clone(), None).await?;
        drive(&weak, client, Some(("device", "panel-secret"))).await?;
        Ok(())
    }

    async fn drive(
        weak: &lcars_ui::slint::Weak<lcars_ui::AppWindow>,
        client: async_nats::Client,
        token: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let expect = token.map(|(user, secret)| authorization_value(user, secret));
        let gate = serve(expect).await;
        on_ui(weak, |app| {
            app.set_cameras(lcars_ui::slint::ModelRc::new(lcars_ui::slint::VecModel::<
                lcars_ui::CameraCard,
            >::from(
                Vec::new()
            )));
            app.set_camera_status("LINKING".into());
            app.set_live_open(false);
        })?;
        let (commands_tx, commands_rx) = tokio::sync::mpsc::unbounded_channel();
        let weak_for_attach = weak.clone();
        let commands_for_attach = commands_tx.clone();
        on_ui(&weak_for_attach, move |app| {
            super::attach(app, commands_for_attach)
        })?;
        let mut subscription = client
            .subscribe("lcars.cameras.list")
            .await
            .map_err(|error| error.to_string())?;
        let publisher = client.clone();
        let lists = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let listed = std::sync::Arc::clone(&lists);
        let responder = tokio::spawn(async move {
            use futures::StreamExt;
            use lcars_api_proto::prost::Message;
            use lcars_api_proto::v1::{Camera, ListCamerasResponse};
            while let Some(message) = subscription.next().await {
                let Some(reply) = message.reply else { continue };
                let n = listed.load(std::sync::atomic::Ordering::SeqCst) + 1;
                let body = ListCamerasResponse {
                    cameras: vec![Camera {
                        sn: "CAM1".into(),
                        name: if n == 1 {
                            "Front door".into()
                        } else {
                            "Garage".into()
                        },
                        has_camera: true,
                        stream_path: "/cameras/CAM1/stream".into(),
                        snapshot_path: "/cameras/CAM1/snapshot".into(),
                    }],
                };
                let _ = publisher.publish(reply, body.encode_to_vec().into()).await;
                listed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let (client_tx, client_rx) = tokio::sync::watch::channel(None);
        let session = tokio::spawn(super::run(
            client_rx,
            Some(super::HttpConfig {
                base: gate.base.clone(),
                user: token.map(|(user, _)| user.to_string()),
                secret: token.map(|(_, secret)| secret.to_string()),
            }),
            commands_rx,
            weak.clone(),
        ));
        client_tx
            .send(Some(client.clone()))
            .map_err(|error| error.to_string())?;
        wait_card(weak, "CAM1", true).await?;
        on_ui(weak, |app| app.invoke_open_camera("CAM1".into()))?;
        wait_stream(&gate).await?;
        wait_status(weak, "LIVE").await?;
        let seen = lists.load(std::sync::atomic::Ordering::SeqCst);
        client_tx
            .send(Some(client.clone()))
            .map_err(|error| error.to_string())?;
        wait_lists(&lists, seen).await?;
        wait_card_name(weak, "Garage").await?;
        let status = camera_status(weak)?;
        if status != "LIVE" {
            return Err(format!("catalog refresh replaced LIVE with {status}"));
        }
        on_ui(weak, |app| app.invoke_close_live())?;
        wait_closed(&gate).await?;
        wait_status(weak, "1 CAMERA").await?;
        if let Some(expected) = gate.expect.as_deref() {
            let got = gate.authorization.lock().expect("auth").clone();
            if got.as_deref() != Some(expected) {
                return Err(format!("authorization {got:?}"));
            }
        }
        session.abort();
        responder.abort();
        Ok(())
    }

    fn start_nats() -> Result<(std::process::Child, u16), String> {
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        drop(listener);
        let child = std::process::Command::new("nats-server")
            .args(["-a", "127.0.0.1", "-p", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| format!("nats-server is required for the camera e2e test: {error}"))?;
        Ok((child, port))
    }

    async fn wait_for_nats(port: u16) -> Result<async_nats::Client, String> {
        let url = format!("nats://127.0.0.1:{port}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(client) = async_nats::connect(&url).await {
                return Ok(client);
            }
            if std::time::Instant::now() > deadline {
                return Err("nats-server did not accept a connection".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn on_ui(
        weak: &lcars_ui::slint::Weak<lcars_ui::AppWindow>,
        update: impl FnOnce(&lcars_ui::AppWindow) + Send + 'static,
    ) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let weak = weak.clone();
        lcars_ui::slint::invoke_from_event_loop(move || {
            let result = weak.upgrade().map(|app| update(&app));
            let _ = tx.send(result.is_some());
        })
        .map_err(|error| error.to_string())?;
        if rx.recv().map_err(|error| error.to_string())? {
            Ok(())
        } else {
            Err("window closed".into())
        }
    }

    async fn wait_card(
        weak: &lcars_ui::slint::Weak<lcars_ui::AppWindow>,
        sn: &str,
        still: bool,
    ) -> Result<(), String> {
        let serial = sn.to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let weak = weak.clone();
            let serial = serial.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            lcars_ui::slint::invoke_from_event_loop(move || {
                use lcars_ui::slint::Model;
                let found = weak.upgrade().and_then(|app| {
                    let cards = app.get_cameras();
                    (0..cards.row_count()).find_map(|index| {
                        let card = cards.row_data(index)?;
                        (card.sn.as_str() == serial && (!still || card.has_still)).then_some(())
                    })
                });
                let _ = tx.send(found.is_some());
            })
            .map_err(|error| error.to_string())?;
            if rx.recv().unwrap_or(false) {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(format!("camera {sn} did not appear"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn wait_card_name(
        weak: &lcars_ui::slint::Weak<lcars_ui::AppWindow>,
        name: &str,
    ) -> Result<(), String> {
        let want = name.to_string();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let weak = weak.clone();
            let want = want.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            lcars_ui::slint::invoke_from_event_loop(move || {
                use lcars_ui::slint::Model;
                let found = weak.upgrade().is_some_and(|app| {
                    let cards = app.get_cameras();
                    (0..cards.row_count()).any(|index| {
                        cards
                            .row_data(index)
                            .is_some_and(|card| card.name.as_str() == want)
                    })
                });
                let _ = tx.send(found);
            })
            .map_err(|error| error.to_string())?;
            if rx.recv().unwrap_or(false) {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(format!("camera card {name} did not appear"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    fn camera_status(weak: &lcars_ui::slint::Weak<lcars_ui::AppWindow>) -> Result<String, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let weak = weak.clone();
        lcars_ui::slint::invoke_from_event_loop(move || {
            let _ = tx.send(
                weak.upgrade()
                    .map(|app| app.get_camera_status().to_string()),
            );
        })
        .map_err(|error| error.to_string())?;
        rx.recv()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "window closed".into())
    }

    async fn wait_status(
        weak: &lcars_ui::slint::Weak<lcars_ui::AppWindow>,
        want: &str,
    ) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if camera_status(weak)? == want {
                return Ok(());
            }
            if std::time::Instant::now() > deadline {
                return Err(format!("camera status did not become {want}"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn wait_lists(lists: &std::sync::atomic::AtomicU32, seen: u32) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while lists.load(std::sync::atomic::Ordering::SeqCst) <= seen {
            if std::time::Instant::now() > deadline {
                return Err("catalog did not refresh during the live view".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn wait_stream(gate: &Gate) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while gate.streams.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            if std::time::Instant::now() > deadline {
                return Err("stream was not opened".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok(())
    }

    async fn wait_closed(gate: &Gate) -> Result<(), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gate.closed.load(std::sync::atomic::Ordering::SeqCst) {
            if std::time::Instant::now() > deadline {
                return Err("closing the stream did not disconnect".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok(())
    }

    struct KillChild(std::process::Child);

    impl Drop for KillChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
