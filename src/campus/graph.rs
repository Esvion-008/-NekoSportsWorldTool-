//! 校园路网图与 A* 寻路。
//!
//! 建图：路网折线 → 交点打断 → 0.5 m 网格合并节点 → 按允许区域内缩/禁行净空过滤边。
//! 接入：打卡点/POI 投影到最近可行边，必要时在该边上打断并生成连接段。
//! 寻路：状态 = (当前节点, 上一节点)，代价 = 米制长度 + 转角惩罚；启发式 = 直线距离。

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use super::geo::{self, Projection};
use super::{Campus, LngLat};

/// 节点合并容差（米）。
const NODE_SNAP_M: f64 = 0.35;
/// 接入点允许离路网的最大距离（米）。
pub const MAX_CONNECTOR_M: f64 = 300.0;
/// 最短边长度（米），低于此值忽略。
const MIN_EDGE_M: f64 = 0.05;

#[derive(Clone, Debug)]
struct Edge {
    a: u32,
    b: u32,
    len: f64,
}

/// 一条寻路结果。
#[derive(Clone, Debug, PartialEq)]
pub struct RoutePath {
    pub nodes: Vec<u32>,
    pub points: Vec<LngLat>,
    pub length_m: f64,
}

/// 路网统计。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
    pub segments: usize,
}

/// 校园路网图（节点坐标为内部米制投影）。
pub struct RoadGraph {
    projection: Projection,
    nodes: Vec<[f64; 2]>,
    edges: Vec<Edge>,
    adjacency: Vec<Vec<(u32, f64)>>,
    area_rings: Vec<Vec<[f64; 2]>>,
    blocked_rings: Vec<Vec<Vec<[f64; 2]>>>,
    pub clearance_m: f64,
    pub shrink_m: f64,
    /// 原始路网折线段数量（用于界面统计）。
    pub source_segments: usize,
}

