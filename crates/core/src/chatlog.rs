//! Chat-log and game-log monitoring (the Zig build's chatlog.zig): finds each logged-in character's newest Local
//! chat log (UTF-16 LE) and game log (UTF-8), tails them, and turns what it reads into system-name updates,
//! notifications and activity-tracker entries.
//!
//! Platform-neutral: [`ChatlogMonitor`] is plain file I/O driven by `poll()`, and [`ChatlogWorker`] runs it on its own
//! thread with commands in and events out, for the Windows and Linux front ends alike. Differences from the Zig build:
//! new log files are noticed through the log directories' modification time (no Win32 change notifications), and
//! character IDs learned from log headers come back as [`ChatlogEvent::CharacterId`] so only the caller's thread writes
//! global settings.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::activity_tracker::{self, BountyTracker, CombatTracker, MiningTracker};
use crate::layout::is_generic_character_name;
use crate::log::Scope;
use crate::types::NotificationType;

const SLOG: Scope = Scope::new("chatlog");

/// Caps memory use on malformed logs; 4KB covers combat logs with color tags (up to 2-3KB).
const MAX_LINE_LENGTH: usize = 4000;
/// Shorter lines can't hold a timestamp plus a message.
const MIN_LINE_LENGTH: usize = 25;
const SCAN_CHUNK_SIZE: u64 = 8192;
/// How far back from EOF the initial-system scan looks.
const MAX_BACKWARD_SCAN_BYTES: u64 = 8 * 1024 * 1024;
/// Bytes read from one file per poll; the rest waits for the next poll.
const MAX_READ_PER_POLL: u64 = 256 * 1024;
const MAX_SYSTEM_NAME_LEN: usize = 64;
const SYSTEM_CHANGE_PREFIX: &str = "EVE System > Channel changed to Local";

/// What the monitor found, for the front end to apply on its own thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatlogEvent {
    /// `event_ts` is the line's in-game time as YYYYMMDDhhmmss (0 for live lines, which are already in order), so an
    /// older initial-scan result never overwrites a newer one. `is_jump` marks stargate/conduit travel.
    System { character: String, system: String, event_ts: u64, is_jump: bool },
    Notification { character: String, text: String, ntype: NotificationType },
    /// A character's ID, learned from a log header; worth caching (GlobalSettings::update_character_id).
    CharacterId { character: String, id: String },
}

/// Optional activity trackers fed from game logs (DPS, mining and bounty overlays).
#[derive(Default)]
pub struct ActivityTrackers {
    pub combat: Option<Arc<CombatTracker>>,
    pub mining: Option<Arc<MiningTracker>>,
    pub bounty: Option<Arc<BountyTracker>>,
    /// Comma-separated weapon names whose incoming hits don't count for the Taking Damage alert.
    pub damage_alert_excluded_weapons: String,
    /// (m3 per unit, ISK per unit) for an ore/ice/gas name; mining yields need it.
    pub ore: Option<Box<dyn Fn(&str) -> Option<(f64, f64)> + Send>>,
}

/// True only when "EVE System" is the line's speaker (right after "[ timestamp ]"). Anyone in Local can type "EVE System >
/// Channel changed to Local : Jita", but their line reads "[ ts ] Their Name > EVE System > ...", so a substring match
/// alone would let them fake the victim's system.
pub fn is_system_change_line(line: &str) -> bool {
    let Some(close) = line.find(']') else { return false };
    line[close + 1..].trim_start_matches([' ', '\t']).starts_with(SYSTEM_CHANGE_PREFIX)
}

/// Log file name timestamp as YYYYMMDDhhmmss: `Local_YYYYMMDD_HHMMSS_<id>.txt` for chat logs, `YYYYMMDD_HHMMSS[_<id>].txt`
/// for game logs; 0 if it doesn't parse.
pub fn parse_log_timestamp(filename: &str, is_chatlog: bool) -> u64 {
    let name = filename.strip_suffix(".txt").unwrap_or(filename);
    let ts = if is_chatlog {
        match name.strip_prefix("Local_") {
            Some(rest) => rest,
            None => return 0,
        }
    } else {
        name
    };
    let b = ts.as_bytes();
    if b.len() < 15 || b[8] != b'_' {
        return 0;
    }
    let digits = |s: &str| -> Option<u64> { s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok()).flatten() };
    match (digits(&ts[..8]), digits(&ts[9..15])) {
        (Some(date), Some(time)) => date * 1_000_000 + time,
        _ => 0,
    }
}

/// The character ID at the end of a log file name (8-13 digits after the last underscore).
pub fn extract_character_id(filename: &str) -> Option<&str> {
    let name = filename.strip_suffix(".txt").unwrap_or(filename);
    let id = &name[name.rfind('_')? + 1..];
    ((8..=13).contains(&id.len()) && id.bytes().all(|c| c.is_ascii_digit())).then_some(id)
}

/// "Listener: Name" from a log header.
pub fn extract_listener_name(text: &str) -> Option<&str> {
    let after = &text[text.find("Listener:")? + "Listener:".len()..];
    let end = after.find(['\r', '\n']).unwrap_or(after.len());
    let name = after[..end].trim_matches([' ', '\t']);
    (!name.is_empty()).then_some(name)
}

