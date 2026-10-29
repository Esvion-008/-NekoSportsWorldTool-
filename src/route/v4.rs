//! V4 无敌寻路：开阔区域网格 A* + 建筑净空 + 抖动 + P1→P5 严格顺序。
//! 与网页版算法一致：不贴建筑、直线带轻微抖动、默认全账号使用。

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::campus::{geo, Campus, LngLat};
use crate::track::geom::Rng;

use super::RouteCandidate;

const STEP_M: f64 = 2.0;
/// 逐级降级档位：（主净空，降级净空）。服务端会把「贴着楼」的轨迹判成异常
/// （实测反馈：距离建筑物太近 → 成绩无效），所以几何本身必须和建筑拉开距离。
/// 首选 10/8；校园实在塞不下再退 8/6、6/5。
const CLEARANCE_STEPS: [(f64, f64, f64); 4] = [
    (10.0, 8.0, 10.0),
    (8.0, 6.0, 6.0),
    (6.0, 5.0, 3.0),
    // 最后一档：校园里两栋楼之间常有 3–4m 的小道，5m 净空会判"不连通"。
    // 这一档能钻过去，但离楼只有 3m 左右，可能被服务端判贴楼——日志会写明。
    (4.0, 3.0, 2.0),
];

#[derive(Clone, Copy, Debug)]
struct Node {
    x: i32,
    y: i32,
    f: f64,
}
impl PartialEq for Node {
    fn eq(&self, other: &Self) -> bool { self.f == other.f }
}
impl Eq for Node {}
impl PartialOrd for Node {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> { Some(self.cmp(other)) }
}
impl Ord for Node {
    fn cmp(&self, other: &Self) -> Ordering { other.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal) }
}

struct Grid<'a> {
    campus: &'a Campus,
    proj: geo::Projection,
    x0: f64,
    y0: f64,
    nx: i32,
    ny: i32,
    clearance: f64,
    /// 离校区围墙的最小距离（米），随净空档位一起降级。
    wall_margin: f64,
    /// 每个格子被走过几次：补距时用重复惩罚把回程逼到平行路/别的路上，
    /// 而不是把原路倒着再走一遍（那样整条轨迹 100% 重合，一眼假）。
    visits: HashMap<(i32, i32), u32>,
}

impl<'a> Grid<'a> {
    fn new(campus: &'a Campus, clearance: f64, wall_margin: f64) -> Self {
        let proj = geo::Projection::for_polygon(&campus.area);
        let pts: Vec<[f64; 2]> = campus.area.outer.iter().map(|p| proj.xy(*p)).collect();
        let x0 = pts.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min) - STEP_M * 2.0;
        let x1 = pts.iter().map(|p| p[0]).fold(f64::NEG_INFINITY, f64::max) + STEP_M * 2.0;
        let y0 = pts.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min) - STEP_M * 2.0;
        let y1 = pts.iter().map(|p| p[1]).fold(f64::NEG_INFINITY, f64::max) + STEP_M * 2.0;
        Self {
            campus,
            proj,
            x0,
            y0,
            nx: ((x1 - x0) / STEP_M).ceil() as i32 + 1,
            ny: ((y1 - y0) / STEP_M).ceil() as i32 + 1,
            clearance,
            wall_margin,
            visits: HashMap::new(),
        }
    }

    /// 把路径经过的格子标记一次。
    fn mark_visits(&mut self, cells: &[[f64; 2]]) {
        for point in cells {
            let x = ((point[0] - self.x0) / STEP_M).round() as i32;
            let y = ((point[1] - self.y0) / STEP_M).round() as i32;
            *self.visits.entry((x, y)).or_insert(0) += 1;
        }
    }

    /// 挑一个「离已走路线够远、离当前位置不远不近」的空地当补距目标点。
    ///
    /// 这样补出来的每一段都往没走过的地方去，整条轨迹不会自己叠自己。
    fn pick_far_goal(
        &self,
        walked: &[[f64; 2]],
        current: [f64; 2],
        rng: &mut Rng,
    ) -> Option<LngLat> {
        let mut best: Option<([f64; 2], f64)> = None;
        for _ in 0..80 {
            if self.nx <= 1 || self.ny <= 1 {
                return None;
            }
            let x = rng.randint(0, (self.nx - 1) as i64) as i32;
            let y = rng.randint(0, (self.ny - 1) as i64) as i32;
            let point = self.xy(x, y);
            if !self.free(point, self.clearance) {
                continue;
            }
            let to_current = ((point[0] - current[0]).powi(2) + (point[1] - current[1]).powi(2)).sqrt();
            if !(70.0..=420.0).contains(&to_current) {
                continue;
            }
            let to_walked = walked
                .iter()
                .map(|w| ((w[0] - point[0]).powi(2) + (w[1] - point[1]).powi(2)).sqrt())
                .fold(f64::INFINITY, f64::min);
            if to_walked < 30.0 {
                continue;
            }
            // 越远离已走路线越好，但别偏到天边去。
            let score = to_walked.min(160.0) + 0.15 * to_current;
            if best.map(|(_, s)| score > s).unwrap_or(true) {
                best = Some((point, score));
            }
        }
        best.map(|(point, _)| self.proj.lnglat(point))
    }

    fn xy(&self, x: i32, y: i32) -> [f64; 2] {
        [self.x0 + x as f64 * STEP_M, self.y0 + y as f64 * STEP_M]
    }

    fn free(&self, xy: [f64; 2], clearance: f64) -> bool {
        if !xy[0].is_finite() || !xy[1].is_finite() {
            return false;
        }
        let p = self.proj.lnglat(xy);
        if !geo::point_in_polygon(p, &self.campus.area, &self.proj) {
            return false;
        }
        // 离校区围墙留出这一档的余量（默认 10m，塞不下会降到 6m/3m）。
        if geo::distance_to_polygon_boundary(p, &self.campus.area, &self.proj) < self.wall_margin {
            return false;
        }
        let in_corridor = self
            .campus
            .corridors
            .iter()
            .any(|corridor| geo::point_in_polygon(p, &corridor.polygon, &self.proj));
        for blocked in &self.campus.blocked {
            if geo::point_in_polygon(p, &blocked.polygon, &self.proj) {
                return false;
            }
        }
        // 围墙门洞/窄通道用 corridor 显式放行；建筑本体仍然禁行。
        // 路网同样放行：真人就是沿这些路跑的，原来 roads 只画图、不参与判定，
        // 结果「明明有路」却走不过去。
        if in_corridor {
            return true;
        }
        let on_road = self
            .campus
            .roads
            .iter()
            .any(|road| geo::distance_to_polyline(p, &road.path, &self.proj) <= 4.0);
        if on_road {
            return true;
        }
        for blocked in &self.campus.blocked {
            // 围墙/护栏只要 1m 余量（它们贴着操场边画，按建筑净空算整片操场都不可走）
            let required = if crate::campus::is_wall_name(&blocked.name) {
                clearance.min(1.0)
            } else {
                clearance
            };
            if geo::distance_to_polygon_boundary(p, &blocked.polygon, &self.proj) < required {
                return false;
            }
        }
        true
    }

    fn nearest_free(&self, p: LngLat, clearance: f64) -> Option<(i32, i32)> {
        let xy = self.proj.xy(p);
        let bx = ((xy[0] - self.x0) / STEP_M).round() as i32;
        let by = ((xy[1] - self.y0) / STEP_M).round() as i32;
        for radius in 0..30 {
            for dx in -radius..=radius {
                for dy in -radius..=radius {
                    let x = bx + dx;
                    let y = by + dy;
                    if x < 0 || y < 0 || x >= self.nx || y >= self.ny {
                        continue;
                    }
                    if self.free(self.xy(x, y), clearance) {
                        return Some((x, y));
                    }
                }
            }
        }
        None
    }

    /// 平面坐标版的 A*（混合模式里路径点已经是 xy，避免来回投影）。
    fn astar_xy(&mut self, a: [f64; 2], b: [f64; 2], clearance: f64, penalty: f64) -> Option<Vec<[f64; 2]>> {
        let a_lnglat = self.proj.lnglat(a);
        let b_lnglat = self.proj.lnglat(b);
        self.astar(a_lnglat, b_lnglat, clearance, penalty)
    }

    fn astar(
        &self,
        a: LngLat,
        b: LngLat,
        clearance: f64,
        revisit_penalty: f64,
    ) -> Option<Vec<[f64; 2]>> {
        let start = self.nearest_free(a, clearance)?;
        let goal = self.nearest_free(b, clearance)?;
        let start_key = (start.0, start.1);
        let goal_key = (goal.0, goal.1);
        let mut open = BinaryHeap::new();
        let mut g = HashMap::new();
        let mut came = HashMap::new();
        g.insert(start_key, 0.0f64);
        open.push(Node {
            x: start.0,
            y: start.1,
            f: self.heuristic(start, goal),
        });
        while let Some(cur) = open.pop() {
            if (cur.x, cur.y) == goal_key {
                break;
            }
            let cg = g.get(&(cur.x, cur.y)).copied().unwrap_or(f64::INFINITY);
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let nx = cur.x + dx;
                let ny = cur.y + dy;
                if nx < 0 || ny < 0 || nx >= self.nx || ny >= self.ny {
                    continue;
                }
                let p = self.xy(nx, ny);
                if !self.free(p, clearance) {
                    continue;
                }
                let step = if dx == 0 || dy == 0 { STEP_M } else { STEP_M * 1.414_213_562_373_095 };
                // 走过的格子加价：补距时回程会自己绕到平行路/别的路上，不再原路重叠。
                let visits = self.visits.get(&(nx, ny)).copied().unwrap_or(0) as f64;
                let ng = cg + step * (1.0 + revisit_penalty * visits);
                let key = (nx, ny);
                if ng + 1e-9 < g.get(&key).copied().unwrap_or(f64::INFINITY) {
                    g.insert(key, ng);
                    came.insert(key, (cur.x, cur.y));
                    open.push(Node { x: nx, y: ny, f: ng + self.heuristic((nx, ny), goal) });
                }
            }
        }
        if !came.contains_key(&goal_key) {
            return None;
        }
        let mut cells = Vec::new();
        let mut key = goal_key;
        loop {
            cells.push(self.xy(key.0, key.1));
            if key == start_key {
                break;
            }
            key = *came.get(&key)?;
        }
        cells.reverse();
        Some(cells)
    }

    fn heuristic(&self, a: (i32, i32), b: (i32, i32)) -> f64 {
        let dx = (a.0 - b.0) as f64 * STEP_M;
        let dy = (a.1 - b.1) as f64 * STEP_M;
        (dx * dx + dy * dy).sqrt()
    }
}

