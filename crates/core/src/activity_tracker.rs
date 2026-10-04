//! Sliding-window combat DPS, mining-rate and bounty-rate trackers fed by gamelog lines, plus the
//! `(combat)`/`(mining)`/`(bounty)` line parsers. Platform-neutral (no Win32).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use crate::log::Scope;

const SLOG: Scope = Scope::new("activity_tracker");

const MS_PER_S: i64 = 1000;

/// A single parsed combat event (one damage hit, incoming or outgoing).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CombatEvent {
    pub timestamp_ms: i64,
    pub amount: u32,
    pub is_incoming: bool,
}

/// Ring-buffer capacity per character; 512 entries covers ~8 min at 1 hit/sec, within window_seconds ≤ 600.
pub const RING_CAPACITY: usize = 512;

/// Guards against simultaneous multi-module/multi-weapon log lines spiking a rate computed over a near-zero span.
pub const MIN_RATE_SPAN_MS: i64 = 3 * MS_PER_S;

/// Rate multiplier that decays 1.0 -> 0.0 as idle time crosses the window's second half, instead of holding flat then cutting to zero.
fn idle_decay_factor(now_ms: i64, last_activity_ms: i64, window_ms: i64) -> f32 {
    let idle_ms = now_ms - last_activity_ms;
    let grace_ms = window_ms / 2;
    if idle_ms <= grace_ms {
        return 1.0;
    }
    let decay_span_ms = window_ms - grace_ms;
    let over_ms = (idle_ms - grace_ms).min(decay_span_ms);
    1.0 - over_ms as f32 / decay_span_ms as f32
}

/// Ring-buffer entries that carry the time they were recorded.
trait Timestamped {
    fn timestamp_ms(&self) -> i64;
}

/// Walks a ring buffer newest-first: `head` is the next write slot and `count` the valid entry count.
fn newest_first<T>(entries: &[T], head: usize, count: usize) -> impl Iterator<Item = &T> {
    let len = entries.len();
    (0..count).map(move |i| &entries[(head + len - 1 - i) % len])
}

/// Sums `field(entry)` over the window ending at now_ms, decayed by idle_decay_factor. None means not enough span yet to trust a rate.
#[allow(clippy::too_many_arguments)]
fn compute_window_rate<T: Timestamped>(
    entries: &[T],
    head: usize,
    count: usize,
    window_ms: i64,
    last_hit_ms: i64,
    now_ms: i64,
    field: impl Fn(&T) -> f32,
) -> Option<f32> {
    if last_hit_ms == 0 || now_ms - last_hit_ms >= window_ms {
        return Some(0.0);
    }
    let cutoff = now_ms - window_ms;
    let mut total: f32 = 0.0;
    let mut newest_ms: i64 = 0;
    let mut oldest_ms: i64 = 0;

    for (i, entry) in newest_first(entries, head, count).enumerate() {
        if entry.timestamp_ms() < cutoff {
            break;
        }
        if i == 0 {
            newest_ms = entry.timestamp_ms();
        }
        oldest_ms = entry.timestamp_ms();
        total += field(entry);
    }
    let span_ms = newest_ms - oldest_ms;
    if span_ms < MIN_RATE_SPAN_MS {
        return None;
    }

    let window_secs = window_ms.min(span_ms) as f32 / 1000.0;
    Some((total / window_secs) * idle_decay_factor(now_ms, last_hit_ms, window_ms))
}

/// True if a cached rate moved by >= 0.1, or crossed to/from None.
fn rate_changed(old: Option<f32>, new: Option<f32>) -> bool {
    match (old, new) {
        (Some(old), Some(new)) => (new - old).abs() >= 0.1,
        (Some(_), None) => true,
        (None, new) => new.is_some(),
    }
}

impl Timestamped for CombatEvent {
    fn timestamp_ms(&self) -> i64 {
        self.timestamp_ms
    }
}

/// Incoming and outgoing DPS. None means not enough span yet to trust a rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dps {
    pub incoming: Option<f32>,
    pub outgoing: Option<f32>,
}

impl Dps {
    const ZERO: Dps = Dps { incoming: Some(0.0), outgoing: Some(0.0) };
}

/// Per-character sliding-window DPS accumulator with zero heap allocations after init.
#[derive(Debug, Clone)]
pub struct CombatWindow {
    entries: [CombatEvent; RING_CAPACITY],
    /// Next write slot, wraps mod RING_CAPACITY.
    head: usize,
    /// Valid entry count, saturates at RING_CAPACITY.
    count: usize,
    pub window_ms: i64,
    pub last_hit_ms: i64,
    pub last_incoming_hit_ms: i64,
    /// 0 = never fired.
    pub last_damage_alert_ms: i64,
    /// Per-direction activity clocks for idle_decay_factor; unlike last_incoming_hit_ms, not gated by counts_for_alert.
    pub last_incoming_activity_ms: i64,
    pub last_outgoing_activity_ms: i64,

    // None means not enough span yet to trust a rate.
    pub last_incoming_dps: Option<f32>,
    pub last_outgoing_dps: Option<f32>,
}

impl CombatWindow {
    pub fn new(window_seconds: u32) -> Self {
        Self {
            entries: [CombatEvent::default(); RING_CAPACITY],
            head: 0,
            count: 0,
            window_ms: i64::from(window_seconds) * MS_PER_S,
            last_hit_ms: 0,
            last_incoming_hit_ms: 0,
            last_damage_alert_ms: 0,
            last_incoming_activity_ms: 0,
            last_outgoing_activity_ms: 0,
            last_incoming_dps: None,
            last_outgoing_dps: None,
        }
    }

    /// Append a new hit to the ring buffer (O(1), overwrites oldest on overflow).
    /// `counts_for_alert` only gates `last_incoming_hit_ms` (check_damage_alert's trigger) — the hit is always ring-buffered so DPS stays accurate for filtered hits.
    pub fn add_entry(&mut self, amount: u32, is_incoming: bool, timestamp_ms: i64, counts_for_alert: bool) {
        self.entries[self.head] = CombatEvent { timestamp_ms, amount, is_incoming };
        self.head = (self.head + 1) % RING_CAPACITY;
        if self.count < RING_CAPACITY {
            self.count += 1;
        }
        if timestamp_ms > self.last_hit_ms {
            self.last_hit_ms = timestamp_ms;
        }
        if is_incoming {
            if counts_for_alert && timestamp_ms > self.last_incoming_hit_ms {
                self.last_incoming_hit_ms = timestamp_ms;
            }
            if timestamp_ms > self.last_incoming_activity_ms {
                self.last_incoming_activity_ms = timestamp_ms;
            }
        } else if timestamp_ms > self.last_outgoing_activity_ms {
            self.last_outgoing_activity_ms = timestamp_ms;
        }
    }

    /// Fires when incoming damage has landed since the last alert; repeat-rate is the Notifications tab's throttle, not this. Stays silent once combat stops instead of repeating on a timer.
    pub fn check_damage_alert(&mut self, now_ms: i64) -> bool {
        if self.last_incoming_hit_ms == 0 {
            return false;
        }
        if self.last_incoming_hit_ms <= self.last_damage_alert_ms {
            return false;
        }
        self.last_damage_alert_ms = now_ms;
        true
    }

