use crate::data::{decode_blob, encode_values, is_binary, DataType, Endianness};
use crate::redis_client::{KeyInfo, MultiRedisClient, RedisValue, StreamEntry};
use ratatui::style::Color;
use ratatui::widgets::ListState;
use rustfft::{num_complex::Complex, FftPlanner};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc;

/// Maximum number of keys that can be plotted simultaneously
pub const MAX_PLOT_SLOTS: usize = 4;

/// Maximum number of stream entries kept in memory per key.
/// Only the newest entries are needed for display (last 5 shown)
/// and plot extraction (last entry's waveform).
pub const MAX_STREAM_ENTRIES: usize = 10;
/// Default cap on collection elements fetched per value load. Overridable with
/// --max-value-items / REDIS_TUI_MAX_VALUE_ITEMS.
pub const DEFAULT_MAX_VALUE_ITEMS: usize = 1_000;

/// Colors assigned to each plot slot
pub const PLOT_COLORS: [Color; MAX_PLOT_SLOTS] =
    [Color::Cyan, Color::Yellow, Color::Green, Color::Magenta];

/// A slot in the multi-key plot FIFO
#[derive(Clone)]
pub struct PlotSlot {
    pub key_name: String,
    pub data: Vec<f64>,
    pub color: Color,
    pub data_type: DataType,
    pub endianness: Endianness,
    pub y_min: Option<f64>, // None = auto
    pub y_max: Option<f64>, // None = auto
}

/// Windows used for the multi-average display (value view + rate chart header)
pub const RATE_WINDOWS: &[(u64, &str)] =
    &[(1, "1s"), (5, "5s"), (10, "10s"), (20, "20s"), (30, "30s")];

/// How far behind the wall clock a stream's newest entry may fall before the
/// rate is labelled as lagging rather than presented as current.
pub const RATE_LAG_WARN_MS: u64 = 2_000;
/// How long (ms) a display width stays pinned after it would otherwise shrink
const RATE_WIDTH_HYSTERESIS_MS: u64 = 3_000;
/// Fractional dead band for displayed values — only update display when value moves by this fraction
const RATE_VAL_DEAD_BAND: f64 = 0.04; // 4%
/// Minimum absolute change required to update displayed value (handles near-zero rates)
const RATE_VAL_DEAD_BAND_MIN: f64 = 0.1;
/// Max time (ms) before a displayed value is forced to refresh even if inside the dead band
const RATE_VAL_STALENESS_MS: u64 = 2_000;

/// Tracks ingestion rate and gaps for a single stream key being listened to
#[derive(Clone)]
pub struct RateTracker {
    /// unix_ms timestamps of received entries within the averaging window
    pub entry_timestamps: VecDeque<u64>,
    /// (unix_ms, rate) samples for chart display, pruned to history window
    pub rate_history: VecDeque<(u64, f64)>,
    /// unix_ms timestamps where gaps were detected (possible trimmed entries)
    pub gaps: Vec<u64>,
    /// Last received entry's unix_ms timestamp (from entry ID)
    pub last_entry_ms: Option<u64>,
    /// Total entries counted since tracking started
    pub total_entries: u64,
    /// unix_ms when tracking began — used to skip warmup samples
    pub tracking_start_ms: Option<u64>,
    /// Cached five-number summary [min, q1, median, q3, max] of rate_history values
    pub five_num: Option<[f64; 5]>,
    /// unix_ms when five_num was last computed (updated at most once per second)
    pub last_five_num_ms: u64,
    /// Sticky minimum display width for rate values (chars), with hysteresis
    pub rate_display_width: usize,
    /// unix_ms after which rate_display_width may shrink
    pub rate_display_width_until_ms: u64,
    /// Sticky minimum display width for five-number summary values (chars), with hysteresis
    pub stat_display_width: usize,
    /// unix_ms after which stat_display_width may shrink
    pub stat_display_width_until_ms: u64,
    /// Displayed rate values per window (dead-band stabilised), index matches RATE_WINDOWS
    pub rate_display_vals: [f64; 5],
    /// unix_ms after which each rate display value must refresh regardless of dead band
    pub rate_display_vals_until_ms: [u64; 5],
    /// Displayed five-number summary values (dead-band stabilised): [min, q1, med, q3, max]
    pub stat_display_vals: [f64; 5],
    /// unix_ms after which each stat display value must refresh regardless of dead band
    pub stat_display_vals_until_ms: [u64; 5],
}

impl Default for RateTracker {
    fn default() -> Self {
        Self {
            entry_timestamps: VecDeque::new(),
            rate_history: VecDeque::new(),
            gaps: Vec::new(),
            last_entry_ms: None,
            total_entries: 0,
            tracking_start_ms: None,
            five_num: None,
            last_five_num_ms: 0,
            rate_display_width: 3,
            rate_display_width_until_ms: 0,
            stat_display_width: 3,
            stat_display_width_until_ms: 0,
            rate_display_vals: [0.0; 5],
            rate_display_vals_until_ms: [0; 5],
            stat_display_vals: [0.0; 5],
            stat_display_vals_until_ms: [0; 5],
        }
    }
}

