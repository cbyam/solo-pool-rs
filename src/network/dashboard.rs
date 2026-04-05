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
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use charming::{
    component::{Axis, Grid},
    datatype::{CompositeValue, DataPoint},
    element::{
        smoothness::Smoothness, AreaStyle, AxisLabel, AxisType, BoundaryGap, Color, LineStyle,
        SplitLine, Tooltip, Trigger,
    },
    series::Line,
    Chart,
};
use serde::{Deserialize, Serialize};
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
        .route("/history", get(history_json))
        .route("/chart", get(chart_json))
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

#[derive(Deserialize)]
struct HistoryParams {
    since: Option<u64>,
}

#[derive(Serialize)]
struct HistoryPoint {
    ts: u64,
    hps: f64,
}

async fn history_json(
    State(state): State<DashState>,
    Query(params): Query<HistoryParams>,
) -> Json<Vec<HistoryPoint>> {
    let since = params.since.unwrap_or(0);
    let points = state
        .stats
        .get_hashrate_history(since)
        .into_iter()
        .map(|(ts, hps)| HistoryPoint { ts, hps })
        .collect();
    Json(points)
}

#[derive(Deserialize)]
struct ChartParams {
    window: Option<String>,
}

async fn chart_json(
    State(state): State<DashState>,
    Query(params): Query<ChartParams>,
) -> impl IntoResponse {
    let window = params.window.as_deref().unwrap_or("36h");
    let window_secs: u64 = match window {
        "36h" => 36 * 3600,
        "1w"  => 7 * 24 * 3600,
        "1m"  => 30 * 24 * 3600,
        "6m"  => 6 * 30 * 24 * 3600,
        _     => 0,
    };

    let since = if window_secs > 0 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().saturating_sub(window_secs))
            .unwrap_or(0)
    } else {
        0
    };

    let mut history = state.stats.get_hashrate_history(since);

    // Append current live value as the trailing edge of the chart.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let live_10m: f64 = state.stats.snapshot().total_hashrate_10m;
    history.push((now_ms, live_10m));

    let data: Vec<DataPoint> = history
        .iter()
        .map(|(ts, hps)| {
            DataPoint::from(CompositeValue::from(vec![
                CompositeValue::from(*ts as i64 * 1000),
                CompositeValue::from(*hps),
            ]))
        })
        .collect();

    let chart = Chart::new()
        .background_color(Color::Value("transparent".to_string()))
        .tooltip(
            Tooltip::new()
                .trigger(Trigger::Axis)
                .background_color(Color::Value("#1e293b".to_string()))
                .border_color(Color::Value("#334155".to_string())),
        )
        .grid(
            Grid::new()
                .left(CompositeValue::from("60px"))
                .right(CompositeValue::from("20px"))
                .top(CompositeValue::from("10px"))
                .bottom(CompositeValue::from("30px"))
                .contain_label(true),
        )
        .x_axis(
            Axis::new()
                .type_(AxisType::Time)
                .boundary_gap(BoundaryGap::CategoryAxis(false))
                .split_line(SplitLine::new().line_style(
                    LineStyle::new().color(Color::Value("rgba(51,65,85,0.4)".to_string())),
                ))
                .axis_label(
                    AxisLabel::new()
                        .color(Color::Value("#64748b".to_string()))
                        .font_size(10.0),
                ),
        )
        .y_axis(
            Axis::new()
                .type_(AxisType::Value)
                .min(CompositeValue::from(0))
                .split_line(SplitLine::new().line_style(
                    LineStyle::new().color(Color::Value("rgba(51,65,85,0.4)".to_string())),
                ))
                .axis_label(
                    AxisLabel::new()
                        .color(Color::Value("#64748b".to_string()))
                        .font_size(10.0),
                ),
        )
        .series(
            Line::new()
                .data(data)
                .show_symbol(false)
                .smooth(Smoothness::from(0.35f64))
                .line_style(
                    LineStyle::new()
                        .color(Color::Value("#38bdf8".to_string()))
                        .width(1.0),
                )
                .area_style(
                    AreaStyle::new()
                        .color(Color::Value("rgba(56,189,248,0.08)".to_string())),
                ),
        );

    let body = serde_json::to_string(&chart).unwrap_or_else(|_| "{}".to_string());
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
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
<script src="https://cdn.jsdelivr.net/npm/echarts@5.5.1/dist/echarts.min.js"></script>
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
header { display: grid; grid-template-columns: auto 1fr auto; align-items: center; margin-bottom: 1.5rem; gap: 0.5rem; }
h1 { grid-column: 1; font-size: 1.4rem; font-weight: 700; color: var(--accent); letter-spacing: -0.02em; text-align: left; margin: 0; }
#network-hashrate-display { grid-column: 2; text-align: center; font-weight: 600; color: var(--accent); }
.header-controls { grid-column: 3; justify-self: end; display: flex; gap: 1rem; align-items: center; }
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
#hashrate-chart { height: 240px; width: 100%; }
table { width: 100%; border-collapse: collapse; font-size: 0.875rem; }
th { text-align: left; color: var(--muted); font-weight: 500; padding: 0.3rem 0.5rem; border-bottom: 1px solid var(--border); font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.06em; }
td { padding: 0.45rem 0.5rem; border-bottom: 1px solid rgba(51,65,85,0.4); }
tr:last-child td { border-bottom: none; }
.online { color: var(--green); }
.offline { color: var(--red); }
.empty-row { color: var(--muted); text-align: center; padding: 1.2rem; font-size: 0.875rem; }
</style>
</head>
<body>
<header>
  <h1>&#9729; solo-pool-rs</h1>
  <div id="network-hashrate-display">Network Hashrate: <span id="v-network-hashrate">—</span> / Diff: <span id="v-network-difficulty-header">—</span></div>
  <div class="header-controls">
    <a href="/metrics" style="color: var(--accent); font-size: 0.75rem; text-decoration: none;">Raw metrics</a>
    <span id="last-updated">Loading&hellip;</span>
  </div>
