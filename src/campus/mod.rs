//! 校园地图数据模型（BD09）。
//!
//! 坐标约定：持久化与内存中的坐标均为百度 BD09，元素顺序 `[经度, 纬度]`。
//! 外部导入的 WGS84 GeoJSON 在 `import` 层依次转换为 GCJ02、BD09；
//! 提交服务端时仍由 `track::wire::bd09_to_gcj02` 转回 GCJ02。

pub mod geo;
pub mod graph;
pub mod import;
pub mod store;

use serde::{Deserialize, Serialize};

/// campus.json 当前结构版本。
pub const SCHEMA_VERSION: u32 = 1;
/// 校园允许区域面积下限（米²）。
pub const MIN_CAMPUS_AREA_M2: f64 = 10_000.0;
/// 默认净空下限（米）。
pub const DEFAULT_CLEARANCE_MIN_M: f64 = 1.5;
/// 默认净空上限（米）。
pub const DEFAULT_CLEARANCE_MAX_M: f64 = 3.0;
/// 默认基准海拔（米）。
pub const DEFAULT_BASE_ALTITUDE_M: f64 = 82.0;
/// 允许区域默认内缩（米）。
pub const DEFAULT_ALLOW_SHRINK_M: f64 = 10.0;
/// 保留的历史版本快照份数。
pub const SNAPSHOT_KEEP: usize = 12;

/// BD09 坐标 `[经度, 纬度]`。
pub type LngLat = [f64; 2];

/// 一个多边形：外环 + 可选内环（洞）。环内不重复首点。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Polygon {
    pub outer: Vec<LngLat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub holes: Vec<Vec<LngLat>>,
}

impl Polygon {
    pub fn new(outer: Vec<LngLat>) -> Self {
        Self {
            outer,
            holes: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.outer.len() < 3
    }

    /// 外环 + 全部内环。
    pub fn rings(&self) -> impl Iterator<Item = &Vec<LngLat>> {
        std::iter::once(&self.outer).chain(self.holes.iter())
    }

    /// 外环 + 内环数量。
    pub fn ring_count(&self) -> usize {
        1 + self.holes.len()
    }
}

/// 禁行区（建筑 / 水面 / 施工）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct BlockedArea {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub polygon: Polygon,
}

/// 操场区域（绕圈跑范围；可包含田径场内场和外圈）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlaygroundArea {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub polygon: Polygon,
}

/// 可行走走廊（路网缺失处手工补充）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Corridor {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub polygon: Polygon,
}

/// POI 类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoiKind {
    Canteen,
    Dorm,
    Gate,
}

impl PoiKind {
    pub const ALL: [PoiKind; 3] = [PoiKind::Canteen, PoiKind::Dorm, PoiKind::Gate];

    pub fn key(self) -> &'static str {
        match self {
            PoiKind::Canteen => "canteen",
            PoiKind::Dorm => "dorm",
            PoiKind::Gate => "gate",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PoiKind::Canteen => "食堂",
            PoiKind::Dorm => "宿舍",
            PoiKind::Gate => "校门",
        }
    }

    pub fn from_kind_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "poi_canteen" | "canteen" | "食堂" => Some(PoiKind::Canteen),
            "poi_dorm" | "dorm" | "宿舍" => Some(PoiKind::Dorm),
            "poi_gate" | "gate" | "校门" => Some(PoiKind::Gate),
            _ => None,
        }
    }
}

/// 终点 POI，坐标 BD09。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Poi {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub pos: LngLat,
}

/// 三类 POI 集合。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pois {
    #[serde(default)]
    pub canteen: Vec<Poi>,
    #[serde(default)]
    pub dorm: Vec<Poi>,
    #[serde(default)]
    pub gate: Vec<Poi>,
}

