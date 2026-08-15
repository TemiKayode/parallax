//! `docs/GOING-LIVE.md` Stage 1, "order lifecycle that survives a
//! restart":
//!
//! > `in_flight` tracking in memory is not enough. Persist intent
//! > *before* sending and outcome *after*, so a crash between the two is
//! > recoverable. On restart the question "did I have an order out?"
//! > must be answerable from disk, not inferred.
//!
//! An append-only JSONL log — the same durability pattern this repo
//! already uses for its replay corpus (`parallax_sim::load_jsonl`), and
//! for the same reason: a plain, inspectable, crash-safe file beats an
//! in-memory map for the one piece of state that has to survive the
//! process dying. Each line is one `JournalEntry`; a caller logs an
//! `Intent` immediately before calling `VenueAdapter::submit` and an
//! `Outcome` immediately after it resolves. `recover_unresolved` replays
//! the file and returns every order whose `Intent` has no matching
//! `Outcome` — the exact set `reconcile::reconcile_startup` needs to
//! resolve against the venue's own truth before trading can begin.

use parallax_types::{AckStatus, ClientOrderId, OrderAck, OrderIntent, Side, Timestamp, VenueId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalEntry {
    Intent {
        client_order_id: ClientOrderId,
        intent: OrderIntent,
        logged_at: Timestamp,
    },
    Outcome {
        client_order_id: ClientOrderId,
        ack: OrderAck,
        logged_at: Timestamp,
    },
}

pub struct OrderJournal {
    file: File,
}

impl OrderJournal {
    /// Opens (creating if needed) a journal file in append mode. Never
    /// truncates — a journal that could lose prior entries on reopen
    /// would defeat the entire point of persisting across a restart.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(OrderJournal { file })
    }

    pub fn log_intent(
        &mut self,
        client_order_id: &ClientOrderId,
        intent: &OrderIntent,
    ) -> io::Result<()> {
        self.append(&JournalEntry::Intent {
            client_order_id: client_order_id.clone(),
            intent: intent.clone(),
            logged_at: Timestamp::now(),
        })
    }

    pub fn log_outcome(
        &mut self,
        client_order_id: &ClientOrderId,
        ack: &OrderAck,
    ) -> io::Result<()> {
        self.append(&JournalEntry::Outcome {
            client_order_id: client_order_id.clone(),
            ack: ack.clone(),
            logged_at: Timestamp::now(),
        })
    }

    fn append(&mut self, entry: &JournalEntry) -> io::Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        writeln!(self.file, "{line}")?;
        // A journal entry that's written but not actually on disk when
        // the process dies is exactly the failure mode this file exists
        // to close — the OS write buffer alone isn't enough.
        self.file.flush()
    }
}

/// Replays a journal file and returns every `(client_order_id, intent)`
/// pair logged as `Intent` with no later matching `Outcome` — orders that
/// were "about to be sent" when the process stopped. An empty, or
/// nonexistent, journal returns an empty list (a fresh deployment with no
/// journal yet has nothing to recover, which is a normal startup, not an
/// error).
pub fn recover_unresolved(path: &Path) -> io::Result<Vec<(ClientOrderId, OrderIntent)>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut intents: HashMap<ClientOrderId, OrderIntent> = HashMap::new();
    let mut resolved: HashMap<ClientOrderId, ()> = HashMap::new();

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry: JournalEntry = serde_json::from_str(line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: {e}", line_no + 1),
            )
        })?;
        match entry {
            JournalEntry::Intent {
                client_order_id,
                intent,
                ..
            } => {
                intents.insert(client_order_id, intent);
            }
            JournalEntry::Outcome {
                client_order_id, ..
            } => {
                resolved.insert(client_order_id, ());
            }
        }
    }

    Ok(intents
        .into_iter()
        .filter(|(id, _)| !resolved.contains_key(id))
        .collect())
}

/// One fill, ready for an accounting ledger — `docs/GOING-LIVE.md`
/// Stage 3: "keep records for accounting... much harder to reconstruct
/// after the fact than to capture as you go." Built by joining each
/// `Outcome` journal entry that resolved to a fill against the `Intent`
/// entry sharing its `client_order_id`, since a fill ack alone
/// (`AckStatus::Filled`) carries only qty/price — not which contract or
/// side it was.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerRow {
    pub logged_at: Timestamp,
    pub client_order_id: ClientOrderId,
    pub venue: VenueId,
    pub contract: String,
    pub side: Side,
    pub qty: f64,
    pub price: f64,
    pub notional: f64,
}

