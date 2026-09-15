//! Transaction history logging and reading for `rum history`.
//!
//! Transactions are recorded in an append-only JSON Lines (JSONL) journal at
//! `/var/lib/rum/history.jsonl` (or fallback to user cachedir when non-root).
//! This provides full auditing, intent tracking (distinguishing explicit user
//! targets from transitive dependencies), and instant O(1) appending.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

/// An altered package in a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlteredPackage {
    pub nevra: String,
    pub state: PackageState,
    pub is_explicit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageState {
    Installed,
    Upgraded,
    Removed,
}

impl PackageState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PackageState::Installed => "Install",
            PackageState::Upgraded => "Upgrade",
            PackageState::Removed => "Remove",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Install" | "Installed" => Some(PackageState::Installed),
            "Upgrade" | "Upgraded" => Some(PackageState::Upgraded),
            "Remove" | "Removed" => Some(PackageState::Removed),
            _ => None,
        }
    }
}

/// A complete transaction record stored as one JSON line in `history.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionRecord {
    pub id: u64,
    pub timestamp: String,
    pub command: String,
    pub action: String,
    pub requested: Vec<String>,
    pub altered: Vec<AlteredPackage>,
}

/// Locate the active `history.jsonl` file.
pub fn history_path() -> PathBuf {
    let var_lib = PathBuf::from("/var/lib/rum");
    if crate::sys::is_root() || var_lib.is_dir() {
        var_lib.join("history.jsonl")
    } else {
        crate::sys::effective_cachedir("/var/lib/rum").join("history.jsonl")
    }
}

/// Record a completed transaction to the journal.
/// Never panics or returns error that fails the transaction if logging encounters I/O issues.
pub fn record_transaction(
    command: &str,
    action: &str,
    requested: &[String],
    altered: Vec<AlteredPackage>,
) {
    if altered.is_empty() {
        return;
    }

    let path = history_path();
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("could not create history directory {}: {e}", parent.display());
            return;
        }
    }

    let existing = read_records().unwrap_or_default();
    let next_id = existing.last().map(|r| r.id + 1).unwrap_or(1);
    let timestamp = current_utc_timestamp();

    let record = TransactionRecord {
        id: next_id,
        timestamp,
        command: command.to_string(),
        action: action.to_string(),
        requested: requested.to_vec(),
        altered,
    };

    let line = to_json_line(&record);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    } else {
        tracing::warn!("could not append to transaction history {}", path.display());
    }
}

/// Read all transaction records from the journal.
pub fn read_records() -> anyhow::Result<Vec<TransactionRecord>> {
    let path = history_path();
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(&path)?;
    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rec) = from_json_line(trimmed) {
            records.push(rec);
        }
    }

    Ok(records)
}

/// Civil calendar UTC timestamp format: `YYYY-MM-DD HH:MM:SS UTC`.
pub fn current_utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day, hour, min, sec) = unix_to_datetime(secs);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{min:02}:{sec:02} UTC")
}

pub fn unix_to_datetime(secs: u64) -> (i64, u8, u8, u8, u8, u8) {
    let sec = (secs % 60) as u8;
    let mins = secs / 60;
    let min = (mins % 60) as u8;
    let hours = mins / 60;
    let hour = (hours % 24) as u8;
    let mut days = (hours / 24) as i64;

    // Shift to epoch 0000-03-01
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, hour, min, sec)
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

/// Serialize a record to a compact JSON line.
pub fn to_json_line(tx: &TransactionRecord) -> String {
    let mut s = String::new();
    s.push('{');
    s.push_str(&format!("\"id\":{},", tx.id));
    s.push_str(&format!("\"timestamp\":\"{}\",", escape_json(&tx.timestamp)));
    s.push_str(&format!("\"command\":\"{}\",", escape_json(&tx.command)));
    s.push_str(&format!("\"action\":\"{}\",", escape_json(&tx.action)));
    s.push_str("\"requested\":[");
    for (i, r) in tx.requested.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(&escape_json(r));
        s.push('"');
    }
    s.push_str("],\"altered\":[");
    for (i, p) in tx.altered.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"nevra\":\"{}\",\"state\":\"{}\",\"is_explicit\":{}}}",
            escape_json(&p.nevra),
            p.state.as_str(),
            p.is_explicit
        ));
    }
    s.push_str("]}");
    s
}

// Minimal, zero-dependency JSON parser for TransactionRecord.