impl Pois {
    pub fn get(&self, kind: PoiKind) -> &Vec<Poi> {
        match kind {
            PoiKind::Canteen => &self.canteen,
            PoiKind::Dorm => &self.dorm,
            PoiKind::Gate => &self.gate,
        }
    }

    pub fn get_mut(&mut self, kind: PoiKind) -> &mut Vec<Poi> {
        match kind {
            PoiKind::Canteen => &mut self.canteen,
            PoiKind::Dorm => &mut self.dorm,
            PoiKind::Gate => &mut self.gate,
        }
    }

    /// 按 食堂 → 宿舍 → 校门 顺序遍历。
    pub fn all(&self) -> impl Iterator<Item = (PoiKind, &Poi)> {
        self.canteen
            .iter()
            .map(|p| (PoiKind::Canteen, p))
            .chain(self.dorm.iter().map(|p| (PoiKind::Dorm, p)))
            .chain(self.gate.iter().map(|p| (PoiKind::Gate, p)))
    }

    pub fn total(&self) -> usize {
        self.canteen.len() + self.dorm.len() + self.gate.len()
    }
}

/// 路网折线（Polyline），坐标为 BD09。
/// 判断这个禁行区是不是「围墙/护栏」而不是建筑。
///
/// campus.json 里操场四周的围墙是 5–20m 的细长条、名字带「墙/牆」；
/// 它们只需要 1m 余量，不该按建筑的 8m 净空处理——否则整个操场内部
/// 都会被判成「贴着墙」，跑道绕圈根本画不出来。
pub fn is_wall_name(name: &str) -> bool {
    name.contains('墙') || name.contains('牆')
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Road {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    pub path: Vec<LngLat>,
}

/// 校园地图完整数据，对应 `campus.json`。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Campus {
    pub version: u32,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub area: Polygon,
    #[serde(default)]
    pub blocked: Vec<BlockedArea>,
    #[serde(default)]
    pub corridors: Vec<Corridor>,
    /// QGIS 绘制的操场区域；有数据时优先按多边形判断是否绕圈。
    #[serde(default)]
    pub playgrounds: Vec<PlaygroundArea>,
    /// QGIS 绘制的广场区域；同样作为可绕圈/可跑的开阔区域。
    #[serde(default)]
    pub plazas: Vec<PlaygroundArea>,
    #[serde(default)]
    pub pois: Pois,
    #[serde(default)]
    pub roads: Vec<Road>,
    pub clearance_min: f64,
    pub clearance_max: f64,
    pub base_altitude_m: f64,
    pub allow_shrink_m: f64,
}

impl Campus {
    pub fn new(area: Polygon, now_ms: i64) -> Self {
        Self {
            version: SCHEMA_VERSION,
            created_ms: now_ms,
            updated_ms: now_ms,
            area,
            blocked: Vec::new(),
            corridors: Vec::new(),
            playgrounds: Vec::new(),
            plazas: Vec::new(),
            pois: Pois::default(),
            roads: Vec::new(),
            clearance_min: DEFAULT_CLEARANCE_MIN_M,
            clearance_max: DEFAULT_CLEARANCE_MAX_M,
            base_altitude_m: DEFAULT_BASE_ALTITUDE_M,
            allow_shrink_m: DEFAULT_ALLOW_SHRINK_M,
        }
    }

    pub fn poi_total(&self) -> usize {
        self.pois.total()
    }

    /// POI 合规校验：坐标在中国 BD09 范围、位于允许区域内、且不在禁行区内。
    pub fn validate_poi_position(&self, pos: LngLat) -> Result<(), String> {
        if !pos[0].is_finite() || !pos[1].is_finite() {
            return Err("坐标不是有限数值".into());
        }
        if !(73.0..=135.5).contains(&pos[0]) || !(3.0..=53.6).contains(&pos[1]) {
            return Err("坐标不在中国范围（BD09 经度/纬度）".into());
        }
        let projection = geo::Projection::for_polygon(&self.area);
        if !geo::point_in_polygon(pos, &self.area, &projection) {
            return Err("坐标位于校园允许区域外".into());
        }
        if let Some(blocked) = self
            .blocked
            .iter()
            .find(|blocked| geo::point_in_polygon(pos, &blocked.polygon, &projection))
        {
            let name = if blocked.name.trim().is_empty() {
                format!("#{}", blocked.id)
            } else {
                blocked.name.trim().to_string()
            };
            return Err(format!("坐标位于禁行区「{name}」内"));
        }
        Ok(())
    }

