//! 校园几何工具：WGS84 → BD09、局部米制投影、多边形包含/自交检测。
//!
//! 对外坐标统一为 `[经度, 纬度]`；拓扑与长度计算先投影到米制平面。
//! 投影以校区质心为原点、等距圆柱近似，经度按 `cos(lat0)` 缩放。

use super::{LngLat, Polygon};

/// 纬度方向每度米数（与现有 `track::geom` 保持一致）。
pub const M_PER_DEG_LAT: f64 = 111_132.0;
/// 赤道处经度每度米数。
pub const M_PER_DEG_LNG_EQ: f64 = 111_320.0;

/// 点/边判定的米制容差。
const EPS_M: f64 = 1e-6;
/// 叉积判定容差（米²）。
const EPS_CROSS: f64 = 1e-7;

/// 局部等距圆柱投影。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projection {
    pub lat0: f64,
    pub lng0: f64,
    pub m_per_deg_lat: f64,
    pub m_per_deg_lng: f64,
}

impl Projection {
    pub fn at(lat0: f64, lng0: f64) -> Self {
        let m_per_deg_lng = (M_PER_DEG_LNG_EQ * lat0.to_radians().cos()).abs().max(1.0);
        Self {
            lat0,
            lng0,
            m_per_deg_lat: M_PER_DEG_LAT,
            m_per_deg_lng,
        }
    }

    /// 以多边形外环顶点均值为原点（校园尺度下足够稳定）。
    pub fn for_polygon(poly: &Polygon) -> Self {
        if poly.outer.is_empty() {
            return Self::at(0.0, 0.0);
        }
        let n = poly.outer.len() as f64;
        let lat0 = poly.outer.iter().map(|p| p[1]).sum::<f64>() / n;
        let lng0 = poly.outer.iter().map(|p| p[0]).sum::<f64>() / n;
        Self::at(lat0, lng0)
    }

    pub fn xy(&self, p: LngLat) -> [f64; 2] {
        [
            (p[0] - self.lng0) * self.m_per_deg_lng,
            (p[1] - self.lat0) * self.m_per_deg_lat,
        ]
    }

    pub fn lnglat(&self, xy: [f64; 2]) -> LngLat {
        [
            self.lng0 + xy[0] / self.m_per_deg_lng,
            self.lat0 + xy[1] / self.m_per_deg_lat,
        ]
    }

    pub fn distance_m(&self, a: LngLat, b: LngLat) -> f64 {
        let (ax, ay) = (a[0] - b[0], a[1] - b[1]);
        ((ax * self.m_per_deg_lng).powi(2) + (ay * self.m_per_deg_lat).powi(2)).sqrt()
    }
}

/// 米制平面包围盒。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: [f64; 2],
    pub max: [f64; 2],
}

impl Default for Bounds {
    fn default() -> Self {
        Self::empty()
    }
}

impl Bounds {
    pub fn empty() -> Self {
        Self {
            min: [f64::INFINITY; 2],
            max: [f64::NEG_INFINITY; 2],
        }
    }

    pub fn is_empty(&self) -> bool {
        !(self.min[0].is_finite()
            && self.min[1].is_finite()
            && self.max[0].is_finite()
            && self.max[1].is_finite()
            && self.min[0] <= self.max[0]
            && self.min[1] <= self.max[1])
    }

    pub fn width(&self) -> f64 {
        (self.max[0] - self.min[0]).max(0.0)
    }

    pub fn height(&self) -> f64 {
        (self.max[1] - self.min[1]).max(0.0)
    }

    pub fn center(&self) -> [f64; 2] {
        [
            (self.min[0] + self.max[0]) / 2.0,
            (self.min[1] + self.max[1]) / 2.0,
        ]
    }

    fn push(&mut self, p: [f64; 2]) {
        self.min[0] = self.min[0].min(p[0]);
        self.min[1] = self.min[1].min(p[1]);
        self.max[0] = self.max[0].max(p[0]);
        self.max[1] = self.max[1].max(p[1]);
    }
}

pub fn project_ring(ring: &[LngLat], proj: &Projection) -> Vec<[f64; 2]> {
    ring.iter().map(|p| proj.xy(*p)).collect()
}

pub fn polygon_bounds(poly: &Polygon, proj: &Projection) -> Bounds {
    let mut bounds = Bounds::empty();
    for ring in poly.rings() {
        for p in ring {
            bounds.push(proj.xy(*p));
        }
    }
    bounds
}