    /// Compute incoming and outgoing DPS over the sliding window ending at now_ms. None means not enough span yet to trust a rate.
    pub fn compute_dps(&self, now_ms: i64) -> Dps {
        // Short-circuit: if the newest hit is already outside the window, skip the O(n) walk over stale entries.
        if self.last_hit_ms == 0 || now_ms - self.last_hit_ms >= self.window_ms {
            return Dps::ZERO;
        }
        let cutoff = now_ms - self.window_ms;
        let mut in_total: u64 = 0;
        let mut out_total: u64 = 0;
        let mut newest_ms: i64 = 0;
        let mut oldest_ms: i64 = 0;

        for (i, entry) in newest_first(&self.entries, self.head, self.count).enumerate() {
            // Ring is chronologically ordered, so all further entries are also expired
            if entry.timestamp_ms < cutoff {
                break;
            }
            if i == 0 {
                newest_ms = entry.timestamp_ms;
            }
            oldest_ms = entry.timestamp_ms;
            if entry.is_incoming {
                in_total += u64::from(entry.amount);
            } else {
                out_total += u64::from(entry.amount);
            }
        }
        let span_ms = newest_ms - oldest_ms;
        if span_ms < MIN_RATE_SPAN_MS {
            return Dps { incoming: None, outgoing: None };
        }

        let window_secs = self.window_ms.min(span_ms) as f32 / 1000.0;
        let in_factor = idle_decay_factor(now_ms, self.last_incoming_activity_ms, self.window_ms);
        let out_factor = idle_decay_factor(now_ms, self.last_outgoing_activity_ms, self.window_ms);
        Dps {
            incoming: Some((in_total as f32 / window_secs) * in_factor),
            outgoing: Some((out_total as f32 / window_secs) * out_factor),
        }
    }
}

/// A per-character sliding window the shared tracker plumbing can create and refresh.
pub trait TrackerWindow {
    fn new(window_seconds: u32) -> Self;
    /// Recompute the cached rate(s) against now_ms; true if any changed by >= 0.1 or crossed to/from None.
    fn refresh(&mut self, now_ms: i64) -> bool;
}

impl TrackerWindow for CombatWindow {
    fn new(window_seconds: u32) -> Self {
        CombatWindow::new(window_seconds)
    }

    /// Recompute DPS, update last_ fields. Returns true if either value changed by >= 0.1, or crossed to/from None.
    fn refresh(&mut self, now_ms: i64) -> bool {
        let new = self.compute_dps(now_ms);
        let in_changed = rate_changed(self.last_incoming_dps, new.incoming);
        let out_changed = rate_changed(self.last_outgoing_dps, new.outgoing);
        self.last_incoming_dps = new.incoming;
        self.last_outgoing_dps = new.outgoing;
        in_changed || out_changed
    }
}

/// The tracker mutex was poisoned by a panic on another thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockPoisoned;

impl fmt::Display for LockPoisoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("tracker mutex poisoned")
    }
}

impl std::error::Error for LockPoisoned {}

/// Shared mutex/hashmap plumbing for a per-character sliding-window tracker.
struct TrackerBase<W> {
    windows: Mutex<HashMap<String, W>>,
    window_seconds: u32,
}

impl<W: TrackerWindow> TrackerBase<W> {
    fn new(window_seconds: u32) -> Self {
        Self { windows: Mutex::new(HashMap::new()), window_seconds }
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, W>>, LockPoisoned> {
        self.windows.lock().map_err(|_| LockPoisoned)
    }

    fn remove_character(&self, character_name: &str) {
        let mut windows = match self.lock() {
            Ok(w) => w,
            Err(err) => {
                SLOG.warn(format_args!("Failed to lock tracker mutex removing '{character_name}': {err}"));
                return;
            }
        };
        windows.remove(character_name);
    }

    fn refresh_all(&self, now_ms: i64) -> bool {
        let mut windows = match self.lock() {
            Ok(w) => w,
            Err(err) => {
                SLOG.warn(format_args!("Failed to lock tracker mutex refreshing windows: {err}"));
                return false;
            }
        };
        let mut any_changed = false;
        for window in windows.values_mut() {
            if window.refresh(now_ms) {
                any_changed = true;
            }
        }
        any_changed
    }

    /// Runs `f` on character_name's window, creating one via W::new(window_seconds) if absent.
    fn with_or_create<R>(&self, character_name: &str, f: impl FnOnce(&mut W) -> R) -> Result<R, LockPoisoned> {
        let mut windows = self.lock()?;
        if let Some(window) = windows.get_mut(character_name) {
            return Ok(f(window));
        }
        let window = windows.entry(character_name.to_owned()).or_insert_with(|| W::new(self.window_seconds));
        Ok(f(window))
    }

    /// Runs `f` on character_name's window if one exists; logs `what` and returns None if the mutex is poisoned.
    fn with_existing<R>(&self, what: &str, character_name: &str, f: impl FnOnce(&mut W) -> R) -> Option<Option<R>> {
        match self.lock() {
            Ok(mut windows) => Some(windows.get_mut(character_name).map(f)),
            Err(err) => {
                SLOG.warn(format_args!("Failed to lock {what} tracker mutex for '{character_name}': {err}"));
                None
            }
        }
    }
}

/// Multi-character DPS tracker; owns a CombatWindow and key string per character.
/// Thread-safe: the main thread calls refresh_all/get_dps while the chatlog worker thread calls add_entry/remove_character concurrently.
pub struct CombatTracker {
    base: TrackerBase<CombatWindow>,
}

impl CombatTracker {
    pub fn new(window_seconds: u32) -> Self {
        Self { base: TrackerBase::new(window_seconds) }
    }

    /// Record a hit for character_name.  Creates a new window on the first call per character.
    pub fn add_entry(
        &self,
        character_name: &str,
        amount: u32,
        is_incoming: bool,
        timestamp_ms: i64,
        counts_for_alert: bool,
    ) -> Result<(), LockPoisoned> {
        self.base.with_or_create(character_name, |window| {
            window.add_entry(amount, is_incoming, timestamp_ms, counts_for_alert)
        })
    }

    /// Remove a character's window (call on character logout to free the entry).
    pub fn remove_character(&self, character_name: &str) {
        self.base.remove_character(character_name);
    }

    /// Return the last-refreshed DPS values for character_name. None means not enough span yet to trust a rate.
    pub fn get_dps(&self, character_name: &str) -> Dps {
        self.base
            .with_existing("combat", character_name, |window| Dps {
                incoming: window.last_incoming_dps,
                outgoing: window.last_outgoing_dps,
            })
            .flatten()
            .unwrap_or(Dps::ZERO)
    }

    /// Re-evaluate all windows against now_ms.  Returns true if any DPS value changed by >= 0.1.
    pub fn refresh_all(&self, now_ms: i64) -> bool {
        self.base.refresh_all(now_ms)
    }

    /// See CombatWindow::check_damage_alert. Returns false if character_name has no window yet.
    pub fn check_damage_alert(&self, character_name: &str, now_ms: i64) -> bool {
        self.base
            .with_existing("combat", character_name, |window| window.check_damage_alert(now_ms))
            .flatten()
            .unwrap_or(false)
    }
}

/// Parsed `(combat)` hit; `weapon` borrows from the stripped line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedCombatLine<'a> {
    pub amount: u32,
    pub is_incoming: bool,
    pub weapon: &'a str,
}

const BLANKS: &[char] = &[' ', '\t'];

