use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use clap::Args;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use forge_core::audit::AuditReader;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::VecDeque,
    fs::File,
    io,
    time::{Duration, Instant},
};

// ─── Constants ────────────────────────────────────────────────────────

/// Latency threshold for yellow highlighting (milliseconds).
const SLOW_LATENCY_MS: u64 = 1000;
/// Fixed height of the detail panel in rows.
const DETAIL_HEIGHT: u16 = 8;
/// Latency threshold for magenta highlighting (milliseconds).
const VERY_SLOW_LATENCY_MS: u64 = 5000;
/// Number of buckets in the sparkline.
const SPARKLINE_BUCKETS: usize = 20;
/// Sliding window for event-rate calculation (seconds).
const RATE_WINDOW_SECS: f64 = 10.0;
/// Sparkline characters from low to high.
const SPARK_CHARS: &[char] = &['\u{2581}', '\u{2582}', '\u{2583}', '\u{2585}', '\u{2587}'];

// ─── Sort Mode ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    Time,
    Latency,
    Server,
}

// ─── Status Filter ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    All,
    Errors,
    Denials,
    Blocked,
}

impl StatusFilter {
    fn cycle(self) -> Self {
        match self {
            StatusFilter::All => StatusFilter::Errors,
            StatusFilter::Errors => StatusFilter::Denials,
            StatusFilter::Denials => StatusFilter::Blocked,
            StatusFilter::Blocked => StatusFilter::All,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            StatusFilter::All => "all",
            StatusFilter::Errors => "errors",
            StatusFilter::Denials => "denials",
            StatusFilter::Blocked => "blocked",
        }
    }
}

// ─── Bookmark ─────────────────────────────────────────────────────────

/// A user-placed bookmark for quick navigation.
#[derive(Debug)]
struct Bookmark {
    event_id: String,
    label: String,
    row_index: usize,
}

impl SortMode {
    fn cycle(self) -> Self {
        match self {
            SortMode::Time => SortMode::Latency,
            SortMode::Latency => SortMode::Server,
            SortMode::Server => SortMode::Time,
        }
    }
    fn label(&self) -> &'static str {
        match self {
            SortMode::Time => "time",
            SortMode::Latency => "latency",
            SortMode::Server => "server",
        }
    }
}

// ─── CLI Arguments ────────────────────────────────────────────────────

#[derive(Debug, Args)]
#[command(about = "Live-view MCP tool calls in the terminal")]
pub struct Watch {
    #[arg(long, short, help = "Filter by server name")]
    pub server: Option<String>,

    #[arg(long, short = 't', help = "Filter by tool name")]
    pub tool: Option<String>,

    #[arg(long, short = 'e', help = "Only show errors / denials")]
    pub errors: bool,

    #[arg(long, default_value_t = 500, help = "Poll interval in milliseconds")]
    pub interval: u64,
}

// ─── Application State ────────────────────────────────────────────────

enum InputMode {
    Normal,
    Search,
    Help,
}

struct App {
    reader: AuditReader,
    poll_timeout_ms: u64,
    events: Vec<forge_core::audit::AuditRecord>,
    latest_ts: i64,
    filter_server: Option<String>,
    filter_tool: Option<String>,
    filter_errors: bool,
    /// Live text search (fuzzy match against server, tool, args).
    search_text: String,
    /// Status filter cycle (all/errors/denials/blocked).
    status_filter: StatusFilter,
    /// Bookmark list for navigating to specific events.
    bookmarks: Vec<Bookmark>,
    state: TableState,
    selected: usize,
    quit: bool,
    paused: bool,
    follow: bool,
    sort_mode: SortMode,
    message: Option<String>,
    /// Sliding window of event timestamps for rate calculation.
    event_times: VecDeque<Instant>,
    input_mode: InputMode,
    /// Running count of events that passed CLI/status filters before search filter.
    pre_filter_count: usize,
}

/// RAII guard that restores the terminal on drop, regardless of whether
/// `run()` returns normally or exits via `?`.
struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

impl Watch {
    pub fn run(self) -> Result<()> {
        enable_raw_mode()?;
        let _guard = RawModeGuard;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let reader = AuditReader::open_default().context("failed to open audit database")?;

        let mut app = App {
            reader,
            poll_timeout_ms: self.interval,
            events: Vec::new(),
            latest_ts: 0,
            filter_server: self.server.clone(),
            filter_tool: self.tool.clone(),
            filter_errors: self.errors,
            search_text: String::new(),
            status_filter: StatusFilter::All,
            bookmarks: Vec::new(),
            state: TableState::default(),
            selected: 0,
            quit: false,
            paused: false,
            follow: true,
            sort_mode: SortMode::Time,
            message: None,
            event_times: VecDeque::new(),
            input_mode: InputMode::Normal,
            pre_filter_count: 0,
        };

        app.poll();
        app.state.select(Some(0));

        while !app.quit {
            if app.follow && !app.events.is_empty() {
                app.selected = app.events.len() - 1;
                app.state.select(Some(app.selected));
            }

            terminal.draw(|f| app.render(f))?;

            let poll_ms = if matches!(app.input_mode, InputMode::Search) {
                100
            } else {
                app.poll_timeout_ms.min(500)
            };

            if event::poll(Duration::from_millis(poll_ms))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match app.input_mode {
                            InputMode::Normal => app.handle_normal_key(key),
                            InputMode::Search => app.handle_search_key(key),
                            InputMode::Help => {
                                // Any key dismisses help overlay.
                                app.input_mode = InputMode::Normal;
                            }
                        }
                    }
                }
            }

            if !app.paused {
                app.poll();
            }

            if app.message.is_some() {
                app.message = None;
            }
        }

        terminal.show_cursor().ok();
        Ok(())
        // _guard drops here: disable_raw_mode + LeaveAlternateScreen + DisableMouseCapture
    }
}

