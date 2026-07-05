/// network/dashboard.rs
///
/// Visual HTTP dashboard served on the configured prometheus_addr.
///
/// Routes:
///   GET /            → HTML dashboard (ECharts, auto-refreshes every 10 s)
///   GET /favicon.ico → embedded site icon
///   GET /stats       → JSON snapshot of PoolStats
///   GET /metrics  → Prometheus text (via PrometheusHandle::render)
use crate::{mining::engine::TemplateEngine, settings::RuntimeSettings, stats::PoolStats};
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
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tracing::{info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// State
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DashState {
    pub stats: Arc<PoolStats>,
    pub prometheus: Option<PrometheusHandle>,
    pub settings: Arc<RuntimeSettings>,
    pub engine: Arc<TemplateEngine>,
    pub allow_settings: bool,
    /// Port miners connect to (Stratum). Shown on the Connect page; the host is
    /// derived client-side from the browser's own location.
    pub stratum_port: u16,
    pub sv2_enabled: bool,
    /// Base58check SV2 Noise authority public key (None when SV2 is disabled).
    /// Shown on the Connect page so miners can pin the pool identity.
    pub sv2_authority_pubkey: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Startup
// ─────────────────────────────────────────────────────────────────────────────

// Startup wiring pulls together independently-constructed runtime pieces; a
// parameter bundle would just move the same fields around for no clarity gain.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    addr: &str,
    stats: Arc<PoolStats>,
    prometheus: Option<PrometheusHandle>,
    settings: Arc<RuntimeSettings>,
    engine: Arc<TemplateEngine>,
    allow_settings: bool,
    stratum_listen_addr: &str,
    sv2_enabled: bool,
    sv2_authority_pubkey: Option<String>,
) {
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

    // Just the port — the Connect page builds the URL from the browser's host.
    let stratum_port = stratum_listen_addr
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0);

    let state = DashState {
        stats,
        prometheus,
        settings,
        engine,
        allow_settings,
        stratum_port,
        sv2_enabled,
        sv2_authority_pubkey,
    };
    let app = Router::new()
        .route("/", get(dashboard_html))
        .route("/favicon.ico", get(favicon))
        .route("/logo-dark.svg", get(logo_dark))
        .route("/logo-light.svg", get(logo_light))
        .route("/stats", get(stats_json))
        .route("/history", get(history_json))
        .route("/chart", get(chart_json))
        .route("/api/settings", get(settings_get).post(settings_post))
        .route("/api/info", get(info_get))
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

async fn favicon() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/x-icon")],
        include_bytes!("favicon.ico").as_slice(),
    )
}

async fn logo_dark() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        LOGO_DARK_SVG,
    )
}

async fn logo_light() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        LOGO_LIGHT_SVG,
    )
}

// Brand mark: a Bitcoin "block" (isometric cube) crossed by a miner's pickaxe.
// Two theme-tuned variants — the orange block is shared, only the badge tile and
// the steel of the pickaxe change so the mark reads on each theme's rail.
//   carbon: dark tile, light-steel pick.  light: porcelain tile, slate pick.
const LOGO_DARK_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img" aria-label="solo-pool-rs">
<rect x="3" y="3" width="58" height="58" rx="13" fill="#1a1a1f" stroke="#232328" stroke-width="1.5"/>
<polygon points="32,29 48,37.5 32,46 16,37.5" fill="#f7931a"/>
<polygon points="16,37.5 32,46 32,58 16,49.5" fill="#b8650a"/>
<polygon points="32,46 48,37.5 48,49.5 32,58" fill="#d97b10"/>
<path d="M33 19 L31 42" fill="none" stroke="#9aa0ac" stroke-width="5.5" stroke-linecap="round"/>
<path d="M13 30 Q20 17 33 16 Q47 17 55 30 Q47 23 33 23 Q20 23 13 30 Z" fill="#cdd2db"/>
</svg>"##;

const LOGO_LIGHT_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" role="img" aria-label="solo-pool-rs">
<rect x="3" y="3" width="58" height="58" rx="13" fill="#f4f4f5" stroke="#e4e4e7" stroke-width="1.5"/>
<polygon points="32,29 48,37.5 32,46 16,37.5" fill="#f7931a"/>
<polygon points="16,37.5 32,46 32,58 16,49.5" fill="#b8650a"/>
<polygon points="32,46 48,37.5 48,49.5 32,58" fill="#d97b10"/>
<path d="M33 19 L31 42" fill="none" stroke="#3f4651" stroke-width="5.5" stroke-linecap="round"/>
<path d="M13 30 Q20 17 33 16 Q47 17 55 30 Q47 23 33 23 Q20 23 13 30 Z" fill="#555b66"/>
</svg>"##;

async fn stats_json(State(state): State<DashState>) -> Json<crate::stats::StatsSnapshot> {
    Json(state.stats.snapshot())
}

/// Static-ish pool info for the Connect page: how to point a miner here, plus
/// build/network/payout context. The host is added client-side from the URL.
#[derive(Serialize)]
struct InfoView {
    version: &'static str,
    stratum_port: u16,
    sv2_enabled: bool,
    /// SV2 Noise authority public key (base58check) for identity pinning;
    /// null when SV2 is disabled.
    sv2_authority_pubkey: Option<String>,
    network: String,
    coinbase_address: String,
}

async fn info_get(State(state): State<DashState>) -> Json<InfoView> {
    Json(InfoView {
        version: env!("CARGO_PKG_VERSION"),
        stratum_port: state.stratum_port,
        sv2_enabled: state.sv2_enabled,
        sv2_authority_pubkey: state.sv2_authority_pubkey.clone(),
        network: state.settings.network().to_string(),
        coinbase_address: state.settings.coinbase_address(),
    })
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

// ── Runtime settings (payout address / network) ──────────────────────────────

#[derive(Serialize)]
struct SettingsView {
    coinbase_address: String,
    /// Node-derived network ("mainnet" | "testnet" | "signet" | "regtest").
    network: String,
    /// Whether the address validates against the node's network. Mining is
    /// paused while false.
    address_valid: bool,
    /// Whether a stats DB backs the settings (changes survive restarts).
    persisted: bool,
    /// Whether changes are allowed ([metrics] allow_runtime_settings).
    editable: bool,
}

async fn settings_get(State(state): State<DashState>) -> Json<SettingsView> {
    Json(SettingsView {
        coinbase_address: state.settings.coinbase_address(),
        network: state.settings.network().to_string(),
        address_valid: state.settings.address_valid(),
        persisted: state.stats.has_store(),
        editable: state.allow_settings,
    })
}

#[derive(Deserialize)]
struct SettingsUpdate {
    coinbase_address: String,
}

async fn settings_post(
    State(state): State<DashState>,
    Json(req): Json<SettingsUpdate>,
) -> Response {
    if !state.allow_settings {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "runtime settings are disabled ([metrics] allow_runtime_settings = false)"
            })),
        )
            .into_response();
    }

    let address = req.coinbase_address.trim();
    if let Err(e) = state.settings.update(address) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response();
    }

    let persisted = state.stats.save_setting("coinbase_address", address);

    info!(
        address = %address,
        network = %state.settings.network(),
        persisted,
        "Payout address changed via dashboard — broadcasting clean job"
    );

    // New clean job so connected miners switch to the new payout immediately.
    state.engine.force_refresh().await;

    Json(serde_json::json!({ "ok": true, "persisted": persisted })).into_response()
}

#[derive(Deserialize)]
struct ChartParams {
    window: Option<String>,
}

