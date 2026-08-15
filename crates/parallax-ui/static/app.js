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

  function escapeHtml(str) {
    return String(str).replace(/[&<>"']/g, (c) => ({
      "&": "&amp;",
      "<": "&lt;",
      ">": "&gt;",
      '"': "&quot;",
      "'": "&#39;",
    }[c]));
  }

  // The page's CSP has no `style-src 'unsafe-inline'` (design doc review,
  // Phase 3 audit) — a `style="width:70%"` string baked into an innerHTML
  // template is inline style and the browser silently drops it. Setting
  // the same property through the CSSOM (`el.style.width = ...`) is not
  // "inline style" in CSP's sense and is unaffected, so every proportional
  // bar below is rendered with a `data-w` percentage and filled in here
  // as a pass over the DOM right after the markup lands.
  function applyBarWidths(scope) {
    scope.querySelectorAll("[data-w]").forEach((el) => {
      el.style.width = `${el.dataset.w}%`;
    });
  }

  // ---------------------------------------------------------------
  // Toast notifications — network failures, rate limits, staleness.
  // ---------------------------------------------------------------
  const toastStack = document.getElementById("toast-stack");

  function toast(type, title, message, durationMs = 6000) {
    const el = document.createElement("div");
    el.className = `toast ${type}`;
    el.innerHTML = `
      <p class="toast-title">${escapeHtml(title)}</p>
      <p class="toast-msg">${escapeHtml(message)}</p>
      <button class="toast-close" type="button" aria-label="Dismiss notification">×</button>
      <div class="toast-progress"></div>
    `;
    // CSSOM property assignment, not the `style` attribute — the page's
    // CSP has no `style-src 'unsafe-inline'`, so an inline `style="..."`
    // string here would be silently dropped by the browser.
    el.querySelector(".toast-progress").style.animationDuration = `${durationMs}ms`;
    const dismiss = () => el.remove();
    el.querySelector(".toast-close").addEventListener("click", dismiss);
    setTimeout(dismiss, durationMs);
    toastStack.appendChild(el);
  }

  // ---------------------------------------------------------------
  // Live venue quotes — real fetches, freshness/latency, book viz.
  // ---------------------------------------------------------------
  const liveCards = {
    kalshi: document.getElementById("live-kalshi"),
    polymarket: document.getElementById("live-polymarket"),
  };
  const venueLabel = (venue) => (venue === "kalshi" ? "Kalshi" : "Polymarket");
  const liveState = {
    kalshi: { fetchedAtMs: null, latencyMs: null, ok: null },
    polymarket: { fetchedAtMs: null, latencyMs: null, ok: null },
  };
  const staleToastShown = { kalshi: false, polymarket: false };

  function freshnessSpan(venue) {
    return `<span class="freshness" id="freshness-${venue}"></span>`;
  }

  function updateFreshnessDisplay(venue) {
    const el = document.getElementById(`freshness-${venue}`);
    if (!el) return;
    const s = liveState[venue];
    if (!s || s.fetchedAtMs == null) {
      el.innerHTML = '<span class="status-dot"></span>—';
      return;
    }
    const ageSec = Math.max(0, Math.round((Date.now() - s.fetchedAtMs) / 1000));
    let dotClass = "good";
    if (!s.ok) dotClass = "bad";
    else if (ageSec > 30) dotClass = "bad";
    else if (ageSec > 10) dotClass = "amber";
    const pulse = s.ok && ageSec < 3 ? " pulse" : "";
    const ageText = ageSec < 1 ? "just now" : `${ageSec}s ago`;
    const latencyText = s.latencyMs != null ? ` <span class="latency">· ${s.latencyMs}ms</span>` : "";
    el.innerHTML = `<span class="status-dot ${dotClass}${pulse}"></span>${ageText}${latencyText}`;

    if (s.ok && ageSec > 30 && !staleToastShown[venue]) {
      staleToastShown[venue] = true;
      toast(
        "warning",
        `${venueLabel(venue)} quote is stale`,
        "No refresh in over 30s — this book may no longer reflect the live market. Refresh to update."
      );
    }
  }

  function bookVizHtml(q) {
    const maxSize = Math.max(q.bid_size, q.ask_size, 1e-9);
    const bidPct = Math.max(4, Math.round((q.bid_size / maxSize) * 100));
    const askPct = Math.max(4, Math.round((q.ask_size / maxSize) * 100));
    const spread = q.ask - q.bid;
    const mid = (q.ask + q.bid) / 2;
    const spreadBps = mid > 0 ? Math.round((spread / mid) * 10000) : 0;
    return `
      <div class="book-viz">
        <div class="book-row bid">
          <span class="book-side-label">BID</span>
          <span class="book-bar-track"><span class="book-bar-fill" data-w="${bidPct}"></span></span>
          <span class="book-price">${fmt(q.bid)}</span>
          <span class="book-size">×${fmt(q.bid_size, 0)}</span>
        </div>
        <div class="book-row ask">
          <span class="book-side-label">ASK</span>
          <span class="book-bar-track"><span class="book-bar-fill" data-w="${askPct}"></span></span>
          <span class="book-price">${fmt(q.ask)}</span>
          <span class="book-size">×${fmt(q.ask_size, 0)}</span>
        </div>
        <div class="spread-readout"><span>Spread</span><b>${fmt(spread)} (${spreadBps} bps)</b></div>
      </div>
    `;
  }

  function renderLiveQuote(card, nameLabel, q, venue) {
    card.innerHTML = `
      <div class="live-card-head">
        <span class="venue-name">${nameLabel}</span>
        ${freshnessSpan(venue)}
      </div>
      <p class="live-label">${escapeHtml(q.label)}</p>
      ${bookVizHtml(q)}
      <p class="live-detail">${escapeHtml(q.detail)}</p>
    `;
    applyBarWidths(card);
    updateFreshnessDisplay(venue);
  }

  async function loadLiveQuote(venue) {
    const card = liveCards[venue];
    const nameLabel = venueLabel(venue);
    card.innerHTML = `
      <div class="live-card-head">
        <span class="venue-name">${nameLabel}</span>
        <span class="freshness"><span class="status-dot amber pulse"></span>fetching…</span>
      </div>
      <p class="hint">Fetching live quote…</p>
    `;
    const start = performance.now();
    try {
      const res = await fetch(`/api/live/${venue}`);
      const latency = Math.round(performance.now() - start);
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
      liveState[venue] = { fetchedAtMs: Date.now(), latencyMs: latency, ok: true };
      staleToastShown[venue] = false;
      renderLiveQuote(card, nameLabel, body, venue);
      card.classList.add("just-updated");
      setTimeout(() => card.classList.remove("just-updated"), 900);
    } catch (err) {
      const latency = Math.round(performance.now() - start);
      liveState[venue] = { fetchedAtMs: Date.now(), latencyMs: latency, ok: false };
      const message = String(err.message || err);
      card.innerHTML = `
        <div class="live-card-head">
          <span class="venue-name">${nameLabel}</span>
          ${freshnessSpan(venue)}
        </div>
        <p class="live-error">Live fetch failed: ${escapeHtml(message)}</p>
      `;
      updateFreshnessDisplay(venue);
      if (message.toLowerCase().includes("rate limited")) {
        toast("warning", `${nameLabel} rate limited`, message);
      } else {
        toast("error", `${nameLabel} live fetch failed`, message);
      }
    }
  }

  const AUTO_REFRESH_SECONDS = 30;
  let autoRefreshRemaining = AUTO_REFRESH_SECONDS;
  const autoRefreshToggle = document.getElementById("auto-refresh-toggle");
  const autoRefreshCountdownEl = document.getElementById("auto-refresh-countdown");

  function loadAllLiveQuotes() {
    loadLiveQuote("kalshi");
    loadLiveQuote("polymarket");
    autoRefreshRemaining = AUTO_REFRESH_SECONDS;
  }

  document.getElementById("refresh-live").addEventListener("click", loadAllLiveQuotes);
  loadAllLiveQuotes();

  setInterval(() => {
    updateFreshnessDisplay("kalshi");
    updateFreshnessDisplay("polymarket");
    if (autoRefreshToggle.checked) {
      autoRefreshRemaining -= 1;
      if (autoRefreshRemaining <= 0) {
        loadAllLiveQuotes();
      } else {
        autoRefreshCountdownEl.textContent = `${autoRefreshRemaining}s`;
      }
    } else {
      autoRefreshCountdownEl.textContent = "off";
    }
  }, 1000);

  // ---------------------------------------------------------------
  // Cross-venue arb detector — live-recompute + executable depth.
  // ---------------------------------------------------------------
  const arbForm = document.getElementById("arb-form");
  const arbResult = document.getElementById("arb-result");
  const livePreviewToggle = document.getElementById("live-preview-toggle");
  let arbDebounceTimer = null;

  function collectArbBody() {
    const data = Object.fromEntries(new FormData(arbForm).entries());
    return {
      polymarket_bid: Number(data.polymarket_bid),
      polymarket_ask: Number(data.polymarket_ask),
      polymarket_size: Number(data.polymarket_size),
      kalshi_bid: Number(data.kalshi_bid),
      kalshi_ask: Number(data.kalshi_ask),
      kalshi_size: Number(data.kalshi_size),
    };
  }

  async function submitArb(body) {
    arbResult.className = "result-box";
    arbResult.innerHTML = '<p class="hint">Checking…</p>';
    try {
      const res = await fetch("/api/arb", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      if (!res.ok) throw new Error(`server returned ${res.status}`);
      const arb = await res.json();
      renderArb(arb, body);
    } catch (err) {
      const message = String(err.message || err);
      arbResult.innerHTML = `<p class="hint">Request failed: ${escapeHtml(message)}</p>`;
      toast("error", "Arb check failed", message);
    }
  }

  arbForm.addEventListener("submit", (event) => {
    event.preventDefault();
    clearTimeout(arbDebounceTimer);
    submitArb(collectArbBody());
  });

  arbForm.addEventListener("input", () => {
    if (!livePreviewToggle.checked) return;
    if (!arbForm.checkValidity()) return;
    clearTimeout(arbDebounceTimer);
    arbDebounceTimer = setTimeout(() => submitArb(collectArbBody()), 300);
  });

  function renderArb(arb, body) {
    if (!arb.found) {
      arbResult.className = "result-box arb-miss";
      arbResult.innerHTML =
        '<p class="headline"><span class="badge">NO EDGE</span> Books are internally consistent — no riskless arb.</p>';
      return;
    }
    arbResult.className = "result-box arb-hit";
    const buySize = body[`${arb.buy_venue}_size`] ?? 0;
    const sellSize = body[`${arb.sell_venue}_size`] ?? 0;
    const maxSize = Math.max(buySize, sellSize, arb.executable_size, 1e-9);
    const buyPct = Math.max(4, Math.round((buySize / maxSize) * 100));
    const sellPct = Math.max(4, Math.round((sellSize / maxSize) * 100));
    const execPct = Math.round((arb.executable_size / maxSize) * 100);
    arbResult.innerHTML = `
      <p class="headline"><span class="badge good">EDGE FOUND</span> Buy ${escapeHtml(arb.buy_venue)}, sell ${escapeHtml(arb.sell_venue)}</p>
      <div class="detail-row"><span>Buy price</span><b>${fmt(arb.buy_price)}</b></div>
      <div class="detail-row"><span>Sell price</span><b>${fmt(arb.sell_price)}</b></div>
      <div class="detail-row"><span>Edge per contract</span><b>${fmt(arb.edge)}</b></div>
      <div class="detail-row"><span>Executable size</span><b>${fmt(arb.executable_size, 0)}</b></div>
      <div class="depth-viz">
        <p class="subhead">Executable depth — limited by the thinner side</p>
        <div class="book-viz">
          <div class="book-row bid">
            <span class="book-side-label">BUY</span>
            <span class="book-bar-track"><span class="book-bar-fill" data-w="${buyPct}"></span></span>
            <span class="book-size">×${fmt(buySize, 0)}</span>
          </div>
          <div class="book-row ask">
            <span class="book-side-label">SELL</span>
            <span class="book-bar-track"><span class="book-bar-fill" data-w="${sellPct}"></span></span>
            <span class="book-size">×${fmt(sellSize, 0)}</span>
          </div>
          <div class="spread-readout"><span>Executable</span><b>×${fmt(arb.executable_size, 0)} (${execPct}% of the larger side)</b></div>
        </div>
      </div>
    `;
    applyBarWidths(arbResult);
  }

  // ---------------------------------------------------------------
  // Backtest runner — step progress, PnL hero, drawdown, histogram.
  // ---------------------------------------------------------------
  const runBacktestBtn = document.getElementById("run-backtest");
  const backtestResult = document.getElementById("backtest-result");
  const progressStepsEl = document.getElementById("progress-steps");

  const PIPELINE_STEPS = [
    "Aggregating alpha signals",
    "Running strategy engines",
    "Evaluating risk gate",
    "Matching engine settlement",
    "Computing PnL & drawdown",
  ];

  function renderProgressSteps(activeIndex, done) {
    progressStepsEl.hidden = false;
    progressStepsEl.innerHTML = PIPELINE_STEPS.map((label, i) => {
      let cls = "";
      if (done || i < activeIndex) cls = "done";
      else if (i === activeIndex) cls = "active";
      return `<li class="${cls}"><span class="dot"></span>${escapeHtml(label)}</li>`;
    }).join("");
  }

  runBacktestBtn.addEventListener("click", async () => {
    runBacktestBtn.disabled = true;
    backtestResult.className = "result-box";
    backtestResult.innerHTML = '<p class="hint">Running the pipeline against the scenario…</p>';
    let stepIndex = 0;
    renderProgressSteps(0, false);
    const stepTimer = setInterval(() => {
      stepIndex = Math.min(stepIndex + 1, PIPELINE_STEPS.length - 1);
      renderProgressSteps(stepIndex, false);
    }, 260);

    try {
      const res = await fetch("/api/backtest", { method: "POST" });
      if (!res.ok) throw new Error(`server returned ${res.status}`);
      const report = await res.json();
      clearInterval(stepTimer);
      renderProgressSteps(PIPELINE_STEPS.length - 1, true);
      renderBacktest(report);
    } catch (err) {
      clearInterval(stepTimer);
      progressStepsEl.hidden = true;
      const message = String(err.message || err);
      backtestResult.innerHTML = `<p class="hint">Request failed: ${escapeHtml(message)}</p>`;
      toast("error", "Backtest run failed", message);
    } finally {
      runBacktestBtn.disabled = false;
    }
  });

  function rejectCategory(reason) {
    if (reason === "KillSwitch") return "critical";
    if (
      [
        "ContractLimitExceeded",
        "ClusterLimitExceeded",
        "VenueLimitExceeded",
        "GrossLimitExceeded",
        "NotionalPerOrderExceeded",
        "NotionalVenueExceeded",
        "NotionalTotalExceeded",
        "PriceThroughBook",
      ].includes(reason)
    )
      return "limit";
    if (["NotReconciled", "NoMarketData", "FeedStale", "ClockSkew"].includes(reason)) return "integrity";
    return "guard";
  }

  function rejectionHtml(histogram) {
    if (!histogram.length) return "";
    const maxCount = Math.max(...histogram.map((rc) => rc.count));
    const rows = histogram
      .slice()
      .sort((a, b) => b.count - a.count)
      .map((rc) => {
        const cat = rejectCategory(rc.reason);
        const pct = Math.max(4, Math.round((rc.count / maxCount) * 100));
        return `
        <div class="rej-bar-row" title="${escapeHtml(rc.reason)}: ${rc.count}">
          <span class="rej-bar-label">${escapeHtml(rc.reason)}</span>
          <span class="rej-bar-track"><span class="rej-bar-fill cat-${cat}" data-w="${pct}"></span></span>
          <span class="rej-bar-count">${rc.count}</span>
        </div>`;
      })
      .join("");
    return `
      <div class="rejection-list">
        <p class="subhead">Why orders were rejected by the risk gate</p>
        ${rows}
        <div class="rej-legend">
          <span><i class="cat-critical"></i>kill switch</span>
          <span><i class="cat-limit"></i>position / notional / price guard</span>
          <span><i class="cat-integrity"></i>data integrity</span>
          <span><i class="cat-guard"></i>other</span>
        </div>
      </div>`;
  }

  function pnlHeroHtml(r) {
    const cls = r.net_pnl >= 0 ? "good" : "bad";
    const sign = r.net_pnl >= 0 ? "+" : "";
    return `
      <div class="pnl-hero">
        <div>
          <div class="n ${cls}">${sign}${fmt(r.net_pnl)}</div>
          <div class="l">Net PnL (after fees)</div>
        </div>
        <div class="pnl-hero-meta">
          <div><div class="n small">${fmt(r.gross_pnl)}</div><div class="l">Gross PnL</div></div>
          <div><div class="n small">${fmt(r.fees_paid)}</div><div class="l">Fees paid</div></div>
          <div><div class="n small">${fmt(r.max_drawdown)}</div><div class="l">Max drawdown</div></div>
        </div>
      </div>`;
  }

  function feeBarHtml(r) {
    if (r.gross_pnl <= 0 || r.fees_paid <= 0) {
      return `
        <div class="fee-bar-wrap">
          <p class="subhead">Gross PnL → net PnL</p>
          <p class="hint">Gross PnL wasn't positive (or no fees were paid) this run, so a fee-share bar isn't meaningful — see the PnL figures above.</p>
        </div>`;
    }
    const feesPct = Math.min(100, Math.round((r.fees_paid / r.gross_pnl) * 100));
    const netPct = Math.max(0, 100 - feesPct);
    return `
      <div class="fee-bar-wrap">
        <p class="subhead">Gross PnL → net PnL</p>
        <div class="fee-bar-track">
          <span class="fee-bar-seg net" data-w="${netPct}"></span>
          <span class="fee-bar-seg fees" data-w="${feesPct}"></span>
        </div>
        <div class="fee-bar-legend">
          <span class="net-label">net ${fmt(r.net_pnl)} (${netPct}%)</span>
          <span class="fees-label">fees ${fmt(r.fees_paid)} (${feesPct}%)</span>
        </div>
      </div>`;
  }

  function drawdownChartHtml(curve, maxDrawdown) {
    if (!curve || curve.length < 2) {
      return `
        <div class="drawdown-wrap">
          <p class="subhead"><span>Equity curve &amp; drawdown</span></p>
          <div class="drawdown-chart drawdown-empty">
            not enough fills to chart
          </div>
        </div>`;
    }
    const w = 600;
    const h = 120;
    const pad = 6;
    const min = Math.min(...curve);
    const max = Math.max(...curve);
    const range = max - min || 1;
    const stepX = (w - pad * 2) / (curve.length - 1);
    const points = curve.map((v, i) => [pad + i * stepX, pad + (1 - (v - min) / range) * (h - pad * 2)]);
    const pathD = points.map(([x, y], i) => `${i === 0 ? "M" : "L"}${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
    const last = points[points.length - 1];
    const first = points[0];
    const areaD = `${pathD} L${last[0].toFixed(1)},${h - pad} L${first[0].toFixed(1)},${h - pad} Z`;

    let peakIdx = 0;
    let peakVal = curve[0];
    let worstDd = -1;
    let ddStartX = points[0][0];
    let ddEndX = points[0][0];
    curve.forEach((v, i) => {
      if (v > peakVal) {
        peakVal = v;
        peakIdx = i;
      }
      const dd = peakVal - v;
      if (dd > worstDd) {
        worstDd = dd;
        ddStartX = points[peakIdx][0];
        ddEndX = points[i][0];
      }
    });

    const trend = curve[curve.length - 1] >= curve[0] ? "up" : "down";
    const bandHtml =
      worstDd > 0 && ddEndX > ddStartX
        ? `<rect class="dd-band" x="${ddStartX.toFixed(1)}" y="${pad}" width="${(ddEndX - ddStartX).toFixed(1)}" height="${h - pad * 2}" />`
        : "";

    return `
      <div class="drawdown-wrap">
        <p class="subhead"><span>Equity curve &amp; drawdown</span><span>max drawdown ${fmt(maxDrawdown)}</span></p>
        <svg class="drawdown-chart" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" role="img" aria-label="Equity curve over the backtest run">
          ${bandHtml}
          <path class="dd-area ${trend}" d="${areaD}" />
          <path class="dd-line ${trend}" d="${pathD}" />
        </svg>
      </div>`;
  }

  function renderBacktest(r) {
    let bannerHtml = "";
    if (r.bus_integrity_violated) {
      // Mirrors BacktestReport::headline() on the Rust side: once a
      // Critical bus topic has dropped an order ack, the position book
      // is known-wrong and no PnL number below can be trusted, so don't
      // show one dressed up as a real result.
      bannerHtml = `
        <div class="banner critical">
          <span class="icon">⛔</span>
          <div><strong>INTEGRITY VIOLATED —</strong> a critical bus topic dropped an order ack during this run. The position book, and therefore every P&amp;L number, cannot be trusted this time. Try running it again.</div>
        </div>`;
      toast(
        "error",
        "Bus integrity violated",
        "A critical bus topic dropped an order ack — this run's P&L cannot be trusted."
      );
      backtestResult.className = "result-box arb-miss";
      backtestResult.innerHTML = bannerHtml;
      return;
    }

    const killSwitchTripped = r.rejection_histogram.some((rc) => rc.reason === "KillSwitch");
    if (killSwitchTripped) {
      bannerHtml = `
        <div class="banner warning">
          <span class="icon">⚠</span>
          <div><strong>Kill switch tripped —</strong> the risk gate vetoed one or more orders this run because a kill switch was active. See the rejection breakdown below.</div>
        </div>`;
      toast("warning", "Kill switch tripped", "The risk gate rejected orders this run because a kill switch was active.");
    }

    const opTiles = [
      ["ticks", r.ticks_processed, ""],
      ["alpha events", r.alpha_events_processed, ""],
      ["proposed", r.orders_proposed, ""],
      ["risk-rejected", r.orders_rejected_by_risk, ""],
      ["fills", r.fills, "good"],
      ["filled volume", fmt(r.filled_volume), ""],
      ["gross notional", fmt(r.gross_notional_traded), ""],
    ];
    const tileHtml = opTiles
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
    backtestResult.innerHTML = `
      ${bannerHtml}
      ${pnlHeroHtml(r)}
      ${feeBarHtml(r)}
      ${drawdownChartHtml(r.equity_curve, r.max_drawdown)}
      <div class="stat-grid">${tileHtml}</div>
      ${rejectionHtml(r.rejection_histogram)}
      ${positionsHtml}
    `;
    applyBarWidths(backtestResult);
  }
})();
