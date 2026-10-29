//! 路线几何造型：折线简化、椭圆弧倒角、5 m 重采样、AR(1) 横向抖动、直/曲统计。

use super::super::campus::geo::Projection;
use super::super::campus::{Campus, LngLat};
use crate::track::geom::Rng;

/// 造型结果。
#[derive(Clone, Debug)]
pub struct ShapeResult {
    pub points: Vec<LngLat>,
    pub total_m: f64,
    pub straight_m: f64,
    pub curve_m: f64,
    pub straight_ratio: f64,
    pub curve_ratio: f64,
    pub corners: usize,
    pub min_curve_radius_m: f64,
}

/// 把路网折线造型为可提交的路线几何。
pub fn shape_route(
    raw: &[LngLat],
    campus: &Campus,
    checkpoints: &[LngLat],
    rng: &mut Rng,
) -> Option<ShapeResult> {
    if raw.len() < 2 {
        return None;
    }
    let projection = Projection::for_polygon(&campus.area);
    let simplified = simplify(raw, &projection);
    let (filleted, _fillet_curve, corners) = fillet(&simplified, &projection, rng);
    let base = resample(&filleted, &projection, 5.0);
    if base.len() < 3 {
        return None;
    }
    let mut best: Option<ShapeResult> = None;
    let mut best_deviation = f64::INFINITY;
    for attempt in 0..6 {
        let coverage = 0.10 + attempt as f64 * 0.08;
        let mut candidate = base.clone();
        let sigma = rng.uniform(0.20, 0.40);
        jitter(
            &mut candidate,
            &projection,
            checkpoints,
            rng,
            coverage,
            sigma,
        );
        // 造型后仍须满足区域内缩与净空；不允许就减少抖动覆盖率重试
        if candidate
            .iter()
            .any(|point| !campus.route_point_ok(*point, 0.8))
        {
            continue;
        }
        snap_checkpoints(&mut candidate, checkpoints, &projection);
        let stats = measure(&candidate, &projection, corners);
        if (0.60..=0.75).contains(&stats.straight_ratio) {
            return Some(stats);
        }
        let deviation = (stats.straight_ratio - 0.65).abs();
        if deviation < best_deviation {
            best_deviation = deviation;
            best = Some(stats);
        }
    }
    // 直线比例低于 60% 的路线不返回，避免接受“绕圈型”结果。
    best.filter(|result| result.straight_ratio >= 0.60)
}

/// 去掉重复点与近似共线点（转角 < 5°）。
pub fn simplify(points: &[LngLat], projection: &Projection) -> Vec<LngLat> {
    let mut out: Vec<LngLat> = Vec::new();
    for point in points {
        if out
            .last()
            .map(|previous| projection.distance_m(*previous, *point) < 0.3)
            .unwrap_or(false)
        {
            continue;
        }
        out.push(*point);
    }
    if out.len() <= 2 {
        return out;
    }
    let mut compact = vec![out[0]];
    for i in 1..(out.len() - 1) {
        let a = projection.xy(compact[compact.len() - 1]);
        let b = projection.xy(out[i]);
        let c = projection.xy(out[i + 1]);
        let angle = direction_angle(a, b, c);
        if angle > 5.0f64.to_radians() {
            compact.push(out[i]);
        }
    }
    compact.push(*out.last().expect("非空"));
    compact
}

