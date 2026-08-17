use crate::source::AlphaSource;
use crate::stats::clamp_probability;
use parallax_types::{
    AlphaEventKind, CanonicalContractId, EstimateKind, ProbabilityEstimate, RawEvent,
    StalenessPolicy,
};
use serde::Deserialize;
use std::collections::HashMap;

/// Expected `RawEvent::payload` shape for one live tennis match-state
/// snapshot, mirroring the vendor's documented free-tier score object
/// (`GET /matches/{matchId}/score`: `sets`, per-set `games`, in-game
/// `points` as tennis strings, nullable `server`, `is_tiebreak`) plus the
/// match-level `format`, with the canonical contract id and which side
/// the contract's YES resolves on already resolved by ingestion — the
/// same division of labor as `WeatherPayload`: id resolution is an
/// ingestion-layer concern, not an alpha-model concern.
#[derive(Debug, Deserialize)]
struct TennisPayload {
    contract: String,
    /// Which sport this snapshot describes. `AlphaEventKind::
    /// SportsMatchState` is shared across sports by design, so this
    /// source insists on `"tennis"` and stays silent on anything else —
    /// a football clock routed here must produce nothing, not a tennis
    /// guess.
    sport: String,
    /// Which player (1 or 2) the contract's YES resolves on — the
    /// direction field of this domain, handled as explicitly as
    /// weather's `gt`/`lt`/`between`: any value other than 1 or 2 is a
    /// payload bug and yields silence, never a coin flip.
    yes_side: i64,
    /// Match lifecycle status when the ingestion layer knows it. Only a
    /// live match is priced: a completed or cancelled snapshot has
    /// nothing left to quote, and an upcoming one has no state to price
    /// from.
    status: Option<String>,
    /// `"BO3"` or `"BO5"`. Required: the same scoreline is a very
    /// different bet over three sets than over five, so a snapshot that
    /// doesn't say which is unpriceable, not defaultable.
    format: Option<String>,
    /// `[sets_p1, sets_p2]`.
    sets: Vec<i64>,
    /// `[games_p1_per_set, games_p2_per_set]` — the last entry of each
    /// is the set in progress.
    games: Vec<Vec<i64>>,
    /// `[points_p1, points_p2]` as tennis strings (`"0"`, `"15"`,
    /// `"30"`, `"40"`, `"AD"`). The vendor documents the entries as
    /// nullable; a null here is read as "no in-game information" and the
    /// current game is priced from its start — the one in-game prior
    /// that doesn't guess a specific score.
    #[serde(default)]
    points: Option<Vec<Option<String>>>,
    /// 1, 2, or null. Without it the game and set recursions have no
    /// orientation, and an unoriented guess is exactly what this crate
    /// refuses to emit.
    server: Option<i64>,
    #[serde(default)]
    is_tiebreak: bool,
}

/// In-game point score as an index: 0/15/30/40 → 0..=3, advantage → 4.
/// Returns `None` for any string outside the vendor's documented
/// vocabulary — an unrecognized token is a payload bug, not a score.
fn point_index(s: &str) -> Option<u8> {
    match s {
        "0" => Some(0),
        "15" => Some(1),
        "30" => Some(2),
        "40" => Some(3),
        "AD" => Some(4),
        _ => None,
    }
}

/// P(server wins the game) from in-game state `(s, r)` (server's and
/// receiver's `point_index`), with per-point serve-win probability `p`.
/// Deuce is the closed form `p² / (p² + (1-p)²)`; everything else is the
/// finite recursion toward it.
fn game_win_prob(p: f64, s: u8, r: u8) -> f64 {
    if s == 3 && r == 3 {
        let q = 1.0 - p;
        return (p * p) / (p * p + q * q);
    }
    if s == 4 {
        // Advantage server: win the point and the game is over, lose it
        // and we are back at deuce.
        return p + (1.0 - p) * game_win_prob(p, 3, 3);
    }
    if r == 4 {
        return p * game_win_prob(p, 3, 3);
    }
    let win = if s == 3 {
        1.0
    } else {
        game_win_prob(p, s + 1, r)
    };
    let lose = if r == 3 {
        0.0
    } else {
        game_win_prob(p, s, r + 1)
    };
    p * win + (1.0 - p) * lose
}