    /// 路线点是否满足允许区域内缩与禁行净空（不检查是否位于禁行区内部）。
    pub fn route_point_ok(&self, pos: LngLat, clearance_m: f64) -> bool {
        if !pos[0].is_finite() || !pos[1].is_finite() {
            return false;
        }
        let projection = geo::Projection::for_polygon(&self.area);
        if !geo::point_in_polygon(pos, &self.area, &projection) {
            return false;
        }
        // 走廊/门洞：标记过的通道直接放行。
        // 原来这里完全没读 corridors，用户在地图上画的通道从来没生效过，
        // 操场被建筑围成孤岛、A* 永远出不去。容差 2m 是因为 corridors 常常只有 1m 宽，
        // 而 A* 网格是 2m，采样点会落在通道外面一点点。
        let in_corridor = self.corridors.iter().any(|corridor| {
            geo::point_in_polygon(pos, &corridor.polygon, &projection)
                || geo::distance_to_polygon_boundary(pos, &corridor.polygon, &projection) <= 2.0
        });
        if in_corridor {
            return true;
        }
        // 路网：沿着道路 4m 内放行。真人就是在这些路上跑的，
        // 原来 roads 只拿来画图、完全不参与通行判定，所以「有路」也走不过去。
        let on_road = self.roads.iter().any(|road| {
            geo::distance_to_polyline(pos, &road.path, &projection) <= 4.0
        });
        if on_road {
            return true;
        }
        if self.allow_shrink_m > 0.0
            && geo::distance_to_polygon_boundary(pos, &self.area, &projection) < self.allow_shrink_m
        {
            return false;
        }
        for blocked in &self.blocked {
            if geo::point_in_polygon(pos, &blocked.polygon, &projection) {
                return false;
            }
            // 围墙/护栏只要 1m 余量：它们是操场边界的细长条（5–20m 厚），
            // 按建筑的 8m 净空算，整个操场内部都会被判成"贴着墙"，跑道根本画不出来。
            let required = if is_wall_name(&blocked.name) {
                clearance_m.min(1.0)
            } else {
                clearance_m
            };
            if geo::distance_to_polygon_boundary(pos, &blocked.polygon, &projection) < required {
                return false;
            }
        }
        true
    }

    /// 新增 POI：先做区域/禁行校验，空名称自动生成。
    pub fn add_poi(&mut self, kind: PoiKind, name: String, pos: LngLat) -> Result<u64, String> {
        self.validate_poi_position(pos)?;
        let list = self.pois.get_mut(kind);
        let id = list.iter().map(|poi| poi.id).max().unwrap_or(0) + 1;
        let name = if name.trim().is_empty() {
            format!("{} POI #{}", kind.label(), id)
        } else {
            name.trim().to_string()
        };
        list.push(Poi { id, name, pos });
        Ok(id)
    }

    /// 修改 POI 坐标/名称：坐标非法时不动原数据。
    pub fn update_poi(
        &mut self,
        kind: PoiKind,
        index: usize,
        name: String,
        pos: LngLat,
    ) -> Result<(), String> {
        self.validate_poi_position(pos)?;
        let list = self.pois.get_mut(kind);
        let Some(poi) = list.get_mut(index) else {
            return Err("POI 不存在（可能已被删除）".into());
        };
        poi.pos = pos;
        if !name.trim().is_empty() {
            poi.name = name.trim().to_string();
        }
        Ok(())
    }

