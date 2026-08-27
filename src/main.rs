mod app;
mod data;
mod redis_client;
mod ui;

use anyhow::{Context, Result};
use app::{App, InputMode, Panel};
use clap::Parser;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyModifiers, MouseEvent, MouseEventKind,
    },
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::prelude::*;
use redis_client::{MultiRedisClient, RedisClient, StreamEntry};
use std::io;
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "redis-tui",
    about = "A Redis TUI client inspired by Redis Insight"
)]
struct Args {
    /// Redis host
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Redis port
    #[arg(short, long, default_value_t = 6379)]
    port: u16,

    /// Redis password
    #[arg(long)]
    password: Option<String>,

    /// Redis database number
    #[arg(short, long, default_value_t = 0)]
    db: u16,

    /// Full Redis URL (overrides host/port/password/db)
    #[arg(short, long)]
    url: Option<String>,

    /// Path to hosts file (one Redis URL per line, # for comments).
    /// Connects to all listed hosts and aggregates keys.
    #[arg(long)]
    hosts_file: Option<String>,

    /// Rolling window for ingestion rate chart history (minutes)
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    rate_history: u64,

    /// Sliding window for the plotted ingestion rate line (seconds)
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
    rate_avg_window: u64,

    /// Retry attempts for a multi-host entry that is not up yet
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(0..))]
    connect_retries: u32,

    /// Socket timeout per connection attempt in multi-host mode (seconds)
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..))]
    connect_timeout: u64,
}

impl Args {
    fn redis_url(&self) -> String {
        if let Some(url) = &self.url {
            return url.clone();
        }
        let auth = match &self.password {
            Some(pw) => format!(":{}@", pw),
            None => String::new(),
        };
        format!("redis://{}{}:{}/{}", auth, self.host, self.port, self.db)
    }
}

/// Parse a hosts file. Each line is a Redis URL. Lines starting with # are comments.
/// Blank lines are skipped. Returns (label, url) pairs.
fn parse_hosts_file(path: &str) -> Result<Vec<(String, String)>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read hosts file: {}", path))?;

    let mut hosts = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    for (i, line) in content.lines().enumerate() {
        // Physical line number, so blanks and comments above a bad entry do not
        // shift what gets reported back to the user.
        let lineno = i + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Validate with the same parser `RedisClient::connect` uses, rather than
        // a hand-rolled check that could drift from what redis-rs accepts. An
        // entry that fails here can never connect, so it must not reach the
        // retry loop in `MultiRedisClient::from_urls`.
        if let Err(e) = redis::Client::open(trimmed) {
            problems.push(format!(
                "  line {}: {}\n    {}{}",
                lineno,
                trimmed,
                e,
                scheme_hint(trimmed)
            ));
            continue;
        }

        // Use the host:port portion as a label, or fallback to line number
        let label = trimmed
            .strip_prefix("redis://")
            .and_then(|s| s.split('/').next())
            .map(|s| {
                // Remove auth portion for label
                if let Some(at_pos) = s.rfind('@') {
                    s[at_pos + 1..].to_string()
                } else {
                    s.to_string()
                }
            })
            .unwrap_or_else(|| format!("host-{}", lineno));
        hosts.push((label, trimmed.to_string()));
    }

    if !problems.is_empty() {
        anyhow::bail!(
            "Hosts file '{}' has {} invalid {}:\n{}",
            path,
            problems.len(),
            if problems.len() == 1 {
                "entry"
            } else {
                "entries"
            },
            problems.join("\n")
        );
    }

    if hosts.is_empty() {
        anyhow::bail!("Hosts file '{}' contains no valid URLs", path);
    }

    Ok(hosts)
}

/// Suggest the scheme-prefixed form when an entry looks like a bare `host:port`.
/// That is the likeliest typo, and the raw redis-rs error for it does not say so.
fn scheme_hint(entry: &str) -> String {
    if entry.contains("://") {
        return String::new();
    }
    match entry.split_once(':') {
        Some((host, port))
            if !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            format!(" - did you mean redis://{} ?", entry)
        }
        _ => String::new(),
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Connect to Redis (single or multi-host)
    let mut client = if let Some(ref hosts_path) = args.hosts_file {
        let hosts = parse_hosts_file(hosts_path)?;
        eprintln!("Connecting to {} hosts...", hosts.len());
        MultiRedisClient::from_urls(
            &hosts,
            args.connect_retries,
            Duration::from_secs(args.connect_timeout),
        )?
    } else {
        let url = args.redis_url();
        MultiRedisClient::from_single(&url)
            .with_context(|| format!("Failed to connect to Redis at {}", url))?
    };

    // Install panic hook that restores the terminal before printing the panic.
    // Only run cleanup on the main thread — a background thread panic should not
    // tear down the terminal while the main UI loop is still running.
    let main_thread_id = std::thread::current().id();
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == main_thread_id {
            let _ = disable_raw_mode();
            let _ = io::stdout().execute(DisableBracketedPaste);
            let _ = io::stdout().execute(DisableMouseCapture);
            let _ = io::stdout().execute(LeaveAlternateScreen);
        }
        original_hook(info);
    }));

    // Set up terminal
    enable_raw_mode().context("Failed to enable raw mode")?;
    io::stdout()
        .execute(EnterAlternateScreen)
        .context("Failed to enter alternate screen")?;
    io::stdout()
        .execute(EnableMouseCapture)
        .context("Failed to enable mouse capture")?;
    io::stdout()
        .execute(EnableBracketedPaste)
        .context("Failed to enable bracketed paste")?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;

    // Run app
    let result = run_app(
        &mut terminal,
        &mut client,
        args.rate_history * 60,
        args.rate_avg_window,
    );

    // Restore terminal
    disable_raw_mode().context("Failed to disable raw mode")?;
    io::stdout()
        .execute(DisableBracketedPaste)
        .context("Failed to disable bracketed paste")?;
    io::stdout()
        .execute(DisableMouseCapture)
        .context("Failed to disable mouse capture")?;
    io::stdout()
        .execute(LeaveAlternateScreen)
        .context("Failed to leave alternate screen")?;
    terminal.show_cursor().context("Failed to show cursor")?;

    result
}