/// The "[ YYYY.MM.DD HH:MM:SS ]" timestamp just before `needle_pos`, as YYYYMMDDhhmmss; 0 if there isn't one. Bounded to
/// 64 bytes back so a chunk-boundary truncation can't pick up an earlier line's bracket.
fn parse_line_timestamp(text: &str, needle_pos: usize) -> u64 {
    let mut window_start = needle_pos.saturating_sub(64);
    while !text.is_char_boundary(window_start) {
        window_start += 1;
    }
    let Some(open) = text[window_start..needle_pos].rfind('[').map(|p| window_start + p) else { return 0 };
    let Some(close) = text[open..].find(']').map(|p| open + p) else { return 0 };
    let inner = text[open + 1..close].trim_matches([' ', '\t']).as_bytes();
    if inner.len() < 19 || inner[4] != b'.' || inner[7] != b'.' || inner[10] != b' ' || inner[13] != b':' || inner[16] != b':' {
        return 0;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u64> { std::str::from_utf8(&inner[r]).ok()?.parse().ok() };
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (num(0..4), num(5..7), num(8..10), num(11..13), num(14..16), num(17..19)) else {
        return 0;
    };
    (y * 10000 + mo * 100 + d) * 1_000_000 + (h * 10000 + mi * 100 + s)
}

/// A system name as the log writes it, minus a trailing period or " solar system".
fn clean_system_name(raw: &str) -> Option<String> {
    let mut s = raw.trim_matches([' ', '\t']);
    s = s.strip_suffix('.').unwrap_or(s).trim_end();
    s = s.strip_suffix(" solar system").unwrap_or(s).trim_end();
    (!s.is_empty() && s.len() <= MAX_SYSTEM_NAME_LEN).then(|| s.to_owned())
}

/// Finds `needle`, then `secondary` after it, and returns the system name between `secondary` and the end of the line.
fn extract_system_after_markers(line: &str, needle: &str, secondary: &str) -> Option<String> {
    let after_needle = &line[line.find(needle)? + needle.len()..];
    let rest = &after_needle[after_needle.find(secondary)? + secondary.len()..];
    let end = rest.find(['\r', '\n']).unwrap_or(rest.len());
    clean_system_name(&rest[..end])
}

/// "Channel changed to Local : System" (callers check is_system_change_line first).
fn parse_system_change_from_chat(line: &str) -> Option<String> {
    extract_system_after_markers(line, "Channel changed to Local", ":")
}

/// "Jumping from A to B".
fn parse_jump(line: &str) -> Option<String> {
    extract_system_after_markers(line, "Jumping from", " to ")
}

/// "Undocking from Station in System" (older clients) or "Undocking from Station to System solar system".
fn parse_undock(line: &str) -> Option<String> {
    extract_system_after_markers(line, "Undocking from", " in ").or_else(|| extract_system_after_markers(line, "Undocking from", " to "))
}

/// "A Conduit Field activated by X jumps you to System." The activating character's own line reads "...jumps you to
/// System, bringing along N passengers.", so a comma ends the name too.
fn parse_conduit_jump(line: &str) -> Option<String> {
    let rest = &line[line.find("jumps you to ")? + "jumps you to ".len()..];
    let end = rest.find(['\r', '\n', '.', ',']).unwrap_or(rest.len());
    clean_system_name(&rest[..end])
}

/// The last genuine "EVE System" Local announcement in `text`, with its timestamp.
fn extract_system_from_chatlog(text: &str) -> Option<(String, u64)> {
    let needle = "Channel changed to Local";
    let mut last = None;
    for (pos, _) in text.match_indices(needle) {
        let line_start = text[..pos].rfind('\n').map_or(0, |n| n + 1);
        let line_end = text[pos..].find('\n').map_or(text.len(), |n| pos + n);
        if is_system_change_line(&text[line_start..line_end]) {
            last = Some(pos);
        }
    }
    let pos = last?;
    Some((parse_system_change_from_chat(&text[pos..])?, parse_line_timestamp(text, pos)))
}

/// The system from the latest jump or undock line in `text`, with its timestamp.
fn extract_system_from_gamelog(text: &str) -> Option<(String, u64)> {
    let jump = text.rfind("Jumping from");
    let undock = text.rfind("Undocking from");
    let (pos, system) = match (jump, undock) {
        (Some(j), u) if u.is_none_or(|u| j > u) => (j, parse_jump(&text[j..])?),
        (_, Some(u)) => (u, parse_undock(&text[u..])?),
        _ => return None,
    };
    Some((system, parse_line_timestamp(text, pos)))
}

/// A popup-worthy game log line: its text and type; None for lines that aren't one. `event_text` has HTML stripped.
pub fn classify_notification(event_text: &str) -> Option<(String, NotificationType)> {
    let rest = event_text.find(']').map_or(event_text, |p| &event_text[p + 1..]).trim_matches([' ', '\t', '\r', '\n']);
    let (text, ntype) = if let Some(m) = rest.strip_prefix("(question)") {
        classify_question(m)
    } else if let Some(m) = rest.strip_prefix("(notify)") {
        classify_notify(m)
    } else if let Some(m) = rest.strip_prefix("(None)") {
        classify_none(m)
    } else if let Some(m) = rest.strip_prefix("(combat)") {
        classify_combat(m)
    } else if rest.starts_with("(hint)") {
        return None;
    } else {
        (rest.trim_matches([' ', '\t', '\r', '\n', '.']).to_owned(), NotificationType::Generic)
    };
    (!text.is_empty()).then_some((text, ntype))
}

fn classify_question(message: &str) -> (String, NotificationType) {
    if message.contains("wants you to join their fleet") {
        return ("Fleet invite".into(), NotificationType::FleetInvite);
    }
    (String::new(), NotificationType::Generic)
}

fn classify_notify(message: &str) -> (String, NotificationType) {
    use NotificationType as N;
    let t = message.trim_matches([' ', '\t', '\r', '\n']);
    let has = |s: &str| t.contains(s);
    let r = |text: &str, n: N| (text.to_owned(), n);
    if t.starts_with("Following ") && has(" in warp") {
        return r("Following", N::FleetFollow);
    }
    if has("Regrouping to ") {
        return r("Regrouping", N::FleetRegroup);
    }
    if has("Your fleet is disbanding") {
        return r("Fleet disbanding", N::FleetDisband);
    }
    if has("Starting clone jumping") {
        return r("Jump Cloning", N::JumpCloning);
    }
    if has("Successfully compressed") {
        return r("Compressed", N::MiningCompression);
    }
    if has("a pale shadow of its former glory") {
        return r("Asteroid Depleted", N::AsteroidDepleted);
    }
    if has("cargo hold is full") {
        return r("Cargo full", N::CargoFull);
    }
    if has("cloak deactivates") && has("Mobile Observatory") {
        return r("Observatory Decloak", N::ObservatoryDecloak);
    }
    if has("cloak deactivates") {
        return r("Decloaked", N::Decloak);
    }
    if has("cloaking systems are unable to activate") {
        return r("Can't cloak", N::CloakFailed);
    }
    if has("deactivates due to the destruction") {
        return r("Crystal broke", N::CrystalBroke);
    }
    if has("Bomb Launcher") && has("has run out of charges") {
        return r("Bomb Launcher Empty", N::BombLauncherEmpty);
    }
    // "Your" so other players' self-destructs don't trigger it.
    if has("Your") && has("will self-destruct in") {
        return r("Self-Destruct", N::SelfDestruct);
    }
    if has("You have aborted the self-destruct") {
        return r("Self-Destruct Aborted", N::SelfDestruct);
    }
    if has("You cannot do that while docking") {
        return r("Docking", N::Docking);
    }
    if has("Autopilot disabled - Waypoint reached") {
        return r("Waypoint reached", N::AutopilotReached);
    }
    if has("Autopilot approaching target") {
        return r("Approaching", N::AutopilotApproaching);
    }
    if has("get within") && has("stargate to jump") {
        return r("Can't Jump: Range", N::JumpRange);
    }
    if has("within a warp disruption zone") {
        return r("Warp Disrupted", N::WarpBubble);
    }
    if has("recent acts of aggression") {
        return r("Can't Jump: Aggression", N::AggressionCantJump);
    }
    if has("Conduit Field") && has("jumps you to") {
        return match parse_conduit_jump(t) {
            Some(system) => (format!("Taking Conduit to {system}"), N::ConduitJump),
            None => r("Conduit Jump", N::ConduitJump),
        };
    }
    (String::new(), N::Generic)
}

/// Only scrams/points landing on you are worth a popup; damage lines feed the DPS tracker instead.
fn classify_combat(message: &str) -> (String, NotificationType) {
    let t = message.trim_matches([' ', '\t', '\r', '\n']);
    // "to you!": the same attempt on someone else ends "...to <their name>!".
    if t.contains("Warp scramble attempt") && t.ends_with("to you!") {
        return ("Warp Scrambled".into(), NotificationType::WarpScrambled);
    }
    if t.contains("Warp disruption attempt") && t.ends_with("to you!") {
        return ("Warp Disrupted".into(), NotificationType::WarpDisrupted);
    }
    (String::new(), NotificationType::Generic)
}

fn classify_none(message: &str) -> (String, NotificationType) {
    let t = message.trim_matches([' ', '\t', '\r', '\n']);
    // Jumps and undocks are system changes, handled (and announced) separately.
    if t.starts_with("Jumping from") || t.starts_with("Undocking from") {
        return (String::new(), NotificationType::SystemChange);
    }
    if t.contains("is inviting you to a conversation") {
        return ("Convo request".into(), NotificationType::ConversationInvite);
    }
    (t.trim_matches([' ', '\t', '\r', '\n', '.']).to_owned(), NotificationType::Generic)
}

/// Decodes UTF-16 LE `bytes`, leaving an odd trailing byte or a lone trailing high surrogate in `carry` for next time.
fn decode_utf16le(carry: &mut Vec<u8>, bytes: &[u8]) -> String {
    carry.extend_from_slice(bytes);
    let mut usable = carry.len() & !1;
    if usable >= 2 {
        let last = u16::from_le_bytes([carry[usable - 2], carry[usable - 1]]);
        if (0xD800..0xDC00).contains(&last) {
            usable -= 2;
        }
    }
    let units: Vec<u16> = carry[..usable].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    carry.drain(..usable);
    String::from_utf16_lossy(&units)
}

/// True if `pattern` (ASCII) appears in UTF-16 LE `data` at an even offset.
fn contains_utf16_pattern(data: &[u8], pattern: &str) -> bool {
    let encoded: Vec<u8> = pattern.bytes().flat_map(|c| [c, 0]).collect();
    data.windows(encoded.len()).step_by(2).any(|w| w == encoded.as_slice())
}

#[derive(Default)]
struct LogFile {
    path: PathBuf,
    character: String,
    is_chatlog: bool,
    position: u64,
    last_size: u64,
    last_modified: Option<SystemTime>,
    /// Undecoded UTF-16 bytes left over from the previous read.
    carry: Vec<u8>,
    /// The incomplete last line from the previous read.
    line: Vec<u8>,
    has_bom: bool,
    last_system: Option<String>,
    idle_checks: u32,
    cycle_counter: u32,
    multiplier: u8,
    disabled: bool,
    long_line_warnings: u32,
}

/// Where parsed lines go: the event list and the trackers (split from the monitor so a file can be borrowed alongside).
struct Sink<'a> {
    events: &'a mut Vec<ChatlogEvent>,
    trackers: &'a ActivityTrackers,
}