/// 有向面积（米²，逆时针为正）。
pub fn ring_signed_area_xy(points: &[[f64; 2]]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    for i in 0..points.len() {
        let a = points[i];
        let b = points[(i + 1) % points.len()];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum / 2.0
}

pub fn ring_area_m2(ring: &[LngLat], proj: &Projection) -> f64 {
    ring_signed_area_xy(&project_ring(ring, proj)).abs()
}

/// 外环面积减去内环面积（不小于 0）。
pub fn polygon_area_m2(poly: &Polygon, proj: &Projection) -> f64 {
    let mut area = ring_area_m2(&poly.outer, proj);
    for hole in &poly.holes {
        area -= ring_area_m2(hole, proj);
    }
    area.max(0.0)
}

pub fn ring_perimeter_m(ring: &[LngLat], proj: &Projection) -> f64 {
    if ring.len() < 2 {
        return 0.0;
    }
    let mut len = 0.0;
    for i in 0..ring.len() {
        len += proj.distance_m(ring[i], ring[(i + 1) % ring.len()]);
    }
    len
}

pub fn polyline_length_m(path: &[LngLat], proj: &Projection) -> f64 {
    path.windows(2).map(|w| proj.distance_m(w[0], w[1])).sum()
}

pub fn midpoint(a: LngLat, b: LngLat) -> LngLat {
    [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]
}

fn orient(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn on_segment_xy(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> bool {
    let cross = orient(a, b, p);
    if cross.abs() > EPS_CROSS {
        return false;
    }
    p[0] >= a[0].min(b[0]) - EPS_M
        && p[0] <= a[0].max(b[0]) + EPS_M
        && p[1] >= a[1].min(b[1]) - EPS_M
        && p[1] <= a[1].max(b[1]) + EPS_M
}

/// 点是否落在线段上（含端点）。
/// 点到线段的距离（米）。
pub fn distance_to_segment(p: LngLat, a: LngLat, b: LngLat, proj: &Projection) -> f64 {
    let pp = proj.xy(p);
    let pa = proj.xy(a);
    let pb = proj.xy(b);
    let dx = pb[0] - pa[0];
    let dy = pb[1] - pa[1];
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 1e-12 {
        0.0
    } else {
        (((pp[0] - pa[0]) * dx + (pp[1] - pa[1]) * dy) / len2).clamp(0.0, 1.0)
    };
    let cx = pa[0] + t * dx;
    let cy = pa[1] + t * dy;
    ((pp[0] - cx).powi(2) + (pp[1] - cy).powi(2)).sqrt()
}

/// 点到折线（路网）的最近距离（米）。
pub fn distance_to_polyline(p: LngLat, path: &[LngLat], proj: &Projection) -> f64 {
    if path.is_empty() {
        return f64::INFINITY;
    }
    if path.len() == 1 {
        let pp = proj.xy(p);
        let a = proj.xy(path[0]);
        return ((pp[0] - a[0]).powi(2) + (pp[1] - a[1]).powi(2)).sqrt();
    }
    path.windows(2)
        .map(|pair| distance_to_segment(p, pair[0], pair[1], proj))
        .fold(f64::INFINITY, f64::min)
}

pub fn point_on_segment(p: LngLat, a: LngLat, b: LngLat, proj: &Projection) -> bool {
    on_segment_xy(proj.xy(a), proj.xy(b), proj.xy(p))
}

/// 点在环内（边界视为在内），环按首尾相连处理。
pub fn point_in_ring_xy(p: [f64; 2], ring: &[[f64; 2]]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    for i in 0..ring.len() {
        if on_segment_xy(ring[i], ring[(i + 1) % ring.len()], p) {
            return true;
        }
    }
    let mut inside = false;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let x = a[0] + (p[1] - a[1]) * (b[0] - a[0]) / (b[1] - a[1]);
            if p[0] < x {
                inside = !inside;
            }
        }
    }
    inside
}

pub fn point_in_ring(p: LngLat, ring: &[LngLat], proj: &Projection) -> bool {
    point_in_ring_xy(proj.xy(p), &project_ring(ring, proj))
}

/// 点在多边形内（允许区域边界算内；落在内环洞内算外）。
pub fn point_in_polygon(p: LngLat, poly: &Polygon, proj: &Projection) -> bool {
    if !point_in_ring(p, &poly.outer, proj) {
        return false;
    }
    for hole in &poly.holes {
        if point_in_ring(p, hole, proj) {
            return false;
        }
    }
    true
}

/// 点到多边形边界（外环+内环）的最短距离（米）。
pub fn distance_to_polygon_boundary(p: LngLat, poly: &Polygon, proj: &Projection) -> f64 {
    let mut best = f64::INFINITY;
    for ring in poly.rings() {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..ring.len() {
            let a = proj.xy(ring[i]);
            let b = proj.xy(ring[(i + 1) % ring.len()]);
            best = best.min(point_segment_distance(proj.xy(p), a, b));
        }
    }
    best
}

/// 点到线段距离（米制平面）。
pub fn point_segment_distance(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let len2 = dx * dx + dy * dy;
    if len2 <= 1e-12 {
        return ((p[0] - a[0]).powi(2) + (p[1] - a[1]).powi(2)).sqrt();
    }
    let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0);
    let q = [a[0] + dx * t, a[1] + dy * t];
    ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2)).sqrt()
}