/// State for managing the background XREAD thread
#[allow(dead_code)]
struct StreamListener {
    rx: mpsc::Receiver<Vec<StreamEntry>>,
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// The key this listener was started for
    watching_key: String,
}

impl StreamListener {
    fn start(url: &str, key: &str, last_id: &str, db: i64) -> Option<Self> {
        let mut client = RedisClient::connect(url).ok()?;
        if db != 0 {
            client.select_db(db).ok()?;
        }
        let (tx, rx) = mpsc::channel();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop = stop_flag.clone();
        let watching_key = key.to_string();
        let watching_id = last_id.to_string();
        let thread_key = watching_key.clone();
        let mut lid = watching_id.clone();

        let handle = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Block up to 1s so we can check the stop flag periodically
                match client.xread_blocking(&thread_key, &lid, 1000) {
                    Ok(entries) if !entries.is_empty() => {
                        if let Some(last) = entries.last() {
                            lid = last.id.clone();
                        }
                        if tx.send(entries).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Ok(_) => {} // timeout, no data
                    Err(_) => {
                        // Connection error, back off briefly
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
        });

        Some(Self {
            rx,
            stop_flag,
            handle: Some(handle),
            watching_key,
        })
    }

    // Reached only through Drop, and the test target never constructs a listener -
    // main() is replaced by the harness there - so rustc sees this as dead in that
    // build. It is live in the real binary.
    #[allow(dead_code)]
    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StreamListener {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Background thread that generates wave data and writes to a Redis stream
#[allow(dead_code)]
struct SignalGenerator {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    watching_key: String,
}

impl SignalGenerator {
    fn start(url: &str, key: &str, db: i64, config: app::SignalGenConfig) -> Option<Self> {
        let mut client = RedisClient::connect(url).ok()?;
        if db != 0 {
            client.select_db(db).ok()?;
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop = stop_flag.clone();
        let watching_key = key.to_string();
        let thread_key = watching_key.clone();
        let sleep_dur = Duration::from_secs_f64(1.0 / config.entries_per_sec);

        let handle = std::thread::spawn(move || {
            let mut time_offset: f64 = 0.0;

            while !stop.load(Ordering::Relaxed) {
                let blob = app::generate_wave_blob(&config, time_offset);
                if client.xadd_binary(&thread_key, "_", &blob).is_err() {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                // Trim stream to last 100 entries
                let _ = client.xtrim(&thread_key, 100);
                // Advance phase by freq cycles so next entry continues seamlessly
                time_offset += config.frequency;
                std::thread::sleep(sleep_dur);
            }
        });

        Some(Self {
            stop_flag,
            handle: Some(handle),
            watching_key,
        })
    }

    // Same as StreamListener::stop - live in the binary, unreachable in the test
    // target, which never constructs a generator.
    #[allow(dead_code)]
    fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SignalGenerator {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    client: &mut MultiRedisClient,
    rate_history_secs: u64,
    rate_avg_window_secs: u64,
) -> Result<()> {
    let mut app = App::new();
    app.db = client.db;
    app.host_count = client.host_count();
    app.rate_history_secs = rate_history_secs;
    app.rate_avg_window_secs = rate_avg_window_secs;

    // Initial key load
    app.refresh_keys(client);
    app.connected = client.is_connected();

    let mut stream_listeners: Vec<StreamListener> = Vec::new();
    let mut signal_generators: Vec<SignalGenerator> = Vec::new();

    let mut shift_selecting = false;

    loop {
        // Skip redraws while shift-selecting to preserve native text selection
        if !shift_selecting {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
        }

        // Poll for events with short timeout
        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;

            // Handle mouse events (shift bypass: let terminal handle native selection)
            // Shift+drag = line select, Shift+Alt+drag = block/rectangular select
            if let Event::Mouse(mouse) = ev {
                if shift_selecting {
                    // While selection is active, consume all mouse events to
                    // prevent redraws. Only a plain click (no modifiers) clears it.
                    if !mouse.modifiers.contains(KeyModifiers::SHIFT)
                        && matches!(mouse.kind, MouseEventKind::Down(_))
                    {
                        shift_selecting = false;
                        handle_mouse_event(&mut app, mouse);
                    }
                    // Otherwise: swallow the event entirely (don't let terminal see it)
                } else if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                    // Starting a new selection — only activate on drag/down
                    if matches!(
                        mouse.kind,
                        MouseEventKind::Down(_) | MouseEventKind::Drag(_)
                    ) {
                        shift_selecting = true;
                    }
                } else {
                    handle_mouse_event(&mut app, mouse);
                }
            }

            // A non-modifier key press clears selection (user is done copying)
            if let Event::Key(key) = ev {
                if key.kind == event::KeyEventKind::Press
                    && !matches!(key.code, KeyCode::Modifier(_))
                {
                    shift_selecting = false;
                }
            }

            // Handle paste events (bracketed paste from terminal)
            if let Event::Paste(data) = &ev {
                match app.input_mode {
                    InputMode::Filter => app.filter_text.push_str(data),
                    InputMode::Edit => {
                        if let Some((_label, value)) = app.edit_fields.get_mut(app.edit_focus) {
                            value.push_str(data);
                        }
                    }
                    InputMode::PlotLimit => {
                        if let Some((_label, value)) = app.edit_fields.get_mut(app.edit_focus) {
                            value.push_str(data);
                        }
                    }
                    InputMode::SignalGen => {
                        if let Some(idx) = app.signal_gen_focus.checked_sub(2) {
                            if let Some((_label, value)) = app.signal_gen_fields.get_mut(idx) {
                                value.push_str(data);
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Load value after click-to-select (needs client access)
            if app.pending_click_load {
                app.pending_click_load = false;
                app.load_selected_value(client);
            }

            if let Event::Key(key) = ev {
                // Only handle key press events — ignore release/repeat to prevent
                // input issues with crossterm 0.28+ terminal protocols
                if key.kind == event::KeyEventKind::Press {
                    match app.input_mode {
                        InputMode::Filter => handle_filter_input(&mut app, client, key.code),
                        InputMode::Confirm => handle_confirm_input(&mut app, client, key.code),
                        InputMode::Help => match key.code {
                            KeyCode::Up => {
                                app.help_scroll = app.help_scroll.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                app.help_scroll = app.help_scroll.saturating_add(1);
                            }
                            _ => {
                                app.help_scroll = 0;
                                app.input_mode = InputMode::Normal;
                            }
                        },
                        InputMode::Edit => {
                            handle_edit_input(&mut app, client, key.code, key.modifiers)
                        }
                        InputMode::PlotLimit => handle_plot_limit_input(&mut app, key.code),
                        InputMode::SignalGen => {
                            handle_signal_gen_input(&mut app, key.code);
                            // Check if user pressed Enter to start the generator
                            if app.input_mode == InputMode::Normal
                                && app.status_message == "Signal gen: starting"
                            {
                                // Parse config and start generator
                                let all_types = data::DataType::all();
                                let config = app::SignalGenConfig {
                                    wave_type: app.signal_gen_wave_type().to_string(),
                                    data_type: all_types[app.signal_gen_dtype_idx],
                                    endianness: app.endianness,
                                    frequency: app.signal_gen_fields[0]
                                        .1
                                        .trim()
                                        .parse()
                                        .unwrap_or(1.0),
                                    amplitude: app.signal_gen_fields[1]
                                        .1
                                        .trim()
                                        .parse()
                                        .unwrap_or(1.0),
                                    noise: app.signal_gen_fields[2].1.trim().parse().unwrap_or(0.0),
                                    samples_per_entry: app.signal_gen_fields[3]
                                        .1
                                        .trim()
                                        .parse()
                                        .unwrap_or(100),
                                    entries_per_sec: app.signal_gen_fields[4]
                                        .1
                                        .trim()
                                        .parse()
                                        .unwrap_or(10.0),
                                };
                                if let Some(k) = app.selected_key_name().map(|s| s.to_string()) {
                                    let key_url = client.url_for_key(&k).to_string();
                                    if let Some(sg) =
                                        SignalGenerator::start(&key_url, &k, app.db, config)
                                    {
                                        // Evict oldest if at capacity (non-blocking)
                                        if signal_generators.len() >= app::MAX_PLOT_SLOTS {
                                            let mut oldest = signal_generators.remove(0);
                                            oldest.stop_flag.store(true, Ordering::Relaxed);
                                            if let Some(h) = oldest.handle.take() {
                                                std::thread::spawn(move || {
                                                    let _ = h.join();
                                                });
                                            }
                                        }
                                        signal_generators.push(sg);
                                        app.status_message = format!(
                                            "Signal gen: running on '{}' ({}/{})",
                                            k,
                                            signal_generators.len(),
                                            app::MAX_PLOT_SLOTS
                                        );
                                    } else {
                                        app.status_message =
                                            "Signal gen: failed to start".to_string();
                                    }
                                }
                            }
                        }
                        InputMode::Normal => {
                            handle_normal_input(&mut app, client, key.code, key.modifiers);

                            // Toggle key in plot slots with 'p'
                            if key.code == KeyCode::Char('p') {
                                if let Some(k) = app.selected_key_name().map(|s| s.to_string()) {
                                    let added = app.toggle_plot_slot(&k);
                                    if added {
                                        // Fetch value directly from Redis for this key
                                        if let Ok(value) = client.get_value(&k) {
                                            app.update_slot_data(&k, &value);
                                        }
                                        app.plot_visible = true;
                                        app.status_message = format!(
                                            "Plot: added '{}' ({}/{})",
                                            k,
                                            app.plot_slots.len(),
                                            app::MAX_PLOT_SLOTS
                                        );
                                    } else {
                                        if app.plot_slots.is_empty() {
                                            app.plot_visible = false;
                                        }
                                        app.status_message = format!("Plot: removed '{}'", k);
                                    }
                                }
                            }

                            // Toggle stream listener with 'l'
                            if key.code == KeyCode::Char('l') && app.is_viewing_stream() {
                                if let Some(k) = app.selected_key_name().map(|s| s.to_string()) {
                                    // Check if already listening on this key
                                    if let Some(idx) =
                                        stream_listeners.iter().position(|sl| sl.watching_key == k)
                                    {
                                        // Stop this specific listener (non-blocking)
                                        let mut sl = stream_listeners.remove(idx);
                                        sl.stop_flag.store(true, Ordering::Relaxed);
                                        if let Some(h) = sl.handle.take() {
                                            std::thread::spawn(move || {
                                                let _ = h.join();
                                            });
                                        }
                                        app.clear_rate_tracker(&k);
                                        app.rate_view = false;
                                        app.status_message = format!("Stream: stopped '{}'", k);
                                    } else {
                                        // Start new listener
                                        let lid = app
                                            .last_stream_id
                                            .clone()
                                            .unwrap_or_else(|| "$".to_string());
                                        let key_url = client.url_for_key(&k).to_string();
                                        if let Some(sl) =
                                            StreamListener::start(&key_url, &k, &lid, app.db)
                                        {
                                            // Evict oldest if at capacity (non-blocking)
                                            if stream_listeners.len() >= app::MAX_PLOT_SLOTS {
                                                let mut oldest = stream_listeners.remove(0);
                                                oldest.stop_flag.store(true, Ordering::Relaxed);
                                                if let Some(h) = oldest.handle.take() {
                                                    std::thread::spawn(move || {
                                                        let _ = h.join();
                                                    });
                                                }
                                            }
                                            stream_listeners.push(sl);
                                            app.status_message = format!(
                                                "Stream: listening on '{}' ({}/{})",
                                                k,
                                                stream_listeners.len(),
                                                app::MAX_PLOT_SLOTS
                                            );
                                        }
                                    }
                                }
                            }

                            // Toggle signal generator with 'w'
                            if key.code == KeyCode::Char('w') {
                                if let Some(k) = app.selected_key_name().map(|s| s.to_string()) {
                                    // Check if already generating on this key
                                    if let Some(idx) =
                                        signal_generators.iter().position(|sg| sg.watching_key == k)
                                    {
                                        // Non-blocking stop
                                        let mut sg = signal_generators.remove(idx);
                                        sg.stop_flag.store(true, Ordering::Relaxed);
                                        if let Some(h) = sg.handle.take() {
                                            std::thread::spawn(move || {
                                                let _ = h.join();
                                            });
                                        }
                                        app.status_message = format!("Signal gen: stopped '{}'", k);
                                    } else if app.is_viewing_stream() {
                                        app.start_signal_gen_popup();
                                    } else {
                                        app.status_message =
                                            "Signal gen: select a stream key first".to_string();
                                    }
                                }
                            }

                            // No longer stop generators on key navigation — they run independently
                        }
                    }
                } // if KeyEventKind::Press
            }
        }

        // Check for completed background FFT
        app.poll_fft();

        // Drain new stream entries from all background listeners (bounded per tick)
        const MAX_DRAIN_PER_TICK: usize = 20;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let viewed_key = app.selected_key_name().map(|s| s.to_string());
        for listener in &stream_listeners {
            let mut total_new = 0;
            let mut drained = 0;
            while drained < MAX_DRAIN_PER_TICK {
                match listener.rx.try_recv() {
                    Ok(entries) => {
                        total_new += entries.len();
                        app.update_rate_tracker(&listener.watching_key, &entries, now_ms);
                        app.append_slot_stream_entries(&listener.watching_key, &entries);
                        // Only update main view state if this listener matches the viewed key
                        if viewed_key.as_deref() == Some(listener.watching_key.as_str()) {
                            app.append_stream_entries(entries);
                        }
                        drained += 1;
                    }
                    Err(_) => break,
                }
            }
            if total_new > 0 {
                app.status_message = format!(
                    "Stream: +{} entries on '{}' (live)",
                    total_new, listener.watching_key
                );
            }
        }

        // Sync active key indicators for UI
        app.listening_keys = stream_listeners
            .iter()
            .map(|sl| sl.watching_key.clone())
            .collect();
        app.siggen_keys = signal_generators
            .iter()
            .map(|sg| sg.watching_key.clone())
            .collect();

        if !app.running {
            // Disable mouse capture immediately so events don't queue
            // in stdin during the blocking thread joins below.
            let _ = io::stdout().execute(DisableMouseCapture);

            // Signal all threads to stop in parallel before joining
            for sg in &signal_generators {
                sg.stop_flag.store(true, Ordering::Relaxed);
            }
            for sl in &stream_listeners {
                sl.stop_flag.store(true, Ordering::Relaxed);
            }
            // Join any in-flight FFT thread
            if let Some(h) = app.fft_handle.take() {
                let _ = h.join();
            }
            drop(signal_generators);
            drop(stream_listeners);
            return Ok(());
        }
    }
}

fn handle_normal_input(
    app: &mut App,
    client: &mut MultiRedisClient,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => {
            app.running = false;
        }
        KeyCode::Char('?') => {
            app.help_scroll = 0;
            app.input_mode = InputMode::Help;
        }
        KeyCode::Tab => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                app.active_panel = app.active_panel.prev();
            } else {
                app.active_panel = app.active_panel.next();
            }
        }
        KeyCode::BackTab => {
            app.active_panel = app.active_panel.prev();
        }

        // Key list navigation — auto-load value so key info stays in sync
        KeyCode::Up if app.active_panel == Panel::KeyList => {
            app.select_prev_key();
            app.load_selected_value(client);
        }
        KeyCode::Down if app.active_panel == Panel::KeyList => {
            app.select_next_key();
            app.load_selected_value(client);
        }
        KeyCode::Enter if app.active_panel == Panel::KeyList => {
            app.load_selected_value(client);
        }

        // Value view scrolling
        KeyCode::Up if app.active_panel == Panel::ValueView => {
            app.scroll_value_up();
        }
        KeyCode::Down if app.active_panel == Panel::ValueView => {
            app.scroll_value_down();
        }

        // Data plot: arrow keys to select sub-plot when FFT is active
        KeyCode::Up if app.active_panel == Panel::DataPlot && app.fft_enabled => {
            app.plot_focus = app::PlotFocus::Signal;
        }
        KeyCode::Down if app.active_panel == Panel::DataPlot && app.fft_enabled => {
            app.plot_focus = app::PlotFocus::FFT;
        }

        // Data type / endianness — updates the selected key's plot slot if plotted
        KeyCode::Char('t') if app.active_panel == Panel::DataPlot => {
            if modifiers.contains(KeyModifiers::SHIFT) {
                app.cycle_data_type(false);
            } else {
                app.cycle_data_type(true);
            }
        }
        KeyCode::Char('T') => {
            app.cycle_data_type(false);
        }
        KeyCode::Char('e') => {
            app.toggle_endianness();
        }
        KeyCode::Char('a') => {
            app.set_auto_limits();
            app.status_message = "Plot: auto limits".to_string();
        }
        KeyCode::Char('y') => {
            // Reserved — no longer used for plot limits
        }
        KeyCode::Char('f') => {
            app.toggle_fft();
            if !app.fft_enabled {
                app.plot_focus = app::PlotFocus::Signal;
            }
            let state = if app.fft_enabled { "ON" } else { "OFF" };
            app.status_message = format!("FFT: {}", state);
        }
        KeyCode::Char('g') => {
            app.fft_log_scale = !app.fft_log_scale;
            let state = if app.fft_log_scale { "log" } else { "linear" };
            app.status_message = format!("FFT scale: {}", state);
        }

        // Data type from any panel
        KeyCode::Char('t') if app.active_panel != Panel::DataPlot => {
            app.cycle_data_type(true);
        }

        // Actions
        KeyCode::Char('/') => {
            app.input_mode = InputMode::Filter;
            app.filter_text.clear();
        }
        KeyCode::Char('r') => {
            app.refresh_keys(client);
            app.status_message = "Refreshed".to_string();
        }
        KeyCode::Char('s') => {
            if app.current_key_info.is_some() {
                app.start_edit();
            }
        }
        KeyCode::Char('n') => {
            app.start_new_key();
        }
        KeyCode::Char('x') => {
            app.start_plot_settings();
        }
        KeyCode::Char('z') => {
            if app.current_key_info.is_some() {
                app.start_set_ttl();
            }
        }
        KeyCode::Char('R') => {
            if app.current_key_info.is_some() {
                app.start_rename();
            }
        }
        KeyCode::Char('d') => {
            if let Some(key) = app.selected_key_name() {
                app.confirm_action = Some(format!("Delete key '{}'", key));
                app.input_mode = InputMode::Confirm;
            }
        }

        KeyCode::Char('i') => {
            if app.rate_view {
                // Always allow toggling off regardless of current key
                app.rate_view = false;
            } else if app.is_viewing_stream() {
                if let Some(k) = app.selected_key_name().map(|s| s.to_string()) {
                    if app.listening_keys.contains(&k) {
                        app.rate_view = true;
                    } else {
                        app.status_message = "Rate view: start listening first ([l])".to_string();
                    }
                }
            }
        }

        // Database selection
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let db = c.to_digit(10).unwrap() as i64;
            if let Err(e) = client.select_db(db) {
                app.status_message = format!("Error: {}", e);
            } else {
                app.db = db;
                app.refresh_keys(client);
                app.status_message = format!("Switched to DB {}", db);
            }
        }

        _ => {}
    }
}

fn handle_filter_input(app: &mut App, client: &mut MultiRedisClient, code: KeyCode) {
    match code {
        KeyCode::Enter => {
            app.apply_filter();
            app.refresh_keys(client);
            app.input_mode = InputMode::Normal;
            app.status_message = format!("Filter: {}", app.filter_pattern);
        }
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.filter_text.pop();
        }
        KeyCode::Char(c) => {
            app.filter_text.push(c);
        }
        _ => {}
    }
}

fn handle_confirm_input(app: &mut App, client: &mut MultiRedisClient, code: KeyCode) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            // Execute the confirmed action
            if app.confirm_action.is_some() {
                if let Some(key) = app.selected_key_name().map(|s| s.to_string()) {
                    match client.delete_key(&key) {
                        Ok(_) => {
                            app.status_message = format!("Deleted '{}'", key);
                            app.current_value = None;
                            app.current_key_info = None;
                            app.plot_data.clear();
                            app.refresh_keys(client);
                        }
                        Err(e) => {
                            app.status_message = format!("Error deleting: {}", e);
                        }
                    }
                }
            }
            app.confirm_action = None;
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.confirm_action = None;
            app.input_mode = InputMode::Normal;
        }
        _ => {}
    }
}

fn handle_edit_input(
    app: &mut App,
    client: &mut MultiRedisClient,
    code: KeyCode,
    modifiers: KeyModifiers,
) {
    let is_new_key = app.edit_operation == Some(app::EditOperation::NewKey);

    // Ctrl+B: toggle binary mode
    if code == KeyCode::Char('b') && modifiers.contains(KeyModifiers::CONTROL) {
        app.edit_binary_mode = !app.edit_binary_mode;
        let state = if app.edit_binary_mode { "ON" } else { "OFF" };
        app.status_message = format!("Binary encode: {}", state);
        return;
    }
    // Ctrl+T: cycle binary data type
    if code == KeyCode::Char('t')
        && modifiers.contains(KeyModifiers::CONTROL)
        && app.edit_binary_mode
    {
        let all = data::DataType::all();
        // Skip String and Blob types (last two)
        let max_idx = all.len() - 2;
        app.edit_binary_dtype_idx = (app.edit_binary_dtype_idx + 1) % (max_idx);
        app.status_message = format!("Binary type: {}", all[app.edit_binary_dtype_idx]);
        return;
    }
    // Ctrl+E: toggle endianness
    if code == KeyCode::Char('e')
        && modifiers.contains(KeyModifiers::CONTROL)
        && app.edit_binary_mode
    {
        app.endianness = app.endianness.toggle();
        app.status_message = format!("Endianness: {}", app.endianness);
        return;
    }

    match code {
        KeyCode::Esc => {
            let had_entries = app.edit_multi_count > 0;
            app.cancel_edit();
            if had_entries {
                // Refresh after multi-entry session
                app.refresh_keys(client);
                app.load_selected_value(client);
            }
        }
        KeyCode::Tab => {
            app.edit_next_field();
        }
        KeyCode::BackTab => {
            // Reverse tab
            if !app.edit_fields.is_empty() {
                if app.edit_focus == 0 {
                    app.edit_focus = app.edit_fields.len() - 1;
                } else {
                    app.edit_focus -= 1;
                }
            }
        }
        KeyCode::Enter => {
            match app.execute_edit(client) {
                Ok(_) => {
                    let op_label = app.edit_op_label().to_string();
                    let key = app.edit_key.clone();
                    if app.is_multi_entry_edit() {
                        // Stay open for next entry, clear fields
                        app.reset_edit_fields_for_next();
                        app.status_message = format!(
                            "{} on '{}' OK ({} added so far)",
                            op_label, key, app.edit_multi_count
                        );
                    } else {
                        // Single-entry operation, close popup
                        app.cancel_edit();
                        app.status_message = format!("{} on '{}' OK", op_label, key);
                        app.refresh_keys(client);
                        app.load_selected_value(client);
                    }
                }
                Err(e) => {
                    app.status_message = format!("Error: {}", e);
                }
            }
        }
        // Left/Right to change type for new key
        KeyCode::Left if is_new_key => {
            if app.new_key_type_idx == 0 {
                app.new_key_type_idx = app::KEY_TYPES.len() - 1;
            } else {
                app.new_key_type_idx -= 1;
            }
        }
        KeyCode::Right if is_new_key => {
            app.new_key_type_idx = (app.new_key_type_idx + 1) % app::KEY_TYPES.len();
        }
        KeyCode::Backspace => {
            if let Some((_label, value)) = app.edit_fields.get_mut(app.edit_focus) {
                value.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some((_label, value)) = app.edit_fields.get_mut(app.edit_focus) {
                value.push(c);
            }
        }
        _ => {}
    }
}

fn handle_signal_gen_input(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Tab => {
            app.signal_gen_next_field();
        }
        KeyCode::BackTab => {
            app.signal_gen_prev_field();
        }
        KeyCode::Left => match app.signal_gen_focus {
            0 => {
                if app.signal_gen_wave_idx == 0 {
                    app.signal_gen_wave_idx = app::WAVE_TYPES.len() - 1;
                } else {
                    app.signal_gen_wave_idx -= 1;
                }
            }
            1 => {
                let all = data::DataType::all();
                if app.signal_gen_dtype_idx == 0 {
                    app.signal_gen_dtype_idx = all.len() - 1;
                } else {
                    app.signal_gen_dtype_idx -= 1;
                }
            }
            _ => {}
        },
        KeyCode::Right => match app.signal_gen_focus {
            0 => {
                app.signal_gen_wave_idx = (app.signal_gen_wave_idx + 1) % app::WAVE_TYPES.len();
            }
            1 => {
                let all = data::DataType::all();
                app.signal_gen_dtype_idx = (app.signal_gen_dtype_idx + 1) % all.len();
            }
            _ => {}
        },
        KeyCode::Enter => {
            let freq: f64 = match app.signal_gen_fields[0].1.trim().parse::<f64>() {
                Ok(v) if v > 0.0 && v.is_finite() => v,
                _ => {
                    app.status_message = "Error: cycles/entry must be > 0 and finite".to_string();
                    return;
                }
            };
            let amp: f64 = match app.signal_gen_fields[1].1.trim().parse::<f64>() {
                Ok(v) if v.is_finite() => v,
                _ => {
                    app.status_message = "Error: amplitude must be a finite number".to_string();
                    return;
                }
            };
            let noise: f64 = match app.signal_gen_fields[2].1.trim().parse::<f64>() {
                Ok(v) if v >= 0.0 && v.is_finite() => v,
                _ => {
                    app.status_message = "Error: noise must be >= 0 and finite".to_string();
                    return;
                }
            };
            let samples: usize = match app.signal_gen_fields[3].1.trim().parse() {
                Ok(v) if v > 0 && v <= app::MAX_SAMPLES_PER_ENTRY => v,
                _ => {
                    app.status_message = format!(
                        "Error: samples/entry must be 1 - {}",
                        app::MAX_SAMPLES_PER_ENTRY
                    );
                    return;
                }
            };
            let rate: f64 = match app.signal_gen_fields[4].1.trim().parse::<f64>() {
                Ok(v) if v > 0.0 && v.is_finite() => v,
                _ => {
                    app.status_message = "Error: entries/sec must be > 0 and finite".to_string();
                    return;
                }
            };
            let _ = (freq, amp, noise, samples, rate);
            // Signal to the event loop to start the generator
            app.status_message = "Signal gen: starting".to_string();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            if let Some(idx) = app.signal_gen_focus.checked_sub(2) {
                if let Some((_label, value)) = app.signal_gen_fields.get_mut(idx) {
                    value.pop();
                }
            }
        }
        KeyCode::Char(c) => {
            if let Some(idx) = app.signal_gen_focus.checked_sub(2) {
                if let Some((_label, value)) = app.signal_gen_fields.get_mut(idx) {
                    value.push(c);
                }
            }
        }
        _ => {}
    }
}

fn handle_plot_limit_input(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Tab => {
            app.edit_next_field();
        }
        KeyCode::BackTab => {
            if !app.edit_fields.is_empty() {
                if app.edit_focus == 0 {
                    app.edit_focus = app.edit_fields.len() - 1;
                } else {
                    app.edit_focus -= 1;
                }
            }
        }
        KeyCode::Enter => {
            let result = app.apply_plot_settings();
            match result {
                Ok(_) => {
                    app.status_message = "Plot settings applied".to_string();
                    app.input_mode = InputMode::Normal;
                }
                Err(e) => {
                    app.status_message = format!("Error: {}", e);
                }
            }
        }
        KeyCode::Backspace => {
            if let Some((_label, value)) = app.edit_fields.get_mut(app.edit_focus) {
                value.pop();
            }
        }
        KeyCode::Char(c) => {
            if let Some((_label, value)) = app.edit_fields.get_mut(app.edit_focus) {
                value.push(c);
            }
        }
        _ => {}
    }
}