impl Sink<'_> {
    fn notify(&mut self, character: &str, text: String, ntype: NotificationType) {
        self.events.push(ChatlogEvent::Notification { character: character.to_owned(), text, ntype });
    }
}

impl LogFile {
    fn kind(&self) -> &'static str {
        if self.is_chatlog { "chatlog" } else { "gamelog" }
    }

    /// Reads the file's size and its most recent system (scanning back from EOF), and starts tailing from EOF.
    fn read_initial_state(&mut self, sink: &mut Sink) -> std::io::Result<()> {
        let mut file = File::open(&self.path)?;
        let meta = file.metadata()?;
        let size = meta.len();
        self.last_size = size;
        self.last_modified = meta.modified().ok();
        if !self.is_chatlog && size >= 3 {
            let mut bom = [0u8; 3];
            if file.read_exact(&mut bom).is_ok() && bom == [0xEF, 0xBB, 0xBF] {
                self.has_bom = true;
            }
        }
        match self.find_system_backward(&mut file, size)? {
            Some((system, event_ts)) => {
                SLOG.debug(format_args!("Initial system for {}: {system} (event_ts={event_ts})", self.character));
                self.last_system = Some(system.clone());
                sink.events.push(ChatlogEvent::System { character: self.character.clone(), system, event_ts, is_jump: false });
            }
            None if size > MAX_BACKWARD_SCAN_BYTES => SLOG.warn(format_args!(
                "No system found for {} within the last {MAX_BACKWARD_SCAN_BYTES} bytes of {}; unknown until the next channel change or jump",
                self.character,
                self.path.display()
            )),
            None => {}
        }
        self.position = size;
        Ok(())
    }

    /// Scans back from EOF in overlapping chunks for the latest system announcement; stops at the first chunk with one.
    fn find_system_backward(&self, file: &mut File, size: u64) -> std::io::Result<Option<(String, u64)>> {
        let overlap: u64 = if self.is_chatlog { 128 } else { 256 };
        let floor = size.saturating_sub(MAX_BACKWARD_SCAN_BYTES) & !1;
        let mut scan_pos = size & !1;
        let mut buf = vec![0u8; SCAN_CHUNK_SIZE as usize];
        while scan_pos > floor {
            let chunk_size = SCAN_CHUNK_SIZE.min(scan_pos - floor);
            let start = scan_pos - chunk_size;
            file.seek(SeekFrom::Start(start))?;
            let n = read_up_to(file, &mut buf[..chunk_size as usize])?;
            if n == 0 {
                break;
            }
            let chunk = &buf[..n];
            let found = if self.is_chatlog {
                if contains_utf16_pattern(chunk, "Channel") { extract_system_from_chatlog(&decode_utf16le(&mut Vec::new(), chunk)) } else { None }
            } else if chunk.windows(12).any(|w| w == b"Jumping from") || chunk.windows(14).any(|w| w == b"Undocking from") {
                extract_system_from_gamelog(&String::from_utf8_lossy(chunk))
            } else {
                None
            };
            if found.is_some() {
                return Ok(found);
            }
            scan_pos = if start > floor + overlap { start + overlap } else { floor };
        }
        Ok(None)
    }

    /// Reads whatever was appended since the last poll and parses complete lines. Backs off on idle files.
    fn poll(&mut self, sink: &mut Sink, idle_threshold: u32, max_multiplier: u8) {
        if self.disabled {
            return;
        }
        self.cycle_counter += 1;
        if self.cycle_counter < self.multiplier as u32 {
            return;
        }
        self.cycle_counter = 0;
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Possibly mid-rotation; start over if it comes back.
                self.position = 0;
                self.last_size = 0;
                self.line.clear();
                self.carry.clear();
                return;
            }
            Err(e) => {
                SLOG.warn(format_args!("Failed to open {}: {e}", self.path.display()));
                return;
            }
        };
        if let Err(e) = self.read_new(&mut file, sink, idle_threshold, max_multiplier) {
            SLOG.err(format_args!("Error reading {}: {e}", self.path.display()));
        }
    }

    fn read_new(&mut self, file: &mut File, sink: &mut Sink, idle_threshold: u32, max_multiplier: u8) -> std::io::Result<()> {
        let meta = file.metadata()?;
        let size = meta.len();
        let modified = meta.modified().ok();
        if size == self.last_size && modified == self.last_modified {
            self.idle_checks += 1;
            if self.idle_checks >= idle_threshold && self.multiplier < max_multiplier {
                self.multiplier = (self.multiplier.max(1) * 2).min(max_multiplier);
                self.idle_checks = 0;
                SLOG.debug(format_args!("Poll backoff {} ({}): {}x", self.character, self.kind(), self.multiplier));
            }
            return Ok(());
        }
        self.idle_checks = 0;
        self.multiplier = 1;
        if size < self.last_size {
            // Truncated: read it again from the start.
            self.position = 0;
            self.line.clear();
            self.carry.clear();
        }
        let want = size.saturating_sub(self.position).min(MAX_READ_PER_POLL) as usize;
        let mut buf = vec![0u8; want];
        file.seek(SeekFrom::Start(self.position))?;
        let n = read_up_to(file, &mut buf)?;
        self.position += n as u64;
        // Only once caught up, so unread data makes the next poll read again.
        if self.position >= size {
            self.last_size = size;
            self.last_modified = modified;
        }
        if n == 0 {
            return Ok(());
        }
        if self.is_chatlog {
            let text = decode_utf16le(&mut self.carry, &buf[..n]);
            self.process(text.as_bytes(), sink);
        } else {
            self.process(&buf[..n], sink);
        }
        Ok(())
    }

    /// Appends `bytes` to the pending line and parses every complete line.
    fn process(&mut self, bytes: &[u8], sink: &mut Sink) {
        self.line.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(nl) = self.line[start..].iter().position(|&b| b == b'\n') {
            let end = start + nl;
            let line = String::from_utf8_lossy(&self.line[start..end]).into_owned();
            let trimmed = line.trim_matches([' ', '\r', '\t']);
            if !trimmed.is_empty() {
                self.parse_line(trimmed, sink);
            }
            start = end + 1;
        }
        self.line.drain(..start);
        if self.line.len() > MAX_LINE_LENGTH * 2 {
            SLOG.warn(format_args!("Discarding {} bytes without a newline for {} ({})", self.line.len(), self.character, self.kind()));
            self.line.clear();
        }
    }

    fn parse_line(&mut self, line: &str, sink: &mut Sink) {
        let mut line = line;
        if self.has_bom {
            if let Some(rest) = line.strip_prefix('\u{FEFF}') {
                line = rest;
            }
            self.has_bom = false;
        }
        if line.len() < MIN_LINE_LENGTH {
            return;
        }
        if line.len() > MAX_LINE_LENGTH {
            self.long_line_warnings += 1;
            if self.long_line_warnings <= 3 || self.long_line_warnings % 100 == 0 {
                SLOG.warn(format_args!("Dropping a {}-byte line for {} (over the {MAX_LINE_LENGTH}-byte cap)", line.len(), self.character));
            }
            return;
        }
        if self.is_chatlog {
            // Only the client's own "EVE System" speaker announces a system change.
            if is_system_change_line(line) {
                if let Some(system) = parse_system_change_from_chat(line) {
                    self.system_change(system, "chatlog", sink);
                }
            }
            return;
        }
        // Game log lines are "[ timestamp ] (type) message".
        let Some(message) = line.find("] ").map(|p| &line[p + 2..]) else { return };
        // Jump and undock lines carry a "(None)" tag in current clients; accept them with or without it.
        let untagged = message.strip_prefix("(None) ").unwrap_or(message);
        if untagged.starts_with("Jumping from") {
            if let Some(system) = parse_jump(line) {
                self.system_change(system, "jump", sink);
            }
            return;
        }
        if untagged.starts_with("Undocking from") {
            if let Some(system) = parse_undock(line) {
                self.system_change(system, "undock", sink);
            }
            return;
        }
        let mut tag = message.split_once(')').map_or("", |(t, _)| t);
        tag = tag.strip_prefix('(').unwrap_or("");
        match tag {
            "notify" => {
                if message.contains("Conduit Field") && message.contains("jumps you to") {
                    if let Some(system) = parse_conduit_jump(line) {
                        self.system_change(system, "conduit", sink);
                    }
                }
                self.combat_event(line, sink);
            }
            "question" | "combat" | "None" => self.combat_event(line, sink),
            "mining" => self.mining_event(line, sink),
            "bounty" => self.bounty_event(line, sink),
            _ => {}
        }
    }

    fn system_change(&mut self, system: String, kind: &str, sink: &mut Sink) {
        if self.last_system.as_deref() == Some(system.as_str()) {
            return;
        }
        // Only jumps pop a SystemChange notification: undock and the chat log's Local detection race the same event.
        if kind == "jump" {
            sink.notify(&self.character, format!("Jumped to {system}"), NotificationType::SystemChange);
        }
        SLOG.info(format_args!("System change ({kind}): {} -> {system}", self.character));
        sink.events.push(ChatlogEvent::System {
            character: self.character.clone(),
            system: system.clone(),
            event_ts: 0,
            is_jump: kind == "jump" || kind == "conduit",
        });
        self.last_system = Some(system);
    }

    fn combat_event(&self, line: &str, sink: &mut Sink) {
        let mut buf = [0u8; 512];
        let stripped = activity_tracker::strip_html(line, &mut buf);
        if let Some(tracker) = &sink.trackers.combat {
            if let Some(parsed) = activity_tracker::parse_combat_line(stripped) {
                // Hits from excluded weapons still count for DPS but don't retrigger the Taking Damage alert.
                let counts_for_alert = !activity_tracker::is_weapon_excluded(parsed.weapon, &sink.trackers.damage_alert_excluded_weapons);
                let _ = tracker.add_entry(&self.character, parsed.amount, parsed.is_incoming, crate::render::now_ms() as i64, counts_for_alert);
            }
        }
        if let Some((text, ntype)) = classify_notification(stripped) {
            sink.notify(&self.character, text, ntype);
        }
    }

    fn mining_event(&self, line: &str, sink: &mut Sink) {
        let (Some(tracker), Some(ore)) = (&sink.trackers.mining, &sink.trackers.ore) else { return };
        let Some(parsed) = activity_tracker::parse_mining_line(line) else { return };
        // A missing price counts as 0 ISK; a missing volume drops the yield, since m3 can't be computed without it.
        let Some((volume, price)) = ore(parsed.name()) else {
            SLOG.warn(format_args!("Unknown ore/ice/gas type in mining line, dropping yield: '{}'", parsed.name()));
            return;
        };
        let amount = parsed.amount as f32;
        let _ = tracker.add_entry(&self.character, amount * volume as f32, amount * price as f32, crate::render::now_ms() as i64);
    }

    fn bounty_event(&self, line: &str, sink: &mut Sink) {
        let Some(tracker) = &sink.trackers.bounty else { return };
        if let Some(isk) = activity_tracker::parse_bounty_line(line) {
            let _ = tracker.add_entry(&self.character, isk, crate::render::now_ms() as i64);
        }
    }
}

