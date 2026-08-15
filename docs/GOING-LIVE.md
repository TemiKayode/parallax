# Going live: a readiness plan

Engineering notes on what has to exist, and in what order, before an automated
prediction-market strategy touches funded accounts.

This is engineering guidance, not legal, compliance, tax or investment advice.
Venue terms of service, geofencing, eligibility and market-conduct rules are
real constraints that need review by someone qualified for your jurisdiction and
account type. Nothing here substitutes for that.

---

## The item that outranks the whole list

Every checklist of this kind — including the one in the README — is a list of
*engineering* gaps. They are all real and all tractable. None of them is the
thing most likely to lose money.

**The thing most likely to lose money is going live with an edge that was never
demonstrated.** Connectivity is a solved problem you can grind through in a
fortnight. A strategy that is quietly negative after fees will take your capital
smoothly, correctly, and with every log line green.

Two concrete reasons to think the edge is not yet demonstrated here:

1. **The backtest reports `fees paid = 0.00` on a scenario that crosses the
   spread twice.** On Polymarket crypto markets the taker fee is
   `0.07 × p × (1−p)` — 1.75¢ per share at the money. A backtest that charges
   nothing is not measuring the strategy, it is measuring an idealisation. On
   thin edges the fee *is* the result.
2. **The reference implementation of this design is net negative in
   simulation** — median −$232 across 40 seeds, 3/40 profitable, with fees
   modelled exactly. That is not proof your version is negative. It is proof
   that this class of strategy is not obviously positive, and that the burden of
   evidence sits with the claim that it is.

So the ordering below puts proving the edge first, and treats every engineering
stage as gated behind it. Building connectivity first is the common path and it
is backwards: it converts an open question into a funded position.

---

## Stage 0 — Prove the edge offline

**Goal:** positive expectancy on data you did not fit to, with real costs.

- Replay against **recorded venue book data**, not a synthetic venue. Every
  parameter in a simulator config is a guess until it has been checked against
  real flow. Record the websocket feed now, in parallel with everything else —
  it costs nothing and you cannot backfill it later.
- Charge the **exact** fee formula per fill, per category, and print fees as a
  separate line in every report. If a result does not survive its own fee line
  being displayed next to it, it was never a result.
- Model **latency** between decision and execution, and model **queue position**
  on passive orders. A backtest where your quotes fill without competition is
  a fiction generator.
- Report the **distribution**, not a number: run many seeds and many time
  periods, and judge on the 10th percentile. The median is what you tell people;
  the p10 is what you live through.
- **Walk-forward** any parameter you tune. Fitting `safety_k` or spread width to
  one dataset and reporting the fitted result is the fastest route to a
  confident loss.

**Gate:** out-of-sample p10 is positive after fees, across periods that include
at least one volatility shock and one quiet week. Write the number down before
you look at it.

---

## Stage 1 — Correctness of the money-touching primitives

Only after Stage 0. These are the parts where a bug costs money directly rather
than through the strategy.

### Idempotency — the one that bites first

Not on most checklists, and it is the classic way to lose money on day one:

> You send an order. The connection times out. You do not know whether it
> arrived. You retry. It had arrived. You are now twice the size you intended,
> on a position you sized with Kelly.

Every order must carry a **client-generated order ID**, and the retry path must
be idempotent on that ID. Before any retry, query order state by client ID; only
resend if the venue has no record. A timeout is *not* a rejection — treat
unknown as "possibly filled" until proven otherwise, and never let the unknown
state authorise a second order.

### Order lifecycle that survives a restart

`in_flight` tracking in memory is not enough. Persist intent **before** sending
and outcome **after**, so a crash between the two is recoverable. On restart the
question "did I have an order out?" must be answerable from disk, not inferred.

### Reconciliation that actually reconciles

The README is right that readiness is not reconciliation. The rule that makes it
tractable:

> **The venue is always right.** Never trade until local state and venue state
> agree. On disagreement, adopt the venue's view, log the delta loudly, and stay
> flat until a human has looked at it.

Fetch positions *and* working orders at startup, then on a schedule, then after
every disconnect. A silent divergence between what you think you hold and what
you hold is how a hedged book becomes a naked one.

### Signing, tested where it is safe to be wrong

RSA-PSS for Kalshi, EIP-712 for Polymarket, verified against each venue's
sandbox or demo environment before production credentials exist anywhere on the
machine. Signing bugs fail loudly and cheaply in sandbox; in production they
fail as rejected orders during the one minute you needed to flatten.

Clock discipline belongs here: signed requests carry timestamps, and skew shows
up as authentication failures at the worst possible moment. NTP, monitored, with
an alert on drift.

---

## Stage 2 — Rails that work when your software does not

