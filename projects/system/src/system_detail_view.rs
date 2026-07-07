//! `system.detail_view` — chart-ready typed point series for the detail page.
//!
//! Slice S5 moved chart-segmentation math off the client. The host detail
//! page used to read raw `SystemHistoryPoint`s out of `system.detail` and
//! run `chartSegments`/`xAxisLabels`/`relTime` in TypeScript to produce SVG
//! paths. This tool returns pre-scaled `(x,y)` points plus gap indices, so
//! the SvelteKit UI and native iOS/Android clients all render the same
//! data with their own primitives (5 lines of `M x y L x y` per segment).
//!
//! The raw `system.detail` tool stays — CLI / automation callers still get
//! the unmassaged history. This tool is purely a view-projection sibling.
//!
//! `width`/`height` are required client-supplied dimensions; the server
//! scales `(timestamp, value)` into the client's SVG-space so the wire
//! format is render-ready. Native clients pass their own dimensions.

use crate::system_info::history;
use crate::system_info_types::SystemHistoryPoint;
use derive::orca_tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Tail length matching `SystemInfoReport.history` (≈1h @ 5s cadence). Keeps
/// the detail view bounded regardless of how big the on-disk ring grew.
const DEFAULT_POINTS: usize = 720;

/// If two consecutive samples are this far apart (seconds), the renderer
/// breaks the path between them. Picked at 4× the 2s refresh cadence so a
/// single dropped tick is still a connected segment but a multi-second
/// stall reads as a gap. Mirrors the implicit threshold the old TS path
/// had when samples were sparse.
const GAP_THRESHOLD_SECS: i64 = 8;

#[derive(clap::Args, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct SystemDetailViewArgs {
    /// Peer to fetch the chart series for. Unused locally — peer dispatch
    /// is handled by the transport layer (`X-Orca-Peer` header / mesh
    /// routing); the daemon on the target host reads its own history ring.
    /// Kept on the args struct so the tool surfaces a `peer` flag like its
    /// siblings.
    #[arg(long)]
    pub peer_id: Option<String>,
    /// Tail length to read. Defaults to ≈1h of samples.
    #[arg(long)]
    pub points: Option<usize>,
    /// SVG-space width to scale `x` against.
    #[arg(long)]
    pub width: u32,
    /// SVG-space height to scale `y` against.
    #[arg(long)]
    pub height: u32,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ChartPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct ChartSeries {
    /// Pre-scaled SVG-space points, ordered oldest → newest.
    pub points: Vec<ChartPoint>,
    /// Indices into `points` where a gap exists. `gaps[i] = N` means the
    /// renderer should break the path between `points[N-1]` and `points[N]`.
    pub gaps: Vec<usize>,
    /// Evenly-spaced relative-time labels for the x axis (e.g.
    /// `["-5m", "-3m", "-1m", "now"]`). Length 0 when the series has fewer
    /// than 2 points.
    pub x_axis_labels: Vec<String>,
    /// Y-axis ceiling used for scaling. Clamped to a floor of 1.0 to avoid
    /// div-by-zero on flat-zero series.
    pub vmax: f32,
    /// Most recent raw value (pre-scaling). `None` when the series is empty
    /// or every sample lacked this metric. Lets the client render a
    /// "current value" label without re-fetching the raw history.
    pub last_value: Option<f32>,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct GpuSeries {
    pub name: String,
    pub utilization: ChartSeries,
    pub memory: ChartSeries,
}

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug)]
pub struct SystemDetailView {
    pub cpu: ChartSeries,
    pub mem: ChartSeries,
    /// 1-min load average normalised to a 0–100% scale. Empty series until
    /// the history ring starts retaining a load field — `SystemHistoryPoint`
    /// does not record it today, so the points vector is always empty here.
    pub load: ChartSeries,
    pub gpus: Vec<GpuSeries>,
    pub samples_count: usize,
    /// Time span between earliest and latest sample (seconds). Zero when the
    /// series has fewer than 2 points.
    pub window_secs: i64,
}

