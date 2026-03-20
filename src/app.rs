use crate::data::{DataType, Endianness, decode_blob, encode_values, is_binary};
use crate::redis_client::{KeyInfo, MultiRedisClient, RedisValue, StreamEntry};
use ratatui::style::Color;
use ratatui::widgets::ListState;
use rustfft::{FftPlanner, num_complex::Complex};
use std::sync::mpsc;

/// Maximum number of keys that can be plotted simultaneously
pub const MAX_PLOT_SLOTS: usize = 4;

/// Maximum number of stream entries kept in memory per key.
/// Only the newest entries are needed for display (last 5 shown)
/// and plot extraction (last entry's waveform).
pub const MAX_STREAM_ENTRIES: usize = 10;

/// Colors assigned to each plot slot
pub const PLOT_COLORS: [Color; MAX_PLOT_SLOTS] = [
    Color::Cyan,
    Color::Yellow,
    Color::Green,
    Color::Magenta,
];

/// A slot in the multi-key plot FIFO
#[derive(Clone)]
pub struct PlotSlot {
    pub key_name: String,
    pub data: Vec<f64>,
    pub color: Color,
    pub data_type: DataType,
    pub endianness: Endianness,
    pub y_min: Option<f64>,  // None = auto
    pub y_max: Option<f64>,  // None = auto
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PlotFocus {
    Signal,
    FFT,
}

pub const KEY_TYPES: &[&str] = &["string", "hash", "list", "set", "zset", "stream"];
pub const WAVE_TYPES: &[&str] = &["sine", "square", "sawtooth", "triangle"];

/// Default number of data points to show in auto-range plot mode
pub const PLOT_WINDOW: usize = 2000;

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
    pub plot_data: Vec<f64>,            // primary key's plot data (backward compat)
    pub plot_slots: Vec<PlotSlot>,      // multi-key plot FIFO (up to MAX_PLOT_SLOTS)
    pub listening_keys: Vec<String>,    // keys with active stream listeners (set by main loop)
    pub siggen_keys: Vec<String>,       // keys with active signal generators (set by main loop)
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
    pub mouse_x: u16,         // terminal column
    pub mouse_y: u16,         // terminal row
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
    pub hover_in_fft: bool,   // true if hovering in FFT chart

    // Panel area rects (set during draw)
    pub key_list_area: Option<(u16, u16, u16, u16)>,     // x, y, w, h
    pub value_view_area: Option<(u16, u16, u16, u16)>,   // x, y, w, h
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
    pub confirm_action: Option<String>,

    // Edit state
    pub edit_operation: Option<EditOperation>,
    pub edit_fields: Vec<(String, String)>, // (label, value)
    pub edit_focus: usize,
    pub edit_key: String,          // the key being edited
    pub edit_multi_count: usize,   // how many entries submitted in this session
    pub new_key_type_idx: usize,   // index into KEY_TYPES for new key creation
    pub edit_binary_mode: bool,    // encode values as binary blobs
    pub edit_binary_dtype_idx: usize, // index into DataType::all() for binary encoding

    // Signal generator state
    pub signal_gen_fields: Vec<(String, String)>,
    pub signal_gen_focus: usize,
    pub signal_gen_wave_idx: usize,
    pub signal_gen_dtype_idx: usize,
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