fn segments_intersect_xy(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let o1 = orient(a, b, c);
    let o2 = orient(a, b, d);
    let o3 = orient(c, d, a);
    let o4 = orient(c, d, b);
    let opposite = |x: f64, y: f64| (x > EPS_CROSS && y < -EPS_CROSS) || (x < -EPS_CROSS && y > EPS_CROSS);
    if opposite(o1, o2) && opposite(o3, o4) {
        return true;
    }
    (o1.abs() <= EPS_CROSS && on_segment_xy(a, b, c))
        || (o2.abs() <= EPS_CROSS && on_segment_xy(a, b, d))
        || (o3.abs() <= EPS_CROSS && on_segment_xy(c, d, a))
        || (o4.abs() <= EPS_CROSS && on_segment_xy(c, d, b))
}

/// 两线段是否严格相交（交点在两段内部，端点接触不算）。
pub fn segments_properly_cross_xy(
    a: [f64; 2],
    b: [f64; 2],
    c: [f64; 2],
    d: [f64; 2],
) -> bool {
    let o1 = orient(a, b, c);
    let o2 = orient(a, b, d);
    let o3 = orient(c, d, a);
    let o4 = orient(c, d, b);
    let opposite = |x: f64, y: f64| (x > EPS_CROSS && y < -EPS_CROSS) || (x < -EPS_CROSS && y > EPS_CROSS);
    opposite(o1, o2) && opposite(o3, o4)
}

/// 环的自交点：返回首个相交的线段对 `(i, j)`（线段 i 为 i→i+1）。
pub fn ring_self_intersections(ring: &[LngLat], proj: &Projection) -> Vec<(usize, usize)> {
    let points = project_ring(ring, proj);
    let n = points.len();
    let mut hits = Vec::new();
    if n < 4 {
        return hits;
    }
    for i in 0..n {
        let a = points[i];
        let b = points[(i + 1) % n];
        for j in (i + 1)..n {
            // 相邻线段共享端点，不算自交
            if j == i + 1 || (i == 0 && j == n - 1) {
                continue;
            }
            let c = points[j];
            let d = points[(j + 1) % n];
            if segments_intersect_xy(a, b, c, d) {
                hits.push((i, j));
                if hits.len() >= 8 {
                    return hits;
                }
            }
        }
    }
    hits
}

/// 多边形所有环的自交点，返回 `(环序号, 线段 i, 线段 j)`；环 0 为外环。
pub fn polygon_self_intersections(poly: &Polygon, proj: &Projection) -> Vec<(usize, usize, usize)> {
    let mut hits = Vec::new();
    for (ring_index, ring) in poly.rings().enumerate() {
        for (i, j) in ring_self_intersections(ring, proj) {
            hits.push((ring_index, i, j));
        }
    }
    hits
}

/// 两个环的边是否有严格交叉；返回首个 `(i, j)`。
pub fn rings_properly_cross(a: &[LngLat], b: &[LngLat], proj: &Projection) -> Option<(usize, usize)> {
    if a.len() < 2 || b.len() < 2 {
        return None;
    }
    let pa = project_ring(a, proj);
    let pb = project_ring(b, proj);
    for i in 0..pa.len() {
        let a1 = pa[i];
        let a2 = pa[(i + 1) % pa.len()];
        for j in 0..pb.len() {
            let b1 = pb[j];
            let b2 = pb[(j + 1) % pb.len()];
            if segments_properly_cross_xy(a1, a2, b1, b2) {
                return Some((i, j));
            }
        }
    }
    None
}