// ─── Key Handlers ─────────────────────────────────────────────────────

impl App {
    fn handle_normal_key(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            // Navigation
            KeyCode::Down => {
                self.follow = false;
                self.scroll_down();
            }
            KeyCode::Up => {
                self.follow = false;
                self.scroll_up();
            }
            KeyCode::PageDown => {
                self.follow = false;
                self.scroll_page_down();
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll_page_up();
            }
            KeyCode::Home => {
                self.follow = false;
                self.selected = 0;
                self.state.select(Some(0));
            }
            KeyCode::End => {
                self.follow = false;
                self.selected = self.events.len().saturating_sub(1);
                self.state.select(Some(self.selected));
            }

            // Actions
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('p') => {
                self.paused = !self.paused;
                let msg = if self.paused { "paused" } else { "resumed" };
                self.message = Some(msg.to_string());
            }
            KeyCode::Tab => {
                self.sort_mode = self.sort_mode.cycle();
                self.sort_and_reselect();
                self.message = Some(format!("sort: {}", self.sort_mode.label()));
            }
            KeyCode::Char('/') => {
                self.follow = !self.follow;
                let msg = if self.follow {
                    "follow on"
                } else {
                    "follow off"
                };
                self.message = Some(msg.to_string());
            }
            KeyCode::Char('f') => {
                self.input_mode = InputMode::Search;
                self.search_text.clear();
            }
            KeyCode::Char('e') => {
                self.filter_errors = !self.filter_errors;
                self.events.clear();
                self.latest_ts = 0;
                self.selected = 0;
                self.message = Some("errors-only toggled".into());
                self.poll();
            }
            KeyCode::Char('x') => {
                if let Some(path) = self.export_json() {
                    self.message = Some(format!("exported: {}", path));
                }
            }
            KeyCode::Char('v') => {
                if let Some(path) = self.export_csv() {
                    self.message = Some(format!("exported: {}", path));
                }
            }
            KeyCode::Char('C') => {
                // Uppercase C clears events and resets poll cursor.
                self.events.clear();
                self.latest_ts = 0;
                self.selected = 0;
                self.event_times.clear();
                self.message = Some("events cleared".into());
            }
            KeyCode::Char('?') => {
                self.input_mode = InputMode::Help;
            }
            KeyCode::Char('g') => {
                // 'g' jumps to top of list.
                self.follow = false;
                self.selected = 0;
                self.state.select(Some(0));
            }
            KeyCode::Char('G') => {
                // 'G' jumps to bottom.
                self.follow = false;
                self.selected = self.events.len().saturating_sub(1);
                self.state.select(Some(self.selected));
            }
            // Status filter cycle (s key).
            KeyCode::Char('s') => {
                self.status_filter = self.status_filter.cycle();
                self.message = Some(format!("filter: {}", self.status_filter.label()));
                self.apply_filters_and_sort();
            }
            // Bookmark current row (m key).
            KeyCode::Char('m') => {
                if let Some(e) = self.events.get(self.selected) {
                    let label = format!("{}__{}", e.server, e.tool);
                    // Update existing or create new.
                    if let Some(bm) = self
                        .bookmarks
                        .iter_mut()
                        .find(|b| b.row_index == self.selected)
                    {
                        bm.event_id = e.id.clone();
                        bm.label = label;
                    } else {
                        self.bookmarks.push(Bookmark {
                            event_id: e.id.clone(),
                            label,
                            row_index: self.selected,
                        });
                    }
                    self.message = Some(format!(
                        "bookmarked: {} ({})",
                        self.selected + 1,
                        self.bookmarks.len()
                    ));
                }
            }
            // Next bookmark (n key).
            KeyCode::Char('n') => {
                if let Some(current) = self
                    .bookmarks
                    .iter()
                    .position(|b| b.row_index == self.selected)
                {
                    let next = (current + 1) % self.bookmarks.len();
                    self.selected = self.bookmarks[next]
                        .row_index
                        .min(self.events.len().saturating_sub(1));
                    self.state.select(Some(self.selected));
                    self.message = Some(format!("bookmark {}/{}", next + 1, self.bookmarks.len()));
                } else if !self.bookmarks.is_empty() {
                    self.selected = self.bookmarks[0]
                        .row_index
                        .min(self.events.len().saturating_sub(1));
                    self.state.select(Some(self.selected));
                    self.message = Some(format!("bookmark 1/{}", self.bookmarks.len()));
                }
            }
            // Clear bookmarks (B key).
            KeyCode::Char('B') => {
                self.bookmarks.clear();
                self.message = Some("bookmarks cleared".into());
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                // Re-apply search filtering.
                self.apply_filters_and_sort();
            }
            KeyCode::Char(c) => {
                self.search_text.push(c);
                self.apply_filters_and_sort();
            }
            KeyCode::Backspace => {
                self.search_text.pop();
                self.apply_filters_and_sort();
            }
            KeyCode::Delete => {
                self.search_text.clear();
                self.apply_filters_and_sort();
            }
            _ => {}
        }
    }

    fn apply_filters_and_sort(&mut self) {
        // Reset to full dataset, then re-poll and re-filter.
        self.events.clear();
        self.latest_ts = 0;
        self.pre_filter_count = 0;
        self.poll();
    }

    // ─── Polling ──────────────────────────────────────────────────────

    fn poll(&mut self) {
        match self.reader.poll_new(self.latest_ts, 200) {
            Ok(mut new_events) => {
                if new_events.is_empty() {
                    return;
                }
                if let Some(ts) = new_events.last().map(|e| e.ts) {
                    self.latest_ts = ts;
                }

                // Apply CLI filters.
                if self.filter_errors {
                    new_events.retain(|e| e.result_code != 0);
                }
                if let Some(ref server) = self.filter_server {
                    new_events.retain(|e| e.server == *server);
                }
                if let Some(ref tool) = self.filter_tool {
                    new_events.retain(|e| e.tool == *tool);
                }
                // Apply status filter.
                match self.status_filter {
                    StatusFilter::All => {}
                    StatusFilter::Errors => {
                        new_events.retain(|e| e.result_code != 0);
                    }
                    StatusFilter::Denials => {
                        new_events.retain(|e| e.result_code == -403);
                    }
                    StatusFilter::Blocked => {
                        new_events.retain(|e| e.result_code == -32002);
                    }
                }

                // Count events that survived CLI/status filters (denominator for search title).
                let pre_search_count = new_events.len();

                // Apply live search.
                if !self.search_text.is_empty() {
                    let needle = self.search_text.to_lowercase();
                    new_events.retain(|e| {
                        e.server.to_lowercase().contains(&needle)
                            || e.tool.to_lowercase().contains(&needle)
                            || e.args_json
                                .as_ref()
                                .map(|a| a.to_lowercase().contains(&needle))
                                .unwrap_or(false)
                            || e.args_hash.to_lowercase().contains(&needle)
                    });
                }

                let count = new_events.len();
                self.pre_filter_count += pre_search_count;
                self.events.extend(new_events);
                self.sort_events();

                // Update rate window.
                let now = Instant::now();
                for _ in 0..count {
                    self.event_times.push_back(now);
                }
                // Prune events older than the rate window.
                let cutoff = now - Duration::from_secs_f64(RATE_WINDOW_SECS);
                while self
                    .event_times
                    .front()
                    .map(|t| *t < cutoff)
                    .unwrap_or(false)
                {
                    self.event_times.pop_front();
                }
            }
            Err(e) => {
                tracing::warn!("watch poll error: {}", e);
            }
        }
    }

    // ─── Sorting ─────────────────────────────────────────────────────

    fn sort_events(&mut self) {
        match self.sort_mode {
            SortMode::Time => self.events.sort_by_key(|e| e.ts),
            SortMode::Latency => {
                self.events.sort_by(|a, b| b.latency_ms.cmp(&a.latency_ms));
            }
            SortMode::Server => self
                .events
                .sort_by(|a, b| a.server.cmp(&b.server).then_with(|| a.tool.cmp(&b.tool))),
        }
    }

    fn sort_and_reselect(&mut self) {
        let old_id = self.events.get(self.selected).map(|e| e.id.clone());
        self.sort_events();
        if let Some(id) = old_id {
            if let Some(idx) = self.events.iter().position(|e| e.id == id) {
                self.selected = idx;
                self.state.select(Some(self.selected));
                return;
            }
        }
        self.selected = self.selected.min(self.events.len().saturating_sub(1));
        self.state.select(Some(self.selected));
    }

    // ─── Scrolling ───────────────────────────────────────────────────

    fn scroll_down(&mut self) {
        if self.selected < self.events.len().saturating_sub(1) {
            self.selected += 1;
            self.state.select(Some(self.selected));
        }
    }

    fn scroll_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.state.select(Some(self.selected));
        }
    }

    fn scroll_page_down(&mut self) {
        let page = 10;
        self.selected = (self.selected + page).min(self.events.len().saturating_sub(1));
        self.state.select(Some(self.selected));
    }

    fn scroll_page_up(&mut self) {
        let page = 10;
        self.selected = self.selected.saturating_sub(page);
        self.state.select(Some(self.selected));
    }

    // ─── Export ───────────────────────────────────────────────────────

    fn export_json(&mut self) -> Option<String> {
        if self.events.is_empty() {
            self.message = Some("nothing to export".into());
            return None;
        }
        let filename = format!("forge_events_{}.json", Utc::now().format("%Y%m%d_%H%M%S"));
        let path = std::env::current_dir().ok()?.join(&filename);
        match File::create(&path) {
            Ok(mut f) => {
                if serde_json::to_writer_pretty(&mut f, &self.events).is_ok() {
                    return Some(filename);
                }
            }
            Err(e) => {
                tracing::warn!("export failed: {}", e);
            }
        }
        None
    }

    fn export_csv(&mut self) -> Option<String> {
        if self.events.is_empty() {
            self.message = Some("nothing to export".into());
            return None;
        }
        let filename = format!("forge_events_{}.csv", Utc::now().format("%Y%m%d_%H%M%S"));
        let path = std::env::current_dir().ok()?.join(&filename);
        let mut w = csv::WriterBuilder::new().from_path(&path).ok()?;
        w.write_record([
            "timestamp",
            "server",
            "tool",
            "result_code",
            "latency_ms",
            "error",
            "args_json",
        ])
        .ok()?;
        for e in &self.events {
            let ts_str = format_timestamp(e.ts);
            w.write_record(&[
                ts_str,
                e.server.clone(),
                e.tool.clone(),
                e.result_code.to_string(),
                e.latency_ms.to_string(),
                e.error.clone().unwrap_or_default(),
                e.args_json
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| e.args_hash.clone()),
            ])
            .ok()?;
        }
        w.flush().ok()?;
        Some(filename)
    }
}