// Chart colors here are placeholders: the dashboard JS re-skins every color
// (line, area, axes, grid, tooltip) from the active theme's CSS variables in
// loadChart(), so the light/dark toggle restyles the chart without a server
// round-trip carrying theme state.
async fn chart_json(
    State(state): State<DashState>,
    Query(params): Query<ChartParams>,
) -> impl IntoResponse {
    let window = params.window.as_deref().unwrap_or("36h");
    let window_secs: u64 = match window {
        "36h" => 36 * 3600,
        "1w" => 7 * 24 * 3600,
        "1m" => 30 * 24 * 3600,
        "6m" => 6 * 30 * 24 * 3600,
        _ => 0,
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
        .tooltip(Tooltip::new().trigger(Trigger::Axis))
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
                .split_line(SplitLine::new().line_style(LineStyle::new()))
                .axis_label(AxisLabel::new().font_size(10.0)),
        )
        .y_axis(
            Axis::new()
                .type_(AxisType::Value)
                .min(CompositeValue::from(0))
                .split_line(SplitLine::new().line_style(LineStyle::new()))
                .axis_label(AxisLabel::new().font_size(10.0)),
        )
        .series(
            Line::new()
                .data(data)
                .show_symbol(false)
                .smooth(Smoothness::from(0.35f64))
                .line_style(LineStyle::new().width(1.5))
                .area_style(AreaStyle::new()),
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

// Console layout: fixed left rail (nav + status) with the content in sections.
// Two themes via CSS custom properties on <html data-theme="...">:
//   carbon (default dark) — near-black neutrals, single amber accent
//   light                 — porcelain/Swiss, single cobalt accent
// The choice persists in localStorage and seeds from prefers-color-scheme.
//
// The build version is baked into the rail footer at compile time from
// CARGO_PKG_VERSION (sourced from Cargo.toml), so it can never drift from the
// crate version. `concat!` keeps the whole page a single `&'static str`.
const DASHBOARD_HTML: &str = concat!(
    r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<link rel="icon" href="/favicon.ico" type="image/x-icon">
<title>solo-pool-rs</title>
<script src="https://cdn.jsdelivr.net/npm/echarts@5.5.1/dist/echarts.min.js"></script>
<style>
:root {
  --bg: #0a0a0b;
  --surface: #131316;
  --surface2: #1a1a1f;
  --border: #232328;
  --text: #ededef;
  --muted: #8b8b93;
  --accent: #f7931a;
  --ok: #3ecf8e;
  --warn: #e3b341;
  --bad: #f0564a;
  --grid: rgba(255,255,255,0.06);
}
:root[data-theme="light"] {
  --bg: #fafafa;
  --surface: #ffffff;
  --surface2: #f4f4f5;
  --border: #e4e4e7;
  --text: #111113;
  --muted: #71717a;
  --accent: #2456e6;
  --ok: #16a34a;
  --warn: #ca8a04;
  --bad: #dc2626;
  --grid: rgba(0,0,0,0.07);
}
* { box-sizing: border-box; margin: 0; padding: 0; }
html { scroll-behavior: smooth; }
body {
  background: var(--bg); color: var(--text); min-height: 100vh;
  font-family: 'Inter', ui-sans-serif, system-ui, -apple-system, 'Segoe UI', sans-serif;
  font-size: 15px;
}
.shell { display: flex; min-height: 100vh; }

/* ── Left rail ── */
.rail {
  width: 208px; flex: none; position: sticky; top: 0; height: 100vh;
  display: flex; flex-direction: column; gap: 1.4rem;
  padding: 1.2rem 0.85rem; background: var(--surface);
  border-right: 1px solid var(--border);
}
.brand { display: flex; align-items: center; gap: 0.5rem; padding: 0 0.6rem; }
.brand img.mark { height: 1.9rem; width: auto; border-radius: 5px; display: block; }
.brand .name { font-weight: 700; font-size: 0.92rem; letter-spacing: -0.02em; }
nav { display: flex; flex-direction: column; gap: 2px; }
nav a, nav .nav-btn {
  color: var(--muted); text-decoration: none; font-size: 0.8rem; font-weight: 500;
  padding: 0.42rem 0.6rem; border-radius: 5px; border-left: 2px solid transparent;
  font-family: inherit; text-align: left; background: none; border-top: none;
  border-right: none; border-bottom: none; cursor: pointer; width: 100%;
}
nav a:hover, nav .nav-btn:hover { color: var(--text); background: var(--surface2); }
nav a.active { color: var(--text); background: var(--surface2); border-left-color: var(--accent); }
.rail-foot {
  margin-top: auto; display: flex; flex-direction: column; gap: 0.5rem;
  font-size: 0.7rem; color: var(--muted); padding: 0 0.6rem;
  font-variant-numeric: tabular-nums;
}
#theme-toggle {
  align-self: flex-start; cursor: pointer; font: inherit; color: var(--muted);
  background: none; border: 1px solid var(--border); border-radius: 5px;
  padding: 0.3rem 0.6rem;
}
#theme-toggle:hover { color: var(--text); border-color: var(--muted); }
.rail-led { margin-right: 0.4rem; }
.rail-foot a { color: var(--muted); text-decoration: none; }
.rail-foot a:hover { color: var(--text); }

/* ── Main column ── */
main { flex: 1; min-width: 0; max-width: 1240px; padding: 1.7rem 2.1rem 2.5rem; }
section { margin-bottom: 2.4rem; scroll-margin-top: 1.2rem; }
.sec-title {
  font-size: 0.66rem; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.13em; color: var(--muted); margin-bottom: 0.9rem;
}

/* ── Hero ── */
.hero {
  display: flex; flex-wrap: wrap; gap: 1.6rem 2.4rem; align-items: stretch;
  background: var(--surface); border: 1px solid var(--border); border-radius: 8px;
  padding: 1.4rem 1.7rem; margin-bottom: 1.4rem;
}
.hero .label, .kpi .label {
  font-size: 0.62rem; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.11em; color: var(--muted); margin-bottom: 0.4rem;
}
.hero-value {
  font-size: 3.1rem; font-weight: 740; line-height: 1.04; letter-spacing: -0.045em;
  color: var(--accent); font-variant-numeric: tabular-nums;
}
.hero-sub { display: flex; gap: 1.2rem; margin-top: 0.5rem; font-size: 0.76rem; color: var(--muted); font-variant-numeric: tabular-nums; }
.hero-side {
  margin-left: auto; display: flex; flex-direction: column; justify-content: center;
  gap: 0.32rem; padding-left: 2.2rem; border-left: 1px solid var(--border);
  font-size: 0.78rem; font-variant-numeric: tabular-nums;
}
.hero-side .label { margin-bottom: 0.2rem; }

/* ── KPI strip ── */
.kpis {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(158px, 1fr));
  gap: 1.1rem 1.5rem; margin-bottom: 1.4rem;
}
.kpi { border-left: 1px solid var(--border); padding-left: 0.9rem; min-width: 0; }
.kpi .val { font-size: 1.06rem; font-weight: 650; letter-spacing: -0.01em; font-variant-numeric: tabular-nums; }
.kpi .sub { font-size: 0.72rem; color: var(--muted); margin-top: 0.15rem; font-variant-numeric: tabular-nums; }
.kpi .sub.trunc { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ok  { color: var(--ok); }
.bad { color: var(--bad); }
.accent { color: var(--accent); }

/* ── Panels / chart / table ── */
.panel { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 1.15rem 1.3rem; }
.panel-head { display: flex; justify-content: space-between; align-items: center; margin-bottom: 0.7rem; }
.panel-controls { display: flex; align-items: center; gap: 0.7rem; }
#chart-toggle {
  cursor: pointer; font: inherit; font-size: 0.72rem; color: var(--muted);
  background: none; border: 1px solid var(--border); border-radius: 5px;
  padding: 0.22rem 0.45rem;
}
#chart-toggle:hover { color: var(--text); border-color: var(--muted); }
.panel-title { font-size: 0.66rem; font-weight: 600; text-transform: uppercase; letter-spacing: 0.13em; color: var(--muted); }
#timeframe-select {
  font: inherit; font-size: 0.72rem; color: var(--text); background: var(--surface2);
  border: 1px solid var(--border); border-radius: 5px; padding: 0.22rem 0.45rem;
}
#hashrate-chart { height: 260px; width: 100%; }
table { width: 100%; border-collapse: collapse; font-size: 0.84rem; font-variant-numeric: tabular-nums; }
th {
  text-align: left; color: var(--muted); font-weight: 500; padding: 0.34rem 0.55rem;
  border-bottom: 1px solid var(--border); font-size: 0.66rem;
  text-transform: uppercase; letter-spacing: 0.09em; white-space: nowrap;
}
td { padding: 0.5rem 0.55rem; border-bottom: 1px solid var(--grid); white-space: nowrap; }
tr:last-child td { border-bottom: none; }
.empty-row { color: var(--muted); text-align: center; padding: 1.2rem; font-size: 0.84rem; }
/* Worker status LED — green when online, grey when offline. */
.led { width: 9px; height: 9px; border-radius: 50%; display: inline-block; vertical-align: middle; }
.led-on { background: var(--ok); box-shadow: 0 0 5px var(--ok); }
.led-warn { background: var(--warn); box-shadow: 0 0 5px var(--warn); }
.led-off { background: var(--muted); opacity: 0.45; }
.col-led { text-align: center; }
/* New chain tip: pulse the number itself in the accent color (two beats),
   matching the other highlighted values instead of flashing the background. */