/// P(server wins the game) from the state *after* the next point, given
/// who took it — the two branches `game_win_prob` averages over, needed
/// separately because the emitted uncertainty is half the gap between
/// them (see `TennisMatchStateSource`).
fn game_win_prob_after_point(p: f64, s: u8, r: u8, server_won_point: bool) -> f64 {
    if server_won_point {
        match (s, r) {
            (4, _) => 1.0,
            (3, 3) => game_win_prob(p, 4, 3),
            (3, _) => 1.0,
            (_, 4) => game_win_prob(p, 3, 3),
            _ => game_win_prob(p, s + 1, r),
        }
    } else {
        match (s, r) {
            (_, 4) => 0.0,
            (3, 3) => game_win_prob(p, 3, 4),
            (_, 3) => 0.0,
            (4, _) => game_win_prob(p, 3, 3),
            _ => game_win_prob(p, s, r + 1),
        }
    }
}

/// P(p1 wins the set) from a game score with a fresh game about to
/// start, `p1_serves` it, and both players holding at `hold`. 6–6 is a
/// tiebreak, which under this symmetric model is exactly a coin flip.
fn set_win_prob(hold: f64, g1: i64, g2: i64, p1_serves: bool) -> f64 {
    if g1 >= 6 && g1 - g2 >= 2 {
        return 1.0;
    }
    if g2 >= 6 && g2 - g1 >= 2 {
        return 0.0;
    }
    if g1 >= 7 {
        return 1.0;
    }
    if g2 >= 7 {
        return 0.0;
    }
    if g1 == 6 && g2 == 6 {
        return 0.5;
    }
    let p1_game = if p1_serves { hold } else { 1.0 - hold };
    p1_game * set_win_prob(hold, g1 + 1, g2, !p1_serves)
        + (1.0 - p1_game) * set_win_prob(hold, g1, g2 + 1, !p1_serves)
}

/// P(p1 wins the match) from a set score, with every not-yet-started set
/// a coin flip — exact under the symmetric model once who serves a
/// future set's first game is unknown (averaging over the two
/// assignments cancels the first-server edge by symmetry).
fn match_win_prob(s1: i64, s2: i64, sets_to_win: i64) -> f64 {
    if s1 >= sets_to_win {
        return 1.0;
    }
    if s2 >= sets_to_win {
        return 0.0;
    }
    0.5 * match_win_prob(s1 + 1, s2, sets_to_win) + 0.5 * match_win_prob(s1, s2 + 1, sets_to_win)
}

/// True when the *receiver* is one point from taking the server's game:
/// receiver at advantage, or receiver at 40 while the server sits at
/// 0/15/30. Never during a tiebreak (there is no service game to break),
/// and `false` — not a guess — whenever `server` or either points entry
/// is null or outside the documented vocabulary. Deuce and a server
/// advantage are not break points.
pub fn is_break_point(
    server: Option<i64>,
    points: Option<&[Option<String>]>,
    is_tiebreak: bool,
) -> bool {
    if is_tiebreak {
        return false;
    }
    let server_idx = match server {
        Some(1) => 0,
        Some(2) => 1,
        _ => return false,
    };
    let points = match points {
        Some(p) if p.len() == 2 => p,
        _ => return false,
    };
    let (srv, rcv) = match (&points[server_idx], &points[1 - server_idx]) {
        (Some(s), Some(r)) => match (point_index(s), point_index(r)) {
            (Some(s), Some(r)) => (s, r),
            _ => return false,
        },
        _ => return false,
    };
    rcv == 4 || (rcv == 3 && srv <= 2)
}

