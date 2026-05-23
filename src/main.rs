use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, OnceLock,
    mpsc::{self, Receiver, Sender},
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use chrono_tz::America::Regina;
use clap::{Parser, ValueEnum};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::sliced::{SignedPosition, SlicedImage, SlicedProtocol};
use regex::Regex;
use reqwest::Url;
use reqwest::blocking::Client;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};

const BASE_URL: &str = "https://www.mcdougallauction.com/";
const EVENT_LIST_URL: &str = "https://www.mcdougallauction.com/auction-event-list.php";
const PAGE_SIZE: usize = 14;
const MAX_PHOTO_PREVIEW_WIDTH: u32 = 1024;
const MAX_PHOTO_PREVIEW_HEIGHT: u32 = 1024;
const DETAILS_IMAGE_GAP: u16 = 1;
const DETAILS_IMAGE_MIN_WIDTH: u16 = 12;
const DETAILS_TEXT_MIN_WIDTH: u16 = 24;
const DETAILS_IMAGE_TARGET_WIDTH: u16 = 42;
const DETAILS_IMAGE_MAX_WIDTH: u16 = 56;
const PANE_RESIZE_DEBOUNCE: Duration = Duration::from_millis(180);
const DETAIL_WORKER_COUNT: usize = 3;
const PHOTO_WORKER_COUNT: usize = 8;
const DETAIL_PREFETCH_IN_FLIGHT_LIMIT: usize = DETAIL_WORKER_COUNT;
const PHOTO_PREFETCH_IN_FLIGHT_LIMIT: usize = PHOTO_WORKER_COUNT;
const TERMINAL_PHOTO_WORKER_COUNT: usize = 2;
const TERMINAL_PHOTO_IN_FLIGHT_LIMIT: usize = TERMINAL_PHOTO_WORKER_COUNT;
const PHOTO_CACHE_MAGIC: &[u8] = b"MCDOUGS_PHOTO_PREVIEW_V1\n";
const DETAIL_RESULTS_PER_FRAME: usize = 24;
const PHOTO_RESULTS_PER_FRAME: usize = 12;
const GLOBAL_BACKGROUND_HYDRATION_INTERVAL: Duration = Duration::from_secs(30);
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(8);
const MAX_INPUT_EVENTS_PER_FRAME: usize = 32;
const TERMINAL_PHOTO_STATE_CACHE_LIMIT: usize = 640;
const PHOTO_RENDER_BUFFER_CACHE_LIMIT: usize = 640;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "mcdougs",
    about = "Browse live McDougall auction lots in a Rust CLI and TUI."
)]
struct Cli {
    #[arg(
        long,
        default_value = "Saskatoon",
        help = "Only include locations containing this text."
    )]
    city: String,
    #[arg(long, help = "Only include lots whose item name contains this text.")]
    query: Option<String>,
    #[arg(long, value_enum, default_value_t = SortKey::Close, help = "Sort lots inside each auction.")]
    sort: SortKey,
    #[arg(long, help = "Include lots whose close time has already passed.")]
    include_past: bool,
    #[arg(long, help = "Print a plain report instead of launching the TUI.")]
    plain: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SortKey {
    Close,
    Price,
    Name,
}

impl std::fmt::Display for SortKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Close => formatter.write_str("close"),
            Self::Price => formatter.write_str("price"),
            Self::Name => formatter.write_str("name"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Events,
    Lots,
    Details,
}

impl std::fmt::Display for FocusPane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Events => formatter.write_str("events"),
            Self::Lots => formatter.write_str("lots"),
            Self::Details => formatter.write_str("details"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragTarget {
    EventsWidth,
    LotsDetailsSplit,
}

#[derive(Debug, Clone)]
struct EventStub {
    title: String,
    url: String,
    location: String,
    close_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone)]
struct Lot {
    name: String,
    url: String,
    thumbnail_url: Option<String>,
    location: String,
    bid_display: String,
    bid_cents: Option<i64>,
    close_at: Option<DateTime<FixedOffset>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LotDetails {
    photos: Vec<String>,
    info_lines: Vec<String>,
    documents: Vec<DetailLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DetailLink {
    label: String,
    url: String,
}

#[derive(Debug, Clone)]
enum DetailState {
    Loading,
    Loaded(LotDetails),
    Failed(String),
}

#[derive(Debug, Clone)]
struct DetailFetchRequest {
    lot_url: String,
    expires_at: Option<i64>,
}

struct DetailFetchResult {
    lot_url: String,
    result: std::result::Result<LotDetails, String>,
}

#[derive(Debug, Clone)]
struct PhotoPreview {
    width: u32,
    height: u32,
    image: image::RgbImage,
}

#[derive(Debug, Clone)]
enum PhotoState {
    Loading,
    Loaded(PhotoPreview),
    Failed(String),
}

#[derive(Debug, Clone)]
struct PhotoFetchRequest {
    url: String,
    expires_at: Option<i64>,
}

struct PhotoFetchResult {
    url: String,
    result: std::result::Result<PhotoPreview, String>,
}

enum TerminalPhotoState {
    Loaded(SlicedProtocol),
    Failed,
}

type TerminalPhotoKey = (String, u16, u16);
type PhotoRenderBufferKey = (String, u16, u16, bool);

struct TerminalPhotoRenderRequest {
    key: TerminalPhotoKey,
    picker: Picker,
    preview: PhotoPreview,
}

struct TerminalPhotoRenderResult {
    key: TerminalPhotoKey,
    state: TerminalPhotoState,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedLotDetails {
    lot_url: String,
    expires_at: Option<i64>,
    details: LotDetails,
}

#[derive(Debug, Clone)]
struct CacheStore {
    root: PathBuf,
}

impl CacheStore {
    fn new() -> Self {
        Self { root: cache_root() }
    }

    #[cfg(test)]
    fn from_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn load_lot_details(&self, lot_url: &str) -> Result<Option<LotDetails>> {
        let path = self.detail_path(lot_url);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to read cached lot details"),
        };

        let cached = match serde_json::from_slice::<CachedLotDetails>(&bytes) {
            Ok(cached) => cached,
            Err(error) => {
                let _ = fs::remove_file(&path);
                return Err(error).context("failed to parse cached lot details");
            }
        };

        if cached.lot_url != lot_url || cache_entry_expired(cached.expires_at) {
            let _ = fs::remove_file(path);
            return Ok(None);
        }

        let original_info_lines = cached.details.info_lines.clone();
        let details = clean_lot_details(cached.details);

        if details.info_lines != original_info_lines {
            let sanitized = CachedLotDetails {
                lot_url: cached.lot_url,
                expires_at: cached.expires_at,
                details: details.clone(),
            };
            if let Ok(bytes) = serde_json::to_vec(&sanitized) {
                let _ = fs::write(path, bytes);
            }
        }

        Ok(Some(details))
    }

    fn save_lot_details(
        &self,
        lot_url: &str,
        expires_at: Option<i64>,
        details: &LotDetails,
    ) -> Result<()> {
        let path = self.detail_path(lot_url);
        create_parent_dir(&path)?;
        let cached = CachedLotDetails {
            lot_url: lot_url.to_string(),
            expires_at,
            details: clean_lot_details(details.clone()),
        };
        let bytes = serde_json::to_vec(&cached).context("failed to serialize lot details cache")?;
        fs::write(path, bytes).context("failed to write lot details cache")
    }

    fn load_photo_preview(&self, photo_url: &str) -> Result<Option<PhotoPreview>> {
        let path = self.photo_path(photo_url);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to read cached photo preview"),
        };

        match decode_cached_photo_preview(&bytes) {
            Ok(Some(preview)) => Ok(Some(preview)),
            Ok(None) => {
                let _ = fs::remove_file(path);
                Ok(None)
            }
            Err(error) => {
                let _ = fs::remove_file(&path);
                Err(error).context("failed to parse cached photo preview")
            }
        }
    }

    fn save_photo_preview(
        &self,
        photo_url: &str,
        expires_at: Option<i64>,
        preview: &PhotoPreview,
    ) -> Result<()> {
        let path = self.photo_path(photo_url);
        create_parent_dir(&path)?;
        let mut bytes = format!(
            "{}{} {} {}\n",
            std::str::from_utf8(PHOTO_CACHE_MAGIC).expect("photo cache magic is utf-8"),
            preview.width,
            preview.height,
            expires_at.unwrap_or(0)
        )
        .into_bytes();
        bytes.extend_from_slice(preview.image.as_raw());
        fs::write(path, bytes).context("failed to write photo preview cache")
    }

    fn prune_expired(&self) -> bool {
        let mut changed = false;
        changed |= prune_expired_detail_files(&self.root.join("details"));
        changed |= prune_expired_photo_files(&self.root.join("photos"));
        changed
    }

    fn detail_path(&self, lot_url: &str) -> PathBuf {
        self.root
            .join("details")
            .join(format!("{}.json", cache_key(lot_url)))
    }

    fn photo_path(&self, photo_url: &str) -> PathBuf {
        self.root
            .join("photos")
            .join(format!("{}.rgb", cache_key(photo_url)))
    }
}

fn cache_root() -> PathBuf {
    if let Some(path) = std::env::var_os("MCDOUGS_CACHE_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data).join("mcdougs").join("cache");
    }

    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(".cache").join("mcdougs");
    }

    std::env::temp_dir().join("mcdougs-cache")
}

fn cache_key(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;

    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }

    format!("{hash:016x}")
}

fn create_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("failed to create cache directory")?;
    }

    Ok(())
}

fn cache_entry_expired(expires_at: Option<i64>) -> bool {
    expires_at
        .map(|timestamp| timestamp > 0 && timestamp <= Utc::now().timestamp())
        .unwrap_or(false)
}

fn decode_cached_photo_preview(bytes: &[u8]) -> Result<Option<PhotoPreview>> {
    if !bytes.starts_with(PHOTO_CACHE_MAGIC) {
        return Ok(None);
    }

    let header_start = PHOTO_CACHE_MAGIC.len();
    let Some(header_end_offset) = bytes[header_start..].iter().position(|byte| *byte == b'\n')
    else {
        return Ok(None);
    };
    let header_end = header_start + header_end_offset;
    let header = std::str::from_utf8(&bytes[header_start..header_end])
        .context("photo cache header is not utf-8")?;
    let parts = header.split_whitespace().collect::<Vec<_>>();

    if parts.len() != 3 {
        return Ok(None);
    }

    let width = parts[0]
        .parse::<u32>()
        .context("invalid cached photo width")?;
    let height = parts[1]
        .parse::<u32>()
        .context("invalid cached photo height")?;
    let expires_at = parts[2]
        .parse::<i64>()
        .context("invalid cached photo expiry")?;

    if width == 0 || height == 0 || cache_entry_expired((expires_at > 0).then_some(expires_at)) {
        return Ok(None);
    }

    let pixel_bytes = bytes[header_end + 1..].to_vec();
    let expected_len = width as usize * height as usize * 3;
    if pixel_bytes.len() != expected_len {
        return Ok(None);
    }

    let image = image::RgbImage::from_raw(width, height, pixel_bytes)
        .ok_or_else(|| anyhow!("cached photo pixel buffer has invalid dimensions"))?;

    Ok(Some(PhotoPreview {
        width,
        height,
        image,
    }))
}

fn prune_expired_detail_files(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    let mut changed = false;

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let remove = serde_json::from_slice::<CachedLotDetails>(&bytes)
            .map(|cached| cache_entry_expired(cached.expires_at))
            .unwrap_or(true);

        if remove && fs::remove_file(path).is_ok() {
            changed = true;
        }
    }

    changed
}

fn prune_expired_photo_files(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };

    let mut changed = false;

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let remove = matches!(decode_cached_photo_preview(&bytes), Ok(None) | Err(_));

        if remove && fs::remove_file(path).is_ok() {
            changed = true;
        }
    }

    changed
}

#[derive(Debug, Clone)]
struct AuctionEvent {
    title: String,
    url: String,
    location: String,
    close_at: Option<DateTime<FixedOffset>>,
    lots: Vec<Lot>,
}

impl AuctionEvent {
    fn primary_close_at(&self) -> Option<&DateTime<FixedOffset>> {
        self.lots
            .iter()
            .filter_map(|lot| lot.close_at.as_ref())
            .min()
            .or(self.close_at.as_ref())
    }
}

struct AuctionScraper {
    client: Client,
}

impl AuctionScraper {
    fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (compatible; mcdougs-rust/1.0)")
            .timeout(Duration::from_secs(20))
            .default_headers(
                [(
                    reqwest::header::ACCEPT_LANGUAGE,
                    "en-CA,en;q=0.9".parse().expect("valid header"),
                )]
                .into_iter()
                .collect(),
            )
            .build()
            .context("failed to build HTTP client")?;

        Ok(Self { client })
    }

    fn fetch_all_events(&self) -> Result<Vec<EventStub>> {
        let html = self.fetch_text(EVENT_LIST_URL)?;
        let mut events = parse_events(&html)?;
        let mut seen = events
            .iter()
            .map(|event| event.url.clone())
            .collect::<HashSet<_>>();

        if let Some(mut next_item) = parse_next_item(&html)? {
            loop {
                let mut url = Url::parse(&format!("{BASE_URL}searchmore.php"))?;
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("type", "event");
                    query.append_pair("nextitem", &next_item.to_string());
                }

                let fragment = self.fetch_text(url.as_str())?;
                let trimmed = fragment.trim();

                if trimmed.is_empty() {
                    break;
                }

                let parsed = parse_events_fragment(trimmed)?;

                if parsed.is_empty() {
                    break;
                }

                for event in parsed {
                    if seen.insert(event.url.clone()) {
                        events.push(event);
                    }
                }

                next_item += PAGE_SIZE;
            }
        }

        Ok(events)
    }

    fn fetch_all_lots(&self, event: &EventStub) -> Result<Vec<Lot>> {
        let html = self.fetch_text(&event.url)?;
        let mut lots = parse_lots(&html)?;
        let mut seen = lots
            .iter()
            .map(|lot| lot.url.clone())
            .collect::<HashSet<_>>();

        if let Some(mut next_item) = parse_next_item(&html)? {
            let event_url = Url::parse(&event.url)?;
            let extra_query = event_url
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();

            loop {
                let mut url = Url::parse(&format!("{BASE_URL}searchmore.php"))?;
                {
                    let mut query = url.query_pairs_mut();
                    query.append_pair("type", "eventproducts");
                    query.append_pair("nextitem", &next_item.to_string());

                    for (key, value) in &extra_query {
                        query.append_pair(key, value);
                    }
                }

                let fragment = self.fetch_text(url.as_str())?;
                let trimmed = fragment.trim();

                if trimmed.is_empty() || trimmed == "No More Items" {
                    break;
                }

                let parsed = parse_lots_fragment(trimmed)?;

                if parsed.is_empty() {
                    break;
                }

                for lot in parsed {
                    if seen.insert(lot.url.clone()) {
                        lots.push(lot);
                    }
                }

                next_item += PAGE_SIZE;
            }
        }

        Ok(lots)
    }

    fn fetch_lot_details(&self, lot_url: &str) -> Result<LotDetails> {
        let html = self.fetch_text(lot_url)?;
        parse_lot_details(&html)
    }

    fn fetch_photo_preview(&self, photo_url: &str) -> Result<PhotoPreview> {
        let bytes = self.fetch_bytes(photo_url)?;
        decode_photo_preview(&bytes)
    }

    fn fetch_text(&self, url: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("request failed for {url}"))?;

        let status = response.status();

        if !status.is_success() {
            return Err(anyhow!("request failed for {url}: {status}"));
        }

        response
            .text()
            .with_context(|| format!("failed to read response body for {url}"))
    }

    fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let response = self
            .client
            .get(url)
            .send()
            .with_context(|| format!("request failed for {url}"))?;

        let status = response.status();

        if !status.is_success() {
            return Err(anyhow!("request failed for {url}: {status}"));
        }

        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .with_context(|| format!("failed to read response body for {url}"))
    }
}