/// Chart-ready typed series for the host detail page. The daemon on the
/// target peer reads its local history ring, projects each metric into
/// SVG-space using the caller's `width`/`height`, and emits a flat,
/// natively-renderable shape.
#[orca_tool(domain = "system", verb = "detail_view")]
async fn system_chart_view(
    args: SystemDetailViewArgs,
    _ctx: &contract::ToolCtx,
) -> anyhow::Result<SystemDetailView> {
    let n = args.points.unwrap_or(DEFAULT_POINTS);
    let history = history::read_tail(n);
    Ok(build_view(&history, args.width, args.height))
}

fn build_view(history: &[SystemHistoryPoint], width: u32, height: u32) -> SystemDetailView {
    let samples_count = history.len();
    let window_secs = match (history.first(), history.last()) {
        (Some(a), Some(b)) if samples_count >= 2 => b.ts - a.ts,
        _ => 0,
    };

    let cpu_pairs: Vec<(i64, f32)> = history
        .iter()
        .filter_map(|p| p.cpu_percent.map(|v| (p.ts, v)))
        .collect();
    let mem_pairs: Vec<(i64, f32)> = history
        .iter()
        .filter_map(|p| match (p.mem_used_mb, p.mem_total_mb) {
            (Some(used), Some(total)) if total > 0 => {
                Some((p.ts, (used as f32 / total as f32) * 100.0))
            }
            _ => None,
        })
        .collect();

    let mut gpu_names: Vec<String> = Vec::new();
    for p in history {
        for g in &p.gpus {
            if !gpu_names.iter().any(|n| n == &g.name) {
                gpu_names.push(g.name.clone());
            }
        }
    }
    let gpus: Vec<GpuSeries> = gpu_names
        .into_iter()
        .map(|name| {
            let util_pairs: Vec<(i64, f32)> = history
                .iter()
                .filter_map(|p| {
                    p.gpus
                        .iter()
                        .find(|g| g.name == name)
                        .and_then(|g| g.utilization_percent.map(|v| (p.ts, v)))
                })
                .collect();
            let mem_pairs: Vec<(i64, f32)> = history
                .iter()
                .filter_map(|p| {
                    p.gpus.iter().find(|g| g.name == name).and_then(|g| {
                        match (g.vram_used_mb, g.vram_total_mb) {
                            (Some(used), Some(total)) if total > 0 => {
                                Some((p.ts, (used as f32 / total as f32) * 100.0))
                            }
                            _ => None,
                        }
                    })
                })
                .collect();
            GpuSeries {
                name,
                utilization: series_from_pairs(&util_pairs, history, width, height, 100.0),
                memory: series_from_pairs(&mem_pairs, history, width, height, 100.0),
            }
        })
        .collect();

    SystemDetailView {
        cpu: series_from_pairs(&cpu_pairs, history, width, height, 100.0),
        mem: series_from_pairs(&mem_pairs, history, width, height, 100.0),
        load: empty_series(),
        gpus,
        samples_count,
        window_secs,
    }
}

fn empty_series() -> ChartSeries {
    ChartSeries {
        points: Vec::new(),
        gaps: Vec::new(),
        x_axis_labels: Vec::new(),
        vmax: 1.0,
        last_value: None,
    }
}