</header>

<div class="cards" style="display:none;">
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
    <div class="card-value" id="v-height-hidden" title="legacy hidden block">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Last Block Worker</div>
    <div class="card-value" id="v-last-block-worker">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Last Block Hash</div>
    <div class="card-value" id="v-last-block-hash">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Last Block Time</div>
    <div class="card-value" id="v-last-block-time">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Best Hashrate (Since boot)</div>
    <div class="card-value accent" id="v-session-best-hashrate">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Best Hashrate (All-time)</div>
    <div class="card-value accent" id="v-best-hashrate">&mdash;</div>
  </div>
  <div class="card">
    <div class="card-label">Best Share (All-time)</div>
    <div class="card-value" id="v-best-share">—</div>
  </div>
  <div class="card">
    <div class="card-label">Best Share (Session)</div>
    <div class="card-value" id="v-session-best-share">—</div>
  </div>
  <div class="card">
    <div class="card-label">Network Difficulty</div>
    <div class="card-value" id="v-network-difficulty">—</div>
  </div>
  <div class="card">
    <div class="card-label">Best ≥ Network</div>
    <div class="card-value accent" id="v-best-over-network">—</div>
  </div>
  <div class="card">
    <div class="card-label">Uptime</div>
    <div class="card-value" id="v-uptime">&mdash;</div>
  </div>
</div>
<!-- new panels layout -->
<div class="cards">
  <div class="card">
    <div class="card-label">Total Reported Hashrate</div>
    <div class="card-value accent" id="v-reported-current" title="Current pool hash from worker rate">—</div>
    <div class="card-value" id="v-reported-3h" style="font-size:0.8rem; font-weight:500;" title="3-hour moving average">3h avg: —</div>
    <div class="card-value" id="v-reported-24h" style="font-size:0.8rem; font-weight:500;" title="24-hour moving average">24h avg: —</div>
  </div>
  <div class="card">
    <div class="card-label">Total Effective Hashrate</div>
    <div class="card-value accent" id="v-effective-hashrate" title="Accepted share rate hash estimate">—</div>
    <div class="card-value" style="font-size:0.8rem; font-weight:500;">Based on accepted shares</div>
  </div>
  <div class="card">
    <div class="card-label">Active Workers</div>
    <div class="card-value" id="v-workers-online" style="font-size:0.8rem; font-weight:500;">Online: —</div>
    <div class="card-value" id="v-workers-offline" style="font-size:0.8rem; font-weight:500;">Offline: —</div>
    <div class="card-value" id="v-workers-degraded" style="font-size:0.8rem; font-weight:500;">Degraded: —</div>
  </div>
  <div class="card">
    <div class="card-label">Reject / Stale</div>
    <div class="card-value red" id="v-reject-rate" style="font-size:0.8rem; font-weight:500;">Reject: —</div>
    <div class="card-value" id="v-stale-rate" style="font-size:0.8rem; font-weight:500;">Stale: —</div>
  </div>
  <div class="card">
    <div class="card-label">Block Height</div>
    <div class="card-value" id="v-height" title="Height of current best chain tip">—</div>
    <div class="card-value" id="v-block-reward" style="font-size:0.8rem; font-weight:500;">Reward: —</div>
    <div class="card-value" id="v-btc-price" style="font-size:0.8rem; font-weight:500; color:var(--muted);">BTC: —</div>
  </div>
  <div class="card">
    <div class="card-label">Probability</div>
    <div class="card-value" id="v-prob-daily" style="font-size:0.8rem; font-weight:500;">Daily: —</div>
    <div class="card-value" id="v-prob-monthly" style="font-size:0.8rem; font-weight:500;">Monthly: —</div>
    <div class="card-value" id="v-prob-yearly" style="font-size:0.8rem; font-weight:500;">Yearly: —</div>
    <div class="card-value" id="v-prob-powerball" style="font-size:0.8rem; font-weight:500; color:var(--muted);">vs Powerball: —</div>
  </div>