fn smooth(points: &[[f64; 2]], grid: &Grid<'_>, passes: usize, clearance: f64) -> Vec<[f64; 2]> {
    let mut out = points.to_vec();
    for _ in 0..passes {
        if out.len() < 3 {
            break;
        }
        let current = out.clone();
        for i in 1..current.len() - 1 {
            let a = current[i - 1];
            let b = current[i];
            let c = current[i + 1];
            let q = [(a[0] + 2.0 * b[0] + c[0]) / 4.0, (a[1] + 2.0 * b[1] + c[1]) / 4.0];
            if grid.free(q, clearance) {
                out[i] = q;
            }
        }
    }
    out
}

/// 沿法向的低频漂移（一阶自相关），替代原来的固定周期正弦。
///
/// 固定周期的结果是整条线等幅波浪，放大一眼就看出是生成的；真人横向偏移是
/// 低频随机游走：几段贴这边、几段贴那边，幅度 0.5–1.6m。
fn jitter(points: &[[f64; 2]], grid: &Grid<'_>, seed: u64, clearance: f64) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut rng = Rng::new(seed ^ 0x517c_c1b7_2722_0a95);
    let mut out = Vec::with_capacity(points.len());
    let mut lateral = 0.0f64;
    for i in 0..points.len() {
        if i == 0 || i == points.len() - 1 {
            out.push(points[i]);
            continue;
        }
        let a = points[i.saturating_sub(2)];
        let b = points[(i + 2).min(points.len() - 1)];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt().max(1e-6);
        let nx = -dy / len;
        let ny = dx / len;
        lateral = 0.965 * lateral + rng.gauss(0.0, 0.62);
        lateral = lateral.clamp(-1.7, 1.7);
        let q = [points[i][0] + nx * lateral, points[i][1] + ny * lateral];
        out.push(if grid.free(q, clearance) { q } else { points[i] });
    }
    out
}

/// 转角圆化：转角超过 `min_turn_deg` 的地方用二次贝塞尔倒一个 `radius` 的圆角。
///
/// A* 出来的 4m 折线是硬折角（尤其掉头是 180° 尖角），真人转弯半径 2–4m。
/// 先圆角再叠加漂移，轨迹看起来才像脚跑出来的。
fn round_corners(
    points: &[[f64; 2]],
    radius: f64,
    min_turn_deg: f64,
    grid: &Grid<'_>,
    clearance: f64,
) -> Vec<[f64; 2]> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(points.len() + 32);
    out.push(points[0]);
    for i in 1..points.len() - 1 {
        let b = points[i];
        let v1 = [points[i - 1][0] - b[0], points[i - 1][1] - b[1]];
        let v2 = [points[i + 1][0] - b[0], points[i + 1][1] - b[1]];
        let l1 = (v1[0] * v1[0] + v1[1] * v1[1]).sqrt();
        let l2 = (v2[0] * v2[0] + v2[1] * v2[1]).sqrt();
        if l1 < 1e-6 || l2 < 1e-6 {
            out.push(b);
            continue;
        }
        let cos = ((v1[0] * v2[0] + v1[1] * v2[1]) / (l1 * l2)).clamp(-1.0, 1.0);
        let turn_off = 180.0 - cos.acos().to_degrees(); // 0 = 直行，90 = 直角，180 = 掉头
        if turn_off < min_turn_deg {
            out.push(b);
            continue;
        }
        let half = (turn_off * 0.5).to_radians();
        let tangent = (radius * half.tan()).min(l1 * 0.5).min(l2 * 0.5);
        if tangent < 0.4 {
            out.push(b);
            continue;
        }
        let p1 = [b[0] + v1[0] / l1 * tangent, b[1] + v1[1] / l1 * tangent];
        let p2 = [b[0] + v2[0] / l2 * tangent, b[1] + v2[1] / l2 * tangent];
        // 圆弧自身也必须满足净空：贴着楼的角不做倒角，宁可保留折角。
        let mut arc: Vec<[f64; 2]> = Vec::with_capacity(6);
        arc.push(p1);
        for step in 1..4 {
            let s = step as f64 / 4.0;
            let u = 1.0 - s;
            arc.push([
                u * u * p1[0] + 2.0 * u * s * b[0] + s * s * p2[0],
                u * u * p1[1] + 2.0 * u * s * b[1] + s * s * p2[1],
            ]);
        }
        arc.push(p2);
        if !arc.iter().all(|point| grid.free(*point, clearance)) {
            out.push(b);
            continue;
        }
        out.extend(arc);
    }
    out.push(points[points.len() - 1]);
    out
}