/// Project `(timestamp, value)` pairs into SVG-space points. The x-axis is
/// scaled across the full history window (earliest→latest sample timestamp)
/// rather than the pairs' own span, so metrics that started reporting late
/// land at the correct horizontal position instead of being stretched to
/// fill the chart.
fn series_from_pairs(
    pairs: &[(i64, f32)],
    history: &[SystemHistoryPoint],
    width: u32,
    height: u32,
    vmax_floor: f32,
) -> ChartSeries {
    if pairs.is_empty() {
        return empty_series();
    }
    let w = width as f32;
    let h = height as f32;

    let earliest = history.first().map(|p| p.ts).unwrap_or(pairs[0].0);
    let latest = history
        .last()
        .map(|p| p.ts)
        .unwrap_or(pairs[pairs.len() - 1].0);
    let span = (latest - earliest).max(1) as f32;

    let mut vmax: f32 = vmax_floor;
    for (_, v) in pairs {
        if *v > vmax {
            vmax = *v;
        }
    }

    let points: Vec<ChartPoint> = pairs
        .iter()
        .map(|(ts, v)| {
            let x = if pairs.len() == 1 {
                w
            } else {
                ((*ts - earliest) as f32 / span) * w
            };
            let clamped = v.clamp(0.0, vmax);
            let y = h - (clamped / vmax) * h;
            ChartPoint { x, y }
        })
        .collect();

    let mut gaps: Vec<usize> = Vec::new();
    for i in 1..pairs.len() {
        if pairs[i].0 - pairs[i - 1].0 > GAP_THRESHOLD_SECS {
            gaps.push(i);
        }
    }

    let x_axis_labels = x_axis_labels(history);
    let last_value = pairs.last().map(|(_, v)| *v);

    ChartSeries {
        points,
        gaps,
        x_axis_labels,
        vmax,
        last_value,
    }
}

/// Produce 3 evenly-spaced relative-time labels (`"-5m"`, `"-2m"`, `"now"`)
/// against the latest sample in `history`. Matches the old TS `relTime`
/// + `xAxisLabels` pair.
fn x_axis_labels(history: &[SystemHistoryPoint]) -> Vec<String> {
    if history.len() < 2 {
        return Vec::new();
    }
    let now = history[history.len() - 1].ts;
    let first = history[0].ts;
    let mid = history[history.len() / 2].ts;
    vec![rel_time(first, now), rel_time(mid, now), "now".to_string()]
}