@keyframes blockPulse {
  0%   { color: var(--text);   transform: scale(1); }
  15%  { color: var(--accent); transform: scale(1.14); }
  40%  { color: var(--accent); transform: scale(1); }
  55%  { color: var(--accent); transform: scale(1.08); }
  75%  { color: var(--accent); transform: scale(1); }
  100% { color: var(--text);   transform: scale(1); }
}
#v-height.block-new { animation: blockPulse 1.6s ease-in-out; transform-origin: left center; }

/* BTC price tick: pulse just the price digits green/red by direction. */
@keyframes pricePulse { 0% { color: var(--pulse); } 70% { color: var(--pulse); } 100% { color: inherit; } }
#v-btc-price-num.price-up   { --pulse: var(--ok);  animation: pricePulse 1.4s ease-out; }
#v-btc-price-num.price-down { --pulse: var(--bad); animation: pricePulse 1.4s ease-out; }
#pair-select {
  font: inherit; font-size: 0.62rem; color: var(--muted); text-transform: none;
  letter-spacing: normal; background: var(--surface2);
  border: 1px solid var(--border); border-radius: 4px; padding: 0.08rem 0.25rem;
}
/* .kpi .sub sets the muted color at higher specificity; win it back for the
   24h change line. */
.kpi .sub.ok  { color: var(--ok); }
.kpi .sub.bad { color: var(--bad); }

/* ── Settings form / network badge ── */
#net-badge {
  font-size: 0.58rem; font-weight: 700; text-transform: uppercase; letter-spacing: 0.1em;
  color: var(--accent); border: 1px solid var(--accent); border-radius: 4px;
  padding: 0.1rem 0.35rem; margin-left: 0.5rem; align-self: center;
}
.field { margin-bottom: 0.95rem; }
.field label {
  display: block; font-size: 0.62rem; font-weight: 600; text-transform: uppercase;
  letter-spacing: 0.11em; color: var(--muted); margin-bottom: 0.35rem;
}
.field input, .field select {
  font: inherit; font-size: 0.85rem; color: var(--text); background: var(--surface2);
  border: 1px solid var(--border); border-radius: 5px; padding: 0.45rem 0.6rem;
  width: 100%; max-width: 520px; font-variant-numeric: tabular-nums;
}
.field input:focus, .field select:focus { outline: none; border-color: var(--accent); }
#settings-save {
  font: inherit; font-size: 0.8rem; font-weight: 600; cursor: pointer;
  color: var(--bg); background: var(--accent); border: none; border-radius: 5px;
  padding: 0.45rem 1.1rem;
}
#settings-save:disabled { opacity: 0.45; cursor: not-allowed; }
#settings-msg { font-size: 0.76rem; margin-left: 0.8rem; }
.settings-note { font-size: 0.72rem; color: var(--muted); margin-top: 0.9rem; line-height: 1.5; }
.paused-banner {
  margin-top: 0.9rem; padding: 0.6rem 0.8rem; font-size: 0.78rem; font-weight: 600;
  color: var(--bad); border: 1px solid var(--bad); border-radius: 5px;
}

/* ── Settings / Connect modals ── */
#settings-modal, #connect-modal {
  /* Pin to the viewport center — the UA default doesn't reliably vertically
     center a modal <dialog> across browsers. */
  position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); margin: 0;
  width: min(640px, calc(100vw - 2rem)); max-height: calc(100vh - 2rem); overflow: auto;
  border: 1px solid var(--border);
  border-radius: 10px; background: var(--surface); color: var(--text);
  padding: 1.3rem 1.5rem; box-shadow: 0 24px 60px rgba(0,0,0,0.45);
}
#settings-modal::backdrop, #connect-modal::backdrop { background: rgba(0,0,0,0.55); backdrop-filter: blur(2px); }
.copy-btn {
  font: inherit; font-size: 0.8rem; font-weight: 600; cursor: pointer; flex: none;
  color: var(--bg); background: var(--accent); border: none; border-radius: 5px; padding: 0 0.9rem;
}
.connect-ro {
  font-size: 0.85rem; font-variant-numeric: tabular-nums; word-break: break-all;
  background: var(--surface2); border: 1px solid var(--border); border-radius: 5px; padding: 0.45rem 0.6rem;
}
.connect-hints { list-style: none; display: flex; flex-direction: column; gap: 0.5rem; font-size: 0.78rem; color: var(--muted); line-height: 1.5; }
.connect-hints code { background: var(--surface2); padding: 0.05rem 0.3rem; border-radius: 4px; }
.modal-head { display: flex; justify-content: space-between; align-items: center; margin-bottom: 1rem; }
.modal-x {
  font: inherit; font-size: 1.3rem; line-height: 1; cursor: pointer; color: var(--muted);
  background: none; border: none; padding: 0 0.2rem;
}
.modal-x:hover { color: var(--text); }
.modal-actions { display: flex; align-items: center; margin-top: 0.4rem; }

/* Rail "mining paused" pill — keeps the safety state visible while Settings
   lives in a modal. */
#paused-pill {
  display: none; align-items: center; gap: 0.35rem; cursor: pointer;
  font-size: 0.66rem; font-weight: 700; text-transform: uppercase; letter-spacing: 0.06em;
  color: var(--bad); border: 1px solid var(--bad); border-radius: 5px;
  padding: 0.3rem 0.5rem; background: none; font-family: inherit; text-align: left;
}
#paused-pill.show { display: inline-flex; }

/* ── Narrow screens: rail becomes a top bar ── */
@media (max-width: 880px) {
  .shell { flex-direction: column; }
  .rail {
    width: 100%; height: auto; position: static; flex-direction: row;
    align-items: center; gap: 0.9rem; padding: 0.7rem 1rem;
    border-right: none; border-bottom: 1px solid var(--border);
  }
  nav { flex-direction: row; }
  nav a { border-left: none; border-bottom: 2px solid transparent; border-radius: 5px 5px 0 0; }
  nav a.active { border-left-color: transparent; border-bottom-color: var(--accent); }
  .rail-foot { margin-top: 0; margin-left: auto; flex-direction: row; align-items: center; gap: 0.8rem; }
  .rail-foot .hide-sm { display: none; }
  main { padding: 1.2rem 1rem 2rem; }
  .hero-side { margin-left: 0; padding-left: 0; border-left: none; }
}
</style>
</head>
<body>
<div class="shell">