fn read_up_to(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Tails every monitored character's chat and game log. Single-threaded; see [`ChatlogWorker`] for the threaded runner.
pub struct ChatlogMonitor {
    chatlog_dir: PathBuf,
    gamelog_dir: PathBuf,
    files: Vec<LogFile>,
    /// Logged-in characters, including ones whose logs haven't appeared yet (re-checked when new files show up).
    characters: Vec<String>,
    /// Character name -> ID, from settings and from log headers.
    ids: BTreeMap<String, String>,
    dir_mtimes: [Option<SystemTime>; 2],
    events: Vec<ChatlogEvent>,
    idle_poll_threshold: u32,
    max_poll_multiplier: u8,
    pub trackers: ActivityTrackers,
}

impl ChatlogMonitor {
    pub fn new(chatlog_dir: impl Into<PathBuf>, gamelog_dir: impl Into<PathBuf>, idle_poll_threshold: u32, max_poll_multiplier: u8, ids: BTreeMap<String, String>) -> Self {
        let chatlog_dir = chatlog_dir.into();
        let gamelog_dir = gamelog_dir.into();
        let dir_mtimes = [dir_mtime(&chatlog_dir), dir_mtime(&gamelog_dir)];
        Self {
            chatlog_dir,
            gamelog_dir,
            files: Vec::new(),
            characters: Vec::new(),
            ids,
            dir_mtimes,
            events: Vec::new(),
            idle_poll_threshold: idle_poll_threshold.max(1),
            max_poll_multiplier: max_poll_multiplier.max(1),
            trackers: ActivityTrackers::default(),
        }
    }

    /// Starts monitoring a logged-in character (no-op for the generic "EVE" name or one already monitored).
    pub fn add_character(&mut self, name: &str) {
        if is_generic_character_name(name) || self.characters.iter().any(|c| c == name) {
            return;
        }
        self.characters.push(name.to_owned());
        self.attach(name);
    }

    /// Stops monitoring a character (logout or window closed).
    pub fn remove_character(&mut self, name: &str) {
        self.characters.retain(|c| c != name);
        self.files.retain(|f| {
            let keep = f.character != name;
            if !keep {
                SLOG.info(format_args!("Stopped monitoring {} for {name}: {}", f.kind(), f.path.display()));
            }
            keep
        });
        if let Some(t) = &self.trackers.combat {
            t.remove_character(name);
        }
        if let Some(t) = &self.trackers.mining {
            t.remove_character(name);
        }
        if let Some(t) = &self.trackers.bounty {
            t.remove_character(name);
        }
    }

    pub fn characters(&self) -> &[String] {
        &self.characters
    }

    /// One monitoring pass: picks up new or rotated log files, then reads what was appended to each.
    pub fn poll(&mut self) {
        let now = [dir_mtime(&self.chatlog_dir), dir_mtime(&self.gamelog_dir)];
        if now != self.dir_mtimes {
            self.dir_mtimes = now;
            for name in self.characters.clone() {
                self.attach(&name);
            }
        }
        let mut sink = Sink { events: &mut self.events, trackers: &self.trackers };
        for f in &mut self.files {
            f.poll(&mut sink, self.idle_poll_threshold, self.max_poll_multiplier);
        }
    }

    pub fn take_events(&mut self) -> Vec<ChatlogEvent> {
        std::mem::take(&mut self.events)
    }

    /// Finds and starts tailing the character's newest chat and game logs (replacing older ones: log rotation).
    fn attach(&mut self, name: &str) {
        for is_chatlog in [true, false] {
            let dir = if is_chatlog { self.chatlog_dir.clone() } else { self.gamelog_dir.clone() };
            if let Some(path) = self.find_log_file(&dir, is_chatlog, name) {
                self.add_log_file(path, name, is_chatlog);
            }
        }
    }

    fn add_log_file(&mut self, path: PathBuf, name: &str, is_chatlog: bool) {
        if self.files.iter().any(|f| f.path == path) {
            return;
        }
        if let Some(i) = self.files.iter().position(|f| f.is_chatlog == is_chatlog && f.character == name) {
            let old = self.files.remove(i);
            SLOG.info(format_args!("Log rotated for {name} ({}): {} -> {}", old.kind(), old.path.display(), path.display()));
        }
        let mut file = LogFile { path, character: name.to_owned(), is_chatlog, multiplier: 1, ..Default::default() };
        let mut sink = Sink { events: &mut self.events, trackers: &self.trackers };
        if let Err(e) = file.read_initial_state(&mut sink) {
            SLOG.warn(format_args!("Failed to open {} for initial read: {e}", file.path.display()));
            return;
        }
        SLOG.info(format_args!("Monitoring {} for {name}: {}", file.kind(), file.path.display()));
        self.files.push(file);
    }

    /// The newest log in `dir` belonging to `name`: by cached character ID if known, else by reading log headers newest
    /// first (and caching the ID found).
    fn find_log_file(&mut self, dir: &Path, is_chatlog: bool, name: &str) -> Option<PathBuf> {
        let candidates = log_candidates(dir, is_chatlog);
        if let Some(id) = self.ids.get(name) {
            let newest = candidates.iter().filter(|c| extract_character_id(c) == Some(id.as_str())).max_by_key(|c| parse_log_timestamp(c, is_chatlog));
            if let Some(c) = newest {
                return Some(dir.join(c));
            }
            // Stale ID: fall through to the header scan.
        }
        let mut sorted = candidates;
        sorted.sort_by_key(|c| std::cmp::Reverse(parse_log_timestamp(c, is_chatlog)));
        for candidate in sorted {
            let path = dir.join(&candidate);
            if read_listener(&path, is_chatlog).as_deref() != Some(name) {
                continue;
            }
            if let Some(id) = extract_character_id(&candidate) {
                if self.ids.get(name).map(String::as_str) != Some(id) {
                    self.ids.insert(name.to_owned(), id.to_owned());
                    self.events.push(ChatlogEvent::CharacterId { character: name.to_owned(), id: id.to_owned() });
                }
            }
            return Some(path);
        }
        None
    }
}

fn dir_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// Log file names in `dir` with a parseable timestamp: "Local_*.txt" for chat logs, other "*.txt" for game logs.
fn log_candidates(dir: &Path, is_chatlog: bool) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".txt") && n.starts_with("Local_") == is_chatlog && parse_log_timestamp(n, is_chatlog) != 0)
        .collect()
}