fn rel_time(target_ts: i64, now_ts: i64) -> String {
    let dt = now_ts - target_ts;
    if dt < 5 {
        return "now".to_string();
    }
    if dt < 60 {
        return format!("-{dt}s");
    }
    if dt < 3600 {
        return format!("-{}m", (dt as f32 / 60.0).round() as i64);
    }
    format!("-{:.1}h", dt as f32 / 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_info_types::GpuPoint;

    fn pt(ts: i64, cpu: Option<f32>) -> SystemHistoryPoint {
        SystemHistoryPoint {
            ts,
            cpu_percent: cpu,
            mem_used_mb: None,
            mem_total_mb: None,
            process_rss_mb: None,
            gpus: Vec::new(),
        }
    }

    #[test]
    fn empty_history_yields_empty_series() {
        let view = build_view(&[], 800, 120);
        assert!(view.cpu.points.is_empty());
        assert!(view.cpu.gaps.is_empty());
        assert!(view.cpu.x_axis_labels.is_empty());
        assert_eq!(view.samples_count, 0);
        assert_eq!(view.window_secs, 0);
        assert_eq!(view.cpu.last_value, None);
    }

    #[test]
    fn single_sample_lands_at_right_edge() {
        let history = vec![pt(100, Some(42.0))];
        let view = build_view(&history, 800, 120);
        assert_eq!(view.cpu.points.len(), 1);
        assert!((view.cpu.points[0].x - 800.0).abs() < 0.01);
        assert!(view.cpu.gaps.is_empty());
        assert_eq!(view.cpu.last_value, Some(42.0));
    }

    #[test]
    fn two_contiguous_samples_have_no_gap() {
        let history = vec![pt(100, Some(10.0)), pt(102, Some(20.0))];
        let view = build_view(&history, 800, 120);
        assert_eq!(view.cpu.points.len(), 2);
        assert!(view.cpu.gaps.is_empty());
        assert!((view.cpu.points[0].x - 0.0).abs() < 0.01);
        assert!((view.cpu.points[1].x - 800.0).abs() < 0.01);
    }

    #[test]
    fn time_gap_marks_break() {
        let history = vec![pt(100, Some(10.0)), pt(200, Some(20.0))];
        let view = build_view(&history, 800, 120);
        assert_eq!(view.cpu.points.len(), 2);
        assert_eq!(view.cpu.gaps, vec![1]);
    }

    #[test]
    fn vmax_derives_from_series_with_floor() {
        let history = vec![pt(0, Some(5.0)), pt(2, Some(15.0))];
        let view = build_view(&history, 100, 100);
        // Values <= 100 keep the 100.0 floor.
        assert!((view.cpu.vmax - 100.0).abs() < 0.01);

        let history = vec![pt(0, Some(150.0)), pt(2, Some(200.0))];
        let view = build_view(&history, 100, 100);
        assert!((view.cpu.vmax - 200.0).abs() < 0.01);
    }

    #[test]
    fn multi_gpu_history_emits_per_gpu_series() {
        let mk = |ts: i64, a: f32, b: f32| SystemHistoryPoint {
            ts,
            cpu_percent: None,
            mem_used_mb: None,
            mem_total_mb: None,
            process_rss_mb: None,
            gpus: vec![
                GpuPoint {
                    name: "A".into(),
                    utilization_percent: Some(a),
                    vram_used_mb: Some(1024),
                    vram_total_mb: Some(8192),
                    temperature_c: None,
                },
                GpuPoint {
                    name: "B".into(),
                    utilization_percent: Some(b),
                    vram_used_mb: Some(2048),
                    vram_total_mb: Some(8192),
                    temperature_c: None,
                },
            ],
        };
        let history = vec![mk(0, 10.0, 20.0), mk(2, 15.0, 25.0)];
        let view = build_view(&history, 200, 100);
        assert_eq!(view.gpus.len(), 2);
        assert_eq!(view.gpus[0].name, "A");
        assert_eq!(view.gpus[1].name, "B");
        assert_eq!(view.gpus[0].utilization.points.len(), 2);
        assert_eq!(view.gpus[0].memory.points.len(), 2);
        assert_eq!(view.gpus[0].utilization.last_value, Some(15.0));
        assert_eq!(view.gpus[1].utilization.last_value, Some(25.0));
    }

    #[test]
    fn x_axis_labels_count() {
        let history = vec![pt(0, Some(10.0)), pt(60, Some(20.0)), pt(120, Some(30.0))];
        let view = build_view(&history, 100, 100);
        assert_eq!(view.cpu.x_axis_labels.len(), 3);
        assert_eq!(view.cpu.x_axis_labels.last().unwrap(), "now");
    }

    #[test]
    fn load_series_is_always_empty_today() {
        let history = vec![pt(0, Some(10.0)), pt(2, Some(20.0))];
        let view = build_view(&history, 100, 100);
        assert!(view.load.points.is_empty());
        assert_eq!(view.load.last_value, None);
    }

    #[test]
    fn mem_series_uses_used_over_total_percent() {
        let mk = |ts: i64, used: u64, total: u64| SystemHistoryPoint {
            ts,
            cpu_percent: None,
            mem_used_mb: Some(used),
            mem_total_mb: Some(total),
            process_rss_mb: None,
            gpus: Vec::new(),
        };
        let history = vec![mk(0, 1024, 4096), mk(2, 2048, 4096)];
        let view = build_view(&history, 100, 100);
        assert_eq!(view.mem.points.len(), 2);
        assert_eq!(view.mem.last_value, Some(50.0));
    }

    #[test]
    fn samples_count_and_window_secs() {
        let history = vec![pt(0, Some(10.0)), pt(30, Some(20.0)), pt(60, Some(30.0))];
        let view = build_view(&history, 100, 100);
        assert_eq!(view.samples_count, 3);
        assert_eq!(view.window_secs, 60);
    }
}