fn spawn_detail_worker() -> (Sender<DetailFetchRequest>, Receiver<DetailFetchResult>) {
    let (request_tx, request_rx) = mpsc::channel::<DetailFetchRequest>();
    let (result_tx, result_rx) = mpsc::channel::<DetailFetchResult>();
    let request_rx = Arc::new(Mutex::new(request_rx));
    let cache = CacheStore::new();

    for _ in 0..DETAIL_WORKER_COUNT {
        let request_rx = Arc::clone(&request_rx);
        let result_tx = result_tx.clone();
        let cache = cache.clone();

        thread::spawn(move || {
            let scraper = match AuctionScraper::new() {
                Ok(scraper) => scraper,
                Err(error) => {
                    loop {
                        let request = match request_rx.lock().expect("detail worker mutex").recv() {
                            Ok(request) => request,
                            Err(_) => break,
                        };

                        let _ = result_tx.send(DetailFetchResult {
                            lot_url: request.lot_url,
                            result: Err(format!("failed to initialize detail scraper: {error}")),
                        });
                    }
                    return;
                }
            };

            loop {
                let request = match request_rx.lock().expect("detail worker mutex").recv() {
                    Ok(request) => request,
                    Err(_) => break,
                };

                let result = match cache.load_lot_details(&request.lot_url) {
                    Ok(Some(details)) => Ok(details),
                    Ok(None) | Err(_) => scraper
                        .fetch_lot_details(&request.lot_url)
                        .inspect(|details| {
                            let _ = cache.save_lot_details(
                                &request.lot_url,
                                request.expires_at,
                                details,
                            );
                        })
                        .map_err(|error| error.to_string()),
                };

                if result_tx
                    .send(DetailFetchResult {
                        lot_url: request.lot_url,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    (request_tx, result_rx)
}

fn spawn_photo_worker() -> (Sender<PhotoFetchRequest>, Receiver<PhotoFetchResult>) {
    let (request_tx, request_rx) = mpsc::channel::<PhotoFetchRequest>();
    let (result_tx, result_rx) = mpsc::channel::<PhotoFetchResult>();
    let request_rx = Arc::new(Mutex::new(request_rx));
    let cache = CacheStore::new();

    for _ in 0..PHOTO_WORKER_COUNT {
        let request_rx = Arc::clone(&request_rx);
        let result_tx = result_tx.clone();
        let cache = cache.clone();

        thread::spawn(move || {
            let scraper = match AuctionScraper::new() {
                Ok(scraper) => scraper,
                Err(error) => {
                    loop {
                        let request = match request_rx.lock().expect("photo worker mutex").recv() {
                            Ok(request) => request,
                            Err(_) => break,
                        };

                        let _ = result_tx.send(PhotoFetchResult {
                            url: request.url,
                            result: Err(format!("failed to initialize photo scraper: {error}")),
                        });
                    }
                    return;
                }
            };

            loop {
                let request = match request_rx.lock().expect("photo worker mutex").recv() {
                    Ok(request) => request,
                    Err(_) => break,
                };

                let result = match cache.load_photo_preview(&request.url) {
                    Ok(Some(preview)) => Ok(preview),
                    Ok(None) | Err(_) => scraper
                        .fetch_photo_preview(&request.url)
                        .inspect(|preview| {
                            let _ =
                                cache.save_photo_preview(&request.url, request.expires_at, preview);
                        })
                        .map_err(|error| error.to_string()),
                };

                if result_tx
                    .send(PhotoFetchResult {
                        url: request.url,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    (request_tx, result_rx)
}

fn spawn_cache_prune(cache: CacheStore) {
    thread::spawn(move || {
        let _ = cache.prune_expired();
    });
}

fn spawn_terminal_photo_worker() -> (
    Sender<TerminalPhotoRenderRequest>,
    Receiver<TerminalPhotoRenderResult>,
) {
    let (request_tx, request_rx) = mpsc::channel::<TerminalPhotoRenderRequest>();
    let (result_tx, result_rx) = mpsc::channel::<TerminalPhotoRenderResult>();
    let request_rx = Arc::new(Mutex::new(request_rx));

    for _ in 0..TERMINAL_PHOTO_WORKER_COUNT {
        let request_rx = Arc::clone(&request_rx);
        let result_tx = result_tx.clone();

        thread::spawn(move || {
            loop {
                let request = match request_rx
                    .lock()
                    .expect("terminal photo worker mutex")
                    .recv()
                {
                    Ok(request) => request,
                    Err(_) => break,
                };

                let state = terminal_photo_state(
                    &request.picker,
                    &request.preview,
                    request.key.1,
                    request.key.2,
                );

                if result_tx
                    .send(TerminalPhotoRenderResult {
                        key: request.key,
                        state,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    (request_tx, result_rx)
}

struct App {
    cli: Cli,
    events: Vec<AuctionEvent>,
    focus: FocusPane,
    event_index: usize,
    lot_index: usize,
    event_offset: usize,
    lot_offset: usize,
    details_scroll: usize,
    drag_target: Option<DragTarget>,
    resize_hover_target: Option<DragTarget>,
    event_pane_width: u16,
    lots_pane_percent: u16,
    stacked_events_percent: u16,
    stacked_lots_percent: u16,
    observed_lot_url: Option<String>,
    detail_states: HashMap<String, DetailState>,
    detail_prefetch_queue: VecDeque<String>,
    queued_detail_urls: HashSet<String>,
    active_detail_urls: HashSet<String>,
    detail_requests_in_flight: usize,
    detail_request_tx: Sender<DetailFetchRequest>,
    detail_result_rx: Receiver<DetailFetchResult>,
    photo_states: HashMap<String, PhotoState>,
    photo_prefetch_queue: VecDeque<String>,
    queued_photo_urls: HashSet<String>,
    active_photo_urls: HashSet<String>,
    photo_requests_in_flight: usize,
    photo_request_tx: Sender<PhotoFetchRequest>,
    photo_result_rx: Receiver<PhotoFetchResult>,
    cache: CacheStore,
    terminal_image_picker: Option<Picker>,
    terminal_photo_states: HashMap<TerminalPhotoKey, TerminalPhotoState>,
    terminal_photo_lru: VecDeque<TerminalPhotoKey>,
    terminal_photo_queue: VecDeque<TerminalPhotoKey>,
    queued_terminal_photo_keys: HashSet<TerminalPhotoKey>,
    active_terminal_photo_keys: HashSet<TerminalPhotoKey>,
    terminal_photo_requests_in_flight: usize,
    terminal_photo_request_tx: Sender<TerminalPhotoRenderRequest>,
    terminal_photo_result_rx: Receiver<TerminalPhotoRenderResult>,
    photo_render_buffers: HashMap<PhotoRenderBufferKey, Buffer>,
    photo_render_lru: VecDeque<PhotoRenderBufferKey>,
    last_photo_area_size: Option<(u16, u16)>,
    last_pane_resize_at: Option<Instant>,
    pending_image_resize_redraw: bool,
    current_auction_hydration_dirty: bool,
    last_global_hydration_at: Option<Instant>,
    status: String,
    loaded_at: DateTime<Utc>,
}

impl App {
    fn new(cli: Cli, events: Vec<AuctionEvent>) -> Self {
        let (detail_request_tx, detail_result_rx) = spawn_detail_worker();
        let (photo_request_tx, photo_result_rx) = spawn_photo_worker();
        let (terminal_photo_request_tx, terminal_photo_result_rx) = spawn_terminal_photo_worker();
        let cache = CacheStore::new();
        spawn_cache_prune(cache.clone());
        let mut app = Self {
            cli,
            events,
            focus: FocusPane::Events,
            event_index: 0,
            lot_index: 0,
            event_offset: 0,
            lot_offset: 0,
            details_scroll: 0,
            drag_target: None,
            resize_hover_target: None,
            event_pane_width: 38,
            lots_pane_percent: 25,
            stacked_events_percent: 12,
            stacked_lots_percent: 13,
            observed_lot_url: None,
            detail_states: HashMap::new(),
            detail_prefetch_queue: VecDeque::new(),
            queued_detail_urls: HashSet::new(),
            active_detail_urls: HashSet::new(),
            detail_requests_in_flight: 0,
            detail_request_tx,
            detail_result_rx,
            photo_states: HashMap::new(),
            photo_prefetch_queue: VecDeque::new(),
            queued_photo_urls: HashSet::new(),
            active_photo_urls: HashSet::new(),
            photo_requests_in_flight: 0,
            photo_request_tx,
            photo_result_rx,
            cache,
            terminal_image_picker: None,
            terminal_photo_states: HashMap::new(),
            terminal_photo_lru: VecDeque::new(),
            terminal_photo_queue: VecDeque::new(),
            queued_terminal_photo_keys: HashSet::new(),
            active_terminal_photo_keys: HashSet::new(),
            terminal_photo_requests_in_flight: 0,
            terminal_photo_request_tx,
            terminal_photo_result_rx,
            photo_render_buffers: HashMap::new(),
            photo_render_lru: VecDeque::new(),
            last_photo_area_size: None,
            last_pane_resize_at: None,
            pending_image_resize_redraw: false,
            current_auction_hydration_dirty: true,
            last_global_hydration_at: None,
            status: String::new(),
            loaded_at: Utc::now(),
        };
        app.sync_status("Loaded live auction data.");
        app.clamp_selection();
        app
    }

    fn current_event(&self) -> Option<&AuctionEvent> {
        self.events.get(self.event_index)
    }

    fn current_lot(&self) -> Option<&Lot> {
        self.current_event()
            .and_then(|event| event.lots.get(self.lot_index))
    }

    fn current_lot_url(&self) -> Option<String> {
        self.current_lot().map(|lot| lot.url.clone())
    }

    fn current_detail_state(&self) -> Option<&DetailState> {
        self.current_lot()
            .and_then(|lot| self.detail_states.get(&lot.url))
    }

    fn current_loaded_details(&self) -> Option<&LotDetails> {
        match self.current_detail_state() {
            Some(DetailState::Loaded(details)) => Some(details),
            _ => None,
        }
    }

    fn terminal_photo_state_for_render(
        &mut self,
        photo_url: &str,
        width: u16,
        height: u16,
    ) -> Option<&TerminalPhotoState> {
        let key = (photo_url.to_string(), width, height);

        if self.terminal_photo_states.contains_key(&key) {
            self.touch_terminal_photo_key(&key);
        }

        self.terminal_photo_states.get(&key)
    }

    fn remember_terminal_photo_state(&mut self, key: TerminalPhotoKey) {
        self.touch_terminal_photo_key(&key);
        self.trim_terminal_photo_states();
    }

    fn touch_terminal_photo_key(&mut self, key: &TerminalPhotoKey) {
        self.terminal_photo_lru
            .retain(|cached_key| cached_key != key);
        self.terminal_photo_lru.push_back(key.clone());
    }

    fn terminal_photo_state_limit(&self) -> usize {
        let current_auction_lots = self
            .current_event()
            .map(|event| event.lots.len())
            .unwrap_or(0);

        TERMINAL_PHOTO_STATE_CACHE_LIMIT.max(current_auction_lots.saturating_mul(2))
    }

    fn trim_terminal_photo_states(&mut self) {
        let limit = self.terminal_photo_state_limit();

        while self.terminal_photo_states.len() > limit {
            let Some(key) = self.terminal_photo_lru.pop_front() else {
                break;
            };

            if self.terminal_photo_states.remove(&key).is_some() {
                let render_key = (key.0, key.1, key.2, true);
                self.remove_photo_render_buffer(&render_key);
            }
        }
    }

    fn touch_photo_render_buffer(&mut self, key: &PhotoRenderBufferKey) {
        self.photo_render_lru.retain(|cached_key| cached_key != key);
        self.photo_render_lru.push_back(key.clone());
    }

    fn insert_photo_render_buffer(&mut self, key: PhotoRenderBufferKey, buffer: Buffer) {
        self.photo_render_buffers.insert(key.clone(), buffer);
        self.touch_photo_render_buffer(&key);
        self.trim_photo_render_buffers();
    }

    fn remove_photo_render_buffer(&mut self, key: &PhotoRenderBufferKey) {
        self.photo_render_buffers.remove(key);
        self.photo_render_lru.retain(|cached_key| cached_key != key);
    }

    fn remove_photo_render_buffers_for_photo(&mut self, photo_url: &str) {
        self.photo_render_buffers
            .retain(|(cached_photo_url, _, _, _), _| cached_photo_url != photo_url);
        self.photo_render_lru
            .retain(|(cached_photo_url, _, _, _)| cached_photo_url != photo_url);
    }

    fn photo_render_buffer_limit(&self) -> usize {
        let current_auction_lots = self
            .current_event()
            .map(|event| event.lots.len())
            .unwrap_or(0);

        PHOTO_RENDER_BUFFER_CACHE_LIMIT.max(current_auction_lots.saturating_mul(2))
    }

    fn trim_photo_render_buffers(&mut self) {
        let limit = self.photo_render_buffer_limit();

        while self.photo_render_buffers.len() > limit {
            let Some(key) = self.photo_render_lru.pop_front() else {
                break;
            };

            self.photo_render_buffers.remove(&key);
        }
    }

    fn cached_photo_render_buffer(&self, key: &PhotoRenderBufferKey) -> Option<&Buffer> {
        self.photo_render_buffers.get(key)
    }

    fn total_lots(&self) -> usize {
        self.events.iter().map(|event| event.lots.len()).sum()
    }

    fn process_detail_results(&mut self) -> bool {
        let mut changed = false;
        let selected_lot_url = self.current_lot_url();

        for _ in 0..DETAIL_RESULTS_PER_FRAME {
            let Ok(result) = self.detail_result_rx.try_recv() else {
                break;
            };
            let affects_current = selected_lot_url.as_deref() == Some(result.lot_url.as_str());
            self.detail_requests_in_flight = self.detail_requests_in_flight.saturating_sub(1);
            self.active_detail_urls.remove(&result.lot_url);

            if !self.is_cacheable_lot_url(&result.lot_url, Utc::now()) {
                self.detail_states.remove(&result.lot_url);
                changed |= affects_current;
                continue;
            }

            match result.result {
                Ok(details) => {
                    let primary_photo_url = details.photos.first().cloned();
                    let priority =
                        self.current_lot_url().as_deref() == Some(result.lot_url.as_str());
                    self.detail_states
                        .insert(result.lot_url, DetailState::Loaded(details));

                    if let Some(photo_url) = primary_photo_url {
                        self.queue_photo_preview(photo_url, priority);
                    }
                }
                Err(error) => {
                    self.detail_states
                        .insert(result.lot_url, DetailState::Failed(error));
                }
            };
            changed |= affects_current;
        }

        changed
    }

    fn process_photo_results(&mut self) -> bool {
        let mut changed = false;
        let primary_photo_url = self.primary_photo_url().map(str::to_string);
        let photo_area_size = self.last_photo_area_size;

        for _ in 0..PHOTO_RESULTS_PER_FRAME {
            let Ok(result) = self.photo_result_rx.try_recv() else {
                break;
            };
            let affects_current = primary_photo_url.as_deref() == Some(result.url.as_str());
            self.photo_requests_in_flight = self.photo_requests_in_flight.saturating_sub(1);
            self.active_photo_urls.remove(&result.url);

            if !self.is_cacheable_photo_url(&result.url) {
                self.photo_states.remove(&result.url);
                changed |= affects_current;
                continue;
            }

            let photo_url = result.url;
            let loaded = result.result.is_ok();
            let state = match result.result {
                Ok(preview) => PhotoState::Loaded(preview),
                Err(error) => PhotoState::Failed(error),
            };

            self.terminal_photo_states
                .retain(|(cached_photo_url, _, _), _| cached_photo_url != &photo_url);
            self.terminal_photo_lru
                .retain(|(cached_photo_url, _, _)| cached_photo_url != &photo_url);
            self.remove_photo_render_buffers_for_photo(&photo_url);
            self.photo_states.insert(photo_url.clone(), state);
            changed |= affects_current;

            if let (true, Some((width, height))) = (loaded, photo_area_size) {
                self.queue_terminal_photo_render(photo_url, width, height, false);
            }
        }

        changed
    }

    fn process_terminal_photo_results(&mut self) -> bool {
        let mut changed = false;
        let primary_photo_url = self.primary_photo_url().map(str::to_string);

        while let Ok(result) = self.terminal_photo_result_rx.try_recv() {
            self.terminal_photo_requests_in_flight =
                self.terminal_photo_requests_in_flight.saturating_sub(1);
            self.active_terminal_photo_keys.remove(&result.key);
            let affects_current = primary_photo_url.as_deref() == Some(result.key.0.as_str());

            let render_key = (result.key.0.clone(), result.key.1, result.key.2, true);
            self.remove_photo_render_buffer(&render_key);
            self.terminal_photo_states
                .insert(result.key.clone(), result.state);
            self.remember_terminal_photo_state(result.key);
            if affects_current {
                changed = true;
            }
        }

        changed
    }

    fn hydrate_current_context(&mut self) -> bool {
        let mut changed = false;

        if let Some(lot_url) = self.current_lot_url() {
            changed |= self.queue_lot_details(lot_url, true);
        }

        if let Some(photo_url) = self.primary_photo_url().map(str::to_string) {
            changed |= self.queue_photo_preview(photo_url, true);
        }

        if self.current_auction_hydration_dirty {
            let current_lot_urls = self.current_auction_lot_urls_by_priority();
            let _ = self.queue_detail_batch_front(current_lot_urls);
            let current_photo_urls = self.current_auction_photo_urls_by_priority();
            let _ = self.queue_photo_batch_front(current_photo_urls);
            self.queue_terminal_photo_batch_front(self.current_auction_photo_urls_by_priority());
            self.current_auction_hydration_dirty = false;
        }

        if self
            .last_global_hydration_at
            .map(|last_hydration| last_hydration.elapsed() >= GLOBAL_BACKGROUND_HYDRATION_INTERVAL)
            .unwrap_or(true)
        {
            let background_lot_urls = self.other_auction_lot_urls();
            for lot_url in background_lot_urls {
                let _ = self.queue_lot_details(lot_url, false);
            }
            self.last_global_hydration_at = Some(Instant::now());
        }

        changed
    }

    fn pump_prefetch_queues(&mut self) -> bool {
        let mut changed = false;

        while self.detail_requests_in_flight < DETAIL_PREFETCH_IN_FLIGHT_LIMIT {
            let Some(lot_url) = self.detail_prefetch_queue.pop_front() else {
                break;
            };

            self.queued_detail_urls.remove(&lot_url);

            if !self.is_cacheable_lot_url(&lot_url, Utc::now()) {
                self.detail_states.remove(&lot_url);
                changed = true;
                continue;
            }

            let request = DetailFetchRequest {
                lot_url: lot_url.clone(),
                expires_at: self.lot_cache_expires_at(&lot_url),
            };

            if let Err(error) = self.detail_request_tx.send(request) {
                self.detail_states.insert(
                    lot_url,
                    DetailState::Failed(format!("failed to queue detail load: {error}")),
                );
                changed = true;
                continue;
            }

            self.active_detail_urls.insert(lot_url.clone());
            self.detail_requests_in_flight += 1;
        }

        while self.photo_requests_in_flight < PHOTO_PREFETCH_IN_FLIGHT_LIMIT {
            let Some(photo_url) = self.photo_prefetch_queue.pop_front() else {
                break;
            };

            self.queued_photo_urls.remove(&photo_url);

            let request = PhotoFetchRequest {
                url: photo_url.clone(),
                expires_at: self.photo_cache_expires_at(&photo_url),
            };

            if let Err(error) = self.photo_request_tx.send(request) {
                self.photo_states.insert(
                    photo_url,
                    PhotoState::Failed(format!("failed to queue photo load: {error}")),
                );
                changed = true;
                continue;
            }

            self.active_photo_urls.insert(photo_url.clone());
            self.photo_requests_in_flight += 1;
        }

        changed
    }

    fn pump_terminal_photo_queue(&mut self) -> bool {
        let Some(picker) = self.terminal_image_picker.clone() else {
            self.terminal_photo_queue.clear();
            self.terminal_photo_lru.clear();
            self.terminal_photo_states.clear();
            self.queued_terminal_photo_keys.clear();
            self.active_terminal_photo_keys.clear();
            self.terminal_photo_requests_in_flight = 0;
            return false;
        };

        while self.terminal_photo_requests_in_flight < TERMINAL_PHOTO_IN_FLIGHT_LIMIT {
            let Some(key) = self.terminal_photo_queue.pop_front() else {
                break;
            };

            self.queued_terminal_photo_keys.remove(&key);

            if self.terminal_photo_states.contains_key(&key) {
                self.touch_terminal_photo_key(&key);
                continue;
            }

            if self.active_terminal_photo_keys.contains(&key) {
                continue;
            }

            let Some(PhotoState::Loaded(preview)) = self.photo_states.get(&key.0).cloned() else {
                continue;
            };

            let request = TerminalPhotoRenderRequest {
                key: key.clone(),
                picker: picker.clone(),
                preview,
            };

            if self.terminal_photo_request_tx.send(request).is_err() {
                self.terminal_photo_states
                    .insert(key.clone(), TerminalPhotoState::Failed);
                continue;
            }

            self.active_terminal_photo_keys.insert(key);
            self.terminal_photo_requests_in_flight += 1;
        }

        false
    }

    fn configure_terminal_images(&mut self) {
        if !images_enabled() {
            self.terminal_image_picker = None;
            self.status = "Loaded live auction data. Inline photos are disabled.".to_string();
            return;
        }

        if image_mode_fast() {
            self.terminal_image_picker = None;
            self.status = "Loaded live auction data. Using fast portable terminal photo rendering."
                .to_string();
            return;
        }

        match Picker::from_query_stdio() {
            Ok(picker) if picker.protocol_type() == ProtocolType::Halfblocks => {
                self.terminal_image_picker = None;
                self.status = "Loaded live auction data. Using portable terminal photo rendering."
                    .to_string();
            }
            Ok(picker) => {
                let protocol = picker.protocol_type();
                self.terminal_image_picker = Some(picker);
                self.status = format!(
                    "Loaded live auction data. Using {protocol:?} terminal graphics when possible."
                );
            }
            Err(error) => {
                self.terminal_image_picker = None;
                self.status = format!(
                    "Loaded live auction data. Terminal graphics detection failed; using portable rendering: {error}"
                );
            }
        }
    }

    fn mark_pane_resize_changed(&mut self) {
        self.last_pane_resize_at = Some(Instant::now());
        self.pending_image_resize_redraw = true;
    }

    fn should_render_terminal_images(&self) -> bool {
        self.terminal_image_picker.is_some() && !self.image_render_deferred()
    }

    fn image_render_deferred(&self) -> bool {
        self.drag_target.is_some()
            || self
                .last_pane_resize_at
                .map(|last_resize| last_resize.elapsed() < PANE_RESIZE_DEBOUNCE)
                .unwrap_or(false)
    }

    fn image_redraw_ready(&mut self) -> bool {
        if self.image_render_deferred() {
            return false;
        }

        let ready = self.pending_image_resize_redraw;
        self.pending_image_resize_redraw = false;
        ready
    }

    fn sync_selected_lot(&mut self) -> bool {
        let selected_url = self.current_lot_url();

        if self.observed_lot_url != selected_url {
            self.observed_lot_url = selected_url;
            self.details_scroll = 0;
            self.current_auction_hydration_dirty = true;
            return true;
        }

        false
    }

    fn prefetch_lot_index(&mut self, index: usize) {
        let lot = self
            .current_event()
            .and_then(|event| event.lots.get(index))
            .map(|lot| (lot.url.clone(), lot.thumbnail_url.clone()));

        if let Some((lot_url, thumbnail_url)) = lot {
            self.queue_lot_details(lot_url, true);
            if let Some(thumbnail_url) = thumbnail_url {
                self.queue_photo_preview(thumbnail_url, true);
            }
        }
    }

    fn queue_lot_details(&mut self, lot_url: String, priority: bool) -> bool {
        if !self.is_cacheable_lot_url(&lot_url, Utc::now()) {
            return false;
        }

        if matches!(
            self.detail_states.get(&lot_url),
            Some(DetailState::Loaded(_)) | Some(DetailState::Failed(_))
        ) {
            return false;
        }

        if self.active_detail_urls.contains(&lot_url) {
            return false;
        }

        let mut changed = false;

        if self
            .detail_states
            .insert(lot_url.clone(), DetailState::Loading)
            .is_none()
        {
            changed = true;
        }

        if self.queued_detail_urls.contains(&lot_url) {
            if priority {
                self.detail_prefetch_queue
                    .retain(|queued_url| queued_url != &lot_url);
                self.detail_prefetch_queue.push_front(lot_url);
            }
            return changed;
        }

        self.queued_detail_urls.insert(lot_url.clone());
        if priority {
            self.detail_prefetch_queue.push_front(lot_url);
        } else {
            self.detail_prefetch_queue.push_back(lot_url);
        }

        true
    }

    fn queue_detail_batch_front(&mut self, lot_urls: Vec<String>) -> bool {
        let mut changed = false;

        for lot_url in lot_urls.into_iter().rev() {
            changed |= self.queue_lot_details(lot_url, true);
        }

        changed
    }

    fn queue_photo_batch_front(&mut self, photo_urls: Vec<String>) -> bool {
        let mut changed = false;

        for photo_url in photo_urls.into_iter().rev() {
            changed |= self.queue_photo_preview(photo_url, true);
        }

        changed
    }

    fn queue_terminal_photo_batch_front(&mut self, photo_urls: Vec<String>) {
        let Some((width, height)) = self.last_photo_area_size else {
            return;
        };

        for photo_url in photo_urls.into_iter().rev() {
            self.queue_terminal_photo_render(photo_url, width, height, true);
        }
    }

    fn queue_photo_preview(&mut self, photo_url: String, priority: bool) -> bool {
        if matches!(
            self.photo_states.get(&photo_url),
            Some(PhotoState::Loaded(_)) | Some(PhotoState::Failed(_))
        ) {
            return false;
        }

        if self.active_photo_urls.contains(&photo_url) {
            return false;
        }

        let mut changed = false;

        if self
            .photo_states
            .insert(photo_url.clone(), PhotoState::Loading)
            .is_none()
        {
            changed = true;
        }

        if self.queued_photo_urls.contains(&photo_url) {
            if priority {
                self.photo_prefetch_queue
                    .retain(|queued_url| queued_url != &photo_url);
                self.photo_prefetch_queue.push_front(photo_url);
            }
            return changed;
        }

        self.queued_photo_urls.insert(photo_url.clone());
        if priority {
            self.photo_prefetch_queue.push_front(photo_url);
        } else {
            self.photo_prefetch_queue.push_back(photo_url);
        }

        true
    }

    fn queue_terminal_photo_render(
        &mut self,
        photo_url: String,
        width: u16,
        height: u16,
        priority: bool,
    ) -> bool {
        if width == 0
            || height == 0
            || self.terminal_image_picker.is_none()
            || !matches!(
                self.photo_states.get(&photo_url),
                Some(PhotoState::Loaded(_))
            )
        {
            return false;
        }

        let key = (photo_url, width, height);

        if self.terminal_photo_states.contains_key(&key) {
            self.touch_terminal_photo_key(&key);
            return false;
        }

        if self.active_terminal_photo_keys.contains(&key) {
            return false;
        }

        if self.queued_terminal_photo_keys.contains(&key) {
            if priority {
                self.terminal_photo_queue
                    .retain(|queued_key| queued_key != &key);
                self.terminal_photo_queue.push_front(key);
            }
            return false;
        }

        self.queued_terminal_photo_keys.insert(key.clone());
        if priority {
            self.terminal_photo_queue.push_front(key);
        } else {
            self.terminal_photo_queue.push_back(key);
        }

        false
    }

    fn current_auction_lot_urls_by_priority(&self) -> Vec<String> {
        let Some(event) = self.current_event() else {
            return Vec::new();
        };

        if event.lots.is_empty() {
            return Vec::new();
        }

        let selected = self.lot_index.min(event.lots.len() - 1);
        let mut indexes = Vec::with_capacity(event.lots.len());
        indexes.push(selected);

        for offset in 1..event.lots.len() {
            if selected + offset < event.lots.len() {
                indexes.push(selected + offset);
            }

            if selected >= offset {
                indexes.push(selected - offset);
            }
        }

        indexes
            .into_iter()
            .map(|index| event.lots[index].url.clone())
            .collect()
    }

    fn current_auction_photo_urls_by_priority(&self) -> Vec<String> {
        let Some(event) = self.current_event() else {
            return Vec::new();
        };

        if event.lots.is_empty() {
            return Vec::new();
        }

        let selected = self.lot_index.min(event.lots.len() - 1);
        let mut urls = Vec::new();

        if let Some(url) = event.lots[selected].thumbnail_url.clone() {
            urls.push(url);
        }

        for offset in 1..event.lots.len() {
            if selected + offset < event.lots.len()
                && let Some(url) = event.lots[selected + offset].thumbnail_url.clone()
            {
                urls.push(url);
            }

            if selected >= offset
                && let Some(url) = event.lots[selected - offset].thumbnail_url.clone()
            {
                urls.push(url);
            }
        }

        urls
    }

    fn other_auction_lot_urls(&self) -> Vec<String> {
        self.events
            .iter()
            .enumerate()
            .filter(|(event_index, _)| *event_index != self.event_index)
            .flat_map(|(_, event)| event.lots.iter().map(|lot| lot.url.clone()))
            .collect()
    }

    fn is_cacheable_lot_url(&self, lot_url: &str, now: DateTime<Utc>) -> bool {
        self.events.iter().any(|event| {
            event
                .lots
                .iter()
                .any(|lot| lot.url == lot_url && lot_cache_is_active(lot, now))
        })
    }

    fn lot_cache_expires_at(&self, lot_url: &str) -> Option<i64> {
        self.events
            .iter()
            .flat_map(|event| event.lots.iter())
            .find(|lot| lot.url == lot_url)
            .and_then(lot_cache_expires_at)
    }

    fn photo_cache_expires_at(&self, photo_url: &str) -> Option<i64> {
        if let Some(expires_at) = self
            .events
            .iter()
            .flat_map(|event| event.lots.iter())
            .find(|lot| lot.thumbnail_url.as_deref() == Some(photo_url))
            .and_then(lot_cache_expires_at)
        {
            return Some(expires_at);
        }

        self.detail_states.iter().find_map(|(lot_url, state)| {
            let DetailState::Loaded(details) = state else {
                return None;
            };

            details
                .photos
                .iter()
                .any(|url| url == photo_url)
                .then(|| self.lot_cache_expires_at(lot_url))
                .flatten()
        })
    }

    fn is_cacheable_photo_url(&self, photo_url: &str) -> bool {
        self.events
            .iter()
            .flat_map(|event| event.lots.iter())
            .any(|lot| {
                lot_cache_is_active(lot, Utc::now())
                    && lot.thumbnail_url.as_deref() == Some(photo_url)
            })
            || self.detail_states.values().any(|state| match state {
                DetailState::Loaded(details) => details.photos.iter().any(|url| url == photo_url),
                DetailState::Loading | DetailState::Failed(_) => false,
            })
    }

    fn primary_photo_url(&self) -> Option<&str> {
        self.current_lot()
            .and_then(|lot| lot.thumbnail_url.as_deref())
            .or_else(|| {
                self.current_loaded_details()
                    .and_then(|details| details.photos.first())
                    .map(String::as_str)
            })
    }

    fn scroll_details_down(&mut self, amount: usize) {
        self.details_scroll = self.details_scroll.saturating_add(amount);
    }

    fn scroll_details_up(&mut self, amount: usize) {
        self.details_scroll = self.details_scroll.saturating_sub(amount);
    }

    fn set_details_scroll(&mut self, value: usize) {
        self.details_scroll = value;
    }

    fn move_down(&mut self) {
        match self.focus {
            FocusPane::Events => {
                if self.event_index + 1 < self.events.len() {
                    self.event_index += 1;
                    self.lot_index = 0;
                }
            }
            FocusPane::Lots => {
                if let Some(event) = self.current_event()
                    && self.lot_index + 1 < event.lots.len()
                {
                    self.lot_index += 1;
                }
            }
            FocusPane::Details => self.scroll_details_down(1),
        }
    }

    fn move_up(&mut self) {
        match self.focus {
            FocusPane::Events => {
                if self.event_index > 0 {
                    self.event_index -= 1;
                    self.lot_index = 0;
                }
            }
            FocusPane::Lots => {
                if self.lot_index > 0 {
                    self.lot_index -= 1;
                }
            }
            FocusPane::Details => {
                if self.details_scroll == 0 {
                    self.focus = FocusPane::Lots;
                    self.status = "Returned to lots.".to_string();
                } else {
                    self.scroll_details_up(1);
                }
            }
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusPane::Events => FocusPane::Lots,
            FocusPane::Lots => FocusPane::Details,
            FocusPane::Details => FocusPane::Events,
        };
    }

    fn move_focus_left(&mut self) {
        self.focus = match self.focus {
            FocusPane::Events => FocusPane::Events,
            FocusPane::Lots => FocusPane::Events,
            FocusPane::Details => FocusPane::Lots,
        };
    }

    fn move_focus_right(&mut self) {
        self.focus = match self.focus {
            FocusPane::Events => FocusPane::Lots,
            FocusPane::Lots => FocusPane::Details,
            FocusPane::Details => FocusPane::Details,
        };
    }

    fn back_focus(&mut self) {
        self.focus = match self.focus {
            FocusPane::Details => FocusPane::Lots,
            FocusPane::Lots => FocusPane::Events,
            FocusPane::Events => FocusPane::Events,
        };
    }

    fn set_focus(&mut self, focus: FocusPane) {
        self.focus = focus;
    }

    fn focus_lots(&mut self) {
        if self.current_event().is_some() {
            self.set_focus(FocusPane::Lots);
        }
    }

    fn focus_details(&mut self) {
        if self.current_lot().is_some() {
            self.set_focus(FocusPane::Details);
        }
    }

    fn refresh(&mut self, scraper: &AuctionScraper) {
        match load_data(scraper, &self.cli) {
            Ok(events) => {
                self.events = events;
                self.loaded_at = Utc::now();
                self.clamp_selection();
                self.prune_expired_caches();
                self.prune_expired_disk_caches_async();
                self.current_auction_hydration_dirty = true;
                self.last_global_hydration_at = None;
                self.sync_status("Refreshed live auction data.");
            }
            Err(error) => {
                self.status = format!("Refresh failed: {error}");
            }
        }
    }

    fn clamp_selection(&mut self) {
        if self.events.is_empty() {
            self.event_index = 0;
            self.lot_index = 0;
            self.event_offset = 0;
            self.lot_offset = 0;
            return;
        }

        if self.event_index >= self.events.len() {
            self.event_index = self.events.len() - 1;
        }

        let lot_count = self
            .current_event()
            .map(|event| event.lots.len())
            .unwrap_or(0);

        if lot_count == 0 {
            self.lot_index = 0;
        } else if self.lot_index >= lot_count {
            self.lot_index = lot_count - 1;
        }
    }

    fn prune_expired_disk_caches_async(&self) {
        spawn_cache_prune(self.cache.clone());
    }

    fn prune_expired_caches(&mut self) -> bool {
        let now = Utc::now();
        let cacheable_lot_urls = self
            .events
            .iter()
            .flat_map(|event| {
                event
                    .lots
                    .iter()
                    .filter(move |lot| lot_cache_is_active(lot, now))
                    .map(|lot| lot.url.clone())
            })
            .collect::<HashSet<_>>();

        let before = [
            self.detail_states.len(),
            self.detail_prefetch_queue.len(),
            self.queued_detail_urls.len(),
            self.active_detail_urls.len(),
            self.photo_states.len(),
            self.photo_prefetch_queue.len(),
            self.queued_photo_urls.len(),
            self.active_photo_urls.len(),
            self.terminal_photo_states.len(),
            self.terminal_photo_lru.len(),
            self.terminal_photo_queue.len(),
            self.queued_terminal_photo_keys.len(),
            self.active_terminal_photo_keys.len(),
            self.photo_render_buffers.len(),
            self.photo_render_lru.len(),
        ];

        self.detail_states
            .retain(|lot_url, _| cacheable_lot_urls.contains(lot_url));
        self.detail_prefetch_queue
            .retain(|lot_url| cacheable_lot_urls.contains(lot_url));
        self.queued_detail_urls
            .retain(|lot_url| cacheable_lot_urls.contains(lot_url));
        self.active_detail_urls
            .retain(|lot_url| cacheable_lot_urls.contains(lot_url));

        let mut live_photo_urls = self
            .events
            .iter()
            .flat_map(|event| {
                event
                    .lots
                    .iter()
                    .filter(move |lot| lot_cache_is_active(lot, now))
                    .filter_map(|lot| lot.thumbnail_url.clone())
            })
            .collect::<HashSet<_>>();

        live_photo_urls.extend(
            self.detail_states
                .values()
                .filter_map(|state| match state {
                    DetailState::Loaded(details) => Some(details),
                    DetailState::Loading | DetailState::Failed(_) => None,
                })
                .flat_map(|details| details.photos.iter().cloned()),
        );

        self.photo_states
            .retain(|photo_url, _| live_photo_urls.contains(photo_url));
        self.photo_prefetch_queue
            .retain(|photo_url| live_photo_urls.contains(photo_url));
        self.queued_photo_urls
            .retain(|photo_url| live_photo_urls.contains(photo_url));
        self.active_photo_urls
            .retain(|photo_url| live_photo_urls.contains(photo_url));
        self.terminal_photo_states
            .retain(|(photo_url, _, _), _| live_photo_urls.contains(photo_url));
        self.terminal_photo_lru
            .retain(|(photo_url, _, _)| live_photo_urls.contains(photo_url));
        self.terminal_photo_queue
            .retain(|(photo_url, _, _)| live_photo_urls.contains(photo_url));
        self.queued_terminal_photo_keys
            .retain(|(photo_url, _, _)| live_photo_urls.contains(photo_url));
        self.active_terminal_photo_keys
            .retain(|(photo_url, _, _)| live_photo_urls.contains(photo_url));
        self.photo_render_buffers
            .retain(|(photo_url, _, _, _), _| live_photo_urls.contains(photo_url));
        self.photo_render_lru
            .retain(|(photo_url, _, _, _)| live_photo_urls.contains(photo_url));
        self.trim_terminal_photo_states();
        self.trim_photo_render_buffers();

        before
            != [
                self.detail_states.len(),
                self.detail_prefetch_queue.len(),
                self.queued_detail_urls.len(),
                self.active_detail_urls.len(),
                self.photo_states.len(),
                self.photo_prefetch_queue.len(),
                self.queued_photo_urls.len(),
                self.active_photo_urls.len(),
                self.terminal_photo_states.len(),
                self.terminal_photo_lru.len(),
                self.terminal_photo_queue.len(),
                self.queued_terminal_photo_keys.len(),
                self.active_terminal_photo_keys.len(),
                self.photo_render_buffers.len(),
                self.photo_render_lru.len(),
            ]
    }

    fn select_event(&mut self, index: usize) {
        if index >= self.events.len() {
            return;
        }

        self.event_index = index;
        self.lot_index = 0;
        self.details_scroll = 0;
        self.current_auction_hydration_dirty = true;
        self.set_focus(FocusPane::Events);
        self.clamp_selection();
        self.prefetch_lot_index(self.lot_index);
        self.queue_terminal_photo_batch_front(self.current_auction_photo_urls_by_priority());

        if let Some(event) = self.current_event() {
            self.status = format!("Selected auction: {}", event.title);
        }
    }

    fn select_lot(&mut self, index: usize) {
        let Some(event) = self.current_event() else {
            return;
        };

        if index >= event.lots.len() {
            return;
        }

        self.lot_index = index;
        self.set_focus(FocusPane::Lots);
        self.prefetch_lot_index(index);

        if let Some(lot) = self.current_lot() {
            self.status = format!("Selected lot: {}", lot.name);
        }
    }

    fn open_selected_lot(&mut self) {
        let Some((name, url)) = self
            .current_lot()
            .map(|lot| (lot.name.clone(), lot.url.clone()))
        else {
            return;
        };

        match webbrowser::open(&url) {
            Ok(_) => {
                self.status = format!("Opened {name} in your browser.");
            }
            Err(error) => {
                self.status = format!("Could not open browser for {name}: {error}");
            }
        }
    }

    fn sync_status(&mut self, prefix: &str) {
        self.status = format!(
            "{prefix} {} auctions, {} lots.",
            self.events.len(),
            self.total_lots()
        );
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let scraper = AuctionScraper::new()?;
    let events = load_data(&scraper, &cli)?;

    if cli.plain {
        print_plain(&events);
        return Ok(());
    }

    run_tui(App::new(cli, events), &scraper)
}

fn load_data(scraper: &AuctionScraper, cli: &Cli) -> Result<Vec<AuctionEvent>> {
    let city = cli.city.to_lowercase();
    let query = cli.query.as_ref().map(|value| value.to_lowercase());
    let now = Utc::now();
    let mut events = Vec::new();

    for event in scraper.fetch_all_events()? {
        if !contains_case_insensitive(&event.location, &city) {
            continue;
        }

        let mut lots = scraper.fetch_all_lots(&event)?;
        lots.retain(|lot| contains_case_insensitive(&lot.location, &city));

        if let Some(query) = &query {
            lots.retain(|lot| contains_case_insensitive(&lot.name, query));
        }

        if !cli.include_past {
            lots.retain(|lot| {
                lot.close_at
                    .as_ref()
                    .map(|close_at| close_at.with_timezone(&Utc) > now)
                    .unwrap_or(false)
            });
        }

        if lots.is_empty() {
            continue;
        }

        sort_lots(&mut lots, cli.sort);
        events.push(AuctionEvent {
            title: event.title,
            url: event.url,
            location: event.location,
            close_at: event.close_at,
            lots,
        });
    }

    events.sort_by(|left, right| {
        compare_option_i64(
            left.primary_close_at().map(|close_at| close_at.timestamp()),
            right
                .primary_close_at()
                .map(|close_at| close_at.timestamp()),
        )
        .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
    });

    Ok(events)
}

fn sort_lots(lots: &mut [Lot], sort_key: SortKey) {
    lots.sort_by(|left, right| match sort_key {
        SortKey::Close => compare_option_i64(
            left.close_at.as_ref().map(|close_at| close_at.timestamp()),
            right.close_at.as_ref().map(|close_at| close_at.timestamp()),
        )
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())),
        SortKey::Price => compare_option_i64_desc(left.bid_cents, right.bid_cents)
            .then_with(|| {
                compare_option_i64(
                    left.close_at.as_ref().map(|close_at| close_at.timestamp()),
                    right.close_at.as_ref().map(|close_at| close_at.timestamp()),
                )
            })
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())),
        SortKey::Name => left
            .name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| {
                compare_option_i64(
                    left.close_at.as_ref().map(|close_at| close_at.timestamp()),
                    right.close_at.as_ref().map(|close_at| close_at.timestamp()),
                )
            }),
    });
}

fn lot_cache_is_active(lot: &Lot, now: DateTime<Utc>) -> bool {
    lot.close_at
        .as_ref()
        .map(|close_at| close_at.with_timezone(&Utc) > now)
        .unwrap_or(true)
}

fn lot_cache_expires_at(lot: &Lot) -> Option<i64> {
    lot.close_at
        .as_ref()
        .map(|close_at| close_at.with_timezone(&Utc).timestamp())
}

fn parse_events(html: &str) -> Result<Vec<EventStub>> {
    parse_events_from_html(Html::parse_document(html))
}

fn parse_events_fragment(html: &str) -> Result<Vec<EventStub>> {
    parse_events_from_html(Html::parse_fragment(html))
}

fn parse_events_from_html(document: Html) -> Result<Vec<EventStub>> {
    let item_selector = selector(".online-auction-item")?;
    let link_selector = selector(".item-title h4 a")?;
    let location_selector = selector(".item-location")?;
    let close_selector = selector(".item-date-no-feature h3")?;
    let mut events = Vec::new();

    for item in document.select(&item_selector) {
        let Some(link) = item.select(&link_selector).next() else {
            continue;
        };

        let Some(relative_url) = link.value().attr("href") else {
            continue;
        };

        let title = text_from_node(&link);

        if title.is_empty() {
            continue;
        }

        let location = item
            .select(&location_selector)
            .next()
            .map(|node| extract_location(&text_from_node(&node)))
            .unwrap_or_default();

        let close_at = item
            .select(&close_selector)
            .next()
            .map(|node| parse_event_close(&text_from_node(&node)))
            .transpose()?
            .flatten();

        events.push(EventStub {
            title,
            url: Url::parse(BASE_URL)?.join(relative_url)?.to_string(),
            location,
            close_at,
        });
    }

    Ok(events)
}

fn parse_lots(html: &str) -> Result<Vec<Lot>> {
    parse_lots_from_html(Html::parse_document(html))
}

fn parse_lots_fragment(html: &str) -> Result<Vec<Lot>> {
    parse_lots_from_html(Html::parse_fragment(html))
}

fn parse_lot_details(html: &str) -> Result<LotDetails> {
    let document = Html::parse_document(html);
    let gallery_selector = selector(r#"a[data-fancybox="gallery"]"#)?;
    let bid_details_selector = selector(".bid-details p")?;
    let listing_selector = selector("#listing-details")?;
    let row_selector = selector("tr")?;
    let document_selector = selector(".documents-wrap a[href]")?;
    let mut seen_photos = HashSet::new();
    let mut photos = Vec::new();

    for link in document.select(&gallery_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };

        if !is_image_url(href) {
            continue;
        }

        let url = Url::parse(BASE_URL)?.join(href)?.to_string();

        if seen_photos.insert(url.clone()) {
            photos.push(url);
        }
    }

    let mut info_lines = Vec::new();

    for node in document.select(&bid_details_selector) {
        let line = text_from_node(&node);

        if !line.is_empty() {
            push_unique_line(&mut info_lines, line);
        }
    }

    if let Some(listing) = document.select(&listing_selector).next() {
        for row in listing.select(&row_selector) {
            let line = text_from_node(&row);

            if !line.is_empty() && line != "\u{a0}" {
                push_unique_line(&mut info_lines, line);
            }
        }
    }

    let mut documents = Vec::new();
    let mut seen_documents = HashSet::new();

    for link in document.select(&document_selector) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };

        let url = Url::parse(BASE_URL)?.join(href)?.to_string();
        let label = text_from_node(&link);

        if seen_documents.insert(url.clone()) {
            documents.push(DetailLink {
                label: if label.is_empty() {
                    "Document".to_string()
                } else {
                    label
                },
                url,
            });
        }
    }

    let info_lines = clean_detail_lines(&info_lines);

    Ok(LotDetails {
        photos,
        info_lines,
        documents,
    })
}

fn clean_lot_details(mut details: LotDetails) -> LotDetails {
    details.info_lines = clean_detail_lines(&details.info_lines);
    details
}

fn clean_detail_lines(lines: &[String]) -> Vec<String> {
    let mut cleaned_lines = Vec::new();

    for line in lines {
        if let Some(cleaned) = clean_detail_line(line) {
            push_unique_line(&mut cleaned_lines, cleaned);
        }
    }

    cleaned_lines
}

fn clean_detail_line(line: &str) -> Option<String> {
    let mut cleaned = normalize_whitespace(line);

    for regex in generic_detail_regexes() {
        cleaned = regex.replace_all(&cleaned, " ").into_owned();
        cleaned = normalize_whitespace(&cleaned);
    }

    (!cleaned.is_empty()).then_some(cleaned)
}

fn generic_detail_regexes() -> &'static [Regex] {
    static REGEXES: OnceLock<Vec<Regex>> = OnceLock::new();

    REGEXES.get_or_init(|| {
        [
            r#"(?i)\bMcDougall Auctioneers Ltd\. strives to represent the item\(s\) as visible to the naked eye\..*?mandatory disposal fee\.?"#,
            r#"(?i)\bAll payments must be made in full before the item\(s\) can be released to the purchaser\..*?mandatory disposal fee\.?"#,
            r#"(?i)\bFrom the time this unit arrived at McDougall Auction'?s sale location, McDougall Auction verified its specific running condition\..*?the purchaser accepts the unit "as is\.?""#,
            r#"(?i)^Item Quantity:\s*1$"#,
            r#"(?i)^(Other Details|Other Description):\s*$"#,
        ]
        .into_iter()
        .map(|pattern| Regex::new(pattern).expect("generic detail regex should compile"))
        .collect()
    })
}

fn decode_photo_preview(bytes: &[u8]) -> Result<PhotoPreview> {
    let image = image::load_from_memory(bytes).context("failed to decode photo")?;
    let rgb = image.to_rgb8();
    let (width, height) = rgb.dimensions();

    if width == 0 || height == 0 {
        return Err(anyhow!("photo has no pixels"));
    }

    let scale = (MAX_PHOTO_PREVIEW_WIDTH as f32 / width as f32)
        .min(MAX_PHOTO_PREVIEW_HEIGHT as f32 / height as f32)
        .min(1.0);
    let target_width = ((width as f32 * scale).round() as u32).max(1);
    let target_height = ((height as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(
        &rgb,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );

    Ok(PhotoPreview {
        width: target_width,
        height: target_height,
        image: resized,
    })
}

fn parse_lots_from_html(document: Html) -> Result<Vec<Lot>> {
    let item_selector = selector(".auction-product-item")?;
    let image_selector = selector(".item-img img")?;
    let link_selector = selector(".item-title h4 a")?;
    let location_selector = selector(".item-location")?;
    let bid_selector = selector(".current-bid")?;
    let close_selector = selector(r#"input[id^="txtLotEndDate"]"#)?;
    let mut lots = Vec::new();

    for item in document.select(&item_selector) {
        let Some(link) = item.select(&link_selector).next() else {
            continue;
        };

        let Some(relative_url) = link.value().attr("href") else {
            continue;
        };

        let name = text_from_node(&link);

        if name.is_empty() {
            continue;
        }

        let thumbnail_url = item
            .select(&image_selector)
            .next()
            .and_then(|node| {
                node.value()
                    .attr("src")
                    .or_else(|| node.value().attr("data-src"))
            })
            .filter(|src| is_image_url(src))
            .and_then(|src| Url::parse(BASE_URL).ok()?.join(src).ok())
            .map(|url| url.to_string());

        let location = item
            .select(&location_selector)
            .next()
            .map(|node| extract_location(&text_from_node(&node)))
            .unwrap_or_default();

        let bid_display = item
            .select(&bid_selector)
            .next()
            .map(|node| extract_bid(&text_from_node(&node)))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "N/A".to_string());

        let close_at = item
            .select(&close_selector)
            .next()
            .and_then(|node| node.value().attr("value"))
            .map(parse_lot_close)
            .transpose()?
            .flatten();

        lots.push(Lot {
            name,
            url: Url::parse(BASE_URL)?.join(relative_url)?.to_string(),
            thumbnail_url,
            location,
            bid_cents: parse_bid_cents(&bid_display),
            bid_display,
            close_at,
        });
    }

    Ok(lots)
}

fn parse_next_item(html: &str) -> Result<Option<usize>> {
    let document = Html::parse_document(html);
    let selector = selector("#txtNextItem")?;
    let value = document
        .select(&selector)
        .next()
        .and_then(|node| node.value().attr("value"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<usize>()
                .with_context(|| format!("invalid next item value: {value}"))
        })
        .transpose()?;

    Ok(value)
}

fn parse_event_close(value: &str) -> Result<Option<DateTime<FixedOffset>>> {
    let parts = value.split('/').map(str::trim).collect::<Vec<_>>();

    if parts.len() != 3 {
        return Ok(None);
    }

    let naive = chrono::NaiveDateTime::parse_from_str(
        &format!("{} {}", parts[1], parts[2]),
        "%b %e, %Y %I:%M %p",
    )
    .with_context(|| format!("failed to parse event close time: {value}"))?;

    Ok(FixedOffset::west_opt(6 * 60 * 60)
        .and_then(|offset| offset.from_local_datetime(&naive).single()))
}

fn parse_lot_close(value: &str) -> Result<Option<DateTime<FixedOffset>>> {
    Ok(DateTime::parse_from_rfc3339(value).ok())
}

fn parse_bid_cents(value: &str) -> Option<i64> {
    let filtered = value
        .chars()
        .filter(|character| character.is_ascii_digit() || *character == '.')
        .collect::<String>();

    if filtered.is_empty() {
        return None;
    }

    let mut parts = filtered.split('.');
    let dollars = parts.next()?.parse::<i64>().ok()?;
    let cents_fragment = parts.next().unwrap_or("0");
    let padded = format!("{cents_fragment:0<2}");
    let cents = padded.get(..2)?.parse::<i64>().ok()?;

    Some(dollars * 100 + cents)
}

fn compare_option_i64(left: Option<i64>, right: Option<i64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_option_i64_desc(left: Option<i64>, right: Option<i64>) -> Ordering {
    compare_option_i64(right, left)
}

fn extract_location(value: &str) -> String {
    value
        .rsplit_once("Location:")
        .map(|(_, location)| location.trim().to_string())
        .unwrap_or_else(|| value.trim().to_string())
}

fn extract_bid(value: &str) -> String {
    if let Some(index) = value.find("Bid:") {
        let tail = value[(index + 4)..].trim();

        if let Some(cad_index) = tail.find(" CAD") {
            return tail[..cad_index].trim().to_string();
        }

        return tail.to_string();
    }

    value.trim().to_string()
}

fn contains_case_insensitive(value: &str, needle_lower: &str) -> bool {
    value.to_lowercase().contains(needle_lower)
}

fn is_image_url(value: &str) -> bool {
    let lower = value
        .split('?')
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();

    [".jpg", ".jpeg", ".png", ".gif", ".webp"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

fn push_unique_line(lines: &mut Vec<String>, line: String) {
    if !lines.iter().any(|existing| existing == &line) {
        lines.push(line);
    }
}

fn selector(value: &str) -> Result<Selector> {
    Selector::parse(value).map_err(|_| anyhow!("invalid selector: {value}"))
}

fn text_from_node(node: &ElementRef<'_>) -> String {
    normalize_whitespace(&node.text().collect::<Vec<_>>().join(" "))
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn format_close_time(value: Option<&DateTime<FixedOffset>>) -> String {
    value
        .map(|close_at| {
            close_at
                .with_timezone(&Regina)
                .format("%b %d, %I:%M %P")
                .to_string()
        })
        .unwrap_or_else(|| "Unknown".to_string())
}

fn lot_status(lot: &Lot) -> &'static str {
    match lot.close_at.as_ref() {
        Some(close_at) if close_at.with_timezone(&Utc) > Utc::now() => "Open",
        Some(_) => "Closed",
        None => "Unknown",
    }
}

fn print_plain(events: &[AuctionEvent]) {
    if events.is_empty() {
        println!("No matching auction lots found.");
        return;
    }

    for (index, event) in events.iter().enumerate() {
        if index > 0 {
            println!();
        }

        println!("{}", event.title);
        println!("{}", "=".repeat(event.title.len()));
        println!("Auction: {}", event.url);
        println!("Location: {}", event.location);
        println!("Lots: {}", event.lots.len());
        println!();
        println!("{:<19}{:<14}Item", "Close", "Price");

        for lot in &event.lots {
            println!(
                "{:<19}{:<14}{}",
                format_close_time(lot.close_at.as_ref()),
                lot.bid_display,
                lot.name
            );
        }
    }
}

fn run_tui(mut app: App, scraper: &AuctionScraper) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;
    app.configure_terminal_images();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let result = tui_loop(&mut terminal, &mut app, scraper);
    restore_terminal(&mut terminal)?;
    result
}

#[derive(Default)]
struct FrameProfile {
    input: Duration,
    detail_results: Duration,
    photo_results: Duration,
    terminal_photo_results: Duration,
    sync_selection: Duration,
    image_redraw: Duration,
    hydrate: Duration,
    pump_queues: Duration,
    pump_terminal_photos: Duration,
    prune: Duration,
    draw: Duration,
    input_events: usize,
    drew: bool,
}

impl FrameProfile {
    fn active_duration(&self) -> Duration {
        self.input
            + self.detail_results
            + self.photo_results
            + self.terminal_photo_results
            + self.sync_selection
            + self.image_redraw
            + self.hydrate
            + self.pump_queues
            + self.pump_terminal_photos
            + self.prune
            + self.draw
    }
}

struct PerfProfiler {
    file: Option<File>,
    frame: u64,
    wait_samples: u64,
    wait_total: Duration,
}

impl PerfProfiler {
    fn from_env() -> Self {
        let Some(value) = std::env::var_os("MCDOUGS_PROFILE") else {
            return Self {
                file: None,
                frame: 0,
                wait_samples: 0,
                wait_total: Duration::ZERO,
            };
        };

        let path = if value.is_empty() || value == "1" {
            std::env::temp_dir().join("mcdougs-profile.csv")
        } else {
            PathBuf::from(value)
        };

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)
            .ok();

        if let Some(file) = file.as_mut() {
            let _ = writeln!(
                file,
                "frame,input_events,drew,active_ms,input_ms,detail_ms,photo_ms,terminal_photo_ms,sync_ms,image_ms,hydrate_ms,pump_ms,pump_terminal_photo_ms,prune_ms,draw_ms,avg_wait_ms"
            );
        }

        Self {
            file,
            frame: 0,
            wait_samples: 0,
            wait_total: Duration::ZERO,
        }
    }

    fn record(&mut self, profile: &FrameProfile) {
        let Some(file) = self.file.as_mut() else {
            return;
        };

        self.frame += 1;
        let active_duration = profile.active_duration();

        if !profile.drew && profile.input_events == 0 && active_duration < Duration::from_millis(4)
        {
            return;
        }

        let avg_wait = if self.wait_samples == 0 {
            0.0
        } else {
            duration_ms(self.wait_total) / self.wait_samples as f64
        };

        let _ = writeln!(
            file,
            "{},{},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3}",
            self.frame,
            profile.input_events,
            profile.drew,
            duration_ms(active_duration),
            duration_ms(profile.input),
            duration_ms(profile.detail_results),
            duration_ms(profile.photo_results),
            duration_ms(profile.terminal_photo_results),
            duration_ms(profile.sync_selection),
            duration_ms(profile.image_redraw),
            duration_ms(profile.hydrate),
            duration_ms(profile.pump_queues),
            duration_ms(profile.pump_terminal_photos),
            duration_ms(profile.prune),
            duration_ms(profile.draw),
            avg_wait,
        );
    }

    fn record_wait(&mut self, duration: Duration) {
        if self.file.is_none() {
            return;
        }

        self.wait_samples += 1;
        self.wait_total += duration;
    }
}

fn measure_phase<T>(duration: &mut Duration, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let result = work();
    *duration = started.elapsed();
    result
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn image_mode_value() -> String {
    std::env::var("MCDOUGS_IMAGE_MODE")
        .or_else(|_| {
            std::env::var("MCDOUGS_TERMINAL_GRAPHICS").map(|value| {
                if value == "1" || value.eq_ignore_ascii_case("true") {
                    "auto".to_string()
                } else {
                    "fast".to_string()
                }
            })
        })
        .unwrap_or_else(|_| "auto".to_string())
        .to_ascii_lowercase()
}

fn images_enabled() -> bool {
    !matches!(image_mode_value().as_str(), "off" | "none" | "false" | "0")
}

fn image_mode_fast() -> bool {
    matches!(
        image_mode_value().as_str(),
        "fast" | "portable" | "ansi" | "halfblocks"
    )
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    scraper: &AuctionScraper,
) -> Result<()> {
    let mut needs_draw = true;
    let mut last_cache_prune_at = Instant::now();
    let mut profiler = PerfProfiler::from_env();

    loop {
        let mut changed = false;
        let mut profile = FrameProfile::default();

        let input_started = Instant::now();
        for _ in 0..MAX_INPUT_EVENTS_PER_FRAME {
            if !event::poll(Duration::from_millis(0))? {
                break;
            }

            profile.input_events += 1;
            if handle_terminal_event(terminal, app, scraper, event::read()?)? {
                return Ok(());
            }
            needs_draw = true;
        }
        profile.input = input_started.elapsed();

        changed |= measure_phase(&mut profile.detail_results, || app.process_detail_results());
        changed |= measure_phase(&mut profile.photo_results, || app.process_photo_results());
        changed |= measure_phase(&mut profile.terminal_photo_results, || {
            app.process_terminal_photo_results()
        });
        changed |= measure_phase(&mut profile.sync_selection, || app.sync_selected_lot());
        changed |= measure_phase(&mut profile.image_redraw, || app.image_redraw_ready());
        changed |= measure_phase(&mut profile.hydrate, || app.hydrate_current_context());
        changed |= measure_phase(&mut profile.pump_queues, || app.pump_prefetch_queues());
        changed |= measure_phase(&mut profile.pump_terminal_photos, || {
            app.pump_terminal_photo_queue()
        });

        if last_cache_prune_at.elapsed() >= Duration::from_secs(30) {
            changed |= measure_phase(&mut profile.prune, || app.prune_expired_caches());
            app.prune_expired_disk_caches_async();
            last_cache_prune_at = Instant::now();
        }

        if needs_draw || changed {
            let draw_started = Instant::now();
            terminal.draw(|frame| draw(frame, app))?;
            profile.draw = draw_started.elapsed();
            profile.drew = true;
            needs_draw = false;
        }

        profiler.record(&profile);
        let wait_started = Instant::now();
        let _ = event::poll(INPUT_POLL_INTERVAL)?;
        profiler.record_wait(wait_started.elapsed());
    }
}

fn handle_terminal_event(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    scraper: &AuctionScraper,
    event: Event,
) -> Result<bool> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }

            match key.code {
                KeyCode::Char('q') => return Ok(true),
                KeyCode::Esc => {
                    if app.focus == FocusPane::Events {
                        return Ok(true);
                    } else {
                        app.back_focus();
                    }
                }
                KeyCode::Tab => app.toggle_focus(),
                KeyCode::Backspace => app.back_focus(),
                KeyCode::Left => app.move_focus_left(),
                KeyCode::Right => app.move_focus_right(),
                KeyCode::Enter => match app.focus {
                    FocusPane::Events => app.focus_lots(),
                    FocusPane::Lots => app.focus_details(),
                    FocusPane::Details => {
                        app.status =
                            "Press o to open the bidding page in your browser.".to_string();
                    }
                },
                KeyCode::Char('o') => app.open_selected_lot(),
                KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                KeyCode::Char('r') => app.refresh(scraper),
                KeyCode::Home => match app.focus {
                    FocusPane::Events => {
                        app.event_index = 0;
                        app.lot_index = 0;
                    }
                    FocusPane::Lots => {
                        app.lot_index = 0;
                    }
                    FocusPane::Details => {
                        app.set_details_scroll(0);
                    }
                },
                KeyCode::End => match app.focus {
                    FocusPane::Events => {
                        if !app.events.is_empty() {
                            app.event_index = app.events.len() - 1;
                            app.lot_index = 0;
                        }
                    }
                    FocusPane::Lots => {
                        if let Some(event) = app.current_event()
                            && !event.lots.is_empty()
                        {
                            app.lot_index = event.lots.len() - 1;
                        }
                    }
                    FocusPane::Details => {
                        app.set_details_scroll(usize::from(u16::MAX));
                    }
                },
                _ => {}
            }
        }
        Event::Mouse(mouse) => {
            handle_mouse(app, terminal.size()?.into(), mouse);
        }
        Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
    }

    Ok(false)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")
}

#[derive(Debug, Clone, Copy)]
struct UiLayout {
    header: Rect,
    body: Rect,
    footer: Rect,
    wide: bool,
    right: Rect,
    events: Rect,
    lots_header: Rect,
    lots_body: Rect,
    details: Rect,
}

fn split_outer(area: Rect) -> [Rect; 3] {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(12),
            Constraint::Length(2),
        ])
        .split(area);
    [outer[0], outer[1], outer[2]]
}

fn compute_layout(area: Rect, app: &App) -> UiLayout {
    let outer = split_outer(area);

    if outer[1].width >= 120 {
        let event_width = app
            .event_pane_width
            .clamp(24, outer[1].width.saturating_sub(50));
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(event_width), Constraint::Min(50)])
            .split(outer[1]);
        let lots_percent = app.lots_pane_percent.clamp(20, 80);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(lots_percent),
                Constraint::Percentage(100 - lots_percent),
            ])
            .split(columns[1]);
        let lots = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(right[0]);

        UiLayout {
            header: outer[0],
            body: outer[1],
            footer: outer[2],
            wide: true,
            right: columns[1],
            events: columns[0],
            lots_header: lots[0],
            lots_body: lots[1],
            details: right[1],
        }
    } else {
        let events_percent = app.stacked_events_percent.clamp(10, 55);
        let lots_percent = app
            .stacked_lots_percent
            .clamp(10, 80_u16.saturating_sub(events_percent).max(10));
        let details_percent = 100_u16
            .saturating_sub(events_percent)
            .saturating_sub(lots_percent);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(events_percent),
                Constraint::Percentage(lots_percent),
                Constraint::Percentage(details_percent),
            ])
            .split(outer[1]);
        let lots = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(rows[1]);

        UiLayout {
            header: outer[0],
            body: outer[1],
            footer: outer[2],
            wide: false,
            right: outer[1],
            events: rows[0],
            lots_header: lots[0],
            lots_body: lots[1],
            details: rows[2],
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let layout = compute_layout(frame.area(), app);

    draw_header(frame, layout.header, app);
    draw_events(frame, layout.events, app);
    draw_lots(frame, layout.lots_header, layout.lots_body, app);
    draw_details(frame, layout.details, app);
    draw_footer(frame, layout.footer, app);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let query_label = app.cli.query.as_deref().unwrap_or("all items");
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "McDougall Browser",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "{} auctions | {} lots | focus: {}",
                app.events.len(),
                app.total_lots(),
                app.focus
            )),
        ]),
        Line::from(format!(
            "city={} | query={} | sort={} | include_past={}",
            app.cli.city, query_label, app.cli.sort, app.cli.include_past
        )),
        Line::from(format!(
            "loaded {}",
            app.loaded_at
                .with_timezone(&Regina)
                .format("%Y-%m-%d %I:%M:%S %P CST")
        )),
    ];

    let widget = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Session"))
        .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

fn resize_border_style(app: &App, targets: &[DragTarget]) -> Option<Style> {
    if app
        .drag_target
        .is_some_and(|target| targets.contains(&target))
    {
        return Some(
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        );
    }

    if app
        .resize_hover_target
        .is_some_and(|target| targets.contains(&target))
    {
        return Some(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }

    None
}

fn draw_events(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let items = if app.events.is_empty() {
        vec![ListItem::new(Line::from("No matching auctions found."))]
    } else {
        app.events
            .iter()
            .map(|event| {
                ListItem::new(vec![
                    Line::from(Span::styled(
                        event.title.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        format!(
                            "{} | {} lots | closes {}",
                            event.location,
                            event.lots.len(),
                            format_close_time(event.primary_close_at())
                        ),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect::<Vec<_>>()
    };

    let border_style = resize_border_style(app, &[DragTarget::EventsWidth]).unwrap_or_else(|| {
        if app.focus == FocusPane::Events {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        }
    });

    let widget = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title("Auctions"),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default().with_offset(app.event_offset);

    if !app.events.is_empty() {
        state.select(Some(app.event_index));
    }

    frame.render_stateful_widget(widget, area, &mut state);
    app.event_offset = state.offset();
}

fn draw_lots(frame: &mut ratatui::Frame<'_>, header_area: Rect, body_area: Rect, app: &mut App) {
    let border_style = resize_border_style(
        app,
        &[DragTarget::EventsWidth, DragTarget::LotsDetailsSplit],
    )
    .unwrap_or_else(|| {
        if app.focus == FocusPane::Lots {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        }
    });

    let header = Paragraph::new(Line::from(vec![
        Span::styled("Close", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("              "),
        Span::styled("Price", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("        "),
        Span::styled("Item", Style::default().add_modifier(Modifier::BOLD)),
    ]))
    .block(
        Block::default()
            .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
            .border_style(border_style)
            .title("Lots"),
    );
    frame.render_widget(header, header_area);

    let list_block = Block::default()
        .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
        .border_style(border_style);
    let visible_rows = usize::from(list_block.inner(body_area).height.max(1));

    let lot_count = app
        .current_event()
        .map(|event| event.lots.len())
        .unwrap_or(0);

    let (items, selected) = if lot_count > 0 {
        app.lot_offset = visible_offset(app.lot_offset, app.lot_index, visible_rows)
            .min(lot_count.saturating_sub(1));
        let offset = app.lot_offset;
        let selected = Some(app.lot_index.saturating_sub(offset));
        let items = app
            .current_event()
            .map(|event| {
                event
                    .lots
                    .iter()
                    .skip(offset)
                    .take(visible_rows)
                    .map(|lot| {
                        ListItem::new(Line::from(format!(
                            "{:<18} {:<12} {}",
                            format_close_time(lot.close_at.as_ref()),
                            lot.bid_display,
                            lot.name
                        )))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (items, selected)
    } else {
        (vec![ListItem::new(Line::from("No lots available."))], None)
    };

    let widget = List::new(items).block(list_block).highlight_style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let mut state = ListState::default();
    state.select(selected);

    frame.render_stateful_widget(widget, body_area, &mut state);
}

fn visible_offset(current_offset: usize, selected_index: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return selected_index;
    }

    if selected_index < current_offset {
        selected_index
    } else if selected_index >= current_offset.saturating_add(visible_rows) {
        selected_index
            .saturating_add(1)
            .saturating_sub(visible_rows)
    } else {
        current_offset
    }
}

fn draw_details(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let border_style = resize_border_style(
        app,
        &[DragTarget::EventsWidth, DragTarget::LotsDetailsSplit],
    )
    .unwrap_or_else(|| {
        if app.focus == FocusPane::Details {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        }
    });

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title("Details");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (image_area, text_area) = details_body_areas(app, inner);

    if let Some(image_area) = image_area {
        draw_primary_photo(frame, image_area, app);
    }

    if text_area.height == 0 || text_area.width == 0 {
        return;
    }

    let lines = detail_text_lines(app);
    let document_height = wrapped_document_height(&lines, text_area.width);
    let max_scroll = document_height.saturating_sub(usize::from(text_area.height));
    app.details_scroll = app
        .details_scroll
        .min(max_scroll)
        .min(usize::from(u16::MAX));

    let details = Paragraph::new(lines)
        .scroll((app.details_scroll as u16, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(details, text_area);
}

fn details_body_areas(app: &App, inner: Rect) -> (Option<Rect>, Rect) {
    let image_width = details_image_width(app, inner);

    if image_width == 0 {
        return (None, inner);
    }

    let text_x = inner
        .x
        .saturating_add(image_width)
        .saturating_add(DETAILS_IMAGE_GAP);
    let text_width = inner
        .width
        .saturating_sub(image_width)
        .saturating_sub(DETAILS_IMAGE_GAP);

    if text_width == 0 {
        return (None, inner);
    }

    (
        Some(Rect {
            x: inner.x,
            y: inner.y,
            width: image_width,
            height: inner.height,
        }),
        Rect {
            x: text_x,
            y: inner.y,
            width: text_width,
            height: inner.height,
        },
    )
}

fn details_image_width(app: &App, inner: Rect) -> u16 {
    if !images_enabled() {
        return 0;
    }

    if inner.height < 3
        || inner.width
            < DETAILS_IMAGE_MIN_WIDTH
                .saturating_add(DETAILS_IMAGE_GAP)
                .saturating_add(DETAILS_TEXT_MIN_WIDTH)
    {
        return 0;
    }

    if app.primary_photo_url().is_none() {
        return 0;
    }

    let max_width = inner
        .width
        .saturating_sub(DETAILS_IMAGE_GAP)
        .saturating_sub(DETAILS_TEXT_MIN_WIDTH)
        .min(DETAILS_IMAGE_MAX_WIDTH);

    if max_width < DETAILS_IMAGE_MIN_WIDTH {
        return 0;
    }

    DETAILS_IMAGE_TARGET_WIDTH.clamp(DETAILS_IMAGE_MIN_WIDTH, max_width)
}

fn draw_primary_photo(frame: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let Some(photo_url) = app.primary_photo_url().map(str::to_string) else {
        return;
    };

    let photo_area_size = (area.width, area.height);
    let photo_area_changed = app.last_photo_area_size != Some(photo_area_size);
    app.last_photo_area_size = Some(photo_area_size);
    if photo_area_changed {
        app.queue_terminal_photo_batch_front(app.current_auction_photo_urls_by_priority());
    }
    app.queue_terminal_photo_render(photo_url.clone(), area.width, area.height, true);

    if app.image_render_deferred() {
        frame.render_widget(Clear, area);
        return;
    }

    let use_terminal_images = app.should_render_terminal_images();
    let render_key = (
        photo_url.clone(),
        area.width,
        area.height,
        use_terminal_images,
    );

    if app.photo_render_buffers.contains_key(&render_key) {
        app.touch_photo_render_buffer(&render_key);
    } else if let Some(buffer) = render_primary_photo_buffer(
        app,
        &photo_url,
        area.width,
        area.height,
        use_terminal_images,
    ) {
        app.insert_photo_render_buffer(render_key.clone(), buffer);
    }

    if let Some(buffer) = app.cached_photo_render_buffer(&render_key) {
        copy_photo_buffer(buffer, area, frame.buffer_mut());
    }
}

fn render_primary_photo_buffer(
    app: &mut App,
    photo_url: &str,
    width: u16,
    height: u16,
    use_terminal_images: bool,
) -> Option<Buffer> {
    if width == 0 || height == 0 {
        return None;
    }

    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);

    if use_terminal_images {
        match app.terminal_photo_state_for_render(photo_url, width, height) {
            Some(TerminalPhotoState::Loaded(protocol)) => {
                SlicedImage::new(protocol, SignedPosition::from((0, 0))).render(area, &mut buffer);
                return Some(buffer);
            }
            Some(TerminalPhotoState::Failed) => {}
            None => return Some(buffer),
        }
    }

    match app.photo_states.get(photo_url) {
        Some(PhotoState::Loaded(preview)) => {
            PhotoFitWidget {
                preview,
                full_width: width,
                full_height: height,
                row_offset: 0,
            }
            .render(area, &mut buffer);
        }
        Some(PhotoState::Loading) | None => {}
        Some(PhotoState::Failed(error)) => {
            Paragraph::new(format!("Could not load photo: {error}"))
                .wrap(Wrap { trim: true })
                .render(area, &mut buffer);
        }
    }

    Some(buffer)
}

fn copy_photo_buffer(source: &Buffer, area: Rect, target: &mut Buffer) {
    let width = area.width.min(source.area.width);
    let height = area.height.min(source.area.height);

    for y in 0..height {
        for x in 0..width {
            target[(area.x.saturating_add(x), area.y.saturating_add(y))] = source[(x, y)].clone();
        }
    }
}

fn detail_text_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = detail_summary_lines(app);
    let notes = detail_note_lines(app);

    if !notes.is_empty() {
        lines.push(Line::from(""));
        lines.extend(notes);
    }

    lines
}

fn wrapped_document_height(lines: &[Line<'static>], width: u16) -> usize {
    let width = usize::from(width.max(1));

    lines
        .iter()
        .map(|line| {
            let character_count = line
                .spans
                .iter()
                .map(|span| span.content.chars().count())
                .sum::<usize>();
            character_count.max(1).div_ceil(width).max(1)
        })
        .sum()
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let widget = Paragraph::new(vec![
        Line::from(app.status.clone()),
        Line::from("Left/Right panes | Up/Down scroll details | Drag highlighted borders to resize | o open lot | r refresh | q quit"),
    ])
    .block(Block::default().borders(Borders::ALL).title("Help"))
    .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
}

fn detail_summary_lines(app: &App) -> Vec<Line<'static>> {
    match (app.current_event(), app.current_lot()) {
        (Some(event), Some(lot)) => vec![
            Line::from(Span::styled(
                lot.name.clone(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            detail_line("Bid", &lot.bid_display),
            Line::from(vec![
                Span::styled("Status: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(lot_status(lot).to_string()),
                Span::raw(" | "),
                Span::styled("Closes: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format_close_time(lot.close_at.as_ref())),
            ]),
            detail_line("Lot Location", &lot.location),
            detail_line("Auction", &event.title),
        ],
        (Some(event), None) => vec![
            Line::from(Span::styled(
                event.title.clone(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            detail_line("Auction Location", &event.location),
            detail_line("Auction URL", &event.url),
            Line::from("No lot is currently selected."),
        ],
        (None, _) => vec![
            Line::from("No auction data to display."),
            Line::from("Try a different --city or --query, or press r to refresh."),
        ],
    }
}

fn detail_note_lines(app: &App) -> Vec<Line<'static>> {
    match app.current_detail_state() {
        Some(DetailState::Loaded(details)) => {
            let mut lines = Vec::new();

            if !details.info_lines.is_empty() {
                lines.push(Line::from(Span::styled(
                    "Other Details",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));

                for line in &details.info_lines {
                    lines.push(Line::from(line.clone()));
                }
            }

            if !details.documents.is_empty() {
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled(
                    "Documents",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));

                for document in &details.documents {
                    lines.push(Line::from(format!("{} - {}", document.label, document.url)));
                }
            }

            lines
        }
        Some(DetailState::Loading) | None => {
            vec![Line::from("Loading lot photos and listing details...")]
        }
        Some(DetailState::Failed(error)) => {
            vec![Line::from(format!(
                "Could not load listing details: {error}"
            ))]
        }
    }
}

fn detail_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{label}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_string()),
    ])
}

struct PhotoFitWidget<'a> {
    preview: &'a PhotoPreview,
    full_width: u16,
    full_height: u16,
    row_offset: u16,
}

impl Widget for PhotoFitWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let (target_width, target_pixel_height) =
            fit_photo_size(self.preview, self.full_width, self.full_height);
        let terminal_rows = target_pixel_height.div_ceil(2) as u16;
        let x_offset = 0;

        for terminal_row in 0..area.height {
            let document_row = self.row_offset.saturating_add(terminal_row);

            if document_row >= terminal_rows {
                break;
            }

            let upper_y = u32::from(document_row) * 2;
            let lower_y =
                (u32::from(document_row) * 2 + 1).min(target_pixel_height.saturating_sub(1));

            for column in 0..u32::from(target_width) {
                let upper = self.preview.sample(
                    column,
                    upper_y,
                    u32::from(target_width),
                    target_pixel_height,
                );
                let lower = self.preview.sample(
                    column,
                    lower_y,
                    u32::from(target_width),
                    target_pixel_height,
                );
                let (symbol, foreground, background) = half_block_cell(upper, lower);

                buf[(area.x + x_offset + column as u16, area.y + terminal_row)]
                    .set_symbol(symbol)
                    .set_style(Style::default().fg(foreground).bg(background));
            }
        }
    }
}

fn fit_photo_size(preview: &PhotoPreview, width: u16, height: u16) -> (u16, u32) {
    let max_pixel_width = u32::from(width).max(1);
    let max_pixel_height = u32::from(height).max(1).saturating_mul(2);
    let width_scale = max_pixel_width as f32 / preview.width.max(1) as f32;
    let height_scale = max_pixel_height as f32 / preview.height.max(1) as f32;
    let scale = width_scale.min(height_scale).max(0.01);
    let target_width = ((preview.width as f32 * scale).ceil() as u16)
        .max(1)
        .min(width.max(1));
    let target_pixel_height = ((preview.height as f32 * scale).ceil() as u32)
        .max(1)
        .min(max_pixel_height);

    (target_width, target_pixel_height)
}

fn terminal_photo_state(
    picker: &Picker,
    preview: &PhotoPreview,
    width: u16,
    height: u16,
) -> TerminalPhotoState {
    let image = image::DynamicImage::ImageRgb8(preview.image.clone());

    match SlicedProtocol::new(picker, image, Some(Size::new(width, height))) {
        Ok(protocol) => TerminalPhotoState::Loaded(protocol),
        Err(_error) => TerminalPhotoState::Failed,
    }
}

impl PhotoPreview {
    fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        self.image
            .get_pixel(x.min(self.width - 1), y.min(self.height - 1))
            .0
    }

    fn sample(
        &self,
        target_x: u32,
        target_y: u32,
        target_width: u32,
        target_height: u32,
    ) -> [u8; 3] {
        if self.width <= 1 || self.height <= 1 || target_width <= 1 || target_height <= 1 {
            return self.pixel(0, 0);
        }

        let source_x = ((target_x as f32 + 0.5) * self.width as f32 / target_width as f32 - 0.5)
            .clamp(0.0, (self.width - 1) as f32);
        let source_y = ((target_y as f32 + 0.5) * self.height as f32 / target_height as f32 - 0.5)
            .clamp(0.0, (self.height - 1) as f32);

        let x0 = source_x.floor() as u32;
        let y0 = source_y.floor() as u32;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let x_weight = source_x - x0 as f32;
        let y_weight = source_y - y0 as f32;

        let top = blend_rgb(self.pixel(x0, y0), self.pixel(x1, y0), x_weight);
        let bottom = blend_rgb(self.pixel(x0, y1), self.pixel(x1, y1), x_weight);
        blend_rgb(top, bottom, y_weight)
    }
}

fn blend_rgb(left: [u8; 3], right: [u8; 3], weight: f32) -> [u8; 3] {
    let weight = weight.clamp(0.0, 1.0);
    [
        blend_channel(left[0], right[0], weight),
        blend_channel(left[1], right[1], weight),
        blend_channel(left[2], right[2], weight),
    ]
}

fn blend_channel(left: u8, right: u8, weight: f32) -> u8 {
    (left as f32 + (right as f32 - left as f32) * weight).round() as u8
}

fn half_block_cell(upper: [u8; 3], lower: [u8; 3]) -> (&'static str, Color, Color) {
    let upper_color = Color::Rgb(upper[0], upper[1], upper[2]);
    let lower_color = Color::Rgb(lower[0], lower[1], lower[2]);

    if upper == lower {
        return (" ", upper_color, upper_color);
    }

    if luminance(lower) > luminance(upper) {
        ("▄", lower_color, upper_color)
    } else {
        ("▀", upper_color, lower_color)
    }
}

fn luminance([red, green, blue]: [u8; 3]) -> u32 {
    2126 * u32::from(red) + 7152 * u32::from(green) + 722 * u32::from(blue)
}

fn handle_mouse(app: &mut App, area: Rect, mouse: MouseEvent) {
    let layout = compute_layout(area, app);

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.drag_target = drag_target_at(&layout, mouse.column, mouse.row);
            app.resize_hover_target = app.drag_target;

            if app.drag_target.is_some() {
                if resize_drag_target(app, &layout, mouse.column, mouse.row) {
                    app.mark_pane_resize_changed();
                }
                return;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if resize_drag_target(app, &layout, mouse.column, mouse.row) {
                app.mark_pane_resize_changed();
            }
            return;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let was_dragging = app.drag_target.is_some();
            app.drag_target = None;
            app.resize_hover_target = drag_target_at(&layout, mouse.column, mouse.row);

            if was_dragging {
                app.mark_pane_resize_changed();
            }

            return;
        }
        MouseEventKind::Moved => {
            let hover_target = drag_target_at(&layout, mouse.column, mouse.row);
            if app.resize_hover_target != hover_target {
                app.resize_hover_target = hover_target;
                if let Some(target) = hover_target {
                    app.status = resize_status(target);
                }
            }

            if contains_point(layout.details, mouse.column, mouse.row) {
                app.focus_details();
            }

            if let Some(index) = hit_test_lot_index(app, layout.lots_body, mouse.column, mouse.row)
            {
                app.prefetch_lot_index(index);
            }

            return;
        }
        MouseEventKind::ScrollDown => {
            handle_mouse_scroll(app, &layout, mouse.column, mouse.row, 3);
            return;
        }
        MouseEventKind::ScrollUp => {
            handle_mouse_scroll(app, &layout, mouse.column, mouse.row, -3);
            return;
        }
        _ => return,
    }

    if contains_point(layout.details, mouse.column, mouse.row) {
        handle_details_click(app, layout.details, mouse.column, mouse.row);
        return;
    }

    if let Some(index) = hit_test_event_index(app, layout.events, mouse.column, mouse.row) {
        app.select_event(index);
        return;
    }

    if let Some(index) = hit_test_lot_index(app, layout.lots_body, mouse.column, mouse.row) {
        app.select_lot(index);
        app.open_selected_lot();
    }
}

fn handle_details_click(app: &mut App, details_area: Rect, column: u16, row: u16) {
    app.focus_details();

    if hit_test_primary_photo(app, details_area, column, row) {
        app.open_selected_lot();
    }
}

fn handle_mouse_scroll(app: &mut App, layout: &UiLayout, column: u16, row: u16, amount: i16) {
    if contains_point(layout.details, column, row) {
        app.focus_details();
        scroll_details_by(app, amount);
    }
}

fn scroll_details_by(app: &mut App, amount: i16) {
    if amount > 0 {
        app.scroll_details_down(amount as usize);
    } else if amount < 0 {
        app.scroll_details_up(amount.unsigned_abs() as usize);
    }
}

fn drag_target_at(layout: &UiLayout, column: u16, row: u16) -> Option<DragTarget> {
    if layout.wide {
        let events_boundary = layout.events.x.saturating_add(layout.events.width);

        if row >= layout.body.y
            && row < layout.body.y.saturating_add(layout.body.height)
            && near_line(column, events_boundary)
        {
            return Some(DragTarget::EventsWidth);
        }

        if column >= layout.right.x
            && column < layout.right.x.saturating_add(layout.right.width)
            && near_line(row, layout.details.y)
        {
            return Some(DragTarget::LotsDetailsSplit);
        }
    } else {
        if column >= layout.body.x
            && column < layout.body.x.saturating_add(layout.body.width)
            && near_line(row, layout.events.y.saturating_add(layout.events.height))
        {
            return Some(DragTarget::EventsWidth);
        }

        if column >= layout.body.x
            && column < layout.body.x.saturating_add(layout.body.width)
            && near_line(row, layout.details.y)
        {
            return Some(DragTarget::LotsDetailsSplit);
        }
    }

    None
}

fn resize_drag_target(app: &mut App, layout: &UiLayout, column: u16, row: u16) -> bool {
    let Some(target) = app.drag_target else {
        return false;
    };

    match (layout.wide, target) {
        (true, DragTarget::EventsWidth) => {
            let max_width = layout.body.width.saturating_sub(50).max(24);
            let new_width = column.saturating_sub(layout.body.x).clamp(24, max_width);
            let changed = app.event_pane_width != new_width;
            app.event_pane_width = new_width;
            app.status = format!("Auction pane width: {}", app.event_pane_width);
            changed
        }
        (true, DragTarget::LotsDetailsSplit) => {
            let percent = percent_from_position(row, layout.right.y, layout.right.height);
            let new_percent = percent.clamp(20, 80);
            let changed = app.lots_pane_percent != new_percent;
            app.lots_pane_percent = new_percent;
            app.status = format!("Lots/details split: {}%", app.lots_pane_percent);
            changed
        }
        (false, DragTarget::EventsWidth) => {
            let percent = percent_from_position(row, layout.body.y, layout.body.height);
            let new_percent = percent.clamp(10, 55);
            let changed = app.stacked_events_percent != new_percent;
            app.stacked_events_percent = new_percent;
            let max_lots = 80_u16.saturating_sub(app.stacked_events_percent).max(10);
            app.stacked_lots_percent = app.stacked_lots_percent.min(max_lots);
            app.status = format!("Auction pane height: {}%", app.stacked_events_percent);
            changed
        }
        (false, DragTarget::LotsDetailsSplit) => {
            let total_percent = percent_from_position(row, layout.body.y, layout.body.height);
            let lots_percent = total_percent.saturating_sub(app.stacked_events_percent);
            let max_lots = 80_u16.saturating_sub(app.stacked_events_percent).max(10);
            let new_percent = lots_percent.clamp(10, max_lots);
            let changed = app.stacked_lots_percent != new_percent;
            app.stacked_lots_percent = new_percent;
            app.status = format!("Lots pane height: {}%", app.stacked_lots_percent);
            changed
        }
    }
}

fn resize_status(target: DragTarget) -> String {
    match target {
        DragTarget::EventsWidth => "Drag highlighted boundary to resize auction pane.".to_string(),
        DragTarget::LotsDetailsSplit => {
            "Drag highlighted boundary to resize lots/details panes.".to_string()
        }
    }
}

fn percent_from_position(position: u16, start: u16, span: u16) -> u16 {
    if span == 0 {
        return 0;
    }

    (((position.saturating_sub(start) as u32) * 100) / u32::from(span)) as u16
}

fn near_line(value: u16, line: u16) -> bool {
    value.abs_diff(line) <= 1
}

fn hit_test_event_index(app: &App, area: Rect, column: u16, row: u16) -> Option<usize> {
    let inner = Block::default().borders(Borders::ALL).inner(area);

    if !contains_point(inner, column, row) {
        return None;
    }

    let relative_row = usize::from(row.saturating_sub(inner.y));
    let index = app.event_offset + (relative_row / 2);
    (index < app.events.len()).then_some(index)
}

fn hit_test_primary_photo(app: &App, details_area: Rect, column: u16, row: u16) -> bool {
    let inner = Block::default().borders(Borders::ALL).inner(details_area);
    let (image_area, _) = details_body_areas(app, inner);

    image_area
        .map(|area| contains_point(area, column, row))
        .unwrap_or(false)
}

fn hit_test_lot_index(app: &App, area: Rect, column: u16, row: u16) -> Option<usize> {
    let inner = Block::default()
        .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
        .inner(area);

    if !contains_point(inner, column, row) {
        return None;
    }

    let relative_row = usize::from(row.saturating_sub(inner.y));
    let index = app.lot_offset + relative_row;
    let lot_count = app
        .current_event()
        .map(|event| event.lots.len())
        .unwrap_or(0);
    (index < lot_count).then_some(index)
}

fn contains_point(area: Rect, column: u16, row: u16) -> bool {
    let right = area.x.saturating_add(area.width);
    let bottom = area.y.saturating_add(area.height);

    column >= area.x && column < right && row >= area.y && row < bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lot_details_photos_notes_and_documents() {
        let html = r#"
            <div class="product-gallery">
                <a data-fancybox="gallery" href="/images/repository/foo/IMG_1.JPG"><img /></a>
                <a data-fancybox="gallery" href="/images/repository/foo/IMG_1.JPG"><img /></a>
                <a class="video" data-fancybox="gallery" href="https://www.youtube.com/embed/example"><img /></a>
                <a data-fancybox="gallery" href="https://mcdougallauction.com/images/repository/foo/IMG_2.png"><img /></a>
            </div>
            <div class="bid-details">
                <p><strong>Condition:</strong> Has Key - Starts and Runs</p>
            </div>
            <div id="listing-details">
                <table>
                    <tr><td><span>Engine:</span> Audi TFSI</td></tr>
                    <tr><td><span>Damages:</span><p>Minor Wear</p></td></tr>
                </table>
                <div class="documents-wrap">
                    <a href="https://www.youtube.com/embed/example">Video</a>
                </div>
            </div>
        "#;

        let details = parse_lot_details(html).expect("details should parse");

        assert_eq!(details.photos.len(), 2);
        assert!(details.photos[0].ends_with("/images/repository/foo/IMG_1.JPG"));
        assert!(details.photos[1].ends_with("/images/repository/foo/IMG_2.png"));
        assert!(
            details
                .info_lines
                .iter()
                .any(|line| line.contains("Condition:"))
        );
        assert!(
            details
                .info_lines
                .iter()
                .any(|line| line.contains("Engine:"))
        );
        assert!(
            details
                .info_lines
                .iter()
                .any(|line| line.contains("Minor Wear"))
        );
        assert_eq!(details.documents.len(), 1);
        assert_eq!(details.documents[0].label, "Video");
    }

    #[test]
    fn parses_bid_cents() {
        assert_eq!(parse_bid_cents("$1,234.50"), Some(123450));
        assert_eq!(parse_bid_cents("$5"), Some(500));
        assert_eq!(parse_bid_cents("N/A"), None);
    }

    #[test]
    fn parses_lot_thumbnail_from_auction_listing() {
        let html = r#"
            <div class="auction-product-item">
                <div class="item-img">
                    <a href="products-full-view.php?arg=abc">
                        <img src="/images/repository/foo/Thumbs/large_item.JPG">
                    </a>
                </div>
                <div class="item-title">
                    <h4><a href="products-full-view.php?arg=abc">Widget</a></h4>
                </div>
                <div class="item-location"><p><span>Location:</span> Saskatoon, SK</p></div>
                <div class="current-bid"><p><span>Current Bid:</span> $25.00 CAD</p></div>
                <input id="txtLotEndDateabc" value="2026-05-26T18:05:00Z">
            </div>
        "#;

        let lots = parse_lots(html).expect("lots should parse");

        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].name, "Widget");
        assert_eq!(lots[0].bid_cents, Some(2500));
        assert!(
            lots[0]
                .thumbnail_url
                .as_deref()
                .unwrap()
                .ends_with("/images/repository/foo/Thumbs/large_item.JPG")
        );
    }

    #[test]
    fn photo_fit_preserves_aspect_ratio_inside_cell() {
        let preview = PhotoPreview {
            width: 1600,
            height: 900,
            image: image::RgbImage::new(1600, 900),
        };

        assert_eq!(fit_photo_size(&preview, 32, 9), (32, 18));
        assert_eq!(fit_photo_size(&preview, 18, 5), (18, 10));
    }

    #[test]
    fn wrapped_document_height_counts_wrapped_lines() {
        let lines = vec![Line::from("short"), Line::from("this line wraps")];

        assert_eq!(wrapped_document_height(&lines, 80), 2);
        assert_eq!(wrapped_document_height(&lines, 5), 4);
    }

    #[test]
    fn cleans_generic_detail_boilerplate_without_losing_useful_text() {
        let cleaned = clean_detail_line(
            "Other Details: Model: 10811KT McDougall Auctioneers Ltd. strives to represent the item(s) as visible to the naked eye. We strongly recommend manual inspection at the item(s) location. All payments must be made in full before the item(s) can be released to the purchaser. Please note that all sales are final on this auction sale. Please be advised the purchaser is held accountable for the item(s) within their purchased lot(s). The purchased lot(s) must be completely removed from its location when picked up by the purchaser. Failure to comply to the above requirements results in a mandatory disposal fee.",
        )
        .expect("useful prefix should remain");

        assert_eq!(cleaned, "Other Details: Model: 10811KT");
    }

    #[test]
    fn drops_generic_only_detail_lines() {
        assert_eq!(clean_detail_line("Item Quantity: 1"), None);
        assert_eq!(
            clean_detail_line(
                "Other Details: All payments must be made in full before the item(s) can be released to the purchaser. Please note that all sales are final on this auction sale. Please be advised the purchaser is held accountable for the item(s) within their purchased lot(s). The purchased lot(s) must be completely removed from its location when picked up by the purchaser. Failure to comply to the above requirements results in a mandatory disposal fee. From the time this unit arrived at McDougall Auction's sale location, McDougall Auction verified its specific running condition. Please note there may be minor wear and tear, such as dents, scratches, etc., that are not disclosed in this listing. There is no guarantee, representation, or warranty that the vehicle will start, run at idle, or regarding its current registration status at the time of pick-up from McDougall Auction's facility. It is the purchaser's sole responsibility to ascertain, confirm, research, inspect, and/or investigate the unit prior to bidding. Once the unit has been removed from McDougall Auction's sale location, the purchaser accepts the unit \"as is.\"",
            ),
            None
        );
    }

    #[test]
    fn disk_cache_round_trips_lot_details() {
        let cache_dir = unique_test_cache_dir("details");
        let cache = CacheStore::from_root(cache_dir.clone());
        let details = LotDetails {
            photos: vec!["https://example.com/photo.jpg".to_string()],
            info_lines: vec![
                "Item Quantity: 1".to_string(),
                "Condition: Good".to_string(),
            ],
            documents: vec![DetailLink {
                label: "Spec".to_string(),
                url: "https://example.com/spec.pdf".to_string(),
            }],
        };

        cache
            .save_lot_details(
                "https://example.com/lot",
                Some(Utc::now().timestamp() + 60),
                &details,
            )
            .expect("details should save");
        let loaded = cache
            .load_lot_details("https://example.com/lot")
            .expect("details should load")
            .expect("details should be cached");

        assert_eq!(loaded.photos, details.photos);
        assert_eq!(loaded.info_lines, vec!["Condition: Good"]);
        assert_eq!(loaded.documents[0].url, details.documents[0].url);

        let _ = fs::remove_dir_all(cache_dir);
    }

    #[test]
    fn disk_cache_round_trips_photo_preview_and_expires() {
        let cache_dir = unique_test_cache_dir("photo");
        let cache = CacheStore::from_root(cache_dir.clone());
        let preview = PhotoPreview {
            width: 2,
            height: 1,
            image: image::RgbImage::from_raw(2, 1, vec![1, 2, 3, 4, 5, 6]).unwrap(),
        };

        cache
            .save_photo_preview(
                "https://example.com/photo.jpg",
                Some(Utc::now().timestamp() + 60),
                &preview,
            )
            .expect("photo should save");
        let loaded = cache
            .load_photo_preview("https://example.com/photo.jpg")
            .expect("photo should load")
            .expect("photo should be cached");

        assert_eq!(loaded.width, 2);
        assert_eq!(loaded.height, 1);
        assert_eq!(loaded.image.as_raw(), preview.image.as_raw());

        cache
            .save_photo_preview(
                "https://example.com/expired.jpg",
                Some(Utc::now().timestamp() - 1),
                &preview,
            )
            .expect("expired photo should save");
        assert!(
            cache
                .load_photo_preview("https://example.com/expired.jpg")
                .expect("expired photo load should not fail")
                .is_none()
        );

        let _ = fs::remove_dir_all(cache_dir);
    }

    fn unique_test_cache_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "mcdougs-test-cache-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }
}
