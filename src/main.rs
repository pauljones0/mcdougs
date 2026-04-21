use std::cmp::Ordering;
use std::collections::HashSet;
use std::io::{self, Stdout};
use std::time::Duration;

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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use reqwest::Url;
use reqwest::blocking::Client;
use scraper::{ElementRef, Html, Selector};

const BASE_URL: &str = "https://www.mcdougallauction.com/";
const EVENT_LIST_URL: &str = "https://www.mcdougallauction.com/auction-event-list.php";
const PAGE_SIZE: usize = 14;

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
}

impl std::fmt::Display for FocusPane {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Events => formatter.write_str("events"),
            Self::Lots => formatter.write_str("lots"),
        }
    }
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
    location: String,
    bid_display: String,
    bid_cents: Option<i64>,
    close_at: Option<DateTime<FixedOffset>>,
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
}

struct App {
    cli: Cli,
    events: Vec<AuctionEvent>,
    focus: FocusPane,
    event_index: usize,
    lot_index: usize,
    event_offset: usize,
    lot_offset: usize,
    status: String,
    loaded_at: DateTime<Utc>,
}

impl App {
    fn new(cli: Cli, events: Vec<AuctionEvent>) -> Self {
        let mut app = Self {
            cli,
            events,
            focus: FocusPane::Events,
            event_index: 0,
            lot_index: 0,
            event_offset: 0,
            lot_offset: 0,
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

    fn total_lots(&self) -> usize {
        self.events.iter().map(|event| event.lots.len()).sum()
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
                if let Some(event) = self.current_event() {
                    if self.lot_index + 1 < event.lots.len() {
                        self.lot_index += 1;
                    }
                }
            }
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
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            FocusPane::Events => FocusPane::Lots,
            FocusPane::Lots => FocusPane::Events,
        };
    }

    fn focus_lots(&mut self) {
        if self.current_event().is_some() {
            self.focus = FocusPane::Lots;
        }
    }

    fn refresh(&mut self, scraper: &AuctionScraper) {
        match load_data(scraper, &self.cli) {
            Ok(events) => {
                self.events = events;
                self.loaded_at = Utc::now();
                self.clamp_selection();
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

    fn select_event(&mut self, index: usize) {
        if index >= self.events.len() {
            return;
        }

        self.event_index = index;
        self.lot_index = 0;
        self.focus = FocusPane::Events;
        self.clamp_selection();

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
        self.focus = FocusPane::Lots;
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

fn parse_lots_from_html(document: Html) -> Result<Vec<Lot>> {
    let item_selector = selector(".auction-product-item")?;
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
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("failed to create terminal backend")?;

    let result = tui_loop(&mut terminal, &mut app, scraper);
    restore_terminal(&mut terminal)?;
    result
}

fn tui_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    scraper: &AuctionScraper,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Tab | KeyCode::Left | KeyCode::Right => app.toggle_focus(),
                    KeyCode::Enter => app.focus_lots(),
                    KeyCode::Char('j') | KeyCode::Down => app.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => app.move_up(),
                    KeyCode::Char('r') => app.refresh(scraper),
                    KeyCode::Home => {
                        if app.focus == FocusPane::Events {
                            app.event_index = 0;
                            app.lot_index = 0;
                        } else {
                            app.lot_index = 0;
                        }
                    }
                    KeyCode::End => match app.focus {
                        FocusPane::Events => {
                            if !app.events.is_empty() {
                                app.event_index = app.events.len() - 1;
                                app.lot_index = 0;
                            }
                        }
                        FocusPane::Lots => {
                            if let Some(event) = app.current_event() {
                                if !event.lots.is_empty() {
                                    app.lot_index = event.lots.len() - 1;
                                }
                            }
                        }
                    },
                    _ => {}
                }
            }
            Event::Mouse(mouse) => handle_mouse(app, terminal.size()?.into(), mouse),
            _ => {}
        }
    }