</div>
<div class="panel">
  <div style="display:flex; justify-content:space-between; align-items:center; margin-bottom:0.6rem;">
    <div class="panel-title">Hashrate over time <span title="Plots the 10-minute average hashrate, sampled every 10 minutes" style="cursor:help; font-size:0.7rem; color:var(--muted); border:1px solid var(--muted); border-radius:50%; padding:0 0.3rem; vertical-align:middle;">?</span></div>
    <label style="font-size:0.72rem; color:var(--muted);">Window:
      <select id="timeframe-select" style="margin-left:0.4rem; font-size:0.72rem; padding:0.2rem 0.4rem;">
        <option value="36h" selected>36h</option>
        <option value="1w">1w</option>
        <option value="1m">1m</option>
        <option value="6m">6m</option>
        <option value="all">all</option>
      </select>
    </label>
  </div>
  <div id="hashrate-chart"></div>
</div>

<div class="panel">
  <div class="panel-title">Workers</div>
  <table>
    <thead>
      <tr>
        <th>Worker</th>
        <th>Status</th>
        <th>Vardiff</th>
        <th>Hashrate (10m)</th>
        <th>Hashrate (3h)</th>
        <th>Effective (10m)</th>
        <th>Accepted</th>
        <th>Rejected</th>
        <th>Best Share</th>
        <th>Last Share</th>
        <th>Uptime</th>
      </tr>
    </thead>
    <tbody id="workers-tbody">
      <tr><td colspan="11" class="empty-row">Loading workers…</td></tr>
    </tbody>
  </table>

<script>
const DEFAULT_WINDOW = '36h';
let selectedWindow = DEFAULT_WINDOW;

const myChart = echarts.init(document.getElementById('hashrate-chart'), null, { renderer: 'canvas' });
window.addEventListener('resize', () => myChart.resize());