    /// 删除 POI；返回被删除项。
    pub fn remove_poi(&mut self, kind: PoiKind, index: usize) -> Result<Poi, String> {
        let list = self.pois.get_mut(kind);
        if index >= list.len() {
            return Err("POI 不存在（可能已被删除）".into());
        }
        Ok(list.remove(index))
    }

    /// 点位是否落在任一操场区域内。没有操场数据时返回 false，由调用方决定
    /// 是否使用旧的“点位集中度”兜底判断。
    pub fn in_playground(&self, point: LngLat) -> bool {
        let projection = geo::Projection::for_polygon(&self.area);
        self.playgrounds
            .iter()
            .any(|playground| geo::point_in_polygon(point, &playground.polygon, &projection))
    }

    /// 点位是否在任一可跑开阔区域（操场或广场）内。
    pub fn in_run_area(&self, point: LngLat) -> bool {
        let projection = geo::Projection::for_polygon(&self.area);
        self.playgrounds
            .iter()
            .chain(self.plazas.iter())
            .any(|area| geo::point_in_polygon(point, &area.polygon, &projection))
    }

    /// 所有任务点是否落在同一个操场内。
    pub fn tasks_in_one_playground(&self, tasks: &[LngLat]) -> bool {
        if self.playgrounds.is_empty() {
            return false;
        }
        let projection = geo::Projection::for_polygon(&self.area);
        self.playgrounds.iter().any(|playground| {
            tasks
                .iter()
                .all(|point| geo::point_in_polygon(*point, &playground.polygon, &projection))
        })
    }

    /// 点位是否在广场内（广场是可穿行开阔区，不触发绕圈）。
    pub fn in_plaza(&self, point: LngLat) -> bool {
        let projection = geo::Projection::for_polygon(&self.area);
        self.plazas
            .iter()
            .any(|plaza| geo::point_in_polygon(point, &plaza.polygon, &projection))
    }

    /// 所有任务点是否落在同一个可跑区域内（避免跨操场/广场强行绕圈）。
    pub fn tasks_in_one_run_area(&self, tasks: &[LngLat]) -> bool {
        if self.playgrounds.is_empty() && self.plazas.is_empty() {
            return false;
        }
        let projection = geo::Projection::for_polygon(&self.area);
        self.playgrounds
            .iter()
            .chain(self.plazas.iter())
            .any(|area| {
                tasks
                    .iter()
                    .all(|point| geo::point_in_polygon(*point, &area.polygon, &projection))
            })
    }

    /// 统计量（面积、周长、POI/路网数量等），用于界面与导入报告。
    pub fn stats(&self) -> CampusStats {
        let proj = geo::Projection::for_polygon(&self.area);
        let area_m2 = geo::polygon_area_m2(&self.area, &proj);
        let blocked_area_m2 = self
            .blocked
            .iter()
            .map(|b| geo::polygon_area_m2(&b.polygon, &proj))
            .sum::<f64>();
        let road_length_m = self
            .roads
            .iter()
            .map(|r| geo::polyline_length_m(&r.path, &proj))
            .sum::<f64>();
        CampusStats {
            area_m2,
            allowed_area_m2: (area_m2 - blocked_area_m2).max(0.0),
            area_vertices: self.area.outer.len(),
            area_perimeter_m: geo::ring_perimeter_m(&self.area.outer, &proj),
            blocked_count: self.blocked.len(),
            blocked_area_m2,
            corridor_count: self.corridors.len(),
            playground_count: self.playgrounds.len(),
            plaza_count: self.plazas.len(),
            canteen: self.pois.canteen.len(),
            dorm: self.pois.dorm.len(),
            gate: self.pois.gate.len(),
            road_count: self.roads.len(),
            road_length_m,
        }
    }