#[derive(Clone, Copy)]
struct HeapItem {
    f: f64,
    g: f64,
    node: u32,
    prev: u32,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f && self.g == other.g && self.node == other.node && self.prev == other.prev
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // 最小堆：f 小的优先级高
        other
            .f
            .partial_cmp(&self.f)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.g.partial_cmp(&self.g).unwrap_or(Ordering::Equal))
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl RoadGraph {
    /// 按 campus 的默认净空/内缩参数建图。
    pub fn build(campus: &Campus) -> Result<Self, String> {
        Self::build_with(campus, campus.clearance_min, campus.allow_shrink_m)
    }

    /// 按指定净空/内缩重试建图（窄走廊可传 0.8）。
    pub fn build_with(campus: &Campus, clearance_m: f64, shrink_m: f64) -> Result<Self, String> {
        if campus.area.is_empty() {
            return Err("校园区域为空，无法建图".into());
        }
        if campus.roads.is_empty() {
            return Err("尚未导入路网（roads.geojson），无法建图".into());
        }
        let projection = Projection::for_polygon(&campus.area);
        let area_rings: Vec<Vec<[f64; 2]>> = campus
            .area
            .rings()
            .map(|ring| geo::project_ring(ring, &projection))
            .collect();
        let blocked_polys: Vec<Vec<Vec<[f64; 2]>>> = campus
            .blocked
            .iter()
            .map(|blocked| {
                blocked
                    .polygon
                    .rings()
                    .map(|ring| geo::project_ring(ring, &projection))
                    .collect()
            })
            .collect();

        // ① 原始折线段
        let mut raw: Vec<([f64; 2], [f64; 2])> = Vec::new();
        for road in &campus.roads {
            if road.path.len() < 2 {
                continue;
            }
            for pair in road.path.windows(2) {
                let a = projection.xy(pair[0]);
                let b = projection.xy(pair[1]);
                if distance(a, b) >= MIN_EDGE_M {
                    raw.push((a, b));
                }
            }
        }
        if raw.is_empty() {
            return Err("路网没有有效线段，无法建图".into());
        }
        let source_segments = raw.len();

        // ② 求交点并在参数域打断
        let mut splits: Vec<Vec<f64>> = vec![vec![0.0, 1.0]; raw.len()];
        for i in 0..raw.len() {
            for j in (i + 1)..raw.len() {
                if let Some((ti, tj)) = segment_intersection_params(raw[i], raw[j]) {
                    splits[i].push(ti);
                    splits[j].push(tj);
                }
            }
        }

        // ③ 生成节点/边
        let mut graph = Self {
            projection,
            nodes: Vec::new(),
            edges: Vec::new(),
            adjacency: Vec::new(),
            area_rings: area_rings.clone(),
            blocked_rings: blocked_polys.clone(),
            clearance_m,
            shrink_m,
            source_segments,
        };
        let mut node_map: HashMap<(i64, i64), u32> = HashMap::new();
        for (index, (a, b)) in raw.iter().enumerate() {
            let mut ts = splits[index].clone();
            ts.sort_by(|x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal));
            ts.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
            for pair in ts.windows(2) {
                let p = lerp(*a, *b, pair[0]);
                let q = lerp(*a, *b, pair[1]);
                if distance(p, q) < MIN_EDGE_M {
                    continue;
                }
                let pa = node_for(&mut graph.nodes, &mut node_map, p);
                let pb = node_for(&mut graph.nodes, &mut node_map, q);
                if pa == pb {
                    continue;
                }
                if !edge_allowed(
                    p,
                    q,
                    &area_rings,
                    &blocked_polys,
                    shrink_m,
                    clearance_m,
                ) {
                    continue;
                }
                graph.push_edge(pa, pb);
            }
        }
        graph.rebuild_adjacency();
        if graph.edges.is_empty() {
            return Err(format!(
                "路网在净空 {:.2} m / 内缩 {:.1} m 约束下没有可用边（可尝试降低净空）",
                clearance_m, shrink_m
            ));
        }
        Ok(graph)
    }

    pub fn stats(&self) -> GraphStats {
        GraphStats {
            nodes: self.nodes.len(),
            edges: self.edges.len(),
            segments: self.source_segments,
        }
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn node_bd(&self, node: u32) -> Option<LngLat> {
        self.nodes.get(node as usize).map(|p| self.projection.lnglat(*p))
    }

    pub fn node_xy(&self, node: u32) -> Option<[f64; 2]> {
        self.nodes.get(node as usize).copied()
    }

    /// 把一个 BD 坐标接入路网，返回虚拟节点。`allow_blocked` 仅对点位落在禁行区内的
    /// 打卡点/POI 开放（3 m 可行走小圆规则），连接段本身仍需在允许区域内。
    pub fn attach(&mut self, pos: LngLat, allow_blocked: bool) -> Result<u32, String> {
        let target = self.projection.xy(pos);
        if let Some(node) = self.node_within(target, NODE_SNAP_M) {
            return Ok(node);
        }
        if self.edges.is_empty() {
            return Err("路网为空，无法接入点位".into());
        }
        let mut candidates = self.closest_edges(target, 12);
        if candidates.is_empty() {
            return Err("路网没有可接入的边".into());
        }
        candidates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
        for (edge_index, distance_to_edge, t) in candidates {
            if distance_to_edge > MAX_CONNECTOR_M {
                break;
            }
            let edge = self.edges.get(edge_index).cloned();
            let Some(edge) = edge else { continue };
            let a = self.nodes[edge.a as usize];
            let b = self.nodes[edge.b as usize];
            let projection_point = lerp(a, b, t);
            // 点位本身在禁行区时，只允许点位（不检查连接段），避免 3 m 小圆规则失效。
            let force_connector = allow_blocked && self.point_in_blocked(target);
            if distance_to_edge > MIN_EDGE_M
                && !force_connector
                && !self.connector_allowed(target, projection_point)
            {
                continue;
            }
            // 先把投影点接入路网（必要时打断边），再创建点位节点。
            let edge_node = if let Some(node) = self.node_within(projection_point, NODE_SNAP_M) {
                node
            } else {
                self.split_edge(edge_index, projection_point)
            };
            let point_node = if distance(target, projection_point) < MIN_EDGE_M {
                edge_node
            } else if let Some(node) = self.node_within(target, NODE_SNAP_M) {
                node
            } else {
                let id = self.nodes.len() as u32;
                self.nodes.push(target);
                id
            };
            if point_node != edge_node {
                let len = distance(target, projection_point);
                if len >= MIN_EDGE_M {
                    self.push_edge(point_node, edge_node);
                }
            }
            self.rebuild_adjacency();
            return Ok(point_node);
        }
        Err(format!(
            "点位离最近可行路网超过 {:.0} m，或连接段穿过禁行区",
            MAX_CONNECTOR_M
        ))
    }

    /// 在开阔区把两个已接入节点用一条可行走直连接入图（用于 OSM 路网断开的场地/操场）。
    pub fn connect_nodes(&mut self, a: u32, b: u32, allow_blocked: bool) -> bool {
        if a == b {
            return true;
        }
        let (Some(pa), Some(pb)) = (self.node_xy(a), self.node_xy(b)) else {
            return false;
        };
        if distance(pa, pb) > 500.0 {
            return false;
        }
        let force = allow_blocked
            && (self.point_in_blocked(pa) || self.point_in_blocked(pb));
        if !force && !self.connector_allowed(pa, pb) {
            return false;
        }
        self.push_edge(a, b);
        self.rebuild_adjacency();
        true
    }

    /// 从任意已接入节点出发的最短路。
    pub fn shortest_path(&self, start: u32, goal: u32) -> Option<RoutePath> {
        self.shortest_paths(start, goal, 1).into_iter().next()
    }

    /// K 条最短路径：首条 A* 后对已用边加惩罚再跑，直到无新路径或达到 K。
    pub fn shortest_paths(&self, start: u32, goal: u32, k: usize) -> Vec<RoutePath> {
        let k = k.max(1);
        let mut penalties: HashMap<(u32, u32), f64> = HashMap::new();
        let mut out: Vec<RoutePath> = Vec::new();
        for _ in 0..k {
            let Some(path) = self.astar(start, goal, &penalties) else {
                break;
            };
            if out.iter().any(|existing| existing.nodes == path.nodes) {
                break;
            }
            for pair in path.nodes.windows(2) {
                *penalties.entry((pair[0], pair[1])).or_insert(0.0) += 20.0;
                *penalties.entry((pair[1], pair[0])).or_insert(0.0) += 20.0;
            }
            out.push(path);
            if start == goal {
                break;
            }
        }
        out
    }

    fn astar(
        &self,
        start: u32,
        goal: u32,
        penalties: &HashMap<(u32, u32), f64>,
    ) -> Option<RoutePath> {
        if start as usize >= self.nodes.len() || goal as usize >= self.nodes.len() {
            return None;
        }
        if start == goal {
            return Some(RoutePath {
                nodes: vec![start],
                points: vec![self.node_bd(start)?],
                length_m: 0.0,
            });
        }
        let start_key = (start, u32::MAX);
        let mut gscore: HashMap<(u32, u32), f64> = HashMap::new();
        gscore.insert(start_key, 0.0);
        let mut came: HashMap<(u32, u32), (u32, u32)> = HashMap::new();
        let mut heap = BinaryHeap::new();
        heap.push(HeapItem {
            f: self.distance_xy(start, goal),
            g: 0.0,
            node: start,
            prev: u32::MAX,
        });
        let mut found: Option<(u32, u32)> = None;
        while let Some(item) = heap.pop() {
            let key = (item.node, item.prev);
            if let Some(&best) = gscore.get(&key) {
                if item.g > best + 1e-6 {
                    continue;
                }
            }
            if item.node == goal {
                found = Some(key);
                break;
            }
            let current_pos = self.nodes[item.node as usize];
            for &(next, edge_len) in self.adjacency.get(item.node as usize)? {
                if next == item.prev {
                    // 禁止原地折返；真需要掉头时由路线层显式构造 U 型弯
                    continue;
                }
                let mut cost = edge_len;
                if item.prev != u32::MAX {
                    let prev_pos = self.nodes[item.prev as usize];
                    let next_pos = self.nodes[next as usize];
                    let angle = angle_between(prev_pos, current_pos, next_pos);
                    if angle > std::f64::consts::PI * 5.0 / 6.0 {
                        cost += 35.0;
                    } else {
                        cost += 1.2 * (angle / (std::f64::consts::PI / 2.0)).powi(2);
                    }
                }
                cost += penalties.get(&(item.node, next)).copied().unwrap_or(0.0);
                let tentative = item.g + cost;
                let next_key = (next, item.node);
                if tentative + 1e-9 < *gscore.get(&next_key).unwrap_or(&f64::INFINITY) {
                    gscore.insert(next_key, tentative);
                    came.insert(next_key, key);
                    heap.push(HeapItem {
                        f: tentative + self.distance_xy(next, goal),
                        g: tentative,
                        node: next,
                        prev: item.node,
                    });
                }
            }
        }
        let (goal_node, goal_prev) = found?;
        let mut states = vec![(goal_node, goal_prev)];
        let mut current = (goal_node, goal_prev);
        while current != start_key {
            let previous = *came.get(&current)?;
            states.push(previous);
            current = previous;
        }
        states.reverse();
        let nodes: Vec<u32> = states.iter().map(|state| state.0).collect();
        let points: Vec<LngLat> = nodes
            .iter()
            .filter_map(|node| self.node_bd(*node))
            .collect();
        let length_m = nodes
            .windows(2)
            .map(|pair| self.distance_xy(pair[0], pair[1]))
            .sum();
        Some(RoutePath {
            nodes,
            points,
            length_m,
        })
    }

    fn distance_xy(&self, a: u32, b: u32) -> f64 {
        match (self.nodes.get(a as usize), self.nodes.get(b as usize)) {
            (Some(a), Some(b)) => distance(*a, *b),
            _ => f64::INFINITY,
        }
    }

    fn push_edge(&mut self, a: u32, b: u32) {
        if a == b || a as usize >= self.nodes.len() || b as usize >= self.nodes.len() {
            return;
        }
        let len = distance(self.nodes[a as usize], self.nodes[b as usize]);
        if len < MIN_EDGE_M {
            return;
        }
        self.edges.push(Edge { a, b, len });
    }

    fn rebuild_adjacency(&mut self) {
        self.adjacency = vec![Vec::new(); self.nodes.len()];
        for edge in &self.edges {
            self.adjacency[edge.a as usize].push((edge.b, edge.len));
            self.adjacency[edge.b as usize].push((edge.a, edge.len));
        }
    }

    fn split_edge(&mut self, edge_index: usize, point: [f64; 2]) -> u32 {
        let edge = self.edges[edge_index].clone();
        let node = if let Some(node) = self.node_within(point, NODE_SNAP_M) {
            node
        } else {
            let id = self.nodes.len() as u32;
            self.nodes.push(point);
            id
        };
        self.edges.remove(edge_index);
        if node != edge.a {
            self.push_edge(edge.a, node);
        }
        if node != edge.b {
            self.push_edge(node, edge.b);
        }
        node
    }

    fn node_within(&self, point: [f64; 2], tolerance: f64) -> Option<u32> {
        let mut best = None;
        let mut best_d = tolerance;
        for (index, node) in self.nodes.iter().enumerate() {
            let d = distance(*node, point);
            if d <= best_d {
                best_d = d;
                best = Some(index as u32);
            }
        }
        best
    }

    fn closest_edges(&self, point: [f64; 2], limit: usize) -> Vec<(usize, f64, f64)> {
        let mut out = Vec::new();
        for (index, edge) in self.edges.iter().enumerate() {
            let a = self.nodes[edge.a as usize];
            let b = self.nodes[edge.b as usize];
            let t = project_t(point, a, b);
            let q = lerp(a, b, t);
            out.push((index, distance(point, q), t));
        }
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
        out.truncate(limit);
        out
    }

    fn point_in_blocked(&self, point: [f64; 2]) -> bool {
        self.blocked_rings
            .iter()
            .any(|polygon| point_in_poly_xy(point, polygon))
    }

    fn connector_allowed(&self, a: [f64; 2], b: [f64; 2]) -> bool {
        let len = distance(a, b);
        let samples = ((len / 2.0).ceil() as usize).max(1);
        for step in 0..=samples {
            let t = step as f64 / samples as f64;
            let p = lerp(a, b, t);
            if !point_allowed(
                p,
                &self.area_rings,
                &self.blocked_rings,
                self.shrink_m,
                self.clearance_m,
            ) {
                return false;
            }
        }
        true
    }
}

