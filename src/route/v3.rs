//! V3 路线生成：道路优先混合图 + 严格任务顺序 + 束搜索绕行。
//!
//! 当前地图没有 corridor 数据，因此默认只允许：
//! - 道路走廊（主要通行）
//! - 任务点接入段（checkpoint_access，必要离路）
//! 不用“校园区域减禁行区”的剩余空间作为补距空间。

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use crate::campus::geo::{self, Projection};
use crate::campus::{Campus, LngLat};
use crate::track::generator;
use crate::track::geom::{round_to, Rng};
use crate::track::model::Track;

#[derive(Clone, Copy, Debug)]
pub struct V3Rules {
    pub arrival_radius_m: f64,
    pub min_distance_m: f64,
    pub preferred_max_m: f64,
    /// 没有道路宽度数据时的保守半宽，仅用于覆盖率统计。
    pub road_half_width_m: f64,
    pub road_cost: f64,
    pub access_cost: f64,
    pub switch_cost: f64,
    pub max_access_m: f64,
    pub beam_width: usize,
    pub max_rounds: usize,
    pub max_search_m: f64,
    pub seed: u64,
}

impl Default for V3Rules {
    fn default() -> Self {
        Self {
            arrival_radius_m: 10.0,
            min_distance_m: 2000.0,
            preferred_max_m: 2200.0,
            road_half_width_m: 2.0,
            road_cost: 1.0,
            access_cost: 3.0,
            switch_cost: 8.0,
            max_access_m: 200.0,
            beam_width: 32,
            max_rounds: 14,
            max_search_m: 2600.0,
            seed: 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct V3Candidate {
    pub points: Vec<LngLat>,
    pub task_points: Vec<LngLat>,
    pub distance_m: f64,
    pub p5_distance_m: f64,
    pub tail_distance_m: f64,
    pub road_coverage: f64,
    pub offroad_m: f64,
    pub max_offroad_m: f64,
    pub repeated_road_m: f64,
    pub open_area_m: f64,
    pub immediate_backtracks: usize,
    pub turns: usize,
    pub min_clearance_m: f64,
    pub score: f64,
    pub subscores: ScoreBreakdown,
    pub diagnostics: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScoreBreakdown {
    pub excess: f64,
    pub tail_share: f64,
    pub repeated: f64,
    pub offroad: f64,
    pub switches: f64,
    pub turns: f64,
    pub backtracks: f64,
    pub coverage: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeSource {
    Road,
    Access(usize),
    OpenArea,
}

#[derive(Clone, Debug)]
struct NetNode {
    xy: [f64; 2],
    bd: LngLat,
}

#[derive(Clone, Debug)]
struct NetEdge {
    a: usize,
    b: usize,
    length_m: f64,
    source: EdgeSource,
    geometry: Vec<[f64; 2]>,
    alive: bool,
}

struct Network {
    nodes: Vec<NetNode>,
    edges: Vec<NetEdge>,
    adjacency: Vec<Vec<usize>>,
    task_nodes: Vec<usize>,
    projection: Projection,
}

#[derive(Clone, Copy)]
struct HeapItem {
    f: f64,
    g: f64,
    node: usize,
    prev_edge: usize,
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.f == other.f && self.g == other.g && self.node == other.node
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
        other
            .f
            .partial_cmp(&self.f)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.g.partial_cmp(&self.g).unwrap_or(Ordering::Equal))
            .then_with(|| self.node.cmp(&other.node))
    }
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
}

fn point_segment_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let l2 = dx * dx + dy * dy;
    if l2 < 1e-9 {
        return dist(p, a);
    }
    let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / l2).clamp(0.0, 1.0);
    dist(p, [a[0] + t * dx, a[1] + t * dy])
}

fn polygon_distance_m(point: LngLat, ring: &[LngLat], projection: &Projection) -> f64 {
    if ring.len() < 2 {
        return f64::INFINITY;
    }
    let p = projection.xy(point);
    let mut best = f64::INFINITY;
    for index in 0..ring.len() {
        let a = projection.xy(ring[index]);
        let b = projection.xy(ring[(index + 1) % ring.len()]);
        best = best.min(point_segment_distance(p, a, b));
    }
    best
}

fn segment_circle_intersects(a: [f64; 2], b: [f64; 2], center: [f64; 2], radius: f64) -> bool {
    point_segment_distance(center, a, b) <= radius
}

fn edge_segment_intersects(
    geometry: &[[f64; 2]],
    center: [f64; 2],
    radius: f64,
) -> bool {
    geometry
        .windows(2)
        .any(|pair| segment_circle_intersects(pair[0], pair[1], center, radius))
}

fn angle_difference(a: [f64; 2], b: [f64; 2]) -> f64 {
    let na = (a[0] * a[0] + a[1] * a[1]).sqrt().max(1e-9);
    let nb = (b[0] * b[0] + b[1] * b[1]).sqrt().max(1e-9);
    ((a[0] * b[0] + a[1] * b[1]) / (na * nb))
        .clamp(-1.0, 1.0)
        .acos()
}

impl Network {
    fn build(campus: &Campus, tasks: &[LngLat], rules: &V3Rules) -> Result<Self, String> {
        if campus.roads.is_empty() {
            return Err("没有道路数据：V3 道路优先模式无法建图".into());
        }
        let projection = Projection::for_polygon(&campus.area);
        let mut nodes: Vec<NetNode> = Vec::new();
        let mut edges: Vec<NetEdge> = Vec::new();
        let mut node_map: HashMap<(i64, i64), usize> = HashMap::new();
        // 原始道路线段
        let mut raw: Vec<([f64; 2], [f64; 2])> = Vec::new();
        for road in &campus.roads {
            for pair in road.path.windows(2) {
                let a = projection.xy(pair[0]);
                let b = projection.xy(pair[1]);
                if dist(a, b) >= 0.1 {
                    raw.push((a, b));
                }
            }
        }
        if raw.is_empty() {
            return Err("道路没有有效线段".into());
        }
        // 道路两两交点，按参数切分
        let mut splits: Vec<Vec<f64>> = vec![vec![0.0, 1.0]; raw.len()];
        for i in 0..raw.len() {
            for j in (i + 1)..raw.len() {
                if let Some((ti, tj)) = segment_intersection_params(raw[i], raw[j]) {
                    splits[i].push(ti);
                    splits[j].push(tj);
                }
            }
        }
        for (index, (a, b)) in raw.iter().enumerate() {
            let mut ts = splits[index].clone();
            ts.sort_by(|x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal));
            ts.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
            for pair in ts.windows(2) {
                let p = lerp(*a, *b, pair[0]);
                let q = lerp(*a, *b, pair[1]);
                if dist(p, q) < 0.1 {
                    continue;
                }
                let na = node_for(&mut nodes, &mut node_map, p, projection);
                let nb = node_for(&mut nodes, &mut node_map, q, projection);
                if na == nb {
                    continue;
                }
                if min_segment_clearance(campus, p, q, projection) < 0.5 {
                    continue;
                }
                edges.push(NetEdge {
                    a: na,
                    b: nb,
                    length_m: dist(p, q),
                    source: EdgeSource::Road,
                    geometry: vec![p, q],
                    alive: true,
                });
            }
        }
        let mut net = Self {
            nodes,
            edges,
            adjacency: Vec::new(),
            task_nodes: Vec::new(),
            projection,
        };
        net.rebuild_adjacency();
        // 任务点接入：只允许从任务点到最近合法道路边，记录为 access 边
        for (task_index, task) in tasks.iter().enumerate() {
            net.attach_task(task_index, *task, campus, rules)?;
        }
        net.rebuild_adjacency();
        net.add_task_bypasses(campus, rules);
        net.add_plaza_shortcuts(campus, rules);
        net.rebuild_adjacency();
        Ok(net)
    }