fn handle_mouse_event(app: &mut App, mouse: MouseEvent) {
    let col = mouse.column;
    let row = mouse.row;

    match mouse.kind {
        MouseEventKind::Moved | MouseEventKind::Drag(_) => {
            app.mouse_x = col;
            app.mouse_y = row;

            // Update hover data coordinates
            if let Some((dx, dy, is_fft)) = app.mouse_to_data(col, row) {
                app.hover_data_x = Some(dx);
                app.hover_data_y = Some(dy);
                app.hover_in_fft = is_fft;
            } else {
                app.hover_data_x = None;
                app.hover_data_y = None;
            }

            // Handle drag panning
            if app.mouse_dragging {
                let is_fft = app.hover_in_fft;
                let chart_area = if is_fft {
                    app.fft_chart_area
                } else {
                    app.signal_chart_area
                };
                if let Some((_cx, _cy, cw, ch)) = chart_area {
                    let dx_pixels = col as f64 - app.drag_start_x as f64;
                    let dy_pixels = row as f64 - app.drag_start_y as f64;
                    let x_range = app.drag_start_plot_x_max - app.drag_start_plot_x_min;
                    let y_range = app.drag_start_plot_y_max - app.drag_start_plot_y_min;
                    let dx_data = -dx_pixels * x_range / cw.max(1) as f64;
                    let dy_data = dy_pixels * y_range / ch.max(1) as f64;

                    if is_fft {
                        app.fft_x_min = app.drag_start_plot_x_min + dx_data;
                        app.fft_x_max = app.drag_start_plot_x_max + dx_data;
                        app.fft_y_min = app.drag_start_plot_y_min + dy_data;
                        app.fft_y_max = app.drag_start_plot_y_max + dy_data;
                        app.fft_auto_limits = false;
                    } else {
                        app.plot_x_min = app.drag_start_plot_x_min + dx_data;
                        app.plot_x_max = app.drag_start_plot_x_max + dx_data;
                        app.plot_y_min = app.drag_start_plot_y_min + dy_data;
                        app.plot_y_max = app.drag_start_plot_y_max + dy_data;
                        app.plot_auto_limits = false;
                    }
                }
            }
        }
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            // Click-to-select in key list
            if let Some((kx, ky, kw, kh)) = app.key_list_area {
                if col >= kx && col < kx + kw && row > ky && row < ky + kh.saturating_sub(1) {
                    let visible_idx = (row - ky - 1) as usize;
                    let actual_idx = visible_idx + app.key_list_state.offset();
                    if actual_idx < app.keys.len() {
                        app.key_list_state.select(Some(actual_idx));
                        app.active_panel = Panel::KeyList;
                        app.pending_click_load = true;
                    }
                    return;
                }
            }
            if app.mouse_to_data(col, row).is_some() {
                app.mouse_dragging = true;
                app.drag_start_x = col;
                app.drag_start_y = row;
                if app.hover_in_fft {
                    let (x0, x1) = app.fft_x_bounds();
                    let (y0, y1) = if app.fft_auto_limits {
                        app.auto_fft_bounds()
                    } else {
                        (app.fft_y_min, app.fft_y_max)
                    };
                    app.drag_start_plot_x_min = x0;
                    app.drag_start_plot_x_max = x1;
                    app.drag_start_plot_y_min = y0;
                    app.drag_start_plot_y_max = y1;
                } else {
                    let (x0, x1) = app.signal_x_bounds();
                    let (y0, y1) = if app.plot_auto_limits {
                        app.auto_signal_bounds()
                    } else {
                        (app.plot_y_min, app.plot_y_max)
                    };
                    app.drag_start_plot_x_min = x0;
                    app.drag_start_plot_x_max = x1;
                    app.drag_start_plot_y_min = y0;
                    app.drag_start_plot_y_max = y1;
                }
            }
        }
        MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
            app.mouse_dragging = false;
        }
        MouseEventKind::ScrollUp => {
            // Scroll key list viewport (without changing selection)
            if let Some((kx, ky, kw, kh)) = app.key_list_area {
                if col >= kx && col < kx + kw && row >= ky && row < ky + kh {
                    let offset = app.key_list_state.offset_mut();
                    *offset = offset.saturating_sub(1);
                    return;
                }
            }
            // Scroll value view
            if let Some((vx, vy, vw, vh)) = app.value_view_area {
                if col >= vx && col < vx + vw && row >= vy && row < vy + vh {
                    app.scroll_value_up();
                    return;
                }
            }
            // Zoom plot if mouse is over chart area
            if let Some((cx, cy, cw, ch)) = if app.hover_in_fft {
                app.fft_chart_area
            } else {
                app.signal_chart_area
            } {
                let frac_x = col.saturating_sub(cx) as f64 / cw.max(1) as f64;
                let frac_y = 1.0 - row.saturating_sub(cy) as f64 / ch.max(1) as f64;
                app.zoom_plot(1.3, frac_x.clamp(0.0, 1.0), frac_y.clamp(0.0, 1.0));
            }
        }
        MouseEventKind::ScrollDown => {
            // Scroll key list viewport (without changing selection)
            if let Some((kx, ky, kw, kh)) = app.key_list_area {
                if col >= kx && col < kx + kw && row >= ky && row < ky + kh {
                    let max_offset = app.keys.len().saturating_sub(1);
                    let offset = app.key_list_state.offset_mut();
                    if *offset < max_offset {
                        *offset += 1;
                    }
                    return;
                }
            }
            // Scroll value view
            if let Some((vx, vy, vw, vh)) = app.value_view_area {
                if col >= vx && col < vx + vw && row >= vy && row < vy + vh {
                    app.scroll_value_down();
                    return;
                }
            }
            // Zoom plot if mouse is over chart area
            if let Some((cx, cy, cw, ch)) = if app.hover_in_fft {
                app.fft_chart_area
            } else {
                app.signal_chart_area
            } {
                let frac_x = col.saturating_sub(cx) as f64 / cw.max(1) as f64;
                let frac_y = 1.0 - row.saturating_sub(cy) as f64 / ch.max(1) as f64;
                app.zoom_plot(1.0 / 1.3, frac_x.clamp(0.0, 1.0), frac_y.clamp(0.0, 1.0));
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_app_with_fields(freq: &str, amp: &str, noise: &str, samples: &str, rate: &str) -> App {
        let mut app = App::new();
        app.start_signal_gen_popup();
        app.signal_gen_fields[0].1 = freq.to_string();
        app.signal_gen_fields[1].1 = amp.to_string();
        app.signal_gen_fields[2].1 = noise.to_string();
        app.signal_gen_fields[3].1 = samples.to_string();
        app.signal_gen_fields[4].1 = rate.to_string();
        app
    }

    fn submit(app: &mut App) {
        handle_signal_gen_input(app, KeyCode::Enter);
    }

    #[test]
    fn valid_inputs_pass_validation() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.1", "100", "10.0");
        submit(&mut app);
        assert_eq!(app.status_message, "Signal gen: starting");
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    // --- cycles/entry (freq) ---

    #[test]
    fn zero_freq_rejected() {
        let mut app = make_app_with_fields("0", "1.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("cycles/entry"),
            "{}",
            app.status_message
        );
        assert_eq!(app.input_mode, InputMode::SignalGen);
    }

    #[test]
    fn negative_freq_rejected() {
        let mut app = make_app_with_fields("-1.0", "1.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("cycles/entry"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn infinite_freq_rejected() {
        let mut app = make_app_with_fields("inf", "1.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("cycles/entry"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn negative_infinite_freq_rejected() {
        let mut app = make_app_with_fields("-inf", "1.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("cycles/entry"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn nan_freq_rejected() {
        let mut app = make_app_with_fields("NaN", "1.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("cycles/entry"),
            "{}",
            app.status_message
        );
    }

    // --- amplitude ---

    #[test]
    fn nan_amp_rejected() {
        let mut app = make_app_with_fields("2.0", "NaN", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("amplitude"),
            "{}",
            app.status_message
        );
        assert_eq!(app.input_mode, InputMode::SignalGen);
    }

    #[test]
    fn infinite_amp_rejected() {
        let mut app = make_app_with_fields("2.0", "inf", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("amplitude"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn negative_infinite_amp_rejected() {
        let mut app = make_app_with_fields("2.0", "-inf", "0.0", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("amplitude"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn zero_amp_accepted() {
        let mut app = make_app_with_fields("2.0", "0.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert_eq!(app.status_message, "Signal gen: starting");
    }

    #[test]
    fn negative_amp_accepted() {
        let mut app = make_app_with_fields("2.0", "-1.5", "0.0", "100", "10.0");
        submit(&mut app);
        assert_eq!(app.status_message, "Signal gen: starting");
    }

    // --- noise ---

    #[test]
    fn negative_noise_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "-0.1", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("noise"),
            "{}",
            app.status_message
        );
        assert_eq!(app.input_mode, InputMode::SignalGen);
    }

    #[test]
    fn infinite_noise_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "inf", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("noise"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn negative_infinite_noise_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "-inf", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("noise"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn nan_noise_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "NaN", "100", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("noise"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn zero_noise_accepted() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "100", "10.0");
        submit(&mut app);
        assert_eq!(app.status_message, "Signal gen: starting");
    }

    // --- samples/entry ---

    #[test]
    fn zero_samples_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "0", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("samples/entry"),
            "{}",
            app.status_message
        );
        assert_eq!(app.input_mode, InputMode::SignalGen);
    }

    #[test]
    fn oversized_samples_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "1000001", "10.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("samples/entry"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn max_allowed_samples_accepted() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "1000000", "10.0");
        submit(&mut app);
        assert_eq!(app.status_message, "Signal gen: starting");
    }

    // --- entries/sec (rate) ---

    #[test]
    fn zero_rate_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "100", "0");
        submit(&mut app);
        assert!(
            app.status_message.contains("entries/sec"),
            "{}",
            app.status_message
        );
        assert_eq!(app.input_mode, InputMode::SignalGen);
    }

    #[test]
    fn negative_rate_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "100", "-1.0");
        submit(&mut app);
        assert!(
            app.status_message.contains("entries/sec"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn infinite_rate_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "100", "inf");
        submit(&mut app);
        assert!(
            app.status_message.contains("entries/sec"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn negative_infinite_rate_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "100", "-inf");
        submit(&mut app);
        assert!(
            app.status_message.contains("entries/sec"),
            "{}",
            app.status_message
        );
    }

    #[test]
    fn nan_rate_rejected() {
        let mut app = make_app_with_fields("2.0", "1.0", "0.0", "100", "NaN");
        submit(&mut app);
        assert!(
            app.status_message.contains("entries/sec"),
            "{}",
            app.status_message
        );
    }

    // ─── parse_hosts_file ────────────────────────────────────

    /// Write `content` to a temp file and parse it. Returns the parse result so a
    /// test can assert on either the hosts or the error text.
    fn parse_hosts_str(content: &str) -> Result<Vec<(String, String)>> {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "redis-tui-hosts-test-{}-{:?}.txt",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, content).unwrap();
        let result = parse_hosts_file(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        result
    }

    #[test]
    fn parse_hosts_accepts_a_valid_file_with_comments_and_blanks() {
        let hosts = parse_hosts_str(
            "# leading comment\n\nredis://127.0.0.1:6379/0\n\n# another\nredis://127.0.0.1:6380/1\n",
        )
        .expect("valid file should parse");
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].1, "redis://127.0.0.1:6379/0");
        assert_eq!(hosts[1].1, "redis://127.0.0.1:6380/1");
    }

    /// This build has no TLS: Cargo.toml declares `redis = "1.0"` with no TLS
    /// feature, so redis-rs refuses a `rediss://` URL outright. Reporting that at
    /// parse time is the point - previously the entry was accepted, then spent the
    /// whole retry budget failing to connect before being silently dropped.
    #[test]
    fn parse_hosts_rejects_rediss_because_tls_is_not_compiled_in() {
        let err = parse_hosts_str("rediss://example.com:6380/0\n")
            .expect_err("this build has no TLS support");
        let msg = err.to_string();
        assert!(msg.contains("line 1"), "should name the line: {}", msg);
        assert!(
            msg.to_lowercase().contains("tls"),
            "should say why, mentioning TLS: {}",
            msg
        );
    }

    #[test]
    fn parse_hosts_rejects_bare_host_port_with_a_scheme_hint() {
        let err = parse_hosts_str("localhost:6379\n").expect_err("bare host:port is not a URL");
        let msg = err.to_string();
        assert!(msg.contains("line 1"), "should name the line: {}", msg);
        assert!(
            msg.contains("localhost:6379"),
            "should quote the entry: {}",
            msg
        );
        assert!(
            msg.contains("redis://localhost:6379"),
            "should suggest the scheme-prefixed form: {}",
            msg
        );
    }

    #[test]
    fn parse_hosts_rejects_a_url_with_no_host() {
        let err = parse_hosts_str("redis://\n").expect_err("no host is not usable");
        assert!(err.to_string().contains("line 1"), "{}", err);
    }

    #[test]
    fn parse_hosts_rejects_outright_garbage() {
        let err = parse_hosts_str("!!! not a url at all\n").expect_err("garbage is not a URL");
        assert!(err.to_string().contains("line 1"), "{}", err);
    }

    #[test]
    fn parse_hosts_reports_every_bad_line_not_just_the_first() {
        let err = parse_hosts_str("localhost:6379\nredis://127.0.0.1:6379\nalso-bad:1\n")
            .expect_err("two bad lines");
        let msg = err.to_string();
        assert!(msg.contains("localhost:6379"), "first bad entry: {}", msg);
        assert!(msg.contains("also-bad:1"), "second bad entry: {}", msg);
    }

    #[test]
    fn parse_hosts_line_numbers_count_blanks_and_comments() {
        // The bad entry is on physical line 4; blanks and comments above it must
        // not shift the number that gets reported.
        let err = parse_hosts_str("# comment\n\n\nbad-entry:1\n").expect_err("bad entry");
        let msg = err.to_string();
        assert!(
            msg.contains("line 4"),
            "should report physical line 4, got: {}",
            msg
        );
    }

    #[test]
    fn parse_hosts_rejects_a_file_with_no_entries() {
        let err = parse_hosts_str("# only a comment\n\n").expect_err("nothing usable");
        assert!(err.to_string().contains("no valid URLs"), "{}", err);
    }

    #[test]
    fn parse_hosts_label_strips_scheme_auth_and_db() {
        let hosts = parse_hosts_str("redis://:secret@10.0.0.5:6379/2\n").unwrap();
        assert_eq!(hosts[0].0, "10.0.0.5:6379");
    }

    #[test]
    fn parse_hosts_label_for_a_url_without_auth() {
        let hosts = parse_hosts_str("redis://127.0.0.1:6379/0\n").unwrap();
        assert_eq!(hosts[0].0, "127.0.0.1:6379");
    }
}
