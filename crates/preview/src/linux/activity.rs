//! Activity overlays on Linux: the combat/mining/bounty trackers (fed by the chat-log worker from game-log lines) and
//! per-client CPU/RAM, pushed into each thumbnail's metrics on their update intervals, plus the trackers' alerts
//! (Taking damage, Laser idle, Mining stopped). The Zig build's main.zig updateThrottledTracker/push*Update.

use std::collections::HashMap;
use std::sync::Arc;

use eve_maj_core::activity_tracker::{BountyTracker, CombatTracker, MiningTracker};
use eve_maj_core::chatlog::ActivityTrackers;
use eve_maj_core::config::{Config, GlobalSettings};
use eve_maj_core::render::now_ms;
use eve_maj_core::types::NotificationType;

use super::App;

/// The settings that decide which trackers exist; a change recreates them (and restarts the chat-log worker that feeds them).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TrackerKey {
    combat: Option<(u32, String)>,
    mining: Option<u32>,
    bounty: Option<u32>,
}

impl TrackerKey {
    pub fn from(config: &Config) -> Self {
        TrackerKey {
            combat: config.combat.enabled.then(|| (config.combat.window_seconds, config.combat.damage_alert_excluded_weapons.clone())),
            mining: config.mining.enabled.then_some(config.mining.window_seconds),
            bounty: config.bounty.enabled.then_some(config.bounty.window_seconds),
        }
    }
}

#[derive(Default)]
pub(super) struct Trackers {
    combat: Option<Arc<CombatTracker>>,
    mining: Option<Arc<MiningTracker>>,
    bounty: Option<Arc<BountyTracker>>,
    last_combat_ms: u64,
    last_mining_ms: u64,
    last_bounty_ms: u64,
    last_resources_ms: u64,
    /// pid -> (CPU seconds, wall ms) at the last sample.
    cpu_samples: HashMap<i32, (f64, u64)>,
}

impl Trackers {
    /// Fresh trackers for `config`, and the handles the chat-log worker feeds.
    pub fn new(config: &Config, global: &GlobalSettings) -> (Trackers, ActivityTrackers) {
        let combat = config.combat.enabled.then(|| Arc::new(CombatTracker::new(config.combat.window_seconds)));
        let mining = config.mining.enabled.then(|| Arc::new(MiningTracker::new(config.mining.window_seconds)));
        let bounty = config.bounty.enabled.then(|| Arc::new(BountyTracker::new(config.bounty.window_seconds)));
        // Prices can be overridden in global settings; a private copy answers the worker's lookups.
        let mut ore_settings = GlobalSettings::default();
        ore_settings.ore_table = global.ore_table.clone();
        let feeds = ActivityTrackers {
            combat: combat.clone(),
            mining: mining.clone(),
            bounty: bounty.clone(),
            damage_alert_excluded_weapons: config.combat.damage_alert_excluded_weapons.clone(),
            ore: mining.is_some().then(|| {
                Box::new(move |name: &str| ore_settings.ore_volume(name).map(|v| (v, ore_settings.ore_price(name).unwrap_or(0.0)))) as Box<dyn Fn(&str) -> Option<(f64, f64)> + Send>
            }),
        };
        (Trackers { combat, mining, bounty, ..Default::default() }, feeds)
    }
}

fn due(last: &mut u64, now: u64, interval_ms: u32) -> bool {
    if now.saturating_sub(*last) < interval_ms as u64 {
        return false;
    }
    *last = now;
    true
}

