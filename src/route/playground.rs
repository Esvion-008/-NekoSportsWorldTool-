//! V4 操场跑道模式：把「操场多边形 + 服务器打卡点」变成真人在田径场上跑出来的轨迹。
//!
//! ## 旧实现为什么假
//!
//! 旧版 `v4::playground_ellipse` 用「过 5 个打卡点的圆锥曲线」当环线，形状完全由点位
//! 决定：长轴方向跟操场长边对不上、弯道半径靠点位碰运气、整圈是一条纯椭圆（没有直道），
//! 而且为了穿过点位常常贴到操场围栏边上。真人跑出来的不是这样：
//!
//! * 田径场的形状是固定的**胶囊形**——两条直道 + 两个半圆，直道占全圈约 42%；
//! * 跑者在一到八道之间，道宽 1.22m，圈与圈之间会换道，所以每一圈的线不重合；
//! * 打卡点在跑道内场/边上时，真人是「跑进去打一下卡再绕回来」，不是把整圈拽歪。
//!
//! ## 这里的做法
//!
//! 1. **拟合跑道**：从操场多边形估中心 / 长轴 / 可用半长半宽，套标准 400m 比例
//!    （直道 84.39m、半径 36.5m），放不下就等比缩；逐步收缩直到整圈落在操场内。
//! 2. **打卡点只弯不拽**：每个点在它自己的弧长位置上做宽度 26m 的局部径向弯曲
//!    （最大 24m），曲线「绕过去打一下卡」后立刻回到跑道；跑道形状不动。
//! 3. **分道 + 每圈独立**：每条记录随机选道次（1-8 道，1.22m/道），圈间按概率换道，
//!    圈内再加低频横向摆动，使相邻两圈的轨迹横向错开而不是同一条线。
//! 4. **起点硬约束**：弧长 0 = 第一个打卡点，首点精确等于 P1（服务端要求从起点跑）。
//!
//! 叠加 GPS 噪声见 [`super::apply_gps_noise`]；采样落位见 [`super::build_track`]。

use super::RouteCandidate;
use crate::campus::{geo, Campus, LngLat, PlaygroundArea};
use crate::track::geom::Rng;

/// 单条跑道宽度（米）。
pub const LANE_WIDTH_M: f64 = 1.22;
/// 标准 400m 田径场弯道半径（第 1 道测量线）。
pub const STADIUM_RADIUS_M: f64 = 36.5;
/// 标准 400m 田径场直道全长（米）。
pub const STADIUM_STRAIGHT_M: f64 = 84.39;

const TAU: f64 = std::f64::consts::TAU;
/// 跑道环离操场多边形边界的最小距离（米）。
const SAFE_MARGIN_M: f64 = 5.0;
/// 轨迹离建筑的最小净空（米）。
///
/// 服务端会把「贴着楼」的轨迹判成异常（实测反馈：距离建筑物太近）。真机 GPS 精度
/// 一般 3–5m，所以几何本身至少要和楼拉开 8m，再加上 ±2m 噪声才不会被判穿楼。
pub const MIN_CLEARANCE_M: f64 = 8.0;
/// 逐级降级的净空档位：优先 10m；操场贴楼放不下时一级级往回收，最后才用 5m。
const CLEARANCE_STEPS: [f64; 4] = [10.0, 8.0, 6.0, 5.0];
/// 每圈几何采样步长（米）。
const SAMPLE_STEP_M: f64 = 1.0;
/// 小偏差（≤4m）用宽一点、软一点的余弦核；大偏差用「平台 + 过渡」的折返核。
const BEND_SOFT_WIDTH_M: f64 = 12.0;
const BEND_FLAT_M: f64 = 5.0;
const BEND_RAMP_M: f64 = 15.0;
/// 打卡点局部弯曲的最大径向幅度（米）。
const BEND_MAX_M: f64 = 24.0;
/// 道次上限（1-8 道）。
const MAX_LANE: usize = 8;
/// 圈内换道过渡长度（米）。
const LANE_BLEND_M: f64 = 18.0;
/// 单条记录最多圈数（防止目标距离异常时死循环）。
const MAX_LAPS: usize = 64;

/// 胶囊形跑道中心线：`center` + 长轴 `dir`，直道半长 `half_straight`，弯道半径 `radius`。
///
/// 弧长参数化顺序（俯视逆时针）：+法向侧直道 → 左端半圆 → -法向侧直道 → 右端半圆。
#[derive(Clone, Copy, Debug)]
pub struct Stadium {
    pub center: [f64; 2],
    pub dir: [f64; 2],
    pub half_straight: f64,
    pub radius: f64,
}

impl Stadium {
    /// 一圈周长（米）。
    pub fn perimeter(&self) -> f64 {
        4.0 * self.half_straight + TAU * self.radius
    }

    /// 弧长 `s` → （中心线坐标，向外单位法向）。
    pub fn at(&self, s: f64) -> ([f64; 2], [f64; 2]) {
        let perimeter = self.perimeter();
        let mut s = s.rem_euclid(perimeter.max(1e-9));
        let half = self.half_straight;
        let radius = self.radius;
        let u = self.dir;
        let v = [-self.dir[1], self.dir[0]];
        let place = |along: f64, lateral: f64| {
            [
                self.center[0] + u[0] * along + v[0] * lateral,
                self.center[1] + u[1] * along + v[1] * lateral,
            ]
        };
        let straight = 2.0 * half;
        let arc = std::f64::consts::PI * radius;
        // ① +法向侧直道：along 从 +half 递减到 -half
        if s < straight {
            return (place(half - s, radius), v);
        }
        s -= straight;
        // ② 左端半圆：φ 从 +π/2 递增到 +3π/2
        if s < arc {
            let phi = std::f64::consts::FRAC_PI_2 + s / radius;
            let (sin, cos) = phi.sin_cos();
            let normal = [u[0] * cos + v[0] * sin, u[1] * cos + v[1] * sin];
            return (place(-half + radius * cos, radius * sin), normal);
        }
        s -= arc;
        // ③ -法向侧直道：along 从 -half 递增到 +half
        if s < straight {
            return (place(-half + s, -radius), [-v[0], -v[1]]);
        }
        s -= straight;
        // ④ 右端半圆：φ 从 -π/2 递增到 +π/2
        let phi = -std::f64::consts::FRAC_PI_2 + s / radius;
        let (sin, cos) = phi.sin_cos();
        let normal = [u[0] * cos + v[0] * sin, u[1] * cos + v[1] * sin];
        (place(half + radius * cos, radius * sin), normal)
    }