function fmtHr(hps, short) {
  if (hps >= 1e21) return (hps / 1e21).toFixed(2) + (short ? ' Z'  : ' ZH/s');
  if (hps >= 1e18) return (hps / 1e18).toFixed(2) + (short ? ' E'  : ' EH/s');
  if (hps >= 1e15) return (hps / 1e15).toFixed(2) + (short ? ' P'  : ' PH/s');
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

async function loadChart(window) {
  try {
    const resp = await fetch('/chart?window=' + window);
    if (!resp.ok) return;
    const options = await resp.json();
    // Patch in JS formatter callbacks that cannot be serialised from Rust.
    const yAxis = Array.isArray(options.yAxis) ? options.yAxis[0] : options.yAxis;
    if (yAxis) yAxis.axisLabel = Object.assign(yAxis.axisLabel || {}, { formatter: v => fmtHr(v, true) });
    const xAxis = Array.isArray(options.xAxis) ? options.xAxis[0] : options.xAxis;
    if (xAxis) xAxis.axisLabel = Object.assign(xAxis.axisLabel || {}, {
      formatter: v => {
        const d = new Date(v);
        if (d.getHours() === 0 && d.getMinutes() === 0) {
          return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
        }
        return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
      }
    });
    if (options.tooltip) {
      options.tooltip.formatter = params => {
        if (!params || !params.length) return '';
        const pt = params[0];
        const ts = Array.isArray(pt.value) ? pt.value[0] : pt.value;
        const hps = Array.isArray(pt.value) ? pt.value[1] : 0;
        const date = new Date(ts).toLocaleString([], { year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
        return date + '<br/><span style="color:#38bdf8">Hashrate (10m)</span>: ' + fmtHr(hps, false);
      };
    }
    myChart.setOption(options, true);
  } catch (e) {
    console.error('Chart fetch error:', e);
  }
}

async function refresh() {
  try {
    const resp = await fetch('/stats');
    if (!resp.ok) return;
    const d = await resp.json();

    const reported10m = d.total_hashrate_10m || 0;
    const reported3h  = d.total_hashrate_3h  || 0;
    const effective10m = d.total_effective_10m || 0;

    document.getElementById('v-reported-current').textContent = fmtHr(reported10m, false);
    document.getElementById('v-reported-3h').textContent = '3h avg: ' + fmtHr(reported3h, false);
    document.getElementById('v-reported-24h').textContent = '24h avg: ' + fmtHr(d.total_hashrate_24h || 0, false);
    document.getElementById('v-effective-hashrate').textContent = fmtHr(effective10m, false);

    updateProbability(d.total_hashrate_10m || 0, d.network_hashrate_hps || 0);

    document.getElementById('v-miners').textContent = d.connected_miners;
    document.getElementById('v-height').textContent = d.current_height.toLocaleString();
    if (d.current_coinbase_value) {
      const btc = d.current_coinbase_value / 1e8;
      document.getElementById('v-block-reward').textContent = 'Reward: ' + btc.toFixed(8) + ' BTC';
    }
    document.getElementById('v-last-block-worker').textContent = d.last_block_worker || '—';
    document.getElementById('v-last-block-hash').textContent = d.last_block_hash || '—';
    document.getElementById('v-last-block-time').textContent = fmtTimestamp(d.last_block_ts);
    document.getElementById('v-best-share').textContent = fmtDiff(d.best_share_difficulty);
    document.getElementById('v-session-best-share').textContent = fmtDiff(d.session_best_share_difficulty);
    document.getElementById('v-network-difficulty').textContent = d.network_difficulty.toFixed(4);
    document.getElementById('v-network-difficulty-header').textContent = d.network_difficulty.toFixed(4);
    document.getElementById('v-best-over-network').textContent = d.best_share_difficulty >= Math.ceil(d.network_difficulty) ? 'YES' : 'no';
    document.getElementById('v-session-best-hashrate').textContent = fmtHr(d.session_best_hashrate_hps, false);
    document.getElementById('v-best-hashrate').textContent = fmtHr(d.best_hashrate_hps, false);
    document.getElementById('v-uptime').textContent = fmtUptime(d.uptime_secs);

    const total = d.shares_accepted + d.shares_rejected;
    const rejectPct = total > 0 ? (d.shares_rejected / total * 100).toFixed(1) : '0.0';
    const staleTotal = Array.isArray(d.worker_states) ? d.worker_states.reduce((sum, w) => sum + (w.shares_stale || 0), 0) : 0;
    const stalePct = total > 0 ? (staleTotal / total * 100).toFixed(1) : '0.0';

    document.getElementById('v-reject-rate').textContent = `Reject: ${d.shares_rejected.toLocaleString()} (${rejectPct}%)`;
    document.getElementById('v-stale-rate').textContent = `Stale: ${staleTotal.toLocaleString()} (${stalePct}%)`;

    const workers = Array.isArray(d.worker_states) ? d.worker_states : [];
    const onlineCount = workers.filter(w => w.online).length;
    const offlineCount = workers.filter(w => !w.online).length;
    const degradedCount = workers.filter(w => w.online && w.last_submit_ts > 0 && Math.floor(Date.now() / 1000) - w.last_submit_ts > 120).length;

    document.getElementById('v-workers-online').textContent = 'Online: ' + onlineCount;
    document.getElementById('v-workers-offline').textContent = 'Offline: ' + offlineCount;
    document.getElementById('v-workers-degraded').textContent = 'Degraded: ' + degradedCount;

    document.getElementById('v-network-hashrate').textContent = fmtHr(d.network_hashrate_hps, false);

    // Workers table
    const tbody = document.getElementById('workers-tbody');
    if (workers.length === 0) {
      tbody.innerHTML = '<tr><td colspan="11" class="empty-row">No connected workers</td></tr>';
    } else {
      tbody.innerHTML = workers
        .sort((a, b) => b.hashrate_10m_hps - a.hashrate_10m_hps)
        .map(w => {
          const workerName = w.worker.includes('.') ? w.worker.split('.')[1] : w.worker;
          const nowSec = Math.floor(Date.now() / 1000);
          const lastShareAgo = w.last_submit_ts > 0 ? fmtUptime(nowSec - w.last_submit_ts) : '—';
          const uptime = w.connected_ts > 0 ? fmtUptime(nowSec - w.connected_ts) : '—';
          return `<tr>
            <td>${escHtml(workerName)}</td>
            <td class="${w.online ? 'online' : 'offline'}">${w.online ? 'Online' : 'Offline'}</td>
            <td>${fmtDiff(w.current_vardiff)}</td>
            <td>${fmtHr(w.hashrate_10m_hps, false)}</td>
            <td>${fmtHr(w.hashrate_3h_hps, false)}</td>
            <td>${fmtHr(w.effective_10m_hps, false)}</td>
            <td>${w.shares_accepted.toLocaleString()}</td>
            <td>${w.shares_rejected.toLocaleString()}</td>
            <td>${fmtDiff(w.best_share_difficulty)}</td>
            <td>${lastShareAgo}</td>
            <td>${uptime}</td>
          </tr>`;
        })
        .join('');
    }

    document.getElementById('last-updated').textContent = 'Updated ' + new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
  } catch (e) {
    console.error('Dashboard refresh error:', e);
  }
}

function fmtOdds(p) {
  if (p <= 0) return '—';
  const inv = Math.round(1 / p);
  if (inv >= 1e9)  return '1 in ' + (inv / 1e9).toFixed(1) + 'B';
  if (inv >= 1e6)  return '1 in ' + (inv / 1e6).toFixed(2) + 'M';
  if (inv >= 1e3)  return '1 in ' + (inv / 1e3).toFixed(1) + 'K';
  return '1 in ' + inv.toLocaleString();
}


function updateProbability(ourHps, netHps) {
  const el = id => document.getElementById(id);
  if (!ourHps || !netHps || netHps === 0) {
    el('v-prob-daily').textContent   = 'Daily: —';
    el('v-prob-monthly').textContent = 'Monthly: —';
    el('v-prob-yearly').textContent  = 'Yearly: —';
    el('v-prob-powerball').textContent = 'vs Powerball: —';
    return;
  }
  // Probability of finding a block per block (~10 min)
  const pBlock = ourHps / netHps;
  // Blocks per period
  const blocksPerDay   = 144;
  const blocksPerMonth = blocksPerDay * 30;
  const blocksPerYear  = blocksPerDay * 365;
  // P(at least one block in N blocks) = 1 - (1 - pBlock)^N
  const pDaily   = 1 - Math.pow(1 - pBlock, blocksPerDay);
  const pMonthly = 1 - Math.pow(1 - pBlock, blocksPerMonth);
  const pYearly  = 1 - Math.pow(1 - pBlock, blocksPerYear);
  // Powerball jackpot: 1 in 292,201,338 per ticket
  const pPowerball = 1 / 292201338;
  const ratio = pDaily / pPowerball;
  const vsText = ratio >= 1
    ? (ratio.toFixed(1) + '× better than Powerball')
    : ((1 / ratio).toFixed(1) + '× worse than Powerball');

  el('v-prob-daily').textContent   = 'Daily: '   + fmtOdds(pDaily);
  el('v-prob-monthly').textContent = 'Monthly: ' + fmtOdds(pMonthly);
  el('v-prob-yearly').textContent  = 'Yearly: '  + fmtOdds(pYearly);
  el('v-prob-powerball').textContent = vsText;
}

function attachTimeframeSelector() {
  const select = document.getElementById('timeframe-select');
  select.addEventListener('change', () => {
    selectedWindow = select.value;
    loadChart(selectedWindow);
  });
}

function escHtml(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

async function fetchBtcPrice() {
  try {
    const resp = await fetch('https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd');
    if (!resp.ok) return;
    const data = await resp.json();
    const price = data?.bitcoin?.usd;
    if (price != null) {
      document.getElementById('v-btc-price').textContent = 'BTC $' + price.toLocaleString([], { maximumFractionDigits: 0 });
    }
  } catch (_) {}
}

attachTimeframeSelector();
loadChart(DEFAULT_WINDOW);
refresh();
fetchBtcPrice();
setInterval(refresh, 10000);
setInterval(() => loadChart(selectedWindow), 60000);
setInterval(fetchBtcPrice, 60000);
</script>
</body>
</html>"#;