/// Parses a run of ASCII digits after optional leading blanks; returns (amount, end index) or None if
/// anything else comes first. Wraps on overflow like the release build of the Zig original.
fn parse_leading_digits(s: &str) -> Option<(u32, usize)> {
    let mut amount: u32 = 0;
    let mut digits_end: usize = 0;
    let mut found_digit = false;
    for (i, c) in s.bytes().enumerate() {
        if c.is_ascii_digit() {
            amount = amount.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
            digits_end = i + 1;
            found_digit = true;
        } else if found_digit {
            break;
        } else {
            // Allow leading whitespace; anything else is not a damage line
            if c != b' ' && c != b'\t' {
                return None;
            }
        }
    }
    if !found_digit {
        return None;
    }
    Some((amount, digits_end))
}

/// Parse a `(combat)` gamelog line and extract damage amount + direction.
/// Incoming misses return a zero-amount incoming hit (counts for the Taking Damage alert but not DPS); outgoing misses are ignored entirely.
/// Direction: " from " → incoming, " to " → outgoing, matched against the text following the damage number.
/// `stripped_line` must already have HTML stripped by the caller; the returned `weapon` slice borrows from it.
pub fn parse_combat_line(stripped_line: &str) -> Option<ParsedCombatLine<'_>> {
    const COMBAT_PREFIX: &str = "(combat)";
    let combat_pos = stripped_line.find(COMBAT_PREFIX)?;
    let stripped = stripped_line[combat_pos + COMBAT_PREFIX.len()..].trim_start_matches(BLANKS);

    // Skip remote-rep / cap-transfer lines (these are not damage hits)
    if stripped.contains("boosts your")
        || stripped.contains("shields your")
        || stripped.contains("repairs your")
        || stripped.contains("transfers")
    {
        return None;
    }

    if stripped.contains("misses you") && !stripped.starts_with("You ") {
        return Some(ParsedCombatLine { amount: 0, is_incoming: true, weapon: "" });
    }

    // Rejects lines that start with a non-digit and aren't an incoming miss (handled above).
    let (amount, digits_end) = parse_leading_digits(stripped)?;
    if amount == 0 {
        return None;
    }

    // Direction keyword immediately follows the number (see doc comment above)
    let rest = &stripped[digits_end..];
    let is_incoming = if rest.contains(" from ") {
        true
    } else if rest.contains(" to ") {
        false
    } else {
        return None;
    };

    // Weapon name sits before the trailing hit-quality word; searched from the end because target names can themselves contain " - " (e.g. structure kills), which would otherwise be misread as the weapon segment.
    let mut weapon = "";
    if let Some(quality_dash) = rest.rfind(" - ") {
        let before_quality = &rest[..quality_dash];
        if let Some(weapon_dash) = before_quality.rfind(" - ") {
            weapon = before_quality[weapon_dash + 3..].trim_matches(BLANKS);
        }
    }

    Some(ParsedCombatLine { amount, is_incoming, weapon })
}

/// True if `weapon` case-insensitively contains any comma-separated entry of `excluded_csv`; empty entries are skipped so trailing/stray commas don't match everything.
pub fn is_weapon_excluded(weapon: &str, excluded_csv: &str) -> bool {
    if weapon.is_empty() || excluded_csv.is_empty() {
        return false;
    }
    let weapon = weapon.as_bytes();
    for raw_entry in excluded_csv.split(',') {
        let entry = raw_entry.trim_matches(BLANKS).as_bytes();
        if entry.is_empty() || entry.len() > weapon.len() {
            continue;
        }
        if weapon.windows(entry.len()).any(|w| w.eq_ignore_ascii_case(entry)) {
            return true;
        }
    }
    false
}

/// A single parsed mining event (one yield from a mining cycle).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MiningEvent {
    pub timestamp_ms: i64,
    pub m3: f32,
    pub isk: f32,
}

impl Timestamped for MiningEvent {
    fn timestamp_ms(&self) -> i64 {
        self.timestamp_ms
    }
}

/// Per-character sliding-window mining-rate accumulator with zero heap allocations after init.
#[derive(Debug, Clone)]
pub struct MiningWindow {
    entries: [MiningEvent; RING_CAPACITY],
    head: usize,
    count: usize,
    pub window_ms: i64,
    pub last_hit_ms: i64,

    // None means not enough span yet to trust a rate.
    pub last_m3_per_sec: Option<f32>,
    pub last_isk_per_sec: Option<f32>,
    // Timestamp of the last idle-alert fired for this window (ms). 0 = never.
    pub last_alert_ms: i64,
    // Timestamp of the last stopped-alert fired (ms). Reset when mining resumes.
    pub last_stopped_alert_ms: i64,
}

impl MiningWindow {
    pub fn new(window_seconds: u32) -> Self {
        Self {
            entries: [MiningEvent::default(); RING_CAPACITY],
            head: 0,
            count: 0,
            window_ms: i64::from(window_seconds) * MS_PER_S,
            last_hit_ms: 0,
            last_m3_per_sec: None,
            last_isk_per_sec: None,
            last_alert_ms: 0,
            last_stopped_alert_ms: 0,
        }
    }

    /// Append a new yield to the ring buffer (O(1), overwrites oldest on overflow).
    pub fn add_entry(&mut self, m3: f32, isk: f32, timestamp_ms: i64) {
        self.entries[self.head] = MiningEvent { timestamp_ms, m3, isk };
        self.head = (self.head + 1) % RING_CAPACITY;
        if self.count < RING_CAPACITY {
            self.count += 1;
        }
        if timestamp_ms > self.last_hit_ms {
            self.last_hit_ms = timestamp_ms;
        }
        // Mining resumed — allow the stopped alert to fire again next time.
        self.last_stopped_alert_ms = 0;
    }

    /// Count the number of events within the sliding window ending at now_ms.
    pub fn count_events(&self, window_ms: i64, now_ms: i64) -> usize {
        if self.last_hit_ms == 0 || now_ms - self.last_hit_ms >= window_ms {
            return 0;
        }
        let cutoff = now_ms - window_ms;
        newest_first(&self.entries, self.head, self.count)
            .take_while(|e| e.timestamp_ms >= cutoff)
            .count()
    }

    /// Compute m3-per-second over the sliding window ending at now_ms. None means not enough span yet to trust a rate.
    pub fn compute_rate(&self, now_ms: i64) -> Option<f32> {
        compute_window_rate(&self.entries, self.head, self.count, self.window_ms, self.last_hit_ms, now_ms, |e| e.m3)
    }

    /// Compute ISK-per-second over the sliding window ending at now_ms; same walk as compute_rate but summing isk instead of m3. None means not enough span yet to trust a rate.
    pub fn compute_isk_rate(&self, now_ms: i64) -> Option<f32> {
        compute_window_rate(&self.entries, self.head, self.count, self.window_ms, self.last_hit_ms, now_ms, |e| e.isk)
    }
}

impl TrackerWindow for MiningWindow {
    fn new(window_seconds: u32) -> Self {
        MiningWindow::new(window_seconds)
    }