    /// 点 → （最近中心线弧长，带符号的径向偏移）。
    ///
    /// 粗采样 + 三分钟细化，精度好于 0.05m；跑道尺度下足够。
    pub fn param_of(&self, point: [f64; 2]) -> (f64, f64) {
        let perimeter = self.perimeter();
        let samples = 900usize;
        let mut best_s = 0.0f64;
        let mut best_d = f64::INFINITY;
        for index in 0..samples {
            let s = perimeter * index as f64 / samples as f64;
            let (candidate, _) = self.at(s);
            let d = (candidate[0] - point[0]).powi(2) + (candidate[1] - point[1]).powi(2);
            if d < best_d {
                best_d = d;
                best_s = s;
            }
        }
        let step = perimeter / samples as f64;
        let mut low = best_s - step;
        let mut high = best_s + step;
        for _ in 0..48 {
            let m1 = low + (high - low) / 3.0;
            let m2 = high - (high - low) / 3.0;
            let (p1, _) = self.at(m1);
            let (p2, _) = self.at(m2);
            let d1 = (p1[0] - point[0]).powi(2) + (p1[1] - point[1]).powi(2);
            let d2 = (p2[0] - point[0]).powi(2) + (p2[1] - point[1]).powi(2);
            if d1 < d2 {
                high = m2;
            } else {
                low = m1;
            }
        }
        let s = ((low + high) * 0.5).rem_euclid(perimeter);
        let (base, normal) = self.at(s);
        let offset = (point[0] - base[0]) * normal[0] + (point[1] - base[1]) * normal[1];
        (s, offset)
    }

    /// 直道里程占全圈的比例（正常 400m 跑道约 0.42）。
    pub fn straight_ratio(&self) -> f64 {
        let perimeter = self.perimeter();
        if perimeter <= 1e-9 {
            return 0.0;
        }
        (4.0 * self.half_straight / perimeter).clamp(0.0, 1.0)
    }

    /// 等比例缩放（保持跑道形状）。
    pub fn scale(&mut self, factor: f64) {
        self.half_straight *= factor;
        self.radius *= factor;
    }
}

/// 从操场多边形拟合标准跑道。长轴取「边长⁴ 加权的方向平均」，抗手绘/梯形畸变。
pub fn fit_stadium(playground: &PlaygroundArea, projection: &geo::Projection) -> Option<Stadium> {
    let ring = &playground.polygon.outer;
    if ring.len() < 3 {
        return None;
    }
    let points: Vec<[f64; 2]> = ring.iter().map(|point| projection.xy(*point)).collect();
    let count = points.len() as f64;
    let mx = points.iter().map(|point| point[0]).sum::<f64>() / count;
    let my = points.iter().map(|point| point[1]).sum::<f64>() / count;

    // 方向平均用 2θ 形式，边方向正负无关；权重 len⁴ 让长边主导。
    let (mut cos_sum, mut sin_sum) = (0.0f64, 0.0f64);
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        let edge = [b[0] - a[0], b[1] - a[1]];
        let len = (edge[0] * edge[0] + edge[1] * edge[1]).sqrt();
        if len < 1e-6 {
            continue;
        }
        let angle = edge[1].atan2(edge[0]);
        let weight = len.powi(4);
        cos_sum += weight * (2.0 * angle).cos();
        sin_sum += weight * (2.0 * angle).sin();
    }
    if cos_sum.abs() < 1e-12 && sin_sum.abs() < 1e-12 {
        return None;
    }
    let theta = 0.5 * sin_sum.atan2(cos_sum);
    let mut dir = [theta.cos(), theta.sin()];
    let mut perp = [-dir[1], dir[0]];

    // 沿两轴投影出包围范围，把中心挪到包围盒中心，充分利用操场形状。
    let (mut u0, mut u1) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut v0, mut v1) = (f64::INFINITY, f64::NEG_INFINITY);
    for point in &points {
        let du = (point[0] - mx) * dir[0] + (point[1] - my) * dir[1];
        let dv = (point[0] - mx) * perp[0] + (point[1] - my) * perp[1];
        u0 = u0.min(du);
        u1 = u1.max(du);
        v0 = v0.min(dv);
        v1 = v1.max(dv);
    }
    if !(u1 - u0).is_finite() || !(v1 - v0).is_finite() {
        return None;
    }
    let center = [
        mx + dir[0] * (u0 + u1) * 0.5 + perp[0] * (v0 + v1) * 0.5,
        my + dir[1] * (u0 + u1) * 0.5 + perp[1] * (v0 + v1) * 0.5,
    ];
    let mut half_len = (u1 - u0) * 0.5;
    let mut half_wid = (v1 - v0) * 0.5;
    // dir 必须是长轴方向。
    if half_len < half_wid {
        std::mem::swap(&mut half_len, &mut half_wid);
        std::mem::swap(&mut dir, &mut perp);
    }
    if half_len < 8.0 || half_wid < 5.0 {
        return None;
    }

    let usable_len = (half_len - SAFE_MARGIN_M).max(6.0);
    let usable_wid = (half_wid - SAFE_MARGIN_M).max(4.0);
    // 优先套标准 400m 跑道；放不下就等比缩到刚好放下。
    let mut radius = STADIUM_RADIUS_M.min(usable_wid).max(8.0);
    let mut half_straight = (usable_len - radius).min(STADIUM_STRAIGHT_M * 0.5);
    if half_straight < 4.0 {
        // 操场过于扁：保证直道至少有 4m，压缩半径。
        radius = (usable_len - 4.0).min(usable_wid).max(6.0);
        half_straight = (usable_len - radius).max(2.0);
    }
    if !(radius.is_finite() && half_straight.is_finite()) {
        return None;
    }
    Some(Stadium {
        center,
        dir,
        half_straight,
        radius,
    })
}