    /// 载入/保存前的最小结构校验。
    pub fn validate_basic(&self) -> Result<(), String> {
        if self.version != SCHEMA_VERSION {
            return Err(format!(
                "campus.json 版本 {} 不受支持（当前版本 {}）",
                self.version, SCHEMA_VERSION
            ));
        }
        if self.area.is_empty() {
            return Err("campus.json 缺少有效的校园区域（area 顶点不足 3 个）".into());
        }
        for (name, value) in [
            ("clearance_min", self.clearance_min),
            ("clearance_max", self.clearance_max),
            ("base_altitude_m", self.base_altitude_m),
            ("allow_shrink_m", self.allow_shrink_m),
        ] {
            if !value.is_finite() {
                return Err(format!("campus.json 参数 {name} 不是有限数值"));
            }
        }
        if self.clearance_min < 0.0 || self.clearance_max < self.clearance_min {
            return Err("campus.json 净空参数不合法（要求 0 ≤ min ≤ max）".into());
        }
        if self.allow_shrink_m < 0.0 {
            return Err("campus.json 允许区域内缩不能为负数".into());
        }
        if self.base_altitude_m < -500.0 || self.base_altitude_m > 9000.0 {
            return Err("campus.json 基准海拔超出合理范围".into());
        }
        for point in self
            .area
            .rings()
            .flat_map(|ring| ring.iter())
            .chain(self.blocked.iter().flat_map(|b| b.polygon.rings().flat_map(|r| r.iter())))
            .chain(self.corridors.iter().flat_map(|c| c.polygon.rings().flat_map(|r| r.iter())))
            .chain(self.playgrounds.iter().flat_map(|p| p.polygon.rings().flat_map(|r| r.iter())))
            .chain(self.plazas.iter().flat_map(|p| p.polygon.rings().flat_map(|r| r.iter())))
            .chain(self.pois.all().map(|(_, p)| &p.pos))
            .chain(self.roads.iter().flat_map(|r| r.path.iter()))
        {
            if !point[0].is_finite() || !point[1].is_finite() {
                return Err("campus.json 含非有限坐标".into());
            }
        }
        Ok(())
    }
}

/// 校园数据统计量。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CampusStats {
    pub area_m2: f64,
    pub allowed_area_m2: f64,
    pub area_vertices: usize,
    pub area_perimeter_m: f64,
    pub blocked_count: usize,
    pub blocked_area_m2: f64,
    pub corridor_count: usize,
    pub playground_count: usize,
    pub plaza_count: usize,
    pub canteen: usize,
    pub dorm: usize,
    pub gate: usize,
    pub road_count: usize,
    pub road_length_m: f64,
}