/// Prices a live tennis match-winner contract from free-tier match state
/// alone — score, server, break-point state — via a symmetric
/// point→game→set→match chain with one parameter, P(server wins a
/// point). Both players get the same parameter: with no per-player skill
/// input a fresh match is honestly a coin flip, and everything the
/// estimate says comes from the observed state, which is the point — an
/// in-play fair value moves discontinuously on break points, and a
/// market maker holding pre-point quotes through one is picked off by
/// anyone watching the court.
///
/// The emitted uncertainty is half the gap between the fair values
/// conditional on the server winning vs losing the *next point* — the
/// delta method applied to the one genuinely unknowable input. It is
/// widest exactly on structurally decisive points (break points late in
/// a set), which is the correct direction: the band should widen where
/// one point moves the price most. During a tiebreak the within-tiebreak
/// point count is deliberately not parsed — the vendor documents the
/// `points` entries only as in-game tennis strings — so the set in
/// progress is priced as the coin flip the symmetric model says a
/// tiebreak is, with the band widened to span both set outcomes rather
/// than narrowed by a guess.
///
/// Vendor disclosure: this source is built against, and was contributed
/// by the operator of, the Live Tennis API (https://livetennisapi.com).
/// It consumes free-tier fields only — score, server, break-point state.
/// The paid model win-probability field is deliberately not consumed,
/// referenced, or mapped here. Plain quota facts, per the same standard
/// `parallax-cli/src/feed_health.rs` holds the rest of this repo to: the
/// free tier is 30 requests/minute and 100 requests/day — enough to
/// develop and test this source against live matches, NOT enough to run
/// continuous point-scale polling across a trading day, which needs a
/// paid tier. The API key that entitles whatever polls the feed lives in
/// deployment config or the environment, injected like every other
/// source's constants (`TennisConfig`), never in this repository.
///
/// Staleness follows the Stage-4 live-feed-health pattern
/// (`parallax-cli/src/feed_health.rs`): every estimate is
/// `StalenessPolicy::Decays`, never `Permanent`, so a feed that goes
/// quiet has its opinion age out and the aggregator's band widen — a
/// stale feed widens or pulls quotes, never holds them. The
/// fetch-outcome half of that pattern is `TennisFeedHealth` below:
/// consecutive-failure streaks, not single blips.
pub struct TennisMatchStateSource {
    name: String,
    kinds: [AlphaEventKind; 1],
    correlation_group: Option<String>,
    /// P(server wins any given point) — the chain's one parameter. A
    /// tour-average-shaped prior by default; operator-retuned via
    /// `TennisConfig`, not offline-fitted (the offline pipeline has no
    /// tennis data).
    serve_point_win: f64,
    /// Floor applied on top of the next-point-swing uncertainty —
    /// defense in depth, same role as the weather source's floor.
    min_std_dev: f64,
}

impl TennisMatchStateSource {
    pub fn new(name: impl Into<String>) -> Self {
        let defaults = crate::config::TennisConfig::default();
        TennisMatchStateSource {
            name: name.into(),
            kinds: [AlphaEventKind::SportsMatchState],
            correlation_group: None,
            serve_point_win: defaults.serve_point_win,
            min_std_dev: defaults.min_std_dev,
        }
    }

    /// Builds from operator-supplied config (see `TennisConfig` for why
    /// this is deployment config rather than the offline-fitted
    /// `AlphaConfig` artifact).
    pub fn from_config(name: impl Into<String>, config: &crate::config::TennisConfig) -> Self {
        TennisMatchStateSource {
            serve_point_win: config.serve_point_win,
            min_std_dev: config.min_std_dev,
            ..Self::new(name)
        }
    }

    /// Marks every estimate this source emits as correlated with every
    /// other estimate sharing the same group — e.g. two pollers reading
    /// the same vendor feed are one observation, not two.
    pub fn with_correlation_group(mut self, group: impl Into<String>) -> Self {
        self.correlation_group = Some(group.into());
        self
    }
}

/// The validated, oriented state one payload parses into. `None` at any
/// step of the parse means silence, per the trait contract.
struct MatchState {
    sets_p1: i64,
    sets_p2: i64,
    games_p1: i64,
    games_p2: i64,
    /// `(server_points, receiver_points)` as `point_index` values, or
    /// `None` when the payload carried null entries — price the game
    /// from its start.
    in_game: Option<(u8, u8)>,
    p1_serving: bool,
    tiebreak: bool,
    sets_to_win: i64,
}