/// 拟合跑道并逐步收缩，直到整圈都落在操场内且离建筑 `clearance` 米以上。
pub fn fit_stadium_shrunk(
    campus: &Campus,
    playground: &PlaygroundArea,
    projection: &geo::Projection,
    clearance: f64,
) -> Option<(Stadium, usize)> {
    let mut stadium = fit_stadium(playground, projection)?;
    for step in 0..16 {
        if loop_inside(campus, playground, projection, &stadium, clearance) {
            return Some((stadium, step));
        }
        stadium.scale(0.9);
    }
    None
}

/// 整圈是否落在操场内（含 3m 边线容差）、离建筑够远、且校园层面合法。
fn loop_inside(
    campus: &Campus,
    playground: &PlaygroundArea,
    projection: &geo::Projection,
    stadium: &Stadium,
    clearance: f64,
) -> bool {
    let perimeter = stadium.perimeter();
    let samples = 180usize;
    for index in 0..samples {
        let s = perimeter * index as f64 / samples as f64;
        let (point, _) = stadium.at(s);
        let lnglat = projection.lnglat(point);
        if !campus.route_point_ok(lnglat, clearance) {
            return false;
        }
        if !geo::point_in_polygon(lnglat, &playground.polygon, projection)
            && geo::distance_to_polygon_boundary(lnglat, &playground.polygon, projection) > 3.0
        {
            return false;
        }
    }
    true
}

fn pick_playground<'a>(
    campus: &'a Campus,
    tasks: &[LngLat],
    projection: &geo::Projection,
) -> Result<&'a PlaygroundArea, String> {
    campus
        .playgrounds
        .iter()
        .map(|playground| {
            let count = tasks
                .iter()
                .filter(|point| geo::point_in_polygon(**point, &playground.polygon, projection))
                .count();
            (playground, count)
        })
        .max_by_key(|(_, count)| *count)
        .map(|(playground, _)| playground)
        .ok_or_else(|| "V4：校园数据没有 playground 区域".to_string())
}

/// 环形最短弧长差。
fn ring_delta(a: f64, b: f64, perimeter: f64) -> f64 {
    let mut d = (a - b).rem_euclid(perimeter);
    if d > perimeter * 0.5 {
        d -= perimeter;
    }
    d.abs()
}

/// 打卡点径向弯曲的权重核。
///
/// * 偏差 ≤4m（点位本来就压在跑道上）：宽 12m 的余弦核，跑者「让一下」就够；
/// * 偏差 >4m（点位在内场/操场入口）：5m 平台 + 15m 过渡，轨迹是「跑出去打卡再回来」，
///   也就是一个有平台的小折返，而不是把整圈慢慢拽歪。
fn bend_weight(distance: f64, amplitude: f64) -> f64 {
    let strength = amplitude.abs();
    let (flat, ramp) = if strength <= 4.0 {
        (0.0, BEND_SOFT_WIDTH_M)
    } else {
        // 偏差越大，过渡越长：避免「24m 高、15m 宽」那种尖包（转角会到 50°+）。
        (
            BEND_FLAT_M * 0.6,
            (BEND_RAMP_M + (strength - 4.0) * 1.6).clamp(BEND_RAMP_M, 34.0),
        )
    };
    if distance <= flat {
        return 1.0;
    }
    let t = (distance - flat) / ramp;
    if t >= 1.0 {
        return 0.0;
    }
    let cosine = 0.5 * (1.0 + (std::f64::consts::PI * t).cos());
    cosine * cosine
}

/// 首圈道次：多数学生在 1-5 道，靠内侧。
fn seed_lane(rng: &mut Rng) -> f64 {
    let lane = rng.weighted(&[(1usize, 26), (2, 30), (3, 22), (4, 14), (5, 6), (6, 2)]);
    lane as f64 * LANE_WIDTH_M
}

/// 圈间换道：60% 不动，其余相邻换一道，再叠一点道内漂移。
fn next_lane(rng: &mut Rng, current: f64) -> f64 {
    let lane = (current / LANE_WIDTH_M).round() as i64;
    let step = if rng.random() < 0.62 {
        0
    } else {
        rng.choice(&[-1i64, -1, 1, 1, 2, -2])
    };
    let lane = (lane + step).clamp(0, MAX_LANE as i64 - 1) as f64;
    lane * LANE_WIDTH_M + rng.uniform(-0.30, 0.30)
}

