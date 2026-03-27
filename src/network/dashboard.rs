/// network/dashboard.rs
///
/// Visual HTTP dashboard served on the configured prometheus_addr.
///
/// Routes:
///   GET /         → HTML dashboard (Chart.js, auto-refreshes every 10 s)
///   GET /stats    → JSON snapshot of PoolStats
///   GET /metrics  → Prometheus text (via PrometheusHandle::render)
use crate::stats::PoolStats;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use metrics_exporter_prometheus::PrometheusHandle;
use std::{net::SocketAddr, sync::Arc};
use tracing::{info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// State
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DashState {
    pub stats: Arc<PoolStats>,
    pub prometheus: Option<PrometheusHandle>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Startup
// ─────────────────────────────────────────────────────────────────────────────

pub async fn start(addr: &str, stats: Arc<PoolStats>, prometheus: Option<PrometheusHandle>) {
    if addr.is_empty() {
        return;
    }

    let socket_addr: SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            warn!("Invalid dashboard addr '{addr}': {e}");
            return;
        }
    };

    let state = DashState { stats, prometheus };
    let app = Router::new()
        .route("/", get(dashboard_html))
        .route("/stats", get(stats_json))
        .route("/metrics", get(metrics_text))
        .with_state(state);

    match tokio::net::TcpListener::bind(socket_addr).await {
        Ok(listener) => {
            info!("Dashboard at http://{addr}/  metrics at http://{addr}/metrics");
            tokio::spawn(async move {
                axum::serve(listener, app).await.ok();
            });
        }
        Err(e) => warn!("Failed to bind dashboard on {addr}: {e}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Route handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn dashboard_html() -> Html<&'static str> {
    Html(DASHBOARD_HTML)
}

async fn stats_json(State(state): State<DashState>) -> Json<crate::stats::StatsSnapshot> {
    Json(state.stats.snapshot())
}

async fn metrics_text(State(state): State<DashState>) -> Response {
    match &state.prometheus {
        Some(handle) => {
            let body = handle.render();
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; version=0.0.4",
                )],
                body,
            )
                .into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Prometheus metrics not enabled",
        )
            .into_response(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Dashboard HTML
// ─────────────────────────────────────────────────────────────────────────────

const DASHBOARD_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>solo-pool-rs</title>
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.0/dist/chart.umd.min.js"></script>
<style>
:root {
  --bg: #0f172a;
  --card: #1e293b;
  --border: #334155;
  --text: #f1f5f9;
  --muted: #94a3b8;
  --accent: #38bdf8;
  --green: #4ade80;
  --red: #f87171;
}
* { box-sizing: border-box; margin: 0; padding: 0; }
body { background: var(--bg); color: var(--text); font-family: 'Segoe UI', system-ui, sans-serif; padding: 1.5rem; min-height: 100vh; }
header { display: flex; align-items: center; justify-content: space-between; margin-bottom: 1.5rem; flex-wrap: wrap; gap: 0.5rem; }
h1 { font-size: 1.4rem; font-weight: 700; color: var(--accent); letter-spacing: -0.02em; }
#last-updated { font-size: 0.72rem; color: var(--muted); }
.cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(190px, 1fr)); gap: 0.9rem; margin-bottom: 1.25rem; }
.card { background: var(--card); border: 1px solid var(--border); border-radius: 0.75rem; padding: 1rem 1.2rem; }
.card-label { font-size: 0.65rem; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); margin-bottom: 0.35rem; }
.card-value { font-size: 1.55rem; font-weight: 700; font-variant-numeric: tabular-nums; line-height: 1.2; }
.green { color: var(--green); }
.red   { color: var(--red); }
.accent { color: var(--accent); }
.panel { background: var(--card); border: 1px solid var(--border); border-radius: 0.75rem; padding: 1.2rem; margin-bottom: 1.25rem; }
.panel-title { font-size: 0.72rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.08em; color: var(--muted); margin-bottom: 1rem; }
canvas { max-height: 240px; width: 100% !important; }
table { width: 100%; border-collapse: collapse; font-size: 0.875rem; }
th { text-align: left; color: var(--muted); font-weight: 500; padding: 0.3rem 0.5rem; border-bottom: 1px solid var(--border); font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.06em; }
td { padding: 0.45rem 0.5rem; border-bottom: 1px solid rgba(51,65,85,0.4); }
tr:last-child td { border-bottom: none; }
.empty-row { color: var(--muted); text-align: center; padding: 1.2rem; font-size: 0.875rem; }
</style>
</head>
<body>
<header>
  <h1>&#9729; solo-pool-rs</h1>
  <span id="last-updated">Loading&hellip;</span>