/// Compute the five-number summary [min, q1, median, q3, max] from rate_history.
/// Uses linear interpolation for quartiles (same as numpy's default).
fn compute_five_num(rate_history: &VecDeque<(u64, f64)>) -> Option<[f64; 5]> {
    if rate_history.is_empty() {
        return None;
    }
    let mut vals: Vec<f64> = rate_history.iter().map(|(_, r)| *r).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    let percentile = |p: f64| -> f64 {
        if n == 1 {
            return vals[0];
        }
        let idx = p * (n - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = (idx.ceil() as usize).min(n - 1);
        let frac = idx - lo as f64;
        vals[lo] * (1.0 - frac) + vals[hi] * frac
    };
    Some([
        vals[0],
        percentile(0.25),
        percentile(0.5),
        percentile(0.75),
        vals[n - 1],
    ])
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum EditOperation {
    SetString,
    HSet,
    RPush,
    LSet,
    SAdd,
    ZAdd,
    XAdd,
    NewKey,
    SetTTL,
    RenameKey,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Panel {
    KeyList,
    ValueView,
    DataPlot,
}

impl Panel {
    pub fn next(&self) -> Panel {
        match self {
            Panel::KeyList => Panel::ValueView,
            Panel::ValueView => Panel::DataPlot,
            Panel::DataPlot => Panel::KeyList,
        }
    }

    pub fn prev(&self) -> Panel {
        match self {
            Panel::KeyList => Panel::DataPlot,
            Panel::ValueView => Panel::KeyList,
            Panel::DataPlot => Panel::ValueView,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    Normal,
    Filter,
    Confirm,
    Help,
    Edit,
    PlotLimit,
    SignalGen,
}

/// A pending confirmation, carrying the thing it is about.
///
/// The target is stored here rather than being re-read from the live selection
/// when the user answers. The selection can move while the popup is open - the
/// confirm popup is half the frame's width and does not cover the key list, and
/// mouse input is not gated by `InputMode` - so re-reading it meant `y` acted on
/// whatever happened to be selected at that moment rather than on the key the
/// prompt named.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmAction {
    DeleteKey { key: String },
}

impl ConfirmAction {
    /// The sentence shown in the confirmation popup, minus punctuation.
    pub fn prompt(&self) -> String {
        match self {
            ConfirmAction::DeleteKey { key } => format!("Delete key '{}'", key),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlotFocus {
    Signal,
    // clippy suggests `Fft`. FFT is the universal spelling for this transform in
    // signal-processing code, and it is spelled that way everywhere else here -
    // fft_enabled, fft_dirty, the [f] binding, the FFT chart title. Renaming the
    // variant alone would make it the odd one out and read worse at every call site.
    #[allow(clippy::upper_case_acronyms)]
    FFT,
}

pub const KEY_TYPES: &[&str] = &["string", "hash", "list", "set", "zset", "stream"];
pub const WAVE_TYPES: &[&str] = &["sine", "square", "sawtooth", "triangle"];

/// Maximum samples per stream entry in signal generator
pub const MAX_SAMPLES_PER_ENTRY: usize = 1_000_000;

/// Default number of data points to show in auto-range plot mode
pub const PLOT_WINDOW: usize = 2000;

impl RateTracker {
    /// Count entries with ID timestamps within the last `window_secs` seconds,
    /// expressed relative to `now_ms`. Returns entries/second.
    /// Entries per second over the last `window_secs`, measured against the
    /// stream's own newest entry rather than the wall clock.
    ///
    /// Windowing on `now_ms` made a backlog read as no traffic at all: once the
    /// drain fell behind, the newest entry processed was older than the window,
    /// every window counted zero, and the rate display reported 0.0 while the
    /// status bar printed "+20 entries (live)" (#88). Measuring on the stream's
    /// timeline reports what the data says instead.
    ///
    /// The cost is that a stopped stream keeps reporting its last rate rather
    /// than decaying to zero, so callers must show `lag_ms` alongside it.
    pub fn rate_for_window(&self, window_secs: u64, now_ms: u64) -> f64 {
        if window_secs == 0 {
            return 0.0;
        }
        let reference = self.entry_timestamps.back().copied().unwrap_or(now_ms);
        let cutoff = reference.saturating_sub(window_secs * 1000);
        let count = self
            .entry_timestamps
            .iter()
            .filter(|&&ts| ts >= cutoff)
            .count();
        count as f64 / window_secs as f64
    }

    /// How far behind wall-clock time the newest processed entry is.
    ///
    /// Zero for a tracker with no entries, and for one that is keeping up.
    pub fn lag_ms(&self, now_ms: u64) -> u64 {
        match self.entry_timestamps.back() {
            Some(&newest) => now_ms.saturating_sub(newest),
            None => 0,
        }
    }

    /// Whether the rate being displayed describes a time far enough in the past
    /// that presenting it as current would mislead.
    pub fn is_lagging(&self, now_ms: u64) -> bool {
        self.lag_ms(now_ms) >= RATE_LAG_WARN_MS
    }

    /// Update a sticky display width: grows immediately, shrinks only after RATE_WIDTH_HYSTERESIS_MS.
    fn update_sticky_width(
        pinned: &mut usize,
        pinned_until_ms: &mut u64,
        needed: usize,
        now_ms: u64,
    ) {
        if needed > *pinned {
            *pinned = needed;
            *pinned_until_ms = now_ms + RATE_WIDTH_HYSTERESIS_MS;
        } else if now_ms >= *pinned_until_ms {
            *pinned = needed;
        }
    }

    /// Update a sticky displayed value with a dead band: only updates when the new value
    /// moves outside a ±RATE_VAL_DEAD_BAND fraction of the current display value, or when
    /// the staleness timeout expires (so gradual drift still shows through).
    fn update_sticky_val(current: &mut f64, until_ms: &mut u64, new_val: f64, now_ms: u64) {
        let dead_band = (current.abs() * RATE_VAL_DEAD_BAND).max(RATE_VAL_DEAD_BAND_MIN);
        if (new_val - *current).abs() > dead_band || now_ms >= *until_ms {
            *current = new_val;
            *until_ms = now_ms + RATE_VAL_STALENESS_MS;
        }
    }
}

pub struct App {
    pub running: bool,
    pub active_panel: Panel,
    pub input_mode: InputMode,

    // Key list state
    pub keys: Vec<String>,
    pub key_types: Vec<String>,
    pub key_list_state: ListState,
    pub filter_text: String,
    pub filter_pattern: String,

    // Value display
    pub current_key_info: Option<KeyInfo>,
    pub current_value: Option<RedisValue>,
    pub value_scroll: u16,

    // Stream state
    pub expanded_stream_entries: Vec<bool>,
    pub last_stream_id: Option<String>, // for XREAD tracking

    // Data plot
    pub data_type: DataType,
    pub endianness: Endianness,
    pub plot_data: Vec<f64>, // primary key's plot data (backward compat)
    pub plot_slots: Vec<PlotSlot>, // multi-key plot FIFO (up to MAX_PLOT_SLOTS)
    pub listening_keys: Vec<String>, // keys with active stream listeners (set by main loop)
    pub siggen_keys: Vec<String>, // keys with active signal generators (set by main loop)
    pub plot_auto_limits: bool,
    pub plot_y_min: f64,
    pub plot_y_max: f64,
    pub fft_enabled: bool,
    pub fft_data: Vec<f64>,
    pub fft_computing: bool,
    pub fft_rx: Option<mpsc::Receiver<Vec<f64>>>,
    pub fft_handle: Option<std::thread::JoinHandle<()>>,
    pub fft_dirty: bool, // true if plot_data changed while FFT was in flight
    pub fft_auto_limits: bool,
    pub fft_y_min: f64,
    pub fft_y_max: f64,
    pub fft_log_scale: bool,
    pub plot_focus: PlotFocus, // which sub-plot is selected when FFT is on
    pub plot_visible: bool,
    pub help_scroll: u16,

    // Plot viewport (x-axis panning/zooming)
    pub plot_x_min: f64,
    pub plot_x_max: f64,
    pub fft_x_min: f64,
    pub fft_x_max: f64,

    // Mouse state
    pub mouse_x: u16, // terminal column
    pub mouse_y: u16, // terminal row
    pub mouse_dragging: bool,
    pub drag_start_x: u16,
    pub drag_start_y: u16,
    pub drag_start_plot_x_min: f64,
    pub drag_start_plot_x_max: f64,
    pub drag_start_plot_y_min: f64,
    pub drag_start_plot_y_max: f64,
    /// Data coordinates of hover position (if mouse is in a chart area)
    pub hover_data_x: Option<f64>,
    pub hover_data_y: Option<f64>,
    pub hover_in_fft: bool, // true if hovering in FFT chart

    // Panel area rects (set during draw)
    pub key_list_area: Option<(u16, u16, u16, u16)>, // x, y, w, h
    pub value_view_area: Option<(u16, u16, u16, u16)>, // x, y, w, h
    pub signal_chart_area: Option<(u16, u16, u16, u16)>, // x, y, w, h (inner)
    pub fft_chart_area: Option<(u16, u16, u16, u16)>,

    // Pending actions from mouse clicks (processed in main loop with client access)
    pub pending_click_load: bool,

    // Connection
    pub db: i64,
    pub db_size: i64,
    pub connected: bool,
    pub status_message: String,
    pub host_count: usize,
    pub hosts_connected: usize,
    /// Key collisions: (key_name, list_of_host_labels)
    pub collisions: Vec<(String, Vec<String>)>,
    /// Host label for the currently selected key
    pub current_key_host: Option<String>,

    // Confirmation dialog
    pub confirm_action: Option<ConfirmAction>,

    // Edit state
    pub edit_operation: Option<EditOperation>,
    pub edit_fields: Vec<(String, String)>, // (label, value)
    pub edit_focus: usize,
    pub edit_key: String,             // the key being edited
    pub edit_multi_count: usize,      // how many entries submitted in this session
    pub new_key_type_idx: usize,      // index into KEY_TYPES for new key creation
    pub edit_binary_mode: bool,       // encode values as binary blobs
    pub edit_binary_dtype_idx: usize, // index into DataType::all() for binary encoding
    /// Most collection elements fetched per value load (--max-value-items)
    pub max_value_items: usize,
    /// True length of the loaded collection, when it is one. Compared against
    /// what was actually loaded to report how much is hidden.
    pub value_total_items: Option<usize>,

    // Signal generator state
    pub signal_gen_fields: Vec<(String, String)>,
    pub signal_gen_focus: usize,
    pub signal_gen_wave_idx: usize,
    pub signal_gen_dtype_idx: usize,

    // Ingestion rate tracking
    pub rate_view: bool,
    pub rate_history_secs: u64,
    pub rate_avg_window_secs: u64,
    pub rate_trackers: HashMap<String, RateTracker>,
}

impl App {
    pub fn new() -> Self {
        Self {
            running: true,
            active_panel: Panel::KeyList,
            input_mode: InputMode::Normal,

            keys: Vec::new(),
            key_types: Vec::new(),
            key_list_state: ListState::default(),
            filter_text: String::new(),
            filter_pattern: String::from("*"),

            current_key_info: None,
            current_value: None,
            value_scroll: 0,

            expanded_stream_entries: Vec::new(),
            last_stream_id: None,

            data_type: DataType::UInt8,
            endianness: Endianness::Little,
            plot_data: Vec::new(),
            plot_slots: Vec::new(),
            listening_keys: Vec::new(),
            siggen_keys: Vec::new(),
            plot_auto_limits: true,
            plot_y_min: 0.0,
            plot_y_max: 1.0,
            fft_enabled: false,
            fft_data: Vec::new(),
            fft_computing: false,
            fft_rx: None,
            fft_handle: None,
            fft_dirty: false,
            fft_auto_limits: true,
            fft_y_min: 0.0,
            fft_y_max: 1.0,
            fft_log_scale: false,
            plot_focus: PlotFocus::Signal,
            plot_visible: false,
            help_scroll: 0,

            plot_x_min: 0.0,
            plot_x_max: 0.0, // 0 means auto (full range)
            fft_x_min: 0.0,
            fft_x_max: 0.0,

            mouse_x: 0,
            mouse_y: 0,
            mouse_dragging: false,
            drag_start_x: 0,
            drag_start_y: 0,
            drag_start_plot_x_min: 0.0,
            drag_start_plot_x_max: 0.0,
            drag_start_plot_y_min: 0.0,
            drag_start_plot_y_max: 0.0,
            hover_data_x: None,
            hover_data_y: None,
            hover_in_fft: false,

            key_list_area: None,
            value_view_area: None,
            signal_chart_area: None,
            fft_chart_area: None,
            pending_click_load: false,

            db: 0,
            db_size: 0,
            connected: false,
            status_message: String::from("Connecting..."),
            host_count: 1,
            hosts_connected: 0,
            collisions: Vec::new(),
            current_key_host: None,

            confirm_action: None,

            edit_operation: None,
            edit_fields: Vec::new(),
            edit_focus: 0,
            edit_key: String::new(),
            edit_multi_count: 0,
            new_key_type_idx: 0,
            edit_binary_mode: false,
            edit_binary_dtype_idx: 6, // Float32 default
            max_value_items: DEFAULT_MAX_VALUE_ITEMS,
            value_total_items: None,

            signal_gen_fields: Vec::new(),
            signal_gen_focus: 0,
            signal_gen_wave_idx: 0,
            signal_gen_dtype_idx: 7, // float32 index in DataType::all()

            rate_view: false,
            rate_history_secs: 1200, // 20 minutes default (overridden by CLI)
            rate_avg_window_secs: 2, // 2 seconds default (overridden by CLI)
            rate_trackers: HashMap::new(),
        }
    }

    /// Toggle a key in the plot slots. Returns true if added, false if removed.
    pub fn toggle_plot_slot(&mut self, key_name: &str) -> bool {
        // If already plotted, remove it
        if let Some(idx) = self.plot_slots.iter().position(|s| s.key_name == key_name) {
            self.plot_slots.remove(idx);
            // Reassign colors to keep them sequential
            for (i, slot) in self.plot_slots.iter_mut().enumerate() {
                slot.color = PLOT_COLORS[i];
            }
            return false;
        }
        // If at capacity, evict the oldest (first) slot
        if self.plot_slots.len() >= MAX_PLOT_SLOTS {
            self.plot_slots.remove(0);
            for (i, slot) in self.plot_slots.iter_mut().enumerate() {
                slot.color = PLOT_COLORS[i];
            }
        }
        // Add new slot
        let color = PLOT_COLORS[self.plot_slots.len()];
        self.plot_slots.push(PlotSlot {
            key_name: key_name.to_string(),
            data: Vec::new(),
            data_type: self.data_type,
            endianness: self.endianness,
            y_min: None,
            y_max: None,
            color,
        });
        true
    }

    /// Get the color assigned to a plotted key, if any
    pub fn plot_color_for_key(&self, key_name: &str) -> Option<Color> {
        self.plot_slots
            .iter()
            .find(|s| s.key_name == key_name)
            .map(|s| s.color)
    }

    /// Update plot data for a specific slot by key name, using the slot's own data type
    pub fn update_slot_data(&mut self, key_name: &str, value: &RedisValue) {
        if let Some(slot) = self.plot_slots.iter_mut().find(|s| s.key_name == key_name) {
            let dt = slot.data_type;
            let en = slot.endianness;
            slot.data = match value {
                RedisValue::String(bytes) => decode_blob(bytes, dt, en),
                RedisValue::Stream(entries) => extract_stream_plot_data(entries, dt, en),
                RedisValue::List(items) => {
                    let mut data = Vec::new();
                    for item in items {
                        if let Ok(s) = std::str::from_utf8(item) {
                            if let Ok(v) = s.parse::<f64>() {
                                data.push(v);
                                continue;
                            }
                        }
                        data.extend(decode_blob(item, dt, en));
                    }
                    data
                }
                RedisValue::ZSet(pairs) => pairs.iter().map(|(_, score)| *score).collect(),
                RedisValue::Hash(pairs) => {
                    let mut data = Vec::new();
                    for (_, val) in pairs {
                        if let Ok(s) = std::str::from_utf8(val) {
                            if let Ok(v) = s.parse::<f64>() {
                                data.push(v);
                                continue;
                            }
                        }
                        data.extend(decode_blob(val, dt, en));
                    }
                    data
                }
                _ => Vec::new(),
            };
            // Sanitize
            for v in &mut slot.data {
                if !v.is_finite() {
                    *v = 0.0;
                }
            }
        }
    }

    /// Append stream entries to a specific plot slot, using the slot's own data type
    pub fn append_slot_stream_entries(&mut self, key_name: &str, new_entries: &[StreamEntry]) {
        if let Some(slot) = self.plot_slots.iter_mut().find(|s| s.key_name == key_name) {
            let dt = slot.data_type;
            let en = slot.endianness;
            // Re-extract plot data from the newest entry
            if let Some(entry) = new_entries.last() {
                for (fname, fval) in &entry.fields {
                    if fname.starts_with('_') {
                        slot.data = decode_blob(fval, dt, en);
                        for v in &mut slot.data {
                            if !v.is_finite() {
                                *v = 0.0;
                            }
                        }
                        break;
                    }
                }
            }
        }
    }

    /// Update the ingestion rate tracker for a stream key with newly received entries.
    /// Called from the main loop drain for every active StreamListener.
    pub fn update_rate_tracker(
        &mut self,
        key_name: &str,
        new_entries: &[StreamEntry],
        now_ms: u64,
    ) {
        let history_secs = self.rate_history_secs;

        // Parse unix_ms from entry IDs (format: "{unix_ms}-{seq}")
        let new_ms: Vec<u64> = new_entries
            .iter()
            .filter_map(|e| e.id.split('-').next()?.parse::<u64>().ok())
            .collect();

        if new_ms.is_empty() {
            return;
        }

        let tracker = self.rate_trackers.entry(key_name.to_string()).or_default();

        // Gap detection: compare first new entry's timestamp to last seen timestamp.
        // A jump larger than 1.85x the expected inter-arrival interval suggests
        // entries were written and trimmed before we could read them.
        // Use 30s window for the reference rate — stable enough to avoid false positives.
        if let Some(last_ms) = tracker.last_entry_ms {
            let first_new_ms = new_ms[0];
            if first_new_ms > last_ms {
                let gap_ms = first_new_ms - last_ms;

                let now_for_gap = first_new_ms; // approximate: compare relative to arrival
                let cutoff_30s = now_for_gap.saturating_sub(30_000);
                let count_30s = tracker
                    .entry_timestamps
                    .iter()
                    .filter(|&&ts| ts >= cutoff_30s)
                    .count();
                let current_rate = if count_30s > 0 {
                    count_30s as f64 / 30.0
                } else {
                    0.0
                };

                let threshold_ms = if current_rate > 0.0 {
                    ((1000.0 / current_rate) * 1.85).max(500.0) as u64
                } else {
                    500
                };

                if gap_ms > threshold_ms {
                    tracker.gaps.push(first_new_ms);
                }
            }
        }

        // Keep 60s of timestamps — enough to compute all multi-window averages (1s..30s)
        // and to use the stable 30s window for gap detection.
        for ms in &new_ms {
            tracker.entry_timestamps.push_back(*ms);
        }
        let timestamps_cutoff_ms = now_ms.saturating_sub(60 * 1000);
        while let Some(&front) = tracker.entry_timestamps.front() {
            if front < timestamps_cutoff_ms {
                tracker.entry_timestamps.pop_front();
            } else {
                break;
            }
        }

        // Record tracking start time on first call
        let start_ms = *tracker.tracking_start_ms.get_or_insert(now_ms);

        // Chart rate: sliding window controlled by --rate-avg-window (default 2s)
        let chart_window = self.rate_avg_window_secs.max(1);
        let chart_cutoff_ms = now_ms.saturating_sub(chart_window * 1000);
        let chart_count = tracker
            .entry_timestamps
            .iter()
            .filter(|&&ts| ts >= chart_cutoff_ms)
            .count();
        let chart_rate = chart_count as f64 / chart_window as f64;

        // Only record once the window is full — avoids low warmup samples skewing the minimum
        if now_ms.saturating_sub(start_ms) >= chart_window * 1000 {
            tracker.rate_history.push_back((now_ms, chart_rate));
        }

        // Prune chart history outside the rolling display window
        let history_cutoff_ms = now_ms.saturating_sub(history_secs * 1000);
        while let Some(&(ts, _)) = tracker.rate_history.front() {
            if ts < history_cutoff_ms {
                tracker.rate_history.pop_front();
            } else {
                break;
            }
        }

        // Prune gaps that are older than the history window
        tracker.gaps.retain(|&ts| ts >= history_cutoff_ms);

        tracker.last_entry_ms = new_ms.last().copied();
        tracker.total_entries += new_entries.len() as u64;

        // Recompute five-number summary at most once per second
        if now_ms.saturating_sub(tracker.last_five_num_ms) >= 1000 {
            tracker.five_num = compute_five_num(&tracker.rate_history);
            tracker.last_five_num_ms = now_ms;
        }

        // Update sticky display values and widths
        for (i, (secs, _)) in RATE_WINDOWS.iter().enumerate() {
            let rate = tracker.rate_for_window(*secs, now_ms);
            RateTracker::update_sticky_val(
                &mut tracker.rate_display_vals[i],
                &mut tracker.rate_display_vals_until_ms[i],
                rate,
                now_ms,
            );
        }
        let rate_needed = tracker
            .rate_display_vals
            .iter()
            .map(|r| format!("{:.1}", r).len())
            .max()
            .unwrap_or(3);
        RateTracker::update_sticky_width(
            &mut tracker.rate_display_width,
            &mut tracker.rate_display_width_until_ms,
            rate_needed,
            now_ms,
        );

        if let Some(vals) = tracker.five_num {
            for (i, v) in vals.iter().enumerate() {
                RateTracker::update_sticky_val(
                    &mut tracker.stat_display_vals[i],
                    &mut tracker.stat_display_vals_until_ms[i],
                    *v,
                    now_ms,
                );
            }
            let stat_needed = tracker
                .stat_display_vals
                .iter()
                .map(|v| format!("{:.1}", v).len())
                .max()
                .unwrap_or(3);
            RateTracker::update_sticky_width(
                &mut tracker.stat_display_width,
                &mut tracker.stat_display_width_until_ms,
                stat_needed,
                now_ms,
            );
        }
    }

    /// Remove the rate tracker for a key (called when listener stops).
    pub fn clear_rate_tracker(&mut self, key_name: &str) {
        self.rate_trackers.remove(key_name);
    }

    /// Drop the state that only makes sense while a listener is running.
    ///
    /// One home for the rule, because there are two removal paths and the
    /// eviction one drifted: it was a copy of the explicit-stop sequence with
    /// the tracker cleanup missing (#96). Anything future listeners accumulate
    /// belongs here rather than at either call site.
    pub fn stop_listening(&mut self, key_name: &str) {
        self.clear_rate_tracker(key_name);
    }

    pub fn refresh_keys(&mut self, client: &mut MultiRedisClient) {
        let scan_ok = match client.scan_keys(&self.filter_pattern) {
            Ok((keys, skipped)) => {
                // Get types for each key
                let mut types = Vec::with_capacity(keys.len());
                for key in &keys {
                    let t = client
                        .get_key_info(key)
                        .map(|info| info.key_type)
                        .unwrap_or_else(|_| "?".to_string());
                    types.push(t);
                }
                self.keys = keys;
                self.key_types = types;

                // Track collisions
                self.collisions = client.collisions.clone();
                let collision_count = self.collisions.len();

                // Keys that failed UTF-8 decoding cannot be shown in a text UI, but
                // staying silent would make Redis look like it holds fewer keys than it does.
                let skipped_note = if skipped > 0 {
                    format!(" ({} skipped: encoding errors)", skipped)
                } else {
                    String::new()
                };

                // A host that failed to scan is silently absent from the key
                // list otherwise, which reads as "those keys are gone".
                let host_note = if client.host_errors.is_empty() {
                    String::new()
                } else {
                    let hosts: Vec<&str> = client
                        .host_errors
                        .iter()
                        .map(|(label, _)| label.as_str())
                        .collect();
                    format!(
                        " ({} host{} unreachable: {})",
                        hosts.len(),
                        if hosts.len() == 1 { "" } else { "s" },
                        hosts.join(", ")
                    )
                };

                if collision_count > 0 {
                    self.status_message = format!(
                        "Loaded {} keys ({} collisions!){}{}",
                        self.keys.len(),
                        collision_count,
                        skipped_note,
                        host_note
                    );
                } else {
                    self.status_message = format!(
                        "Loaded {} keys{}{}",
                        self.keys.len(),
                        skipped_note,
                        host_note
                    );
                }

                // Preserve selection if possible
                if self.keys.is_empty() {
                    self.key_list_state.select(None);
                } else if self.key_list_state.selected().is_none() {
                    self.key_list_state.select(Some(0));
                } else if let Some(sel) = self.key_list_state.selected() {
                    if sel >= self.keys.len() {
                        self.key_list_state.select(Some(self.keys.len() - 1));
                    }
                }
                true
            }
            Err(e) => {
                self.status_message = format!("Error scanning keys: {}", e);
                false
            }
        };

        self.db_size = client.get_db_size().unwrap_or(0);
        self.host_count = client.host_count();
        self.hosts_connected = client.num_connected();
        self.connected = client.is_connected();

        // The value pane has to follow the key list. Only the selection *index*
        // is preserved above, so any refresh that changes what that index points
        // at - a filter change, a DB switch, or keys appearing and disappearing
        // under `r` - would otherwise leave `current_value` and `current_key_info`
        // describing the previously selected key while the list highlights a
        // different one. `start_edit` reads the type from `current_key_info` and
        // the name from the live selection, so that mismatch meant `s` prefilled
        // the new key's editor with the old key's value and saving wrote it back
        // under the new name.
        //
        // Doing it here rather than at each call site is deliberate: two of the
        // five refresh sites already called `load_selected_value` explicitly and
        // three did not, which is exactly how the mismatch survived.
        //
        // Only on a successful scan. If the scan failed, `self.keys` and the
        // selection still describe the last good listing, so loading would issue
        // further commands down a connection that just failed and replace the
        // specific "Error scanning keys" message with a vaguer one from the
        // value read - burying the actual cause.
        if scan_ok {
            self.load_selected_value(client);
        }
    }

    /// `(shown, total)` when the loaded collection is larger than what was
    /// loaded, so the pane can say so. Truncation the user cannot see is the
    /// same quiet wrongness as showing a mangled value: partial data reads as
    /// complete.
    pub fn value_truncation(&self) -> Option<(usize, usize)> {
        let total = self.value_total_items?;
        let shown = match self.current_value.as_ref()? {
            RedisValue::List(v) | RedisValue::Set(v) => v.len(),
            RedisValue::ZSet(v) => v.len(),
            RedisValue::Hash(v) => v.len(),
            _ => return None,
        };
        if total > shown {
            Some((shown, total))
        } else {
            None
        }
    }

    pub fn load_selected_value(&mut self, client: &mut MultiRedisClient) {
        if let Some(idx) = self.key_list_state.selected() {
            if idx < self.keys.len() {
                let key = &self.keys[idx].clone();

                // Track which host this key belongs to
                if client.host_count() > 1 {
                    self.current_key_host = Some(client.host_label_for_key(key).to_string());
                } else {
                    self.current_key_host = None;
                }

                // Warn if this is a collision key
                if client.is_collision(key) {
                    self.status_message = format!("WARNING: '{}' exists on multiple hosts!", key);
                }

                match client.get_key_info(key) {
                    Ok(info) => self.current_key_info = Some(info),
                    Err(e) => {
                        self.status_message = format!("Error getting key info: {}", e);
                        self.current_key_info = None;
                    }
                }

                match client.get_value(key, self.max_value_items) {
                    Ok(loaded) => {
                        self.value_total_items = loaded.total_items;
                        let mut value = loaded.value;
                        // Cap stream entries to prevent OOM on large streams
                        cap_stream_entries(&mut value);
                        // Track last stream ID for XREAD polling
                        if let RedisValue::Stream(ref entries) = value {
                            self.last_stream_id = entries.last().map(|e| e.id.clone());
                        } else {
                            self.last_stream_id = None;
                        }
                        self.update_plot_data(&value);
                        self.current_value = Some(value);
                        self.value_scroll = 0;
                    }
                    Err(e) => {
                        self.status_message = format!("Error reading value: {}", e);
                        self.current_value = None;
                        self.value_total_items = None;
                        self.plot_data.clear();
                        self.last_stream_id = None;
                    }
                }
            }
        }
    }

    /// Append new stream entries from XREAD into the current value.
    /// Returns true if new entries were added.
    pub fn append_stream_entries(
        &mut self,
        new_entries: Vec<crate::redis_client::StreamEntry>,
    ) -> bool {
        if new_entries.is_empty() {
            return false;
        }
        // Update last_stream_id
        if let Some(last) = new_entries.last() {
            self.last_stream_id = Some(last.id.clone());
        }
        // Append to existing stream value
        if let Some(RedisValue::Stream(ref mut entries)) = self.current_value {
            entries.extend(new_entries);
            // Cap entries to prevent unbounded memory growth
            if entries.len() > MAX_STREAM_ENTRIES {
                let excess = entries.len() - MAX_STREAM_ENTRIES;
                entries.drain(..excess);
            }
            // Extract plot data from the last entry only (avoids cloning entire stream)
            let mut found_plot_field = false;
            if let Some(last_entry) = entries.last() {
                for (fname, fval) in &last_entry.fields {
                    if fname.starts_with('_') {
                        self.plot_data = decode_blob(fval, self.data_type, self.endianness);
                        for v in &mut self.plot_data {
                            if !v.is_finite() {
                                *v = 0.0;
                            }
                        }
                        found_plot_field = true;
                        break;
                    }
                }
            }
            if !found_plot_field {
                self.plot_data.clear();
            }
            self.expanded_stream_entries = vec![false; entries.len()];
            if self.fft_enabled {
                self.compute_fft();
            }
            true
        } else {
            false
        }
    }

    pub fn is_viewing_stream(&self) -> bool {
        matches!(
            &self.current_key_info,
            Some(info) if info.key_type == "stream"
        )
    }

    fn update_plot_data(&mut self, value: &RedisValue) {
        self.plot_data = match value {
            RedisValue::String(bytes) => decode_blob(bytes, self.data_type, self.endianness),
            RedisValue::Stream(entries) => {
                // Extract _ fields from stream entries and decode
                self.expanded_stream_entries = vec![false; entries.len()];
                extract_stream_plot_data(entries, self.data_type, self.endianness)
            }
            RedisValue::List(items) => {
                // Try to parse list items as numbers or decode as blobs
                let mut data = Vec::new();
                for item in items {
                    if let Ok(s) = std::str::from_utf8(item) {
                        if let Ok(v) = s.parse::<f64>() {
                            data.push(v);
                            continue;
                        }
                    }
                    let decoded = decode_blob(item, self.data_type, self.endianness);
                    data.extend(decoded);
                }
                data
            }
            RedisValue::ZSet(pairs) => {
                // Plot scores
                pairs.iter().map(|(_, score)| *score).collect()
            }
            RedisValue::Hash(pairs) => {
                // Try to parse hash values as numbers
                let mut data = Vec::new();
                for (_, val) in pairs {
                    if let Ok(s) = std::str::from_utf8(val) {
                        if let Ok(v) = s.parse::<f64>() {
                            data.push(v);
                            continue;
                        }
                    }
                    let decoded = decode_blob(val, self.data_type, self.endianness);
                    data.extend(decoded);
                }
                data
            }
            _ => Vec::new(),
        };
        // Sanitize: replace NaN/Infinity with 0.0 to prevent chart panics
        for v in &mut self.plot_data {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
    }

    /// Cycle the data type for the currently selected key's plot slot (if plotted),
    /// otherwise cycle the global data type. Then recompute.
    pub fn cycle_data_type(&mut self, forward: bool) {
        let new_type = if forward {
            self.data_type.next()
        } else {
            self.data_type.prev()
        };
        // Update the slot for the selected key if it's plotted
        if let Some(key) = self.selected_key_name().map(|s| s.to_string()) {
            if let Some(slot) = self.plot_slots.iter_mut().find(|s| s.key_name == key) {
                slot.data_type = if forward {
                    slot.data_type.next()
                } else {
                    slot.data_type.prev()
                };
                self.data_type = slot.data_type;
            } else {
                self.data_type = new_type;
            }
        } else {
            self.data_type = new_type;
        }
        self.recompute_plot();
    }

    /// Toggle endianness for the currently selected key's plot slot (if plotted),
    /// otherwise toggle the global endianness. Then recompute.
    pub fn toggle_endianness(&mut self) {
        let new_end = self.endianness.toggle();
        if let Some(key) = self.selected_key_name().map(|s| s.to_string()) {
            if let Some(slot) = self.plot_slots.iter_mut().find(|s| s.key_name == key) {
                slot.endianness = slot.endianness.toggle();
                self.endianness = slot.endianness;
            } else {
                self.endianness = new_end;
            }
        } else {
            self.endianness = new_end;
        }
        self.recompute_plot();
    }

    pub fn recompute_plot(&mut self) {
        if let Some(value) = self.current_value.take() {
            self.update_plot_data(&value);
            // Also update the plot slot for the currently viewed key
            if let Some(key) = self.selected_key_name().map(|s| s.to_string()) {
                self.update_slot_data(&key, &value);
            }
            self.current_value = Some(value);
        }
        // Clear stale FFT data immediately so UI doesn't use mismatched data
        self.fft_data.clear();
        self.fft_chart_area = None;
        if self.fft_enabled {
            self.compute_fft();
        }
    }

    pub fn toggle_fft(&mut self) {
        self.fft_enabled = !self.fft_enabled;
        if self.fft_enabled {
            self.compute_fft();
        } else {
            self.fft_data.clear();
            self.fft_computing = false;
            self.fft_dirty = false;
            self.fft_rx = None;
            // Non-blocking join — avoid stalling the UI thread
            if let Some(h) = self.fft_handle.take() {
                std::thread::spawn(move || {
                    let _ = h.join();
                });
            }
        }
    }

    pub fn compute_fft(&mut self) {
        if self.plot_data.is_empty() {
            self.fft_data.clear();
            self.fft_computing = false;
            self.fft_dirty = false;
            self.fft_rx = None;
            // Non-blocking join — avoid stalling the UI thread
            if let Some(h) = self.fft_handle.take() {
                std::thread::spawn(move || {
                    let _ = h.join();
                });
            }
            return;
        }
        // If an FFT is already in flight, mark dirty so poll_fft()
        // re-triggers a computation with the latest data once it completes.
        if self.fft_computing {
            self.fft_dirty = true;
            return;
        }
        self.fft_dirty = false;
        let data = self.plot_data.clone();
        let (tx, rx) = mpsc::channel();
        self.fft_rx = Some(rx);
        self.fft_computing = true;
        let handle = std::thread::spawn(move || {
            let result = compute_fft_magnitude(&data);
            let _ = tx.send(result);
        });
        self.fft_handle = Some(handle);
    }

    /// Check if background FFT has completed; call this each tick.
    pub fn poll_fft(&mut self) {
        if let Some(ref rx) = self.fft_rx {
            match rx.try_recv() {
                Ok(data) => {
                    self.fft_data = data;
                    self.fft_computing = false;
                    self.fft_rx = None;
                    if let Some(h) = self.fft_handle.take() {
                        let _ = h.join();
                    }
                    // Re-trigger if data changed while we were computing
                    if self.fft_dirty {
                        self.compute_fft();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.fft_computing = false;
                    self.fft_rx = None;
                    if let Some(h) = self.fft_handle.take() {
                        let _ = h.join();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {} // still computing
            }
        }
    }

    pub fn set_auto_limits(&mut self) {
        match self.plot_focus {
            PlotFocus::Signal => {
                self.plot_auto_limits = true;
                self.plot_x_min = 0.0;
                self.plot_x_max = 0.0;
            }
            PlotFocus::FFT => {
                self.fft_auto_limits = true;
                self.fft_x_min = 0.0;
                self.fft_x_max = 0.0;
            }
        }
        // Clear per-slot Y limits
        for slot in &mut self.plot_slots {
            slot.y_min = None;
            slot.y_max = None;
        }
    }

    /// Get the x-axis bounds for the signal chart.
    /// In auto mode, show the newest data (last PLOT_WINDOW points or fewer).
    pub fn signal_x_bounds(&self) -> (f64, f64) {
        let full = self.plot_data.len() as f64;
        if self.plot_x_max <= self.plot_x_min {
            // Auto: show last PLOT_WINDOW points
            let window = PLOT_WINDOW as f64;
            if full <= window {
                (0.0, full)
            } else {
                (full - window, full)
            }
        } else {
            (self.plot_x_min, self.plot_x_max)
        }
    }

    /// Get the x-axis bounds for the FFT chart
    pub fn fft_x_bounds(&self) -> (f64, f64) {
        let full = self.fft_data.len() as f64;
        if self.fft_x_max <= self.fft_x_min {
            (0.0, full)
        } else {
            (self.fft_x_min, self.fft_x_max)
        }
    }

    /// Zoom in/out on the focused plot. factor > 1 zooms in, < 1 zooms out.
    /// center_frac is where in the viewport to zoom (0.0 = left, 1.0 = right)
    pub fn zoom_plot(&mut self, factor: f64, center_frac_x: f64, center_frac_y: f64) {
        let is_fft = self.hover_in_fft && self.fft_enabled;

        if is_fft {
            let (x0, x1) = self.fft_x_bounds();
            let (y0, y1) = if self.fft_auto_limits {
                self.auto_fft_bounds()
            } else {
                (self.fft_y_min, self.fft_y_max)
            };
            let full_x = self.fft_data.len() as f64;
            let (nx0, nx1) = zoom_range(x0, x1, factor, center_frac_x, 0.0, full_x);
            let (ny0, ny1) = zoom_range(
                y0,
                y1,
                factor,
                center_frac_y,
                f64::NEG_INFINITY,
                f64::INFINITY,
            );
            self.fft_x_min = nx0;
            self.fft_x_max = nx1;
            self.fft_y_min = ny0;
            self.fft_y_max = ny1;
            self.fft_auto_limits = false;
        } else {
            let (x0, x1) = self.signal_x_bounds();
            let (y0, y1) = if self.plot_auto_limits {
                self.auto_signal_bounds()
            } else {
                (self.plot_y_min, self.plot_y_max)
            };
            let full_x = self.plot_data.len() as f64;
            let (nx0, nx1) = zoom_range(x0, x1, factor, center_frac_x, 0.0, full_x);
            let (ny0, ny1) = zoom_range(
                y0,
                y1,
                factor,
                center_frac_y,
                f64::NEG_INFINITY,
                f64::INFINITY,
            );
            self.plot_x_min = nx0;
            self.plot_x_max = nx1;
            self.plot_y_min = ny0;
            self.plot_y_max = ny1;
            self.plot_auto_limits = false;
        }
    }

    /// Convert terminal coordinates to chart data coordinates.
    /// Returns (data_x, data_y) or None if outside chart area.
    pub fn mouse_to_data(&self, col: u16, row: u16) -> Option<(f64, f64, bool)> {
        // Check FFT chart first (if it exists)
        if let Some((cx, cy, cw, ch)) = self.fft_chart_area {
            if col >= cx && col < cx + cw && row >= cy && row < cy + ch {
                let (x0, x1) = self.fft_x_bounds();
                let (y0, y1) = if self.fft_auto_limits {
                    self.auto_fft_bounds()
                } else {
                    (self.fft_y_min, self.fft_y_max)
                };
                let frac_x = (col - cx) as f64 / cw.max(1) as f64;
                let frac_y = 1.0 - (row - cy) as f64 / ch.max(1) as f64;
                let dx = x0 + frac_x * (x1 - x0);
                let dy = y0 + frac_y * (y1 - y0);
                return Some((dx, dy, true));
            }
        }
        // Check signal chart
        if let Some((cx, cy, cw, ch)) = self.signal_chart_area {
            if col >= cx && col < cx + cw && row >= cy && row < cy + ch {
                let (x0, x1) = self.signal_x_bounds();
                // Match the Y bounds logic used by the chart renderer
                let (y0, y1) = if self.plot_slots.len() > 1 {
                    (-0.05, 1.05)
                } else if !self.plot_slots.is_empty() {
                    let slot = &self.plot_slots[0];
                    if let (Some(y_min), Some(y_max)) = (slot.y_min, slot.y_max) {
                        (y_min, y_max)
                    } else {
                        self.auto_signal_bounds()
                    }
                } else if self.plot_auto_limits {
                    self.auto_signal_bounds()
                } else {
                    (self.plot_y_min, self.plot_y_max)
                };
                let frac_x = (col - cx) as f64 / cw.max(1) as f64;
                let frac_y = 1.0 - (row - cy) as f64 / ch.max(1) as f64;
                let dx = x0 + frac_x * (x1 - x0);
                let dy = y0 + frac_y * (y1 - y0);
                return Some((dx, dy, false));
            }
        }
        None
    }

    /// Open plot settings popup: unified X limits + per-slot Y limits
    pub fn start_plot_settings(&mut self) {
        let (x_min, x_max) = match self.plot_focus {
            PlotFocus::Signal => self.signal_x_bounds(),
            PlotFocus::FFT => self.fft_x_bounds(),
        };
        let mut fields = vec![
            ("X Min".to_string(), format!("{:.2}", x_min)),
            ("X Max".to_string(), format!("{:.2}", x_max)),
        ];
        if self.plot_slots.is_empty() {
            // No multi-key: single Y range
            let (y_min, y_max) = if self.plot_auto_limits {
                self.auto_signal_bounds()
            } else {
                (self.plot_y_min, self.plot_y_max)
            };
            fields.push(("Y Min".to_string(), format!("{:.2}", y_min)));
            fields.push(("Y Max".to_string(), format!("{:.2}", y_max)));
        } else {
            // Per-slot Y limits
            for slot in &self.plot_slots {
                let auto_min = slot
                    .data
                    .iter()
                    .copied()
                    .filter(|v| v.is_finite())
                    .fold(f64::INFINITY, f64::min);
                let auto_max = slot
                    .data
                    .iter()
                    .copied()
                    .filter(|v| v.is_finite())
                    .fold(f64::NEG_INFINITY, f64::max);
                let y_min = slot
                    .y_min
                    .unwrap_or(if auto_min.is_finite() { auto_min } else { 0.0 });
                let y_max = slot
                    .y_max
                    .unwrap_or(if auto_max.is_finite() { auto_max } else { 1.0 });
                let short_name = crate::data::truncate_key_name(&slot.key_name, 10);
                fields.push((format!("{} Y Min", short_name), format!("{:.2}", y_min)));
                fields.push((format!("{} Y Max", short_name), format!("{:.2}", y_max)));
            }
        }
        self.edit_fields = fields;
        self.edit_focus = 0;
        self.input_mode = InputMode::PlotLimit;
    }

    /// Apply plot settings: unified X + per-slot Y limits
    pub fn apply_plot_settings(&mut self) -> Result<(), String> {
        let x_min = parse_finite(&self.edit_fields[0].1, "X Min")?;
        let x_max = parse_finite(&self.edit_fields[1].1, "X Max")?;
        if x_min >= x_max {
            return Err("X Min must be less than X Max".to_string());
        }
        match self.plot_focus {
            PlotFocus::Signal => {
                self.plot_x_min = x_min;
                self.plot_x_max = x_max;
            }
            PlotFocus::FFT => {
                self.fft_x_min = x_min;
                self.fft_x_max = x_max;
            }
        }

        if self.plot_slots.is_empty() {
            // Single Y range
            if self.edit_fields.len() >= 4 {
                let y_min = parse_finite(&self.edit_fields[2].1, "Y Min")?;
                let y_max = parse_finite(&self.edit_fields[3].1, "Y Max")?;
                if y_min >= y_max {
                    return Err("Y Min must be less than Y Max".to_string());
                }
                self.plot_y_min = y_min;
                self.plot_y_max = y_max;
                self.plot_auto_limits = false;
            }
        } else {
            // Per-slot Y limits (fields start at index 2, 2 fields per slot)
            for (i, slot) in self.plot_slots.iter_mut().enumerate() {
                let base = 2 + i * 2;
                if base + 1 < self.edit_fields.len() {
                    let y_min = parse_finite(
                        &self.edit_fields[base].1,
                        &format!("Y Min for '{}'", slot.key_name),
                    )?;
                    let y_max = parse_finite(
                        &self.edit_fields[base + 1].1,
                        &format!("Y Max for '{}'", slot.key_name),
                    )?;
                    if y_min >= y_max {
                        return Err(format!("Y Min >= Y Max for '{}'", slot.key_name));
                    }
                    slot.y_min = Some(y_min);
                    slot.y_max = Some(y_max);
                }
            }
            self.plot_auto_limits = false;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn apply_x_limits(&mut self) -> Result<(), String> {
        self.apply_plot_settings()
    }

    #[allow(dead_code)]
    pub fn apply_plot_limits(&mut self) -> Result<(), String> {
        self.apply_plot_settings()
    }

    pub fn auto_signal_bounds(&self) -> (f64, f64) {
        auto_bounds(&self.plot_data)
    }

    pub fn auto_fft_bounds(&self) -> (f64, f64) {
        let data = self.fft_display_data();
        auto_bounds(&data)
    }

    /// Get FFT data for display (applies log scale if enabled)
    pub fn fft_display_data(&self) -> Vec<f64> {
        if self.fft_log_scale {
            self.fft_data
                .iter()
                .map(|&v| if v > 0.0 { v.log10() } else { -10.0 })
                .collect()
        } else {
            self.fft_data.clone()
        }
    }

    pub fn select_next_key(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let i = match self.key_list_state.selected() {
            Some(i) => {
                if i >= self.keys.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.key_list_state.select(Some(i));
    }

    pub fn select_prev_key(&mut self) {
        if self.keys.is_empty() {
            return;
        }
        let i = match self.key_list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.keys.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.key_list_state.select(Some(i));
    }

    pub fn scroll_value_down(&mut self) {
        self.value_scroll = self.value_scroll.saturating_add(1);
    }

    pub fn scroll_value_up(&mut self) {
        self.value_scroll = self.value_scroll.saturating_sub(1);
    }

    pub fn selected_key_name(&self) -> Option<&str> {
        self.key_list_state
            .selected()
            .and_then(|i| self.keys.get(i).map(|s| s.as_str()))
    }

    pub fn apply_filter(&mut self) {
        if self.filter_text.is_empty() {
            self.filter_pattern = "*".to_string();
        } else {
            self.filter_pattern = format!("*{}*", self.filter_text);
        }
    }

    /// Format the current value for display
    pub fn format_value(&self) -> Vec<String> {
        match &self.current_value {
            None => vec!["(no value loaded)".to_string()],
            Some(RedisValue::String(bytes)) => {
                // Show as decoded binary when data type is numeric or data looks binary
                let show_binary = match self.data_type {
                    DataType::String | DataType::Blob => is_binary(bytes),
                    _ => true, // numeric data type selected — always decode
                };
                if show_binary {
                    let mut lines = Vec::new();
                    // Show decoded values using current data type
                    lines.push(format!(
                        "── Decoded as {} ({}) ──",
                        self.data_type, self.endianness
                    ));
                    let decoded = crate::data::format_blob(bytes, self.data_type, self.endianness);
                    for l in decoded.lines() {
                        lines.push(l.to_string());
                    }
                    lines.push(String::new());
                    lines.push(format!("── Hex dump ({} bytes) ──", bytes.len()));
                    for l in crate::data::format_hex(bytes).lines() {
                        lines.push(l.to_string());
                    }
                    lines
                } else {
                    let s = String::from_utf8_lossy(bytes).to_string();
                    // Try to pretty-print JSON
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&s) {
                        if let Ok(pretty) = serde_json::to_string_pretty(&json) {
                            return pretty.lines().map(|l| l.to_string()).collect();
                        }
                    }
                    s.lines().map(|l| l.to_string()).collect()
                }
            }
            Some(RedisValue::List(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let s = String::from_utf8_lossy(item);
                    format!("[{}] {}", i, s)
                })
                .collect(),
            Some(RedisValue::Set(items)) => items
                .iter()
                .map(|item| {
                    let s = String::from_utf8_lossy(item);
                    format!("- {}", s)
                })
                .collect(),
            Some(RedisValue::ZSet(pairs)) => pairs
                .iter()
                .map(|(member, score)| {
                    let s = String::from_utf8_lossy(member);
                    format!("{:.4}  {}", score, s)
                })
                .collect(),
            Some(RedisValue::Hash(pairs)) => pairs
                .iter()
                .map(|(field, val)| {
                    let s = String::from_utf8_lossy(val);
                    format!("{}  =>  {}", field, s)
                })
                .collect(),
            Some(RedisValue::Stream(entries)) => {
                format_stream_entries(entries, self.data_type, self.endianness)
            }
            Some(RedisValue::Unknown(msg)) => vec![msg.clone()],
        }
    }

    // ─── Edit operations ─────────────────────────────────────

    pub fn start_edit(&mut self) {
        let key_type = self
            .current_key_info
            .as_ref()
            .map(|i| i.key_type.as_str())
            .unwrap_or("");
        let key = match self.selected_key_name() {
            Some(k) => k.to_string(),
            None => return,
        };
        self.edit_key = key.clone();
        self.edit_focus = 0;
        self.edit_multi_count = 0;

        match key_type {
            "string" => {
                // Never pre-fill through `from_utf8_lossy`. It collapses every
                // invalid byte sequence into U+FFFD, and `execute_edit` writes
                // that text straight back with SET, so pressing Enter on an
                // untouched popup destroyed a binary value (#82). The question
                // is literally "is this UTF-8", so ask that -- not
                // `is_binary()`, which only looks for control bytes.
                let text = match &self.current_value {
                    Some(RedisValue::String(b)) => std::str::from_utf8(b).ok().map(str::to_string),
                    // No value loaded: an empty text field is the old behaviour
                    // and cannot lose anything.
                    _ => Some(String::new()),
                };
                self.edit_operation = Some(EditOperation::SetString);
                match text {
                    Some(t) => {
                        self.edit_binary_mode = false;
                        self.edit_fields = vec![("Value".to_string(), t)];
                    }
                    None => {
                        // Binary: open straight into binary mode with an empty
                        // field, so the value has to be re-entered deliberately
                        // as numbers. `encode_values` rejects an empty field, so
                        // Enter without typing reports an error and writes
                        // nothing, rather than destroying the key.
                        self.edit_binary_mode = true;
                        self.edit_fields = vec![("Value".to_string(), String::new())];
                        self.status_message = format!(
                            "'{}' holds non-UTF-8 bytes - type new values as {} (Ctrl+T changes type)",
                            key,
                            DataType::all()[self.edit_binary_dtype_idx]
                        );
                    }
                }
            }
            "hash" => {
                self.edit_operation = Some(EditOperation::HSet);
                self.edit_fields = vec![
                    ("Field".to_string(), String::new()),
                    ("Value".to_string(), String::new()),
                ];
            }
            "list" => {
                self.edit_operation = Some(EditOperation::RPush);
                self.edit_fields = vec![("Value (appended)".to_string(), String::new())];
            }
            "set" => {
                self.edit_operation = Some(EditOperation::SAdd);
                self.edit_fields = vec![("Member".to_string(), String::new())];
            }
            "zset" => {
                self.edit_operation = Some(EditOperation::ZAdd);
                self.edit_fields = vec![
                    ("Score".to_string(), "0".to_string()),
                    ("Member".to_string(), String::new()),
                ];
            }
            "stream" => {
                self.edit_operation = Some(EditOperation::XAdd);
                self.edit_fields = vec![
                    ("Field".to_string(), String::new()),
                    ("Value".to_string(), String::new()),
                ];
            }
            _ => return,
        }
        self.input_mode = InputMode::Edit;
    }

    pub fn start_set_ttl(&mut self) {
        let key = match self.selected_key_name() {
            Some(k) => k.to_string(),
            None => return,
        };
        let current_ttl = self
            .current_key_info
            .as_ref()
            .map(|i| {
                if i.ttl < 0 {
                    String::new()
                } else {
                    i.ttl.to_string()
                }
            })
            .unwrap_or_default();
        self.edit_key = key;
        self.edit_operation = Some(EditOperation::SetTTL);
        self.edit_fields = vec![("TTL (seconds, empty=persist)".to_string(), current_ttl)];
        self.edit_focus = 0;
        self.input_mode = InputMode::Edit;
    }

    pub fn start_rename(&mut self) {
        let key = match self.selected_key_name() {
            Some(k) => k.to_string(),
            None => return,
        };
        self.edit_operation = Some(EditOperation::RenameKey);
        self.edit_fields = vec![("New name".to_string(), key.clone())];
        self.edit_key = key;
        self.edit_focus = 0;
        self.input_mode = InputMode::Edit;
    }

    pub fn start_new_key(&mut self) {
        self.edit_operation = Some(EditOperation::NewKey);
        self.new_key_type_idx = 0;
        self.edit_multi_count = 0;
        self.edit_fields = vec![
            ("Key".to_string(), String::new()),
            ("Value".to_string(), String::new()),
        ];
        self.edit_key.clear();
        self.edit_focus = 0;
        self.input_mode = InputMode::Edit;
    }

    pub fn execute_edit(&mut self, client: &mut MultiRedisClient) -> Result<(), String> {
        let op = match &self.edit_operation {
            Some(op) => op.clone(),
            None => return Err("No operation".to_string()),
        };

        // Helper: encode value to binary if binary mode is on
        let bin_dtype = DataType::all()[self.edit_binary_dtype_idx];
        let bin_endian = self.endianness;
        let binary_mode = self.edit_binary_mode;

        let result = match op {
            EditOperation::SetString => {
                let value = &self.edit_fields[0].1;
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    client
                        .set_bytes(&self.edit_key, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .set_string(&self.edit_key, value)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::HSet => {
                let field = &self.edit_fields[0].1;
                let value = &self.edit_fields[1].1;
                if field.is_empty() {
                    return Err("Field name is required".to_string());
                }
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    client
                        .hset_bytes(&self.edit_key, field, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .hset(&self.edit_key, field, value)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::RPush => {
                let value = &self.edit_fields[0].1;
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    client
                        .rpush_bytes(&self.edit_key, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .rpush(&self.edit_key, value)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::LSet => {
                let index: i64 = self.edit_fields[0]
                    .1
                    .parse()
                    .map_err(|_| "Invalid index".to_string())?;
                let value = &self.edit_fields[1].1;
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    client
                        .lset_bytes(&self.edit_key, index, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .lset(&self.edit_key, index, value)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::SAdd => {
                let member = &self.edit_fields[0].1;
                if binary_mode {
                    let bytes = encode_values(member, bin_dtype, bin_endian)?;
                    client
                        .sadd_bytes(&self.edit_key, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .sadd(&self.edit_key, member)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::ZAdd => {
                let score: f64 = self.edit_fields[0]
                    .1
                    .parse()
                    .map_err(|_| "Invalid score (must be a number)".to_string())?;
                let member = &self.edit_fields[1].1;
                if binary_mode {
                    let bytes = encode_values(member, bin_dtype, bin_endian)?;
                    client
                        .zadd_bytes(&self.edit_key, score, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .zadd(&self.edit_key, score, member)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::XAdd => {
                let field = &self.edit_fields[0].1;
                let value = &self.edit_fields[1].1;
                if field.is_empty() {
                    return Err("Field name is required".to_string());
                }
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    client
                        .xadd_binary(&self.edit_key, field, &bytes)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .xadd(&self.edit_key, field, value)
                        .map_err(|e| e.to_string())
                }
            }
            EditOperation::SetTTL => {
                let ttl_str = self.edit_fields[0].1.trim().to_string();
                let ttl = if ttl_str.is_empty() {
                    -1
                } else {
                    ttl_str
                        .parse::<i64>()
                        .map_err(|_| "Invalid TTL (must be a number)".to_string())?
                };
                client
                    .set_ttl(&self.edit_key, ttl)
                    .map_err(|e| e.to_string())
            }
            EditOperation::RenameKey => {
                let new_name = &self.edit_fields[0].1;
                if new_name.is_empty() {
                    return Err("Key name is required".to_string());
                }
                client
                    .rename_key(&self.edit_key, new_name)
                    .map_err(|e| e.to_string())
            }
            EditOperation::NewKey => {
                let key = &self.edit_fields[0].1;
                let value = &self.edit_fields[1].1;
                if key.is_empty() {
                    return Err("Key name is required".to_string());
                }
                let key_type = KEY_TYPES[self.new_key_type_idx];
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    match key_type {
                        "string" => client.set_bytes(key, &bytes).map_err(|e| e.to_string()),
                        "hash" => client
                            .hset_bytes(key, "field", &bytes)
                            .map_err(|e| e.to_string()),
                        "list" => client.rpush_bytes(key, &bytes).map_err(|e| e.to_string()),
                        "set" => client.sadd_bytes(key, &bytes).map_err(|e| e.to_string()),
                        "zset" => client
                            .zadd_bytes(key, 0.0, &bytes)
                            .map_err(|e| e.to_string()),
                        "stream" => client
                            .xadd_binary(key, "data", &bytes)
                            .map_err(|e| e.to_string()),
                        _ => Err(format!("Unknown type: {}", key_type)),
                    }
                } else {
                    match key_type {
                        "string" => client.set_string(key, value).map_err(|e| e.to_string()),
                        "hash" => client.hset(key, "field", value).map_err(|e| e.to_string()),
                        "list" => client.rpush(key, value).map_err(|e| e.to_string()),
                        "set" => client.sadd(key, value).map_err(|e| e.to_string()),
                        "zset" => client.zadd(key, 0.0, value).map_err(|e| e.to_string()),
                        "stream" => client.xadd(key, "data", value).map_err(|e| e.to_string()),
                        _ => Err(format!("Unknown type: {}", key_type)),
                    }
                }
            }
        };

        result
    }

    pub fn cancel_edit(&mut self) {
        self.edit_operation = None;
        self.edit_fields.clear();
        self.edit_focus = 0;
        self.edit_binary_mode = false;
        self.input_mode = InputMode::Normal;
    }

    pub fn edit_next_field(&mut self) {
        if !self.edit_fields.is_empty() {
            self.edit_focus = (self.edit_focus + 1) % self.edit_fields.len();
        }
    }

    pub fn edit_op_label(&self) -> &str {
        match &self.edit_operation {
            Some(EditOperation::SetString) => "SET",
            Some(EditOperation::HSet) => "HSET",
            Some(EditOperation::RPush) => "RPUSH",
            Some(EditOperation::LSet) => "LSET",
            Some(EditOperation::SAdd) => "SADD",
            Some(EditOperation::ZAdd) => "ZADD",
            Some(EditOperation::XAdd) => "XADD",
            Some(EditOperation::SetTTL) => "EXPIRE",
            Some(EditOperation::RenameKey) => "RENAME",
            Some(EditOperation::NewKey) => "NEW KEY",
            None => "",
        }
    }

    /// Returns true if the current edit operation supports adding multiple entries
    pub fn is_multi_entry_edit(&self) -> bool {
        matches!(
            &self.edit_operation,
            Some(EditOperation::HSet)
                | Some(EditOperation::RPush)
                | Some(EditOperation::SAdd)
                | Some(EditOperation::ZAdd)
                | Some(EditOperation::XAdd)
        )
    }

    /// Reset input fields for the next entry (keep labels, clear values)
    pub fn reset_edit_fields_for_next(&mut self) {
        for (_label, value) in &mut self.edit_fields {
            value.clear();
        }
        self.edit_focus = 0;
        self.edit_multi_count += 1;
    }

    // ─── Signal generator ─────────────────────────────────────

    pub fn start_signal_gen_popup(&mut self) {
        self.signal_gen_wave_idx = 0;
        self.signal_gen_dtype_idx = 6; // float32
        self.signal_gen_fields = vec![
            ("Cycles/Entry".to_string(), "2.0".to_string()),
            ("Amplitude".to_string(), "1.0".to_string()),
            ("Noise".to_string(), "0.1".to_string()),
            ("Samples/Entry".to_string(), "100".to_string()),
            ("Entries/Sec".to_string(), "10.0".to_string()),
        ];
        self.signal_gen_focus = 0;
        self.input_mode = InputMode::SignalGen;
    }

    pub fn signal_gen_next_field(&mut self) {
        // 7 total focusable rows: wave type, data type, + 5 text fields
        self.signal_gen_focus = (self.signal_gen_focus + 1) % 7;
    }

    pub fn signal_gen_prev_field(&mut self) {
        if self.signal_gen_focus == 0 {
            self.signal_gen_focus = 6;
        } else {
            self.signal_gen_focus -= 1;
        }
    }

    pub fn signal_gen_wave_type(&self) -> &str {
        WAVE_TYPES[self.signal_gen_wave_idx]
    }

    #[allow(dead_code)]
    pub fn signal_gen_data_type(&self) -> DataType {
        DataType::all()[self.signal_gen_dtype_idx]
    }
}

/// Configuration for the signal generator thread
#[derive(Debug, Clone)]
pub struct SignalGenConfig {
    pub wave_type: String,
    pub data_type: DataType,
    pub endianness: Endianness,
    pub frequency: f64,
    pub amplitude: f64,
    pub samples_per_entry: usize,
    pub entries_per_sec: f64,
    pub noise: f64,
}

/// Encode a single f64 value into bytes for the given DataType + Endianness
pub fn encode_wave_sample(val: f64, data_type: DataType, endianness: Endianness) -> Vec<u8> {
    match (data_type, endianness) {
        (DataType::Int8, _) => vec![(val.clamp(-128.0, 127.0) as i8) as u8],
        (DataType::UInt8, _) => vec![val.clamp(0.0, 255.0) as u8],
        (DataType::Int16, Endianness::Little) => {
            (val.clamp(-32768.0, 32767.0) as i16).to_le_bytes().to_vec()
        }
        (DataType::Int16, Endianness::Big) => {
            (val.clamp(-32768.0, 32767.0) as i16).to_be_bytes().to_vec()
        }
        (DataType::UInt16, Endianness::Little) => {
            (val.clamp(0.0, 65535.0) as u16).to_le_bytes().to_vec()
        }
        (DataType::UInt16, Endianness::Big) => {
            (val.clamp(0.0, 65535.0) as u16).to_be_bytes().to_vec()
        }
        (DataType::Int32, Endianness::Little) => (val.clamp(-2147483648.0, 2147483647.0) as i32)
            .to_le_bytes()
            .to_vec(),
        (DataType::Int32, Endianness::Big) => (val.clamp(-2147483648.0, 2147483647.0) as i32)
            .to_be_bytes()
            .to_vec(),
        (DataType::UInt32, Endianness::Little) => {
            (val.clamp(0.0, 4294967295.0) as u32).to_le_bytes().to_vec()
        }
        (DataType::UInt32, Endianness::Big) => {
            (val.clamp(0.0, 4294967295.0) as u32).to_be_bytes().to_vec()
        }
        (DataType::Float32, Endianness::Little) => (val as f32).to_le_bytes().to_vec(),
        (DataType::Float32, Endianness::Big) => (val as f32).to_be_bytes().to_vec(),
        (DataType::Float64, Endianness::Little) => val.to_le_bytes().to_vec(),
        (DataType::Float64, Endianness::Big) => val.to_be_bytes().to_vec(),
        (DataType::String, _) | (DataType::Blob, _) => (val as f32).to_le_bytes().to_vec(),
    }
}

/// Generate one entry's worth of wave samples as a binary blob.
/// `time_offset` is the cumulative phase offset (in cycles) so the wave
/// continues seamlessly across entries.
/// Frequency = number of complete cycles per entry.
/// Amplitude = peak value of the wave.
pub fn generate_wave_blob(config: &SignalGenConfig, time_offset: f64) -> Vec<u8> {
    let mut blob = Vec::new();
    let n = config.samples_per_entry as f64;
    // Simple xorshift64 RNG seeded from time_offset bits
    let mut rng_state: u64 = (time_offset.to_bits()).wrapping_add(0x9E3779B97F4A7C15);

    for i in 0..config.samples_per_entry {
        // phase in cycles: freq cycles per entry, offset keeps continuity
        let phase = time_offset + config.frequency * (i as f64 / n);
        let raw = match config.wave_type.as_str() {
            "sine" => (2.0 * std::f64::consts::PI * phase).sin(),
            "square" => {
                if (2.0 * std::f64::consts::PI * phase).sin() >= 0.0 {
                    1.0
                } else {
                    -1.0
                }
            }
            "sawtooth" => 2.0 * (phase.fract() + 1.0).fract() - 1.0,
            "triangle" => {
                let f = (phase.fract() + 1.0).fract();
                4.0 * (f - 0.5).abs() - 1.0
            }
            _ => 0.0,
        };
        let mut val = config.amplitude * raw;
        // Add noise: uniform random in [-noise, +noise]
        if config.noise != 0.0 {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            let r = (rng_state as f64) / (u64::MAX as f64) * 2.0 - 1.0; // [-1, 1]
            val += config.noise * r;
        }
        blob.extend(encode_wave_sample(val, config.data_type, config.endianness));
    }
    blob
}

fn format_stream_entries(
    entries: &[StreamEntry],
    data_type: DataType,
    endianness: Endianness,
) -> Vec<String> {
    let mut lines = Vec::new();
    let total = entries.len();
    // Show only last 5 entries (newest first)
    let start = total.saturating_sub(5);
    if start > 0 {
        lines.push(format!("({} older entries hidden)", start));
        lines.push(String::new());
    }
    for entry in entries[start..].iter().rev() {
        let time_str = format_stream_id(&entry.id);
        lines.push(format!("--- {} ({}) ---", entry.id, time_str));
        for (fname, fval) in &entry.fields {
            if fname.starts_with('_') {
                // Binary data field - show decoded values + hex summary
                let decoded = decode_blob(fval, data_type, endianness);
                if !decoded.is_empty() {
                    let preview: Vec<String> = decoded
                        .iter()
                        .take(8)
                        .map(|v| match data_type {
                            DataType::Float32 | DataType::Float64 => format!("{:.4}", v),
                            _ => format!("{}", *v as i64),
                        })
                        .collect();
                    let suffix = if decoded.len() > 8 {
                        format!(" ..({} vals)", decoded.len())
                    } else {
                        String::new()
                    };
                    lines.push(format!(
                        "  {} [{}]: [{}]{}",
                        fname,
                        data_type,
                        preview.join(", "),
                        suffix
                    ));
                }
                // Hex summary
                let hex: String = fval
                    .iter()
                    .take(24)
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                let suffix = if fval.len() > 24 { "..." } else { "" };
                lines.push(format!(
                    "  {} [hex, {} bytes]: {}{}",
                    fname,
                    fval.len(),
                    hex,
                    suffix
                ));
            } else {
                let s = String::from_utf8_lossy(fval);
                lines.push(format!("  {}: {}", fname, s));
            }
        }
    }
    lines
}

/// Convert a Redis stream ID (unix_ms-seq) to a human-readable time string.
/// Format: HH:MM:SS.mmm:seq
fn format_stream_id(id: &str) -> String {
    let parts: Vec<&str> = id.splitn(2, '-').collect();
    if parts.len() != 2 {
        return id.to_string();
    }
    let ms: u64 = match parts[0].parse() {
        Ok(v) => v,
        Err(_) => return id.to_string(),
    };
    let seq = parts[1];

    let total_secs = ms / 1000;
    let millis = ms % 1000;
    let secs = total_secs % 60;
    let mins = (total_secs / 60) % 60;
    let hrs = (total_secs / 3600) % 24;

    format!("{:02}:{:02}:{:02}.{:03}:{}", hrs, mins, secs, millis, seq)
}

/// Truncate stream entries to MAX_STREAM_ENTRIES, keeping the newest.
fn cap_stream_entries(value: &mut RedisValue) {
    if let RedisValue::Stream(ref mut entries) = value {
        if entries.len() > MAX_STREAM_ENTRIES {
            let excess = entries.len() - MAX_STREAM_ENTRIES;
            entries.drain(..excess);
        }
    }
}

/// Parse a user-entered plot bound, rejecting anything non-finite.
///
/// `"nan"` and `"inf"` both parse successfully as f64, and every comparison against NaN
/// is false - so without an explicit finite check they slip past the min/max guards and
/// poison the chart bounds.
fn parse_finite(raw: &str, label: &str) -> Result<f64, String> {
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("Invalid {}", label))?;
    if !value.is_finite() {
        return Err(format!("{} must be a finite number", label));
    }
    Ok(value)
}

fn extract_stream_plot_data(
    entries: &[StreamEntry],
    data_type: DataType,
    endianness: Endianness,
) -> Vec<f64> {
    // Only plot the newest (last) entry's waveform
    if let Some(entry) = entries.last() {
        for (fname, fval) in &entry.fields {
            if fname.starts_with('_') {
                return decode_blob(fval, data_type, endianness);
            }
        }
    }
    Vec::new()
}

/// Zoom a range [lo, hi] by factor centered at frac (0..1).
/// factor > 1 zooms in, < 1 zooms out. Clamps to [abs_min, abs_max].
fn zoom_range(lo: f64, hi: f64, factor: f64, frac: f64, abs_min: f64, abs_max: f64) -> (f64, f64) {
    let span = hi - lo;
    let center = lo + frac * span;
    let new_span = span / factor;
    let mut new_lo = center - frac * new_span;
    let mut new_hi = center + (1.0 - frac) * new_span;
    if abs_min.is_finite() && new_lo < abs_min {
        new_lo = abs_min;
    }
    if abs_max.is_finite() && new_hi > abs_max {
        new_hi = abs_max;
    }
    if new_hi - new_lo < 1.0e-6 {
        return (lo, hi); // prevent degenerate zoom
    }
    (new_lo, new_hi)
}

fn auto_bounds(data: &[f64]) -> (f64, f64) {
    if data.is_empty() {
        return (0.0, 1.0);
    }
    let y_min = data
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(f64::INFINITY, f64::min);
    let y_max = data
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .fold(f64::NEG_INFINITY, f64::max);
    // If all values were non-finite, return safe defaults
    if !y_min.is_finite() || !y_max.is_finite() || y_min > y_max {
        return (0.0, 1.0);
    }
    let range = y_max - y_min;
    let pad = if range == 0.0 { 1.0 } else { range * 0.1 };
    (y_min - pad, y_max + pad)
}

/// Compute FFT magnitude spectrum using rustfft (O(N log N)).
/// Returns magnitudes for the first N/2 frequency bins (DC to Nyquist).
pub fn compute_fft_magnitude(data: &[f64]) -> Vec<f64> {
    let n = data.len();
    if n == 0 {
        return Vec::new();
    }

    // Remove DC offset (mean) for better FFT visualization
    let mean = data.iter().sum::<f64>() / n as f64;

    let mut buffer: Vec<Complex<f64>> = data
        .iter()
        .map(|&v| {
            let val = if v.is_finite() { v - mean } else { 0.0 };
            Complex::new(val, 0.0)
        })
        .collect();

    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(n);
    fft.process(&mut buffer);

    let half = n / 2;
    let inv_n = 1.0 / n as f64;
    buffer[..half].iter().map(|c| c.norm() * inv_n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── #82: editing a binary string must not destroy it ────

    /// An `App` sitting on one selected string key holding `bytes`.
    fn app_on_string_key(bytes: &[u8]) -> App {
        let mut app = App::new();
        app.keys = vec!["blob:float32_1k".to_string()];
        app.key_list_state.select(Some(0));
        app.current_key_info = Some(KeyInfo {
            name: "blob:float32_1k".to_string(),
            key_type: "string".to_string(),
            ttl: -1,
            size: bytes.len() as i64,
            encoding: "raw".to_string(),
        });
        app.current_value = Some(RedisValue::String(bytes.to_vec()));
        app
    }

    /// 1000 little-endian float32 samples, as `start-dev.sh` seeds.
    fn float32_blob() -> Vec<u8> {
        (0..1000u32)
            .flat_map(|i| ((i as f32) * 0.01).sin().to_le_bytes())
            .collect()
    }

    // The reported bug: `s` then Enter, typing nothing, rewrote the key through
    // `String::from_utf8_lossy` and grew a 4000-byte blob to 6847 bytes of
    // U+FFFD. Nothing lossy may reach the edit field.
    #[test]
    fn editing_a_non_utf8_string_does_not_prefill_lossy_text() {
        let blob = float32_blob();
        assert!(std::str::from_utf8(&blob).is_err(), "blob must be binary");

        let mut app = app_on_string_key(&blob);
        app.start_edit();

        assert_eq!(app.edit_fields.len(), 1);
        let field = &app.edit_fields[0].1;
        assert!(
            !field.contains('\u{FFFD}'),
            "replacement chars reached the edit field: {:?}",
            field
        );
        assert!(
            field.is_empty(),
            "field should start empty, got {:?}",
            field
        );
    }

    #[test]
    fn editing_a_non_utf8_string_opens_in_binary_mode() {
        let mut app = app_on_string_key(&float32_blob());
        app.start_edit();
        assert!(
            app.edit_binary_mode,
            "binary mode must be on so the write path encodes bytes"
        );
        assert_eq!(app.input_mode, InputMode::Edit);
    }

    #[test]
    fn editing_a_non_utf8_string_says_why() {
        let mut app = app_on_string_key(&float32_blob());
        app.start_edit();
        assert!(
            app.status_message.contains("non-UTF-8"),
            "user needs to know why the field is empty: {:?}",
            app.status_message
        );
    }

    // The ordinary case must be untouched: text keys still pre-fill as text.
    #[test]
    fn editing_a_utf8_string_still_prefills_the_text() {
        let mut app = app_on_string_key(b"hello world");
        app.start_edit();
        assert_eq!(app.edit_fields[0].1, "hello world");
        assert!(!app.edit_binary_mode);
    }

    // Binary mode is sticky per-edit, so a text key opened after a binary one
    // must not inherit it.
    #[test]
    fn a_text_key_opened_after_a_binary_one_is_not_left_in_binary_mode() {
        let mut app = app_on_string_key(&float32_blob());
        app.start_edit();
        assert!(app.edit_binary_mode);

        app.cancel_edit();
        app.current_value = Some(RedisValue::String(b"plain text".to_vec()));
        app.start_edit();
        assert!(
            !app.edit_binary_mode,
            "binary mode leaked from the previous edit"
        );
    }

    // ─── #96: a stopped listener leaves nothing behind ───────

    #[test]
    fn stopping_a_listener_drops_its_rate_tracker() {
        let mut app = App::new();
        let e = StreamEntry {
            id: "1000-0".to_string(),
            fields: vec![("_".to_string(), vec![0u8; 8])],
        };
        app.update_rate_tracker("s1", std::slice::from_ref(&e), 1_000);
        assert!(app.rate_trackers.contains_key("s1"), "precondition");

        app.stop_listening("s1");

        assert!(
            !app.rate_trackers.contains_key("s1"),
            "a frozen tracker keeps rendering a dead key and makes a later \
             re-listen record a spurious gap"
        );
    }

    // ─── #88: a backlog must not read as zero ────────────────

    /// A tracker holding `n` entries spaced `step_ms` apart, ending at `newest`.
    fn tracker_ending_at(newest: u64, n: u64, step_ms: u64) -> RateTracker {
        let mut t = RateTracker::default();
        for i in 0..n {
            t.entry_timestamps.push_back(newest - (n - 1 - i) * step_ms);
        }
        t.last_entry_ms = Some(newest);
        t
    }

    // The reported bug: the drain falls behind, so the newest entry the app has
    // processed is older than the averaging window. Windowing on wall-clock
    // time then counted nothing and every window read 0.0 -- while the status
    // bar still printed "+20 entries (live)". The rate must describe the
    // stream's own timeline.
    #[test]
    fn a_backlogged_rate_does_not_read_zero() {
        // 100 entries across one second, processed 30s late.
        let newest = 1_000_000_000u64;
        let t = tracker_ending_at(newest, 100, 10);
        let now = newest + 30_000;

        let r = t.rate_for_window(1, now);
        assert!(r > 0.0, "a backlog must not read as no traffic, got {}", r);
        assert!((r - 100.0).abs() < 1.0, "expected ~100/s, got {}", r);
    }

    // A stream that is keeping up must be unaffected.
    #[test]
    fn a_current_rate_is_unchanged() {
        let newest = 1_000_000_000u64;
        let t = tracker_ending_at(newest, 100, 10);
        let r = t.rate_for_window(1, newest);
        assert!((r - 100.0).abs() < 1.0, "expected ~100/s, got {}", r);
    }

    // Windowing on the stream's clock means a stopped stream keeps reporting
    // its old rate, so how far behind it is has to be visible.
    #[test]
    fn lag_is_reported_so_a_stale_rate_is_not_read_as_current() {
        let newest = 1_000_000_000u64;
        let t = tracker_ending_at(newest, 10, 100);
        assert_eq!(t.lag_ms(newest + 30_000), 30_000);
        assert_eq!(t.lag_ms(newest), 0, "a current stream is not lagging");
        assert!(t.is_lagging(newest + 30_000));
        assert!(!t.is_lagging(newest + 100));
    }

    #[test]
    fn an_empty_tracker_reports_no_rate_and_no_lag() {
        let t = RateTracker::default();
        assert_eq!(t.rate_for_window(1, 1_000), 0.0);
        assert_eq!(t.lag_ms(1_000), 0);
        assert!(!t.is_lagging(1_000));
    }

    // ─── #87: truncation must be visible, not silent ─────────

    fn app_with_loaded_list(shown: usize, total: usize) -> App {
        let mut app = App::new();
        app.current_value = Some(RedisValue::List(
            (0..shown)
                .map(|i| format!("item-{}", i).into_bytes())
                .collect(),
        ));
        app.value_total_items = Some(total);
        app
    }

    // Showing 1000 of 5,000,000 without saying so is the same class of quiet
    // wrongness as #82: the user reads partial data as complete.
    #[test]
    fn a_capped_collection_reports_what_is_hidden() {
        let app = app_with_loaded_list(1000, 5_000_000);
        assert_eq!(app.value_truncation(), Some((1000, 5_000_000)));
    }

    #[test]
    fn a_whole_collection_reports_no_truncation() {
        let app = app_with_loaded_list(7, 7);
        assert_eq!(app.value_truncation(), None);
    }

    // A string has no item count, so nothing may claim it was truncated.
    #[test]
    fn a_string_never_reports_truncation() {
        let mut app = App::new();
        app.current_value = Some(RedisValue::String(b"hello".to_vec()));
        app.value_total_items = None;
        assert_eq!(app.value_truncation(), None);
    }

    // ─── #85: multi-byte key names must not panic ────────────

    // `str::len()` is bytes but `str` indexing needs a char boundary, so
    // `&name[..10]` on a name whose byte 10 lands mid-character panics. These
    // names are supported input: scan_keys drops only keys that fail UTF-8
    // decoding, so Japanese and Cyrillic names reach the plot slots intact.
    #[test]
    fn plot_settings_survives_a_multi_byte_key_name() {
        let mut app = App::new();
        app.toggle_plot_slot("温度センサー");
        app.plot_slots[0].data = vec![1.0, 2.0, 3.0];

        app.start_plot_settings();

        let labels: Vec<&str> = app.edit_fields.iter().map(|(l, _)| l.as_str()).collect();
        assert!(
            labels.iter().any(|l| l.contains("温度")),
            "the slot's Y limit fields should be labelled with its name: {:?}",
            labels
        );
    }

    // The issue's cheapest repro: `p` then `x` on a key with no plottable data.
    #[test]
    fn plot_settings_survives_a_multi_byte_key_name_with_no_data() {
        let mut app = App::new();
        app.toggle_plot_slot("温度センサー");
        app.start_plot_settings();
    }

    #[test]
    fn a_long_multi_byte_name_is_shortened_without_splitting_a_character() {
        let mut app = App::new();
        app.toggle_plot_slot("温度センサー_ストリーム_データ");
        app.plot_slots[0].data = vec![1.0];

        app.start_plot_settings();

        // X Min/X Max come first; the per-slot Y limits follow.
        let label = app
            .edit_fields
            .iter()
            .map(|(l, _)| l.as_str())
            .find(|l| l.contains("温度"))
            .expect("the slot should contribute a labelled field");
        assert!(
            label.contains("..."),
            "long name should be elided: {}",
            label
        );
        // Every char must have survived intact -- no replacement or panic.
        assert!(label.starts_with("温度センサー"), "got {}", label);
    }

    // ---- #31: plot limit validation rejects non-finite values ----

    fn app_with_plot_fields(fields: &[&str]) -> App {
        let mut app = App::new();
        app.edit_fields = fields
            .iter()
            .map(|v| ("f".to_string(), v.to_string()))
            .collect();
        app
    }

    #[test]
    fn plot_settings_rejects_nan_x_min() {
        // "nan".parse::<f64>() succeeds, and NaN >= x_max is false, so NaN slips
        // past the min/max guard and poisons the chart bounds.
        let mut app = app_with_plot_fields(&["nan", "100", "0", "1"]);
        assert!(app.apply_plot_settings().is_err());
    }

    #[test]
    fn plot_settings_rejects_negative_infinite_x_min() {
        let mut app = app_with_plot_fields(&["-inf", "100", "0", "1"]);
        assert!(app.apply_plot_settings().is_err());
    }

    #[test]
    fn plot_settings_rejects_infinite_x_max() {
        let mut app = app_with_plot_fields(&["0", "inf", "0", "1"]);
        assert!(app.apply_plot_settings().is_err());
    }

    #[test]
    fn plot_settings_rejects_nan_y_min() {
        let mut app = app_with_plot_fields(&["0", "100", "nan", "1"]);
        assert!(app.apply_plot_settings().is_err());
    }

    #[test]
    fn plot_settings_rejects_infinite_y_max() {
        let mut app = app_with_plot_fields(&["0", "100", "0", "inf"]);
        assert!(app.apply_plot_settings().is_err());
    }

    #[test]
    fn plot_settings_rejects_nan_in_per_slot_y() {
        let mut app = App::new();
        app.toggle_plot_slot("mykey");
        app.edit_fields = vec![
            ("X Min".to_string(), "0".to_string()),
            ("X Max".to_string(), "100".to_string()),
            ("Y Min".to_string(), "nan".to_string()),
            ("Y Max".to_string(), "1".to_string()),
        ];
        assert!(app.apply_plot_settings().is_err());
    }

    #[test]
    fn plot_settings_accepts_finite_values() {
        let mut app = app_with_plot_fields(&["0", "100", "-5", "5"]);
        assert!(app.apply_plot_settings().is_ok());
        assert_eq!(app.plot_x_min, 0.0);
        assert_eq!(app.plot_x_max, 100.0);
        assert_eq!(app.plot_y_min, -5.0);
        assert_eq!(app.plot_y_max, 5.0);
    }

    #[test]
    fn toggle_plot_slot_adds_key() {
        let mut app = App::new();
        let added = app.toggle_plot_slot("mykey");
        assert!(added);
        assert_eq!(app.plot_slots.len(), 1);
        assert_eq!(app.plot_slots[0].key_name, "mykey");
        assert_eq!(app.plot_slots[0].color, PLOT_COLORS[0]);
    }

    #[test]
    fn toggle_plot_slot_removes_key() {
        let mut app = App::new();
        app.toggle_plot_slot("mykey");
        let added = app.toggle_plot_slot("mykey");
        assert!(!added);
        assert!(app.plot_slots.is_empty());
    }

    #[test]
    fn toggle_plot_slot_fifo_eviction() {
        let mut app = App::new();
        app.toggle_plot_slot("key1");
        app.toggle_plot_slot("key2");
        app.toggle_plot_slot("key3");
        app.toggle_plot_slot("key4");
        assert_eq!(app.plot_slots.len(), 4);

        // Adding a 5th should evict "key1"
        app.toggle_plot_slot("key5");
        assert_eq!(app.plot_slots.len(), 4);
        assert_eq!(app.plot_slots[0].key_name, "key2");
        assert_eq!(app.plot_slots[3].key_name, "key5");
    }

    #[test]
    fn toggle_plot_slot_colors_reassigned_on_remove() {
        let mut app = App::new();
        app.toggle_plot_slot("key1");
        app.toggle_plot_slot("key2");
        app.toggle_plot_slot("key3");

        // Remove the middle key
        app.toggle_plot_slot("key2");
        assert_eq!(app.plot_slots.len(), 2);
        assert_eq!(app.plot_slots[0].key_name, "key1");
        assert_eq!(app.plot_slots[0].color, PLOT_COLORS[0]);
        assert_eq!(app.plot_slots[1].key_name, "key3");
        assert_eq!(app.plot_slots[1].color, PLOT_COLORS[1]);
    }

    #[test]
    fn key_plotted_check_via_color() {
        let mut app = App::new();
        assert!(app.plot_color_for_key("mykey").is_none());
        app.toggle_plot_slot("mykey");
        assert!(app.plot_color_for_key("mykey").is_some());
    }

    #[test]
    fn plot_color_for_key_returns_correct_color() {
        let mut app = App::new();
        app.toggle_plot_slot("key1");
        app.toggle_plot_slot("key2");
        assert_eq!(app.plot_color_for_key("key1"), Some(PLOT_COLORS[0]));
        assert_eq!(app.plot_color_for_key("key2"), Some(PLOT_COLORS[1]));
        assert_eq!(app.plot_color_for_key("nonexistent"), None);
    }

    #[test]
    fn update_slot_data_with_string_value() {
        let mut app = App::new();
        app.toggle_plot_slot("mykey");
        let value = RedisValue::String(vec![0x01, 0x02, 0x03]);
        app.update_slot_data("mykey", &value);
        assert_eq!(app.plot_slots[0].data, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn fifo_eviction_preserves_data() {
        let mut app = App::new();
        for i in 1..=4 {
            let name = format!("key{}", i);
            app.toggle_plot_slot(&name);
            let value = RedisValue::String(vec![i as u8]);
            app.update_slot_data(&name, &value);
        }

        // Add 5th, key1 evicted
        app.toggle_plot_slot("key5");
        let value = RedisValue::String(vec![5]);
        app.update_slot_data("key5", &value);

        assert_eq!(app.plot_slots[0].key_name, "key2");
        assert_eq!(app.plot_slots[0].data, vec![2.0]);
        assert_eq!(app.plot_slots[3].key_name, "key5");
        assert_eq!(app.plot_slots[3].data, vec![5.0]);
    }

    #[test]
    fn append_stream_entries_caps_at_max() {
        use crate::redis_client::StreamEntry;

        let mut app = App::new();
        // Seed current_value with an existing stream
        let initial: Vec<StreamEntry> = (0..MAX_STREAM_ENTRIES)
            .map(|i| StreamEntry {
                id: format!("{}-0", i),
                fields: vec![("data".to_string(), vec![i as u8])],
            })
            .collect();
        app.current_value = Some(RedisValue::Stream(initial));

        // Append 500 more entries — should trigger truncation
        let new_entries: Vec<StreamEntry> = (0..500)
            .map(|i| StreamEntry {
                id: format!("{}-0", MAX_STREAM_ENTRIES + i),
                fields: vec![("data".to_string(), vec![(i + 100) as u8])],
            })
            .collect();
        let added = app.append_stream_entries(new_entries);
        assert!(added);

        if let Some(RedisValue::Stream(ref entries)) = app.current_value {
            assert_eq!(entries.len(), MAX_STREAM_ENTRIES);
            // Oldest entries should have been drained — first entry should be "500-0"
            assert_eq!(entries[0].id, "500-0");
            // Last entry should be the newest appended
            assert_eq!(
                entries.last().unwrap().id,
                format!("{}-0", MAX_STREAM_ENTRIES + 499)
            );
        } else {
            panic!("expected Stream value");
        }
    }

    #[test]
    fn format_stream_entries_uint8_printable_ascii() {
        use crate::data::{DataType, Endianness};
        use crate::redis_client::StreamEntry;

        // Create stream entry with uint8 data in printable ASCII range (65-90 = 'A'-'Z')
        let entries = vec![StreamEntry {
            id: "1000-0".to_string(),
            fields: vec![("_waveform".to_string(), vec![65, 66, 67, 68, 69])],
        }];
        let result = format_stream_entries(&entries, DataType::UInt8, Endianness::Little);
        let joined = result.join("\n");
        // Should contain decoded numeric values, not ASCII text
        assert!(
            joined.contains("[uint8]"),
            "should show data type label, got:\n{}",
            joined
        );
        assert!(
            joined.contains("65"),
            "should contain decoded value 65, got:\n{}",
            joined
        );
        assert!(
            joined.contains("hex"),
            "should contain hex dump, got:\n{}",
            joined
        );
        // Should NOT contain the ASCII interpretation 'ABCDE'
        assert!(
            !joined.contains("ABCDE"),
            "should not show ASCII text, got:\n{}",
            joined
        );
    }

    #[test]
    fn format_stream_entries_uint16_data() {
        use crate::data::{DataType, Endianness};
        use crate::redis_client::StreamEntry;

        // Create uint16 LE data: 256 = [0x00, 0x01], 512 = [0x00, 0x02]
        let entries = vec![StreamEntry {
            id: "1000-0".to_string(),
            fields: vec![("_data".to_string(), vec![0x00, 0x01, 0x00, 0x02])],
        }];
        let result = format_stream_entries(&entries, DataType::UInt16, Endianness::Little);
        let joined = result.join("\n");
        assert!(
            joined.contains("[uint16]"),
            "should show uint16 label, got:\n{}",
            joined
        );
        assert!(
            joined.contains("256"),
            "should contain decoded value 256, got:\n{}",
            joined
        );
        assert!(
            joined.contains("512"),
            "should contain decoded value 512, got:\n{}",
            joined
        );
    }

    // ── RateTracker tests ──────────────────────────────────────────────────────

    fn make_entry(unix_ms: u64) -> crate::redis_client::StreamEntry {
        crate::redis_client::StreamEntry {
            id: format!("{}-0", unix_ms),
            fields: vec![],
        }
    }

    #[test]
    fn compute_five_num_empty() {
        let empty: VecDeque<(u64, f64)> = VecDeque::new();
        assert!(compute_five_num(&empty).is_none());
    }

    #[test]
    fn compute_five_num_single() {
        let mut h = VecDeque::new();
        h.push_back((0, 5.0));
        let result = compute_five_num(&h).unwrap();
        assert_eq!(result, [5.0, 5.0, 5.0, 5.0, 5.0]);
    }

    #[test]
    fn compute_five_num_known_values() {
        // Sorted: [1, 2, 3, 4, 5] — min=1 Q1=2 med=3 Q3=4 max=5
        let mut h = VecDeque::new();
        for v in [3.0, 1.0, 5.0, 2.0, 4.0] {
            h.push_back((0, v));
        }
        let [min, q1, med, q3, max] = compute_five_num(&h).unwrap();
        assert_eq!(min, 1.0);
        assert_eq!(med, 3.0);
        assert_eq!(max, 5.0);
        assert!((q1 - 2.0).abs() < 1e-9);
        assert!((q3 - 4.0).abs() < 1e-9);
    }

    #[test]
    fn rate_for_window_counts_within_window() {
        let mut tracker = RateTracker::default();
        let now_ms: u64 = 10_000;
        // 5 entries at 1s intervals, all clearly within a 5s window (none at the boundary)
        // now-4000, now-3000, now-2000, now-1000, now → cutoff = now-5000 → all 5 included
        for i in 0..5u64 {
            tracker.entry_timestamps.push_back(now_ms - (4 - i) * 1000);
        }
        let rate = tracker.rate_for_window(5, now_ms);
        assert!((rate - 1.0).abs() < 1e-9, "expected 1.0/s, got {}", rate);
    }

    #[test]
    fn rate_for_window_zero_window() {
        let tracker = RateTracker::default();
        assert_eq!(tracker.rate_for_window(0, 1000), 0.0);
    }

    #[test]
    fn update_rate_tracker_warmup_gates_history() {
        let mut app = App::new();
        app.rate_avg_window_secs = 5; // 5s warmup

        // now_ms = base_ms: first call sets tracking_start_ms = base_ms; 0s elapsed < 5s warmup
        let base_ms: u64 = 1_700_000_000_000; // realistic timestamp
        let entries: Vec<_> = (0..10).map(|i| make_entry(base_ms + i * 100)).collect();
        app.update_rate_tracker("k", &entries, base_ms);

        let tracker = app.rate_trackers.get("k").unwrap();
        assert!(
            tracker.rate_history.is_empty(),
            "rate_history should be empty during warmup"
        );
        assert!(tracker.tracking_start_ms.is_some());
    }

    #[test]
    fn update_rate_tracker_records_after_warmup() {
        let mut app = App::new();
        app.rate_avg_window_secs = 1; // 1s warmup

        let base_ms: u64 = 1_700_000_000_000;
        // First batch — now_ms = base_ms, sets tracking_start_ms = base_ms
        let batch1: Vec<_> = (0..5).map(|i| make_entry(base_ms + i * 100)).collect();
        app.update_rate_tracker("k", &batch1, base_ms);

        // Second call — now_ms is 2s later, well past the 1s warmup
        let now2 = base_ms + 2000;
        let batch2: Vec<_> = (0..5).map(|i| make_entry(now2 + i * 100)).collect();
        app.update_rate_tracker("k", &batch2, now2);

        let tracker = app.rate_trackers.get("k").unwrap();
        assert!(
            !tracker.rate_history.is_empty(),
            "should have recorded rate after warmup elapsed"
        );
    }

    #[test]
    fn update_rate_tracker_gap_detection() {
        let mut app = App::new();
        app.rate_avg_window_secs = 1;

        let base_ms: u64 = 1_700_000_000_000;
        // 300 entries at 100ms apart → fills the 30s reference window at ~10/s
        // With current_rate = 10/s, threshold = (100ms * 1.85).max(500ms) = 500ms
        let batch1: Vec<_> = (0..300).map(|i| make_entry(base_ms + i * 100)).collect();
        app.update_rate_tracker("k", &batch1, base_ms);

        // Jump of 2000ms from last entry (base+29900ms → base+31900ms)
        // gap_ms = 2000ms > threshold 500ms → gap detected
        let last_ms = base_ms + 299 * 100; // base + 29900
        let gap_start = last_ms + 2000; // base + 31900
        let now2 = base_ms + 35_000;
        let batch2: Vec<_> = (0..5).map(|i| make_entry(gap_start + i * 100)).collect();
        app.update_rate_tracker("k", &batch2, now2);

        let tracker = app.rate_trackers.get("k").unwrap();
        assert_eq!(tracker.gaps.len(), 1, "should detect exactly one gap");
        assert_eq!(tracker.gaps[0], gap_start);
    }

    #[test]
    fn update_rate_tracker_prunes_gaps_to_history_window() {
        let mut app = App::new();
        app.rate_avg_window_secs = 1;
        app.rate_history_secs = 10; // 10s history

        let base_ms: u64 = 1_700_000_000_000;

        // Batch 1: sets tracking start
        let batch1: Vec<_> = (0..10).map(|i| make_entry(base_ms + i * 100)).collect();
        app.update_rate_tracker("k", &batch1, base_ms);

        // Inject an old gap that's outside the 10s history window
        let old_gap = base_ms - 20_000; // 20s before base
        app.rate_trackers.get_mut("k").unwrap().gaps.push(old_gap);
        assert_eq!(app.rate_trackers["k"].gaps.len(), 1);

        // now_ms = base + 15s → history_cutoff = base + 15s - 10s = base + 5s
        // old_gap = base - 20s → old_gap < cutoff → should be pruned
        let now2 = base_ms + 15_000;
        let batch2: Vec<_> = (0..5).map(|i| make_entry(now2 + i * 100)).collect();
        app.update_rate_tracker("k", &batch2, now2);

        let tracker = app.rate_trackers.get("k").unwrap();
        assert!(
            !tracker.gaps.contains(&old_gap),
            "old gap should have been pruned"
        );
    }

    #[test]
    fn clear_rate_tracker_removes_entry() {
        let mut app = App::new();
        app.rate_avg_window_secs = 1;
        let base_ms: u64 = 1_700_000_000_000;
        let entries: Vec<_> = (0..3).map(|i| make_entry(base_ms + i * 1000)).collect();
        app.update_rate_tracker("k", &entries, base_ms);
        assert!(app.rate_trackers.contains_key("k"));
        app.clear_rate_tracker("k");
        assert!(!app.rate_trackers.contains_key("k"));
    }
}
