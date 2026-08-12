use parallax_types::{NormalizedTick, RawEvent};
use serde::Deserialize;
use std::path::Path;

/// One line of a replay corpus (design doc §15): either a venue market
/// data update or an external alpha fact, in the exact envelope each
/// would arrive in live. A JSONL file of these, in chronological order,
/// is a complete historical scenario — weather bulletins, econ releases,
/// and news headlines interleaved with the order-book ticks they should
/// move.
///
/// Externally tagged (`{"tick": {...}}` / `{"alpha": {...}}`) rather than
/// internally tagged deliberately: `RawEvent` already has its own `kind`
/// field (`AlphaEventKind`), and an internal tag sharing that name would
/// silently collide with it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEvent {
    Tick(NormalizedTick),
    Alpha(RawEvent),
}

pub fn load_jsonl(path: &Path) -> std::io::Result<Vec<ReplayEvent>> {
    let content = std::fs::read_to_string(path)?;
    parse_jsonl(&content)
}

pub fn parse_jsonl(content: &str) -> std::io::Result<Vec<ReplayEvent>> {
    let mut events = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let event: ReplayEvent = serde_json::from_str(line).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("line {}: {e}", line_no + 1),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interleaved_tick_and_alpha_lines() {
        let content = r#"
{"tick":{"venue":"polymarket","contract":"wx.temp.chicago.gt.869.2026-08-12.nws_official","bid":0.55,"bid_size":50.0,"ask":0.58,"ask_size":50.0,"venue_ts":null,"receive_ts":0}}
{"alpha":{"source":"hrrr","kind":"weather","publish_ts":null,"receive_ts":1000,"payload":{"contract":"wx.temp.chicago.gt.869.2026-08-12.nws_official","threshold_tenths":869,"ensemble_forecast_tenths":[900,910,895]}}}
"#;
        let events = parse_jsonl(content).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], ReplayEvent::Tick(_)));
        assert!(matches!(events[1], ReplayEvent::Alpha(_)));
    }

    #[test]
    fn skips_blank_lines_and_comments() {
        let content = "\n# a comment\n\n{\"tick\":{\"venue\":\"kalshi\",\"contract\":\"c\",\"bid\":0.5,\"bid_size\":1.0,\"ask\":0.51,\"ask_size\":1.0,\"venue_ts\":null,\"receive_ts\":0}}\n";
        let events = parse_jsonl(content).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn malformed_line_is_a_clear_error_with_line_number() {
        let content = "{\"tick\": \"not an object\"}";
        let err = parse_jsonl(content).unwrap_err();
        assert!(err.to_string().contains("line 1"));
    }
}
