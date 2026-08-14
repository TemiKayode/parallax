use parallax_book::CrossVenueArb;
use parallax_sim::BacktestReport;
use serde::Serialize;

#[derive(Serialize)]
pub struct ArbResponse {
    pub found: bool,
    pub buy_venue: Option<String>,
    pub buy_price: Option<f64>,
    pub sell_venue: Option<String>,
    pub sell_price: Option<f64>,
    pub edge: Option<f64>,
    /// The most this arb can actually be taken for right now — the
    /// smaller of the two venues' top-of-book size, not just a
    /// theoretical per-share edge.
    pub executable_size: Option<f64>,
}

impl From<Option<CrossVenueArb>> for ArbResponse {
    fn from(arb: Option<CrossVenueArb>) -> Self {
        match arb {
            None => ArbResponse {
                found: false,
                buy_venue: None,
                buy_price: None,
                sell_venue: None,
                sell_price: None,
                edge: None,
                executable_size: None,
            },
            Some(arb) => ArbResponse {
                found: true,
                buy_venue: Some(arb.buy_venue.to_string()),
                buy_price: Some(arb.buy_price),
                sell_venue: Some(arb.sell_venue.to_string()),
                sell_price: Some(arb.sell_price),
                edge: Some(arb.edge),
                executable_size: Some(arb.executable_size),
            },
        }
    }
}

#[derive(Serialize)]
pub struct PositionDto {
    pub venue: String,
    pub contract: String,
    pub qty: f64,
    pub avg_price: f64,
}

#[derive(Serialize)]
pub struct RejectionCountDto {
    pub reason: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct BacktestResponse {
    pub ticks_processed: u64,
    pub alpha_events_processed: u64,
    pub orders_proposed: u64,
    pub orders_rejected_by_risk: u64,
    pub orders_failed_submission: u64,
    pub fills: u64,
    pub filled_volume: f64,
    /// Sum of every fill's notional size — how much money's worth of
    /// exposure actually changed hands, not just how many contracts.
    pub gross_notional_traded: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub gross_pnl: f64,
    pub fees_paid: f64,
    /// `gross_pnl - fees_paid` — the number that actually matters.
    pub net_pnl: f64,
    pub max_drawdown: f64,
    /// Set once a Critical bus topic (order acks) has dropped an item
    /// during this run — every PnL number above is then not to be
    /// trusted, and the UI should say so rather than quote them anyway.
    pub bus_integrity_violated: bool,
    /// Why each rejected order was rejected, aggregated by reason — the
    /// fastest diagnostic for "why did the strategy stop trading."
    pub rejection_histogram: Vec<RejectionCountDto>,
    pub open_positions: Vec<PositionDto>,
}

impl From<BacktestReport> for BacktestResponse {
    fn from(r: BacktestReport) -> Self {
        BacktestResponse {
            ticks_processed: r.ticks_processed,
            alpha_events_processed: r.alpha_events_processed,
            orders_proposed: r.orders_proposed,
            orders_rejected_by_risk: r.orders_rejected_by_risk,
            orders_failed_submission: r.orders_failed_submission,
            fills: r.fills,
            filled_volume: r.filled_volume,
            gross_notional_traded: r.gross_notional_traded,
            realized_pnl: r.realized_pnl,
            unrealized_pnl: r.unrealized_pnl,
            gross_pnl: r.gross_pnl(),
            fees_paid: r.fees_paid,
            net_pnl: r.net_pnl(),
            max_drawdown: r.max_drawdown,
            bus_integrity_violated: r.bus_integrity_violated,
            rejection_histogram: r
                .rejection_histogram
                .iter()
                .map(|rc| RejectionCountDto {
                    reason: rc.reason.clone(),
                    count: rc.count,
                })
                .collect(),
            open_positions: r
                .open_positions
                .into_iter()
                .map(|(venue, contract, qty, avg_price)| PositionDto {
                    venue: venue.to_string(),
                    contract: contract.0,
                    qty,
                    avg_price,
                })
                .collect(),
        }
    }
}
