//! Parser for FramePacer's plain-text log (`framepacer.log`).
//!
//! Only the documented, human-readable log format is used (no reverse engineering):
//! `TIMESTAMP | LEVEL | component | header k=v ... | event.name | body k=v ...`

/// One parsed log line.
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    pub timestamp: String,
    pub level: String,
    pub component: String,
    /// `pid=` from the header (the process that wrote the line).
    pub writer_pid: Option<u32>,
    pub event: String,
    pub fields: Vec<(String, String)>,
}

impl LogLine {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn get_u32(&self, key: &str) -> Option<u32> {
        let v = self.get(key)?;
        if let Ok(n) = v.parse::<u32>() {
            return Some(n);
        }
        // Some lines use decimals, e.g. `target_fps=60.000`.
        let f = v.parse::<f64>().ok()?;
        if f.is_finite() && f >= 0.0 {
            Some(f.round() as u32)
        } else {
            None
        }
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.parse::<f64>().ok()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            "true" | "1" | "yes" => Some(true),
            "false" | "0" | "no" => Some(false),
            _ => None,
        }
    }
}

/// Parses a single log line. Returns `None` for lines that don't follow the format.
pub fn parse_line(line: &str) -> Option<LogLine> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.splitn(6, " | ");
    let timestamp = parts.next()?.trim();
    let level = parts.next()?.trim();
    let component = parts.next()?.trim();
    let header = parts.next()?;
    let event = parts.next()?.trim();
    let body = parts.next().unwrap_or("");
    if timestamp.is_empty() || event.is_empty() || event.contains('=') {
        return None;
    }
    let header_fields = parse_kv(header);
    let writer_pid = header_fields
        .iter()
        .find(|(k, _)| k == "pid")
        .and_then(|(_, v)| v.parse().ok());
    Some(LogLine {
        timestamp: timestamp.to_string(),
        level: level.to_string(),
        component: component.to_string(),
        writer_pid,
        event: event.to_string(),
        fields: parse_kv(body),
    })
}

/// Parses `k=v k2="quoted value" ...`.
pub fn parse_kv(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let n = chars.len();
    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < n && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n || chars[i] != '=' {
            // bare token without '=' - skip it
            while i < n && !chars[i].is_whitespace() {
                i += 1;
            }
            continue;
        }
        let key: String = chars[key_start..i].iter().collect();
        i += 1; // '='
        let value: String = if i < n && chars[i] == '"' {
            i += 1;
            let vs = i;
            while i < n && chars[i] != '"' {
                i += 1;
            }
            let v: String = chars[vs..i].iter().collect();
            if i < n {
                i += 1; // closing quote
            }
            v
        } else {
            let vs = i;
            while i < n && !chars[i].is_whitespace() {
                i += 1;
            }
            chars[vs..i].iter().collect()
        };
        if !key.is_empty() {
            out.push((key, value));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HDR: &str = "pid=7108 tid=10392 seq=97 monotonic_ms=186880531 session=0B2D1A0A710672DC game_session=0AFE5A4CF1018762 version=0.6.0.0 build_id=6AE7C178-000F1000";

    #[test]
    fn parses_setting_changed_hotkey() {
        let l = format!(
            "2026-11-12T07:05:37.500Z | INFO | app | {HDR} | limiter.setting_changed | source=ingame_hotkey action=increase previous_target_fps=55 target_fps=60 enabled=true"
        );
        let p = parse_line(&l).unwrap();
        assert_eq!(p.event, "limiter.setting_changed");
        assert_eq!(p.component, "app");
        assert_eq!(p.writer_pid, Some(7108));
        assert_eq!(p.get("source"), Some("ingame_hotkey"));
        assert_eq!(p.get_u32("target_fps"), Some(60));
        assert_eq!(p.get_u32("previous_target_fps"), Some(55));
        assert_eq!(p.get_bool("enabled"), Some(true));
    }

    #[test]
    fn parses_quoted_and_decimal() {
        let l = format!(
            "2026-11-12T07:54:05.165Z | INFO | dll64 | {HDR} | pacing.state | process=\"Yakuza 4.exe\" api=DXGI target_fps=55.000 enabled=true"
        );
        let p = parse_line(&l).unwrap();
        assert_eq!(p.get("process"), Some("Yakuza 4.exe"));
        assert_eq!(p.get_u32("target_fps"), Some(55));
        assert_eq!(p.get_f64("target_fps"), Some(55.0));
    }

    #[test]
    fn parses_line_without_body() {
        let l = format!("2026-11-12T07:56:41.519Z | INFO | app | {HDR} | detector.stopped\r\n");
        let p = parse_line(&l).unwrap();
        assert_eq!(p.event, "detector.stopped");
        assert!(p.fields.is_empty());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_line("").is_none());
        assert!(parse_line("hello world").is_none());
        assert!(parse_line("a | b | c").is_none());
    }

    #[test]
    fn kv_edge_cases() {
        let kv = parse_kv("  a=1  bare b=\"x y\" c= d=\"unterminated");
        assert_eq!(
            kv,
            vec![
                ("a".into(), "1".into()),
                ("b".into(), "x y".into()),
                ("c".into(), "".into()),
                ("d".into(), "unterminated".into()),
            ]
        );
    }
}