#[derive(Debug, PartialEq)]
enum JsonVal {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<JsonVal>),
    Obj(Vec<(String, JsonVal)>),
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        if self.pos < self.bytes.len() {
            Some(self.bytes[self.pos])
        } else {
            None
        }
    }

    fn parse_value(&mut self) -> Option<JsonVal> {
        self.skip_whitespace();
        match self.peek()? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(JsonVal::Str),
            b't' | b'f' => self.parse_bool().map(JsonVal::Bool),
            b'n' => {
                if self.bytes[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Some(JsonVal::Null)
                } else {
                    None
                }
            }
            b'0'..=b'9' => self.parse_num().map(JsonVal::Num),
            _ => None,
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.pos += 1;
        let mut s = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            if b == b'"' {
                return Some(s);
            }
            if b == b'\\' {
                if self.pos >= self.bytes.len() {
                    return None;
                }
                let esc = self.bytes[self.pos];
                self.pos += 1;
                match esc {
                    b'"' => s.push('"'),
                    b'\\' => s.push('\\'),
                    b'n' => s.push('\n'),
                    b'r' => s.push('\r'),
                    b't' => s.push('\t'),
                    _ => s.push(esc as char),
                }
            } else {
                s.push(b as char);
            }
        }
        None
    }

    fn parse_bool(&mut self) -> Option<bool> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Some(true)
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Some(false)
        } else {
            None
        }
    }

    fn parse_num(&mut self) -> Option<u64> {
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        s.parse::<u64>().ok()
    }

    fn parse_array(&mut self) -> Option<JsonVal> {
        if self.peek()? != b'[' {
            return None;
        }
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(JsonVal::Arr(items));
        }
        loop {
            let val = self.parse_value()?;
            items.push(val);
            self.skip_whitespace();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    return Some(JsonVal::Arr(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_object(&mut self) -> Option<JsonVal> {
        if self.peek()? != b'{' {
            return None;
        }
        self.pos += 1;
        let mut fields = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(JsonVal::Obj(fields));
        }
        loop {
            self.skip_whitespace();
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek()? != b':' {
                return None;
            }
            self.pos += 1;
            let val = self.parse_value()?;
            fields.push((key, val));
            self.skip_whitespace();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    return Some(JsonVal::Obj(fields));
                }
                _ => return None,
            }
        }
    }
}

pub fn from_json_line(line: &str) -> Option<TransactionRecord> {
    let mut parser = JsonParser::new(line);
    let val = parser.parse_value()?;
    let fields = match val {
        JsonVal::Obj(f) => f,
        _ => return None,
    };

    let mut id = None;
    let mut timestamp = None;
    let mut command = None;
    let mut action = None;
    let mut requested = Vec::new();
    let mut altered = Vec::new();

    for (k, v) in fields {
        match (k.as_str(), v) {
            ("id", JsonVal::Num(n)) => id = Some(n),
            ("timestamp", JsonVal::Str(s)) => timestamp = Some(s),
            ("command", JsonVal::Str(s)) => command = Some(s),
            ("action", JsonVal::Str(s)) => action = Some(s),
            ("requested", JsonVal::Arr(items)) => {
                for item in items {
                    if let JsonVal::Str(s) = item {
                        requested.push(s);
                    }
                }
            }
            ("altered", JsonVal::Arr(items)) => {
                for item in items {
                    if let JsonVal::Obj(obj) = item {
                        let mut nevra = None;
                        let mut state = None;
                        let mut is_explicit = false;
                        for (ok, ov) in obj {
                            match (ok.as_str(), ov) {
                                ("nevra", JsonVal::Str(s)) => nevra = Some(s),
                                ("state", JsonVal::Str(s)) => state = PackageState::parse(&s),
                                ("is_explicit", JsonVal::Bool(b)) => is_explicit = b,
                                _ => {}
                            }
                        }
                        if let (Some(n), Some(st)) = (nevra, state) {
                            altered.push(AlteredPackage {
                                nevra: n,
                                state: st,
                                is_explicit,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Some(TransactionRecord {
        id: id?,
        timestamp: timestamp.unwrap_or_default(),
        command: command.unwrap_or_default(),
        action: action.unwrap_or_else(|| "Unknown".to_string()),
        requested,
        altered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datetime_formatting() {
        // 2026-09-15 09:30:15 UTC is 1789464615 secs since epoch
        let (y, m, d, hh, mm, ss) = unix_to_datetime(1789464615);
        assert_eq!((y, m, d, hh, mm, ss), (2026, 9, 15, 9, 30, 15));
    }

    #[test]
    fn test_round_trip_json() {
        let tx = TransactionRecord {
            id: 42,
            timestamp: "2026-09-15 09:30:00 UTC".into(),
            command: "rum install redis \"foo bar\"".into(),
            action: "Install".into(),
            requested: vec!["redis".into(), "foo bar".into()],
            altered: vec![
                AlteredPackage {
                    nevra: "redis-7.0.12-1.el9.x86_64".into(),
                    state: PackageState::Installed,
                    is_explicit: true,
                },
                AlteredPackage {
                    nevra: "redis-common-7.0.12-1.el9.x86_64".into(),
                    state: PackageState::Installed,
                    is_explicit: false,
                },
            ],
        };

        let json = to_json_line(&tx);
        let parsed = from_json_line(&json).expect("failed to parse json line");
        assert_eq!(tx, parsed);
    }
}