// ── 坐标系转换 ─────────────────────────────────────────────

const X_PI: f64 = std::f64::consts::PI * 3000.0 / 180.0;
const GCJ_A: f64 = 6_378_245.0;
const GCJ_EE: f64 = 0.006_693_421_622_965_943;
const PI: f64 = std::f64::consts::PI;

fn out_of_china(lng: f64, lat: f64) -> bool {
    !(73.66..=135.05).contains(&lng) || !(3.86..=53.55).contains(&lat)
}

fn transform_lat(x: f64, y: f64) -> f64 {
    let mut ret = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.sqrt();
    ret += (20.0 * (6.0 * x * PI).sin() + 20.0 * (2.0 * x * PI).sin()) * 2.0 / 3.0;
    ret += (20.0 * (y * PI).sin() + 40.0 * (y / 3.0 * PI).sin()) * 2.0 / 3.0;
    ret += (160.0 * (y / 12.0 * PI).sin() + 320.0 * (y * PI / 30.0).sin()) * 2.0 / 3.0;
    ret
}

fn transform_lng(x: f64, y: f64) -> f64 {
    let mut ret = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.sqrt();
    ret += (20.0 * (6.0 * x * PI).sin() + 20.0 * (2.0 * x * PI).sin()) * 2.0 / 3.0;
    ret += (20.0 * (x * PI).sin() + 40.0 * (x / 3.0 * PI).sin()) * 2.0 / 3.0;
    ret += (150.0 * (x / 12.0 * PI).sin() + 300.0 * (x / 30.0 * PI).sin()) * 2.0 / 3.0;
    ret
}

/// WGS84 → GCJ02，返回 `(经度, 纬度)`。
pub fn wgs84_to_gcj02(lng: f64, lat: f64) -> (f64, f64) {
    if out_of_china(lng, lat) {
        return (lng, lat);
    }
    let dlat = transform_lat(lng - 105.0, lat - 35.0);
    let dlng = transform_lng(lng - 105.0, lat - 35.0);
    let radlat = lat / 180.0 * PI;
    let mut magic = radlat.sin();
    magic = 1.0 - GCJ_EE * magic * magic;
    let sqrtmagic = magic.sqrt();
    let dlat = (dlat * 180.0) / ((GCJ_A * (1.0 - GCJ_EE)) / (magic * sqrtmagic) * PI);
    let dlng = (dlng * 180.0) / (GCJ_A / sqrtmagic * radlat.cos() * PI);
    (lng + dlng, lat + dlat)
}

/// GCJ02 → BD09，返回 `(经度, 纬度)`。
pub fn gcj02_to_bd09(lng: f64, lat: f64) -> (f64, f64) {
    let z = (lng * lng + lat * lat).sqrt() + 0.00002 * (lat * X_PI).sin();
    let theta = lat.atan2(lng) + 0.000003 * (lng * X_PI).cos();
    (z * theta.cos() + 0.0065, z * theta.sin() + 0.006)
}

/// BD09 → GCJ02，返回 `(经度, 纬度)`（校验/测试用，与 `track::wire` 的 BD→GCJ 互逆）。
pub fn bd09_to_gcj02(lng: f64, lat: f64) -> (f64, f64) {
    let x = lng - 0.0065;
    let y = lat - 0.006;
    let z = (x * x + y * y).sqrt() - 0.00002 * (y * X_PI).sin();
    let theta = y.atan2(x) - 0.000003 * (x * X_PI).cos();
    (z * theta.cos(), z * theta.sin())
}