    /// 广场是可穿行开阔区：只连接广场内部的图节点，不在此绕圈。
    fn add_plaza_shortcuts(&mut self, campus: &Campus, rules: &V3Rules) {
        if campus.plazas.is_empty() {
            return;
        }
        // 路网节点通常落在广场外侧；不能只连接“已经在广场内部”的节点。
        // 允许距离广场边界 35m 以内的道路节点作为入口/出口，再连接横穿广场。
        let mut candidates: Vec<usize> = Vec::new();
        for index in 0..self.nodes.len() {
            let point = self.nodes[index].bd;
            let near = campus.in_plaza(point) || campus.plazas.iter().any(|plaza| {
                polygon_distance_m(point, &plaza.polygon.outer, &self.projection) <= 35.0
            });
            if near {
                candidates.push(index);
            }
        }
        for i in 0..candidates.len() {
            for j in (i + 1)..candidates.len() {
                let a = self.nodes[candidates[i]].xy;
                let b = self.nodes[candidates[j]].xy;
                let length = dist(a, b);
                if length < 3.0 || length > rules.max_access_m * 1.5 {
                    continue;
                }
                let steps = ((length / 1.0).ceil() as usize).clamp(4, 320);
                let mut clear = true;
                let mut inside = 0;
                for step in 0..=steps {
                    let t = step as f64 / steps as f64;
                    let point = self.projection.lnglat([
                        a[0] + (b[0] - a[0]) * t,
                        a[1] + (b[1] - a[1]) * t,
                    ]);
                    if !campus.route_point_ok(point, 0.3) {
                        clear = false;
                        break;
                    }
                    if campus.in_plaza(point) {
                        inside += 1;
                    }
                }
                if clear && inside * 3 >= steps {
                    self.edges.push(NetEdge {
                        a: candidates[i],
                        b: candidates[j],
                        length_m: length,
                        source: EdgeSource::OpenArea,
                        geometry: vec![a, b],
                        alive: true,
                    });
                }
            }
        }
    }

    /// 固定任务在严格顺序下被后续任务到达圆封路时，允许任务点之间一条
    /// 短距离、净空合法的 open-area connector。它不是自由空间补距，只用于
    /// 任务阶段之间的必要绕行；候选诊断会单独统计长度。
    fn add_task_bypasses(&mut self, campus: &Campus, rules: &V3Rules) {
        for i in 0..self.task_nodes.len() {
            for j in (i + 1)..self.task_nodes.len() {
                let a = self.nodes[self.task_nodes[i]].xy;
                let b = self.nodes[self.task_nodes[j]].xy;
                let length = dist(a, b);
                if length < 5.0 || length > rules.max_access_m * 1.5 {
                    continue;
                }
                let steps = ((length / 1.0).ceil() as usize).clamp(8, 240);
                let mut clear = true;
                for step in 0..=steps {
                    let t = step as f64 / steps as f64;
                    let point = self.projection.lnglat([
                        a[0] + (b[0] - a[0]) * t,
                        a[1] + (b[1] - a[1]) * t,
                    ]);
                    if !campus.route_point_ok(point, 0.8) {
                        clear = false;
                        break;
                    }
                }
                if clear {
                    self.edges.push(NetEdge {
                        a: self.task_nodes[i],
                        b: self.task_nodes[j],
                        length_m: length,
                        source: EdgeSource::OpenArea,
                        geometry: vec![a, b],
                        alive: true,
                    });
                }
            }
        }
    }

    fn attach_task(
        &mut self,
        task_index: usize,
        task: LngLat,
        campus: &Campus,
        rules: &V3Rules,
    ) -> Result<(), String> {
        let target = self.projection.xy(task);
        let mut candidates: Vec<(f64, usize, [f64; 2])> = Vec::new();
        for (index, edge) in self.edges.iter().enumerate() {
            if !edge.alive || !matches!(edge.source, EdgeSource::Road) {
                continue;
            }
            let a = self.nodes[edge.a].xy;
            let b = self.nodes[edge.b].xy;
            let (projection_point, distance_to_edge) = project_point_segment(target, a, b);
            if distance_to_edge <= rules.max_access_m {
                candidates.push((distance_to_edge, index, projection_point));
            }
        }
        candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        let mut selected: Vec<(f64, usize, [f64; 2])> = Vec::new();
        for (distance_to_edge, edge_index, projection_point) in candidates {
            if selected.len() >= 3 {
                break;
            }
            if selected
                .iter()
                .any(|(_, index, _)| *index == edge_index)
            {
                continue;
            }
            let steps = ((distance_to_edge / 1.0).ceil() as usize).clamp(8, 240);
            let mut clear = true;
            for step in 0..=steps {
                let t = step as f64 / steps as f64;
                let point = self.projection.lnglat([
                    target[0] + (projection_point[0] - target[0]) * t,
                    target[1] + (projection_point[1] - target[1]) * t,
                ]);
                if !campus.route_point_ok(point, 0.8) {
                    clear = false;
                    break;
                }
            }
            if clear {
                selected.push((distance_to_edge, edge_index, projection_point));
            }
        }
        if selected.is_empty() {
            return Err(format!(
                "P{} 没有找到满足净空/区域要求的道路接入段（最近道路可能贴建筑或缺少入口）",
                task_index + 1
            ));
        }
        let task_node = self.nodes.len();
        self.nodes.push(NetNode {
            xy: target,
            bd: task,
        });
        for (distance_to_edge, edge_index, projection_point) in selected {
            let edge = self.edges[edge_index].clone();
            if !edge.alive {
                continue;
            }
            let a_xy = self.nodes[edge.a].xy;
            let b_xy = self.nodes[edge.b].xy;
            let projection_node = self.node_for_point(
                projection_point,
                self.projection.lnglat(projection_point),
            );
            self.edges[edge_index].alive = false;
            self.edges.push(NetEdge {
                a: edge.a,
                b: projection_node,
                length_m: dist(a_xy, projection_point),
                source: EdgeSource::Road,
                geometry: vec![a_xy, projection_point],
                alive: true,
            });
            self.edges.push(NetEdge {
                a: projection_node,
                b: edge.b,
                length_m: dist(projection_point, b_xy),
                source: EdgeSource::Road,
                geometry: vec![projection_point, b_xy],
                alive: true,
            });
            self.edges.push(NetEdge {
                a: projection_node,
                b: task_node,
                length_m: distance_to_edge,
                source: EdgeSource::Access(task_index),
                geometry: vec![projection_point, target],
                alive: true,
            });
        }
        self.task_nodes.push(task_node);
        self.rebuild_adjacency();
        Ok(())
    }

