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
            },
            Some(arb) => ArbResponse {
                found: true,
                buy_venue: Some(arb.buy_venue.to_string()),
                buy_price: Some(arb.buy_price),
                sell_venue: Some(arb.sell_venue.to_string()),
                sell_price: Some(arb.sell_price),
                edge: Some(arb.edge),
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
pub struct BacktestResponse {
    pub ticks_processed: u64,
    pub alpha_events_processed: u64,
    pub orders_proposed: u64,
    pub orders_rejected_by_risk: u64,
    pub orders_failed_submission: u64,
    pub fills: u64,
    pub filled_volume: f64,
    pub unrealized_pnl: f64,
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
            unrealized_pnl: r.unrealized_pnl,
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