fn parse_state(payload: &TennisPayload) -> Option<MatchState> {
    if let Some(status) = payload.status.as_deref() {
        if status != "live" {
            return None;
        }
    }
    let sets_to_win = match payload.format.as_deref() {
        Some("BO3") => 2,
        Some("BO5") => 3,
        // Explicitly unrecognized or absent: the bet's length is
        // unknown, so the state is unpriceable — silence, not a BO3
        // assumption.
        _ => return None,
    };
    let (&sets_p1, &sets_p2) = match payload.sets.as_slice() {
        [a, b] => (a, b),
        _ => return None,
    };
    if !(0..sets_to_win).contains(&sets_p1) || !(0..sets_to_win).contains(&sets_p2) {
        // Negative counts are malformed; a side already at the target is
        // a decided match wearing a live status — nothing left to quote
        // either way.
        return None;
    }
    let (games_p1, games_p2) = match payload.games.as_slice() {
        [p1, p2] if !p1.is_empty() && p1.len() == p2.len() => {
            (*p1.last().unwrap(), *p2.last().unwrap())
        }
        _ => return None,
    };
    if !(0..=7).contains(&games_p1) || !(0..=7).contains(&games_p2) {
        return None;
    }
    let p1_serving = match payload.server {
        Some(1) => true,
        Some(2) => false,
        _ => return None,
    };
    // 6–6 is a tiebreak whether or not the feed flagged it.
    let tiebreak = payload.is_tiebreak || (games_p1 == 6 && games_p2 == 6);
    let in_game = if tiebreak {
        None
    } else {
        Some(match payload.points.as_deref() {
            // A documented-nullable null entry: no in-game information;
            // price the game from its start.
            None | Some([None, None]) => (0, 0),
            Some([Some(a), Some(b)]) => {
                let (a, b) = (point_index(a)?, point_index(b)?);
                // Advantage only exists against 40, and both players
                // can't hold it at once.
                if (a == 4 && b != 3) || (b == 4 && a != 3) {
                    return None;
                }
                if p1_serving {
                    (a, b)
                } else {
                    (b, a)
                }
            }
            // A single-sided null or a wrong arity is malformed, not
            // "partially known."
            Some(_) => return None,
        })
    };
    Some(MatchState {
        sets_p1,
        sets_p2,
        games_p1,
        games_p2,
        in_game,
        p1_serving,
        tiebreak,
        sets_to_win,
    })
}

impl TennisMatchStateSource {
    /// P(p1 wins the match) and half the next-point swing, from a parsed
    /// state.
    fn price(&self, state: &MatchState) -> (f64, f64) {
        let p = self.serve_point_win;
        let hold = game_win_prob(p, 0, 0);
        let match_if_set_won = match_win_prob(state.sets_p1 + 1, state.sets_p2, state.sets_to_win);
        let match_if_set_lost = match_win_prob(state.sets_p1, state.sets_p2 + 1, state.sets_to_win);
        let match_given_set =
            |p1_set: f64| p1_set * match_if_set_won + (1.0 - p1_set) * match_if_set_lost;

        if state.tiebreak {
            // The set hangs on the tiebreak, priced as the coin flip the
            // symmetric model says it is; the band spans both set
            // outcomes rather than pretending to know the tiebreak score.
            return (
                match_given_set(0.5),
                (match_if_set_won - match_if_set_lost).abs() / 2.0,
            );
        }

        let set_if_game_won =
            set_win_prob(hold, state.games_p1 + 1, state.games_p2, !state.p1_serving);
        let set_if_game_lost =
            set_win_prob(hold, state.games_p1, state.games_p2 + 1, !state.p1_serving);
        let set_given_game =
            |p1_game: f64| p1_game * set_if_game_won + (1.0 - p1_game) * set_if_game_lost;
        let orient = |server_game: f64| {
            if state.p1_serving {
                server_game
            } else {
                1.0 - server_game
            }
        };

        let (s_pts, r_pts) = state.in_game.unwrap_or((0, 0));
        let fair = match_given_set(set_given_game(orient(game_win_prob(p, s_pts, r_pts))));
        let if_server_takes_point = match_given_set(set_given_game(orient(
            game_win_prob_after_point(p, s_pts, r_pts, true),
        )));
        let if_receiver_takes_point = match_given_set(set_given_game(orient(
            game_win_prob_after_point(p, s_pts, r_pts, false),
        )));
        (
            fair,
            (if_server_takes_point - if_receiver_takes_point).abs() / 2.0,
        )
    }
}

