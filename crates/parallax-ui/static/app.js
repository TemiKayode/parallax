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
    const pnlClass = r.unrealized_pnl >= 0 ? "good" : "bad";
    const tiles = [
      ["ticks", r.ticks_processed, ""],
      ["alpha events", r.alpha_events_processed, ""],
      ["proposed", r.orders_proposed, ""],
      ["risk-rejected", r.orders_rejected_by_risk, ""],
      ["fills", r.fills, "good"],
      ["filled volume", fmt(r.filled_volume), ""],
      ["unrealized pnl", fmt(r.unrealized_pnl), pnlClass],
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
    backtestResult.innerHTML = `<div class="stat-grid">${tileHtml}</div>${positionsHtml}`;
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
