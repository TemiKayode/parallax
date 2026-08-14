use parallax_types::Side;

/// Rounds a price to a venue's tick grid, always conservatively: a buy
/// rounds *down* (never pays more than the strategy computed) and a sell
/// rounds *up* (never receives less). Rounding to the nearest tick
/// instead would let a sell round down and silently accept a worse price
/// than intended (design doc review 3.13).
pub fn round_price(price: f64, tick: f64, side: Side) -> f64 {
    if !tick.is_finite() || tick <= 0.0 || !price.is_finite() {
        return price;
    }
    let ticks = price / tick;
    let rounded_ticks = match side {
        Side::Buy => ticks.floor(),
        Side::Sell => ticks.ceil(),
    };
    (rounded_ticks * tick).clamp(0.0, 1.0)
}

/// Rounds a contract count down to a whole number and rejects (`None`)
/// once that rounds below the venue's minimum order size. `size as i64`
/// truncates 9.7 to 9 silently and a NaN size to 0 — an order for
/// nothing, not a rejection (design doc review 3.12).
pub fn round_lot(size: f64, min_order_size: f64) -> Option<u64> {
    if !size.is_finite() || size <= 0.0 {
        return None;
    }
    let whole = size.floor();
    if whole < min_order_size {
        return None;
    }
    Some(whole as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buy_rounds_down_and_sell_rounds_up() {
        assert!((round_price(0.567, 0.01, Side::Buy) - 0.56).abs() < 1e-9);
        assert!((round_price(0.561, 0.01, Side::Sell) - 0.57).abs() < 1e-9);
    }

    #[test]
    fn rounding_never_pushes_a_price_outside_zero_one() {
        assert!(round_price(0.999, 0.01, Side::Sell) <= 1.0);
        assert!(round_price(0.001, 0.01, Side::Buy) >= 0.0);
    }

    #[test]
    fn a_nan_size_is_rejected_rather_than_truncating_to_zero() {
        assert_eq!(round_lot(f64::NAN, 1.0), None);
    }

    #[test]
    fn a_fractional_size_truncates_down_not_up() {
        assert_eq!(round_lot(9.7, 1.0), Some(9));
    }

    #[test]
    fn a_size_that_rounds_below_the_minimum_is_rejected() {
        assert_eq!(round_lot(4.9, 5.0), None);
    }
}