</header>

<div class="cards">
  <div class="card">
    <div class="card-label">Total Hashrate</div>
    <div class="card-value accent" id="v-hashrate">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Accepted Shares</div>
    <div class="card-value green" id="v-accepted">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Rejected Shares</div>
    <div class="card-value red" id="v-rejected">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Connected Miners</div>
    <div class="card-value" id="v-miners">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Block Height</div>
    <div class="card-value" id="v-height">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Best Share</div>
    <div class="card-value" id="v-best-share">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Uptime</div>
    <div class="card-value" id="v-uptime">&mdash;</div>
  </div>
</div>

<div class="panel">
  <div class="panel-title">Hashrate over time</div>
  <canvas id="hashrate-chart"></canvas>
</div>

<div class="panel">
  <div class="panel-title">Workers</div>
  <table>
    <thead>
      <tr><th>Worker</th><th>Last Share</th><th>Hashrate (60s)</th><th>Hashrate (3h)</th></tr>
    </thead>
    <tbody id="workers-tbody">
      <tr><td colspan="4" class="empty-row">Loading workers…</td></tr>
    </tbody>
  </table>

<script>
const MAX_POINTS = 60;
const chartLabels = [];
const chartData   = [];

const ctx   = document.getElementById('hashrate-chart').getContext('2d');
const chart = new Chart(ctx, {
  type: 'line',
  data: {
    labels: chartLabels,
    datasets: [{
      label: 'Hashrate',
      data: chartData,
      borderColor: '#38bdf8',
      backgroundColor: 'rgba(56,189,248,0.07)',
      borderWidth: 2,
      pointRadius: 2,
      pointHoverRadius: 4,
      fill: true,
      tension: 0.35,
    }],
  },
  options: {
    responsive: true,
    maintainAspectRatio: true,
    animation: false,
    interaction: { mode: 'index', intersect: false },
    scales: {
      x: {
        ticks: { color: '#64748b', maxTicksLimit: 8, font: { size: 10 } },
        grid:  { color: 'rgba(51,65,85,0.4)' },
        border: { color: '#334155' },
      },
      y: {
        min: 0,
        ticks: { color: '#64748b', font: { size: 10 }, callback: v => fmtHr(v, true) },
        grid:  { color: 'rgba(51,65,85,0.4)' },
        border: { color: '#334155' },
      },
    },
    plugins: {
      legend: { display: false },
      tooltip: {
        backgroundColor: '#1e293b',
        borderColor: '#334155',
        borderWidth: 1,
        titleColor: '#94a3b8',
        bodyColor: '#f1f5f9',
        callbacks: { label: c => '  ' + fmtHr(c.parsed.y, false) },
      },
    },
  },
});

function fmtHr(hps, short) {
  if (hps >= 1e12) return (hps / 1e12).toFixed(2) + (short ? ' T'  : ' TH/s');
  if (hps >= 1e9)  return (hps / 1e9 ).toFixed(2) + (short ? ' G'  : ' GH/s');
  if (hps >= 1e6)  return (hps / 1e6 ).toFixed(2) + (short ? ' M'  : ' MH/s');
  if (hps >= 1e3)  return (hps / 1e3 ).toFixed(2) + (short ? ' K'  : ' KH/s');
  return hps.toFixed(0) + (short ? ''    : ' H/s');
}