impl AlphaSource for TennisMatchStateSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn event_kinds(&self) -> &[AlphaEventKind] {
        &self.kinds
    }

    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate> {
        if event.kind != AlphaEventKind::SportsMatchState {
            return None;
        }
        let payload: TennisPayload = serde_json::from_value(event.payload.clone()).ok()?;
        if payload.sport != "tennis" {
            return None;
        }
        let state = parse_state(&payload)?;
        let (p1_prob, half_swing) = self.price(&state);
        // Direction: which side the contract's YES resolves on, handled
        // explicitly — an unrecognized side is silence, never a guess at
        // an orientation.
        let probability = match payload.yes_side {
            1 => p1_prob,
            2 => 1.0 - p1_prob,
            _ => return None,
        };

        Some(ProbabilityEstimate {
            source: self.name.clone(),
            contract: CanonicalContractId(payload.contract),
            probability: clamp_probability(probability),
            std_dev: half_swing.max(self.min_std_dev),
            as_of: event.receive_ts,
            kind: EstimateKind::Absolute,
            staleness: StalenessPolicy::Decays,
            correlation_group: self.correlation_group.clone(),
        })
    }
}

/// One match's feed crossing from "probably fine" to "something is
/// actually wrong" — `consecutive_failures` has reached the configured
/// threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TennisFeedAlert {
    pub match_id: i64,
    pub consecutive_failures: u32,
}

/// The fetch-outcome half of the Stage-4 feed-health pattern
/// (`parallax-cli/src/feed_health.rs`'s `FeedHealthMonitor`, keyed per
/// match instead of per venue): a consecutive-failure streak, because a
/// fetch that fails once in a while is normal internet and a fetch that
/// fails many times in a row is a real problem. Whatever polls the
/// vendor feed records each attempt here; on an alert the correct
/// response for every contract priced off that match is to widen or pull
/// quotes, never to hold them — the estimate side already agrees, since
/// everything this source emits decays rather than persisting. Alerts
/// repeat on every failure past the threshold: an operator should keep
/// hearing about an ongoing outage, not just its first moment.
pub struct TennisFeedHealth {
    halt_after: u32,
    streaks: HashMap<i64, u32>,
}

impl TennisFeedHealth {
    pub fn new(halt_after: u32) -> Self {
        TennisFeedHealth {
            halt_after: halt_after.max(1),
            streaks: HashMap::new(),
        }
    }