/// 操场模式：生成贴着操场的闭合椭圆环线，并且**必须经过每一个打卡点**。
///
/// 1. 5 个打卡点时，先解「过 5 点的圆锥曲线」；如果它确实是椭圆、尺寸正常、
///    且整圈都在操场内、离建筑/围墙有净空，就直接用它——这是最漂亮的真椭圆。
/// 2. 否则退回「径向椭圆变形」：以打卡点 PCA 椭圆为底，按角度给半径做周期
///    Catmull-Rom 插值，使曲线精确穿过每个点；越界采样点再向中心拉回。
/// 3. 结果按目标距离绕圈重复，并补齐最后一圈到目标米数（不再多跑一整圈）。
pub fn playground_ellipse(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
) -> Result<RouteCandidate, String> {
    if tasks.len() < 2 {
        return Err("V4 操场模式至少需要 2 个打卡点".into());
    }
    let projection = geo::Projection::for_polygon(&campus.area);
    let (playground, inside) = campus
        .playgrounds
        .iter()
        .map(|playground| {
            let count = tasks
                .iter()
                .filter(|point| {
                    geo::point_in_polygon(**point, &playground.polygon, &projection)
                })
                .count();
            (playground, count)
        })
        .max_by_key(|(_, count)| *count)
        .ok_or("V4：校园数据没有 playground 区域")?;
    if inside == 0 {
        return Err("V4：打卡点不在操场内".into());
    }
    let xy: Vec<[f64; 2]> = tasks.iter().map(|point| projection.xy(*point)).collect();

    // 首选：过 5 点的真椭圆。
    let mut loop_xy: Option<Vec<[f64; 2]>> = None;
    if xy.len() == 5 {
        if let Some(ellipse) = ellipse_through5(&xy) {
            if ellipse_sane(&ellipse, playground, &projection) {
                let path = sample_ellipse_loop(&ellipse, &xy);
                if path
                    .iter()
                    .all(|point| playground_point_ok(campus, playground, &projection, *point))
                {
                    loop_xy = Some(path);
                }
            }
        }
    }
    // 兜底：径向椭圆变形，保证经过全部打卡点。
    if loop_xy.is_none() {
        loop_xy = radial_ellipse_loop(campus, playground, &projection, &xy);
    }
    let loop_xy = loop_xy.ok_or("V4：操场内放不下闭合椭圆")?;
    if loop_xy.len() < 3 {
        return Err("V4：操场椭圆采样点不足".into());
    }

    let mut loop_points: Vec<LngLat> = loop_xy.iter().map(|point| projection.lnglat(*point)).collect();
    // 起点严格等于 P1（服务端要求从第一个打卡点起跑）。
    if let Some(first) = loop_points.first_mut() {
        *first = tasks[0];
    }
    let (points, total_m) = repeat_loop_to_target(&loop_points, target_m, &projection);
    if points.len() < 3 || total_m <= 0.0 {
        return Err("V4 操场椭圆路线为空".into());
    }
    Ok(RouteCandidate {
        points,
        checkpoints: tasks.to_vec(),
        tour_m: total_m,
        total_m,
        end_poi: None,
        corners: 0,
        straight_ratio: 0.0,
        curve_ratio: 0.0,
        score: 0.0,
        order: (0..tasks.len()).collect(),
        warnings: Vec::new(),
    })
}

/// 本地米制平面上的椭圆：center + dir·(half.0·cosθ) + perp(dir)·(half.1·sinθ)。
#[derive(Clone, Copy, Debug)]
struct EllipseFit {
    center: [f64; 2],
    dir: [f64; 2],
    half: [f64; 2],
}

impl EllipseFit {
    fn point(&self, theta: f64) -> [f64; 2] {
        let (sin, cos) = theta.sin_cos();
        let perp = [-self.dir[1], self.dir[0]];
        [
            self.center[0] + self.dir[0] * self.half[0] * cos + perp[0] * self.half[1] * sin,
            self.center[1] + self.dir[1] * self.half[0] * cos + perp[1] * self.half[1] * sin,
        ]
    }

    fn theta_of(&self, point: [f64; 2]) -> f64 {
        let perp = [-self.dir[1], self.dir[0]];
        let dx = point[0] - self.center[0];
        let dy = point[1] - self.center[1];
        let u = (dx * self.dir[0] + dy * self.dir[1]) / self.half[0].max(1e-9);
        let v = (dx * perp[0] + dy * perp[1]) / self.half[1].max(1e-9);
        v.atan2(u)
    }
}

/// 解过 5 点的圆锥曲线；只有判定为椭圆时才返回。
fn ellipse_through5(points: &[[f64; 2]]) -> Option<EllipseFit> {
    if points.len() != 5 {
        return None;
    }
    let count = points.len() as f64;
    let cx = points.iter().map(|point| point[0]).sum::<f64>() / count;
    let cy = points.iter().map(|point| point[1]).sum::<f64>() / count;
    // 归一化后求 ax²+bxy+cy²+dx+ey+f=0（f=-1），避免 1e6 量级系数把解压坏。
    let scale = points.iter().fold(0.0_f64, |acc, point| {
        acc.max((point[0] - cx).abs()).max((point[1] - cy).abs())
    });
    if !(scale > 1e-9) {
        return None;
    }
    let mut matrix = [[0.0_f64; 6]; 5];
    for (index, point) in points.iter().enumerate() {
        let x = (point[0] - cx) / scale;
        let y = (point[1] - cy) / scale;
        matrix[index] = [x * x, x * y, y * y, x, y, 1.0];
    }
    let solution = solve_linear5(matrix)?;

    // 系数整体取反不改变曲线；统一成「二次型正定、f0<0」便于求半轴。
    for flip in [1.0_f64, -1.0] {
        let a = solution[0] * flip;
        let b = solution[1] * flip;
        let c = solution[2] * flip;
        let d = solution[3] * flip;
        let e = solution[4] * flip;
        let f = -1.0 * flip;
        let det = 4.0 * a * c - b * b;
        if det <= 1e-9 {
            return None;
        }
        let x0 = (b * e - 2.0 * c * d) / det;
        let y0 = (b * d - 2.0 * a * e) / det;
        let f0 = a * x0 * x0 + b * x0 * y0 + c * y0 * y0 + d * x0 + e * y0 + f;
        let (l1, l2, dir) = eig_sym2(a, b * 0.5, c);
        if l1 <= 0.0 || l2 <= 0.0 || f0 >= 0.0 {
            continue;
        }
        let half0 = (-f0 / l1).sqrt() * scale;
        let half1 = (-f0 / l2).sqrt() * scale;
        if !half0.is_finite() || !half1.is_finite() || half0 <= 0.0 || half1 <= 0.0 {
            return None;
        }
        return Some(EllipseFit {
            center: [cx + scale * x0, cy + scale * y0],
            dir,
            half: [half0, half1],
        });
    }
    None
}

fn ellipse_sane(
    ellipse: &EllipseFit,
    playground: &crate::campus::PlaygroundArea,
    projection: &geo::Projection,
) -> bool {
    let major = ellipse.half[0].max(ellipse.half[1]);
    let minor = ellipse.half[0].min(ellipse.half[1]);
    if !(major.is_finite() && minor.is_finite()) {
        return false;
    }
    if major < 12.0 || minor < 5.0 || major > 600.0 || minor > 600.0 {
        return false;
    }
    if major / minor > 8.0 {
        return false;
    }
    let center = projection.lnglat(ellipse.center);
    geo::point_in_polygon(center, &playground.polygon, projection)
        || geo::distance_to_polygon_boundary(center, &playground.polygon, projection) <= 80.0
}