    fn node_for_point(&mut self, xy: [f64; 2], bd: LngLat) -> usize {
        for (index, node) in self.nodes.iter().enumerate() {
            if dist(node.xy, xy) < 0.4 {
                return index;
            }
        }
        let id = self.nodes.len();
        self.nodes.push(NetNode { xy, bd });
        id
    }

    fn rebuild_adjacency(&mut self) {
        self.adjacency = vec![Vec::new(); self.nodes.len()];
        for (index, edge) in self.edges.iter().enumerate() {
            if !edge.alive {
                continue;
            }
            self.adjacency[edge.a].push(index);
            self.adjacency[edge.b].push(index);
        }
    }

    fn edge_weight(&self, edge: &NetEdge, rules: &V3Rules) -> f64 {
        match edge.source {
            EdgeSource::Road => edge.length_m * rules.road_cost,
            EdgeSource::Access(_) => edge.length_m * rules.access_cost,
            EdgeSource::OpenArea => edge.length_m * rules.access_cost,
        }
    }

    fn edge_blocked(
        &self,
        edge: &NetEdge,
        forbidden_tasks: &[(usize, LngLat)],
        radius: f64,
    ) -> bool {
        if let EdgeSource::Access(index) = edge.source {
            if forbidden_tasks.iter().any(|(task_index, _)| *task_index == index) {
                return true;
            }
        }
        forbidden_tasks.iter().any(|(_, task)| {
            edge_segment_intersects(&edge.geometry, self.projection.xy(*task), radius)
        })
    }

    fn shortest(
        &self,
        start: usize,
        goal: usize,
        forbidden_tasks: &[(usize, LngLat)],
        radius: f64,
        banned_edges: &HashSet<usize>,
        rules: &V3Rules,
    ) -> Option<Vec<usize>> {
        if start >= self.nodes.len() || goal >= self.nodes.len() {
            return None;
        }
        let heuristic = |node: usize| {
            dist(self.nodes[node].xy, self.nodes[goal].xy) * rules.road_cost
        };
        let mut gscore: HashMap<(usize, usize), f64> = HashMap::new();
        let mut came: HashMap<(usize, usize), (usize, usize)> = HashMap::new();
        let mut heap = BinaryHeap::new();
        let start_key = (start, usize::MAX);
        gscore.insert(start_key, 0.0);
        heap.push(HeapItem {
            f: heuristic(start),
            g: 0.0,
            node: start,
            prev_edge: usize::MAX,
        });
        let mut found: Option<(usize, usize)> = None;
        while let Some(item) = heap.pop() {
            let key = (item.node, item.prev_edge);
            if let Some(&best) = gscore.get(&key) {
                if item.g > best + 1e-9 {
                    continue;
                }
            }
            if item.node == goal {
                found = Some(key);
                break;
            }
            for &edge_id in &self.adjacency[item.node] {
                if banned_edges.contains(&edge_id) {
                    continue;
                }
                let edge = &self.edges[edge_id];
                if !edge.alive || self.edge_blocked(edge, forbidden_tasks, radius) {
                    continue;
                }
                let next = if edge.a == item.node { edge.b } else { edge.a };
                let mut cost = self.edge_weight(edge, rules);
                if item.prev_edge != usize::MAX {
                    let previous = &self.edges[item.prev_edge];
                    let previous_source = previous.source;
                    let source_changed = std::mem::discriminant(&previous_source)
                        != std::mem::discriminant(&edge.source);
                    if source_changed {
                        cost += rules.switch_cost;
                    }
                    let p = self.nodes[item.node].xy;
                    let prev_other = if previous.a == item.node {
                        self.nodes[previous.b].xy
                    } else {
                        self.nodes[previous.a].xy
                    };
                    let next_xy = self.nodes[next].xy;
                    let angle = angle_difference(
                        [p[0] - prev_other[0], p[1] - prev_other[1]],
                        [next_xy[0] - p[0], next_xy[1] - p[1]],
                    );
                    cost += 4.0 * (angle / std::f64::consts::PI).powi(2);
                }
                let tentative = item.g + cost;
                let next_key = (next, edge_id);
                if tentative + 1e-9 < *gscore.get(&next_key).unwrap_or(&f64::INFINITY) {
                    gscore.insert(next_key, tentative);
                    came.insert(next_key, key);
                    heap.push(HeapItem {
                        f: tentative + heuristic(next),
                        g: tentative,
                        node: next,
                        prev_edge: edge_id,
                    });
                }
            }
        }
        let mut key = found?;
        let mut path = Vec::new();
        while key != start_key {
            let (node, edge) = key;
            if edge == usize::MAX {
                return None;
            }
            path.push(edge);
            key = *came.get(&(node, edge))?;
        }
        path.reverse();
        Some(path)
    }

    fn path_length(&self, edges: &[usize]) -> f64 {
        edges.iter().map(|id| self.edges[*id].length_m).sum()
    }
}

fn node_for(
    nodes: &mut Vec<NetNode>,
    node_map: &mut HashMap<(i64, i64), usize>,
    xy: [f64; 2],
    projection: Projection,
) -> usize {
    let key = ((xy[0] / 0.5).round() as i64, (xy[1] / 0.5).round() as i64);
    if let Some(index) = node_map.get(&key) {
        return *index;
    }
    let id = nodes.len();
    nodes.push(NetNode {
        xy,
        bd: projection.lnglat(xy),
    });
    node_map.insert(key, id);
    id
}

fn lerp(a: [f64; 2], b: [f64; 2], t: f64) -> [f64; 2] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