    /// Recompute m3 and ISK rates, updating both cached values. Returns true if the m3 rate changed by >= 0.1 or crossed to/from None -
    /// the ISK rate is driven by the exact same set of window entries, so an unchanged m3 rate means an unchanged ISK rate too.
    fn refresh(&mut self, now_ms: i64) -> bool {
        let new_rate = self.compute_rate(now_ms);
        let changed = rate_changed(self.last_m3_per_sec, new_rate);
        self.last_m3_per_sec = new_rate;
        self.last_isk_per_sec = self.compute_isk_rate(now_ms);
        changed
    }
}

/// Multi-character mining rate tracker; owns a MiningWindow and key string per character.
/// Thread-safe: the main thread calls refresh_all/get_rate/check_idle_alert/check_stopped_alert while the chatlog worker thread calls add_entry/remove_character concurrently.
pub struct MiningTracker {
    base: TrackerBase<MiningWindow>,
}

impl MiningTracker {
    pub fn new(window_seconds: u32) -> Self {
        Self { base: TrackerBase::new(window_seconds) }
    }

    /// Record a yield (m3 and its ISK value) for character_name. Creates a new window on the first call per character.
    pub fn add_entry(&self, character_name: &str, m3: f32, isk: f32, timestamp_ms: i64) -> Result<(), LockPoisoned> {
        self.base.with_or_create(character_name, |window| window.add_entry(m3, isk, timestamp_ms))
    }

    /// Remove a character's window (call on character logout to free the entry).
    pub fn remove_character(&self, character_name: &str) {
        self.base.remove_character(character_name);
    }

    /// Return the last-refreshed mining rate for character_name (m3/sec). None means not enough span yet to trust a rate.
    pub fn get_rate(&self, character_name: &str) -> Option<f32> {
        self.base
            .with_existing("mining", character_name, |window| window.last_m3_per_sec)
            .flatten()
            .unwrap_or(Some(0.0))
    }

    /// Return the last-refreshed ISK rate for character_name (ISK/sec). None means not enough span yet to trust a rate.
    pub fn get_isk_rate(&self, character_name: &str) -> Option<f32> {
        self.base
            .with_existing("mining", character_name, |window| window.last_isk_per_sec)
            .flatten()
            .unwrap_or(Some(0.0))
    }

    /// Re-evaluate all windows against now_ms. Returns true if any rate changed by >= 0.1.
    pub fn refresh_all(&self, now_ms: i64) -> bool {
        self.base.refresh_all(now_ms)
    }

    /// Returns true (and records the alert) if event count within alert_window_ms is <= threshold and the cooldown since the last alert has elapsed.
    pub fn check_idle_alert(&self, character_name: &str, now_ms: i64, alert_window_ms: i64, threshold: u32) -> bool {
        self.base
            .with_existing("mining", character_name, |window| {
                // Only alert if the character has mined at least once (avoids false positives on start-up).
                if window.last_hit_ms == 0 {
                    return false;
                }
                let count = window.count_events(alert_window_ms, now_ms);
                if count > threshold as usize {
                    // Active — reset so we alert again if they go idle later.
                    window.last_alert_ms = 0;
                    return false;
                }
                // Idle: count <= threshold. Fire once per idle episode; re-arms when mining resumes.
                if window.last_alert_ms != 0 {
                    return false;
                }
                window.last_alert_ms = now_ms;
                true
            })
            .flatten()
            .unwrap_or(false)
    }

    /// Returns true once when mining has stopped for at least stopped_window_ms; rearms when mining resumes (add_entry resets last_stopped_alert_ms).
    pub fn check_stopped_alert(&self, character_name: &str, now_ms: i64, stopped_window_ms: i64) -> bool {
        self.base
            .with_existing("mining", character_name, |window| {
                // Only alert if the character has mined at least once.
                if window.last_hit_ms == 0 {
                    return false;
                }
                // Still within the grace window — not stopped yet.
                if now_ms - window.last_hit_ms < stopped_window_ms {
                    return false;
                }
                // Already fired this stopped-episode — wait for mining to resume.
                if window.last_stopped_alert_ms != 0 {
                    return false;
                }
                window.last_stopped_alert_ms = now_ms;
                true
            })
            .flatten()
            .unwrap_or(false)
    }
}

/// A single parsed bounty payout event.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BountyEvent {
    pub timestamp_ms: i64,
    pub isk: f32,
}

impl Timestamped for BountyEvent {
    fn timestamp_ms(&self) -> i64 {
        self.timestamp_ms
    }
}

/// Per-character sliding-window bounty ISK-rate accumulator with zero heap allocations after init. Mirrors MiningWindow's ISK-rate half; there's no m3 twin since bounty payouts already arrive in ISK.
#[derive(Debug, Clone)]
pub struct BountyWindow {
    entries: [BountyEvent; RING_CAPACITY],
    head: usize,
    count: usize,
    pub window_ms: i64,
    pub last_hit_ms: i64,

    pub last_isk_per_sec: Option<f32>,
}

impl BountyWindow {
    pub fn new(window_seconds: u32) -> Self {
        Self {
            entries: [BountyEvent::default(); RING_CAPACITY],
            head: 0,
            count: 0,
            window_ms: i64::from(window_seconds) * MS_PER_S,
            last_hit_ms: 0,
            last_isk_per_sec: None,
        }
    }

    /// Append a new bounty payout to the ring buffer (O(1), overwrites oldest on overflow).
    pub fn add_entry(&mut self, isk: f32, timestamp_ms: i64) {
        self.entries[self.head] = BountyEvent { timestamp_ms, isk };
        self.head = (self.head + 1) % RING_CAPACITY;
        if self.count < RING_CAPACITY {
            self.count += 1;
        }
        if timestamp_ms > self.last_hit_ms {
            self.last_hit_ms = timestamp_ms;
        }
    }

    /// Compute ISK-per-second over the sliding window ending at now_ms. None means not enough span yet to trust a rate.
    pub fn compute_isk_rate(&self, now_ms: i64) -> Option<f32> {
        compute_window_rate(&self.entries, self.head, self.count, self.window_ms, self.last_hit_ms, now_ms, |e| e.isk)
    }
}

impl TrackerWindow for BountyWindow {
    fn new(window_seconds: u32) -> Self {
        BountyWindow::new(window_seconds)
    }

    /// Recompute the ISK rate. Returns true if it changed by >= 0.1 or crossed to/from None.
    fn refresh(&mut self, now_ms: i64) -> bool {
        let new_rate = self.compute_isk_rate(now_ms);
        let changed = rate_changed(self.last_isk_per_sec, new_rate);
        self.last_isk_per_sec = new_rate;
        changed
    }
}

/// Multi-character bounty ISK-rate tracker; owns a BountyWindow and key string per character.
/// Thread-safe: the main thread calls refresh_all/get_isk_rate while the chatlog worker thread calls add_entry/remove_character concurrently.
pub struct BountyTracker {
    base: TrackerBase<BountyWindow>,
}

impl BountyTracker {
    pub fn new(window_seconds: u32) -> Self {
        Self { base: TrackerBase::new(window_seconds) }
    }

    /// Record a bounty payout (ISK) for character_name. Creates a new window on the first call per character.
    pub fn add_entry(&self, character_name: &str, isk: f32, timestamp_ms: i64) -> Result<(), LockPoisoned> {
        self.base.with_or_create(character_name, |window| window.add_entry(isk, timestamp_ms))
    }

    /// Remove a character's window (call on character logout to free the entry).
    pub fn remove_character(&self, character_name: &str) {
        self.base.remove_character(character_name);
    }

