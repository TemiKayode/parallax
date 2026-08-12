use parallax_types::{AlphaEventKind, ProbabilityEstimate, RawEvent};

/// One pluggable domain model. New alpha sources (a new weather variable,
/// a different econ series, a second NLP vendor) implement this trait and
/// register with the ingestion router — no change to the aggregator, the
/// strategy core, or anything downstream (design doc §17).
///
/// Implementations are expected to be stateless from the trait's point of
/// view; any internal state (a running ensemble, a decay buffer) lives
/// behind interior mutability inside the implementing type so the trait
/// object stays `Send + Sync` and callable from multiple ingestion
/// threads without an external lock.
pub trait AlphaSource: Send + Sync {
    fn name(&self) -> &str;

    /// Which `RawEvent` kinds this source wants routed to it. The router
    /// uses this to avoid handing every source every event.
    fn event_kinds(&self) -> &[AlphaEventKind];

    /// Attempt to turn one raw event into a probability estimate. Returns
    /// `None` when the event isn't relevant to this source's contract
    /// (e.g. a headline that doesn't mention this source's tracked
    /// entities) rather than a low-confidence guess — silence is the
    /// correct response to irrelevance, not noise.
    fn on_event(&self, event: &RawEvent) -> Option<ProbabilityEstimate>;
}