/// 生成操场的真人式绕圈路线。
///
/// * `tasks` 为 BD09 打卡点，顺序不可变，首点即起跑点。
/// * `target_m` 为目标里程（最后一圈按剩余米数截断）。
/// * `seed` 决定道次 / 换道 / 摆动相位，不同账号得到不同轨迹。
/// 操场内连续打卡点的连接：按打卡顺序沿跑道中心线走弧长。
///
/// 混合模式专用：5 个点里有一部分在操场、一部分在操场外时，
/// 操场内那一段不该被当成「校园空地」乱连，而应该沿跑道走。
pub fn arc_segment(stadium: &Stadium, tasks_xy: &[[f64; 2]]) -> Vec<[f64; 2]> {
    if tasks_xy.is_empty() {
        return Vec::new();
    }
    let perimeter = stadium.perimeter();
    let params: Vec<f64> = tasks_xy.iter().map(|p| stadium.param_of(*p).0).collect();
    let mut out: Vec<[f64; 2]> = Vec::new();
    for index in 0..params.len() {
        if index == 0 {
            out.push(stadium.at(params[0]).0);
            continue;
        }
        // 沿「较短方向」从上一个点走到这个点（真人也是就近绕过去）
        let mut delta = params[index] - params[index - 1];
        if delta > perimeter * 0.5 {
            delta -= perimeter;
        } else if delta < -perimeter * 0.5 {
            delta += perimeter;
        }
        let steps = (delta.abs() / SAMPLE_STEP_M).ceil().max(1.0) as usize;
        for k in 1..=steps {
            let s = params[index - 1] + delta * k as f64 / steps as f64;
            out.push(stadium.at(s).0);
        }
        // 关键：打卡点本体可能离跑道中心线很远（个别学校能差到几十米），
        // 只走中心线等于没经过这个点。这里插一段「跑过去打点、再回跑道」，
        // 真人也是这么跑的。
        let base = stadium.at(params[index]).0;
        let task = tasks_xy[index];
        let gap = ((task[0] - base[0]).powi(2) + (task[1] - base[1]).powi(2)).sqrt();
        if gap > 1.5 {
            out.push(task);
            out.push(base);
        }
    }
    // 最后一个点也要打（上面只处理了 index>0 的）
    if let (Some(&last_task), Some(&last_param)) = (tasks_xy.last(), params.last()) {
        let base = stadium.at(last_param).0;
        let gap = ((last_task[0] - base[0]).powi(2) + (last_task[1] - base[1]).powi(2)).sqrt();
        if gap > 1.5 && out.last().map(|p| ((p[0]-last_task[0]).powi(2) + (p[1]-last_task[1]).powi(2)).sqrt() > 1.5).unwrap_or(true) {
            out.push(last_task);
            out.push(base);
        }
    }
    out
}

pub fn track_loop(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
) -> Result<RouteCandidate, String> {
    track_loop_with_laps(campus, tasks, target_m, seed).map(|(candidate, _)| candidate)
}

/// 与 [`track_loop`] 相同，另外返回每圈在 `points` 里的起始下标（可视化 / 调试用）。
///
/// 净空逐级降级：先按 10m 排跑道，整圈离楼不够近就缩跑道；实在放不下再退 8/6/5m。
pub fn track_loop_with_laps(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
) -> Result<(RouteCandidate, Vec<usize>), String> {
    let mut last_error = String::new();
    for clearance in CLEARANCE_STEPS {
        match track_loop_with_clearance(campus, tasks, target_m, seed, clearance) {
            Ok(result) => return Ok(result),
            Err(error) => last_error = error,
        }
    }
    Err(if last_error.is_empty() {
        "V4：操场跑道放不下（净空约束太紧）".into()
    } else {
        format!("{last_error}（已从 10m 净空一路降到 5m）")
    })
}