// ─── Rendering ────────────────────────────────────────────────────────

impl App {
    fn render(&mut self, f: &mut Frame) {
        let title_h: u16 = 3;
        let stats_h: u16 = 3;
        let has_detail = !self.events.is_empty() && self.selected < self.events.len();

        // When in search mode, add a search input bar.
        let search_h: u16 = match self.input_mode {
            InputMode::Search => 1,
            _ => 0,
        };

        let mut constraints = vec![
            Constraint::Length(title_h),
            Constraint::Length(stats_h),
        ];
        if search_h > 0 {
            constraints.push(Constraint::Length(search_h));
        }
        constraints.push(Constraint::Min(1));
        if has_detail {
            constraints.push(Constraint::Length(DETAIL_HEIGHT));
        }

        let chunks = Layout::default().constraints(constraints).split(f.area());

        let mut ci = 0;
        self.render_title(f, chunks[ci]);
        ci += 1;
        self.render_stats(f, chunks[ci]);
        ci += 1;

        if search_h > 0 {
            self.render_search_bar(f, chunks[ci]);
            ci += 1;
        }

        self.render_table(f, chunks[ci]);

        if has_detail {
            self.render_detail_panel(f, chunks[ci + 1]);
        }

        // Help overlay.
        if matches!(self.input_mode, InputMode::Help) {
            self.render_help_overlay(f, f.area());
        }
    }