The kill switch you write in Rust protects you from a strategy that is wrong. It
does nothing for a process that has crashed, a host that has lost power, or a
network that has partitioned while you hold live quotes.

- **Venue-side dead-man switch.** Polymarket's CLOB has a heartbeat endpoint:
  stop sending heartbeats and it cancels all your open orders automatically.
  This is the single highest-value safety control available, because it is
  enforced by the venue and requires nothing of you at the moment you most need
  it. Wire it before your first live order, not after.
- **An out-of-band cancel path.** A second, tiny, separately-deployed process
  (or a documented one-line script) that can `cancel-all` without importing any
  strategy code. When the main system is the problem, you cannot use the main
  system to fix it.
- **Reserve the cancel budget.** Both venues meter requests. Running out of
  cancel capacity while holding live quotes during a fault is the worst
  reachable state, and it is preventable by arithmetic — hold back enough tokens
  to cancel every resting order at all times.
- **Handle restricted trading modes.** Polymarket returns HTTP 425 during
  matching-engine restarts and then runs post-only for two minutes. Code that
  treats 425 as a generic error and retries aggressively will hammer a
  recovering engine and get rate-limited exactly when it wants back in.
- **Fund the account with what you can lose.** The most reliable position limit
  is the account balance. No software limit is as robust as capital that is not
  there.

---

## Stage 3 — Observability, before volume rather than after

- **Log every decision with the rule that fired.** Not just fills — the holds
  too, and why. "Rule 5 fired because the pair cost 96.2 cents" is a debuggable
  sentence at 3am; a fill with no recorded reason is an argument.
- **Alert on divergence, not on badness.** The useful alerts are:
  reconciliation mismatch; realised fee ≠ modelled fee; rejection rate above
  baseline; feed staleness; realised fill rate diverging from the model's
  assumption. Each of these means a *model* is wrong, which is the failure that
  compounds.
- **Continuous fee-model verification.** Do not verify the fee schedule once at
  launch. Assert on every fill that the fee the venue charged matches what your
  model predicted, and halt on a persistent mismatch. Venues change fees; a
  stale fee model turns a profitable strategy unprofitable with no error
  anywhere, which is precisely why it needs an assertion rather than a
  quarterly review.
- **Keep records for accounting.** Unglamorous, and much harder to reconstruct
  after the fact than to capture as you go.

---

## Stage 4 — Paper, then pennies, then scale

The stage most often skipped, and the cheapest one:

**Paper-trade against the live feed.** Full production code path — real
websockets, real auth, real rate limits, real reconnects, real message shapes —
with order submission replaced by a recorder. This catches an entire class of
bugs that no backtest can (schema drift, reconnect storms, auth expiry,
rate-limit behaviour under load) at zero financial risk. Run it for weeks, not
days, and compare its *predicted* fills against what the tape actually did.

Then, in order, with pre-committed numeric gates between each step:

1. **Sandbox** with venue test credentials — proves signing and lifecycle.
2. **Minimum viable real size.** Not 1% of target — the venue minimum. The goal
   is to discover what production does differently, and the cheapest tuition is
   the smallest position the venue will accept.
3. **Scale on evidence.** Increase only when realised P&L, realised fill rates
   and realised fees match what the model predicted. Divergence between
   predicted and realised is the signal; profit alone is not, because a small
   sample of a losing strategy is frequently profitable.

Write the stop condition down in advance and make it numeric: *if drawdown
exceeds X, or realised fills diverge from modelled by more than Y, stop and
review.* A limit decided after the loss is not a limit.

---

## Cross-cutting

**Secrets.** A secrets manager, least-privilege credentials, rotation, and
separate keys per environment. Production credentials should never exist on a
development machine. Where the venue supports scoped API keys, use trade-only
keys with withdrawal disabled — a compromised key that cannot move funds off the
venue is a much smaller incident.

**Redundancy.** One process in one region with no failover is fine for paper and
for small size. It is not fine once the position is large enough that being
unable to flatten for an hour matters. The dead-man switch in Stage 2 is what
makes single-process operation survivable in the interim, and it is a much
better first investment than hot-hot.

**Compliance.** Venue ToS, eligibility, geofencing and market-conduct rules,
reviewed for your jurisdiction and account type by someone qualified. Automated
trading is permitted on both venues under their terms, but the terms have
conditions, and they change.

---

## Honest summary

The engineering here is a few weeks of careful work by someone who has done it
before, and every item is well-understood. The gating question is not
engineering.

It is whether, measured against recorded data with real fees, real latency and
real queue position, this strategy has positive expectation at the tenth
percentile. Right now that question is open, and one of the two available
measurements — a backtest reporting zero fees on a spread-crossing trade — is
not yet trustworthy enough to answer it.

Fix the measurement first. It is cheaper than the alternative, and it is the
only step on this list whose absence cannot be detected from inside a running
system.