fn track_loop_with_clearance(
    campus: &Campus,
    tasks: &[LngLat],
    target_m: f64,
    seed: u64,
    clearance: f64,
) -> Result<(RouteCandidate, Vec<usize>), String> {
    if tasks.len() < 2 {
        return Err("V4 操场模式至少需要 2 个打卡点".into());
    }
    if !(target_m.is_finite() && target_m > 50.0) {
        return Err("V4 操场模式目标距离异常".into());
    }
    let projection = geo::Projection::for_polygon(&campus.area);
    let playground = pick_playground(campus, tasks, &projection)?;
    let (stadium, shrink_steps) = fit_stadium_shrunk(campus, playground, &projection, clearance)
        .ok_or_else(|| format!("V4：操场跑道环放不进操场多边形（净空 {clearance:.0}m）"))?;
    if shrink_steps > 6 {
        return Err(format!("V4：操场多边形装不下跑道（收缩 {shrink_steps} 次）"));
    }
    let perimeter = stadium.perimeter();
    if perimeter < 60.0 {
        return Err(format!("V4：操场跑道环过短（{perimeter:.0}m）"));
    }

    // 打卡点在跑道参数空间里的位置：(弧长, 径向偏移)。
    let anchors: Vec<(f64, f64)> = tasks
        .iter()
        .map(|point| stadium.param_of(projection.xy(*point)))
        .collect();
    for (index, (_, offset)) in anchors.iter().enumerate() {
        if offset.abs() > 40.0 {
            return Err(format!(
                "V4 操场：P{} 离跑道 {:.0}m，超出打卡落位范围",
                index + 1,
                offset.abs()
            ));
        }
    }
    let start_s = anchors[0].0;
    let mut rng = Rng::new(seed);
    // 绝大多数人逆时针跑。
    let direction = if rng.random() < 0.9 { 1.0 } else { -1.0 };

    // —— 阶段 ❶：先只采骨架（中心线坐标 / 法向 / 想走的横向偏移），不出点 ——
    //
    // 为什么不边算边出点：净空收缩是「逐点二分」的，如果某个点被压回中心线、
    // 隔壁却还留在第 8 道，就会在 2m 内挖出一个 6m 深的凹口，转角直接 80°+。
    // 所以先收集「想走的偏移」和「允许的最大偏移」，再统一平滑成缓坡。
    struct Probe {
        base: [f64; 2],
        normal: [f64; 2],
        raw: f64,
        required: f64,
    }
    let mut probes: Vec<Probe> = Vec::new();
    let mut lap_starts: Vec<usize> = Vec::new();
    let mut lane = seed_lane(&mut rng);
    let mut lap = 0usize;
    let mut lap_begin_lane = lane;
    // 起跑汇入：P1 常常在跑道外 10–25m（场边/入口）。旧做法是每圈都「扎出去 20m
    // 再拐回来」，那就是一个梯形尖包，转角能到 146°，看着像突然掉头。
    // 真人是从场边一路滑进跑道，只滑这一次。
    const ENTRY_LEN_M: f64 = 48.0;
    let start_offset = anchors[0].1.clamp(-BEND_MAX_M, BEND_MAX_M);
    let mut rough_len = 0.0f64;
    while rough_len + 0.5 < target_m && lap < MAX_LAPS {
        let lane_end = next_lane(&mut rng, lane);
        let phase = [rng.uniform(0.0, TAU), rng.uniform(0.0, TAU)];
        let wobble_amp = rng.uniform(0.16, 0.34);
        let bend_scale = rng.uniform(0.94, 1.06);
        // 真人只在第一圈把内场/场边的打卡点扫一遍，后面就是老老实实绕跑道。
        let bend_gain = if lap == 0 { 1.0 } else { 0.0 };
        lap_starts.push(probes.len());
        let mut travelled = 0.0f64;
        while travelled < perimeter - 1e-9 {
            let s = (start_s + direction * travelled).rem_euclid(perimeter);
            let (base, normal) = stadium.at(s);
            // 圈内前 18m 完成换道，避免圈起点处横向瞬移。
            let blend = (travelled / LANE_BLEND_M).clamp(0.0, 1.0);
            let current_lane = lap_begin_lane + (lane_end - lap_begin_lane) * blend;
            // 起跑汇入（只第一圈前 ENTRY_LEN_M 米，smoothstep 平滑下降）。
            let (entry, in_entry) = if lap == 0 && travelled < ENTRY_LEN_M {
                let t = (travelled / ENTRY_LEN_M).clamp(0.0, 1.0);
                let smooth = t * t * (3.0 - 2.0 * t);
                (start_offset * (1.0 - smooth), true)
            } else {
                (0.0, false)
            };
            // 打卡点局部弯曲：第一圈才做；P1 已经由「起跑汇入」处理，这里跳过。
            let mut weight_sum = 0.0f64;
            let mut bend_sum = 0.0f64;
            for (index, (anchor_s, anchor_offset)) in anchors.iter().enumerate() {
                if index == 0 {
                    continue;
                }
                let distance = ring_delta(s, *anchor_s, perimeter);
                let amplitude = anchor_offset.clamp(-BEND_MAX_M, BEND_MAX_M);
                let w = bend_weight(distance, amplitude);
                if w <= 0.0 {
                    continue;
                }
                weight_sum += w;
                bend_sum += w * (amplitude - current_lane) * bend_scale * bend_gain;
            }
            let bend = bend_sum / weight_sum.max(1.0);
            // 汇入段不叠加摆动，免得 S 形再被抖动放大。
            let wobble = if in_entry {
                0.0
            } else {
                (1.0 - weight_sum.min(1.0)).max(0.0)
                    * (wobble_amp * (TAU * travelled / perimeter + phase[0]).sin()
                        + 0.10 * (3.0 * TAU * travelled / perimeter + phase[1]).sin())
            };
            probes.push(Probe {
                base,
                normal,
                raw: current_lane + entry + bend + wobble,
                // 汇入段只保证「不穿楼」（4m）：P1 自己就贴在楼边 8m，从它到跑道
                // 这段场地本身只有 4.5m 净空，要求更高只能硬压偏移，压出来就是急转。
                required: if in_entry { 4.0 } else { clearance },
            });
            rough_len += SAMPLE_STEP_M;
            if rough_len >= target_m {
                break;
            }
            travelled += SAMPLE_STEP_M;
        }
        lap_begin_lane = lane_end;
        lane = lane_end;
        lap += 1;
    }
    if probes.len() < 3 {
        return Err("V4：操场跑道环采样点不足".into());
    }

    // —— 阶段 ❷：每点求「允许的最大横向偏移」（越界就二分收缩）——
    let position_of = |probe: &Probe, lateral: f64| -> [f64; 2] {
        [
            probe.base[0] + probe.normal[0] * lateral,
            probe.base[1] + probe.normal[1] * lateral,
        ]
    };
    let mut allowed: Vec<f64> = Vec::with_capacity(probes.len());
    for probe in &probes {
        let position = position_of(probe, probe.raw);
        if sample_ok(campus, playground, &projection, position, probe.required, tasks) {
            allowed.push(probe.raw);
            continue;
        }
        // 从「想走的偏移」一路往内退：允许退到内场（负值），
        // 否则贴楼那几米只能退回中心线，前后就挖出一个凹口。
        const CANDIDATES: [f64; 10] = [1.0, 0.8, 0.6, 0.45, 0.3, 0.15, 0.0, -0.2, -0.4, -0.6];
        let mut chosen = 0.0f64;
        for ratio in CANDIDATES {
            let lateral = probe.raw * ratio;
            if sample_ok(campus, playground, &projection, position_of(probe, lateral), probe.required, tasks) {
                chosen = lateral;
                break;
            }
        }
        allowed.push(chosen);
    }

    // —— 阶段 ❸：把「允许值」摊成缓坡 ——
    //
    // 先高斯平滑（σ≈12m），再逐点取 min(平滑值, 允许值)：凹口被摊平成几十米的
    // 缓坡，而不是 2m 内掉 6m 的台阶。最后再做一次斜率限制兜底。
    const SMOOTH_SIGMA_M: f64 = 12.0;
    const MAX_LATERAL_SLOPE: f64 = 0.42; // ≈ tan(23°)，1m 步长最多横移 0.42m
    let radius = (SMOOTH_SIGMA_M * 3.0) as usize;
    let mut smoothed = vec![0.0f64; allowed.len()];
    for index in 0..allowed.len() {
        let low = index.saturating_sub(radius);
        let high = (index + radius + 1).min(allowed.len());
        let mut weight_sum = 0.0f64;
        let mut value_sum = 0.0f64;
        for other in low..high {
            let d = other as f64 - index as f64;
            let w = (-(d * d) / (2.0 * SMOOTH_SIGMA_M * SMOOTH_SIGMA_M)).exp();
            weight_sum += w;
            value_sum += w * allowed[other];
        }
        smoothed[index] = if weight_sum > 0.0 { value_sum / weight_sum } else { allowed[index] };
    }
    let mut lateral_of: Vec<f64> = smoothed;
    let max_delta = SAMPLE_STEP_M * MAX_LATERAL_SLOPE;
    for index in 1..lateral_of.len() {
        let previous = lateral_of[index - 1];
        lateral_of[index] = lateral_of[index].clamp(previous - max_delta, previous + max_delta);
    }
    for index in (0..lateral_of.len().saturating_sub(1)).rev() {
        let next = lateral_of[index + 1];
        lateral_of[index] = lateral_of[index].clamp(next - max_delta, next + max_delta);
    }

    // —— 阶段 ❹：出点 + 按目标里程截断 ——
    let mut points: Vec<LngLat> = Vec::with_capacity(lateral_of.len());
    let mut total_m = 0.0f64;
    for (index, probe) in probes.iter().enumerate() {
        let lnglat = projection.lnglat(position_of(probe, lateral_of[index]));
        if let Some(previous) = points.last() {
            let segment = projection.distance_m(*previous, lnglat);
            if total_m + segment >= target_m {
                let need = (target_m - total_m).max(0.0);
                if segment > 1e-9 {
                    let t = need / segment;
                    points.push([
                        previous[0] + (lnglat[0] - previous[0]) * t,
                        previous[1] + (lnglat[1] - previous[1]) * t,
                    ]);
                }
                total_m = target_m;
                break;
            }
            total_m += segment;
        }
        points.push(lnglat);
    }
    if points.len() < 3 {
        return Err("V4：操场跑道环采样点不足".into());
    }
    lap_starts.retain(|index| *index < points.len());
    // 起点硬约束：第一条轨迹点必须精确等于服务端给的第一个打卡点。
    points[0] = tasks[0];

    Ok((
        RouteCandidate {
            points,
            checkpoints: tasks.to_vec(),
            tour_m: total_m,
            total_m,
            end_poi: None,
            corners: 0,
            straight_ratio: stadium.straight_ratio(),
            curve_ratio: 1.0 - stadium.straight_ratio(),
            score: 0.0,
            order: (0..tasks.len()).collect(),
            warnings: Vec::new(),
        },
        lap_starts,
    ))
}

