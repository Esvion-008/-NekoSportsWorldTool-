//! V2 候选路线生成：打卡点顺序枚举 + K 最短路 + 终点 POI + 距离补齐 + 造型/评分/去重。

pub mod playground;
pub mod shape;
pub mod v3;
pub mod v4;

use std::collections::HashSet;

use crate::campus::geo::{self, Projection};
use crate::campus::graph::RoadGraph;
use crate::campus::{Campus, LngLat, PoiKind};
use crate::track::geom::{round_to, Rng};
use crate::track::generator;
use crate::track::model::Track;

use shape::{shape_route, ShapeResult};

#[derive(Clone, Debug)]
pub struct RouteCandidate {
    pub points: Vec<LngLat>,
    pub checkpoints: Vec<LngLat>,
    pub tour_m: f64,
    pub total_m: f64,
    pub end_poi: Option<String>,
    pub corners: usize,
    pub straight_ratio: f64,
    pub curve_ratio: f64,
    pub score: f64,
    pub order: Vec<usize>,
    /// 生成过程中的非致命告警（例如两点之间没有通道、只能直连穿楼）。
    /// 由 flow 打到日志里，方便一眼看出这条轨迹哪里不对。
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct RouteOptions {
    pub target_m: f64,
    pub max_candidates: usize,
    pub seed: u64,
}

impl Default for RouteOptions {
    fn default() -> Self {
        Self {
            target_m: 2000.0,
            max_candidates: 12,
            seed: 1,
        }
    }
}

/// 生成候选路线（已按评分排序、去重）。
pub fn generate(
    campus: &Campus,
    checkpoints: &[LngLat],
    options: RouteOptions,
) -> Result<Vec<RouteCandidate>, String> {
    if checkpoints.is_empty() {
        return Err("打卡点为空，无法生成路线".into());
    }
    let graph = RoadGraph::build(campus).or_else(|_| {
        RoadGraph::build_with(campus, 0.8, campus.allow_shrink_m)
            .map_err(|e| format!("路网建图失败：{e}"))
    })?;
    let mut graph = graph;
    let mut checkpoint_nodes = Vec::with_capacity(checkpoints.len());
    for checkpoint in checkpoints {
        checkpoint_nodes.push(
            graph
                .attach(*checkpoint, true)
                .map_err(|e| {
                    format!(
                        "打卡点接入路网失败（点位可能不在当前校园区域内，请导入正确校区的 campus.geojson）：{e}"
                    )
                })?,
        );
    }
    // OSM 场地/操场内部道路经常互不相连：打卡点之间若直线可走，补一条开阔区连接边。
    for i in 0..checkpoint_nodes.len() {
        for j in (i + 1)..checkpoint_nodes.len() {
            if graph.shortest_path(checkpoint_nodes[i], checkpoint_nodes[j]).is_none() {
                graph.connect_nodes(checkpoint_nodes[i], checkpoint_nodes[j], true);
            }
        }
    }
    // 终点 POI 接入
    let mut poi_nodes: Vec<(String, u32)> = Vec::new();
    for (kind, poi) in campus.pois.all() {
        if let Ok(node) = graph.attach(poi.pos, true) {
            let name = if poi.name.trim().is_empty() {
                format!("{} #{}", kind.label(), poi.id)
            } else {
                poi.name.clone()
            };
            poi_nodes.push((name, node));
        }
    }
    // 终点 POI 若与任一打卡点不连通，尝试开放区直连。
    for (_, node) in &poi_nodes {
        for checkpoint_node in &checkpoint_nodes {
            if graph.shortest_path(*checkpoint_node, *node).is_none() {
                graph.connect_nodes(*checkpoint_node, *node, true);
            }
        }
    }

    let order_list = build_orders(campus, checkpoints, checkpoint_nodes.len());
    let projection = Projection::for_polygon(&campus.area);
    let mut raw_candidates: Vec<RouteCandidate> = Vec::new();
    for (order_index, order) in order_list.iter().enumerate() {
        // order 是 checkpoint_nodes 的下标
        let Some(base) = route_through(&graph, &checkpoint_nodes, order) else {
            continue;
        };
        let tour_m = base.length_m;
        if tour_m > options.target_m + 150.0 {
            continue;
        }
        let last_node = checkpoint_nodes[*order.last().expect("order 非空")];
        let last_point = match graph.node_bd(last_node) {
            Some(point) => point,
            None => continue,
        };
        let base_points = base.points.clone();

        // 终点 POI 尾段
        let tails: Vec<(Option<String>, Vec<LngLat>, f64)> = if tour_m <= options.target_m + 5.0 {
            let mut list = Vec::new();
            for (name, node) in &poi_nodes {
                if let Some(path) = graph.shortest_path(last_node, *node) {
                    list.push((Some(name.clone()), path.points, path.length_m));
                }
            }
            list
        } else {
            vec![(None, Vec::new(), 0.0)]
        };
        if tails.is_empty() && tour_m > options.target_m + 5.0 {
            continue;
        }

        for (tail_index, (end_poi, tail_points, tail_m)) in tails.iter().enumerate() {
            let base_total = tour_m + tail_m;
            if base_total > options.target_m + 5.0 {
                // 巡回路已超长：§6.3 仅接受 2000–2050（不补尾段）
                if tour_m > options.target_m - 5.0
                    && tour_m <= options.target_m + 50.0
                    && *tail_m <= 0.1
                {
                    if let Some(candidate) = shape_and_score(
                        campus,
                        &base_points,
                        &base_points,
                        checkpoints,
                        order,
                        tour_m,
                        None,
                        options,
                        order_index * 13 + tail_index,
                        &projection,
                    ) {
                        if (candidate.total_m - options.target_m).abs() <= 5.0 {
                            raw_candidates.push(candidate);
                        }
                    }
                }
                continue;
            }
            let mut pad = options.target_m - base_total;
            for attempt in 0..6 {
                let pad_target = if pad > 0.0 { pad } else { 0.0 };
                let mut points = base_points.clone();
                let padding = build_padding(
                    campus,
                    &graph,
                    last_node,
                    last_point,
                    pad_target,
                    options.seed + order_index as u64 * 31 + attempt as u64,
                    &projection,
                );
                points.extend(padding.iter().skip(1).copied());
                points.extend(tail_points.iter().skip(1).copied());
                if let Some(candidate) = shape_and_score(
                    campus,
                    &points,
                    &base_points,
                    checkpoints,
                    order,
                    tour_m,
                    end_poi.clone(),
                    options,
                    order_index * 13 + tail_index + attempt * 7,
                    &projection,
                ) {
                    let error = options.target_m - candidate.total_m;
                    if error.abs() <= 5.0 {
                        raw_candidates.push(candidate);
                        break;
                    }
                    pad += error;
                    if pad < 10.0 {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }

    raw_candidates.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut unique: Vec<RouteCandidate> = Vec::new();
    for candidate in raw_candidates {
        if unique
            .iter()
            .any(|existing| similar_route(&existing.points, &candidate.points))
        {
            continue;
        }
        unique.push(candidate);
        if unique.len() >= options.max_candidates.max(1) {
            break;
        }
    }
    if unique.is_empty() {
        return Err("没有满足距离与几何约束的候选路线（可尝试调整路网/净空或打卡点）".into());
    }
    Ok(unique)
}

struct BaseRoute {
    points: Vec<LngLat>,
    length_m: f64,
}

/// 从当前路网节点走到一个较远的道路节点，再原路返回；返回点列包含起点。
fn road_out_and_back(
    graph: &RoadGraph,
    start: u32,
    target_add: f64,
    banned: &mut HashSet<u32>,
) -> Option<(Vec<LngLat>, f64, u32)> {
    if target_add < 20.0 {
        return None;
    }
    let mut best: Option<(f64, f64, Vec<LngLat>, u32)> = None;
    for node in 0..graph.node_count() as u32 {
        if node == start || banned.contains(&node) {
            continue;
        }
        let Some(path) = graph.shortest_path(start, node) else {
            continue;
        };
        let total = path.length_m * 2.0;
        if total < 30.0 || total > target_add * 1.02 + 5.0 {
            continue;
        }
        let difference = (total - target_add).abs();
        if best
            .as_ref()
            .map(|existing| difference < existing.0)
            .unwrap_or(true)
        {
            let mut points = path.points.clone();
            points.extend(path.points.iter().rev().skip(1).copied());
            best = Some((difference, total, points, node));
        }
    }
    let (_, length, points, node) = best?;
    banned.insert(node);
    Some((points, length, node))
}

/// 不再用同心圆补距：优先沿道路往返，再退化为一次开阔区直线往返。
fn build_padding(
    campus: &Campus,
    graph: &RoadGraph,
    start_node: u32,
    start_point: LngLat,
    target: f64,
    _seed: u64,
    projection: &Projection,
) -> Vec<LngLat> {
    let mut out: Vec<LngLat> = Vec::new();
    if target < 10.0 {
        return out;
    }
    let mut remaining = target;
    let mut banned: HashSet<u32> = HashSet::new();
    for _ in 0..2 {
        if remaining < 30.0 {
            break;
        }
        let Some((points, length, _)) = road_out_and_back(graph, start_node, remaining, &mut banned)
        else {
            break;
        };
        if out.is_empty() {
            out.extend(points);
        } else {
            out.extend(points.into_iter().skip(1));
        }
        remaining -= length;
    }
    if remaining > 20.0 {
        if let Some(out_back) =
            straight_out_and_back(campus, start_point, remaining / 2.0, projection)
        {
            if out.is_empty() {
                out.extend(out_back);
            } else {
                out.extend(out_back.into_iter().skip(1));
            }
        }
    }
    out
}

fn route_through(graph: &RoadGraph, nodes: &[u32], order: &[usize]) -> Option<BaseRoute> {
    let mut points: Vec<LngLat> = Vec::new();
    let mut length = 0.0;
    for pair in order.windows(2) {
        let start = nodes[pair[0]];
        let goal = nodes[pair[1]];
        let path = graph.shortest_path(start, goal)?;
        length += path.length_m;
        if points.is_empty() {
            points.extend(path.points);
        } else {
            points.extend(path.points.into_iter().skip(1));
        }
    }
    if points.len() < 2 {
        return None;
    }
    Some(BaseRoute {
        points,
        length_m: length,
    })
}

fn shape_and_score(
    campus: &Campus,
    raw: &[LngLat],
    tour_points: &[LngLat],
    checkpoints: &[LngLat],
    order: &[usize],
    tour_m: f64,
    end_poi: Option<String>,
    options: RouteOptions,
    rng_salt: usize,
    projection: &Projection,
) -> Option<RouteCandidate> {
    let mut rng = Rng::new(options.seed.wrapping_add(rng_salt as u64).wrapping_add(1));
    let shaped: ShapeResult = match shape_route(raw, campus, checkpoints, &mut rng) {
        Some(shaped) => shaped,
        None => return None,
    };
    if !shaped.points.iter().all(|p| campus.route_point_ok(*p, 0.6)) {
        return None;
    }
    if shaped.straight_ratio < 0.60 {
        return None;
    }
    let corner_penalty = shaped.corners as f64 * 0.4;
    let ratio_penalty = (shaped.straight_ratio - 0.65).abs() * 200.0;
    let distance_penalty = (shaped.total_m - options.target_m).abs() * 10.0;
    let tail_penalty = if end_poi.is_some() { 0.0 } else { 30.0 };
    let score = distance_penalty + corner_penalty + ratio_penalty + tail_penalty;
    let _ = tour_points;
    let _ = projection;
    Some(RouteCandidate {
        points: shaped.points,
        checkpoints: checkpoints.to_vec(),
        tour_m,
        total_m: shaped.total_m,
        end_poi,
        corners: shaped.corners,
        straight_ratio: shaped.straight_ratio,
        curve_ratio: shaped.curve_ratio,
        score,
        order: order.to_vec(),
        warnings: Vec::new(),
    })
}

/// 一段开阔区直线往返（去 half_m、原路返回），用于补齐绕行段之后的剩余距离。
fn straight_out_and_back(
    campus: &Campus,
    anchor: LngLat,
    half_m: f64,
    projection: &Projection,
) -> Option<Vec<LngLat>> {
    let half_m = half_m.clamp(8.0, 600.0);
    let anchor_xy = projection.xy(anchor);
    for attempt in 0..12 {
        let angle = attempt as f64 * std::f64::consts::TAU / 12.0;
        let steps = ((half_m / 6.0).ceil() as usize).clamp(4, 100);
        let mut leg = Vec::with_capacity(steps + 1);
        leg.push(anchor);
        let mut allowed = true;
        for step in 1..=steps {
            let distance = half_m * step as f64 / steps as f64;
            let point = projection.lnglat([
                anchor_xy[0] + distance * angle.cos(),
                anchor_xy[1] + distance * angle.sin(),
            ]);
            if !campus.route_point_ok(point, 0.8) {
                allowed = false;
                break;
            }
            leg.push(point);
        }
        if !allowed {
            continue;
        }
        // 去程 + 回程（去掉重复的远端点）
        let mut out = leg.clone();
        out.extend(leg.iter().rev().skip(1).copied());
        return Some(out);
    }
    None
}

fn build_orders(campus: &Campus, checkpoints: &[LngLat], checkpoint_count: usize) -> Vec<Vec<usize>> {
    if checkpoint_count <= 1 {
        return vec![vec![0]];
    }
    if checkpoint_count <= 7 {
        let mut rest: Vec<usize> = (1..checkpoint_count).collect();
        let mut permutations = Vec::new();
        permute(&mut rest, 0, &mut permutations, 120);
        return permutations
            .into_iter()
            .map(|mut order| {
                let mut full = vec![0usize];
                full.append(&mut order);
                full
            })
            .collect();
    }
    // N > 7：贪心最近邻 + 简单 2-opt
    let projection = Projection::for_polygon(&campus.area);
    let mut unvisited: HashSet<usize> = (1..checkpoint_count).collect();
    let mut order = vec![0usize];
    while !unvisited.is_empty() {
        let current = *order.last().expect("非空");
        let mut best = None;
        let mut best_d = f64::INFINITY;
        for &candidate in &unvisited {
            let d = projection.distance_m(checkpoints[current], checkpoints[candidate]);
            if d < best_d {
                best_d = d;
                best = Some(candidate);
            }
        }
        let Some(next) = best else { break };
        order.push(next);
        unvisited.remove(&next);
    }
    vec![order]
}

fn permute(items: &mut [usize], start: usize, out: &mut Vec<Vec<usize>>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    if start == items.len() {
        out.push(items.to_vec());
        return;
    }
    for i in start..items.len() {
        items.swap(start, i);
        permute(items, start + 1, out, limit);
        items.swap(start, i);
    }
}

fn similar_route(a: &[LngLat], b: &[LngLat]) -> bool {
    if a.len() < 2 || b.len() < 2 {
        return false;
    }
    let samples = 12usize;
    let mut close = 0usize;
    for i in 0..samples {
        let ia = i * (a.len() - 1) / (samples - 1);
        let ib = i * (b.len() - 1) / (samples - 1);
        let dx = a[ia][0] - b[ib][0];
        let dy = a[ia][1] - b[ib][1];
        if (dx * dx + dy * dy).sqrt() < 1e-3 {
            close += 1;
        }
    }
    close as f64 / samples as f64 > 0.90
}

/// 把 V2 路线几何套进现有协议轨迹（保留速度/步频/海拔/哨兵/10s 窗全部后处理）。
pub fn build_track(
    campus: &Campus,
    candidate: &RouteCandidate,
    pace_sec_per_km: f64,
    seed: u64,
    start_ms: i64,
) -> Track {
    let projection = Projection::for_polygon(&campus.area);
    let dist = round_to(candidate.total_m, 3);
    let duration = ((dist / 1000.0) * pace_sec_per_km.clamp(150.0, 600.0)).round() as i64;
    let center = candidate
        .checkpoints
        .first()
        .copied()
        .unwrap_or([0.0, 0.0]);
    let ring_points: Vec<(f64, f64)> = if candidate.checkpoints.len() >= 2 {
        candidate
            .checkpoints
            .iter()
            .map(|point| (point[1], point[0]))
            .collect()
    } else {
        vec![
            (candidate.points[0][1], candidate.points[0][0]),
            (
                candidate.points.last().map(|p| p[1]).unwrap_or(center[0]),
                candidate.points.last().map(|p| p[0]).unwrap_or(center[1]),
            ),
        ]
    };
    let mut track = generator::build(
        dist,
        duration.max(60),
        seed,
        (center[1], center[0]),
        start_ms,
        &ring_points,
    );
    let sampler = PolylineSampler::new(&candidate.points, projection);
    for location in &mut track.locations {
        let point = sampler.at(location.totalDis);
        location.gLat = round_to(point[1], 7);
        location.gLng = round_to(point[0], 7);
    }
    // 打卡点吸附：最近点精确落位，前后各 3 个点按三角权重把弧线一起带过去。
    // 只拽孤立的单个点会留下一个「6m 瞬移」的钉子，真实 GPS 不会这样跳。
    let snapshot: Vec<(f64, f64)> = track
        .locations
        .iter()
        .map(|location| (location.gLat, location.gLng))
        .collect();
    for checkpoint in &candidate.checkpoints {
        let mut best_index = 0usize;
        let mut best_distance = f64::INFINITY;
        for (index, location) in track.locations.iter().enumerate() {
            let distance = projection.distance_m([location.gLng, location.gLat], *checkpoint);
            if distance < best_distance {
                best_distance = distance;
                best_index = index;
            }
        }
        if best_distance < 40.0 {
            let base = snapshot[best_index];
            let shift = (checkpoint[1] - base.0, checkpoint[0] - base.1);
            for (offset, weight) in [(-3i64, 0.12), (-2, 0.26), (-1, 0.55), (0, 1.0), (1, 0.55), (2, 0.26), (3, 0.12)] {
                let index = best_index as i64 + offset;
                if index < 0 || index as usize >= track.locations.len() {
                    continue;
                }
                let index = index as usize;
                let origin = snapshot[index];
                track.locations[index].gLat = round_to(origin.0 + shift.0 * weight, 7);
                track.locations[index].gLng = round_to(origin.1 + shift.1 * weight, 7);
            }
        }
    }
    if let Some(first) = candidate.checkpoints.first().or_else(|| candidate.points.first()) {
        if let Some(location) = track.locations.first_mut() {
            location.gLat = round_to(first[1], 7);
            location.gLng = round_to(first[0], 7);
        }
        if track.locations.len() > 1 {
            track.locations[1].gLat = round_to(first[1], 7);
            track.locations[1].gLng = round_to(first[0], 7);
        }
    }
    track.startLatitude = track.locations[0].gLat;
    track.startLongitude = track.locations[0].gLng;
    track.totalDistance = dist;
    track.validDistance = dist;
    track
}

/// 在 [`build_track`] 之上叠加 GPS 定位噪声，用于「操场绕圈 / 校园道路」真实化输出。
pub fn build_track_realistic(
    campus: &Campus,
    candidate: &RouteCandidate,
    pace_sec_per_km: f64,
    seed: u64,
    start_ms: i64,
) -> Track {
    let mut track = build_track(campus, candidate, pace_sec_per_km, seed, start_ms);
    let before: Vec<(f64, f64)> = track
        .locations
        .iter()
        .map(|location| (location.gLat, location.gLng))
        .collect();
    apply_gps_noise(&mut track, seed);
    // 噪声不能把点甩到楼边/围墙外：服务端会因为「距离建筑物太近」直接判成绩无效。
    // 几何本身已经有 8m 以上净空，这里再收一次，保证加噪后仍然 ≥5m。
    const NOISE_CLEARANCE_M: f64 = 5.0;
    for (index, location) in track.locations.iter_mut().enumerate() {
        if !campus.route_point_ok([location.gLng, location.gLat], NOISE_CLEARANCE_M) {
            location.gLat = before[index].0;
            location.gLng = before[index].1;
        }
    }
    track
}

/// 调试 / 可视化用：直接返回协议轨迹点 `[lng, lat]`，绕开私有的 `Track` 类型。
pub fn build_track_points(
    campus: &Campus,
    candidate: &RouteCandidate,
    pace_sec_per_km: f64,
    seed: u64,
    start_ms: i64,
) -> Vec<[f64; 2]> {
    let track = build_track_realistic(campus, candidate, pace_sec_per_km, seed, start_ms);
    track
        .locations
        .iter()
        .map(|location| [location.gLng, location.gLat])
        .collect()
}

/// GPS 定位误差：一阶自相关（Ornstein-Uhlenbeck）漂移 + 偶发跳点。
///
/// 关键点是**幅度跟该点自己上报的 `accuracy`/`radius` 挂钩**：精度写 5m 的点，
/// 坐标就必须有 5m 量级的散布。之前所有点精确落在同一条解析曲线上、精度却写
/// 3.7m，等于自己招了——轨迹残差和精度字段对不上。
///
/// 索引 0/1 是起点哨兵点（服务端按首点校核起点坐标），保持不动。
pub fn apply_gps_noise(track: &mut Track, seed: u64) {
    const MET_PER_DEG_LAT: f64 = 111_132.0;
    const MET_PER_DEG_LNG_EQ: f64 = 111_320.0;
    let count = track.locations.len();
    if count < 6 {
        return;
    }
    let mid_lat = track.locations[count / 2].gLat;
    let m_per_deg_lng = (MET_PER_DEG_LNG_EQ * mid_lat.to_radians().cos()).abs().max(1_000.0);
    let mut rng = Rng::new(seed ^ 0x9e37_79b9_7f4a_7c15);
    let (mut east, mut north) = (0.0f64, 0.0f64);
    for index in 2..count {
        let accuracy = track.locations[index].accuracy.clamp(1.0, 60.0);
        // 平稳标准差 ≈ 0.42×accuracy（手机 GPS 在 5m 精度时位置散布约 2m）。
        let sigma = (accuracy * 0.42).clamp(0.25, 3.0);
        let rho: f64 = 0.88;
        let step = sigma * (1.0 - rho * rho).sqrt();
        east = rho * east + rng.gauss(0.0, step);
        north = rho * north + rng.gauss(0.0, step);
        let (mut dx, mut dy) = (east, north);
        // 偶发跳点：真机每一两百个点会飘一次，飘完自己按 OU 衰减回来。
        if index > 3 && index + 2 < count && rng.random() < 0.004 {
            let radius = rng.uniform(8.0, 26.0);
            let angle = rng.uniform(0.0, std::f64::consts::TAU);
            dx += radius * angle.cos();
            dy += radius * angle.sin();
        }
        track.locations[index].gLat += dy / MET_PER_DEG_LAT;
        track.locations[index].gLng += dx / m_per_deg_lng;
    }
}

struct PolylineSampler {
    points: Vec<[f64; 2]>,
    arcs: Vec<f64>,
    projection: Projection,
}

impl PolylineSampler {
    fn new(points: &[LngLat], projection: Projection) -> Self {
        let xy: Vec<[f64; 2]> = points.iter().map(|point| projection.xy(*point)).collect();
        let mut arcs = vec![0.0f64];
        for pair in xy.windows(2) {
            let previous = *arcs.last().expect("非空");
            let dx = pair[1][0] - pair[0][0];
            let dy = pair[1][1] - pair[0][1];
            arcs.push(previous + (dx * dx + dy * dy).sqrt());
        }
        Self {
            points: xy,
            arcs,
            projection,
        }
    }

    fn at(&self, distance: f64) -> LngLat {
        if self.points.len() < 2 {
            return self
                .points
                .first()
                .map(|point| self.projection.lnglat(*point))
                .unwrap_or([0.0, 0.0]);
        }
        let total = *self.arcs.last().unwrap_or(&0.0);
        let target = distance.clamp(0.0, total);
        let index = match self
            .arcs
            .binary_search_by(|value| value.partial_cmp(&target).unwrap_or(std::cmp::Ordering::Equal))
        {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
        .min(self.points.len() - 2);
        let segment = self.arcs[index + 1] - self.arcs[index];
        let t = if segment > 1e-9 {
            ((target - self.arcs[index]) / segment).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let a = self.points[index];
        let b = self.points[index + 1];
        self.projection
            .lnglat([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t])
    }
}

/// 检查候选是否经过全部打卡点（3 m 内）。
pub fn candidate_passes_checkpoints(candidate: &RouteCandidate, projection: &Projection) -> bool {
    candidate.checkpoints.iter().all(|checkpoint| {
        candidate
            .points
            .iter()
            .map(|point| projection.distance_m(*point, *checkpoint))
            .fold(f64::INFINITY, f64::min)
            <= 3.0
    })
}

/// 候选路线的最小净空（粗略统计：距禁行区边界的最近距离）。
pub fn candidate_min_clearance(campus: &Campus, candidate: &RouteCandidate) -> f64 {
    let projection = Projection::for_polygon(&campus.area);
    let mut best = f64::INFINITY;
    for point in &candidate.points {
        for blocked in &campus.blocked {
            best = best.min(geo::distance_to_polygon_boundary(
                *point,
                &blocked.polygon,
                &projection,
            ));
        }
    }
    if best.is_finite() {
        best
    } else {
        999.0
    }
}

/// 供 UI 使用的 POI 类别标签（避免 UI 直接依赖内部顺序）。
pub fn poi_label(kind: PoiKind) -> &'static str {
    kind.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campus::{Poi, Polygon, Road};

    fn test_campus() -> Campus {
        let mut campus = Campus::new(
            Polygon::new(vec![
                [114.295, 30.495],
                [114.305, 30.495],
                [114.305, 30.505],
                [114.295, 30.505],
            ]),
            1,
        );
        campus.allow_shrink_m = 5.0;
        campus.roads = vec![
            Road {
                id: 1,
                name: "东西".into(),
                path: vec![[114.296, 30.500], [114.304, 30.500]],
            },
            Road {
                id: 2,
                name: "南北".into(),
                path: vec![[114.300, 30.496], [114.300, 30.504]],
            },
        ];
        campus.pois.canteen.push(Poi {
            id: 1,
            name: "食堂".into(),
            pos: [114.3035, 30.5000],
        });
        campus
    }

    #[test]
    fn generates_candidates_that_start_at_first_checkpoint_and_meet_target() {
        let campus = test_campus();
        let checkpoints = vec![
            [114.2970, 30.5000],
            [114.3000, 30.5000],
            [114.3020, 30.5000],
        ];
        let candidates = generate(
            &campus,
            &checkpoints,
            RouteOptions {
                target_m: 1000.0,
                max_candidates: 8,
                seed: 7,
            },
        )
        .unwrap();
        assert!(!candidates.is_empty());
        let projection = Projection::for_polygon(&campus.area);
        for candidate in &candidates {
            assert!((candidate.total_m - 1000.0).abs() <= 5.0, "total={}", candidate.total_m);
            assert_eq!(candidate.points.first().copied(), Some(checkpoints[0]));
            assert!(candidate_passes_checkpoints(candidate, &projection));
            assert!(candidate_min_clearance(&campus, candidate) >= 0.0);
        }
    }

    #[test]
    fn generates_2000m_candidates_quickly_with_all_checkpoints() {
        use std::time::Instant;
        let campus = test_campus();
        let checkpoints = vec![
            [114.2970, 30.5000],
            [114.3000, 30.5000],
            [114.3030, 30.5000],
            [114.3000, 30.4980],
            [114.3000, 30.5020],
        ];
        let begin = Instant::now();
        let candidates = generate(
            &campus,
            &checkpoints,
            RouteOptions {
                target_m: 2000.0,
                max_candidates: 12,
                seed: 11,
            },
        )
        .unwrap();
        let elapsed = begin.elapsed();
        assert!(elapsed.as_secs_f64() < 2.0, "生成耗时 {elapsed:?}");
        assert!(!candidates.is_empty());
        let projection = Projection::for_polygon(&campus.area);
        for candidate in &candidates {
            assert!(
                (candidate.total_m - 2000.0).abs() <= 5.0,
                "total={}",
                candidate.total_m
            );
            assert!(candidate.tour_m <= 2050.0, "tour={}", candidate.tour_m);
            assert_eq!(candidate.points.first().copied(), Some(checkpoints[0]));
            assert!(candidate_passes_checkpoints(candidate, &projection));
            assert!(candidate
                .points
                .iter()
                .all(|point| campus.route_point_ok(*point, 0.6)));
            assert!((0.0..=1.0).contains(&candidate.straight_ratio));
            assert!(
                candidate.straight_ratio >= 0.60,
                "straight ratio={} curve={}",
                candidate.straight_ratio,
                candidate.curve_ratio
            );
        }
    }

    #[test]
    fn build_track_maps_route_geometry_and_keeps_protocol_fields() {
        let campus = test_campus();
        let candidate = RouteCandidate {
            points: vec![
                [114.2970, 30.5000],
                [114.2990, 30.5000],
                [114.3010, 30.5000],
                [114.3030, 30.5000],
            ],
            checkpoints: vec![[114.2970, 30.5000], [114.3030, 30.5000]],
            tour_m: 500.0,
            total_m: 500.0,
            end_poi: None,
            corners: 0,
            straight_ratio: 1.0,
            curve_ratio: 0.0,
            score: 0.0,
            order: vec![0, 1],
            warnings: Vec::new(),
        };
        let track = build_track(&campus, &candidate, 360.0, 5, 1_700_000_000_000);
        assert!((track.totalDistance - 500.0).abs() < 0.1);
        assert!(track.totalTime > 0);
        assert!(!track.speedPerTenSec.is_empty());
        assert_eq!(track.locations[0].ptype, 0);
        assert!(matches!(track.locations[0].ptype, 0 | 7));
        assert_eq!(track.locations[1].ptype, 5);
        assert_eq!(track.locations.last().unwrap().ptype, 6);
        assert!((track.locations[0].gLng - 114.2970).abs() < 1e-5);
    }

    /// 190m × 106m 操场 + 5 个分散的点位（P1 在场边、P3 在弯道端外）。
    fn playground_case() -> (Campus, Vec<LngLat>) {
        use crate::campus::PlaygroundArea;
        let m_per_deg_lng = 111_320.0 * 30.5f64.to_radians().cos();
        let bd = |east: f64, north: f64| -> LngLat {
            [114.3 + east / m_per_deg_lng, 30.5 + north / 111_132.0]
        };
        let mut campus = Campus::new(
            Polygon::new(vec![
                bd(-600.0, -600.0),
                bd(600.0, -600.0),
                bd(600.0, 600.0),
                bd(-600.0, 600.0),
            ]),
            1,
        );
        campus.allow_shrink_m = 1.0;
        campus.playgrounds.push(PlaygroundArea {
            id: 1,
            name: "田径场".into(),
            polygon: Polygon::new(vec![
                bd(-53.0, -95.0),
                bd(53.0, -95.0),
                bd(53.0, 95.0),
                bd(-53.0, 95.0),
            ]),
        });
        let checkpoints = vec![
            bd(-50.9, -46.3),
            bd(32.5, 14.3),
            bd(-2.5, 84.5),
            bd(-42.9, 32.4),
            bd(-9.4, -75.6),
        ];
        (campus, checkpoints)
    }

    #[test]
    fn gps_noise_matches_reported_accuracy() {
        let (campus, checkpoints) = playground_case();
        let candidate = playground::track_loop(&campus, &checkpoints, 2000.0, 21).unwrap();
        let clean = build_track(&campus, &candidate, 400.0, 21, 1_700_000_000_000);
        let noisy = build_track_realistic(&campus, &candidate, 400.0, 21, 1_700_000_000_000);
        assert_eq!(clean.locations.len(), noisy.locations.len());
        let projection = Projection::for_polygon(&campus.area);
        let (mut offset_sum, mut accuracy_sum, mut count) = (0.0f64, 0.0f64, 0usize);
        for index in 3..clean.locations.len().saturating_sub(2) {
            offset_sum += projection.distance_m(
                [clean.locations[index].gLng, clean.locations[index].gLat],
                [noisy.locations[index].gLng, noisy.locations[index].gLat],
            );
            accuracy_sum += noisy.locations[index].accuracy;
            count += 1;
        }
        let mean_offset = offset_sum / count as f64;
        let mean_accuracy = accuracy_sum / count as f64;
        // 二维 OU 的期望偏移 ≈ 1.25σ，σ = 0.42×accuracy → 比值 0.4-0.7。
        let ratio = mean_offset / mean_accuracy;
        assert!(mean_offset > 0.15, "噪声过小：{mean_offset:.3}m");
        assert!(ratio > 0.2 && ratio < 0.85, "噪声/精度比 {ratio:.3}（精度 {mean_accuracy:.2}m）");
    }

    #[test]
    fn realistic_track_snaps_checkpoints_and_keeps_start() {
        let (campus, checkpoints) = playground_case();
        let candidate = playground::track_loop(&campus, &checkpoints, 2000.0, 33).unwrap();
        let track = build_track_realistic(&campus, &candidate, 400.0, 33, 1_700_000_000_000);
        let projection = Projection::for_polygon(&campus.area);
        // 起点仍是服务端给的第一个打卡点（噪声不作用于哨兵点）。
        assert_eq!(track.locations[0].gLng, round_to(checkpoints[0][0], 7));
        assert_eq!(track.locations[0].gLat, round_to(checkpoints[0][1], 7));
        for (index, checkpoint) in checkpoints.iter().enumerate() {
            let nearest = track
                .locations
                .iter()
                .map(|location| projection.distance_m([location.gLng, location.gLat], *checkpoint))
                .fold(f64::INFINITY, f64::min);
            assert!(nearest < 10.0, "P{} 最近 {nearest:.2}m，超出 10m 到达圈", index + 1);
        }
        // 相邻点间距必须有抖动：解析曲线上点距是等距的，真人 + GPS 不是。
        let steps: Vec<f64> = track
            .locations
            .windows(2)
            .map(|pair| projection.distance_m([pair[0].gLng, pair[0].gLat], [pair[1].gLng, pair[1].gLat]))
            .collect();
        let mean = steps.iter().sum::<f64>() / steps.len() as f64;
        let variance = steps.iter().map(|step| (step - mean).powi(2)).sum::<f64>() / steps.len() as f64;
        assert!(mean > 1.0, "步长均值 {mean:.2}m");
        assert!(variance.sqrt() / mean > 0.12, "步长离散度 {}", variance.sqrt() / mean);
    }
}