/// Replays a journal file and emits one `LedgerRow` per fill (full or
/// partial) it contains, in the order logged. An `Outcome` entry with no
/// matching `Intent` line is skipped rather than emitted with placeholder
/// data — a ledger row with a guessed contract is worse than a missing
/// one, which at least doesn't misinform whoever reads it.
pub fn export_fill_ledger(path: &Path) -> io::Result<Vec<LedgerRow>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut intents: HashMap<ClientOrderId, OrderIntent> = HashMap::new();
    let mut rows = Vec::new();

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let entry: JournalEntry = serde_json::from_str(line).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line {}: {e}", line_no + 1),
            )
        })?;
        match entry {
            JournalEntry::Intent {
                client_order_id,
                intent,
                ..
            } => {
                intents.insert(client_order_id, intent);
            }
            JournalEntry::Outcome {
                client_order_id,
                ack,
                logged_at,
            } => {
                let Some(intent) = intents.get(&client_order_id) else {
                    continue;
                };
                let (qty, price) = match ack.status {
                    AckStatus::Filled { qty, price } => (qty, price),
                    AckStatus::PartiallyFilled {
                        filled_qty, price, ..
                    } => (filled_qty, price),
                    _ => continue,
                };
                rows.push(LedgerRow {
                    logged_at,
                    client_order_id,
                    venue: ack.venue,
                    contract: intent.contract.0.clone(),
                    side: intent.side,
                    qty,
                    price,
                    notional: qty * price,
                });
            }
        }
    }

    Ok(rows)
}