function fmtDiff(d) {
  if (d >= 1e12) return (d / 1e12).toFixed(2) + 'T';
  if (d >= 1e9)  return (d / 1e9 ).toFixed(2) + 'G';
  if (d >= 1e6)  return (d / 1e6 ).toFixed(2) + 'M';
  if (d >= 1e3)  return (d / 1e3 ).toFixed(1) + 'K';
  return d.toString();
}

function fmtUptime(secs) {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (d) return d + 'd ' + h + 'h';
  if (h) return h + 'h ' + m + 'm';
  if (m) return m + 'm ' + s + 's';
  return s + 's';
}

function fmtTimestamp(ts) {
  if (!ts || ts === 0) return '—';
  return new Date(ts * 1000).toLocaleString();
}

async function refresh() {
  try {
    const resp = await fetch('/stats');
    if (!resp.ok) return;
    const d = await resp.json();

    const displayHashrate = d.total_hashrate_3h > 0 ? d.total_hashrate_3h : (d.total_hashrate_60s > 0 ? d.total_hashrate_60s : d.total_hashrate_hps);
    document.getElementById('v-hashrate').textContent      = fmtHr(displayHashrate, false);
    document.getElementById('v-accepted').textContent      = d.shares_accepted.toLocaleString();
    document.getElementById('v-miners').textContent        = d.connected_miners;
    document.getElementById('v-height').textContent        = d.current_height.toLocaleString();
    document.getElementById('v-best-share').textContent    = fmtDiff(d.best_share_difficulty);
    document.getElementById('v-uptime').textContent        = fmtUptime(d.uptime_secs);

    const total60s = d.total_hashrate_60s ?? 0;
    const total3h = d.total_hashrate_3h ?? 0;
    // Optional: update additional totals in console
    // console.debug('Total 60s', fmtHr(total60s,false), 'Total 3h', fmtHr(total3h,false));

    const total = d.shares_accepted + d.shares_rejected;
    const pct   = total > 0 ? (d.shares_rejected / total * 100).toFixed(1) : '0.0';
    const rejectedEl = document.getElementById('v-rejected');
    rejectedEl.textContent = `${d.shares_rejected.toLocaleString()} (${pct}%)`;
    rejectedEl.className = 'card-value ' + (parseFloat(pct) > 5 ? 'red' : 'green');

    // Chart
    const now = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    chartLabels.push(now);
    chartData.push(d.total_hashrate_hps);
    if (chartLabels.length > MAX_POINTS) { chartLabels.shift(); chartData.shift(); }
    chart.update('none');

    // Workers table
    const tbody = document.getElementById('workers-tbody');
    const workers = Array.isArray(d.worker_hashrates) ? d.worker_hashrates : [];
    if (workers.length === 0) {
      tbody.innerHTML = '<tr><td colspan="4" class="empty-row">No connected workers</td></tr>';
    } else {
      tbody.innerHTML = workers
        .sort((a, b) => b.hashrate_3h_hps - a.hashrate_3h_hps)
        .map(w => {
          const workerName = w.worker.includes('.') ? w.worker.split('.')[1] : w.worker;
          return `<tr><td>${escHtml(workerName)}</td><td>${fmtTimestamp(w.last_submit_ts)}</td><td>${fmtHr(w.hashrate_60s_hps, false)}</td><td>${fmtHr(w.hashrate_3h_hps, false)}</td></tr>`;
        })
        .join('');
    }

    document.getElementById('last-updated').textContent = 'Updated ' + now;
  } catch (e) {
    console.error('Dashboard refresh error:', e);
  }
}

function escHtml(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

refresh();
setInterval(refresh, 10000);
</script>
</body>
</html>"#;