<aside class="rail">
  <div class="brand"><img id="brand-logo" class="mark" src="/logo-dark.svg" alt="solo-pool-rs logo" width="64" height="64"><span class="name">solo-pool-rs</span><span id="net-badge" hidden></span></div>
  <nav id="rail-nav">
    <a href="#overview" data-section="overview" class="active">Overview</a>
    <a href="#workers" data-section="workers">Workers</a>
    <a href="#network" data-section="network">Network</a>
    <button type="button" class="nav-btn" id="open-connect">Connect</button>
    <button type="button" class="nav-btn" id="open-settings">Settings</button>
    <a href="/metrics">Raw metrics &#8599;</a>
  </nav>
  <div class="rail-foot">
    <button id="paused-pill" title="The payout address is not valid for the node's network — open Settings">&#9888; Mining paused</button>
    <button id="theme-toggle" title="Toggle light/dark theme">&#9681; Theme</button>
    <span class="hide-sm"><span id="conn-led" class="led led-off rail-led" title="Connecting&hellip;"></span>Block <span id="rail-height">&mdash;</span></span>
    <span id="server-uptime" title="How long this pool process has been running">Uptime &mdash;</span>
    <span id="last-updated" class="hide-sm">Loading&hellip;</span>
    <span class="hide-sm">v"##,
    env!("CARGO_PKG_VERSION"),
    r##" &middot; <a href="https://github.com/cbyam/solo-pool-rs">source</a></span>
  </div>
</aside>

<main>

<section id="overview">
  <div class="hero">
    <div>
      <div class="label">Pool hashrate &middot; 10m</div>
      <div class="hero-value" id="v-reported-current">&mdash;</div>
      <div class="hero-sub"><span id="v-reported-3h">3h avg: &mdash;</span><span id="v-reported-24h">24h avg: &mdash;</span></div>
    </div>
    <div class="hero-side">
      <div class="label">Block odds</div>
      <span id="v-prob-daily">Daily: &mdash;</span>
      <span id="v-prob-monthly">Monthly: &mdash;</span>
      <span id="v-prob-yearly">Yearly: &mdash;</span>
      <span id="v-prob-powerball" style="color:var(--muted);">vs Powerball: &mdash;</span>
    </div>
  </div>

  <div class="kpis">
    <div class="kpi">
      <div class="label">Miners</div>
      <div class="val" id="v-miners">&mdash;</div>
      <div class="sub"><span id="v-workers-online">Online: &mdash;</span> &middot; <span id="v-workers-degraded">Degraded: &mdash;</span></div>
      <div class="sub" id="v-workers-offline">Offline: &mdash;</div>
    </div>
    <div class="kpi">
      <div class="label">Rejects</div>
      <div class="val" id="v-reject-rate">&mdash;</div>
      <div class="sub" id="v-stale-rate">Stale: &mdash;</div>
    </div>
    <div class="kpi">
      <div class="label">Best share</div>
      <div class="val accent" id="v-best-share">&mdash;</div>
      <div class="sub">session: <span id="v-session-best-share">&mdash;</span> &middot; <span id="v-best-over-network" title="Has the all-time best share met current network difficulty?">&mdash;</span> vs net</div>
    </div>
    <div class="kpi">
      <div class="label">Best hashrate</div>
      <div class="val" id="v-best-hashrate">&mdash;</div>
      <div class="sub">session: <span id="v-session-best-hashrate">&mdash;</span></div>
    </div>
    <div class="kpi">
      <div class="label">Pool uptime</div>
      <div class="val" id="v-uptime">&mdash;</div>
      <div class="sub">found blocks survive restarts</div>
    </div>
    <div class="kpi">
      <div class="label">Last block found</div>
      <div class="val" id="v-last-block-worker">&mdash;</div>
      <div class="sub" id="v-last-block-time">&mdash;</div>
      <div class="sub trunc" id="v-last-block-hash" title="Hash of the last block this pool found">&mdash;</div>
    </div>
  </div>

  <div class="panel">
    <div class="panel-head">
      <div class="panel-title">Hashrate over time <span title="Plots the 10-minute average hashrate, sampled every 10 minutes" style="cursor:help;">&#9432;</span></div>
      <div class="panel-controls">
        <label id="chart-window-label" style="font-size:0.72rem; color:var(--muted);">Window
          <select id="timeframe-select">
            <option value="36h" selected>36h</option>
            <option value="1w">1w</option>
            <option value="1m">1m</option>
            <option value="6m">6m</option>
            <option value="all">all</option>
          </select>
        </label>
        <button id="chart-toggle" title="Hide or show the hashrate chart">Hide</button>
      </div>
    </div>
    <div id="hashrate-chart"></div>
  </div>
</section>

<section id="workers">
  <div class="sec-title">Workers</div>
  <div class="panel">
  <table>
    <thead>
      <tr>
        <th>Worker</th>
        <th class="col-led">Status</th>
        <th>Mode</th>
        <th>Vardiff</th>
        <th>Hashrate (1m)</th>
        <th>Hashrate (3h)</th>
        <th>Hashrate (24h)</th>
        <th>Accepted</th>
        <th>Rejected</th>
        <th>Best Share</th>
        <th>Last Share</th>
        <th>Uptime</th>
      </tr>
    </thead>
    <tbody id="workers-tbody">
      <tr><td colspan="12" class="empty-row">Loading workers&hellip;</td></tr>
    </tbody>
  </table>
  </div>
</section>

<section id="network">
  <div class="sec-title">Network</div>
  <div class="kpis">
    <div class="kpi">
      <div class="label">Network hashrate</div>
      <div class="val" id="v-net-hashrate">&mdash;</div>
      <div class="sub" id="v-net-diff">Diff: &mdash;</div>
    </div>
    <div class="kpi">
      <div class="label">Next adjustment</div>
      <div class="val" id="v-net-next-adj" style="font-size:0.92rem;" title="Estimated time until the next difficulty adjustment (2016-block epochs, ~10 min/block)">&mdash;</div>
      <div class="sub" id="v-net-adj-pct" title="Estimated difficulty change at the next retarget, from actual block timestamps in the current 2016-block epoch. Clamped to the protocol's [-75%, +300%] range.">Est. move: &mdash;</div>
    </div>
    <div class="kpi">
      <div class="label">Chain tip</div>
      <div class="val" id="v-height" title="Height of current best chain tip">&mdash;</div>
      <div class="sub"><span id="v-block-transaction-count">Txs: &mdash;</span></div>
      <div class="sub" id="v-block-reward">Reward: &mdash;</div>
    </div>
    <div class="kpi">
      <div class="label" style="display:flex; justify-content:space-between; align-items:center;">Market
        <select id="pair-select" title="Quote currency">
          <option selected>USD</option>
          <option>EUR</option>
          <option>GBP</option>
          <option>CAD</option>
          <option>AUD</option>
          <option>CHF</option>
          <option>JPY</option>
        </select>
      </div>
      <div class="val" id="v-btc-price" style="font-size:0.92rem;">BTC <span id="v-btc-price-num">&mdash;</span></div>
      <div class="sub" id="v-btc-change">24h: &mdash;</div>
    </div>
  </div>
</section>