/// 兼容入口：从路网图导出指定节点的 BD 坐标。
pub fn path_to_points(graph: &RoadGraph, path: &RoutePath) -> Vec<LngLat> {
    path.nodes
        .iter()
        .filter_map(|node| graph.node_bd(*node))
        .collect()
}

fn node_for(
    nodes: &mut Vec<[f64; 2]>,
    map: &mut HashMap<(i64, i64), u32>,
    point: [f64; 2],
) -> u32 {
    let key = node_key(point);
    if let Some(&index) = map.get(&key) {
        return index;
    }
    for dx in -1..=1i64 {
        for dy in -1..=1i64 {
            if let Some(&index) = map.get(&(key.0 + dx, key.1 + dy)) {
                if distance(nodes[index as usize], point) <= NODE_SNAP_M {
                    return index;
                }
            }
        }
    }
    let index = nodes.len() as u32;
    nodes.push(point);
    map.insert(key, index);
    index
}

fn node_key(point: [f64; 2]) -> (i64, i64) {
    ((point[0] / 0.5).round() as i64, (point[1] / 0.5).round() as i64)
}

fn lerp(a: [f64; 2], b: [f64; 2], t: f64) -> [f64; 2] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
}

fn project_t(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let len2 = dx * dx + dy * dy;
    if len2 <= 1e-12 {
        return 0.0;
    }
    (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
}

fn angle_between(prev: [f64; 2], current: [f64; 2], next: [f64; 2]) -> f64 {
    let ax = current[0] - prev[0];
    let ay = current[1] - prev[1];
    let bx = next[0] - current[0];
    let by = next[1] - current[1];
    let la = (ax * ax + ay * ay).sqrt();
    let lb = (bx * bx + by * by).sqrt();
    if la <= 1e-9 || lb <= 1e-9 {
        return 0.0;
    }
    let cos = ((ax * bx + ay * by) / (la * lb)).clamp(-1.0, 1.0);
    cos.acos()
}

fn orient(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn on_segment(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> bool {
    let eps = 1e-7;
    orient(a, b, p).abs() <= eps
        && p[0] >= a[0].min(b[0]) - eps
        && p[0] <= a[0].max(b[0]) + eps
        && p[1] >= a[1].min(b[1]) - eps
        && p[1] <= a[1].max(b[1]) + eps
}

/// 两线段的参数交点；平行/重合返回 None。
fn segment_intersection_params(
    s1: ([f64; 2], [f64; 2]),
    s2: ([f64; 2], [f64; 2]),
) -> Option<(f64, f64)> {
    let (p, p2) = s1;
    let (q, q2) = s2;
    let r = [p2[0] - p[0], p2[1] - p[1]];
    let s = [q2[0] - q[0], q2[1] - q[1]];
    let denom = r[0] * s[1] - r[1] * s[0];
    if denom.abs() < 1e-9 {
        // 端点接触
        for (a, b) in [(p, p2), (q, q2)] {
            for c in [q, q2] {
                if on_segment(a, b, c) {
                    return None;
                }
            }
        }
        return None;
    }
    let qpx = q[0] - p[0];
    let qpy = q[1] - p[1];
    let t = (qpx * s[1] - qpy * s[0]) / denom;
    let u = (qpx * r[1] - qpy * r[0]) / denom;
    if (-1e-9..=1.0 + 1e-9).contains(&t) && (-1e-9..=1.0 + 1e-9).contains(&u) {
        let t = t.clamp(0.0, 1.0);
        let u = u.clamp(0.0, 1.0);
        Some((t, u))
    } else {
        None
    }
}

fn point_in_poly_xy(point: [f64; 2], rings: &[Vec<[f64; 2]>]) -> bool {
    let Some(outer) = rings.first() else { return false };
    if !geo::point_in_ring_xy(point, outer) {
        return false;
    }
    for hole in &rings[1..] {
        if geo::point_in_ring_xy(point, hole) {
            return false;
        }
    }
    true
}

fn distance_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let t = project_t(p, a, b);
    distance(p, lerp(a, b, t))
}

fn distance_to_rings(p: [f64; 2], rings: &[Vec<[f64; 2]>]) -> f64 {
    let mut best = f64::INFINITY;
    for ring in rings {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..ring.len() {
            best = best.min(distance_to_segment(p, ring[i], ring[(i + 1) % ring.len()]));
        }
    }
    best
}

fn point_allowed(
    p: [f64; 2],
    area_rings: &[Vec<[f64; 2]>],
    blocked: &[Vec<Vec<[f64; 2]>>],
    shrink_m: f64,
    clearance_m: f64,
) -> bool {
    if !point_in_poly_xy(p, area_rings) {
        return false;
    }
    if shrink_m > 0.0 && distance_to_rings(p, area_rings) < shrink_m {
        return false;
    }
    for polygon in blocked {
        if point_in_poly_xy(p, polygon) {
            return false;
        }
        if distance_to_rings(p, polygon) < clearance_m {
            return false;
        }
    }
    true
}

fn edge_allowed(
    a: [f64; 2],
    b: [f64; 2],
    area_rings: &[Vec<[f64; 2]>],
    blocked: &[Vec<Vec<[f64; 2]>>],
    shrink_m: f64,
    clearance_m: f64,
) -> bool {
    let len = distance(a, b);
    let samples = ((len / 2.0).ceil() as usize).max(1);
    for step in 0..=samples {
        let t = step as f64 / samples as f64;
        let p = lerp(a, b, t);
        if !point_allowed(p, area_rings, blocked, shrink_m, clearance_m) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campus::{Polygon, Road};
    use std::time::Instant;

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
                name: "东西路".into(),
                path: vec![[114.297, 30.500], [114.303, 30.500]],
            },
            Road {
                id: 2,
                name: "南北路".into(),
                path: vec![[114.300, 30.497], [114.300, 30.503]],
            },
            Road {
                id: 3,
                name: "环路".into(),
                path: vec![
                    [114.298, 30.498],
                    [114.302, 30.498],
                    [114.302, 30.502],
                    [114.298, 30.502],
                    [114.298, 30.498],
                ],
            },
        ];
        campus
    }

    #[test]
    fn builds_connected_graph_and_routes_between_points() {
        let campus = test_campus();
        let graph = RoadGraph::build(&campus).unwrap();
        assert!(graph.node_count() >= 8);
        assert!(graph.edge_count() >= 8);
        let mut graph = graph;
        let start = graph.attach([114.2975, 30.5000], false).unwrap();
        let goal = graph.attach([114.3025, 30.5000], false).unwrap();
        let path = graph.shortest_path(start, goal).unwrap();
        assert!(path.length_m > 300.0, "len={}", path.length_m);
        assert_eq!(path.points.first().copied(), Some(graph.node_bd(start).unwrap()));
        assert_eq!(path.points.last().copied(), Some(graph.node_bd(goal).unwrap()));
    }

    #[test]
    fn attached_off_road_points_are_exact_endpoints() {
        let campus = test_campus();
        let mut graph = RoadGraph::build(&campus).unwrap();
        let start = graph.attach([114.2975, 30.5012], false).unwrap();
        let goal = graph.attach([114.3020, 30.4988], false).unwrap();
        let path = graph.shortest_path(start, goal).unwrap();
        assert_eq!(path.points[0], [114.2975, 30.5012]);
        assert_eq!(path.points[path.points.len() - 1], [114.3020, 30.4988]);
    }

    #[test]
    fn blocked_building_removes_nearby_edges() {
        let mut campus = test_campus();
        campus.clearance_min = 1.5;
        campus.allow_shrink_m = 0.0;
        campus.blocked.push(crate::campus::BlockedArea {
            id: 1,
            name: "教学楼".into(),
            polygon: Polygon::new(vec![
                [114.2995, 30.4998],
                [114.3005, 30.4998],
                [114.3005, 30.5002],
                [114.2995, 30.5002],
            ]),
        });
        let graph = RoadGraph::build(&campus).unwrap();
        // 交点附近被禁行区覆盖，但四周仍应连通
        let mut graph = graph;
        let start = graph.attach([114.2980, 30.5000], false).unwrap();
        let goal = graph.attach([114.3020, 30.5000], false).unwrap();
        let path = graph.shortest_path(start, goal).unwrap();
        let projection = Projection::for_polygon(&campus.area);
        let proj = projection;
        let blocked: Vec<Vec<[f64; 2]>> = campus.blocked[0]
            .polygon
            .rings()
            .map(|ring| geo::project_ring(ring, &proj))
            .collect();
        for point in &path.points {
            let p = proj.xy(*point);
            assert!(!point_in_poly_xy(p, &blocked), "path enters blocked: {point:?}");
        }
    }

    #[test]
    fn no_roads_is_an_error() {
        let mut campus = test_campus();
        campus.roads.clear();
        assert!(RoadGraph::build(&campus).is_err());
    }

    #[test]
    fn route_query_is_fast() {
        let mut campus = test_campus();
        campus.allow_shrink_m = 0.0;
        // 画 20 条横纵路，构造较大图
        for i in 0..10 {
            let y = 30.497 + i as f64 * 0.0006;
            campus.roads.push(Road {
                id: 100 + i as u64,
                name: format!("路{i}"),
                path: vec![[114.296, y], [114.304, y]],
            });
            let x = 114.296 + i as f64 * 0.0008;
            campus.roads.push(Road {
                id: 200 + i as u64,
                name: format!("纵{i}"),
                path: vec![[x, 30.496], [x, 30.504]],
            });
        }
        let mut graph = RoadGraph::build(&campus).unwrap();
        let start = graph.attach([114.297, 30.500], false).unwrap();
        let goal = graph.attach([114.303, 30.500], false).unwrap();
        let begin = Instant::now();
        for _ in 0..100 {
            assert!(graph.shortest_path(start, goal).is_some());
        }
        let elapsed = begin.elapsed();
        assert!(
            elapsed.as_millis() < 100,
            "100 次寻路耗时 {} ms",
            elapsed.as_millis()
        );
    }
}