            signal_gen_fields: Vec::new(),
            signal_gen_focus: 0,
            signal_gen_wave_idx: 0,
            signal_gen_dtype_idx: 7, // float32 index in DataType::all()
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
        self.plot_slots.iter().find(|s| s.key_name == key_name).map(|s| s.color)
    }

    /// Update plot data for a specific slot by key name, using the slot's own data type
    pub fn update_slot_data(&mut self, key_name: &str, value: &RedisValue) {
        if let Some(slot) = self.plot_slots.iter_mut().find(|s| s.key_name == key_name) {
            let dt = slot.data_type;
            let en = slot.endianness;
            slot.data = match value {
                RedisValue::String(bytes) => {
                    decode_blob(bytes, dt, en)
                }
                RedisValue::Stream(entries) => {
                    extract_stream_plot_data(entries, dt, en)
                }
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
                RedisValue::ZSet(pairs) => {
                    pairs.iter().map(|(_, score)| *score).collect()
                }
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

    pub fn refresh_keys(&mut self, client: &mut MultiRedisClient) {
        match client.scan_keys(&self.filter_pattern) {
            Ok(keys) => {
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

                if collision_count > 0 {
                    self.status_message = format!(
                        "Loaded {} keys ({} collisions!)",
                        self.keys.len(),
                        collision_count
                    );
                } else {
                    self.status_message = format!("Loaded {} keys", self.keys.len());
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
            }
            Err(e) => {
                self.status_message = format!("Error scanning keys: {}", e);
            }
        }

        self.db_size = client.get_db_size().unwrap_or(0);
        self.host_count = client.host_count();
        self.hosts_connected = client.num_connected();
        self.connected = client.is_connected();
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

                match client.get_value(key) {
                    Ok(mut value) => {
                        // Cap stream entries to prevent OOM on large streams
                        cap_stream_entries(&mut value);
                        // Track last stream ID for XREAD polling
                        if let RedisValue::Stream(ref entries) = value {
                            self.last_stream_id =
                                entries.last().map(|e| e.id.clone());
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
                        self.plot_data.clear();
                        self.last_stream_id = None;
                    }
                }
            }
        }
    }

    /// Append new stream entries from XREAD into the current value.
    /// Returns true if new entries were added.
    pub fn append_stream_entries(&mut self, new_entries: Vec<crate::redis_client::StreamEntry>) -> bool {
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

    /// Reload the current value without resetting scroll.
    #[allow(dead_code)]
    pub fn refresh_selected_value(&mut self, client: &mut MultiRedisClient) {
        if let Some(idx) = self.key_list_state.selected() {
            if idx < self.keys.len() {
                let key = self.keys[idx].clone();

                if let Ok(info) = client.get_key_info(&key) {
                    self.current_key_info = Some(info);
                }

                match client.get_value(&key) {
                    Ok(mut value) => {
                        cap_stream_entries(&mut value);
                        if let RedisValue::Stream(ref entries) = value {
                            self.last_stream_id =
                                entries.last().map(|e| e.id.clone());
                        }
                        self.update_plot_data(&value);
                        if self.fft_enabled {
                            self.compute_fft();
                        }
                        self.current_value = Some(value);
                    }
                    Err(_) => {}
                }
            }
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
            RedisValue::String(bytes) => {
                decode_blob(bytes, self.data_type, self.endianness)
            }
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
        let new_type = if forward { self.data_type.next() } else { self.data_type.prev() };
        // Update the slot for the selected key if it's plotted
        if let Some(key) = self.selected_key_name().map(|s| s.to_string()) {
            if let Some(slot) = self.plot_slots.iter_mut().find(|s| s.key_name == key) {
                slot.data_type = if forward { slot.data_type.next() } else { slot.data_type.prev() };
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
                std::thread::spawn(move || { let _ = h.join(); });
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
                std::thread::spawn(move || { let _ = h.join(); });
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
            let (ny0, ny1) = zoom_range(y0, y1, factor, center_frac_y, f64::NEG_INFINITY, f64::INFINITY);
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
            let (ny0, ny1) = zoom_range(y0, y1, factor, center_frac_y, f64::NEG_INFINITY, f64::INFINITY);
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
                let (y0, y1) = if self.plot_auto_limits {
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

    pub fn apply_plot_limits(&mut self) -> Result<(), String> {
        let y_min: f64 = self.edit_fields[0]
            .1
            .trim()
            .parse()
            .map_err(|_| "Invalid Y Min".to_string())?;
        let y_max: f64 = self.edit_fields[1]
            .1
            .trim()
            .parse()
            .map_err(|_| "Invalid Y Max".to_string())?;
        if y_min >= y_max {
            return Err("Y Min must be less than Y Max".to_string());
        }
        match self.plot_focus {
            PlotFocus::Signal => {
                self.plot_y_min = y_min;
                self.plot_y_max = y_max;
                self.plot_auto_limits = false;
            }
            PlotFocus::FFT => {
                self.fft_y_min = y_min;
                self.fft_y_max = y_max;
                self.fft_auto_limits = false;
            }
        }
        Ok(())
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
                let auto_min = slot.data.iter().copied()
                    .filter(|v| v.is_finite()).fold(f64::INFINITY, f64::min);
                let auto_max = slot.data.iter().copied()
                    .filter(|v| v.is_finite()).fold(f64::NEG_INFINITY, f64::max);
                let y_min = slot.y_min.unwrap_or(if auto_min.is_finite() { auto_min } else { 0.0 });
                let y_max = slot.y_max.unwrap_or(if auto_max.is_finite() { auto_max } else { 1.0 });
                let short_name = if slot.key_name.len() > 10 {
                    format!("{}...", &slot.key_name[..10])
                } else {
                    slot.key_name.clone()
                };
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
        let x_min: f64 = self.edit_fields[0].1.trim().parse()
            .map_err(|_| "Invalid X Min".to_string())?;
        let x_max: f64 = self.edit_fields[1].1.trim().parse()
            .map_err(|_| "Invalid X Max".to_string())?;
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
                let y_min: f64 = self.edit_fields[2].1.trim().parse()
                    .map_err(|_| "Invalid Y Min".to_string())?;
                let y_max: f64 = self.edit_fields[3].1.trim().parse()
                    .map_err(|_| "Invalid Y Max".to_string())?;
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
                    let y_min: f64 = self.edit_fields[base].1.trim().parse()
                        .map_err(|_| format!("Invalid Y Min for '{}'", slot.key_name))?;
                    let y_max: f64 = self.edit_fields[base + 1].1.trim().parse()
                        .map_err(|_| format!("Invalid Y Max for '{}'", slot.key_name))?;
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

    pub fn apply_x_limits(&mut self) -> Result<(), String> {
        let x_min: f64 = self.edit_fields[0]
            .1
            .trim()
            .parse()
            .map_err(|_| "Invalid X Min".to_string())?;
        let x_max: f64 = self.edit_fields[1]
            .1
            .trim()
            .parse()
            .map_err(|_| "Invalid X Max".to_string())?;
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
        Ok(())
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
                    lines.push(format!("── Decoded as {} ({}) ──", self.data_type, self.endianness));
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
            Some(RedisValue::List(items)) => {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
                        let s = String::from_utf8_lossy(item);
                        format!("[{}] {}", i, s)
                    })
                    .collect()
            }
            Some(RedisValue::Set(items)) => {
                items
                    .iter()
                    .map(|item| {
                        let s = String::from_utf8_lossy(item);
                        format!("- {}", s)
                    })
                    .collect()
            }
            Some(RedisValue::ZSet(pairs)) => {
                pairs
                    .iter()
                    .map(|(member, score)| {
                        let s = String::from_utf8_lossy(member);
                        format!("{:.4}  {}", score, s)
                    })
                    .collect()
            }
            Some(RedisValue::Hash(pairs)) => {
                pairs
                    .iter()
                    .map(|(field, val)| {
                        let s = String::from_utf8_lossy(val);
                        format!("{}  =>  {}", field, s)
                    })
                    .collect()
            }
            Some(RedisValue::Stream(entries)) => format_stream_entries(entries, self.data_type, self.endianness),
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
                let current = match &self.current_value {
                    Some(RedisValue::String(b)) => String::from_utf8_lossy(b).to_string(),
                    _ => String::new(),
                };
                self.edit_operation = Some(EditOperation::SetString);
                self.edit_fields = vec![("Value".to_string(), current)];
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
                    client.set_bytes(&self.edit_key, &bytes).map_err(|e| e.to_string())
                } else {
                    client.set_string(&self.edit_key, value).map_err(|e| e.to_string())
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
                    client.hset_bytes(&self.edit_key, field, &bytes).map_err(|e| e.to_string())
                } else {
                    client.hset(&self.edit_key, field, value).map_err(|e| e.to_string())
                }
            }
            EditOperation::RPush => {
                let value = &self.edit_fields[0].1;
                if binary_mode {
                    let bytes = encode_values(value, bin_dtype, bin_endian)?;
                    client.rpush_bytes(&self.edit_key, &bytes).map_err(|e| e.to_string())
                } else {
                    client.rpush(&self.edit_key, value).map_err(|e| e.to_string())
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
                    client.lset_bytes(&self.edit_key, index, &bytes).map_err(|e| e.to_string())
                } else {
                    client.lset(&self.edit_key, index, value).map_err(|e| e.to_string())
                }
            }
            EditOperation::SAdd => {
                let member = &self.edit_fields[0].1;
                if binary_mode {
                    let bytes = encode_values(member, bin_dtype, bin_endian)?;
                    client.sadd_bytes(&self.edit_key, &bytes).map_err(|e| e.to_string())
                } else {
                    client.sadd(&self.edit_key, member).map_err(|e| e.to_string())
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
                    client.zadd_bytes(&self.edit_key, score, &bytes).map_err(|e| e.to_string())
                } else {
                    client.zadd(&self.edit_key, score, member).map_err(|e| e.to_string())
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
                    client.xadd_binary(&self.edit_key, field, &bytes).map_err(|e| e.to_string())
                } else {
                    client.xadd(&self.edit_key, field, value).map_err(|e| e.to_string())
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
                        "hash" => client.hset_bytes(key, "field", &bytes).map_err(|e| e.to_string()),
                        "list" => client.rpush_bytes(key, &bytes).map_err(|e| e.to_string()),
                        "set" => client.sadd_bytes(key, &bytes).map_err(|e| e.to_string()),
                        "zset" => client.zadd_bytes(key, 0.0, &bytes).map_err(|e| e.to_string()),
                        "stream" => client.xadd_binary(key, "data", &bytes).map_err(|e| e.to_string()),
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
            ("Cycles/Entry".to_string(), "1.0".to_string()),
            ("Amplitude".to_string(), "1.0".to_string()),
            ("Noise".to_string(), "0.0".to_string()),
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
        (DataType::Int16, Endianness::Little) => (val.clamp(-32768.0, 32767.0) as i16).to_le_bytes().to_vec(),
        (DataType::Int16, Endianness::Big) => (val.clamp(-32768.0, 32767.0) as i16).to_be_bytes().to_vec(),
        (DataType::UInt16, Endianness::Little) => (val.clamp(0.0, 65535.0) as u16).to_le_bytes().to_vec(),
        (DataType::UInt16, Endianness::Big) => (val.clamp(0.0, 65535.0) as u16).to_be_bytes().to_vec(),
        (DataType::Int32, Endianness::Little) => (val.clamp(-2147483648.0, 2147483647.0) as i32).to_le_bytes().to_vec(),
        (DataType::Int32, Endianness::Big) => (val.clamp(-2147483648.0, 2147483647.0) as i32).to_be_bytes().to_vec(),
        (DataType::UInt32, Endianness::Little) => (val.clamp(0.0, 4294967295.0) as u32).to_le_bytes().to_vec(),
        (DataType::UInt32, Endianness::Big) => (val.clamp(0.0, 4294967295.0) as u32).to_be_bytes().to_vec(),
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
                if (2.0 * std::f64::consts::PI * phase).sin() >= 0.0 { 1.0 } else { -1.0 }
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

fn format_stream_entries(entries: &[StreamEntry], data_type: DataType, endianness: Endianness) -> Vec<String> {
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
                    let preview: Vec<String> = decoded.iter().take(8).map(|v| {
                        match data_type {
                            DataType::Float32 | DataType::Float64 => format!("{:.4}", v),
                            _ => format!("{}", *v as i64),
                        }
                    }).collect();
                    let suffix = if decoded.len() > 8 {
                        format!(" ..({} vals)", decoded.len())
                    } else {
                        String::new()
                    };
                    lines.push(format!("  {} [{}]: [{}]{}",
                        fname, data_type, preview.join(", "), suffix));
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
    let y_min = data.iter().copied().filter(|v| v.is_finite()).fold(f64::INFINITY, f64::min);
    let y_max = data.iter().copied().filter(|v| v.is_finite()).fold(f64::NEG_INFINITY, f64::max);
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
    buffer[..half]
        .iter()
        .map(|c| c.norm() * inv_n)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(entries.last().unwrap().id, format!("{}-0", MAX_STREAM_ENTRIES + 499));
        } else {
            panic!("expected Stream value");
        }
    }

    #[test]
    fn format_stream_entries_uint8_printable_ascii() {
        use crate::redis_client::StreamEntry;
        use crate::data::{DataType, Endianness};

        // Create stream entry with uint8 data in printable ASCII range (65-90 = 'A'-'Z')
        let entries = vec![StreamEntry {
            id: "1000-0".to_string(),
            fields: vec![
                ("_waveform".to_string(), vec![65, 66, 67, 68, 69]),
            ],
        }];
        let result = format_stream_entries(&entries, DataType::UInt8, Endianness::Little);
        let joined = result.join("\n");
        // Should contain decoded numeric values, not ASCII text
        assert!(joined.contains("[uint8]"), "should show data type label, got:\n{}", joined);
        assert!(joined.contains("65"), "should contain decoded value 65, got:\n{}", joined);
        assert!(joined.contains("hex"), "should contain hex dump, got:\n{}", joined);
        // Should NOT contain the ASCII interpretation 'ABCDE'
        assert!(!joined.contains("ABCDE"), "should not show ASCII text, got:\n{}", joined);
    }

    #[test]
    fn format_stream_entries_uint16_data() {
        use crate::redis_client::StreamEntry;
        use crate::data::{DataType, Endianness};

        // Create uint16 LE data: 256 = [0x00, 0x01], 512 = [0x00, 0x02]
        let entries = vec![StreamEntry {
            id: "1000-0".to_string(),
            fields: vec![
                ("_data".to_string(), vec![0x00, 0x01, 0x00, 0x02]),
            ],
        }];
        let result = format_stream_entries(&entries, DataType::UInt16, Endianness::Little);
        let joined = result.join("\n");
        assert!(joined.contains("[uint16]"), "should show uint16 label, got:\n{}", joined);
        assert!(joined.contains("256"), "should contain decoded value 256, got:\n{}", joined);
        assert!(joined.contains("512"), "should contain decoded value 512, got:\n{}", joined);
    }
}