/// A field escaped per RFC 4180: wrapped in quotes with any embedded
/// quote doubled, whenever it contains a comma, quote, or newline — a
/// contract label built from real venue text is exactly the kind of
/// string that shouldn't be trusted to never contain one.
fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Writes `rows` as CSV — the least glamorous, most durable accounting
/// format there is.
pub fn write_ledger_csv(rows: &[LedgerRow], out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "logged_at_ns,client_order_id,venue,contract,side,qty,price,notional"
    )?;
    for row in rows {
        writeln!(
            out,
            "{},{},{:?},{},{:?},{},{},{}",
            row.logged_at.as_nanos(),
            csv_field(&row.client_order_id.0),
            row.venue,
            csv_field(&row.contract),
            row.side,
            row.qty,
            row.price,
            row.notional,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::{
        AckStatus, CanonicalContractId, EngineId, OrderId, OrderType, Outcome, Side, VenueId,
    };

    fn intent(size: f64) -> OrderIntent {
        OrderIntent {
            venue: VenueId::Kalshi,
            contract: CanonicalContractId("wx.temp.chicago.gt_869.test.nws_official".into()),
            outcome: Outcome::Yes,
            side: Side::Buy,
            price: 0.5,
            size,
            order_type: OrderType::Limit,
            engine: EngineId::MarketMaking,
            created_at: Timestamp::from_nanos(0),
        }
    }

    fn ack() -> OrderAck {
        OrderAck {
            order_id: OrderId("k-1".into()),
            venue: VenueId::Kalshi,
            status: AckStatus::Accepted,
            ts: Timestamp::from_nanos(0),
        }
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "parallax_journal_test_{}_{name}",
            std::process::id()
        ));
        p
    }

    #[test]
    fn a_nonexistent_journal_has_nothing_to_recover() {
        let path = temp_path("missing.jsonl");
        std::fs::remove_file(&path).ok();
        assert!(recover_unresolved(&path).unwrap().is_empty());
    }

    #[test]
    fn an_intent_with_no_outcome_is_unresolved() {
        let path = temp_path("unresolved.jsonl");
        std::fs::remove_file(&path).ok();
        let id = ClientOrderId::derive(&intent(10.0));
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id, &intent(10.0)).unwrap();
        }
        let unresolved = recover_unresolved(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].0, id);
    }

    #[test]
    fn an_intent_followed_by_its_outcome_is_resolved() {
        let path = temp_path("resolved.jsonl");
        std::fs::remove_file(&path).ok();
        let id = ClientOrderId::derive(&intent(10.0));
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id, &intent(10.0)).unwrap();
            journal.log_outcome(&id, &ack()).unwrap();
        }
        let unresolved = recover_unresolved(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(unresolved.is_empty());
    }

    #[test]
    fn only_the_genuinely_unresolved_order_survives_among_several() {
        let path = temp_path("mixed.jsonl");
        std::fs::remove_file(&path).ok();
        let resolved_id = ClientOrderId::derive(&intent(10.0));
        let unresolved_id = ClientOrderId::derive(&intent(20.0));
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&resolved_id, &intent(10.0)).unwrap();
            journal.log_intent(&unresolved_id, &intent(20.0)).unwrap();
            journal.log_outcome(&resolved_id, &ack()).unwrap();
        }
        let unresolved = recover_unresolved(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].0, unresolved_id);
    }

    #[test]
    fn reopening_a_journal_appends_rather_than_truncating() {
        let path = temp_path("append.jsonl");
        std::fs::remove_file(&path).ok();
        let id_a = ClientOrderId::derive(&intent(1.0));
        let id_b = ClientOrderId::derive(&intent(2.0));
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id_a, &intent(1.0)).unwrap();
        }
        {
            // Simulates a restart: opening the same path again must not
            // lose the entry written before the "crash".
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id_b, &intent(2.0)).unwrap();
        }
        let unresolved = recover_unresolved(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let ids: Vec<_> = unresolved.into_iter().map(|(id, _)| id).collect();
        assert!(ids.contains(&id_a));
        assert!(ids.contains(&id_b));
    }

    fn filled_ack(qty: f64, price: f64) -> OrderAck {
        OrderAck {
            order_id: OrderId("k-1".into()),
            venue: VenueId::Kalshi,
            status: AckStatus::Filled { qty, price },
            ts: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn export_fill_ledger_joins_a_fill_against_its_intent() {
        let path = temp_path("ledger.jsonl");
        std::fs::remove_file(&path).ok();
        let i = intent(10.0);
        let id = ClientOrderId::derive(&i);
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id, &i).unwrap();
            journal.log_outcome(&id, &filled_ack(10.0, 0.55)).unwrap();
        }
        let rows = export_fill_ledger(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].client_order_id, id);
        assert_eq!(rows[0].venue, VenueId::Kalshi);
        assert_eq!(rows[0].side, Side::Buy);
        assert_eq!(rows[0].qty, 10.0);
        assert_eq!(rows[0].price, 0.55);
        assert!((rows[0].notional - 5.5).abs() < 1e-9);
    }

    #[test]
    fn export_fill_ledger_skips_a_fill_with_no_matching_intent() {
        let path = temp_path("ledger_orphan.jsonl");
        std::fs::remove_file(&path).ok();
        let orphan_id = ClientOrderId("plx-orphan".into());
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal
                .log_outcome(&orphan_id, &filled_ack(5.0, 0.5))
                .unwrap();
        }
        let rows = export_fill_ledger(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(rows.is_empty());
    }

    #[test]
    fn export_fill_ledger_skips_non_fill_outcomes() {
        let path = temp_path("ledger_non_fill.jsonl");
        std::fs::remove_file(&path).ok();
        let i = intent(10.0);
        let id = ClientOrderId::derive(&i);
        let rejected_ack = OrderAck {
            order_id: OrderId("k-2".into()),
            venue: VenueId::Kalshi,
            status: AckStatus::Rejected {
                reason: "no crossing liquidity".into(),
            },
            ts: Timestamp::from_nanos(0),
        };
        {
            let mut journal = OrderJournal::open(&path).unwrap();
            journal.log_intent(&id, &i).unwrap();
            journal.log_outcome(&id, &rejected_ack).unwrap();
        }
        let rows = export_fill_ledger(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!(rows.is_empty());
    }

    #[test]
    fn write_ledger_csv_quotes_a_field_containing_a_comma() {
        let rows = vec![LedgerRow {
            logged_at: Timestamp::from_nanos(0),
            client_order_id: ClientOrderId("plx-1".into()),
            venue: VenueId::Kalshi,
            contract: "a, contract, with, commas".into(),
            side: Side::Buy,
            qty: 1.0,
            price: 0.5,
            notional: 0.5,
        }];
        let mut out = Vec::new();
        write_ledger_csv(&rows, &mut out).unwrap();
        let csv = String::from_utf8(out).unwrap();
        assert!(csv.contains("\"a, contract, with, commas\""));
    }

    #[test]
    fn write_ledger_csv_starts_with_a_header_row() {
        let mut out = Vec::new();
        write_ledger_csv(&[], &mut out).unwrap();
        let csv = String::from_utf8(out).unwrap();
        assert!(
            csv.starts_with("logged_at_ns,client_order_id,venue,contract,side,qty,price,notional")
        );
    }
}