<dialog id="settings-modal">
  <div class="modal-head">
    <span class="sec-title" style="margin:0;">Settings</span>
    <button type="button" class="modal-x" id="close-settings" aria-label="Close">&times;</button>
  </div>
  <form id="settings-form">
    <div class="field">
      <label for="set-address">Payout address &mdash; 100% of every block reward goes here</label>
      <input id="set-address" type="text" spellcheck="false" autocomplete="off" placeholder="bc1q&hellip;">
    </div>
    <div class="field">
      <label for="set-network">Network &mdash; detected from the connected node</label>
      <input id="set-network" type="text" disabled>
    </div>
    <div class="modal-actions">
      <button id="settings-save" type="submit">Save</button>
      <span id="settings-msg"></span>
    </div>
  </form>
  <div id="settings-paused" class="paused-banner" hidden>
    Mining is paused: the payout address is not valid for the node&rsquo;s network.
    No jobs are built until a valid address is saved.
  </div>
  <p class="settings-note">
    Saving validates the address against the node&rsquo;s network, then broadcasts
    a clean job so connected miners switch payout immediately. The network is
    read from the Bitcoin node itself and cannot be selected here &mdash; to mine
    a different chain, connect the pool to a node on that chain.
    <span id="settings-persist-note"></span>
  </p>
</dialog>

<dialog id="connect-modal">
  <div class="modal-head">
    <span class="sec-title" style="margin:0;">Connect a miner</span>
    <button type="button" class="modal-x" id="close-connect" aria-label="Close">&times;</button>
  </div>
  <div class="field">
    <label for="connect-url">Point your miner at this address</label>
    <div style="display:flex; gap:0.5rem;">
      <input id="connect-url" type="text" readonly spellcheck="false" value="&mdash;">
      <button type="button" id="connect-copy" class="copy-btn">Copy</button>
    </div>
    <p class="settings-note" id="connect-proto" style="margin-top:0.5rem;"></p>
  </div>
  <div class="field">
    <label>Payout address</label>
    <div class="connect-ro" id="connect-address">&mdash;</div>
    <p class="settings-note" style="margin-top:0.35rem;">Every block reward pays here in full. Change it on the <a href="#" id="connect-to-settings">Settings</a> page.</p>
  </div>
  <div class="field" id="connect-authority-field" hidden>
    <label for="connect-authority">Pool identity (SV2 authority public key)</label>
    <div style="display:flex; gap:0.5rem;">
      <input id="connect-authority" type="text" readonly spellcheck="false" value="&mdash;">
      <button type="button" id="connect-authority-copy" class="copy-btn">Copy</button>
    </div>
    <p class="settings-note" style="margin-top:0.35rem;">Optional: set this as the pool/authority public key on an SV2 miner to verify it is talking to this pool. Miners connect fine without it.</p>
  </div>
  <div class="field">
    <label>Firmware quick start</label>
    <ul class="connect-hints">
      <li><strong>Bitaxe / AxeOS</strong> &amp; multi-chip (NerdQAxe++, Nexus): open the miner&rsquo;s web UI &rarr; set the Stratum URL + port above, any worker name.</li>
      <li><strong>Avalon Nano / Q</strong>: in the Avalon Family app, add a pool using the host and port above.</li>
      <li><strong>cgminer / generic ASIC</strong>: <code>--url stratum+tcp://HOST:PORT</code> with any worker name.</li>
    </ul>
  </div>
  <p class="settings-note">
    solo-pool-rs <span id="connect-version">&mdash;</span> &middot; network <span id="connect-network">&mdash;</span> &middot;
    <a href="https://github.com/cbyam/solo-pool-rs">source</a> &middot;
    <a href="https://github.com/cbyam/solo-pool-rs/issues">support</a> &middot; MIT/Apache-2.0
  </p>
</dialog>

</main>
</div>

<script>
// ── Theme ────────────────────────────────────────────────────────────────────
const THEME_KEY = 'solo-pool-theme';
function currentTheme() { return document.documentElement.dataset.theme === 'light' ? 'light' : 'carbon'; }
function applyTheme(t) {
  if (t === 'light') document.documentElement.dataset.theme = 'light';
  else delete document.documentElement.dataset.theme;
  try { localStorage.setItem(THEME_KEY, t); } catch (_) {}
  const logo = document.getElementById('brand-logo');
  if (logo) logo.src = t === 'light' ? '/logo-light.svg' : '/logo-dark.svg';
}
(function initTheme() {
  let t = null;
  try { t = localStorage.getItem(THEME_KEY); } catch (_) {}
  if (t !== 'light' && t !== 'carbon') {
    t = window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'carbon';
  }
  applyTheme(t);
})();
function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

const DEFAULT_WINDOW = '36h';
let selectedWindow = DEFAULT_WINDOW;
let lastBlockHeight = 0;
// Degraded detection is *relative to each worker's own baseline*, not an absolute
// timeout — so low-hashrate / never-submitted / just-connected miners (whose
// natural share interval is long, or who haven't established one yet) are never
// falsely flagged. A worker is degraded only if it has an established cadence and
// has since gone silent for well beyond it.
const DEGRADED_SECS = 120;        // floor on the silence threshold (fast miners)
const DEGRADED_INTERVALS = 5;     // missed *expected* shares before flagging

// Has the worker established a share cadence we can reason about? Needs to be
// online, to have submitted at least once, and to have a measurable baseline
// hashrate (the 3h window retains the rate even after a recent stall).
function workerBaselineHps(w) {
  return (w.online && w.last_submit_ts > 0) ? (w.hashrate_3h_hps || 0) : 0;
}

function isDegraded(w, nowSec) {
  const baseHps = workerBaselineHps(w);
  if (baseHps <= 0) return false;                     // no baseline → never alarm
  // Expected seconds/share at the worker's own difficulty + baseline hashrate.
  const expected = (w.current_vardiff * 4294967296) / baseHps;
  const silentFor = nowSec - w.last_submit_ts;
  return silentFor > Math.max(DEGRADED_SECS, DEGRADED_INTERVALS * expected);
}

// Classify a worker's status LED: grey (offline) > yellow (online but degraded)
// > green (healthy). The reason rides in the tooltip, not the colour.
function workerLed(w, nowSec) {
  if (!w.online) return { cls: 'led-off', title: 'Offline' };
  if (isDegraded(w, nowSec)) {
    return { cls: 'led-warn', title: 'Degraded — no share in ' + fmtUptime(nowSec - w.last_submit_ts) + ' (well past its usual cadence)' };
  }
  return { cls: 'led-on', title: 'Online' };
}
// Timestamp (ms) of the last successful /stats refresh. Drives the rail
// connectivity LED: green while updates are landing, grey once they go stale.
let lastStatsOk = 0;

function updateConnLed() {
  const led = document.getElementById('conn-led');
  if (!led) return;
  const ageMs = lastStatsOk ? Date.now() - lastStatsOk : Infinity;
  // Refresh runs every 10s; tolerate one missed beat before flagging stale.
  if (ageMs < 25000) {
    led.classList.add('led-on'); led.classList.remove('led-off');
    led.title = 'Live — updated ' + Math.round(ageMs / 1000) + 's ago';
  } else {
    led.classList.add('led-off'); led.classList.remove('led-on');
    led.title = lastStatsOk
      ? 'Connection lost — no update for ' + Math.round(ageMs / 1000) + 's'
      : 'Connecting…';
  }
}

const myChart = echarts.init(document.getElementById('hashrate-chart'), null, { renderer: 'canvas' });
window.addEventListener('resize', () => myChart.resize());

document.getElementById('theme-toggle').addEventListener('click', () => {
  applyTheme(currentTheme() === 'light' ? 'carbon' : 'light');
  if (!chartCollapsed()) loadChart(selectedWindow); // re-skin chart from the new theme's CSS vars
});