/// 椭圆弧倒角：转角 > 15° 的顶点替换为半径 1.5–3 m 的圆弧（圆是椭圆特例），
/// 返回倒角后的折线、圆弧总长与转角数量。
pub fn fillet(
    points: &[LngLat],
    projection: &Projection,
    rng: &mut Rng,
) -> (Vec<LngLat>, f64, usize) {
    if points.len() <= 2 {
        return (points.to_vec(), 0.0, 0);
    }
    let mut out = Vec::new();
    out.push(points[0]);
    let mut curve_m = 0.0;
    let mut corners = 0usize;
    for i in 1..(points.len() - 1) {
        let a = projection.xy(points[i - 1]);
        let b = projection.xy(points[i]);
        let c = projection.xy(points[i + 1]);
        let turn = direction_angle(a, b, c);
        if turn <= 15.0f64.to_radians() {
            out.push(points[i]);
            continue;
        }
        corners += 1;
        let adjacent = (distance(a, b)).min(distance(b, c));
        let radius = rng.uniform(1.5, 3.0).min(adjacent * 0.35).max(0.6);
        let half = (turn / 2.0).clamp(1e-3, 1.4);
        let tangent = (radius * half.tan()).min(adjacent * 0.45);
        if tangent < 0.2 {
            out.push(points[i]);
            continue;
        }
        let radius = tangent / half.tan();
        let u = normalize([b[0] - a[0], b[1] - a[1]]);
        let v = normalize([c[0] - b[0], c[1] - b[1]]);
        let p = [b[0] - u[0] * tangent, b[1] - u[1] * tangent];
        let q = [b[0] + v[0] * tangent, b[1] + v[1] * tangent];
        let bisector = normalize([u[0] - v[0], u[1] - v[1]]);
        let center_distance = radius / half.sin();
        let center = [b[0] - bisector[0] * center_distance, b[1] - bisector[1] * center_distance];
        let angle_p = (p[1] - center[1]).atan2(p[0] - center[0]);
        let angle_q = (q[1] - center[1]).atan2(q[0] - center[0]);
        let mut delta = angle_q - angle_p;
        while delta > std::f64::consts::PI {
            delta -= std::f64::consts::TAU;
        }
        while delta < -std::f64::consts::PI {
            delta += std::f64::consts::TAU;
        }
        let arc_len = radius * delta.abs();
        if arc_len < 0.3 {
            out.push(points[i]);
            continue;
        }
        curve_m += arc_len;
        let steps = ((arc_len / 0.8).ceil() as usize).clamp(4, 24);
        out.push(projection.lnglat(p));
        for step in 1..steps {
            let t = step as f64 / steps as f64;
            let angle = angle_p + delta * t;
            let point = [
                center[0] + radius * angle.cos(),
                center[1] + radius * angle.sin(),
            ];
            out.push(projection.lnglat(point));
        }
        out.push(projection.lnglat(q));
    }
    out.push(points[points.len() - 1]);
    (out, curve_m, corners)
}

/// 按固定间距重采样（保留首末点）。
pub fn resample(points: &[LngLat], projection: &Projection, spacing: f64) -> Vec<LngLat> {
    if points.len() < 2 {
        return points.to_vec();
    }
    let spacing = spacing.max(1.0);
    let mut out = vec![points[0]];
    let mut carry = 0.0;
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let segment = projection.distance_m(a, b);
        if segment <= 1e-6 {
            continue;
        }
        let mut travelled = spacing - carry;
        while travelled <= segment {
            let t = travelled / segment;
            let point = [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
            ];
            out.push(point);
            travelled += spacing;
        }
        carry = (carry + segment) % spacing;
    }
    let last = *points.last().expect("非空");
    if out
        .last()
        .map(|point| projection.distance_m(*point, last) > 0.3)
        .unwrap_or(true)
    {
        out.push(last);
    }
    out
}

/// 分段 AR(1) 横向抖动：覆盖 `coverage` 比例的路段，间隔段保持直线。
pub fn jitter(
    points: &mut [LngLat],
    projection: &Projection,
    protect: &[LngLat],
    rng: &mut Rng,
    coverage: f64,
    sigma: f64,
) {
    if points.len() < 3 {
        return;
    }
    let sigma = sigma.clamp(0.20, 0.50);
    let mut phase = 0.0f64;
    let mut block = 0.0f64;
    let mut noise = 0.0f64;
    let mut protected = vec![false; points.len()];
    for (i, point) in points.iter().enumerate() {
        if i == 0 || i == points.len() - 1 {
            protected[i] = true;
            continue;
        }
        for guard in protect {
            if projection.distance_m(*point, *guard) < 5.0 {
                protected[i] = true;
                break;
            }
        }
    }
    for i in 0..points.len() {
        if protected[i] {
            continue;
        }
        let previous = points[i - 1];
        let next = points[i + 1];
        let block_len = projection.distance_m(previous, next).max(1.0);
        block += block_len;
        if block > 90.0 {
            block = 0.0;
            phase = if phase > 0.5 { 0.0 } else { 1.0 };
        }
        if phase < coverage {
            noise = 0.70 * noise + rng.gauss(0.0, sigma);
            let dx = next[0] - previous[0];
            let dy = next[1] - previous[1];
            let len = (dx * dx + dy * dy).sqrt().max(1e-9);
            let offset = noise.clamp(-0.8, 0.8);
            let xy = projection.xy(points[i]);
            let offset_xy = [
                xy[0] - dy / len * offset,
                xy[1] + dx / len * offset,
            ];
            points[i] = projection.lnglat(offset_xy);
        } else {
            noise = 0.0;
        }
    }
}