fn min_segment_clearance(campus: &Campus, a: [f64; 2], b: [f64; 2], projection: Projection) -> f64 {
    let mut best = f64::INFINITY;
    let length = dist(a, b);
    let steps = ((length / 1.0).ceil() as usize).clamp(2, 300);
    for step in 0..=steps {
        let t = step as f64 / steps as f64;
        let point = projection.lnglat([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
        for blocked in &campus.blocked {
            best = best.min(geo::distance_to_polygon_boundary(
                point,
                &blocked.polygon,
                &projection,
            ));
        }
    }
    best
}

fn segment_intersection_params(
    a: ([f64; 2], [f64; 2]),
    b: ([f64; 2], [f64; 2]),
) -> Option<(f64, f64)> {
    let p = a.0;
    let r = [a.1[0] - a.0[0], a.1[1] - a.0[1]];
    let q = b.0;
    let s = [b.1[0] - b.0[0], b.1[1] - b.0[1]];
    let denominator = r[0] * s[1] - r[1] * s[0];
    if denominator.abs() < 1e-12 {
        return None;
    }
    let qp = [q[0] - p[0], q[1] - p[1]];
    let t = (qp[0] * s[1] - qp[1] * s[0]) / denominator;
    let u = (qp[0] * r[1] - qp[1] * r[0]) / denominator;
    if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
        Some((t, u))
    } else {
        None
    }
}

fn project_point_segment(
    point: [f64; 2],
    a: [f64; 2],
    b: [f64; 2],
) -> ([f64; 2], f64) {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let l2 = dx * dx + dy * dy;
    if l2 < 1e-9 {
        return (a, dist(point, a));
    }
    let t = (((point[0] - a[0]) * dx + (point[1] - a[1]) * dy) / l2).clamp(0.0, 1.0);
    let projected = [a[0] + t * dx, a[1] + t * dy];
    (projected, dist(point, projected))
}

#[derive(Clone)]
struct State {
    legs: Vec<Vec<usize>>,
    tail: Vec<usize>,
    p5_length_m: f64,
    distance_m: f64,
    score: f64,
}

fn state_usage(net: &Network, state: &State) -> HashMap<usize, u32> {
    let mut usage = HashMap::new();
    for edge in state
        .legs
        .iter()
        .flat_map(|leg| leg.iter())
        .chain(state.tail.iter())
    {
        *usage.entry(*edge).or_insert(0) += 1;
    }
    let _ = net;
    usage
}

fn oriented_nodes(net: &Network, start: usize, edges: &[usize]) -> Option<Vec<usize>> {
    let mut nodes = vec![start];
    let mut current = start;
    for edge_id in edges {
        let edge = &net.edges[*edge_id];
        let next = if edge.a == current {
            edge.b
        } else if edge.b == current {
            edge.a
        } else {
            return None;
        };
        nodes.push(next);
        current = next;
    }
    Some(nodes)
}

fn oriented_points(net: &Network, start: usize, edges: &[usize]) -> Option<Vec<LngLat>> {
    let nodes = oriented_nodes(net, start, edges)?;
    let mut points = Vec::new();
    for (index, edge_id) in edges.iter().enumerate() {
        let edge = &net.edges[*edge_id];
        let mut geometry = edge.geometry.clone();
        if nodes[index] == edge.b && nodes[index + 1] == edge.a {
            geometry.reverse();
        }
        for xy in geometry {
            let bd = net.projection.lnglat(xy);
            if points.last().copied() != Some(bd) {
                points.push(bd);
            }
        }
    }
    Some(points)
}

fn smooth_route(
    points: &[LngLat],
    tasks: &[LngLat],
    campus: &Campus,
    projection: &Projection,
) -> Vec<LngLat> {
    let mut current = points.to_vec();
    for _ in 0..2 {
        let mut next = current.clone();
        for index in 1..current.len().saturating_sub(1) {
            let point = current[index];
            if tasks
                .iter()
                .any(|task| projection.distance_m(*task, point) < 0.8)
            {
                continue;
            }
            let a = projection.xy(current[index - 1]);
            let b = projection.xy(point);
            let c = projection.xy(current[index + 1]);
            let smoothed = projection.lnglat([
                0.5 * b[0] + 0.25 * a[0] + 0.25 * c[0],
                0.5 * b[1] + 0.25 * a[1] + 0.25 * c[1],
            ]);
            if campus.route_point_ok(smoothed, 0.5) {
                next[index] = smoothed;
            }
        }
        current = next;
    }
    current
}

fn route_length(points: &[LngLat], projection: &Projection) -> f64 {
    points
        .windows(2)
        .map(|pair| projection.distance_m(pair[0], pair[1]))
        .sum()
}
fn naturalize_route(
    points: &[LngLat],
    tasks: &[LngLat],
    campus: &Campus,
    projection: &Projection,
    seed: u64,
) -> Vec<LngLat> {
    let mut rng = Rng::new(seed ^ 0x9E37_79B9_7F4A_7C15);
    let phase = rng.uniform(0.0, std::f64::consts::TAU);
    let amplitude = rng.uniform(0.50, 0.90);
    let wavelength = rng.uniform(35.0, 60.0);
    let mut out = vec![points[0]];
    let mut travelled = 0.0;
    for pair in points.windows(2) {
        let a = projection.xy(pair[0]);
        let b = projection.xy(pair[1]);
        let length = dist(a, b);
        if length < 0.1 {
            continue;
        }
        let steps = ((length / 8.0).ceil() as usize).max(1);
        let dx = (b[0] - a[0]) / length;
        let dy = (b[1] - a[1]) / length;
        let normal = [-dy, dx];
        for step in 1..=steps {
            let t = step as f64 / steps as f64;
            let base = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let s = travelled + length * t;
            let offset = amplitude * (s / wavelength * std::f64::consts::TAU + phase).sin();
            let candidate = projection.lnglat([base[0] + normal[0] * offset, base[1] + normal[1] * offset]);
            let is_task = tasks
                .iter()
                .any(|task| projection.distance_m(*task, candidate) < 1.0);
            let point = if !is_task && campus.route_point_ok(candidate, 0.3) {
                candidate
            } else {
                projection.lnglat(base)
            };
            if out.last().copied() != Some(point) {
                out.push(point);
            }
        }
        travelled += length;
    }
    if let Some(last) = points.last() {
        if out.last().copied() != Some(*last) {
            out.push(*last);
        }
    }
    out
}

fn strict_violation(
    net: &Network,
    points: &[LngLat],
    tasks: &[LngLat],
    radius: f64,
) -> Option<String> {
    let projection = net.projection;
    let mut next = 0usize;
    for pair in points.windows(2) {
        let a = projection.xy(pair[0]);
        let b = projection.xy(pair[1]);
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let length2 = dx * dx + dy * dy;
        let mut events: Vec<(f64, usize)> = Vec::new();
        for (index, task) in tasks.iter().enumerate().skip(next) {
            let center = projection.xy(*task);
            let distance = point_segment_distance(center, a, b);
            if distance > radius {
                continue;
            }
            let t = if length2 < 1e-9 {
                0.0
            } else {
                (((center[0] - a[0]) * dx + (center[1] - a[1]) * dy) / length2)
                    .clamp(0.0, 1.0)
            };
            events.push((t, index));
        }
        events.sort_by(|x, y| x.0.partial_cmp(&y.0).unwrap_or(Ordering::Equal));
        for (_, index) in events {
            if index == next {
                next += 1;
            } else if index > next {
                return Some(format!(
                    "严格顺序违规：尚未完成 P{} 就进入 P{} 的到达圆",
                    next + 1,
                    index + 1
                ));
            }
        }
    }
    if next < tasks.len() {
        Some(format!("没有按顺序经过 P{}", next + 1))
    } else {
        None
    }
}

fn actual_turns(points: &[LngLat], projection: &Projection) -> usize {
    let mut count = 0usize;
    let mut last_turn = [f64::INFINITY; 2];
    for triple in points.windows(3) {
        let a = projection.xy(triple[0]);
        let b = projection.xy(triple[1]);
        let c = projection.xy(triple[2]);
        let angle = angle_difference([b[0] - a[0], b[1] - a[1]], [c[0] - b[0], c[1] - b[1]]);
        if angle > 15.0f64.to_radians() {
            if dist(b, last_turn) > 10.0 {
                count += 1;
            }
            last_turn = b;
        }
    }
    count
}

fn immediate_backtracks(points: &[LngLat], projection: &Projection) -> usize {
    let mut count = 0usize;
    for triple in points.windows(3) {
        let a = projection.xy(triple[0]);
        let b = projection.xy(triple[1]);
        let c = projection.xy(triple[2]);
        let angle = angle_difference([b[0] - a[0], b[1] - a[1]], [c[0] - b[0], c[1] - b[1]]);
        if angle > 150.0f64.to_radians() {
            count += 1;
        }
    }
    count
}

fn road_coverage(
    campus: &Campus,
    points: &[LngLat],
    projection: &Projection,
    half_width_m: f64,
) -> (f64, f64, f64) {
    let mut total = 0.0;
    let mut covered = 0.0;
    let mut offroad = 0.0;
    let mut continuous = 0.0;
    let mut max_continuous: f64 = 0.0;
    let mut roads: Vec<([f64; 2], [f64; 2])> = Vec::new();
    for road in &campus.roads {
        for pair in road.path.windows(2) {
            roads.push((projection.xy(pair[0]), projection.xy(pair[1])));
        }
    }
    for pair in points.windows(2) {
        let a = projection.xy(pair[0]);
        let b = projection.xy(pair[1]);
        let length = dist(a, b);
        if length < 0.1 {
            continue;
        }
        let steps = ((length / 2.0).ceil() as usize).max(1);
        for step in 0..=steps {
            let t = step as f64 / steps as f64;
            let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            let on_road = roads
                .iter()
                .any(|(ra, rb)| point_segment_distance(p, *ra, *rb) <= half_width_m);
            let piece = length / steps as f64;
            total += piece;
            if on_road {
                covered += piece;
                continuous = 0.0;
            } else {
                offroad += piece;
                continuous += piece;
                max_continuous = max_continuous.max(continuous);
            }
        }
    }
    let coverage = if total > 0.0 { covered / total } else { 0.0 };
    let _ = offroad;
    (coverage, total - covered, max_continuous)
}

fn min_clearance(campus: &Campus, points: &[LngLat], projection: &Projection) -> (f64, LngLat) {
    let mut best = f64::INFINITY;
    let mut best_point = points[0];
    for pair in points.windows(2) {
        let a = projection.xy(pair[0]);
        let b = projection.xy(pair[1]);
        let length = dist(a, b);
        let steps = ((length / 2.0).ceil() as usize).max(1);
        for step in 0..=steps {
            let t = step as f64 / steps as f64;
            let p = projection.lnglat([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            for blocked in &campus.blocked {
                let d = geo::distance_to_polygon_boundary(p, &blocked.polygon, projection);
                if d < best {
                    best = d;
                    best_point = p;
                }
            }
        }
    }
    (if best.is_finite() { best } else { 999.0 }, best_point)
}

fn score_state(net: &Network, state: &State, rules: &V3Rules) -> f64 {
    let usage = state_usage(net, state);
    let mut repeated = 0.0;
    let mut switches = 0usize;
    let mut previous: Option<EdgeSource> = None;
    for edge_id in state
        .legs
        .iter()
        .flat_map(|leg| leg.iter())
        .chain(state.tail.iter())
    {
        let edge = &net.edges[*edge_id];
        if usage.get(edge_id).copied().unwrap_or(0) > 1
            && matches!(edge.source, EdgeSource::Road)
        {
            repeated += edge.length_m * (usage[edge_id] as f64 - 1.0);
        }
        if let Some(previous) = previous {
            if std::mem::discriminant(&previous) != std::mem::discriminant(&edge.source) {
                switches += 1;
            }
        }
        previous = Some(edge.source);
    }
    let tail_m = net.path_length(&state.tail);
    let excess = (state.distance_m - rules.min_distance_m).max(0.0);
    let tail_share = if state.distance_m > 0.0 {
        tail_m / state.distance_m
    } else {
        0.0
    };
    let mut score = excess * 0.5;
    score += tail_share * 120.0;
    score += repeated * 0.8;
    score += switches as f64 * 3.0;
    score
}

fn rebuild_state(net: &Network, legs: Vec<Vec<usize>>, tail: Vec<usize>, rules: &V3Rules) -> State {
    let p5_length_m = legs.iter().map(|leg| net.path_length(leg)).sum();
    let distance_m = p5_length_m + net.path_length(&tail);
    let mut state = State {
        legs,
        tail,
        p5_length_m,
        distance_m,
        score: 0.0,
    };
    state.score = score_state(net, &state, rules);
    state
}

fn base_state(net: &Network, tasks: &[LngLat], rules: &V3Rules) -> Result<State, String> {
    let mut legs = Vec::new();
    for index in 0..tasks.len().saturating_sub(1) {
        let start = net.task_nodes[index];
        let goal = net.task_nodes[index + 1];
        let forbidden: Vec<(usize, LngLat)> = tasks
            .iter()
            .enumerate()
            .skip(index + 2)
            .map(|(task_index, task)| (task_index, *task))
            .collect();
        let path = net
            .shortest(
                start,
                goal,
                &forbidden,
                rules.arrival_radius_m,
                &HashSet::new(),
                rules,
            )
            .ok_or_else(|| {
                format!(
                    "P{} → P{} 在严格顺序下不可达（道路或接入缺失）",
                    index + 1,
                    index + 2
                )
            })?;
        legs.push(path);
    }
    Ok(rebuild_state(net, legs, Vec::new(), rules))
}

fn random_subpath(rng: &mut Rng, len: usize) -> Option<(usize, usize)> {
    if len < 3 {
        return None;
    }
    let a = (rng.uniform(0.0, (len - 2) as f64) as usize).min(len - 2);
    let max_b = (a + 8).min(len - 1);
    if max_b <= a + 1 {
        return None;
    }
    let b = a + 2 + (rng.uniform(0.0, (max_b - a - 1) as f64) as usize);
    Some((a, b.min(len - 1)))
}

fn replace_state(
    net: &Network,
    state: &State,
    tasks: &[LngLat],
    rules: &V3Rules,
    rng: &mut Rng,
) -> Option<State> {
    if state.legs.is_empty() {
        return None;
    }
    let leg_index = (rng.uniform(0.0, state.legs.len() as f64) as usize)
        .min(state.legs.len() - 1);
    let leg = &state.legs[leg_index];
    let (a, b) = random_subpath(rng, leg.len())?;
    let leg_start = *net.task_nodes.get(leg_index)?;
    let nodes = oriented_nodes(net, leg_start, leg)?;
    let start = *nodes.get(a)?;
    let goal = *nodes.get(b + 1)?;
    let banned: HashSet<usize> = leg[a..=b].iter().copied().collect();
    let forbidden: Vec<(usize, LngLat)> = tasks
        .iter()
        .enumerate()
        .skip(leg_index + 2)
        .map(|(task_index, task)| (task_index, *task))
        .collect();
    let replacement = net.shortest(
        start,
        goal,
        &forbidden,
        rules.arrival_radius_m,
        &banned,
        rules,
    )?;
    let mut new_leg = leg[..a].to_vec();
    new_leg.extend(replacement);
    new_leg.extend(leg[(b + 1)..].iter().copied());
    if new_leg.is_empty() || new_leg == *leg {
        return None;
    }
    let mut legs = state.legs.clone();
    legs[leg_index] = new_leg;
    let candidate = rebuild_state(net, legs, state.tail.clone(), rules);
    if candidate.distance_m <= state.distance_m + 5.0 {
        return None;
    }
    Some(candidate)
}

fn cycle_state(
    net: &Network,
    state: &State,
    tasks: &[LngLat],
    rules: &V3Rules,
    rng: &mut Rng,
) -> Option<State> {
    if state.legs.is_empty() {
        return None;
    }
    let leg_index = (rng.uniform(0.0, state.legs.len() as f64) as usize)
        .min(state.legs.len() - 1);
    let leg = &state.legs[leg_index];
    if leg.is_empty() {
        return None;
    }
    let leg_start = *net.task_nodes.get(leg_index)?;
    let nodes = oriented_nodes(net, leg_start, leg)?;
    if nodes.len() < 3 {
        return None;
    }
    let position = 1 + (rng.uniform(0.0, (nodes.len() - 2) as f64) as usize);
    let position = position.min(nodes.len() - 2);
    let node = nodes[position];
    let mut candidates: Vec<usize> = net.adjacency[node]
        .iter()
        .copied()
        .filter(|id| {
            net.edges[*id].alive
                && matches!(net.edges[*id].source, EdgeSource::Road)
                && !leg.contains(id)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let index = (rng.uniform(0.0, candidates.len() as f64) as usize).min(candidates.len() - 1);
    let first = candidates.swap_remove(index);
    let first_edge = &net.edges[first];
    let neighbor = if first_edge.a == node {
        first_edge.b
    } else {
        first_edge.a
    };
    let mut banned = HashSet::new();
    banned.insert(first);
    let forbidden: Vec<(usize, LngLat)> = tasks
        .iter()
        .enumerate()
        .skip(leg_index + 2)
        .map(|(task_index, task)| (task_index, *task))
        .collect();
    let back = net.shortest(
        neighbor,
        node,
        &forbidden,
        rules.arrival_radius_m,
        &banned,
        rules,
    )?;
    if back.is_empty() {
        return None;
    }
    // 环 = node→neighbor→...→node，插在节点对应的前后缀之间。
    let mut cycle = vec![first];
    cycle.extend(back);
    let mut new_leg = leg[..position].to_vec();
    new_leg.extend(cycle);
    new_leg.extend(leg[position..].iter().copied());
    let mut legs = state.legs.clone();
    legs[leg_index] = new_leg;
    let candidate = rebuild_state(net, legs, state.tail.clone(), rules);
    if candidate.distance_m <= state.distance_m + 20.0 {
        return None;
    }
    Some(candidate)
}

fn extend_tail(
    net: &Network,
    state: &State,
    _tasks: &[LngLat],
    rules: &V3Rules,
    rng: &mut Rng,
) -> Option<State> {
    if state.p5_length_m < rules.min_distance_m * 0.10 {
        return None;
    }
    let task_start = net.task_nodes.last().copied()?;
    let start = if state.tail.is_empty() {
        task_start
    } else {
        *oriented_nodes(net, task_start, &state.tail)?.last()?
    };
    let mut targets: Vec<usize> = (0..net.nodes.len())
        .filter(|node| {
            net.adjacency[*node]
                .iter()
                .any(|id| matches!(net.edges[*id].source, EdgeSource::Road))
                && dist(net.nodes[*node].xy, net.nodes[start].xy) > 80.0
        })
        .collect();
    if targets.is_empty() {
        return None;
    }
    let index = (rng.uniform(0.0, targets.len() as f64) as usize).min(targets.len() - 1);
    let target = targets.swap_remove(index);
    let path = net.shortest(
        start,
        target,
        &[],
        rules.arrival_radius_m,
        &HashSet::new(),
        rules,
    )?;    let path_len = net.path_length(&path);
    if path_len < 40.0 || path_len > 500.0 {
        return None;
    }
    if state.distance_m + path_len > rules.max_search_m {
        return None;
    }
    let mut tail = state.tail.clone();
    tail.extend(path);
    let candidate = rebuild_state(net, state.legs.clone(), tail, rules);
    if candidate.distance_m <= state.distance_m + 20.0 {
        return None;
    }
    Some(candidate)
}

fn candidate_from_state(
    net: &Network,
    state: &State,
    tasks: &[LngLat],
    campus: &Campus,
    rules: &V3Rules,
) -> Result<V3Candidate, String> {
    let mut points = Vec::new();
    for (leg_index, leg) in state.legs.iter().enumerate() {
        let start = *net
            .task_nodes
            .get(leg_index)
            .ok_or_else(|| format!("缺少 P{} 接入节点", leg_index + 1))?;
        let leg_points = oriented_points(net, start, leg)
            .ok_or_else(|| format!("第 {} 段路线边序列不连通", leg_index + 1))?;
        for point in leg_points {
            if points.last().copied() != Some(point) {
                points.push(point);
            }
        }
    }
    if !state.tail.is_empty() {
        let start = *net
            .task_nodes
            .last()
            .ok_or_else(|| "缺少最后一个任务接入节点".to_string())?;
        let tail_points = oriented_points(net, start, &state.tail)
            .ok_or_else(|| "结束段边序列不连通".to_string())?;
        for point in tail_points {
            if points.last().copied() != Some(point) {
                points.push(point);
            }
        }
    }
    if points.len() < 2 {
        return Err("路线为空".into());
    }
    let smoothed = smooth_route(&points, tasks, campus, &net.projection);
    if route_length(&smoothed, &net.projection) >= rules.min_distance_m
        && strict_violation(net, &smoothed, tasks, rules.arrival_radius_m).is_none()
    {
        points = smoothed;
    }
    // 只为视觉自然度做轻微横向波动；此时长度已经满足硬约束，不用抖动补距。
    if route_length(&points, &net.projection) >= rules.min_distance_m {
        let natural = naturalize_route(&points, tasks, campus, &net.projection, rules.seed);
        if route_length(&natural, &net.projection) >= rules.min_distance_m
            && strict_violation(net, &natural, tasks, rules.arrival_radius_m).is_none()
        {
            points = natural;
        }
    }
    let distance_m = points
        .windows(2)
        .map(|pair| net.projection.distance_m(pair[0], pair[1]))
        .sum::<f64>();
    let p5_distance_m = state.p5_length_m;
    let tail_distance_m = net.path_length(&state.tail);
    if distance_m < rules.min_distance_m {
        return Err(format!("长度 {:.1}m 低于 2000m", distance_m));
    }
    if let Some(reason) = strict_violation(net, &points, tasks, rules.arrival_radius_m) {
        return Err(reason);
    }
    let usage = state_usage(net, state);
    let mut repeated_road_m = 0.0;
    let mut open_area_m = 0.0;
    for (edge_id, count) in &usage {
        let edge = &net.edges[*edge_id];
        match edge.source {
            EdgeSource::Road if *count > 1 => {
                repeated_road_m += edge.length_m * (*count as f64 - 1.0);
            }
            EdgeSource::OpenArea => {
                open_area_m += edge.length_m * *count as f64;
            }
            _ => {}
        }
    }
    let (road_coverage, offroad_m, max_offroad_m) =
        road_coverage(campus, &points, &net.projection, rules.road_half_width_m);
    let turns = actual_turns(&points, &net.projection);
    let immediate = immediate_backtracks(&points, &net.projection);
    let (clearance, _clearance_point) = min_clearance(campus, &points, &net.projection);
    if clearance < 0.5 {
        return Err(format!("最小净空 {:.2}m 过小", clearance));
    }
    let excess = (distance_m - rules.min_distance_m).max(0.0);
    let tail_share = if distance_m > 0.0 {
        tail_distance_m / distance_m
    } else {
        0.0
    };
    let mut subscores = ScoreBreakdown {
        excess: excess * 0.5,
        tail_share: tail_share * 120.0,
        repeated: repeated_road_m * 0.8,
        offroad: offroad_m * 0.25,
        switches: 0.0,
        turns: turns as f64 * 1.0,
        backtracks: immediate as f64 * 40.0,
        coverage: ((0.85 - road_coverage).max(0.0)) * 200.0,
    };
    // 开关道路/接入的次数
    let mut switches = 0usize;
    let mut previous: Option<EdgeSource> = None;
    for edge_id in state
        .legs
        .iter()
        .flat_map(|leg| leg.iter())
        .chain(state.tail.iter())
    {
        let edge = &net.edges[*edge_id];
        if let Some(previous) = previous {
            if std::mem::discriminant(&previous) != std::mem::discriminant(&edge.source) {
                switches += 1;
            }
        }
        previous = Some(edge.source);
    }
    subscores.switches = switches as f64 * 2.0;
    let score = subscores.excess
        + subscores.tail_share
        + subscores.repeated
        + subscores.offroad
        + subscores.switches
        + subscores.turns
        + subscores.backtracks
        + subscores.coverage;
    let mut diagnostics = Vec::new();
    diagnostics.push(format!(
        "道路无宽度数据，覆盖率按半宽 {:.1}m 假设统计",
        rules.road_half_width_m
    ));
    diagnostics.push(format!(
        "任务接入段离路总长 {:.1}m；开阔区任务间 bypass {:.1}m；P5 后距离 {:.1}m（{:.1}%）",
        offroad_m,
        open_area_m,
        tail_distance_m,
        tail_share * 100.0
    ));
    for (index, node) in net.task_nodes.iter().enumerate() {
        let access = net.adjacency[*node]
            .iter()
            .filter(|id| matches!(net.edges[**id].source, EdgeSource::Access(_)))
            .map(|id| net.edges[*id].length_m)
            .fold(0.0, f64::max);
        if access > 0.5 {
            diagnostics.push(format!("P{} 接入道路 {:.1}m", index + 1, access));
        }
    }
    Ok(V3Candidate {
        points,
        task_points: tasks.to_vec(),
        distance_m,
        p5_distance_m,
        tail_distance_m,
        road_coverage,
        offroad_m,
        max_offroad_m,
        repeated_road_m,
        open_area_m,
        immediate_backtracks: immediate,
        turns,
        min_clearance_m: clearance,
        score,
        subscores,
        diagnostics,
    })
}

/// 道路长度加权的参考行程选点：先走一条连续道路参考路线，再沿弧长取 P1–P5。
/// 仅用于系统新生成任务；外部给定任务必须走 `generate`，不得重排。
pub fn generate_tasks(
    campus: &Campus,
    count: usize,
    rules: V3Rules,
) -> Result<Vec<LngLat>, String> {
    if count < 2 {
        return Err("任务点数量至少为 2".into());
    }
    let net = Network::build(campus, &[], &rules)?;
    if net.adjacency.is_empty() {
        return Err("道路图为空，无法生成任务点".into());
    }
    let mut rng = Rng::new(rules.seed);
    for _attempt in 0..30 {
        let start_node = random_road_node(&net, &mut rng)?;
        let mut current = start_node;
        let mut points = vec![net.nodes[start_node].bd];
        let mut length = 0.0;
        for _ in 0..8 {
            let target = random_road_node(&net, &mut rng)?;
            if target == current {
                continue;
            }
            let Some(edges) = net.shortest(current, target, &[], rules.arrival_radius_m, &HashSet::new(), &rules)
            else {
                continue;
            };
            if let Some(leg_points) = oriented_points(&net, current, &edges) {
                for point in leg_points.into_iter().skip(1) {
                    if points.last().copied() != Some(point) {
                        points.push(point);
                    }
                }
                length += net.path_length(&edges);
                current = target;
            }
            if length >= 1500.0 {
                break;
            }
        }
        if length < 500.0 || points.len() < 4 {
            continue;
        }
        let total = points
            .windows(2)
            .map(|pair| net.projection.distance_m(pair[0], pair[1]))
            .sum::<f64>();
        if total < 500.0 {
            continue;
        }
        let mut selected = Vec::new();
        for index in 0..count {
            let fraction = (index as f64 + 0.5) / count as f64;
            let target = (fraction + rng.uniform(-0.04, 0.04)).clamp(0.05, 0.95) * total;
            let Some(point) = point_at_distance(&net.projection, &points, target) else {
                break;
            };
            if !campus.route_point_ok(point, 1.0) {
                break;
            }
            if selected
                .iter()
                .any(|existing| net.projection.distance_m(*existing, point) < 3.0 * rules.arrival_radius_m)
            {
                break;
            }
            selected.push(point);
        }
        if selected.len() == count {
            return Ok(selected);
        }
    }
    Err("无法从道路参考行程生成彼此不重叠的任务点；请补道路或减少任务数".into())
}

fn random_road_node(net: &Network, rng: &mut Rng) -> Result<usize, String> {
    let candidates: Vec<usize> = (0..net.nodes.len())
        .filter(|node| {
            net.adjacency[*node]
                .iter()
                .any(|id| matches!(net.edges[*id].source, EdgeSource::Road))
        })
        .collect();
    if candidates.is_empty() {
        return Err("没有道路节点".into());
    }
    let index = (rng.uniform(0.0, candidates.len() as f64) as usize).min(candidates.len() - 1);
    Ok(candidates[index])
}

fn point_at_distance(projection: &Projection, points: &[LngLat], target: f64) -> Option<LngLat> {
    let mut travelled = 0.0;
    for pair in points.windows(2) {
        let segment = projection.distance_m(pair[0], pair[1]);
        if travelled + segment >= target {
            let t = if segment > 1e-9 {
                (target - travelled) / segment
            } else {
                0.0
            };
            return Some([
                pair[0][0] + (pair[1][0] - pair[0][0]) * t,
                pair[0][1] + (pair[1][1] - pair[0][1]) * t,
            ]);
        }
        travelled += segment;
    }
    points.last().copied()
}

/// 把 V3 路线套进 1Hz 协议轨迹；不做任务点批量吸附。
pub fn build_track(
    campus: &Campus,
    candidate: &V3Candidate,
    pace_sec_per_km: f64,
    seed: u64,
    start_ms: i64,
) -> Track {
    let projection = Projection::for_polygon(&campus.area);
    let dist = round_to(candidate.distance_m, 3);
    let duration = ((dist / 1000.0) * pace_sec_per_km.clamp(150.0, 600.0)).round() as i64;
    let center = candidate
        .task_points
        .first()
        .copied()
        .unwrap_or([0.0, 0.0]);
    let ring_points: Vec<(f64, f64)> = candidate
        .task_points
        .iter()
        .map(|point| (point[1], point[0]))
        .collect();
    let mut track = generator::build(
        dist,
        duration.max(60),
        seed,
        (center[1], center[0]),
        start_ms,
        &ring_points,
    );
    let sampler = Sampler::new(&candidate.points, projection);
    for location in &mut track.locations {
        let point = sampler.at(location.totalDis);
        location.gLat = round_to(point[1], 7);
        location.gLng = round_to(point[0], 7);
    }
    if let Some(first) = candidate.points.first() {
        if let Some(location) = track.locations.first_mut() {
            location.gLat = round_to(first[1], 7);
            location.gLng = round_to(first[0], 7);
        }
    }
    if let Some(location) = track.locations.first() {
        track.startLatitude = location.gLat;
        track.startLongitude = location.gLng;
    }
    track.totalDistance = dist;
    track.validDistance = dist;
    track
}

struct Sampler {
    points: Vec<[f64; 2]>,
    arcs: Vec<f64>,
    projection: Projection,
}

impl Sampler {
    fn new(points: &[LngLat], projection: Projection) -> Self {
        let xy: Vec<[f64; 2]> = points.iter().map(|point| projection.xy(*point)).collect();
        let mut arcs = vec![0.0f64];
        for pair in xy.windows(2) {
            let dx = pair[1][0] - pair[0][0];
            let dy = pair[1][1] - pair[0][1];
            arcs.push(arcs.last().copied().unwrap_or(0.0) + (dx * dx + dy * dy).sqrt());
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
            .binary_search_by(|value| value.partial_cmp(&target).unwrap_or(Ordering::Equal))
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
        self.projection.lnglat([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t])
    }
}

/// 判断服务器点位是否集中在操场尺度内。只作为“操场绕圈模式”的入口判断，
/// 不改变点位本身；具体阈值可后续由 campus 数据中的操场多边形替换。
pub fn is_playground(tasks: &[LngLat], radius_m: f64) -> bool {
    if tasks.len() < 3 {
        return false;
    }
    let lat = tasks.iter().map(|point| point[1]).sum::<f64>() / tasks.len() as f64;
    let lng = tasks.iter().map(|point| point[0]).sum::<f64>() / tasks.len() as f64;
    let cos_lat = lat.to_radians().cos();
    tasks.iter().all(|point| {
        let dx = (point[0] - lng) * 111_320.0 * cos_lat;
        let dy = (point[1] - lat) * 111_132.0;
        (dx * dx + dy * dy).sqrt() <= radius_m
    })
}

/// 优先用 QGIS 操场多边形判断；没有操场数据时才退回点位集中度。
///
/// 只有「绝大多数点真的落在操场里」才按操场绕圈。之前有几条很松的兜底
///（点位中心在操场附近 + 全部点落在 180m 内就判操场），服务端下发
///「多个操场点位 + 操场外点位」这种混合点位时也会判成操场，
/// 然后跑道拟合失败、回退成「穿过所有点的巨椭圆」，轨迹伸出操场 100 多米。
pub fn is_playground_in_campus(campus: &Campus, tasks: &[LngLat], fallback_radius_m: f64) -> bool {
    if tasks.len() < 2 {
        return false;
    }
    if !campus.playgrounds.is_empty() {
        if campus.tasks_in_one_playground(tasks) {
            return true;
        }
        // 允许边线/门口的点略微落在多边形外：≥80% 的点在操场内或距边界 ≤12m。
        let projection = geo::Projection::for_polygon(&campus.area);
        for playground in &campus.playgrounds {
            let near = tasks
                .iter()
                .filter(|point| {
                    geo::point_in_polygon(**point, &playground.polygon, &projection)
                        || geo::distance_to_polygon_boundary(**point, &playground.polygon, &projection)
                            <= 12.0
                })
                .count();
            if near * 10 >= tasks.len() * 8 {
                return true;
            }
        }
        return false;
    }
    // 有广场但没有操场时，广场只作为可穿行开阔区，不触发绕圈。
    if !campus.plazas.is_empty() {
        return false;
    }
    is_playground(tasks, fallback_radius_m)
}

/// 生成严格顺序、道路优先的候选路线。
pub fn generate(
    campus: &Campus,
    tasks: &[LngLat],
    rules: V3Rules,
) -> Result<Vec<V3Candidate>, String> {
    if tasks.len() < 2 {
        return Err("至少需要 2 个任务点".into());
    }
    let net = Network::build(campus, tasks, &rules)?;
    let base = base_state(&net, tasks, &rules)?;
    let mut rng = Rng::new(rules.seed);
    let mut beam = vec![base];
    let mut complete: Vec<State> = Vec::new();
    for _ in 0..rules.max_rounds {
        let mut next = Vec::new();
        for state in &beam {
            if state.distance_m >= rules.min_distance_m {
                complete.push(state.clone());
                continue;
            }
            next.push(state.clone());
            for _ in 0..5 {
                if let Some(candidate) = replace_state(&net, state, tasks, &rules, &mut rng) {
                    if candidate.distance_m <= rules.max_search_m {
                        next.push(candidate);
                    }
                }
            }
            for _ in 0..3 {
                if let Some(candidate) = cycle_state(&net, state, tasks, &rules, &mut rng) {
                    if candidate.distance_m <= rules.max_search_m {
                        next.push(candidate);
                    }
                }
            }
            if let Some(candidate) = extend_tail(&net, state, tasks, &rules, &mut rng) {
                if candidate.distance_m <= rules.max_search_m {
                    next.push(candidate);
                }
            }
        }
        let mut seen = HashSet::new();
        next.retain(|state| {
            let key: Vec<usize> = state
                .legs
                .iter()
                .flat_map(|leg| leg.iter().copied())
                .chain(state.tail.iter().copied())
                .collect();
            seen.insert(key)
        });
        next.sort_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.distance_m.partial_cmp(&b.distance_m).unwrap_or(Ordering::Equal))
        });
        let mut kept = Vec::new();
        let mut buckets: HashMap<i32, usize> = HashMap::new();
        for state in next {
            let bucket = (state.distance_m / 200.0) as i32;
            if buckets.get(&bucket).copied().unwrap_or(0) >= 4 {
                continue;
            }
            *buckets.entry(bucket).or_insert(0) += 1;
            kept.push(state);
            if kept.len() >= rules.beam_width {
                break;
            }
        }
        beam = kept;
        if beam.is_empty() {
            break;
        }
    }
    complete.extend(
        beam.into_iter()
            .filter(|state| state.distance_m >= rules.min_distance_m),
    );
    if complete.is_empty() {
        return Err("没有找到 ≥2000m 的道路优先路线；请补道路/连接入口或检查任务接入".into());
    }
    complete.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(Ordering::Equal)
    });
    let mut candidates = Vec::new();
    for state in complete.into_iter().take(24) {
        if let Ok(candidate) = candidate_from_state(&net, &state, tasks, campus, &rules) {
            candidates.push(candidate);
        }
        if candidates.len() >= 12 {
            break;
        }
    }
    if candidates.is_empty() {
        return Err("候选均未通过严格顺序/净空/长度检查".into());
    }
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for candidate in candidates {
        let key: Vec<(i64, i64)> = candidate
            .points
            .iter()
            .map(|point| {
                (
                    (point[0] * 1_000_000.0).round() as i64,
                    (point[1] * 1_000_000.0).round() as i64,
                )
            })
            .collect();
        if seen.insert(key) {
            unique.push(candidate);
        }
        if unique.len() >= 12 {
            break;
        }
    }
    let mut candidates = unique;
    if candidates.is_empty() {
        return Err("去重后没有不同路线".into());
    }
    candidates.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(Ordering::Equal)
    });
    Ok(candidates)
}