// ── Chart collapse toggle ────────────────────────────────────────────────────
// Persisted like the theme choice; while collapsed the periodic chart fetch
// is skipped, and expanding re-fetches so the chart is current immediately.
const CHART_COLLAPSED_KEY = 'chartCollapsed';
function chartCollapsed() {
  try { return localStorage.getItem(CHART_COLLAPSED_KEY) === '1'; } catch (_) { return false; }
}
function applyChartCollapsed(collapsed) {
  try { localStorage.setItem(CHART_COLLAPSED_KEY, collapsed ? '1' : '0'); } catch (_) {}
  document.getElementById('hashrate-chart').style.display = collapsed ? 'none' : '';
  document.getElementById('chart-window-label').style.display = collapsed ? 'none' : '';
  document.getElementById('chart-toggle').textContent = collapsed ? 'Show' : 'Hide';
  if (!collapsed) {
    myChart.resize(); // container was display:none; ECharts needs a re-measure
    loadChart(selectedWindow);
  }
}
document.getElementById('chart-toggle').addEventListener('click', () => {
  applyChartCollapsed(!chartCollapsed());
});

// ── Formatters ───────────────────────────────────────────────────────────────
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

function fmtNextAdjustment(height) {
  if (!height || height <= 0) return '—';
  // Difficulty retargets every 2016 blocks; estimate ~10 min/block.
  const into = height % 2016;
  const blocksLeft = 2016 - into;
  const secs = blocksLeft * 600;
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const eta = d > 0 ? (d + 'd ' + h + 'h') : (h + 'h');
  return '~' + eta + ' (' + blocksLeft + ' blk)';
}