/// 保证每个打卡点在输出点列中精确落位（避免共线简化把打卡点删掉）。
fn snap_checkpoints(points: &mut [LngLat], checkpoints: &[LngLat], projection: &Projection) {
    for checkpoint in checkpoints {
        let mut best_index = 0usize;
        let mut best_distance = f64::INFINITY;
        for (index, point) in points.iter().enumerate() {
            let distance = projection.distance_m(*point, *checkpoint);
            if distance < best_distance {
                best_distance = distance;
                best_index = index;
            }
        }
        if best_distance < 15.0 {
            points[best_index] = *checkpoint;
        }
    }
}

/// 直/曲比例：曲率半径 < 50 m 的相邻段计入曲线段。
pub fn measure(points: &[LngLat], projection: &Projection, corners: usize) -> ShapeResult {    let mut total = 0.0;
    let mut curve = 0.0;
    let mut min_radius = f64::INFINITY;
    if points.len() >= 2 {
        for pair in points.windows(2) {
            total += projection.distance_m(pair[0], pair[1]);
        }
    }
    for i in 1..points.len().saturating_sub(1) {
        let a = projection.xy(points[i - 1]);
        let b = projection.xy(points[i]);
        let c = projection.xy(points[i + 1]);
        let radius = circumradius(a, b, c);
        let seg = (projection.distance_m(points[i - 1], points[i])
            + projection.distance_m(points[i], points[i + 1]))
            / 2.0;
        if radius < 50.0 {
            curve += seg;
            min_radius = min_radius.min(radius);
        }
    }
    let curve = curve.min(total);
    let straight = (total - curve).max(0.0);
    ShapeResult {
        points: points.to_vec(),
        total_m: total,
        straight_m: straight,
        curve_m: curve,
        straight_ratio: if total > 0.0 { straight / total } else { 0.0 },
        curve_ratio: if total > 0.0 { curve / total } else { 0.0 },
        corners,
        min_curve_radius_m: if min_radius.is_finite() { min_radius } else { 0.0 },
    }
}

/// 在锚点插入一个回到锚点的椭圆环路，周长尽量接近 `target_len`。
/// 返回插入的环路段（含锚点起止），供路线拼接。
pub fn loop_pad(
    campus: &Campus,
    anchor: LngLat,
    target_len: f64,
    rng: &mut Rng,
) -> Option<Vec<LngLat>> {
    if target_len < 20.0 {
        return None;
    }
    let projection = Projection::for_polygon(&campus.area);
    let anchor_xy = projection.xy(anchor);
    let radius = (target_len / std::f64::consts::TAU).clamp(1.0, 45.0);
    let base_angle = rng.uniform(0.0, std::f64::consts::TAU);
    for attempt in 0..8 {
        let angle = base_angle + attempt as f64 * std::f64::consts::TAU / 8.0;
        // 锚点固定在圆上：圆心 = 锚点 + r·方向；从锚点对应的角度开始绕一圈。
        let center = [
            anchor_xy[0] + radius * angle.cos(),
            anchor_xy[1] + radius * angle.sin(),
        ];
        let steps = ((std::f64::consts::TAU * radius / 2.0).ceil() as usize).clamp(16, 240);
        let mut loop_points = Vec::with_capacity(steps + 1);
        for step in 0..=steps {
            let theta = angle + std::f64::consts::PI
                + step as f64 / steps as f64 * std::f64::consts::TAU;
            let point_xy = [
                center[0] + radius * theta.cos(),
                center[1] + radius * theta.sin(),
            ];
            loop_points.push(projection.lnglat(point_xy));
        }
        if loop_points
            .iter()
            .all(|point| campus.route_point_ok(*point, 0.8))
        {
            let mut length = 0.0;
            for pair in loop_points.windows(2) {
                length += projection.distance_m(pair[0], pair[1]);
            }
            if (target_len - length).abs() < 25.0 {
                return Some(loop_points);
            }
        }
    }
    None
}