/// WGS84 → BD09，返回 `[经度, 纬度]`。
pub fn wgs84_to_bd09(lng: f64, lat: f64) -> LngLat {
    let (gcj_lng, gcj_lat) = wgs84_to_gcj02(lng, lat);
    let (bd_lng, bd_lat) = gcj02_to_bd09(gcj_lng, gcj_lat);
    [bd_lng, bd_lat]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square_poly(cx: f64, cy: f64, half: f64) -> Polygon {
        let mut outer = vec![
            [cx - half, cy - half],
            [cx + half, cy - half],
            [cx + half, cy + half],
            [cx - half, cy + half],
        ];
        outer.reverse(); // 逆时针
        Polygon::new(outer)
    }

    #[test]
    fn wgs84_to_gcj02_matches_reference_vector() {
        let (lng, lat) = wgs84_to_gcj02(116.404, 39.915);
        assert!((lng - 116.410_244_499_169_38).abs() < 1e-9, "lng={lng}");
        assert!((lat - 39.916_404_281_501_64).abs() < 1e-9, "lat={lat}");
    }

    #[test]
    fn bd09_conversion_round_trips_against_wire_reference() {
        let (gcj_lng, gcj_lat) = wgs84_to_gcj02(116.397128, 39.916527);
        let (bd_lng, bd_lat) = gcj02_to_bd09(gcj_lng, gcj_lat);
        // 与现有 wire.rs 的 BD→GCJ 转换一致（两套公式互逆误差 < 1e-6 度）
        let (wire_lat, wire_lng) = crate::track::wire::bd09_to_gcj02(bd_lat, bd_lng);
        assert!((wire_lng - gcj_lng).abs() < 1e-6, "lng={wire_lng} vs {gcj_lng}");
        assert!((wire_lat - gcj_lat).abs() < 1e-6, "lat={wire_lat} vs {gcj_lat}");
        // 百度坐标相对 WGS84 有几百米偏移
        assert!((bd_lng - 116.397128).abs() > 0.003);
        assert!((bd_lat - 39.916527).abs() > 0.003);
    }

    #[test]
    fn local_projection_has_expected_scale_and_round_trip() {
        let proj = Projection::at(30.5, 114.3);
        let a = [114.3, 30.5];
        assert_eq!(proj.xy(a), [0.0, 0.0]);
        let d = proj.distance_m(a, [114.31, 30.5]);
        assert!((d - 959.0).abs() < 5.0, "d={d}");
        let back = proj.lnglat(proj.xy([114.301, 30.501]));
        assert!((back[0] - 114.301).abs() < 1e-12);
        assert!((back[1] - 30.501).abs() < 1e-12);
    }

    #[test]
    fn point_in_polygon_handles_boundary_and_holes() {
        let poly = Polygon {
            outer: vec![
                [114.298, 30.498],
                [114.302, 30.498],
                [114.302, 30.502],
                [114.298, 30.502],
            ],
            holes: vec![vec![
                [114.2995, 30.4995],
                [114.3005, 30.4995],
                [114.3005, 30.5005],
                [114.2995, 30.5005],
            ]],
        };
        let proj = Projection::for_polygon(&poly);
        assert!(point_in_polygon([114.298, 30.500], &poly, &proj)); // 边界算内
        assert!(point_in_polygon([114.2985, 30.4985], &poly, &proj));
        assert!(!point_in_polygon([114.300, 30.500], &poly, &proj)); // 洞内
        assert!(!point_in_polygon([114.303, 30.500], &poly, &proj));
    }

    #[test]
    fn self_intersection_detects_bow_tie_but_not_square() {
        let proj = Projection::at(30.5, 114.3);
        let square = vec![
            [114.298, 30.498],
            [114.302, 30.498],
            [114.302, 30.502],
            [114.298, 30.502],
        ];
        assert!(ring_self_intersections(&square, &proj).is_empty());
        let bow_tie = vec![
            [114.298, 30.498],
            [114.302, 30.502],
            [114.302, 30.498],
            [114.298, 30.502],
        ];
        assert!(!ring_self_intersections(&bow_tie, &proj).is_empty());
    }

    #[test]
    fn proper_crossing_ignores_touching_endpoints() {
        let proj = Projection::at(30.5, 114.3);
        let a = [[114.300, 30.500], [114.301, 30.500]];
        let b = [[114.300, 30.500], [114.3005, 30.501]]; // 共享端点
        let _ = proj;
        assert!(!segments_properly_cross_xy(a[0], a[1], b[0], b[1]));
        let c = [[114.3005, 30.4995], [114.3005, 30.5005]];
        assert!(segments_properly_cross_xy(a[0], a[1], c[0], c[1]));
    }

    #[test]
    fn polygon_area_and_length_are_metric() {
        let poly = square_poly(114.3, 30.5, 0.001); // 约 192m × 222m
        let proj = Projection::for_polygon(&poly);
        let area = polygon_area_m2(&poly, &proj);
        assert!((area - 42_500.0).abs() < 3000.0, "area={area}");
        let perimeter = ring_perimeter_m(&poly.outer, &proj);
        assert!((perimeter - 828.0).abs() < 40.0, "perimeter={perimeter}");
        assert!(polygon_bounds(&poly, &proj).width() > 180.0);
    }
}