/// 真椭圆采样：从 P1 的角度起跑，并把每个打卡点的角度精确插入点列。
fn sample_ellipse_loop(ellipse: &EllipseFit, tasks_xy: &[[f64; 2]]) -> Vec<[f64; 2]> {
    const SAMPLES: usize = 480;
    let start = ellipse.theta_of(tasks_xy[0]);
    let mut params: Vec<f64> = (0..SAMPLES)
        .map(|index| start + std::f64::consts::TAU * index as f64 / SAMPLES as f64)
        .collect();
    for point in tasks_xy {
        let mut theta = ellipse.theta_of(*point);
        while theta < start {
            theta += std::f64::consts::TAU;
        }
        while theta >= start + std::f64::consts::TAU {
            theta -= std::f64::consts::TAU;
        }
        params.push(theta);
    }
    params.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    params.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let mut out: Vec<[f64; 2]> = params.iter().map(|theta| ellipse.point(*theta)).collect();
    if let Some(first) = out.first_mut() {
        *first = tasks_xy[0];
    }
    out
}

/// 径向椭圆变形：以打卡点 PCA 椭圆为底，按角度周期插值半径，穿过每个打卡点。
fn radial_ellipse_loop(
    campus: &Campus,
    playground: &crate::campus::PlaygroundArea,
    projection: &geo::Projection,
    xy: &[[f64; 2]],
) -> Option<Vec<[f64; 2]>> {
    let n = xy.len();
    if n < 2 {
        return None;
    }
    let count = n as f64;
    let cx = xy.iter().map(|point| point[0]).sum::<f64>() / count;
    let cy = xy.iter().map(|point| point[1]).sum::<f64>() / count;
    let (mut sxx, mut sxy, mut syy) = (0.0_f64, 0.0_f64, 0.0_f64);
    for point in xy {
        let dx = point[0] - cx;
        let dy = point[1] - cy;
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    let (l1, l2, dir) = eig_sym2(sxx / count, sxy / count, syy / count);
    let mut axis_a = l1.max(0.0).sqrt() * 1.35 + 6.0;
    let mut axis_b = l2.max(0.0).sqrt() * 1.35 + 6.0;
    if axis_a < axis_b {
        std::mem::swap(&mut axis_a, &mut axis_b);
    }
    axis_a = axis_a.max(8.0);
    axis_b = axis_b.max(4.0);

    // 按方位角排序成简单闭合圈，再旋转到 P1 起点（与轨迹生成器一致）。
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        let angle_a = (xy[a][1] - cy).atan2(xy[a][0] - cx);
        let angle_b = (xy[b][1] - cy).atan2(xy[b][0] - cx);
        angle_a.partial_cmp(&angle_b).unwrap_or(Ordering::Equal)
    });
    let start_index = order.iter().position(|&index| index == 0).unwrap_or(0);
    order.rotate_left(start_index);

    let mut thetas = Vec::with_capacity(n);
    let mut factors = Vec::with_capacity(n);
    for (rank, &index) in order.iter().enumerate() {
        let mut theta = (xy[index][1] - cy).atan2(xy[index][0] - cx);
        if rank > 0 {
            while theta <= thetas[rank - 1] {
                theta += std::f64::consts::TAU;
            }
        }
        let radius = (xy[index][0] - cx).hypot(xy[index][1] - cy).max(1e-6);
        let base = ellipse_radius(theta, dir, axis_a, axis_b).max(1e-6);
        thetas.push(theta);
        factors.push(radius / base);
    }

    // 每段固定 96 个采样，约 480 个点一圈。
    let mut out: Vec<[f64; 2]> = Vec::new();
    let mut anchors: Vec<usize> = Vec::with_capacity(n);
    for i in 0..n {
        let theta_start = thetas[i];
        let theta_end = if i + 1 < n {
            thetas[i + 1]
        } else {
            thetas[0] + std::f64::consts::TAU
        };
        anchors.push(out.len());
        for step in 0..96 {
            let theta = theta_start + (theta_end - theta_start) * step as f64 / 96.0;
            let factor = interp_radius_factor(&factors, &thetas, theta);
            let radius = ellipse_radius(theta, dir, axis_a, axis_b) * factor;
            out.push([cx + radius * theta.cos(), cy + radius * theta.sin()]);
        }
    }
    // 打卡点精确落位。
    for (rank, &index) in order.iter().enumerate() {
        out[anchors[rank]] = xy[index];
    }
    let center = [cx, cy];
    // 越界/贴墙的中间采样点向中心拉回；打卡点本身保持不动。
    for index in 0..out.len() {
        if anchors.contains(&index) {
            continue;
        }
        let point = out[index];
        if playground_point_ok(campus, playground, projection, point) {
            continue;
        }
        if !playground_point_ok(campus, playground, projection, center) {
            continue;
        }
        let (mut low, mut high) = (0.0_f64, 1.0_f64);
        for _ in 0..22 {
            let mid = (low + high) * 0.5;
            let candidate = [
                point[0] + (center[0] - point[0]) * mid,
                point[1] + (center[1] - point[1]) * mid,
            ];
            if playground_point_ok(campus, playground, projection, candidate) {
                high = mid;
            } else {
                low = mid;
            }
        }
        out[index] = [
            point[0] + (center[0] - point[0]) * high,
            point[1] + (center[1] - point[1]) * high,
        ];
    }
    Some(out)
}

fn playground_point_ok(
    campus: &Campus,
    playground: &crate::campus::PlaygroundArea,
    projection: &geo::Projection,
    point: [f64; 2],
) -> bool {
    if !point[0].is_finite() || !point[1].is_finite() {
        return false;
    }
    let lnglat = projection.lnglat(point);
    if !campus.route_point_ok(lnglat, 0.8) {
        return false;
    }
    // 允许贴着操场边线跑（打卡点常压在边线上），但不能跑出操场太远。
    geo::point_in_polygon(lnglat, &playground.polygon, projection)
        || geo::distance_to_polygon_boundary(lnglat, &playground.polygon, projection) <= 5.0
}

/// 椭圆极径：θ 方向上的半径（a 沿 dir，b 沿垂直方向）。
fn ellipse_radius(theta: f64, dir: [f64; 2], a: f64, b: f64) -> f64 {
    let (sin, cos) = theta.sin_cos();
    let u = cos * dir[0] + sin * dir[1];
    let v = -cos * dir[1] + sin * dir[0];
    (a * b) / ((b * u).powi(2) + (a * v).powi(2)).sqrt().max(1e-9)
}

/// 周期 Catmull-Rom：角度 → 半径系数。
fn interp_radius_factor(factors: &[f64], thetas: &[f64], theta: f64) -> f64 {
    let n = factors.len();
    let mut segment = n - 1;
    for index in 0..n {
        let end = if index + 1 < n {
            thetas[index + 1]
        } else {
            thetas[0] + std::f64::consts::TAU
        };
        if theta >= thetas[index] && theta <= end {
            segment = index;
            break;
        }
    }
    let start = thetas[segment];
    let end = if segment + 1 < n {
        thetas[segment + 1]
    } else {
        thetas[0] + std::f64::consts::TAU
    };
    let u = ((theta - start) / (end - start).max(1e-9)).clamp(0.0, 1.0);
    let p0 = factors[(segment + n - 1) % n];
    let p1 = factors[segment];
    let p2 = factors[(segment + 1) % n];
    let p3 = factors[(segment + 2) % n];
    let (u2, u3) = (u * u, u * u * u);
    let m1 = (p2 - p0) * 0.5;
    let m2 = (p3 - p1) * 0.5;
    (2.0 * u3 - 3.0 * u2 + 1.0) * p1
        + (u3 - 2.0 * u2 + u) * m1
        + (-2.0 * u3 + 3.0 * u2) * p2
        + (u3 - u2) * m2
}