// Estimated difficulty change at the next retarget. Computed on the backend from
// accurate epoch block timestamps (rpc.estimate_difficulty_change_pct); null
// until first polled or right after a retarget.
function fmtAdjustmentPct(pct) {
  if (pct === null || pct === undefined || !isFinite(pct)) {
    return { text: '—', color: 'var(--muted)' };
  }
  const sign = pct >= 0 ? '+' : '';
  // Difficulty up = harder for miners (red), down = easier (green).
  const color = pct > 0.05 ? 'var(--bad)' : (pct < -0.05 ? 'var(--ok)' : 'var(--muted)');
  return { text: sign + pct.toFixed(2) + '%', color };
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

// ── Chart ────────────────────────────────────────────────────────────────────
async function loadChart(window) {
  try {
    const resp = await fetch('/chart?window=' + window);
    if (!resp.ok) return;
    const options = await resp.json();
    // Skin the server-built option object from the active theme's CSS vars,
    // and patch in JS formatter callbacks that cannot be serialised from Rust.
    const accent = cssVar('--accent'), muted = cssVar('--muted'),
          grid = cssVar('--grid'), surface = cssVar('--surface2'),
          border = cssVar('--border'), text = cssVar('--text');
    const yAxis = Array.isArray(options.yAxis) ? options.yAxis[0] : options.yAxis;
    if (yAxis) {
      yAxis.axisLabel = Object.assign(yAxis.axisLabel || {}, { color: muted, formatter: v => fmtHr(v, true) });
      yAxis.splitLine = { lineStyle: { color: grid } };
    }
    const xAxis = Array.isArray(options.xAxis) ? options.xAxis[0] : options.xAxis;
    if (xAxis) {
      xAxis.splitLine = { lineStyle: { color: grid } };
      xAxis.axisLabel = Object.assign(xAxis.axisLabel || {}, {
        color: muted,
        formatter: v => {
          const d = new Date(v);
          if (d.getHours() === 0 && d.getMinutes() === 0) {
            return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
          }
          return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hourCycle: 'h23' });
        }
      });
    }
    const series = Array.isArray(options.series) ? options.series[0] : options.series;
    if (series) {
      series.lineStyle = Object.assign(series.lineStyle || {}, { color: accent });
      series.areaStyle = { color: accent + '17' }; // ~9% alpha hex suffix
    }
    if (options.tooltip) {
      options.tooltip.backgroundColor = surface;
      options.tooltip.borderColor = border;
      options.tooltip.textStyle = { color: text, fontSize: 12 };
      options.tooltip.formatter = params => {
        if (!params || !params.length) return '';
        const pt = params[0];
        const ts = Array.isArray(pt.value) ? pt.value[0] : pt.value;
        const hps = Array.isArray(pt.value) ? pt.value[1] : 0;
        const date = new Date(ts).toLocaleString([], { year: 'numeric', month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit' });
        return date + '<br/><span style="color:' + accent + '">Hashrate (10m)</span>: ' + fmtHr(hps, false);
      };
    }
    myChart.setOption(options, true);
  } catch (e) {
    console.error('Chart fetch error:', e);
  }
}

// ── Stats refresh ────────────────────────────────────────────────────────────
async function refresh() {
  try {
    const resp = await fetch('/stats');
    if (!resp.ok) return;
    const d = await resp.json();

    const reported10m = d.total_hashrate_10m || 0;
    const reported3h  = d.total_hashrate_3h  || 0;

    document.getElementById('v-reported-current').textContent = fmtHr(reported10m, false);
    document.getElementById('v-reported-3h').textContent = '3h avg: ' + fmtHr(reported3h, false);
    document.getElementById('v-reported-24h').textContent = '24h avg: ' + fmtHr(d.total_hashrate_24h || 0, false);

    updateProbability(d.total_hashrate_10m || 0, d.network_hashrate_hps || 0);

    document.getElementById('v-miners').textContent = d.connected_miners;

    // Flash on new block height
    if (d.current_height !== lastBlockHeight) {
      const heightEl = document.getElementById('v-height');
      heightEl.classList.remove('block-new');
      // Trigger reflow to restart animation
      void heightEl.offsetWidth;
      heightEl.classList.add('block-new');
      lastBlockHeight = d.current_height;
    }
    document.getElementById('v-height').textContent = d.current_height.toLocaleString();
    document.getElementById('rail-height').textContent = d.current_height.toLocaleString();
    if (d.current_block_transaction_count != null) {
      document.getElementById('v-block-transaction-count').textContent = 'Txs: ' + d.current_block_transaction_count.toLocaleString();
    }
    if (d.current_coinbase_value) {
      const btc = d.current_coinbase_value / 1e8;
      document.getElementById('v-block-reward').textContent = 'Reward: ' + btc.toFixed(8) + ' BTC';
    }
    document.getElementById('v-last-block-worker').textContent = d.last_block_worker || '—';
    document.getElementById('v-last-block-hash').textContent = d.last_block_hash || '—';
    document.getElementById('v-last-block-time').textContent = fmtTimestamp(d.last_block_ts);
    document.getElementById('v-best-share').textContent = fmtDiff(d.best_share_difficulty);
    document.getElementById('v-session-best-share').textContent = fmtDiff(d.session_best_share_difficulty);
    document.getElementById('v-best-over-network').textContent = d.best_share_difficulty >= Math.ceil(d.network_difficulty) ? 'YES' : 'no';

    // Network section (human-readable hashrate + difficulty + next-adjustment ETA)
    document.getElementById('v-net-hashrate').textContent = fmtHr(d.network_hashrate_hps || 0, false);
    document.getElementById('v-net-diff').textContent = 'Diff: ' + fmtDiff(d.network_difficulty || 0);
    document.getElementById('v-net-next-adj').textContent = fmtNextAdjustment(d.current_height || 0);
    const adj = fmtAdjustmentPct(d.est_difficulty_change_pct);
    const adjEl = document.getElementById('v-net-adj-pct');
    adjEl.textContent = 'Est. move: ' + adj.text;
    adjEl.style.color = adj.color;
    document.getElementById('v-session-best-hashrate').textContent = fmtHr(d.session_best_hashrate_hps, false);
    document.getElementById('v-best-hashrate').textContent = fmtHr(d.best_hashrate_hps, false);
    document.getElementById('v-uptime').textContent = fmtUptime(d.uptime_secs);
    document.getElementById('server-uptime').textContent = 'Uptime ' + fmtUptime(d.uptime_secs);

    const total = d.shares_accepted + d.shares_rejected;
    const rejectPct = total > 0 ? (d.shares_rejected / total * 100).toFixed(1) : '0.0';
    const staleTotal = Array.isArray(d.worker_states) ? d.worker_states.reduce((sum, w) => sum + (w.shares_stale || 0), 0) : 0;
    const stalePct = total > 0 ? (staleTotal / total * 100).toFixed(1) : '0.0';
    const reasonTotals = {};
    (Array.isArray(d.worker_states) ? d.worker_states : []).forEach(w => {
      Object.entries(w.reject_reasons || {}).forEach(([r, n]) => {
        reasonTotals[r] = (reasonTotals[r] || 0) + n;
      });
    });
    const otherReasons = Object.entries(reasonTotals)
      .filter(([r, n]) => r !== 'stale' && n > 0)
      .sort((a, b) => b[1] - a[1])
      .map(([r, n]) => `${rejectLabel(r)}: ${n.toLocaleString()}`)
      .join(' · ');

    document.getElementById('v-reject-rate').textContent = `${d.shares_rejected.toLocaleString()} (${rejectPct}%)`;
    document.getElementById('v-stale-rate').textContent =
      `Stale: ${staleTotal.toLocaleString()} (${stalePct}%)` + (otherReasons ? ` · ${otherReasons}` : '');

    const workers = Array.isArray(d.worker_states) ? d.worker_states : [];
    const onlineCount = workers.filter(w => w.online).length;
    const offlineCount = workers.filter(w => !w.online).length;
    const nowSecKpi = Math.floor(Date.now() / 1000);
    const degradedCount = workers.filter(w => isDegraded(w, nowSecKpi)).length;

    document.getElementById('v-workers-online').textContent = 'Online: ' + onlineCount;
    document.getElementById('v-workers-offline').textContent = 'Offline: ' + offlineCount;
    document.getElementById('v-workers-degraded').textContent = 'Degraded: ' + degradedCount;

    // Workers table
    const tbody = document.getElementById('workers-tbody');
    if (workers.length === 0) {
      tbody.innerHTML = '<tr><td colspan="12" class="empty-row">No connected workers</td></tr>';
    } else {
      tbody.innerHTML = workers
        .sort((a, b) => b.hashrate_60s_hps - a.hashrate_60s_hps)
        .map(w => {
          const workerName = w.worker.includes('.') ? w.worker.split('.')[1] : w.worker;
          const nowSec = Math.floor(Date.now() / 1000);
          const lastShareAgo = w.last_submit_ts > 0 ? fmtUptime(nowSec - w.last_submit_ts) : '—';
          const uptime = w.connected_ts > 0 ? fmtUptime(nowSec - w.connected_ts) : '—';
          const mode = (w.protocol || 'sv1').toUpperCase();
          const led = workerLed(w, nowSec);
          return `<tr>
            <td>${escHtml(workerName)}</td>
            <td class="col-led"><span class="led ${led.cls}" title="${led.title}"></span></td>
            <td>${mode}</td>
            <td>${fmtDiff(w.current_vardiff)}</td>
            <td>${fmtHr(w.hashrate_60s_hps, false)}</td>
            <td>${fmtHr(w.hashrate_3h_hps, false)}</td>
            <td>${fmtHr(w.hashrate_24h_hps, false)}</td>
            <td>${w.shares_accepted.toLocaleString()}</td>
            <td title="${rejectBreakdown(w)}">${w.shares_rejected.toLocaleString()}</td>
            <td>${fmtDiff(w.best_share_difficulty)}</td>
            <td>${lastShareAgo}</td>
            <td>${uptime}</td>
          </tr>`;
        })
        .join('');
    }

    document.getElementById('last-updated').textContent = 'Updated ' + new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23' });
    lastStatsOk = Date.now();
  } catch (e) {
    console.error('Dashboard refresh error:', e);
  }
  updateConnLed();
}

const REJECT_LABELS = {
  stale: 'Stale',
  duplicate: 'Duplicate',
  low_difficulty: 'Low diff',
  job_not_found: 'Unknown job',
  bad_extranonce: 'Bad extranonce',
  invalid: 'Invalid',
};

function rejectLabel(reason) {
  return REJECT_LABELS[reason] || reason;
}

function rejectBreakdown(w) {
  const parts = Object.entries(w.reject_reasons || {})
    .filter(([, n]) => n > 0)
    .sort((a, b) => b[1] - a[1])
    .map(([r, n]) => `${rejectLabel(r)}: ${n.toLocaleString()}`);
  return parts.length ? parts.join(', ') : 'No rejects';
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

const PAIR_KEY = 'btcPair';
const PAIRS = ['USD', 'EUR', 'GBP', 'CAD', 'AUD', 'CHF', 'JPY'];
function currentPair() {
  try {
    const p = localStorage.getItem(PAIR_KEY);
    return PAIRS.includes(p) ? p : 'USD';
  } catch (_) { return 'USD'; }
}

let lastBtcPrice = null;
async function fetchBtcPrice() {
  const pair = currentPair();
  try {
    const vs = pair.toLowerCase();
    const resp = await fetch('https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=' + vs + '&include_24hr_change=true');
    if (!resp.ok) return;
    const data = await resp.json();
    if (pair !== currentPair()) return; // pair switched while the fetch was in flight
    const price = data?.bitcoin?.[vs];
    const change = data?.bitcoin?.[vs + '_24h_change'];
    if (price != null) {
      const el = document.getElementById('v-btc-price-num');
      el.textContent = new Intl.NumberFormat(undefined, {
        style: 'currency', currency: pair, maximumFractionDigits: 0,
      }).format(price);
      if (lastBtcPrice != null && price !== lastBtcPrice) {
        el.classList.remove('price-up', 'price-down');
        void el.offsetWidth; // restart the animation
        el.classList.add(price > lastBtcPrice ? 'price-up' : 'price-down');
      }
      lastBtcPrice = price;
    }
    if (change != null) {
      const chEl = document.getElementById('v-btc-change');
      chEl.textContent = (change >= 0 ? '+' : '') + change.toFixed(1) + '% (24h)';
      chEl.classList.remove('ok', 'bad');
      chEl.classList.add(change >= 0 ? 'ok' : 'bad');
    }
  } catch (_) {}
}

const pairSelect = document.getElementById('pair-select');
pairSelect.value = currentPair();
pairSelect.addEventListener('change', () => {
  try { localStorage.setItem(PAIR_KEY, pairSelect.value); } catch (_) {}
  lastBtcPrice = null; // a currency switch is not a price move; don't pulse
  fetchBtcPrice();
});

// ── Settings page ────────────────────────────────────────────────────────────
function updateNetBadge(network) {
  const badge = document.getElementById('net-badge');
  if (network && network !== 'mainnet') {
    badge.textContent = network;
    badge.hidden = false;
  } else {
    badge.hidden = true;
  }
}

async function loadSettings() {
  try {
    const resp = await fetch('/api/settings');
    if (!resp.ok) return;
    const s = await resp.json();
    document.getElementById('set-address').value = s.coinbase_address;
    document.getElementById('set-network').value = s.network;
    updateNetBadge(s.network);
    document.getElementById('settings-paused').hidden = s.address_valid;
    document.getElementById('paused-pill').classList.toggle('show', !s.address_valid);
    const note = document.getElementById('settings-persist-note');
    if (!s.editable) {
      document.getElementById('set-address').disabled = true;
      document.getElementById('settings-save').disabled = true;
      note.textContent = 'Editing is disabled ([metrics] allow_runtime_settings = false).';
    } else if (!s.persisted) {
      note.textContent = 'No stats database is configured, so changes apply until the next restart only.';
    }
  } catch (e) {
    console.error('Settings fetch error:', e);
  }
}

document.getElementById('settings-form').addEventListener('submit', async ev => {
  ev.preventDefault();
  const msg = document.getElementById('settings-msg');
  const btn = document.getElementById('settings-save');
  btn.disabled = true;
  msg.textContent = 'Saving…';
  msg.style.color = 'var(--muted)';
  try {
    const resp = await fetch('/api/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        coinbase_address: document.getElementById('set-address').value.trim(),
      }),
    });
    const body = await resp.json();
    if (resp.ok && body.ok) {
      msg.textContent = body.persisted
        ? 'Saved — new jobs pay this address.'
        : 'Applied (not persisted: no stats DB) — new jobs pay this address.';
      msg.style.color = 'var(--ok)';
      document.getElementById('settings-paused').hidden = true;
      document.getElementById('paused-pill').classList.remove('show');
    } else {
      msg.textContent = body.error || ('Save failed (HTTP ' + resp.status + ')');
      msg.style.color = 'var(--bad)';
    }
  } catch (e) {
    msg.textContent = 'Save failed: ' + e;
    msg.style.color = 'var(--bad)';
  } finally {
    btn.disabled = false;
  }
});