    fn render_title(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![Span::styled(
            " forge ",
            Style::default().fg(Color::White).bg(Color::Blue),
        )];
        spans.push(Span::raw(" live mcp traffic monitor"));

        if self.paused {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                " PAUSED ",
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));
        }
        if self.follow {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                " FOLLOW ",
                Style::default().fg(Color::Black).bg(Color::Green),
            ));
        }

        let filters = self.active_filters();
        if !filters.is_empty() {
            spans.push(Span::raw("  |"));
            for s in &filters {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(s.as_str(), Style::default().fg(Color::Yellow)));
            }
        }

        spans.push(Span::raw("  sort="));
        spans.push(Span::styled(
            self.sort_mode.label(),
            Style::default().fg(Color::Cyan),
        ));

        if !self.search_text.is_empty() {
            spans.push(Span::raw("  search="));
            spans.push(Span::styled(
                &self.search_text,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        spans.push(Span::raw("  |  "));
        let hint = if let Some(ref msg) = self.message {
            Span::styled(
                msg.to_string(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            let rate = self.current_rate();
            let rate_span = format!("{:.1}/s", rate);
            let mut hint_text =
                "q=quit  e=errors  p=pause  Tab=sort  /=follow  f=search  x=JSON  v=CSV  ?=help  g/G=top/bot".to_string();
            hint_text.push_str(&format!("  rate:{}", rate_span));
            Span::styled(hint_text, Style::default().fg(Color::DarkGray))
        };
        spans.push(hint);

        let title = Paragraph::new(Line::from(spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" forge watch "),
        );
        f.render_widget(title, area);
    }

    fn active_filters(&self) -> Vec<String> {
        let mut f = Vec::new();
        if let Some(ref s) = self.filter_server {
            f.push(format!("server={}", s));
        }
        if let Some(ref t) = self.filter_tool {
            f.push(format!("tool={}", t));
        }
        if self.filter_errors {
            f.push("errors-only".into());
        }
        f
    }

    fn current_rate(&self) -> f64 {
        let now = Instant::now();
        let cutoff = now - Duration::from_secs_f64(RATE_WINDOW_SECS);
        let recent = self.event_times.iter().filter(|t| **t >= cutoff).count() as f64;
        recent / RATE_WINDOW_SECS
    }

    // ─── Latency Stats ────────────────────────────────────────────────

    fn latency_percentile(sorted: &[u64], pct: f64) -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        // Nearest-rank method: ceil(p/100 * n) - 1, clamped to valid range.
        let n = sorted.len();
        let rank = ((pct / 100.0) * n as f64).ceil() as usize;
        sorted[rank.min(n) - 1]
    }

    fn render_stats(&self, f: &mut Frame, area: Rect) {
        let total = self.events.len();
        let errors = self.events.iter().filter(|e| e.result_code != 0).count();
        let denials = self.events.iter().filter(|e| e.result_code == -403).count();
        let blocked = self
            .events
            .iter()
            .filter(|e| e.result_code == -32002)
            .count();

        // Latency distribution.
        let mut lats: Vec<u64> = self.events.iter().map(|e| e.latency_ms).collect();
        lats.sort_unstable();
        let avg_lat = if lats.is_empty() {
            0.0
        } else {
            lats.iter().map(|&l| l as f64).sum::<f64>() / lats.len() as f64
        };
        let p50 = Self::latency_percentile(&lats, 50.0);
        let p95 = Self::latency_percentile(&lats, 95.0);
        let p99 = Self::latency_percentile(&lats, 99.0);
        let min_lat = lats.first().copied().unwrap_or(0);
        let max_lat = lats.last().copied().unwrap_or(0);

        // Sparkline.
        let sparkline = self.build_sparkline();

        let mut line_spans = vec![
            Span::styled(format!("n={}", total), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled(
                format!("err={}", errors),
                if errors > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw("  "),
            Span::styled(
                format!("deny={}", denials),
                if denials > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw("  "),
            Span::styled(
                format!("block={}", blocked),
                if blocked > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::raw("  "),
            Span::styled(
                format!("avg={:.0}ms", avg_lat),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled(
                format!("p50={} p95={} p99={}", p50, p95, p99),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("  "),
            Span::styled(
                format!("min={}/max={}", min_lat, max_lat),
                Style::default().fg(Color::Yellow),
            ),
        ];

        if !sparkline.is_empty() {
            line_spans.push(Span::raw("  "));
            line_spans.push(Span::styled(sparkline, Style::default().fg(Color::Green)));
        }

        line_spans.push(Span::raw("  "));
        line_spans.push(Span::styled(
            format!("{}", Utc::now().format("%H:%M:%S")),
            Style::default().fg(Color::DarkGray),
        ));

        let text = Text::from(vec![Line::from(line_spans)]);
        let stats =
            Paragraph::new(text).block(Block::default().borders(Borders::ALL).title(" stats "));
        f.render_widget(stats, area);
    }

    fn build_sparkline(&self) -> String {
        if self.events.len() < 2 {
            return String::new();
        }
        let n = self.events.len().min(SPARKLINE_BUCKETS);
        let mut buckets = vec![0u64; n];
        for (i, e) in self.events.iter().enumerate() {
            let bucket = (i * n) / self.events.len();
            buckets[bucket] += e.latency_ms;
        }
        let max_val = buckets.iter().copied().max().unwrap_or(1);
        if max_val == 0 {
            return SPARK_CHARS[0].to_string().repeat(n);
        }
        buckets
            .iter()
            .map(|&v| {
                let idx =
                    ((v as f64 / max_val as f64) * (SPARK_CHARS.len() - 1) as f64).round() as usize;
                SPARK_CHARS[idx.min(SPARK_CHARS.len() - 1)]
            })
            .collect()
    }

    fn render_search_bar(&self, f: &mut Frame, area: Rect) {
        let text = format!(" / {}", self.search_text);
        let bar = Paragraph::new(Span::styled(
            text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        f.render_widget(bar, area);
    }

    // ─── Table ────────────────────────────────────────────────────────

    fn render_table(&mut self, f: &mut Frame, area: Rect) {
        let table_title = if self.search_text.is_empty() {
            format!(" log  ({})", self.events.len())
        } else {
            format!(
                " log  ({}/{})",
                self.events.len(),
                self.pre_filter_count
            )
        };

        let mut header_cells = vec![
            Cell::from("time"),
            Cell::from("server"),
            Cell::from("tool"),
            Cell::from("status"),
            Cell::from("latency"),
            Cell::from("args"),
        ];
        match self.sort_mode {
            SortMode::Time => header_cells[0] = Cell::from("time*"),
            SortMode::Latency => header_cells[4] = Cell::from("latency*"),
            SortMode::Server => {
                header_cells[1] = Cell::from("server*");
                header_cells[2] = Cell::from("tool*");
            }
        }

        let header = Row::new(header_cells).style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

        let rows: Vec<Row> = self
            .events
            .iter()
            .map(|e| {
                let time_str = format_timestamp(e.ts);
                let status = status_span(e.result_code);
                let args_preview = match &e.args_json {
                    Some(json) => truncate_args(json, 50),
                    None => e.args_hash.chars().take(12).collect::<String>(),
                };
                let lat_style = latency_style(e.latency_ms);

                Row::new(vec![
                    Cell::from(time_str),
                    Cell::from(e.server.clone()),
                    Cell::from(e.tool.clone()),
                    Cell::from(status),
                    Cell::from(Span::styled(latency_cell(e.latency_ms), lat_style)),
                    Cell::from(args_preview),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Length(14),
                Constraint::Length(22),
                Constraint::Length(10),
                Constraint::Length(12),
                Constraint::Min(1),
            ],
        )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(table_title))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

        f.render_stateful_widget(table, area, &mut self.state);
    }

    // ─── Detail Panel ─────────────────────────────────────────────────

    fn render_detail_panel(&self, f: &mut Frame, area: Rect) {
        let e = &self.events[self.selected];
        let lat_style = latency_style(e.latency_ms);
        let mut lines = vec![Line::from(vec![
            Span::styled(
                format!("[{}] {}__{}", self.selected + 1, e.server, e.tool),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(format!("{}ms", e.latency_ms), lat_style),
        ])];

        if let Some(ref args) = e.args_json {
            let rendered = serde_json::from_str::<serde_json::Value>(args)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_else(|_| args.clone()))
                .unwrap_or_else(|_| args.clone());
            let available_lines = area.height.saturating_sub(3) as usize;
            for line in rendered.lines().take(available_lines) {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().fg(Color::DarkGray)),
                    Span::raw(line.to_string()),
                ]));
            }
            if rendered.lines().count() > available_lines {
                lines.push(Line::from(Span::styled(
                    "... (more below, use scroll to browse events)",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
        }

        if let Some(ref err) = e.error {
            lines.push(Line::from(vec![
                Span::styled(
                    "error: ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(err, Style::default().fg(Color::Red)),
            ]));
        }

        let detail = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" detail "));
        f.render_widget(detail, area);
    }

    // ─── Help Overlay ─────────────────────────────────────────────────

    fn render_help_overlay(&self, f: &mut Frame, area: Rect) {
        // Dim the background.
        let dark_bg = Block::default()
            .style(Style::default().bg(Color::Rgb(20, 20, 20)))
            .borders(Borders::ALL);
        f.render_widget(dark_bg, area);

        let inner = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: area.height.saturating_sub(4),
        };

        let lines = vec![
            Line::from(vec![Span::styled(
                "  forge watch  --  Key Bindings  ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  q / Esc", Style::default().fg(Color::Cyan)),
                Span::raw("    Quit"),
            ]),
            Line::from(vec![
                Span::styled("  p", Style::default().fg(Color::Cyan)),
                Span::raw("    Toggle pause / resume polling"),
            ]),
            Line::from(vec![
                Span::styled("  Tab", Style::default().fg(Color::Cyan)),
                Span::raw("    Cycle sort mode (time / latency / server)"),
            ]),
            Line::from(vec![
                Span::styled("  /", Style::default().fg(Color::Cyan)),
                Span::raw("    Toggle follow mode (auto-scroll to newest)"),
            ]),
            Line::from(vec![
                Span::styled("  f", Style::default().fg(Color::Cyan)),
                Span::raw("    Enter text search (filter events)"),
            ]),
            Line::from(vec![
                Span::styled("  Esc", Style::default().fg(Color::Cyan)),
                Span::raw("    Exit search mode"),
            ]),
            Line::from(vec![
                Span::styled("  e", Style::default().fg(Color::Cyan)),
                Span::raw("    Toggle errors-only filter"),
            ]),
            Line::from(vec![
                Span::styled("  x", Style::default().fg(Color::Cyan)),
                Span::raw("    Export events to JSON"),
            ]),
            Line::from(vec![
                Span::styled("  v", Style::default().fg(Color::Cyan)),
                Span::raw("    Export events to CSV"),
            ]),
            Line::from(vec![
                Span::styled("  C", Style::default().fg(Color::Cyan)),
                Span::raw("    Clear all events and reset"),
            ]),
            Line::from(vec![
                Span::styled("  g / G", Style::default().fg(Color::Cyan)),
                Span::raw("    Jump to top / bottom of list"),
            ]),
            Line::from(vec![
                Span::styled("  ? ", Style::default().fg(Color::Cyan)),
                Span::raw("    Show/hide this help overlay"),
            ]),
            Line::from(vec![
                Span::styled(
                    "  Up/Dn/PgUp/PgDn/Home/End",
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw("    Navigate table"),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Press any key to dismiss  |  Slow: >1s (red)  Very slow: >5s (magenta)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
        ];

        let help = Paragraph::new(Text::from(lines))
            .alignment(Alignment::Left)
            .block(
                Block::default()
                    .title(" help ")
                    .borders(Borders::ALL)
                    .style(Style::default().bg(Color::Rgb(40, 40, 40))),
            );

        f.render_widget(help, inner);
    }
}

// ─── Utility Functions ────────────────────────────────────────────────

fn format_timestamp(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap())
        .format("%H:%M:%S")
        .to_string()
}

fn status_span(code: i32) -> Span<'static> {
    match code {
        0 => Span::styled("ok", Style::default().fg(Color::Green)),
        -403 => Span::styled(
            "denied",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        -32002 => Span::styled(
            "blocked",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        -1 => Span::styled(
            "error",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        other => Span::styled(format!("rc={}", other), Style::default().fg(Color::Yellow)),
    }
}

fn truncate_args(json: &str, max: usize) -> String {
    let preview = json.chars().take(max).collect::<String>();
    if json.chars().count() > max {
        format!("{}...", preview)
    } else {
        preview
    }
}

fn latency_cell(ms: u64) -> String {
    format!("{}ms", ms)
}

fn latency_style(ms: u64) -> Style {
    if ms > VERY_SLOW_LATENCY_MS {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if ms > SLOW_LATENCY_MS {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // truncate_args
    #[test]
    fn truncate_shorter_than_max() {
        assert_eq!(truncate_args(r#"{"x":1}"#, 50), r#"{"x":1}"#);
    }

    #[test]
    fn truncate_exactly_at_max() {
        let input = "a".repeat(20);
        assert_eq!(truncate_args(&input, 20), input);
    }

    #[test]
    fn truncate_longer_than_max() {
        let input = "a".repeat(60);
        let result = truncate_args(&input, 20);
        assert_eq!(result.len(), 23);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_multibyte_chars() {
        let input = "\u{1F600}\u{1F602}\u{1F604}abc";
        let result = truncate_args(input, 3);
        assert_eq!(result, "\u{1F600}\u{1F602}\u{1F604}...");
    }

    // status_span
    #[test]
    fn status_ok() {
        assert_eq!(status_span(0).content.to_string(), "ok");
    }

    #[test]
    fn status_denied() {
        assert_eq!(status_span(-403).content.to_string(), "denied");
    }

    #[test]
    fn status_blocked() {
        assert_eq!(status_span(-32002).content.to_string(), "blocked");
    }

    #[test]
    fn status_error() {
        assert_eq!(status_span(-1).content.to_string(), "error");
    }

    #[test]
    fn status_unknown() {
        assert_eq!(status_span(-999).content.to_string(), "rc=-999");
    }

    // active_filters
    #[test]
    fn active_filters_empty() {
        let app = make_app();
        assert!(app.active_filters().is_empty());
    }

    #[test]
    fn active_filters_with_server() {
        let mut app = make_app();
        app.filter_server = Some("github".into());
        assert_eq!(app.active_filters(), vec!["server=github"]);
    }

    #[test]
    fn active_filters_combined() {
        let mut app = make_app();
        app.filter_server = Some("gh".into());
        app.filter_tool = Some("search".into());
        app.filter_errors = true;
        let f = app.active_filters();
        assert_eq!(f.len(), 3);
        assert!(f.contains(&"server=gh".to_string()));
        assert!(f.contains(&"tool=search".to_string()));
        assert!(f.contains(&"errors-only".to_string()));
    }

    // latency_cell
    #[test]
    fn latency_shows_ms() {
        assert_eq!(latency_cell(50), "50ms");
    }

    // latency_style
    #[test]
    fn latency_style_normal() {
        let s = latency_style(100);
        assert_eq!(s.fg.unwrap_or(Color::Reset), Color::Reset);
    }

    #[test]
    fn latency_style_slow() {
        let s = latency_style(2000);
        assert_eq!(s.fg.unwrap(), Color::Red);
    }

    #[test]
    fn latency_style_very_slow() {
        let s = latency_style(10000);
        assert_eq!(s.fg.unwrap(), Color::Magenta);
    }

    #[test]
    fn latency_at_slow_threshold() {
        let s = latency_style(SLOW_LATENCY_MS);
        assert_eq!(s.fg.unwrap_or(Color::Reset), Color::Reset); // exactly at threshold is not highlighted
    }

    // status filter
    #[test]
    fn status_filter_cycles() {
        assert_eq!(StatusFilter::All.cycle(), StatusFilter::Errors);
        assert_eq!(StatusFilter::Errors.cycle(), StatusFilter::Denials);
        assert_eq!(StatusFilter::Denials.cycle(), StatusFilter::Blocked);
        assert_eq!(StatusFilter::Blocked.cycle(), StatusFilter::All);
    }

    #[test]
    fn status_filter_labels() {
        assert_eq!(StatusFilter::All.label(), "all");
        assert_eq!(StatusFilter::Errors.label(), "errors");
        assert_eq!(StatusFilter::Denials.label(), "denials");
        assert_eq!(StatusFilter::Blocked.label(), "blocked");
    }

    #[test]
    fn status_filter_errors_keeps_nonzero() {
        let mut app = make_app();
        app.status_filter = StatusFilter::Errors;
        app.events = vec![
            make_record("s", "t", 0, 10, 1000),
            make_record("s", "t", -1, 10, 1001),
            make_record("s", "t", -403, 10, 1002),
        ];
        // Simulate what poll() does: retain by status_filter.
        let mut events = app.events.clone();
        events.retain(|e| e.result_code != 0);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.result_code != 0));
    }

    #[test]
    fn status_filter_denials_keeps_403() {
        let events = vec![
            make_record("s", "t", 0, 10, 1000),
            make_record("s", "t", -403, 10, 1001),
            make_record("s", "t", -32002, 10, 1002),
        ];
        let mut filtered = events.clone();
        filtered.retain(|e| e.result_code == -403);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].result_code, -403);
    }

    #[test]
    fn status_filter_blocked_keeps_32002() {
        let events = vec![
            make_record("s", "t", 0, 10, 1000),
            make_record("s", "t", -403, 10, 1001),
            make_record("s", "t", -32002, 10, 1002),
        ];
        let mut filtered = events.clone();
        filtered.retain(|e| e.result_code == -32002);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].result_code, -32002);
    }

    // sort mode
    #[test]
    fn sort_mode_cycles() {
        assert_eq!(SortMode::Time.cycle(), SortMode::Latency);
        assert_eq!(SortMode::Latency.cycle(), SortMode::Server);
        assert_eq!(SortMode::Server.cycle(), SortMode::Time);
    }

    #[test]
    fn sort_mode_labels() {
        assert_eq!(SortMode::Time.label(), "time");
        assert_eq!(SortMode::Latency.label(), "latency");
        assert_eq!(SortMode::Server.label(), "server");
    }

    // scroll boundaries
    #[test]
    fn scroll_down_at_end_does_not_crash() {
        let mut app = make_app_with_events(5);
        app.selected = 4;
        app.scroll_down();
        assert_eq!(app.selected, 4);
    }

    #[test]
    fn scroll_up_at_zero_does_not_underflow() {
        let mut app = make_app_with_events(5);
        app.selected = 0;
        app.scroll_up();
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn scroll_page_down_clamps() {
        let mut app = make_app_with_events(3);
        app.selected = 0;
        app.scroll_page_down();
        assert_eq!(app.selected, 2);
    }

    #[test]
    fn scroll_page_up_clamps() {
        let mut app = make_app_with_events(10);
        app.selected = 5;
        app.scroll_page_up();
        assert_eq!(app.selected, 0);
    }

    // initial state
    #[test]
    fn initial_state_not_paused() {
        assert!(!make_app().paused);
    }

    #[test]
    fn initial_state_following() {
        assert!(make_app().follow);
    }

    #[test]
    fn initial_search_empty() {
        assert!(make_app().search_text.is_empty());
    }

    // latency percentiles
    #[test]
    fn percentile_empty() {
        assert_eq!(App::latency_percentile(&[], 50.0), 0);
    }

    #[test]
    fn percentile_single() {
        assert_eq!(App::latency_percentile(&[42], 50.0), 42);
    }

    #[test]
    fn percentile_computed() {
        let lats = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(App::latency_percentile(&lats, 50.0), 50);
        assert_eq!(App::latency_percentile(&lats, 95.0), 100);
        assert_eq!(App::latency_percentile(&lats, 99.0), 100);
    }

    // sparkline
    #[test]
    fn sparkline_empty() {
        let app = make_app();
        assert!(app.build_sparkline().is_empty());
    }

    #[test]
    fn sparkline_produces_chars() {
        let app = make_app_with_events(20);
        let spark = app.build_sparkline();
        assert!(!spark.is_empty());
        assert_eq!(spark.chars().count(), 20);
        assert!(spark.chars().all(|c| SPARK_CHARS.contains(&c)));
    }

    // helpers
    fn make_app() -> App {
        use forge_core::audit::{AuditReader, AuditWriter};
        use tempfile::NamedTempFile;

        let tmp = NamedTempFile::new().expect("temp file");
        let _writer = AuditWriter::new(tmp.path()).expect("writer");
        let reader = AuditReader::open(tmp.path()).expect("reader");

        App {
            reader,
            poll_timeout_ms: 500,
            events: Vec::new(),
            latest_ts: 0,
            filter_server: None,
            filter_tool: None,
            filter_errors: false,
            search_text: String::new(),
            state: TableState::default(),
            selected: 0,
            quit: false,
            paused: false,
            follow: true,
            sort_mode: SortMode::Time,
            message: None,
            event_times: VecDeque::new(),
            input_mode: InputMode::Normal,
            bookmarks: Vec::new(),
            status_filter: StatusFilter::All,
            pre_filter_count: 0,
        }
    }

    fn make_record(
        server: &str,
        tool: &str,
        result_code: i32,
        latency_ms: u64,
        ts: i64,
    ) -> forge_core::audit::AuditRecord {
        forge_core::audit::AuditRecord {
            id: format!("{}-{}", server, tool),
            ts,
            server: server.to_string(),
            tool: tool.to_string(),
            args_hash: "abc123".to_string(),
            args_json: Some(r#"{"key": "value"}"#.to_string()),
            result_code,
            latency_ms,
            latency_us: Some(latency_ms.saturating_mul(1000)),
            error: None,
            session_id: None,
        }
    }

    fn make_app_with_events(count: usize) -> App {
        let mut app = make_app();
        for i in 0..count {
            app.events.push(make_record(
                "test",
                &format!("tool{}", i),
                0,
                i as u64 * 10,
                1000 + i as i64 * 100,
            ));
        }
        app
    }
}