    /// Records one fetch attempt's outcome for `match_id`. A success
    /// resets that match's streak to zero — this tracks "how bad is it
    /// *right now*," not a lifetime failure count.
    pub fn record(&mut self, match_id: i64, success: bool) -> Option<TennisFeedAlert> {
        let streak = self.streaks.entry(match_id).or_insert(0);
        if success {
            *streak = 0;
            return None;
        }
        *streak += 1;
        if *streak >= self.halt_after {
            Some(TennisFeedAlert {
                match_id,
                consecutive_failures: *streak,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parallax_types::Timestamp;
    use serde_json::json;

    fn event(payload: serde_json::Value) -> RawEvent {
        RawEvent {
            source: "live-tennis".into(),
            kind: AlphaEventKind::SportsMatchState,
            publish_ts: None,
            receive_ts: Timestamp::from_nanos(0),
            payload,
        }
    }

    fn snapshot(
        sets: (i64, i64),
        games: (i64, i64),
        points: (&str, &str),
        server: i64,
    ) -> serde_json::Value {
        json!({
            "contract": "tennis.match.p1_wins.2026-08-17.vendor_official",
            "sport": "tennis",
            "yes_side": 1,
            "status": "live",
            "format": "BO3",
            "sets": [sets.0, sets.1],
            "games": [[games.0], [games.1]],
            "points": [points.0, points.1],
            "server": server,
            "is_tiebreak": false,
        })
    }

    #[test]
    fn a_fresh_match_is_near_a_coin_flip() {
        let src = TennisMatchStateSource::new("live-tennis");
        let est = src
            .on_event(&event(snapshot((0, 0), (0, 0), ("0", "0"), 1)))
            .unwrap();
        // Symmetric players: the only information is who serves first.
        assert!(
            est.probability > 0.45 && est.probability < 0.60,
            "probability was {}",
            est.probability
        );
    }

    #[test]
    fn a_set_and_a_break_up_is_priced_heavily_but_not_certain() {
        let src = TennisMatchStateSource::new("live-tennis");
        let payload = json!({
            "contract": "tennis.match.p1_wins.2026-08-17.vendor_official",
            "sport": "tennis",
            "yes_side": 1,
            "status": "live",
            "format": "BO3",
            "sets": [1, 0],
            "games": [[6, 4], [3, 1]],
            "points": ["0", "0"],
            "server": 1,
            "is_tiebreak": false,
        });
        let est = src.on_event(&event(payload)).unwrap();
        assert!(est.probability > 0.8, "probability was {}", est.probability);
        assert!(est.probability < 1.0);
    }

    #[test]
    fn the_yes_side_orients_the_estimate_explicitly() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut on_p1 = snapshot((1, 0), (3, 1), ("30", "0"), 1);
        let mut on_p2 = on_p1.clone();
        on_p1["yes_side"] = json!(1);
        on_p2["yes_side"] = json!(2);
        let est_p1 = src.on_event(&event(on_p1)).unwrap();
        let est_p2 = src.on_event(&event(on_p2)).unwrap();
        assert!(
            (est_p1.probability + est_p2.probability - 1.0).abs() < 1e-9,
            "sides must price to complements, got {} and {}",
            est_p1.probability,
            est_p2.probability
        );
    }

    #[test]
    fn an_unrecognized_yes_side_yields_silence_not_a_guess() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (2, 2), ("15", "15"), 1);
        payload["yes_side"] = json!(3);
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn irrelevant_event_kind_yields_no_opinion() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut ev = event(snapshot((0, 0), (0, 0), ("0", "0"), 1));
        ev.kind = AlphaEventKind::NewsHeadline;
        assert!(src.on_event(&ev).is_none());
    }

    #[test]
    fn a_different_sport_on_the_shared_kind_yields_silence() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (0, 0), ("0", "0"), 1);
        payload["sport"] = json!("football");
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn an_unrecognized_point_string_is_malformed_not_a_score() {
        let src = TennisMatchStateSource::new("live-tennis");
        let payload = snapshot((0, 0), (2, 2), ("37", "15"), 1);
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn a_null_server_yields_silence_not_an_unoriented_guess() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (2, 2), ("15", "15"), 1);
        payload["server"] = json!(null);
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn null_points_price_the_game_from_its_start() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut nulls = snapshot((0, 0), (3, 2), ("0", "0"), 2);
        nulls["points"] = json!([null, null]);
        let explicit = snapshot((0, 0), (3, 2), ("0", "0"), 2);
        let from_nulls = src.on_event(&event(nulls)).unwrap();
        let from_love_all = src.on_event(&event(explicit)).unwrap();
        assert!(
            (from_nulls.probability - from_love_all.probability).abs() < 1e-9,
            "a null in-game score must price exactly like a game at its start"
        );
    }

    #[test]
    fn a_single_sided_null_point_entry_is_malformed() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (3, 2), ("0", "0"), 1);
        payload["points"] = json!(["30", null]);
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn a_missing_format_is_unpriceable_not_a_bo3_assumption() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (0, 0), ("0", "0"), 1);
        payload.as_object_mut().unwrap().remove("format");
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn a_non_live_status_yields_silence() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (0, 0), ("0", "0"), 1);
        payload["status"] = json!("completed");
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn an_already_decided_scoreline_yields_silence() {
        let src = TennisMatchStateSource::new("live-tennis");
        let payload = snapshot((2, 0), (0, 0), ("0", "0"), 1);
        assert!(src.on_event(&event(payload)).is_none());
    }

    #[test]
    fn a_break_point_prices_lower_and_wider_than_a_game_point() {
        let src = TennisMatchStateSource::new("live-tennis");
        // p1 serving at 2-2 in the first set: down 0-40 (triple break
        // point) vs up 40-0 (triple game point).
        let facing_break = src
            .on_event(&event(snapshot((0, 0), (2, 2), ("0", "40"), 1)))
            .unwrap();
        let game_point = src
            .on_event(&event(snapshot((0, 0), (2, 2), ("40", "0"), 1)))
            .unwrap();
        assert!(
            facing_break.probability < game_point.probability,
            "facing break {} vs game point {}",
            facing_break.probability,
            game_point.probability
        );
        assert!(
            facing_break.std_dev > game_point.std_dev,
            "the band must be widest where one point moves the price most: {} vs {}",
            facing_break.std_dev,
            game_point.std_dev
        );
    }

    #[test]
    fn the_same_lead_is_worth_more_over_three_sets_than_five() {
        let src = TennisMatchStateSource::new("live-tennis");
        let bo3 = snapshot((1, 0), (0, 0), ("0", "0"), 1);
        let mut bo5 = bo3.clone();
        bo5["format"] = json!("BO5");
        bo5["sets"] = json!([1, 0]);
        let est_bo3 = src.on_event(&event(bo3)).unwrap();
        let est_bo5 = src.on_event(&event(bo5)).unwrap();
        assert!(
            est_bo3.probability > est_bo5.probability,
            "BO3 {} must exceed BO5 {}",
            est_bo3.probability,
            est_bo5.probability
        );
    }

    #[test]
    fn a_tiebreak_is_priced_wide_never_narrowed_by_a_guess() {
        let src = TennisMatchStateSource::new("live-tennis");
        let mut payload = snapshot((0, 0), (6, 6), ("0", "0"), 1);
        payload["is_tiebreak"] = json!(true);
        let est = src.on_event(&event(payload)).unwrap();
        assert!(
            (est.probability - 0.5).abs() < 1e-9,
            "probability was {}",
            est.probability
        );
        // First set hangs on the tiebreak: the band must span both set
        // outcomes (match_win_prob(1,0) vs (0,1) is 0.75 vs 0.25).
        assert!(est.std_dev >= 0.2, "std_dev was {}", est.std_dev);
    }

    #[test]
    fn six_all_is_a_tiebreak_even_if_the_flag_is_stale() {
        let src = TennisMatchStateSource::new("live-tennis");
        let flagged = {
            let mut p = snapshot((0, 0), (6, 6), ("0", "0"), 1);
            p["is_tiebreak"] = json!(true);
            p
        };
        let unflagged = snapshot((0, 0), (6, 6), ("0", "0"), 1);
        let a = src.on_event(&event(flagged)).unwrap();
        let b = src.on_event(&event(unflagged)).unwrap();
        assert_eq!(a.probability, b.probability);
        assert_eq!(a.std_dev, b.std_dev);
    }

    #[test]
    fn from_config_applies_the_operator_supplied_constants() {
        let config = crate::config::TennisConfig {
            serve_point_win: 0.62,
            min_std_dev: 0.3,
        };
        let src = TennisMatchStateSource::from_config("live-tennis", &config);
        // Deep in a set, one point barely moves the match price — the
        // configured floor must hold the band open anyway.
        let est = src
            .on_event(&event(snapshot((1, 0), (5, 0), ("40", "0"), 1)))
            .unwrap();
        assert!(est.std_dev >= 0.3, "std_dev was {}", est.std_dev);
    }

    #[test]
    fn correlation_group_is_attached_when_configured() {
        let src = TennisMatchStateSource::new("live-tennis").with_correlation_group("vendor-feed");
        let est = src
            .on_event(&event(snapshot((0, 0), (0, 0), ("0", "0"), 1)))
            .unwrap();
        assert_eq!(est.correlation_group.as_deref(), Some("vendor-feed"));
    }

    #[test]
    fn probability_is_never_reported_as_exactly_certain() {
        let src = TennisMatchStateSource::new("live-tennis");
        // Match point up a set at 5-0, 40-0: as close to certain as a
        // live snapshot gets, and still clamped inside (0, 1).
        let est = src
            .on_event(&event(snapshot((1, 0), (5, 0), ("40", "0"), 1)))
            .unwrap();
        assert!(est.probability < 1.0);
        assert!(est.probability > 0.9, "probability was {}", est.probability);
    }

    mod break_point {
        use super::super::is_break_point;

        fn pts(a: &str, b: &str) -> Vec<Option<String>> {
            vec![Some(a.into()), Some(b.into())]
        }

        #[test]
        fn receiver_at_advantage_is_a_break_point() {
            assert!(is_break_point(Some(1), Some(&pts("40", "AD")), false));
        }

        #[test]
        fn receiver_at_forty_against_a_thirty_serve_is_a_break_point() {
            assert!(is_break_point(Some(1), Some(&pts("30", "40")), false));
            assert!(is_break_point(Some(2), Some(&pts("40", "0")), false));
        }

        #[test]
        fn deuce_is_not_a_break_point() {
            assert!(!is_break_point(Some(1), Some(&pts("40", "40")), false));
        }

        #[test]
        fn a_server_advantage_or_game_point_is_not_a_break_point() {
            assert!(!is_break_point(Some(1), Some(&pts("AD", "40")), false));
            assert!(!is_break_point(Some(1), Some(&pts("40", "15")), false));
        }

        #[test]
        fn a_tiebreak_never_has_a_break_point() {
            assert!(!is_break_point(Some(1), Some(&pts("30", "40")), true));
        }

        #[test]
        fn null_server_or_points_is_false_not_a_guess() {
            assert!(!is_break_point(None, Some(&pts("30", "40")), false));
            assert!(!is_break_point(Some(1), None, false));
            assert!(!is_break_point(
                Some(1),
                Some(&[Some("30".into()), None]),
                false
            ));
        }

        #[test]
        fn an_unrecognized_token_is_false_not_a_guess() {
            assert!(!is_break_point(Some(1), Some(&pts("30", "45")), false));
        }
    }

    mod feed_health {
        use super::super::{TennisFeedAlert, TennisFeedHealth};

        #[test]
        fn a_single_failure_below_the_threshold_raises_no_alert() {
            let mut monitor = TennisFeedHealth::new(3);
            assert_eq!(monitor.record(101, false), None);
            assert_eq!(monitor.record(101, false), None);
        }

        #[test]
        fn reaching_the_threshold_raises_an_alert_with_the_streak_length() {
            let mut monitor = TennisFeedHealth::new(3);
            monitor.record(101, false);
            monitor.record(101, false);
            assert_eq!(
                monitor.record(101, false),
                Some(TennisFeedAlert {
                    match_id: 101,
                    consecutive_failures: 3
                })
            );
        }

        #[test]
        fn continuing_failure_keeps_alerting_past_the_threshold() {
            let mut monitor = TennisFeedHealth::new(2);
            monitor.record(101, false);
            monitor.record(101, false);
            assert_eq!(
                monitor.record(101, false),
                Some(TennisFeedAlert {
                    match_id: 101,
                    consecutive_failures: 3
                })
            );
        }

        #[test]
        fn a_success_resets_the_streak() {
            let mut monitor = TennisFeedHealth::new(2);
            monitor.record(101, false);
            assert_eq!(monitor.record(101, true), None);
            assert_eq!(monitor.record(101, false), None);
        }

        #[test]
        fn matches_are_tracked_independently() {
            let mut monitor = TennisFeedHealth::new(2);
            monitor.record(101, false);
            monitor.record(101, false);
            assert_eq!(monitor.record(202, false), None);
        }

        #[test]
        fn zero_configuration_is_clamped_to_at_least_one() {
            let mut monitor = TennisFeedHealth::new(0);
            assert!(monitor.record(101, false).is_some());
        }
    }
}
