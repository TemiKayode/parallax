(() => {
  const root = document.documentElement;
  const THEME_KEY = "parallax-theme";
  const saved = localStorage.getItem(THEME_KEY);
  if (saved === "light") root.classList.add("light");

  document.getElementById("theme-toggle").addEventListener("click", () => {
    root.classList.toggle("light");
    localStorage.setItem(THEME_KEY, root.classList.contains("light") ? "light" : "dark");
  });

  const fmt = (n, digits = 2) => Number(n).toFixed(digits);

  const liveCards = {
    kalshi: document.getElementById("live-kalshi"),
    polymarket: document.getElementById("live-polymarket"),
  };

  async function loadLiveQuote(venue) {
    const card = liveCards[venue];
    const nameLabel = card.dataset.venue === "kalshi" ? "Kalshi" : "Polymarket";
    card.innerHTML = `<div class="live-card-head"><span class="venue-name">${nameLabel}</span></div><p class="hint">Fetching live quote…</p>`;
    try {
      const res = await fetch(`/api/live/${venue}`);
      // Error responses come back as a plain-text body (the server's
      // `(StatusCode, String)` error type), not JSON — calling
      // `res.json()` on those unconditionally throws a confusing
      // "Unexpected token" parse error instead of showing the real
      // server-side message. Branch on `res.ok` first and read the
      // matching body type for each case.
      if (!res.ok) {
        const message = await res.text();
        throw new Error(message || `server returned ${res.status}`);
      }
      const body = await res.json();
      renderLiveQuote(card, nameLabel, body);
    } catch (err) {
      card.innerHTML = `
        <div class="live-card-head"><span class="venue-name">${nameLabel}</span></div>
        <p class="live-error">Live fetch failed: ${escapeHtml(String(err.message || err))}</p>
      `;
    }
  }

  function renderLiveQuote(card, nameLabel, q) {
    const fetchedAt = new Date().toLocaleTimeString();
    card.innerHTML = `
      <div class="live-card-head">
        <span class="venue-name">${nameLabel}</span>
        <span class="live-fetched">fetched ${fetchedAt}</span>
      </div>
      <p class="live-label">${escapeHtml(q.label)}</p>
      <div class="live-quote-row">
        <div class="q">
          <span class="q-label">Bid</span>
          <span class="q-value">${fmt(q.bid)}</span>
          <span class="q-size">size ${fmt(q.bid_size)}</span>
        </div>
        <div class="q">
          <span class="q-label">Ask</span>
          <span class="q-value">${fmt(q.ask)}</span>
          <span class="q-size">size ${fmt(q.ask_size)}</span>
        </div>
      </div>
      <p class="live-detail">${escapeHtml(q.detail)}</p>
    `;
  }

  function loadAllLiveQuotes() {
    loadLiveQuote("kalshi");
    loadLiveQuote("polymarket");
  }

  document.getElementById("refresh-live").addEventListener("click", loadAllLiveQuotes);
  loadAllLiveQuotes();

  const arbForm = document.getElementById("arb-form");
  const arbResult = document.getElementById("arb-result");

  arbForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const data = Object.fromEntries(new FormData(arbForm).entries());
    const body = {
      polymarket_bid: Number(data.polymarket_bid),
      polymarket_ask: Number(data.polymarket_ask),
      kalshi_bid: Number(data.kalshi_bid),
      kalshi_ask: Number(data.kalshi_ask),
    };

    arbResult.innerHTML = '<p class="hint">Checking…</p>';
    try {
      const res = await fetch("/api/arb", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) throw new Error(`server returned ${res.status}`);
      const arb = await res.json();
      renderArb(arb);
    } catch (err) {
      arbResult.innerHTML = `<p class="hint">Request failed: ${escapeHtml(String(err))}</p>`;
    }
  });

  function renderArb(arb) {
    if (!arb.found) {
      arbResult.className = "result-box arb-miss";
      arbResult.innerHTML = '<p class="headline">No riskless arb — books are internally consistent.</p>';
      return;
    }
    arbResult.className = "result-box arb-hit";
    arbResult.innerHTML = `
      <p class="headline">Arb found: buy ${escapeHtml(arb.buy_venue)}, sell ${escapeHtml(arb.sell_venue)}</p>
      <div class="detail-row"><span>Buy price</span><b>${fmt(arb.buy_price)}</b></div>
      <div class="detail-row"><span>Sell price</span><b>${fmt(arb.sell_price)}</b></div>
      <div class="detail-row"><span>Edge per contract</span><b>${fmt(arb.edge)}</b></div>
      <div class="detail-row"><span>Executable size</span><b>${fmt(arb.executable_size, 0)}</b></div>
    `;
  }

  const runBacktestBtn = document.getElementById("run-backtest");
  const backtestResult = document.getElementById("backtest-result");

  runBacktestBtn.addEventListener("click", async () => {
    runBacktestBtn.disabled = true;
    backtestResult.innerHTML = '<p class="hint">Running the pipeline against the scenario…</p>';
    try {
      const res = await fetch("/api/backtest", { method: "POST" });
      if (!res.ok) throw new Error(`server returned ${res.status}`);
      const report = await res.json();
      renderBacktest(report);
    } catch (err) {
      backtestResult.innerHTML = `<p class="hint">Request failed: ${escapeHtml(String(err))}</p>`;
    } finally {
      runBacktestBtn.disabled = false;
    }
  });

  function renderBacktest(r) {
    if (r.bus_integrity_violated) {
      // Mirrors BacktestReport::headline() on the Rust side: once a
      // Critical bus topic has dropped an order ack, the position book
      // is known-wrong and no PnL number below can be trusted, so don't
      // show one dressed up as a real result.
      backtestResult.className = "result-box arb-miss";
      backtestResult.innerHTML = `
        <p class="live-error">
          INTEGRITY VIOLATED: a critical bus topic dropped an order ack during this run —
          the position book, and therefore every P&amp;L number, cannot be trusted this time.
          Try running it again.
        </p>`;
      return;
    }

    const netClass = r.net_pnl >= 0 ? "good" : "bad";
    const tiles = [
      ["ticks", r.ticks_processed, ""],
      ["alpha events", r.alpha_events_processed, ""],
      ["proposed", r.orders_proposed, ""],
      ["risk-rejected", r.orders_rejected_by_risk, ""],
      ["fills", r.fills, "good"],
      ["filled volume", fmt(r.filled_volume), ""],
      ["gross notional", fmt(r.gross_notional_traded), ""],
      ["realized pnl", fmt(r.realized_pnl), r.realized_pnl >= 0 ? "good" : "bad"],
      ["unrealized pnl", fmt(r.unrealized_pnl), r.unrealized_pnl >= 0 ? "good" : "bad"],
      ["fees paid", fmt(r.fees_paid), r.fees_paid > 0 ? "bad" : ""],
      ["net pnl", fmt(r.net_pnl), netClass],
      ["max drawdown", fmt(r.max_drawdown), r.max_drawdown > 0 ? "bad" : ""],
    ];

    const tileHtml = tiles
      .map(
        ([label, value, cls]) => `
      <div class="stat-tile">
        <div class="n ${cls}">${value}</div>
        <div class="l">${escapeHtml(label)}</div>
      </div>`
      )
      .join("");

    const rejectionHtml = r.rejection_histogram.length
      ? `<div class="rejection-list">
          <p class="subhead">Why orders were rejected by the risk gate</p>
          ${r.rejection_histogram
            .map(
              (rc) => `
            <div class="detail-row"><span>${escapeHtml(rc.reason)}</span><b>${rc.count}</b></div>`
            )
            .join("")}
        </div>`
      : "";

    const rows = r.open_positions
      .map(
        (p) => `
      <tr>
        <td>${escapeHtml(p.venue)}</td>
        <td>${escapeHtml(p.contract)}</td>
        <td>${fmt(p.qty)}</td>
        <td>${fmt(p.avg_price, 4)}</td>
      </tr>`
      )
      .join("");

    const positionsHtml = r.open_positions.length
      ? `<table class="positions">
          <thead><tr><th>Venue</th><th>Contract</th><th>Qty</th><th>Avg price</th></tr></thead>
          <tbody>${rows}</tbody>
        </table>`
      : '<p class="hint">No open positions.</p>';

    backtestResult.className = "result-box";
    backtestResult.innerHTML = `<div class="stat-grid">${tileHtml}</div>${rejectionHtml}${positionsHtml}`;
  }

  function escapeHtml(str) {
    return String(str).replace(/[&<>"']/g, (c) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    }[c]));
  }
})();