    Ok(())
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
    footer: Rect,
    events: Rect,
    lots_header: Rect,
    lots_body: Rect,
    details: Rect,
}

fn compute_layout(area: Rect) -> UiLayout {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(12),
            Constraint::Length(2),
        ])
        .split(area);

    if outer[1].width >= 120 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(38), Constraint::Min(60)])
            .split(outer[1]);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(10), Constraint::Length(12)])
            .split(columns[1]);
        let lots = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(right[0]);

        UiLayout {
            header: outer[0],
            footer: outer[2],
            events: columns[0],
            lots_header: lots[0],
            lots_body: lots[1],
            details: right[1],
        }
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(28),
                Constraint::Percentage(38),
                Constraint::Percentage(34),
            ])
            .split(outer[1]);
        let lots = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(rows[1]);

        UiLayout {
            header: outer[0],
            footer: outer[2],
            events: rows[0],
            lots_header: lots[0],
            lots_body: lots[1],
            details: rows[2],
        }
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let layout = compute_layout(frame.area());

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

    let border_style = if app.focus == FocusPane::Events {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

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
    let border_style = if app.focus == FocusPane::Lots {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };

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

    let items = match app.current_event() {
        Some(event) => event
            .lots
            .iter()
            .map(|lot| {
                ListItem::new(Line::from(format!(
                    "{:<18} {:<12} {}",
                    format_close_time(lot.close_at.as_ref()),
                    lot.bid_display,
                    lot.name
                )))
            })
            .collect::<Vec<_>>(),
        None => vec![ListItem::new(Line::from("No lots available."))],
    };

    let widget = List::new(items)
        .block(
            Block::default()
                .borders(Borders::BOTTOM | Borders::LEFT | Borders::RIGHT)
                .border_style(border_style),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default().with_offset(app.lot_offset);

    if let Some(event) = app.current_event() {
        if !event.lots.is_empty() {
            state.select(Some(app.lot_index));
        }
    }

    frame.render_stateful_widget(widget, body_area, &mut state);
    app.lot_offset = state.offset();
}

fn draw_details(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let lines = match (app.current_event(), app.current_lot()) {
        (Some(event), Some(lot)) => vec![
            Line::from(Span::styled(
                lot.name.clone(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            detail_line("Bid", &lot.bid_display),
            detail_line("Status", lot_status(lot)),
            detail_line("Closes", &format_close_time(lot.close_at.as_ref())),
            detail_line("Lot Location", &lot.location),
            detail_line("Auction", &event.title),
            detail_line("Auction Location", &event.location),
            detail_line("Lot URL", &lot.url),
            detail_line("Auction URL", &event.url),
        ],
        (Some(event), None) => vec![
            Line::from(Span::styled(
                event.title.clone(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            detail_line("Auction Location", &event.location),
            detail_line("Auction URL", &event.url),
            Line::from("No lot is currently selected."),
        ],
        (None, _) => vec![
            Line::from("No auction data to display."),
            Line::from("Try a different --city or --query, or press r to refresh."),
        ],
    };

    let widget = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Details"))
        .wrap(Wrap { trim: false });
    frame.render_widget(widget, area);
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let widget = Paragraph::new(vec![
        Line::from(app.status.clone()),
        Line::from("Click a lot row to open it | Click an auction to select it | Tab/Left/Right switch panes | Up/Down or j/k move | r refresh | q quit"),
    ])
    .block(Block::default().borders(Borders::ALL).title("Help"))
    .wrap(Wrap { trim: true });
    frame.render_widget(widget, area);
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

fn handle_mouse(app: &mut App, area: Rect, mouse: MouseEvent) {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }

    let layout = compute_layout(area);

    if let Some(index) = hit_test_event_index(app, layout.events, mouse.column, mouse.row) {
        app.select_event(index);
        return;
    }

    if let Some(index) = hit_test_lot_index(app, layout.lots_body, mouse.column, mouse.row) {
        app.select_lot(index);
        app.open_selected_lot();
    }
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