/// 2×2 对称阵特征分解：(较大特征值, 较小特征值, 较大特征值方向)。
fn eig_sym2(m11: f64, m12: f64, m22: f64) -> (f64, f64, [f64; 2]) {
    let trace = m11 + m22;
    let det = m11 * m22 - m12 * m12;
    let disc = (trace * trace * 0.25 - det).max(0.0).sqrt();
    let l1 = trace * 0.5 + disc;
    let l2 = trace * 0.5 - disc;
    let dir = if m12.abs() > 1e-12 {
        let vx = m12;
        let vy = l1 - m11;
        let len = vx.hypot(vy);
        if len > 1e-12 {
            [vx / len, vy / len]
        } else {
            [1.0, 0.0]
        }
    } else if m11 >= m22 {
        [1.0, 0.0]
    } else {
        [0.0, 1.0]
    };
    (l1, l2, dir)
}

/// 5×5 高斯消元（列主元），无解返回 None。
fn solve_linear5(mut matrix: [[f64; 6]; 5]) -> Option<[f64; 5]> {
    for col in 0..5 {
        let pivot = (col..5).max_by(|&a, &b| {
            matrix[a][col]
                .abs()
                .partial_cmp(&matrix[b][col].abs())
                .unwrap_or(Ordering::Equal)
        })?;
        if matrix[pivot][col].abs() < 1e-12 {
            return None;
        }
        matrix.swap(col, pivot);
        let inv = 1.0 / matrix[col][col];
        for value in matrix[col].iter_mut().skip(col) {
            *value *= inv;
        }
        for row in 0..5 {
            if row == col {
                continue;
            }
            let factor = matrix[row][col];
            if factor == 0.0 {
                continue;
            }
            for k in col..6 {
                matrix[row][k] -= factor * matrix[col][k];
            }
        }
    }
    let mut out = [0.0_f64; 5];
    for (index, value) in out.iter_mut().enumerate() {
        *value = matrix[index][5];
    }
    Some(out)
}

/// 按目标距离绕圈：整圈重复，最后一圈按剩余米数截断。
fn repeat_loop_to_target(
    loop_points: &[LngLat],
    target_m: f64,
    projection: &geo::Projection,
) -> (Vec<LngLat>, f64) {
    let mut points = loop_points.to_vec();
    let mut total = polyline_len(loop_points, projection);
    if loop_points.len() < 2 {
        return (points, total);
    }
    let closing = projection.distance_m(loop_points[loop_points.len() - 1], loop_points[0]);
    let lap = total + closing;
    if lap < 30.0 || !target_m.is_finite() || target_m <= 0.0 {
        return (points, total);
    }
    let mut guard = 0;
    while total + lap <= target_m * 1.0005 && guard < 400 {
        points.extend_from_slice(loop_points);
        total += lap;
        guard += 1;
    }
    let remain = target_m - total;
    if remain > 1.0 {
        let mut left = remain;
        let mut last = *points.last().unwrap_or(&loop_points[0]);
        let mut sequence: Vec<LngLat> = Vec::with_capacity(loop_points.len() + 1);
        sequence.push(loop_points[0]);
        sequence.extend(loop_points.iter().skip(1).copied());
        for point in sequence {
            let distance = projection.distance_m(last, point);
            if distance >= left && distance > 1e-9 {
                let t = left / distance;
                points.push([
                    last[0] + (point[0] - last[0]) * t,
                    last[1] + (point[1] - last[1]) * t,
                ]);
                total += left;
                break;
            }
            points.push(point);
            total += distance;
            left -= distance;
            last = point;
        }
    }
    (points, total)
}