fn direction_angle(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    let u = normalize([b[0] - a[0], b[1] - a[1]]);
    let v = normalize([c[0] - b[0], c[1] - b[1]]);
    let dot = (u[0] * v[0] + u[1] * v[1]).clamp(-1.0, 1.0);
    dot.acos()
}

fn normalize(v: [f64; 2]) -> [f64; 2] {
    let len = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if len <= 1e-12 {
        [1.0, 0.0]
    } else {
        [v[0] / len, v[1] / len]
    }
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
}

fn circumradius(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    let ab = distance(a, b);
    let bc = distance(b, c);
    let ca = distance(c, a);
    let area2 = ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs();
    if area2 <= 1e-9 {
        return 1e9;
    }
    (ab * bc * ca) / (2.0 * area2)
}

/// 生成一个椭圆弧 S 弯（可选辅助，用于把直线比例压低到目标区间）。
pub fn s_curve(anchor: LngLat, length: f64, width: f64, projection: &Projection) -> Vec<LngLat> {
    let steps = 24;
    let mut points = Vec::with_capacity(steps + 1);
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let theta = t * std::f64::consts::TAU;
        let along = (t - 0.5) * length;
        let lateral = (1.0 - (theta).cos()) * 0.5 * width;
        let xy_anchor = projection.xy(anchor);
        points.push(projection.lnglat([xy_anchor[0] + along, xy_anchor[1] + lateral]));
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::campus::{Polygon, Road};

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
        campus.roads = vec![Road {
            id: 1,
            name: "路".into(),
            path: vec![[114.297, 30.500], [114.303, 30.500]],
        }];
        campus
    }

    #[test]
    fn fillet_turns_a_right_angle_into_curve() {
        let campus = test_campus();
        let projection = Projection::for_polygon(&campus.area);
        let mut rng = Rng::new(7);
        let points = vec![[114.297, 30.500], [114.300, 30.500], [114.300, 30.503]];
        let (filleted, curve, corners) = fillet(&points, &projection, &mut rng);
        assert_eq!(corners, 1);
        assert!(curve > 1.0, "curve={curve}");
        assert!(filleted.len() > 3);
        assert!(filleted.iter().all(|p| p[0].is_finite()));
    }

    #[test]
    fn measure_reports_straight_for_collinear_points() {
        let campus = test_campus();
        let projection = Projection::for_polygon(&campus.area);
        let points = vec![[114.297, 30.500], [114.300, 30.500], [114.303, 30.500]];
        let stats = measure(&points, &projection, 0);
        assert!((stats.straight_ratio - 1.0).abs() < 0.05, "{stats:?}");
        assert!(stats.curve_ratio < 0.05);
    }

    #[test]
    fn jitter_keeps_endpoints_and_stays_inside_area() {
        let campus = test_campus();
        let projection = Projection::for_polygon(&campus.area);
        let mut rng = Rng::new(11);
        let mut points = resample(
            &[
                [114.297, 30.500],
                [114.299, 30.500],
                [114.301, 30.500],
                [114.303, 30.500],
            ],
            &projection,
            5.0,
        );
        let original_first = points[0];
        let original_last = *points.last().unwrap();
        jitter(&mut points, &projection, &[], &mut rng, 1.0, 0.5);
        assert_eq!(points[0], original_first);
        assert_eq!(*points.last().unwrap(), original_last);
        assert!(points.iter().all(|p| campus.route_point_ok(*p, 0.5)));
        let stats = measure(&points, &projection, 0);
        assert!(stats.curve_ratio > 0.2, "{stats:?}");
    }

    #[test]
    fn loop_pad_returns_closed_loop_inside_area() {
        let campus = test_campus();
        let mut rng = Rng::new(3);
        let loop_points = loop_pad(&campus, [114.300, 30.500], 150.0, &mut rng).unwrap();
        assert!(loop_points.len() > 10);
        assert!(loop_points.iter().all(|p| campus.route_point_ok(*p, 0.5)));
        let projection = Projection::for_polygon(&campus.area);
        let length: f64 = loop_points
            .windows(2)
            .map(|pair| projection.distance_m(pair[0], pair[1]))
            .sum();
        assert!((length - 150.0).abs() < 30.0, "length={length}");
    }
}