/// The "Listener:" name from a log's header.
fn read_listener(path: &Path, is_chatlog: bool) -> Option<String> {
    let mut buf = [0u8; 512];
    let n = read_up_to(&mut File::open(path).ok()?, &mut buf).ok()?;
    let text = if is_chatlog { decode_utf16le(&mut Vec::new(), &buf[..n & !1]) } else { String::from_utf8_lossy(&buf[..n]).into_owned() };
    extract_listener_name(&text).map(str::to_owned)
}

pub enum WorkerCommand {
    AddCharacter(String),
    RemoveCharacter(String),
}

/// Runs a [`ChatlogMonitor`] on its own thread: file I/O stays off the UI thread. `wake` is called (from the worker
/// thread) whenever events are waiting, so the UI loop can sleep until then.
pub struct ChatlogWorker {
    commands: Sender<WorkerCommand>,
    events: Receiver<ChatlogEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ChatlogWorker {
    pub fn spawn(mut monitor: ChatlogMonitor, poll_interval: Duration, wake: impl Fn() + Send + 'static) -> std::io::Result<ChatlogWorker> {
        let (cmd_tx, cmd_rx) = channel::<WorkerCommand>();
        let (ev_tx, ev_rx) = channel();
        let thread = std::thread::Builder::new().name("chatlog".into()).spawn(move || {
            SLOG.info(format_args!("Chatlog worker started"));
            loop {
                // Commands wake the worker early, so a login is picked up without waiting a full interval.
                match cmd_rx.recv_timeout(poll_interval) {
                    Ok(cmd) => {
                        apply(&mut monitor, cmd);
                        loop {
                            match cmd_rx.try_recv() {
                                Ok(cmd) => apply(&mut monitor, cmd),
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => return,
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                monitor.poll();
                let events = monitor.take_events();
                if !events.is_empty() {
                    for e in events {
                        if ev_tx.send(e).is_err() {
                            return;
                        }
                    }
                    wake();
                }
            }
            SLOG.info(format_args!("Chatlog worker stopped"));
        })?;
        Ok(ChatlogWorker { commands: cmd_tx, events: ev_rx, thread: Some(thread) })
    }

    pub fn send(&self, cmd: WorkerCommand) {
        let _ = self.commands.send(cmd);
    }

    pub fn try_events(&self) -> Vec<ChatlogEvent> {
        self.events.try_iter().collect()
    }

    /// Stops the worker and waits for it (dropping does the same).
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        // Replacing the sender disconnects the worker's receiver, which ends its loop.
        let (dead, _) = channel();
        self.commands = dead;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ChatlogWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn apply(monitor: &mut ChatlogMonitor, cmd: WorkerCommand) {
    match cmd {
        WorkerCommand::AddCharacter(name) => monitor.add_character(&name),
        WorkerCommand::RemoveCharacter(name) => monitor.remove_character(&name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn system_change_only_trusts_the_eve_system_speaker() {
        assert!(is_system_change_line("[ 2026.10.03 20:00:00 ] EVE System > Channel changed to Local : Jita"));
        assert!(is_system_change_line("\u{FEFF}[ 2026.10.03 20:00:00 ] EVE System > Channel changed to Local : Jita"));
        assert!(!is_system_change_line("[ 2026.10.03 20:00:00 ] Hostile > EVE System > Channel changed to Local : Jita"));
        assert!(!is_system_change_line("[ 2026.10.03 20:00:00 ] Hostile > Channel changed to Local : Jita"));
        assert!(!is_system_change_line("EVE System > Channel changed to Local : Jita"));
    }

    #[test]
    fn file_names_parse() {
        assert_eq!(parse_log_timestamp("Local_20261116_190330_90000001.txt", true), 20261116190330);
        assert_eq!(parse_log_timestamp("20261116_190330_90000001.txt", false), 20261116190330);
        assert_eq!(parse_log_timestamp("20261116_190314.txt", false), 20261116190314);
        assert_eq!(parse_log_timestamp("Corp_20261116_190330_90000001.txt", true), 0);
        assert_eq!(extract_character_id("Local_20261116_190330_90000001.txt"), Some("90000001"));
        assert_eq!(extract_character_id("20261116_190314.txt"), None);
    }

    #[test]
    fn systems_parse() {
        assert_eq!(parse_system_change_from_chat("[ 2026.11.16 07:00:23 ] EVE System > Channel changed to Local : ZZ-9TST").as_deref(), Some("ZZ-9TST"));
        assert_eq!(parse_jump("[ 2026.11.16 07:00:23 ] (None) Jumping from Jita to Perimeter.").as_deref(), Some("Perimeter"));
        assert_eq!(parse_jump("[ 2026.11.16 07:00:23 ] Jumping from Jita to Perimeter").as_deref(), Some("Perimeter"));
        assert_eq!(parse_undock("[ ts ] Undocking from Jita IV - Moon 4 in Jita").as_deref(), Some("Jita"));
        assert_eq!(parse_undock("[ ts ] (None) Undocking from Jita IV - Moon 4 - Caldari Navy Assembly Plant to Jita solar system.").as_deref(), Some("Jita"));
        assert_eq!(parse_conduit_jump("(notify) A Conduit Field activated by X jumps you to Amamake.").as_deref(), Some("Amamake"));
        assert_eq!(parse_conduit_jump("(notify) ... jumps you to Amamake, bringing along 3 passengers.").as_deref(), Some("Amamake"));
        let text = "[ 2026.11.16 07:00:23 ] EVE System > Channel changed to Local : A\n[ 2026.11.16 07:05:00 ] Bob > EVE System > Channel changed to Local : Fake\n[ 2026.11.16 07:09:57 ] EVE System > Channel changed to Local : B\n";
        assert_eq!(extract_system_from_chatlog(text), Some(("B".into(), 20261116070957)));
    }

    #[test]
    fn notifications_classify() {
        let c = |s: &str| classify_notification(s);
        assert_eq!(c("[ ts ] (question) Bob wants you to join their fleet, do you accept?"), Some(("Fleet invite".into(), NotificationType::FleetInvite)));
        assert_eq!(c("[ ts ] (question) Are you sure you want to quit the game?"), None);
        assert_eq!(c("[ ts ] (notify) Your fleet is disbanding"), Some(("Fleet disbanding".into(), NotificationType::FleetDisband)));
        assert_eq!(c("[ ts ] (notify) Your cloak deactivates due to a pulse from a Mobile Observatory"), Some(("Observatory Decloak".into(), NotificationType::ObservatoryDecloak)));
        assert_eq!(c("[ ts ] (notify) A Conduit Field activated by X jumps you to Amamake."), Some(("Taking Conduit to Amamake".into(), NotificationType::ConduitJump)));
        assert_eq!(c("[ ts ] (combat) Warp scramble attempt from Rat to you!"), Some(("Warp Scrambled".into(), NotificationType::WarpScrambled)));
        assert_eq!(c("[ ts ] (combat) Warp scramble attempt from Rat to Someone!"), None);
        assert_eq!(c("[ ts ] (None) Jumping from A to B."), None);
        assert_eq!(c("[ ts ] (None) Bob is inviting you to a conversation"), Some(("Convo request".into(), NotificationType::ConversationInvite)));
        assert_eq!(c("[ ts ] (hint) blah"), None);
    }

    /// Lines exactly as a current client (Nov 2026, under Proton) writes them.
    #[test]
    fn real_client_lines() {
        let mut events = Vec::new();
        let trackers = ActivityTrackers::default();
        let mut sink = Sink { events: &mut events, trackers: &trackers };
        let mut game = LogFile { character: "Some Pilot".into(), multiplier: 1, ..Default::default() };
        game.process(b"[ 2026.11.16 20:01:37 ] (None) Jumping from ZZ-9TST to TST-01\r\n[ 2026.11.16 20:02:47 ] (None) Jumping from TST-01 to ZZ-9TST\r\n", &mut sink);
        let mut chat = LogFile { character: "Some Pilot".into(), is_chatlog: true, multiplier: 1, ..Default::default() };
        chat.process("\u{FEFF}[ 2026.11.16 20:01:41 ] EVE System > Channel changed to Local : TST-01\r\n".as_bytes(), &mut sink);
        let system = |s: &str, jump| ChatlogEvent::System { character: "Some Pilot".into(), system: s.into(), event_ts: 0, is_jump: jump };
        let jumped = |s: &str| ChatlogEvent::Notification { character: "Some Pilot".into(), text: format!("Jumped to {s}"), ntype: NotificationType::SystemChange };
        assert_eq!(events, vec![jumped("TST-01"), system("TST-01", true), jumped("ZZ-9TST"), system("ZZ-9TST", true), system("TST-01", false)]);
    }

    #[test]
    fn html_lines_classify_after_stripping() {
        let line = r#"[ 2026.11.16 19:11:00 ] (question) <a href="showinfo:1377//1">Bob</a> wants you to join their fleet, do you accept?"#;
        let mut buf = [0u8; 512];
        let stripped = activity_tracker::strip_html(line, &mut buf);
        assert_eq!(classify_notification(stripped), Some(("Fleet invite".into(), NotificationType::FleetInvite)));
        let mut sink_events = Vec::new();
        let trackers = ActivityTrackers::default();
        let mut sink = Sink { events: &mut sink_events, trackers: &trackers };
        let mut f = LogFile { character: "Bob".into(), multiplier: 1, ..Default::default() };
        f.process(format!("{line}\r\n").as_bytes(), &mut sink);
        assert_eq!(sink_events, vec![ChatlogEvent::Notification { character: "Bob".into(), text: "Fleet invite".into(), ntype: NotificationType::FleetInvite }]);
    }

    #[test]
    fn utf16_decoding_carries_split_characters() {
        let encoded: Vec<u8> = "Hi 😀!".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut carry = Vec::new();
        let mut out = String::new();
        for chunk in encoded.chunks(3) {
            out += &decode_utf16le(&mut carry, chunk);
        }
        assert_eq!(out, "Hi 😀!");
        assert!(carry.is_empty());
        assert!(contains_utf16_pattern(&encoded, "Hi"));
        assert!(!contains_utf16_pattern(&encoded, "Ho"));
    }

    fn utf16(s: &str) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        v.extend(s.encode_utf16().flat_map(|u| u.to_le_bytes()));
        v
    }

    #[test]
    fn monitor_finds_tails_and_rotates_logs() {
        let root = std::env::temp_dir().join(format!("eve-maj-chatlog-test-{}", std::process::id()));
        let (chat, game) = (root.join("Chatlogs"), root.join("Gamelogs"));
        std::fs::create_dir_all(&chat).unwrap();
        std::fs::create_dir_all(&game).unwrap();
        let header = |name: &str| format!("------\r\n  Channel ID: local\r\n  Listener:        {name}\r\n------\r\n\r\n");
        std::fs::write(chat.join("Local_20261116_070000_11111111.txt"), utf16(&(header("Other Pilot") + "[ 2026.11.16 07:00:00 ] EVE System > Channel changed to Local : Nope\r\n"))).unwrap();
        let mine = chat.join("Local_20261116_080000_22222222.txt");
        std::fs::write(&mine, utf16(&(header("Some Pilot") + "[ 2026.11.16 08:00:01 ] EVE System > Channel changed to Local : ZZ-9TST\r\n"))).unwrap();
        let gamelog = game.join("20261116_080000_22222222.txt");
        std::fs::write(&gamelog, "------\r\n  Gamelog\r\n  Listener: Some Pilot\r\n------\r\n").unwrap();

        let mut m = ChatlogMonitor::new(&chat, &game, 5, 2, BTreeMap::new());
        m.add_character("Some Pilot");
        m.add_character("EVE");
        let events = m.take_events();
        assert!(events.contains(&ChatlogEvent::CharacterId { character: "Some Pilot".into(), id: "22222222".into() }));
        assert!(events.contains(&ChatlogEvent::System { character: "Some Pilot".into(), system: "ZZ-9TST".into(), event_ts: 20261116080001, is_jump: false }));
        assert_eq!(m.files.len(), 2);

        // Appended lines, one of them split across two writes.
        let mut f = std::fs::OpenOptions::new().append(true).open(&gamelog).unwrap();
        f.write_all(b"[ 2026.11.16 08:01:00 ] (None) Jumping from ZZ-9TST to 1DQ1-A.\r\n[ 2026.11.16 08:01:05 ] (notify) Your fleet is dis").unwrap();
        m.poll();
        f.write_all(b"banding\r\n").unwrap();
        drop(f);
        // Same size check needs a changed size/mtime, which the append gives.
        m.poll();
        let events = m.take_events();
        assert!(events.contains(&ChatlogEvent::Notification { character: "Some Pilot".into(), text: "Jumped to 1DQ1-A".into(), ntype: NotificationType::SystemChange }));
        assert!(events.contains(&ChatlogEvent::System { character: "Some Pilot".into(), system: "1DQ1-A".into(), event_ts: 0, is_jump: true }));
        assert!(events.contains(&ChatlogEvent::Notification { character: "Some Pilot".into(), text: "Fleet disbanding".into(), ntype: NotificationType::FleetDisband }));

        // A player faking the system announcement is ignored; the real one comes through.
        let mut f = std::fs::OpenOptions::new().append(true).open(&mine).unwrap();
        let more: Vec<u8> = "[ 2026.11.16 08:02:00 ] Troll > EVE System > Channel changed to Local : Jita\r\n[ 2026.11.16 08:02:01 ] EVE System > Channel changed to Local : 1DQ1-A\r\n"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        f.write_all(&more).unwrap();
        drop(f);
        m.poll();
        // 1DQ1-A is new for the chat log's own dedup, so it's reported (the front end ignores the repeat).
        assert_eq!(m.take_events(), vec![ChatlogEvent::System { character: "Some Pilot".into(), system: "1DQ1-A".into(), event_ts: 0, is_jump: false }]);

        // A newer game log (relog) replaces the old one once the directory changes.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(game.join("20261116_090000_22222222.txt"), "------\r\n  Listener: Some Pilot\r\n------\r\n[ 2026.11.16 09:00:00 ] (None) Undocking from Station to Jita solar system.\r\n").unwrap();
        // Directory mtime resolution can be coarse; force the rescan.
        m.dir_mtimes = [None, None];
        m.poll();
        assert!(m.files.iter().any(|f| f.path.ends_with("20261116_090000_22222222.txt")));
        assert_eq!(m.files.iter().filter(|f| !f.is_chatlog).count(), 1);
        assert!(m.take_events().contains(&ChatlogEvent::System { character: "Some Pilot".into(), system: "Jita".into(), event_ts: 20261116090000, is_jump: false }));

        m.remove_character("Some Pilot");
        assert!(m.files.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