impl CampusStats {
    pub fn poi_total(&self) -> usize {
        self.canteen + self.dorm + self.gate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_campus() -> Campus {
        let area = Polygon::new(vec![
            [114.298, 30.498],
            [114.302, 30.498],
            [114.302, 30.502],
            [114.298, 30.502],
        ]);
        Campus::new(area, 1_700_000_000_000)
    }

    #[test]
    fn stats_are_metric_and_respect_blocked_area() {
        let mut campus = sample_campus();
        campus.blocked.push(BlockedArea {
            id: 1,
            name: "测试楼".into(),
            polygon: Polygon::new(vec![
                [114.299, 30.499],
                [114.300, 30.499],
                [114.300, 30.500],
                [114.299, 30.500],
            ]),
        });
        campus.pois.canteen.push(Poi {
            id: 1,
            name: "一食堂".into(),
            pos: [114.2985, 30.4985],
        });
        campus.roads.push(Road {
            id: 1,
            name: "主路".into(),
            path: vec![[114.298, 30.500], [114.302, 30.500]],
        });
        let stats = campus.stats();
        assert!(stats.area_m2 > 40_000.0, "area={}", stats.area_m2);
        assert!(stats.blocked_area_m2 > 8_000.0);
        assert!(stats.allowed_area_m2 < stats.area_m2);
        assert_eq!(stats.area_vertices, 4);
        assert_eq!(stats.poi_total(), 1);
        assert!(stats.road_length_m > 350.0, "road={}", stats.road_length_m);
    }

    #[test]
    fn validate_basic_rejects_bad_version_and_parameters() {
        let mut campus = sample_campus();
        assert!(campus.validate_basic().is_ok());
        campus.version = 99;
        assert!(campus.validate_basic().unwrap_err().contains("版本"));
        campus.version = SCHEMA_VERSION;
        campus.clearance_min = 5.0;
        campus.clearance_max = 1.0;
        assert!(campus.validate_basic().is_err());
        campus.clearance_min = 1.5;
        campus.clearance_max = 3.0;
        campus.area.outer.clear();
        assert!(campus.validate_basic().is_err());
    }

    #[test]
    fn poi_position_requires_inside_area_and_outside_blocked() {
        let mut campus = sample_campus();
        campus.blocked.push(BlockedArea {
            id: 7,
            name: "图书馆".into(),
            polygon: Polygon::new(vec![
                [114.2990, 30.4990],
                [114.3000, 30.4990],
                [114.3000, 30.5000],
                [114.2990, 30.5000],
            ]),
        });
        assert!(campus.validate_poi_position([114.2985, 30.4985]).is_ok());
        assert!(campus
            .validate_poi_position([114.3030, 30.5000])
            .unwrap_err()
            .contains("允许区域外"));
        assert!(campus
            .validate_poi_position([114.2995, 30.4995])
            .unwrap_err()
            .contains("禁行区「图书馆」"));
        assert!(campus
            .validate_poi_position([30.5000, 114.3000])
            .unwrap_err()
            .contains("中国范围"));
        assert!(campus
            .validate_poi_position([f64::NAN, 30.5])
            .unwrap_err()
            .contains("有限数值"));
    }

    #[test]
    fn poi_add_update_remove_reject_invalid_positions_without_mutating() {
        let mut campus = sample_campus();
        campus.blocked.push(BlockedArea {
            id: 7,
            name: "图书馆".into(),
            polygon: Polygon::new(vec![
                [114.2990, 30.4990],
                [114.3000, 30.4990],
                [114.3000, 30.5000],
                [114.2990, 30.5000],
            ]),
        });
        let id = campus
            .add_poi(PoiKind::Canteen, String::new(), [114.2985, 30.4985])
            .unwrap();
        assert_eq!(id, 1);
        assert_eq!(campus.pois.canteen.len(), 1);
        assert_eq!(campus.pois.canteen[0].name, "食堂 POI #1");
        let original_pos = campus.pois.canteen[0].pos;

        assert!(campus
            .add_poi(PoiKind::Dorm, "湖边宿舍".into(), [114.2995, 30.4995])
            .is_err());
        assert_eq!(campus.pois.dorm.len(), 0);
        assert_eq!(campus.pois.canteen.len(), 1);

        assert!(campus
            .update_poi(
                PoiKind::Canteen,
                0,
                "压楼食堂".into(),
                [114.2995, 30.4995],
            )
            .is_err());
        assert_eq!(campus.pois.canteen[0].pos, original_pos);
        assert_eq!(campus.pois.canteen[0].name, "食堂 POI #1");

        campus
            .update_poi(
                PoiKind::Canteen,
                0,
                "一食堂".into(),
                [114.2988, 30.4988],
            )
            .unwrap();
        assert_eq!(campus.pois.canteen[0].name, "一食堂");
        assert_eq!(campus.pois.canteen[0].pos, [114.2988, 30.4988]);

        let removed = campus.remove_poi(PoiKind::Canteen, 0).unwrap();
        assert_eq!(removed.name, "一食堂");
        assert_eq!(campus.pois.canteen.len(), 0);
        assert!(campus.remove_poi(PoiKind::Canteen, 0).is_err());
    }
}