/// 采样点合法性：校园层面合法（含建筑净空 `clearance`）+ 不跑出操场太远
/// （打卡点常在边线上，放宽 30m）。
fn sample_ok(
    campus: &Campus,
    playground: &PlaygroundArea,
    projection: &geo::Projection,
    point: [f64; 2],
    clearance: f64,
    tasks: &[LngLat],
) -> bool {
    if !point[0].is_finite() || !point[1].is_finite() {
        return false;
    }
    let lnglat = projection.lnglat(point);
    // 打卡点附近放宽到硬底线：轨迹必须经过打卡点，而打卡点本身可能就贴着楼
    // （个别学校起点距楼只有几米）。不放宽的话偏移会被压回跑道，起点直接断开。
    let near_task = tasks
        .iter()
        .any(|task| projection.distance_m(*task, lnglat) < 22.0);
    let required = if near_task {
        clearance.min(MIN_CLEARANCE_M * 0.85)
    } else {
        clearance
    };
    if !campus.route_point_ok(lnglat, required) {
        return false;
    }
    geo::point_in_polygon(lnglat, &playground.polygon, projection)
        || geo::distance_to_polygon_boundary(lnglat, &playground.polygon, projection) <= 30.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campus::{Campus, PlaygroundArea, Polygon};

    const LAT0: f64 = 30.5;
    const LNG0: f64 = 114.3;

    fn bd(east_m: f64, north_m: f64) -> LngLat {
        let m_per_deg_lng = 111_320.0 * LAT0.to_radians().cos();
        [LNG0 + east_m / m_per_deg_lng, LAT0 + north_m / 111_132.0]
    }

    /// 190m × 106m 的操场（标准 400m 跑道外框 + 缓冲），长轴南北向。
    fn campus_400m() -> Campus {
        let mut campus = Campus::new(
            Polygon::new(vec![
                bd(-600.0, -600.0),
                bd(600.0, -600.0),
                bd(600.0, 600.0),
                bd(-600.0, 600.0),
            ]),
            1,
        );
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
        campus.allow_shrink_m = 1.0;
        campus
    }

    /// 真实打卡点分布：P2/P4/P5 几乎压在跑道上，P1 在跑道外侧 20m，P3 在弯道端外 5m。
    fn playground_checkpoints() -> Vec<LngLat> {
        vec![
            bd(-50.9, -46.3),
            bd(32.5, 14.3),
            bd(-2.5, 84.5),
            bd(-42.9, 32.4),
            bd(-9.4, -75.6),
        ]
    }

    #[test]
    fn stadium_arc_parameter_is_unit_speed() {
        let stadium = Stadium {
            center: [0.0, 0.0],
            dir: [1.0, 0.0],
            half_straight: STADIUM_STRAIGHT_M * 0.5,
            radius: STADIUM_RADIUS_M,
        };
        let perimeter = stadium.perimeter();
        let samples = 5000usize;
        let mut total = 0.0f64;
        for index in 0..samples {
            let a = stadium.at(perimeter * index as f64 / samples as f64).0;
            let b = stadium.at(perimeter * (index + 1) as f64 / samples as f64).0;
            total += ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        }
        assert!((total - perimeter).abs() < 0.2, "折线长 {total} vs 周长 {perimeter}");
    }

    #[test]
    fn bare_loop_length_matches_perimeter() {
        // 固定道次、无 bend 时，一圈几何长度应当就是周长。
        let campus = campus_400m();
        let projection = geo::Projection::for_polygon(&campus.area);
        let (stadium, _) =
            fit_stadium_shrunk(&campus, &campus.playgrounds[0], &projection, MIN_CLEARANCE_M).unwrap();
        let perimeter = stadium.perimeter();
        let mut total = 0.0f64;
        let mut previous: Option<LngLat> = None;
        let steps = perimeter.round() as usize;
        let mut worst = (0usize, 0.0f64);
        for index in 0..=steps {
            let s = perimeter * index as f64 / steps as f64;
            let (point, normal) = stadium.at(s);
            let probe = [point[0] + normal[0] * 3.0, point[1] + normal[1] * 3.0];
            let lnglat = projection.lnglat(probe);
            if let Some(prev) = previous {
                let step = projection.distance_m(prev, lnglat);
                total += step;
                if step > worst.1 {
                    worst = (index, step);
                }
            }
            previous = Some(lnglat);
        }
        // 向外偏 3m 跑 = 弯道半径 +3m，周长自然多 2π·3 ≈ 18.85m（这就是分道一圈比内道长的原因）。
        let expected = perimeter + 2.0 * std::f64::consts::PI * 3.0;
        assert!(
            (total - expected).abs() < 1.5,
            "一圈 {total} vs 期望 {expected}（worst {worst:?}）"
        );
    }

    /// 真实场景：操场三面是楼（campus.json 里操场边距楼只有 0.1–0.3m）。
    /// 服务端会把贴楼的轨迹判「距离建筑物太近」，所以环必须自己缩到 8m 以外。
    #[test]
    fn loop_keeps_min_clearance_from_buildings() {
        use crate::campus::BlockedArea;
        let mut campus = campus_400m();
        campus.blocked.push(BlockedArea {
            id: 9,
            name: "看台".into(),
            polygon: Polygon::new(vec![
                bd(55.0, -100.0),
                bd(80.0, -100.0),
                bd(80.0, 100.0),
                bd(55.0, 100.0),
            ]),
        });
        let tasks = playground_checkpoints();
        let candidate = track_loop(&campus, &tasks, 2000.0, 5).expect("应生成操场环");
        let projection = geo::Projection::for_polygon(&campus.area);
        let mut worst = f64::INFINITY;
        for point in &candidate.points {
            for blocked in &campus.blocked {
                if geo::point_in_polygon(*point, &blocked.polygon, &projection) {
                    worst = 0.0;
                    continue;
                }
                worst = worst.min(geo::distance_to_polygon_boundary(
                    *point,
                    &blocked.polygon,
                    &projection,
                ));
            }
        }
        assert!(worst >= MIN_CLEARANCE_M - 0.6, "最近距离 {worst:.2}m，仍然贴楼");

        // 同一份数据还要检查「不能突然大转弯」：净空收缩必须摊成缓坡，
        // 1m 步长下相邻点转角不该超过 60°。
        let xy: Vec<[f64; 2]> = candidate
            .points
            .iter()
            .map(|point| projection.xy(*point))
            .collect();
        let mut sharpest = 0.0f64;
        for index in 1..xy.len().saturating_sub(1) {
            let v1 = [xy[index - 1][0] - xy[index][0], xy[index - 1][1] - xy[index][1]];
            let v2 = [xy[index + 1][0] - xy[index][0], xy[index + 1][1] - xy[index][1]];
            let n1 = (v1[0] * v1[0] + v1[1] * v1[1]).sqrt();
            let n2 = (v2[0] * v2[0] + v2[1] * v2[1]).sqrt();
            if n1 < 0.2 || n2 < 0.2 {
                continue;
            }
            let cos = ((v1[0] * v2[0] + v1[1] * v2[1]) / (n1 * n2)).clamp(-1.0, 1.0);
            sharpest = sharpest.max(180.0 - cos.acos().to_degrees());
        }
        assert!(sharpest < 60.0, "最大转角 {sharpest:.1}°，出现突然大转弯");
    }

    #[test]
    fn stadium_fits_standard_400m() {
        let campus = campus_400m();
        let projection = geo::Projection::for_polygon(&campus.area);
        let stadium = fit_stadium(&campus.playgrounds[0], &projection).expect("应拟合出跑道");
        // 标准尺寸：半径 36.5m、直道全长 84.39m。
        assert!((stadium.radius - STADIUM_RADIUS_M).abs() < 1.0, "半径 {}", stadium.radius);
        assert!((stadium.half_straight - STADIUM_STRAIGHT_M * 0.5).abs() < 1.5);
        let perimeter = stadium.perimeter();
        assert!((perimeter - 398.0).abs() < 6.0, "周长 {perimeter}");
        assert!((stadium.straight_ratio() - 0.42).abs() < 0.05);
    }

    #[test]
    fn stadium_direction_follows_long_side() {
        let campus = campus_400m();
        let projection = geo::Projection::for_polygon(&campus.area);
        let stadium = fit_stadium(&campus.playgrounds[0], &projection).unwrap();
        // 长轴应当是南北向（dir ≈ (0, ±1)）。
        assert!(stadium.dir[1].abs() > 0.98, "长轴方向 {:?}", stadium.dir);
    }

    #[test]
    fn param_roundtrip_is_exact() {
        let campus = campus_400m();
        let projection = geo::Projection::for_polygon(&campus.area);
        let stadium = fit_stadium(&campus.playgrounds[0], &projection).unwrap();
        let perimeter = stadium.perimeter();
        for index in 0..24 {
            let s = perimeter * index as f64 / 24.0;
            let (point, normal) = stadium.at(s);
            let probe = [point[0] + normal[0] * 3.0, point[1] + normal[1] * 3.0];
            let (s2, offset) = stadium.param_of(probe);
            assert!(ring_delta(s, s2, perimeter) < 0.6, "s={s} s2={s2}");
            assert!((offset - 3.0).abs() < 0.4, "offset={offset}");
        }
    }

    #[test]
    fn loop_hits_target_and_passes_checkpoints() {
        let campus = campus_400m();
        let tasks = playground_checkpoints();
        let candidate = track_loop(&campus, &tasks, 2000.0, 42).expect("应生成跑道环");
        let projection = geo::Projection::for_polygon(&campus.area);
        assert!((candidate.total_m - 2000.0).abs() < 15.0, "总长 {}", candidate.total_m);
        assert_eq!(candidate.points.first().copied(), Some(tasks[0]));
        for (index, task) in tasks.iter().enumerate() {
            let nearest = candidate
                .points
                .iter()
                .map(|point| projection.distance_m(*point, *task))
                .fold(f64::INFINITY, f64::min);
            assert!(nearest < 10.0, "P{} 最近 {nearest:.2}m", index + 1);
        }
        assert!(candidate.points.iter().all(|point| campus.route_point_ok(*point, 0.2)));
    }

    #[test]
    fn laps_do_not_overlap() {
        let campus = campus_400m();
        let tasks = playground_checkpoints();
        let candidate = track_loop(&campus, &tasks, 2400.0, 7).expect("应生成跑道环");
        let projection = geo::Projection::for_polygon(&campus.area);
        // 按 400m 左右切圈，比较相邻两圈同弧长位置的横向偏差。
        let mut accrued = 0.0f64;
        let mut laps: Vec<Vec<LngLat>> = vec![Vec::new()];
        for window in candidate.points.windows(2) {
            accrued += projection.distance_m(window[0], window[1]);
            if accrued > 400.0 {
                accrued -= 400.0;
                laps.push(Vec::new());
            }
            laps.last_mut().unwrap().push(window[0]);
        }
        let usable: Vec<&Vec<LngLat>> = laps.iter().filter(|lap| lap.len() > 100).collect();
        assert!(usable.len() >= 3, "切出 {} 圈", usable.len());
        let mut separated = 0usize;
        let mut compared = 0usize;
        for pair in usable.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            for k in 0..a.len().min(b.len()) {
                compared += 1;
                if projection.distance_m(a[k], b[k]) > 0.35 {
                    separated += 1;
                }
            }
        }
        let ratio = separated as f64 / compared.max(1) as f64;
        assert!(ratio > 0.5, "相邻圈只有 {:.0}% 的点错开", ratio * 100.0);
    }

    #[test]
    fn different_seeds_give_different_tracks() {
        let campus = campus_400m();
        let tasks = playground_checkpoints();
        let projection = geo::Projection::for_polygon(&campus.area);
        let a = track_loop(&campus, &tasks, 2000.0, 1).unwrap();
        let b = track_loop(&campus, &tasks, 2000.0, 2).unwrap();
        let count = a.points.len().min(b.points.len());
        let mut far = 0usize;
        for index in 0..count {
            if projection.distance_m(a.points[index], b.points[index]) > 0.8 {
                far += 1;
            }
        }
        assert!(far as f64 / count as f64 > 0.5, "两个 seed 只有 {far}/{count} 点不同");
    }

    #[test]
    fn straight_sections_are_straight() {
        let campus = campus_400m();
        let tasks = playground_checkpoints();
        let candidate = track_loop(&campus, &tasks, 2000.0, 3).unwrap();
        let projection = geo::Projection::for_polygon(&campus.area);
        // 统计相邻三点转角：直道段应接近 0°，弯道段应接近 步长/半径。
        let mut straight = 0usize;
        let mut total = 0usize;
        for chunk in candidate.points.windows(3) {
            let a = projection.xy(chunk[0]);
            let b = projection.xy(chunk[1]);
            let c = projection.xy(chunk[2]);
            let v1 = [b[0] - a[0], b[1] - a[1]];
            let v2 = [c[0] - b[0], c[1] - b[1]];
            let n1 = (v1[0] * v1[0] + v1[1] * v1[1]).sqrt();
            let n2 = (v2[0] * v2[0] + v2[1] * v2[1]).sqrt();
            if n1 < 0.2 || n2 < 0.2 {
                continue;
            }
            total += 1;
            let cross = (v1[0] * v2[1] - v1[1] * v2[0]).abs();
            let turn_deg = (cross / (n1 * n2)).asin().to_degrees();
            if turn_deg < 1.0 {
                straight += 1;
            }
        }
        let ratio = straight as f64 / total.max(1) as f64;
        assert!(ratio > 0.30, "直道点只占 {:.0}%", ratio * 100.0);
    }
}