// ── Scroll spy for the rail nav ──────────────────────────────────────────────
// Position-based: the active link is the last section whose top has scrolled
// above a threshold near the top of the viewport. Robust for short sections and
// the final section (which an IntersectionObserver band can miss).
const navLinks = Array.from(document.querySelectorAll('#rail-nav a[data-section]'));
function updateActiveNav() {
  let current = navLinks[0];
  for (const link of navLinks) {
    const sec = document.getElementById(link.dataset.section);
    if (sec && sec.getBoundingClientRect().top <= 140) current = link;
  }
  navLinks.forEach(l => l.classList.toggle('active', l === current));
}
window.addEventListener('scroll', updateActiveNav, { passive: true });
window.addEventListener('resize', updateActiveNav);
// Immediate feedback on click (before the smooth-scroll settles).
navLinks.forEach(l => l.addEventListener('click', () => {
  navLinks.forEach(x => x.classList.remove('active'));
  l.classList.add('active');
}));
updateActiveNav();

// ── Settings modal ───────────────────────────────────────────────────────────
const settingsModal = document.getElementById('settings-modal');
function openSettings() { loadSettings(); settingsModal.showModal(); }
document.getElementById('open-settings').addEventListener('click', openSettings);
document.getElementById('paused-pill').addEventListener('click', openSettings);
document.getElementById('close-settings').addEventListener('click', () => settingsModal.close());
// Click on the backdrop (outside the dialog content) closes it.
settingsModal.addEventListener('click', e => { if (e.target === settingsModal) settingsModal.close(); });

// ── Connect modal ─────────────────────────────────────────────────────────────
const connectModal = document.getElementById('connect-modal');
async function openConnect() {
  try {
    const resp = await fetch('/api/info');
    if (resp.ok) {
      const i = await resp.json();
      const host = window.location.hostname || 'your-pool-host';
      document.getElementById('connect-url').value = 'stratum+tcp://' + host + ':' + i.stratum_port;
      document.getElementById('connect-proto').textContent = i.sv2_enabled
        ? 'Stratum V1 and V2 (Noise-encrypted) are auto-detected on this one port — point any miner here.'
        : 'Stratum V1 on this port (SV2 is disabled in this pool’s config).';
      document.getElementById('connect-address').textContent = i.coinbase_address || '—';
      document.getElementById('connect-authority-field').hidden = !i.sv2_authority_pubkey;
      document.getElementById('connect-authority').value = i.sv2_authority_pubkey || '—';
      document.getElementById('connect-version').textContent = 'v' + i.version;
      document.getElementById('connect-network').textContent = i.network;
    }
  } catch (e) { console.error('Info fetch error:', e); }
  connectModal.showModal();
}
document.getElementById('open-connect').addEventListener('click', openConnect);
document.getElementById('close-connect').addEventListener('click', () => connectModal.close());
connectModal.addEventListener('click', e => { if (e.target === connectModal) connectModal.close(); });
function wireCopy(inputId, btnId) {
  document.getElementById(btnId).addEventListener('click', () => {
    const el = document.getElementById(inputId);
    const btn = document.getElementById(btnId);
    const done = ok => { const p = btn.textContent; btn.textContent = ok ? 'Copied' : 'Copy manually'; setTimeout(() => btn.textContent = p, 1500); };
    // navigator.clipboard only exists in secure contexts (HTTPS/localhost);
    // this dashboard is usually plain HTTP on the LAN, so fall back to
    // selecting the text and execCommand('copy'). The selection is left in
    // place so a manual Ctrl/Cmd+C works if even that fails.
    const legacy = () => {
      el.focus(); el.select(); el.setSelectionRange(0, el.value.length);
      let ok = false;
      try { ok = document.execCommand('copy'); } catch (e) { ok = false; }
      done(ok);
    };
    if (navigator.clipboard && window.isSecureContext) navigator.clipboard.writeText(el.value).then(() => done(true)).catch(legacy);
    else legacy();
  });
}
wireCopy('connect-url', 'connect-copy');
wireCopy('connect-authority', 'connect-authority-copy');
// Jump from Connect → Settings to edit the payout address.
document.getElementById('connect-to-settings').addEventListener('click', e => {
  e.preventDefault(); connectModal.close(); openSettings();
});

attachTimeframeSelector();
if (chartCollapsed()) {
  applyChartCollapsed(true);
} else {
  loadChart(DEFAULT_WINDOW);
}
updateConnLed();
refresh();
loadSettings();
fetchBtcPrice();
setInterval(refresh, 10000);
// Re-evaluate the connectivity LED between refreshes so it goes stale on its
// own even if refresh() stops landing (server down, tab throttled, etc.).
setInterval(updateConnLed, 5000);
setInterval(() => { if (!chartCollapsed()) loadChart(selectedWindow); }, 60000);
setInterval(fetchBtcPrice, 60000);
</script>
</body>
</html>"##
);

#[cfg(test)]
mod tests {
    use super::DASHBOARD_HTML;
    use std::collections::HashSet;

    /// Every element id the embedded JS looks up must exist in the markup —
    /// a missing one throws inside refresh() and kills the whole update loop.
    #[test]
    fn all_ids_referenced_by_js_exist_in_markup() {
        let mut wanted = HashSet::new();
        // Direct lookups, plus updateProbability's `el('…')` helper shorthand.
        for pat in ["getElementById('", "el('"] {
            for (idx, _) in DASHBOARD_HTML.match_indices(pat) {
                let rest = &DASHBOARD_HTML[idx + pat.len()..];
                if let Some(end) = rest.find('\'') {
                    wanted.insert(&rest[..end]);
                }
            }
        }
        // Sanity: the scrape itself worked.
        assert!(wanted.len() > 20, "id scrape found too few: {wanted:?}");

        let missing: Vec<&&str> = wanted
            .iter()
            .filter(|id| !DASHBOARD_HTML.contains(&format!("id=\"{id}\"")))
            .collect();
        assert!(
            missing.is_empty(),
            "ids referenced by JS but absent from markup: {missing:?}"
        );
    }
}