    /// Return the last-refreshed ISK rate for character_name (ISK/sec). None means not enough span yet to trust a rate.
    pub fn get_isk_rate(&self, character_name: &str) -> Option<f32> {
        self.base
            .with_existing("bounty", character_name, |window| window.last_isk_per_sec)
            .flatten()
            .unwrap_or(Some(0.0))
    }

    /// Re-evaluate all windows against now_ms. Returns true if any rate changed by >= 0.1.
    pub fn refresh_all(&self, now_ms: i64) -> bool {
        self.base.refresh_all(now_ms)
    }
}

/// Size of the stack buffer the bounty and mining parsers strip HTML into (and that the chatlog strips combat lines into).
pub const STRIP_BUF_LEN: usize = 512;

/// Parses a `(bounty)` gamelog line into the ISK amount added to the next payout; returns None for unrecognised formats.
/// Bounty amounts use comma thousand-separators ("246,153 ISK"), unlike the plain digit runs parse_combat_line/parse_mining_line parse.
pub fn parse_bounty_line(line: &str) -> Option<f32> {
    const BOUNTY_PREFIX: &str = "(bounty)";
    let bounty_pos = line.find(BOUNTY_PREFIX)?;
    let payload = line[bounty_pos + BOUNTY_PREFIX.len()..].trim_start_matches(BLANKS);

    let mut stripped_buf = [0u8; STRIP_BUF_LEN];
    let stripped = strip_html(payload, &mut stripped_buf);

    let mut amount: u32 = 0;
    let mut found_digit = false;
    for c in stripped.bytes() {
        if c.is_ascii_digit() {
            amount = amount.wrapping_mul(10).wrapping_add(u32::from(c - b'0'));
            found_digit = true;
        } else if c == b',' && found_digit {
            continue;
        } else if found_digit {
            break;
        } else if c != b' ' && c != b'\t' {
            return None;
        }
    }
    if !found_digit || amount == 0 {
        return None;
    }
    Some(amount as f32)
}

/// Raw unit count plus the mined ore/ice/gas name, copied by value so it doesn't borrow parse_mining_line's stack-local buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedMiningEvent {
    pub amount: u32,
    name_buf: [u8; 64],
    name_len: u8,
}

impl ParsedMiningEvent {
    pub fn name(&self) -> &str {
        std::str::from_utf8(&self.name_buf[..usize::from(self.name_len)]).unwrap_or("")
    }
}

/// Parses a `(mining)` gamelog line into the mined unit count and ore/ice/gas name; returns None for residue/waste lines, missing tags, or unrecognised formats.
pub fn parse_mining_line(line: &str) -> Option<ParsedMiningEvent> {
    const MINING_PREFIX: &str = "(mining)";
    let mining_pos = line.find(MINING_PREFIX)?;
    let payload = line[mining_pos + MINING_PREFIX.len()..].trim_start_matches(BLANKS);

    let mut stripped_buf = [0u8; STRIP_BUF_LEN];
    let stripped = strip_html(payload, &mut stripped_buf);

    // Skip residue/waste lines – the player does not gain those units
    if stripped.contains("depleted from asteroid as residue") {
        return None;
    }

    // Find "You mined" which appears in both normal and critical lines.
    const MINED_KW: &str = "You mined";
    let mined_pos = stripped.find(MINED_KW)?;
    let mut cursor = stripped[mined_pos + MINED_KW.len()..].trim_start_matches(BLANKS);

    // Skip optional "an additional " prefix (critical yield)
    if let Some(after) = cursor.strip_prefix("an additional ") {
        cursor = after;
    }

    let (amount, digit_end) = parse_leading_digits(cursor)?;
    if amount == 0 {
        return None;
    }

    const UNITS_OF_KW: &str = "units of ";
    let rest = &cursor[digit_end..];
    let units_pos = rest.find(UNITS_OF_KW)?;
    let name_start = &rest[units_pos + UNITS_OF_KW.len()..];
    let name_end = name_start.find('.').unwrap_or(name_start.len());
    let ore_name = name_start[..name_end].trim_matches(BLANKS);
    if ore_name.is_empty() {
        return None;
    }

    let mut result = ParsedMiningEvent { amount, name_buf: [0; 64], name_len: 0 };
    if ore_name.len() > result.name_buf.len() {
        return None;
    }
    result.name_buf[..ore_name.len()].copy_from_slice(ore_name.as_bytes());
    result.name_len = ore_name.len() as u8;
    Some(result)
}