/// Total CPU seconds (user + system) and resident memory in MB for a process.
fn process_usage(pid: i32) -> Option<(f64, f32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command name; utime and stime are the 12th and 13th of those.
    let rest = &stat[stat.rfind(')')? + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks: f64 = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss_kb: f32 = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.split_whitespace().next()?.parse().ok()?;
    Some((ticks / hz, rss_kb / 1024.0))
}

impl App {
    /// Pushes tracker readings and resource stats into thumbnails (each on its own interval) and raises tracker alerts.
    pub(super) fn update_activity(&mut self) {
        let now = now_ms();
        let now_i = now as i64;

        if let Some(tracker) = self.trackers.combat.clone() {
            if due(&mut self.trackers.last_combat_ms, now, self.config.combat.update_interval_ms) {
                tracker.refresh_all(now_i);
                for i in 0..self.thumbs.len() {
                    let dps = tracker.get_dps(&self.thumbs[i].name);
                    let m = &mut self.thumbs[i].metrics;
                    if !m.has_dps_data || m.incoming_dps != dps.incoming || m.outgoing_dps != dps.outgoing {
                        (m.has_dps_data, m.incoming_dps, m.outgoing_dps) = (true, dps.incoming, dps.outgoing);
                        self.thumbs[i].needs_render = true;
                    }
                    if tracker.check_damage_alert(&self.thumbs[i].name, now_i) {
                        self.show_notification(i, "Taking damage", NotificationType::TakingDamage);
                    }
                }
            }
        }

        if let Some(tracker) = self.trackers.mining.clone() {
            if due(&mut self.trackers.last_mining_ms, now, self.config.mining.update_interval_ms) {
                tracker.refresh_all(now_i);
                let idle_window = self.config.mining.idle_alert_window_seconds as i64 * 1000;
                let idle_threshold = self.config.mining.idle_alert_threshold;
                let stopped_window = self.config.mining.stopped_alert_window_seconds as i64 * 1000;
                for i in 0..self.thumbs.len() {
                    let name = self.thumbs[i].name.clone();
                    let (rate, isk) = (tracker.get_rate(&name), tracker.get_isk_rate(&name));
                    let m = &mut self.thumbs[i].metrics;
                    if !m.has_mining_data || m.mining_rate != rate || m.mining_isk_rate != isk {
                        (m.has_mining_data, m.mining_rate, m.mining_isk_rate) = (true, rate, isk);
                        self.thumbs[i].needs_render = true;
                    }
                    if tracker.check_idle_alert(&name, now_i, idle_window, idle_threshold) {
                        self.show_notification(i, "Laser idle", NotificationType::MiningIdle);
                    }
                    if tracker.check_stopped_alert(&name, now_i, stopped_window) {
                        self.show_notification(i, "Mining stopped", NotificationType::MiningStopped);
                    }
                }
            }
        }

        if let Some(tracker) = self.trackers.bounty.clone() {
            if due(&mut self.trackers.last_bounty_ms, now, self.config.bounty.update_interval_ms) {
                tracker.refresh_all(now_i);
                for t in &mut self.thumbs {
                    let isk = tracker.get_isk_rate(&t.name);
                    if !t.metrics.has_bounty_data || t.metrics.bounty_isk_rate != isk {
                        (t.metrics.has_bounty_data, t.metrics.bounty_isk_rate) = (true, isk);
                        t.needs_render = true;
                    }
                }
            }
        }

        if self.config.resources.enabled && due(&mut self.trackers.last_resources_ms, now, self.config.resources.update_interval_ms) {
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            for t in &mut self.thumbs {
                let Some(pid) = self.toplevels.iter().find(|tl| tl.id == t.window).map(|tl| tl.pid) else { continue };
                let Some((cpu_s, ram_mb)) = process_usage(pid) else { continue };
                // Share of the whole machine, like the Windows build (process CPU time / wall time / logical CPUs).
                let cpu = match self.trackers.cpu_samples.insert(pid, (cpu_s, now)) {
                    Some((prev_s, prev_ms)) if now > prev_ms && cpu_s >= prev_s => ((cpu_s - prev_s) * 1000.0 / (now - prev_ms) as f64 / cpus * 100.0).clamp(0.0, 100.0) as f32,
                    _ => t.metrics.cpu_percent,
                };
                let m = &mut t.metrics;
                // VRAM has no portable per-process source on Linux; the overlay leaves it out.
                if !m.has_resource_data || m.cpu_percent != cpu || m.ram_mb != ram_mb {
                    (m.has_resource_data, m.cpu_percent, m.ram_mb, m.has_vram_data) = (true, cpu, ram_mb, false);
                    t.needs_render = true;
                }
            }
            let live: Vec<i32> = self.toplevels.iter().map(|t| t.pid).collect();
            self.trackers.cpu_samples.retain(|pid, _| live.contains(pid));
        }
    }
}