/// 把折线整体沿法向平移 `offset` 米（回程用它和去程错开，不重叠）。
fn offset_polyline(points: &[[f64; 2]], offset: f64) -> Vec<[f64; 2]> {
    if points.len() < 2 {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(points.len());
    for i in 0..points.len() {
        let a = points[i.saturating_sub(1)];
        let b = points[(i + 1).min(points.len() - 1)];
        let dx = b[0] - a[0];
        let dy = b[1] - a[1];
        let len = (dx * dx + dy * dy).sqrt().max(1e-6);
        out.push([points[i][0] - dy / len * offset, points[i][1] + dx / len * offset]);
    }
    out
}

/// 统计转角超过 40° 的次数（协议里的「拐角」）。
fn count_corners(points: &[[f64; 2]]) -> usize {
    let mut count = 0usize;
    for i in 1..points.len().saturating_sub(1) {
        let v1 = [points[i - 1][0] - points[i][0], points[i - 1][1] - points[i][1]];
        let v2 = [points[i + 1][0] - points[i][0], points[i + 1][1] - points[i][1]];
        let l1 = (v1[0] * v1[0] + v1[1] * v1[1]).sqrt();
        let l2 = (v2[0] * v2[0] + v2[1] * v2[1]).sqrt();
        if l1 < 1e-6 || l2 < 1e-6 {
            continue;
        }
        let cos = ((v1[0] * v2[0] + v1[1] * v2[1]) / (l1 * l2)).clamp(-1.0, 1.0);
        if 180.0 - cos.acos().to_degrees() > 40.0 {
            count += 1;
        }
    }
    count
}

/// 校园路线。净空逐级降级：先按 10m 排，塞不下退 8/6、最后 6/5。
pub fn generate(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
) -> Result<RouteCandidate, String> {
    let mut last_error = String::new();
    let last_index = CLEARANCE_STEPS.len() - 1;
    for (index, (clearance, hard, wall)) in CLEARANCE_STEPS.iter().enumerate() {
        // 「直连穿楼」只能用在最后一档：否则第一档就会用它兜底成功，
        // 后面更宽松的净空档位根本没机会跑（实测就是这个坑）。
        let allow_direct = index == last_index;
        match generate_with(campus, tasks, target_m, seed, *clearance, *hard, *wall, allow_direct) {
            Ok(candidate) => return Ok(candidate),
            Err(error) => last_error = error,
        }
    }
    Err(if last_error.is_empty() {
        "V4：打卡点之间找不到避开建筑的路径（净空约束太紧）".into()
    } else {
        format!("{last_error}（已从 10m 净空一路降到 5m）")
    })
}

/// 混合模式：一部分打卡点在操场内、一部分在操场外。
///
/// 按打卡顺序切段：操场内的连续段沿跑道弧长走，操场外的段用 A* 绕建筑，
/// **任何一段找不到不穿楼的路径就直接报错**（不直连、不穿楼）。
pub fn generate_mixed(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
) -> Result<RouteCandidate, String> {
    // 净空逐级降级：8m 排不下就退 6/5/4/3（校园里两栋楼之间常有 4m 小道）。
    // 全部档位都失败才报错 —— 绝不直连穿楼。
    let mut last_error = String::new();
    // 最低只降到 4m：3m 时轨迹中心线就贴着楼，GPS 噪声一抖就进建筑（实测 min=0.00）。
    for clearance in [8.0f64, 6.0, 5.0, 4.0] {
        match generate_mixed_with(campus, tasks, target_m, seed, clearance) {
            Ok(candidate) => return Ok(candidate),
            Err(error) => last_error = error,
        }
    }
    Err(format!("{last_error}（净空已从 8m 一路降到 3m）"))
}

fn generate_mixed_with(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
    clearance: f64,
) -> Result<RouteCandidate, String> {
    if tasks.len() < 2 {
        return Err("V4 至少需要 2 个打卡点".into());
    }
    let projection = geo::Projection::for_polygon(&campus.area);
    // ① 每个点归类到「哪个操场」或 None
    let groups: Vec<Option<usize>> = tasks
        .iter()
        .map(|task| {
            campus.playgrounds.iter().position(|playground| {
                geo::point_in_polygon(*task, &playground.polygon, &projection)
                    || geo::distance_to_polygon_boundary(*task, &playground.polygon, &projection) <= 12.0
            })
        })
        .collect();
    if groups.iter().all(|g| g.is_some()) {
        return Err("V4 混合模式：所有点都在操场内，应该走操场模式".into());
    }
    if groups.iter().all(|g| g.is_none()) {
        return Err("V4 混合模式：没有点在操场内，应该走校园模式".into());
    }

    // ② 逐段生成
    let mut grid = Grid::new(campus, clearance, clearance);
    let mut path: Vec<[f64; 2]> = Vec::new();
    let mut segments_desc: Vec<String> = Vec::new();
    let mut index = 0usize;
    while index < tasks.len() {
        let mut end = index;
        while end + 1 < tasks.len() && groups[end + 1] == groups[index] {
            end += 1;
        }
        if let Some(playground_index) = groups[index] {
            // —— 操场内：沿跑道弧长 ——
            let playground = &campus.playgrounds[playground_index];
            let stadium = super::playground::fit_stadium_shrunk(campus, playground, &projection, 8.0)
                .map(|(stadium, _shrinks)| stadium)
                .or_else(|| super::playground::fit_stadium(playground, &projection))
                .ok_or_else(|| format!("P{} 所在操场拟合不出跑道", index + 1))?;
            let xy: Vec<[f64; 2]> = tasks[index..=end].iter().map(|p| projection.xy(*p)).collect();
            let mut seg = super::playground::arc_segment(&stadium, &xy);
            if seg.is_empty() {
                return Err(format!("P{} → P{} 操场内段为空", index + 1, end + 1));
            }
            seg[0] = xy[0];
            if let Some(last) = seg.last_mut() {
                *last = xy[xy.len() - 1];
            }
            // 弧长点贴楼（操场弯道常贴着看台/围墙）时，沿法向往操场中心挪到合法位置
            for point in seg.iter_mut() {
                if grid.free(*point, clearance) {
                    continue;
                }
                let toward = [
                    stadium.center[0] - point[0],
                    stadium.center[1] - point[1],
                ];
                let length = (toward[0] * toward[0] + toward[1] * toward[1]).sqrt().max(1e-6);
                for step in 1..=12 {
                    let t = step as f64 / 12.0;
                    let candidate = [
                        point[0] + toward[0] / length * t * 8.0,
                        point[1] + toward[1] / length * t * 8.0,
                    ];
                    if grid.free(candidate, clearance) {
                        *point = candidate;
                        break;
                    }
                }
            }
            segments_desc.push(format!("P{}→P{} 操场弧长 {:.0}m", index + 1, end + 1, {
                let mut sum = 0.0;
                for pair in seg.windows(2) {
                    sum += ((pair[1][0] - pair[0][0]).powi(2) + (pair[1][1] - pair[0][1]).powi(2)).sqrt();
                }
                sum
            }));
            if path.is_empty() {
                path.extend(seg);
            } else {
                // 从上一段末尾接进来：先 A* 到操场内第一个点，不穿楼
                let from = *path.last().unwrap();
                let to = xy[0];
                if (from[0] - to[0]).hypot(from[1] - to[1]) > STEP_M {
                    let leg = grid
                        .astar_xy(from, to, clearance, 0.0)
                        .ok_or_else(|| format!("P{} 进操场这一段没有不穿楼的路径", index + 1))?;
                    path.extend(leg.into_iter().skip(1));
                }
                path.extend(seg.into_iter().skip(1));
            }
        } else {
            // —— 操场外：A* 逐点 ——
            for point_index in index..=end {
                let target = projection.xy(tasks[point_index]);
                if path.is_empty() {
                    path.push(target);
                    continue;
                }
                let from = *path.last().unwrap();
                if (from[0] - target[0]).hypot(from[1] - target[1]) < STEP_M * 0.5 {
                    continue;
                }
                let leg = grid
                    .astar_xy(from, target, clearance, 0.0)
                    .ok_or_else(|| {
                        format!(
                            "P{} → P{} 之间找不到不穿楼的路径（净空 {:.0}m）——需要在「校园地图」补走廊，或把这两个点之间的小道标成通道",
                            point_index,
                            point_index + 1,
                            clearance
                        )
                    })?;
                path.extend(leg.into_iter().skip(1));
                segments_desc.push(format!("P{}→P{} A* 绕建筑", point_index, point_index + 1));
            }
        }
        index = end + 1;
    }
    if path.len() < 3 {
        return Err("V4 混合模式：路径点数不足".into());
    }

    // ③ 平滑 + 圆角 + 抖动（都不许穿楼）
    path = round_corners(&smooth(&path, &grid, 2, clearance), 4.5, 38.0, &grid, clearance);
    path = jitter(&path, &grid, seed, clearance.max(3.0));

    let mut total_m = 0.0f64;
    for pair in path.windows(2) {
        total_m += ((pair[1][0] - pair[0][0]).powi(2) + (pair[1][1] - pair[0][1]).powi(2)).sqrt();
    }
    // ③ 先闭合：从最后一个打卡点绕回第一个（A*，绝不拉直线穿楼）
    if let (Some(&first), Some(&last)) = (path.first(), path.last()) {
        let gap = (last[0] - first[0]).hypot(last[1] - first[1]);
        if gap > STEP_M {
            if let Some(leg) = grid.astar_xy(last, first, clearance, 0.0) {
                path.extend(leg.into_iter().skip(1));
            }
        }
    }

    // ④ 补距：把闭合后的环重复绕到目标里程（真人也是绕圈跑）
    let base_m = total_m;
    let lap_points: Vec<LngLat> = path.iter().map(|xy| projection.lnglat(*xy)).collect();
    let (points, total_m) = repeat_loop_to_target(&lap_points, target_m, &projection);
    let mut warnings = Vec::new();
    if total_m < target_m * 0.9 {
        warnings.push(format!(
            "混合模式：单圈只有 {base_m:.0}m，绕到 {total_m:.0}m（目标 {target_m:.0}m）",
        ));
    }
    let _ = segments_desc;
    Ok(RouteCandidate {
        points,
        checkpoints: tasks.to_vec(),
        tour_m: total_m,
        total_m,
        end_poi: None,
        corners: 0,
        straight_ratio: 0.0,
        curve_ratio: 0.0,
        score: 0.0,
        order: (0..tasks.len()).collect(),
        warnings,
    })
}

fn generate_with(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
    clearance: f64,
    hard: f64,
    wall_margin: f64,
    allow_direct: bool,
) -> Result<RouteCandidate, String> {
    if tasks.len() < 2 {
        return Err("V4 至少需要 2 个打卡点".into());
    }
    let mut rng = Rng::new(seed);
    let mut grid = Grid::new(campus, clearance, wall_margin);
    let mut warnings: Vec<String> = Vec::new();

    // ① 主路径：严格 P1→P5，A* 避建筑（这段顺序是硬约束，不能动）。
    let mut path: Vec<[f64; 2]> = Vec::new();
    for (index, pair) in tasks.windows(2).enumerate() {
        let searched = grid
            .astar(pair[0], pair[1], clearance, 0.0)
            .or_else(|| {
                grid.clearance = hard;
                grid.astar(pair[0], pair[1], hard, 0.0)
            });
        let leg = match searched {
            Some(leg) => leg,
            None if !allow_direct => {
                return Err(format!(
                    "V4：P{} → P{} 之间找不到避开建筑的路径（净空 {:.0}/{:.0}m、围墙 {:.0}m）",
                    index + 1,
                    index + 2,
                    clearance,
                    hard,
                    wall_margin
                ));
            }
            None => {
                // 兜底：这两点在 campus.json 里完全没有连通通道（被整栋楼隔断）。
                // 直连保证流程还能出轨迹，但会穿过建筑——必须记警告，
                // 让日志明确告诉用户去「校园地图」补一条走廊/门洞。
                let start_ok = grid.nearest_free(pair[0], hard).is_some();
                let end_ok = grid.nearest_free(pair[1], hard).is_some();
                let projection = geo::Projection::for_polygon(&campus.area);
                let a_wall = geo::distance_to_polygon_boundary(pair[0], &campus.area, &projection);
                let b_wall = geo::distance_to_polygon_boundary(pair[1], &campus.area, &projection);
                warnings.push(format!(
                    "P{} → P{} 之间没有通道，已直连穿过建筑（P{} 可落位={} 距围墙 {:.0}m；P{} 可落位={} 距围墙 {:.0}m；净空 {:.0}/{:.0}m）——大概率被判异常，请在「校园地图」补走廊/门洞",
                    index + 1,
                    index + 2,
                    index + 1,
                    start_ok,
                    a_wall,
                    index + 2,
                    end_ok,
                    b_wall,
                    clearance,
                    hard
                ));
                let a = grid.proj.xy(pair[0]);
                let b = grid.proj.xy(pair[1]);
                let span = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
                let steps = (span / STEP_M).ceil().max(1.0) as usize;
                (0..=steps)
                    .map(|k| {
                        let t = k as f64 / steps as f64;
                        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
                    })
                    .collect()
            }
        };
        let mut leg = if path.is_empty() { leg } else { leg.into_iter().skip(1).collect() };
        path.append(&mut leg);
    }
    // 主路径先平滑 + 圆角。注意只能处理这一段：全局平滑会把后面「去程/回程」
    // 两条相隔 3.5m 的平行线平均成一条，2km 直接缩水成 1.9km。
    path = round_corners(&smooth(&path, &grid, 2, hard), 4.5, 38.0, &grid, hard);
    grid.mark_visits(&path);
    let mut walked_xy = path.clone();
    let mut total_m: f64 = path
        .windows(2)
        .map(|w| ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt())
        .sum();

    // ② 补距：往「没走过的地方」绕，而不是原路倒着再走一遍。
    //    回程 A* 给走过的格子加价，它会自己挑平行路/别的路绕回来。
    let mut guard = 0;
    while total_m < target_m * 0.999 && guard < 24 {
        let Some(current) = path.last().copied() else { break };
        let Some(goal) = grid.pick_far_goal(&walked_xy, current, &mut rng) else { break };
        let from = grid.proj.lnglat(current);
        let leg = grid
            .astar(from, goal, clearance, 3.5)
            .or_else(|| grid.astar(from, goal, hard, 3.5));
        let Some(leg) = leg else { break };
        if leg.len() < 3 {
            break;
        }
        // 补距段自己圆角，不进全局平滑。
        let leg = round_corners(&leg, 4.5, 38.0, &grid, hard);
        let mut added = 0.0f64;
        let mut last = current;
        for cell in leg.iter().skip(1) {
            added += ((cell[0] - last[0]).powi(2) + (cell[1] - last[1]).powi(2)).sqrt();
            path.push(*cell);
            walked_xy.push(*cell);
            last = *cell;
            if total_m + added >= target_m * 0.999 {
                break;
            }
        }
        total_m += added;
        grid.mark_visits(&leg);
        guard += 1;
    }

    // ③ 兜底：校园确实走遍了（或太小）才允许折返，但回程整体横向错开 3.5m，
    //    像去程靠一侧、回程靠另一侧的人行道，而不是同一条线跑两遍。
    if total_m < target_m * 0.995 {
        let mut back_guard = 0;
        while total_m < target_m * 0.999 && back_guard < 8 {
            let reverse: Vec<[f64; 2]> = path.iter().rev().skip(1).copied().collect();
            if reverse.is_empty() {
                break;
            }
            let sign = if back_guard % 2 == 0 { 1.0 } else { -1.0 };
            let reverse: Vec<[f64; 2]> = offset_polyline(&reverse, sign * 3.5)
                .into_iter()
                .zip(reverse.iter())
                .map(|(shifted, origin)| {
                    if grid.free(shifted, hard) { shifted } else { *origin }
                })
                .collect();
            let mut last = path.last().copied().unwrap_or(reverse[0]);
            for cell in reverse {
                let step = ((cell[0] - last[0]).powi(2) + (cell[1] - last[1]).powi(2)).sqrt();
                path.push(cell);
                total_m += step;
                last = cell;
                if total_m >= target_m * 0.999 {
                    break;
                }
            }
            back_guard += 1;
        }
    }

    if path.len() < 2 {
        return Err("V4 路线过短".into());
    }
    // ④ 收尾：主路径/补距段前面已经平滑+圆角，这里只叠低频漂移。
    let final_path = jitter(&path, &grid, seed, hard);
    if final_path.len() < 2 {
        return Err("V4 路线为空".into());
    }
    let corners = count_corners(&path);
    let mut points: Vec<LngLat> = final_path.iter().map(|p| grid.proj.lnglat(*p)).collect();

    // ⑤ 截到目标距离，不多跑。
    let mut kept: Vec<LngLat> = Vec::with_capacity(points.len());
    let mut acc = 0.0f64;
    kept.push(points[0]);
    for pair in points.windows(2) {
        let segment = grid.proj.distance_m(pair[0], pair[1]);
        if acc + segment >= target_m {
            let need = (target_m - acc).max(0.0);
            if segment > 1e-9 {
                let t = need / segment;
                kept.push([
                    pair[0][0] + (pair[1][0] - pair[0][0]) * t,
                    pair[0][1] + (pair[1][1] - pair[0][1]) * t,
                ]);
            }
            acc = target_m;
            break;
        }
        kept.push(pair[1]);
        acc += segment;
    }
    points = kept;
    let total = acc;
    // 起点硬约束：首点精确等于服务端给的第一个打卡点（A* 会把起点吸到网格上，
    // 差个 1-2m；build_track 也会兜一次，这里先对齐）。
    if let Some(first) = points.first_mut() {
        *first = tasks[0];
    }

    Ok(RouteCandidate {
        points,
        checkpoints: tasks.to_vec(),
        tour_m: total,
        total_m: total,
        end_poi: None,
        corners,
        straight_ratio: 0.0,
        curve_ratio: 0.0,
        score: 0.0,
        order: (0..tasks.len()).collect(),
        warnings,
    })
}

fn polyline_len(points: &[LngLat], proj: &geo::Projection) -> f64 {
    points.windows(2).map(|w| proj.distance_m(w[0], w[1])).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campus::{BlockedArea, Campus, PlaygroundArea, Polygon};

    fn meters(projection: &geo::Projection) -> impl Fn(f64, f64) -> LngLat + '_ {
        move |east: f64, north: f64| projection.lnglat([east, north])
    }

    /// 600m × 400m 校区 + 三栋挡路建筑，5 个分散的打卡点（都不在操场）。
    fn campus_with_buildings() -> (Campus, Vec<LngLat>) {
        let m_per_deg_lng = 111_320.0 * 30.5f64.to_radians().cos();
        let bd = |east: f64, north: f64| -> LngLat {
            [114.3 + east / m_per_deg_lng, 30.5 + north / 111_132.0]
        };
        let mut campus = Campus::new(
            Polygon::new(vec![
                bd(-300.0, -200.0),
                bd(300.0, -200.0),
                bd(300.0, 200.0),
                bd(-300.0, 200.0),
            ]),
            1,
        );
        campus.allow_shrink_m = 1.0;
        for (index, (cx, cy, w, h)) in [
            (-150.0, 40.0, 60.0, 50.0),
            (60.0, -60.0, 80.0, 60.0),
            (180.0, 90.0, 70.0, 70.0),
        ]
        .iter()
        .enumerate()
        {
            campus.blocked.push(BlockedArea {
                id: index as u64 + 1,
                name: format!("楼{}", index + 1),
                polygon: Polygon::new(vec![
                    bd(cx - w * 0.5, cy - h * 0.5),
                    bd(cx + w * 0.5, cy - h * 0.5),
                    bd(cx + w * 0.5, cy + h * 0.5),
                    bd(cx - w * 0.5, cy + h * 0.5),
                ]),
            });
        }
        let tasks = vec![
            bd(-220.0, -150.0),
            bd(-40.0, 120.0),
            bd(120.0, -150.0),
            bd(240.0, -40.0),
            bd(40.0, 150.0),
        ];
        (campus, tasks)
    }

    /// 校园模式的老毛病：2km = 「一条线倒着再走一遍」，重复度 100%。
    /// 现在要求绕到没走过的地方去补，重复度必须明显低于 100%。
    #[test]
    fn campus_route_does_not_just_run_the_same_line_twice() {
        let (campus, tasks) = campus_with_buildings();
        let candidate = generate(&campus, &tasks, 2000.0, 7).expect("校园路线应生成");
        assert!((candidate.total_m - 2000.0).abs() < 5.0, "总长 {}", candidate.total_m);
        assert_eq!(candidate.points.first().copied(), Some(tasks[0]));
        let projection = geo::Projection::for_polygon(&campus.area);
        let xy: Vec<[f64; 2]> = candidate.points.iter().map(|p| projection.xy(*p)).collect();
        let (mut dup, mut total) = (0usize, 0usize);
        for i in (0..xy.len()).step_by(6) {
            let lo = i.saturating_sub(40);
            let hi = (i + 40).min(xy.len());
            let mut best = f64::INFINITY;
            for (j, p) in xy.iter().enumerate() {
                if j >= lo && j < hi {
                    continue;
                }
                let d = ((p[0] - xy[i][0]).powi(2) + (p[1] - xy[i][1]).powi(2)).sqrt();
                if d < best {
                    best = d;
                }
            }
            if best.is_finite() {
                total += 1;
                if best < 2.0 {
                    dup += 1;
                }
            }
        }
        let ratio = dup as f64 / total.max(1) as f64;
        assert!(ratio < 0.7, "重复度 {:.0}%，还是在原路折返", ratio * 100.0);
        assert!(candidate.corners >= 0);
    }

    #[test]
    fn campus_route_keeps_clear_of_buildings_and_walls() {
        let (campus, tasks) = campus_with_buildings();
        let candidate = generate(&campus, &tasks, 2000.0, 11).expect("校园路线应生成");
        let projection = geo::Projection::for_polygon(&campus.area);
        for point in &candidate.points {
            assert!(campus.route_point_ok(*point, 0.2), "点越界 {point:?}");
            assert!(
                geo::distance_to_polygon_boundary(*point, &campus.area, &projection) >= 5.0,
                "贴围墙 5m 内 {point:?}"
            );
        }
    }

    #[test]
    fn campus_route_varies_with_seed() {
        let (campus, tasks) = campus_with_buildings();
        let a = generate(&campus, &tasks, 2000.0, 3).expect("seed 3");
        let b = generate(&campus, &tasks, 2000.0, 4).expect("seed 4");
        let projection = geo::Projection::for_polygon(&campus.area);
        let count = a.points.len().min(b.points.len());
        let far = (0..count)
            .filter(|&i| projection.distance_m(a.points[i], b.points[i]) > 5.0)
            .count();
        assert!(
            far as f64 / count.max(1) as f64 > 0.3,
            "两个 seed 只有 {far}/{count} 点不同"
        );
    }

    fn playground_campus() -> Campus {
        let mut campus = Campus::new(
            Polygon::new(vec![
                [114.290, 30.494],
                [114.302, 30.494],
                [114.302, 30.506],
                [114.290, 30.506],
            ]),
            1,
        );
        campus.playgrounds.push(PlaygroundArea {
            id: 1,
            name: "操场".into(),
            polygon: Polygon::new(vec![
                [114.2945, 30.4980],
                [114.2985, 30.4980],
                [114.2985, 30.5020],
                [114.2945, 30.5020],
            ]),
        });
        campus.allow_shrink_m = 1.0;
        campus
    }

    /// 5 个点正好落在同一椭圆上（真椭圆拟合路径）。
    fn ellipse_checkpoints(campus: &Campus) -> Vec<LngLat> {
        let projection = geo::Projection::for_polygon(&campus.area);
        let center = projection.xy([114.2965, 30.5000]);
        let (semi_a, semi_b, rotation) = (48.0_f64, 27.0_f64, 0.35_f64);
        (0..5)
            .map(|index| {
                let theta = std::f64::consts::TAU * index as f64 / 5.0 + 0.4;
                let local = [semi_a * theta.cos(), semi_b * theta.sin()];
                let xy = [
                    center[0] + local[0] * rotation.cos() - local[1] * rotation.sin(),
                    center[1] + local[0] * rotation.sin() + local[1] * rotation.cos(),
                ];
                projection.lnglat(xy)
            })
            .collect()
    }

    #[test]
    fn playground_ellipse_passes_every_checkpoint_and_hits_target() {
        let campus = playground_campus();
        let tasks = ellipse_checkpoints(&campus);
        let candidate = playground_ellipse(&campus, &tasks, 2000.0).expect("应生成操场椭圆");
        let projection = geo::Projection::for_polygon(&campus.area);

        assert!(
            (candidate.total_m - 2000.0).abs() < 2.0,
            "总长应贴目标 2000m，实际 {}",
            candidate.total_m
        );
        assert_eq!(candidate.points.first().copied(), Some(tasks[0]));
        for (index, task) in tasks.iter().enumerate() {
            let nearest = candidate
                .points
                .iter()
                .map(|point| projection.distance_m(*point, *task))
                .fold(f64::INFINITY, f64::min);
            assert!(nearest < 0.05, "P{} 未被经过，最近 {nearest:.3}m", index + 1);
        }
        for point in &candidate.points {
            assert!(campus.route_point_ok(*point, 0.2), "路线点越界 {point:?}");
        }
        // 2000m 至少绕好几圈。
        assert!(candidate.total_m > 800.0);
    }

    #[test]
    fn radial_loop_passes_arbitrary_checkpoints() {
        let campus = playground_campus();
        let projection = geo::Projection::for_polygon(&campus.area);
        let center = projection.xy([114.2965, 30.5000]);
        let offsets = [
            (0.0_f64, 0.0_f64),
            (30.0, 8.0),
            (-10.0, 35.0),
            (-40.0, -5.0),
            (12.0, -30.0),
        ];
        let xy: Vec<[f64; 2]> = offsets
            .iter()
            .map(|(dx, dy)| [center[0] + dx, center[1] + dy])
            .collect();
        let path = radial_ellipse_loop(&campus, &campus.playgrounds[0], &projection, &xy)
            .expect("径向椭圆应生成");
        for point in &xy {
            let nearest = path
                .iter()
                .map(|sample| (sample[0] - point[0]).hypot(sample[1] - point[1]))
                .fold(f64::INFINITY, f64::min);
            assert!(nearest < 0.05, "打卡点未被穿过，最近 {nearest:.3}m");
        }
    }

    #[test]
    fn playground_ellipse_rejects_campus_without_playground() {
        let mut campus = playground_campus();
        campus.playgrounds.clear();
        let tasks = vec![[114.2960, 30.5000], [114.2970, 30.5005]];
        assert!(playground_ellipse(&campus, &tasks, 2000.0).is_err());
    }
}