/// Strip HTML/XML tags from src into out_buf.  Returns the written slice.
/// Output that fills out_buf mid-character is cut back to the last whole UTF-8 character.
pub fn strip_html<'a>(src: &str, out_buf: &'a mut [u8]) -> &'a str {
    let mut out: usize = 0;
    let mut in_tag = false;
    for c in src.bytes() {
        if out >= out_buf.len() {
            break;
        }
        match c {
            b'<' => in_tag = true,
            b'>' => in_tag = false,
            _ if !in_tag => {
                out_buf[out] = c;
                out += 1;
            }
            _ => {}
        }
    }
    // Tags are ASCII, so dropping them never splits a character; only the buffer limit can.
    match std::str::from_utf8(&out_buf[..out]) {
        Ok(s) => s,
        Err(e) => {
            let valid = e.valid_up_to();
            std::str::from_utf8(&out_buf[..valid]).unwrap_or("")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    // --- idle_decay_factor / rate_changed ---

    #[test]
    fn idle_decay_holds_then_decays_linearly() {
        let w = 10_000;
        assert_eq!(idle_decay_factor(1_000, 1_000, w), 1.0);
        assert_eq!(idle_decay_factor(6_000, 1_000, w), 1.0); // exactly at grace
        assert!(approx(idle_decay_factor(8_500, 1_000, w), 0.5));
        assert_eq!(idle_decay_factor(11_000, 1_000, w), 0.0);
        assert_eq!(idle_decay_factor(100_000, 1_000, w), 0.0); // clamped
    }

    #[test]
    fn idle_decay_odd_window_truncates_grace() {
        // grace = 1500 (trunc of 3001/2), decay span = 1501
        assert_eq!(idle_decay_factor(1_500, 0, 3_001), 1.0);
        assert!(approx(idle_decay_factor(3_001, 0, 3_001), 0.0));
    }

    #[test]
    fn rate_changed_rules() {
        assert!(!rate_changed(None, None));
        assert!(rate_changed(None, Some(0.0)));
        assert!(rate_changed(Some(1.0), None));
        assert!(!rate_changed(Some(1.0), Some(1.05)));
        assert!(rate_changed(Some(1.0), Some(1.2)));
        assert!(rate_changed(Some(1.0), Some(0.8)));
    }

    // --- CombatWindow ---

    #[test]
    fn combat_window_init() {
        let w = CombatWindow::new(30);
        assert_eq!(w.window_ms, 30_000);
        assert_eq!(w.last_hit_ms, 0);
        assert_eq!(w.last_incoming_dps, None);
        assert_eq!(w.last_outgoing_dps, None);
    }

    #[test]
    fn combat_no_hits_is_zero() {
        let w = CombatWindow::new(30);
        assert_eq!(w.compute_dps(5_000), Dps { incoming: Some(0.0), outgoing: Some(0.0) });
    }

    #[test]
    fn combat_short_span_is_none() {
        let mut w = CombatWindow::new(30);
        w.add_entry(100, true, 10_000, true);
        w.add_entry(100, false, 12_000, true);
        assert_eq!(w.compute_dps(12_000), Dps { incoming: None, outgoing: None });
    }

    #[test]
    fn combat_dps_over_span() {
        let mut w = CombatWindow::new(30);
        // 4 hits over 6 s: 2 incoming x 100, 2 outgoing x 300
        w.add_entry(100, true, 10_000, true);
        w.add_entry(300, false, 12_000, true);
        w.add_entry(100, true, 14_000, true);
        w.add_entry(300, false, 16_000, true);
        let dps = w.compute_dps(16_000);
        assert!(approx(dps.incoming.unwrap(), 200.0 / 6.0));
        assert!(approx(dps.outgoing.unwrap(), 600.0 / 6.0));
    }

    #[test]
    fn combat_span_capped_at_window() {
        let mut w = CombatWindow::new(10);
        w.add_entry(1_000, false, 5_000, true);
        w.add_entry(1_000, false, 15_000, true);
        // cutoff = 5000 so both count; span 10000 == window
        let dps = w.compute_dps(15_000);
        assert!(approx(dps.outgoing.unwrap(), 200.0));
        assert!(approx(dps.incoming.unwrap(), 0.0));
    }

    #[test]
    fn combat_expired_entries_excluded() {
        let mut w = CombatWindow::new(10);
        w.add_entry(9_999, false, 1_000, true); // expired at now=20_000 (cutoff 10_000)
        w.add_entry(100, false, 12_000, true);
        w.add_entry(100, false, 16_000, true);
        let dps = w.compute_dps(20_000);
        // idle 4 s <= grace 5 s, so no decay; 200 over 4 s
        assert!(approx(dps.outgoing.unwrap(), 50.0));
    }

    #[test]
    fn combat_stale_window_short_circuits_to_zero() {
        let mut w = CombatWindow::new(10);
        w.add_entry(100, true, 1_000, true);
        w.add_entry(100, true, 5_000, true);
        assert_eq!(w.compute_dps(15_000), Dps::ZERO);
        // One ms earlier only the newest hit is still inside the window: too short a span to trust
        assert_eq!(w.compute_dps(14_999), Dps { incoming: None, outgoing: None });
    }

    #[test]
    fn combat_per_direction_decay() {
        let mut w = CombatWindow::new(20);
        w.add_entry(100, true, 1, true);
        w.add_entry(100, true, 4_001, true);
        w.add_entry(100, false, 16_001, true);
        // now = 20_001: incoming idle 16 s (decay (16-10)/10 = 0.6 -> factor 0.4), outgoing idle 4 s (factor 1)
        let dps = w.compute_dps(20_001);
        // span = 16000 ms
        assert!(approx(dps.incoming.unwrap(), (200.0 / 16.0) * 0.4));
        assert!(approx(dps.outgoing.unwrap(), 100.0 / 16.0));
    }

    #[test]
    fn combat_ring_overwrites_oldest() {
        let mut w = CombatWindow::new(600);
        for i in 0..(RING_CAPACITY as i64 + 10) {
            w.add_entry(1, false, 1_000 + i * 100, true);
        }
        assert_eq!(w.count, RING_CAPACITY);
        assert_eq!(w.head, 10);
        let newest = 1_000 + (RING_CAPACITY as i64 + 9) * 100;
        let oldest = 1_000 + 10 * 100;
        let dps = w.compute_dps(newest);
        let span_secs = (newest - oldest) as f32 / 1000.0;
        assert!(approx(dps.outgoing.unwrap(), RING_CAPACITY as f32 / span_secs));
    }

    #[test]
    fn combat_refresh_reports_changes() {
        let mut w = CombatWindow::new(30);
        assert!(w.refresh(1_000)); // None -> Some(0)
        assert!(!w.refresh(2_000)); // 0 -> 0
        w.add_entry(100, true, 10_000, true);
        assert!(w.refresh(10_000)); // Some(0) -> None (short span)
        assert!(!w.refresh(10_500)); // None -> None
        w.add_entry(100, true, 14_000, true);
        assert!(w.refresh(14_000)); // None -> Some
        assert!(approx(w.last_incoming_dps.unwrap(), 50.0));
        assert!(!w.refresh(14_001));
    }

    #[test]
    fn damage_alert_fires_once_per_new_hit() {
        let mut w = CombatWindow::new(30);
        assert!(!w.check_damage_alert(1_000));
        w.add_entry(50, true, 2_000, true);
        assert!(w.check_damage_alert(2_100));
        assert!(!w.check_damage_alert(2_200));
        w.add_entry(50, true, 3_000, true);
        assert!(w.check_damage_alert(3_100));
    }

    #[test]
    fn damage_alert_ignores_outgoing_and_filtered() {
        let mut w = CombatWindow::new(30);
        w.add_entry(50, false, 2_000, true);
        assert!(!w.check_damage_alert(2_100));
        w.add_entry(50, true, 3_000, false);
        assert!(!w.check_damage_alert(3_100));
        // Filtered hit still counts toward DPS activity
        assert_eq!(w.last_incoming_activity_ms, 3_000);
        assert_eq!(w.last_incoming_hit_ms, 0);
    }

    #[test]
    fn add_entry_clocks_never_go_backwards() {
        let mut w = CombatWindow::new(30);
        w.add_entry(1, true, 5_000, true);
        w.add_entry(1, true, 4_000, true);
        assert_eq!(w.last_hit_ms, 5_000);
        assert_eq!(w.last_incoming_hit_ms, 5_000);
        assert_eq!(w.last_incoming_activity_ms, 5_000);
        assert_eq!(w.last_outgoing_activity_ms, 0);
    }

    // --- CombatTracker ---

    #[test]
    fn combat_tracker_lifecycle() {
        let t = CombatTracker::new(30);
        assert_eq!(t.get_dps("Nobody"), Dps::ZERO);
        assert!(!t.check_damage_alert("Nobody", 0));
        t.add_entry("Alice", 100, true, 10_000, true).unwrap();
        t.add_entry("Alice", 100, true, 14_000, true).unwrap();
        // Before refresh the cached values are still None
        assert_eq!(t.get_dps("Alice"), Dps { incoming: None, outgoing: None });
        assert!(t.refresh_all(14_000));
        let dps = t.get_dps("Alice");
        assert!(approx(dps.incoming.unwrap(), 50.0));
        assert!(approx(dps.outgoing.unwrap(), 0.0));
        assert!(t.check_damage_alert("Alice", 14_100));
        assert!(!t.check_damage_alert("Alice", 14_200));
        t.remove_character("Alice");
        assert_eq!(t.get_dps("Alice"), Dps::ZERO);
        assert!(!t.refresh_all(20_000));
    }

    #[test]
    fn combat_tracker_names_are_exact_keys() {
        let t = CombatTracker::new(30);
        t.add_entry("Alice", 1, true, 1, true).unwrap();
        t.refresh_all(1);
        assert_eq!(t.get_dps("alice"), Dps::ZERO);
        assert_eq!(t.get_dps("Alice"), Dps { incoming: None, outgoing: None });
    }

    #[test]
    fn combat_tracker_concurrent_access() {
        let t = Arc::new(CombatTracker::new(60));
        let writer = {
            let t = Arc::clone(&t);
            std::thread::spawn(move || {
                for i in 0..2_000i64 {
                    t.add_entry(if i % 2 == 0 { "A" } else { "B" }, 10, i % 3 == 0, 1_000 + i * 10, true).unwrap();
                }
            })
        };
        for i in 0..200 {
            t.refresh_all(1_000 + i * 100);
            let _ = t.get_dps("A");
        }
        writer.join().unwrap();
        t.refresh_all(1_000 + 2_000 * 10);
        assert!(t.get_dps("A").outgoing.unwrap() > 0.0);
        assert!(t.get_dps("B").incoming.unwrap() > 0.0);
    }

    #[test]
    fn poisoned_tracker_degrades_gracefully() {
        let t = Arc::new(CombatTracker::new(30));
        t.add_entry("A", 1, true, 1, true).unwrap();
        let t2 = Arc::clone(&t);
        let _ = std::thread::spawn(move || {
            let _guard = t2.base.windows.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(t.add_entry("A", 1, true, 2, true), Err(LockPoisoned));
        assert_eq!(t.get_dps("A"), Dps::ZERO);
        assert!(!t.check_damage_alert("A", 3));
        assert!(!t.refresh_all(3));
        t.remove_character("A");
    }

    // --- parse_combat_line ---

    #[test]
    fn combat_outgoing_hit() {
        let line = "[ 2024.01.01 12:00:00 ] (combat) 523 to Guristas Pithi - Heavy Missile Launcher II - Hits";
        let p = parse_combat_line(line).unwrap();
        assert_eq!(p, ParsedCombatLine { amount: 523, is_incoming: false, weapon: "Heavy Missile Launcher II" });
    }

    #[test]
    fn combat_incoming_hit() {
        let line = "[ 2024.01.01 12:00:00 ] (combat) 87 from Guristas Pithi - Scourge Light Missile - Smashes";
        let p = parse_combat_line(line).unwrap();
        assert_eq!(p, ParsedCombatLine { amount: 87, is_incoming: true, weapon: "Scourge Light Missile" });
    }

    #[test]
    fn combat_weapon_uses_last_two_dashes() {
        let line = "(combat) 1200 to Astrahus - My - Station - 425mm Railgun II - Penetrates";
        let p = parse_combat_line(line).unwrap();
        assert_eq!(p.weapon, "425mm Railgun II");
        assert!(!p.is_incoming);
    }

    #[test]
    fn combat_no_weapon_segment() {
        let p = parse_combat_line("(combat) 50 from Drone - Hits").unwrap();
        assert_eq!(p.weapon, "");
        let p = parse_combat_line("(combat) 50 from Drone").unwrap();
        assert_eq!(p.weapon, "");
        assert!(p.is_incoming);
    }

    #[test]
    fn combat_from_wins_over_to() {
        let p = parse_combat_line("(combat) 10 to x from y - W - Hits").unwrap();
        assert!(p.is_incoming);
    }

    #[test]
    fn combat_misses() {
        let p = parse_combat_line("(combat) Guristas Pithi misses you completely - Scourge").unwrap();
        assert_eq!(p, ParsedCombatLine { amount: 0, is_incoming: true, weapon: "" });
        assert_eq!(parse_combat_line("(combat) You misses you"), None);
        assert_eq!(parse_combat_line("(combat) Your Hammerhead II misses Guristas Pithi completely"), None);
    }

    #[test]
    fn combat_skips_reps_and_transfers() {
        assert_eq!(parse_combat_line("(combat) 300 remote armor repaired to you - Foo repairs your armor"), None);
        assert_eq!(parse_combat_line("(combat) 300 from Foo boosts your shield"), None);
        assert_eq!(parse_combat_line("(combat) 300 from Foo shields your ship"), None);
        assert_eq!(parse_combat_line("(combat) 300 energy transfers to Foo"), None);
    }

    #[test]
    fn combat_rejects_malformed() {
        assert_eq!(parse_combat_line("no tag here 100 to x"), None);
        assert_eq!(parse_combat_line("(combat)"), None);
        assert_eq!(parse_combat_line("(combat) Warp scramble attempt"), None);
        assert_eq!(parse_combat_line("(combat) 0 to Foo - W - Hits"), None);
        assert_eq!(parse_combat_line("(combat) 100 hits Foo"), None);
        assert_eq!(parse_combat_line("(combat)\t 77\tfrom Foo").map(|p| p.amount), None);
        assert_eq!(parse_combat_line("(combat)\t 77 from Foo").map(|p| p.amount), Some(77));
    }

    #[test]
    fn combat_amount_wraps_like_release_zig() {
        let p = parse_combat_line("(combat) 4294967297 to Foo").unwrap();
        assert_eq!(p.amount, 1);
    }

    // --- is_weapon_excluded ---

    #[test]
    fn weapon_exclusion() {
        assert!(is_weapon_excluded("Heavy Missile Launcher II", "missile"));
        assert!(is_weapon_excluded("Heavy Missile Launcher II", "drone, MISSILE"));
        assert!(is_weapon_excluded("Heavy Missile Launcher II", " heavy missile launcher ii "));
        assert!(!is_weapon_excluded("Heavy Missile Launcher II", "drone,smartbomb"));
        assert!(!is_weapon_excluded("Heavy Missile Launcher II", ",,  ,"));
        assert!(!is_weapon_excluded("", "missile"));
        assert!(!is_weapon_excluded("Missile", ""));
        assert!(!is_weapon_excluded("Gun", "Gunnery"));
        assert!(is_weapon_excluded("Gun", "Gunnery,gun,"));
    }

    // --- strip_html ---

    #[test]
    fn strip_html_removes_tags() {
        let mut buf = [0u8; 64];
        assert_eq!(strip_html("<color=0xff>523</color> <b>to</b> x", &mut buf), "523 to x");
        assert_eq!(strip_html("a > b", &mut buf), "a  b");
        assert_eq!(strip_html("unterminated <tag", &mut buf), "unterminated ");
    }

    #[test]
    fn strip_html_truncates_to_buffer() {
        let mut buf = [0u8; 4];
        assert_eq!(strip_html("<b>abcdef</b>", &mut buf), "abcd");
        let mut buf = [0u8; 4];
        // 'é' is two bytes; the half that fits is dropped
        assert_eq!(strip_html("abcé", &mut buf), "abc");
    }

    // --- parse_bounty_line ---

    #[test]
    fn bounty_parses_thousands() {
        let line = "[ 2024.01.01 12:00:00 ] (bounty) <font size=12><b><color=0xff00aa00>246,153 ISK</b> added to next bounty payout";
        assert_eq!(parse_bounty_line(line), Some(246_153.0));
        assert_eq!(parse_bounty_line("(bounty) 1,234,567 ISK added"), Some(1_234_567.0));
        assert_eq!(parse_bounty_line("(bounty)   15 ISK"), Some(15.0));
    }

    #[test]
    fn bounty_rejects_malformed() {
        assert_eq!(parse_bounty_line("(combat) 100 ISK"), None);
        assert_eq!(parse_bounty_line("(bounty) ISK 100"), None);
        assert_eq!(parse_bounty_line("(bounty) ,100 ISK"), None);
        assert_eq!(parse_bounty_line("(bounty) 0 ISK"), None);
        assert_eq!(parse_bounty_line("(bounty)"), None);
    }

    // --- parse_mining_line ---

    #[test]
    fn mining_normal_yield() {
        let line = "[ 2024.01.01 12:00:00 ] (mining) You mined <color=#ff8dc169>412</color> units of <color=#ffffffff><font size=12>Veldspar</font></color>";
        let p = parse_mining_line(line).unwrap();
        assert_eq!(p.amount, 412);
        assert_eq!(p.name(), "Veldspar");

        let p = parse_mining_line("(mining) You mined 412 units of Concentrated Veldspar").unwrap();
        assert_eq!(p.amount, 412);
        assert_eq!(p.name(), "Concentrated Veldspar");
    }

    #[test]
    fn mining_critical_yield() {
        let p = parse_mining_line("(mining) <b>Critical!</b> You mined an additional 87 units of Blue Ice.").unwrap();
        assert_eq!(p.amount, 87);
        assert_eq!(p.name(), "Blue Ice");
    }

    #[test]
    fn mining_rejects_residue_and_malformed() {
        assert_eq!(parse_mining_line("(mining) 30 units of Veldspar was depleted from asteroid as residue"), None);
        assert_eq!(parse_mining_line("(mining) Your cargo is full"), None);
        assert_eq!(parse_mining_line("(mining) You mined lots of Veldspar"), None);
        assert_eq!(parse_mining_line("(mining) You mined 0 units of Veldspar"), None);
        assert_eq!(parse_mining_line("(mining) You mined 10 chunks of Veldspar"), None);
        assert_eq!(parse_mining_line("(mining) You mined 10 units of ."), None);
        assert_eq!(parse_mining_line("You mined 10 units of Veldspar"), None);
    }

    #[test]
    fn mining_name_length_limit() {
        let name64 = "x".repeat(64);
        let p = parse_mining_line(&format!("(mining) You mined 5 units of {name64}")).unwrap();
        assert_eq!(p.name(), name64);
        let name65 = "x".repeat(65);
        assert_eq!(parse_mining_line(&format!("(mining) You mined 5 units of {name65}")), None);
    }

    // --- MiningWindow / MiningTracker ---

    #[test]
    fn mining_rates() {
        let mut w = MiningWindow::new(60);
        assert_eq!(w.compute_rate(1_000), Some(0.0));
        w.add_entry(100.0, 1_000.0, 10_000);
        assert_eq!(w.compute_rate(10_000), None);
        w.add_entry(100.0, 3_000.0, 20_000);
        assert!(approx(w.compute_rate(20_000).unwrap(), 20.0));
        assert!(approx(w.compute_isk_rate(20_000).unwrap(), 400.0));
        // idle 45 s with 60 s window: grace 30, decay (45-30)/30 = 0.5
        assert!(approx(w.compute_rate(65_000).unwrap(), 10.0));
        assert_eq!(w.compute_rate(80_000), Some(0.0));
    }

    #[test]
    fn mining_count_events() {
        let mut w = MiningWindow::new(600);
        assert_eq!(w.count_events(60_000, 1_000), 0);
        for t in [10_000, 20_000, 30_000, 40_000] {
            w.add_entry(1.0, 0.0, t);
        }
        assert_eq!(w.count_events(60_000, 40_000), 4);
        assert_eq!(w.count_events(15_000, 40_000), 2);
        assert_eq!(w.count_events(10_000, 50_000), 0); // newest outside the window
        assert_eq!(w.count_events(10_001, 50_000), 1);
    }

    #[test]
    fn mining_refresh_tracks_isk_too() {
        let mut w = MiningWindow::new(60);
        w.add_entry(10.0, 100.0, 1_000);
        w.add_entry(10.0, 100.0, 5_000);
        assert!(w.refresh(5_000));
        assert!(approx(w.last_m3_per_sec.unwrap(), 5.0));
        assert!(approx(w.last_isk_per_sec.unwrap(), 50.0));
        assert!(!w.refresh(5_001));
    }

    #[test]
    fn mining_tracker_rates_and_defaults() {
        let t = MiningTracker::new(60);
        assert_eq!(t.get_rate("X"), Some(0.0));
        assert_eq!(t.get_isk_rate("X"), Some(0.0));
        t.add_entry("X", 10.0, 100.0, 1_000).unwrap();
        t.add_entry("X", 10.0, 100.0, 5_000).unwrap();
        assert_eq!(t.get_rate("X"), None);
        assert!(t.refresh_all(5_000));
        assert!(approx(t.get_rate("X").unwrap(), 5.0));
        assert!(approx(t.get_isk_rate("X").unwrap(), 50.0));
        t.remove_character("X");
        assert_eq!(t.get_rate("X"), Some(0.0));
    }

    #[test]
    fn mining_idle_alert() {
        let t = MiningTracker::new(600);
        assert!(!t.check_idle_alert("X", 1_000, 60_000, 0));
        t.add_entry("X", 1.0, 0.0, 10_000).unwrap();
        t.add_entry("X", 1.0, 0.0, 20_000).unwrap();
        // 2 events > threshold 1: active
        assert!(!t.check_idle_alert("X", 25_000, 60_000, 1));
        // Only the 20_000 event remains in the last 10 s window: idle
        assert!(t.check_idle_alert("X", 29_000, 10_000, 1));
        assert!(!t.check_idle_alert("X", 30_000, 10_000, 1)); // fires once per episode
        // Activity re-arms
        assert!(!t.check_idle_alert("X", 30_000, 60_000, 1));
        assert!(t.check_idle_alert("X", 31_000, 10_000, 1));
    }

    #[test]
    fn mining_stopped_alert() {
        let t = MiningTracker::new(600);
        assert!(!t.check_stopped_alert("X", 100_000, 30_000));
        t.add_entry("X", 1.0, 0.0, 10_000).unwrap();
        assert!(!t.check_stopped_alert("X", 39_999, 30_000));
        assert!(t.check_stopped_alert("X", 40_000, 30_000));
        assert!(!t.check_stopped_alert("X", 50_000, 30_000));
        t.add_entry("X", 1.0, 0.0, 60_000).unwrap();
        assert!(!t.check_stopped_alert("X", 70_000, 30_000));
        assert!(t.check_stopped_alert("X", 90_000, 30_000));
    }

    // --- BountyWindow / BountyTracker ---

    #[test]
    fn bounty_window_rate() {
        let mut w = BountyWindow::new(300);
        assert_eq!(w.compute_isk_rate(5_000), Some(0.0));
        w.add_entry(100_000.0, 1);
        w.add_entry(200_000.0, 20_001);
        assert!(approx(w.compute_isk_rate(20_001).unwrap(), 15_000.0));
        assert!(w.refresh(20_001));
        assert!(!w.refresh(20_002));
    }

    #[test]
    fn bounty_tracker_lifecycle() {
        let t = BountyTracker::new(300);
        assert_eq!(t.get_isk_rate("Y"), Some(0.0));
        t.add_entry("Y", 1_000.0, 1_000).unwrap();
        // None -> None (span too short) is not a change
        assert!(!t.refresh_all(1_000));
        assert_eq!(t.get_isk_rate("Y"), None);
        t.add_entry("Y", 1_000.0, 5_000).unwrap();
        assert!(t.refresh_all(5_000));
        assert!(approx(t.get_isk_rate("Y").unwrap(), 500.0));
        t.remove_character("Y");
        assert_eq!(t.get_isk_rate("Y"), Some(0.0));
    }
}
