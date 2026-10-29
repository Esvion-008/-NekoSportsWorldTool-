//! GeoJSON 导入：WGS84 → BD09、结构/几何校验、路网裁剪。
//!
//! 校验策略：能解析的要素全部解析，错误按要素编号累积；只要有一条错误就拒绝
//! 本次导入（不写 campus.json），警告不阻断导入。

use geojson::{Feature, FeatureCollection, GeoJson, Geometry, GeometryValue, Position};
use serde_json::Value;

use super::geo::{
    midpoint, point_in_polygon, point_in_ring, polygon_area_m2, polyline_length_m,
    rings_properly_cross, wgs84_to_bd09, Projection,
};
use super::{
    BlockedArea, Campus, CampusStats, Corridor, LngLat, PlaygroundArea, Poi, PoiKind, Pois,
    Polygon, Road, MIN_CAMPUS_AREA_M2,
};

/// 导入成功结果。
#[derive(Debug)]
pub struct ImportedCampus {
    pub campus: Campus,
    pub stats: CampusStats,
    pub warnings: Vec<String>,
}

/// 导入失败：错误 + 已收集到的警告。
#[derive(Debug)]
pub struct ImportFailure {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// 路网导入成功结果。
#[derive(Debug)]
pub struct ImportedRoads {
    pub roads: Vec<Road>,
    pub warnings: Vec<String>,
    /// 成功保留的原始折线数量。
    pub kept_lines: usize,
    /// 因完全在校园区域外而丢弃的折线数量。
    pub dropped_lines: usize,
}

const CLOSE_EPS_DEG: f64 = 1e-9;

/// 解析 `campus.geojson`（WGS84）并转换/校验为 BD09 校园数据。
pub fn import_campus_geojson(text: &str, now_ms: i64) -> Result<ImportedCampus, ImportFailure> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let fc = match parse_feature_collection(text) {
        Ok(fc) => fc,
        Err(e) => {
            errors.push(e);
            return Err(ImportFailure { errors, warnings });
        }
    };

    let mut area: Option<Polygon> = None;
    let mut blocked: Vec<BlockedArea> = Vec::new();
    let mut corridors: Vec<Corridor> = Vec::new();
    let mut playgrounds: Vec<PlaygroundArea> = Vec::new();
    let mut plazas: Vec<PlaygroundArea> = Vec::new();
    let mut pois = Pois::default();

    for (index, feature) in fc.features.iter().enumerate() {
        let no = index + 1;
        let kind = match feature_kind(feature) {
            Ok(kind) => kind,
            Err(e) => {
                errors.push(format!("要素 #{no}：{e}"));
                continue;
            }
        };
        let name = feature_name(feature);
        let label = feature_label(no, &kind, &name);
        let Some(geometry) = feature.geometry.as_ref() else {
            errors.push(format!("{label} 缺少 geometry"));
            continue;
        };

        match kind.as_str() {
            "area" => {
                if area.is_some() {
                    errors.push(format!("{label}：只允许 1 个 area 校园区域，已忽略该要素"));
                    continue;
                }
                let polygons = match geometry_polygons(geometry) {
                    Ok(p) => p,
                    Err(e) => {
                        errors.push(format!("{label}：{e}"));
                        continue;
                    }
                };
                if polygons.len() != 1 {
                    errors.push(format!("{label}：area 必须是单个 Polygon（不支持 MultiPolygon）"));
                    continue;
                }
                if let Some(polygon) = parse_polygon(&polygons[0], &label, &mut errors, &mut warnings) {
                    area = Some(polygon);
                }
            }
            "blocked" | "corridor" | "playground" | "plaza" => {
                let polygons = match geometry_polygons(geometry) {
                    Ok(p) => p,
                    Err(e) => {
                        errors.push(format!("{label}：{e}"));
                        continue;
                    }
                };
                for (poly_index, rings) in polygons.iter().enumerate() {
                    let polygon_label = if polygons.len() > 1 {
                        format!("{label} 第 {} 个多边形", poly_index + 1)
                    } else {
                        label.clone()
                    };
                    let Some(polygon) =
                        parse_polygon(rings, &polygon_label, &mut errors, &mut warnings)
                    else {
                        continue;
                    };
                    if kind == "blocked" {
                        blocked.push(BlockedArea {
                            id: blocked.len() as u64 + 1,
                            name: name.clone(),
                            polygon,
                        });
                    } else if kind == "playground" {
                        playgrounds.push(PlaygroundArea {
                            id: playgrounds.len() as u64 + 1,
                            name: name.clone(),
                            polygon,
                        });
                    } else if kind == "plaza" {
                        plazas.push(PlaygroundArea {
                            id: plazas.len() as u64 + 1,
                            name: name.clone(),
                            polygon,
                        });
                    } else {
                        corridors.push(Corridor {
                            id: corridors.len() as u64 + 1,
                            name: name.clone(),
                            polygon,
                        });
                    }
                }
            }
            "poi_canteen" | "poi_dorm" | "poi_gate" => {
                let Some(poi_kind) = PoiKind::from_kind_str(&kind) else {
                    unreachable!("kind 已按 poi_* 分支匹配");
                };
                match &geometry.value {
                    GeometryValue::Point { coordinates } => {
                        if let Some(pos) = parse_point(coordinates, &label, &mut errors) {
                            push_poi(&mut pois, poi_kind, name, pos);
                        }
                    }
                    GeometryValue::MultiPoint { coordinates } => {
                        for (point_index, coordinates) in coordinates.iter().enumerate() {
                            let point_label = format!("{label} 第 {} 个点", point_index + 1);
                            if let Some(pos) = parse_point(coordinates, &point_label, &mut errors) {
                                push_poi(&mut pois, poi_kind, name.clone(), pos);
                            }
                        }
                    }
                    other => errors.push(format!(
                        "{label}：几何类型为 {}，POI 需要 Point",
                        other.type_name()
                    )),
                }
            }
            other => warnings.push(format!(
                "要素 #{no}：未识别的 kind「{other}」，已忽略（支持 area/blocked/corridor/poi_*）"
            )),
        }
    }

    let Some(area) = area else {
        errors.push("缺少 kind=\"area\" 的校园区域多边形（必须且只能有 1 个 Polygon）".into());
        return Err(ImportFailure { errors, warnings });
    };

    let projection = Projection::for_polygon(&area);
    validate_area(&area, &projection, &mut errors);
    for blocked_area in &blocked {
        let label = format!(
            "禁行区「{}」",
            display_name(&blocked_area.name, blocked_area.id)
        );
        validate_obstacle(
            &blocked_area.polygon,
            &label,
            &area,
            &projection,
            &mut errors,
        );
    }
    for corridor in &corridors {
        let label = format!("走廊「{}」", display_name(&corridor.name, corridor.id));
        validate_obstacle(&corridor.polygon, &label, &area, &projection, &mut errors);
    }
    for playground in &playgrounds {
        let label = format!("操场「{}」", display_name(&playground.name, playground.id));
        validate_obstacle(&playground.polygon, &label, &area, &projection, &mut errors);
    }
    for plaza in &plazas {
        let label = format!("广场「{}」", display_name(&plaza.name, plaza.id));
        validate_obstacle(&plaza.polygon, &label, &area, &projection, &mut errors);
    }
    validate_pois(&pois, &area, &blocked, &projection, &mut errors, &mut warnings);

    if blocked.is_empty() {
        warnings.push("未导入禁行区（blocked），路线生成将缺少建筑/水面约束".into());
    }

    if !errors.is_empty() {
        return Err(ImportFailure { errors, warnings });
    }

    let mut campus = Campus::new(area, now_ms);
    campus.blocked = blocked;
    campus.corridors = corridors;
    campus.playgrounds = playgrounds;
    campus.plazas = plazas;
    campus.pois = pois;
    let stats = campus.stats();
    Ok(ImportedCampus {
        campus,
        stats,
        warnings,
    })
}

/// 解析 `roads.geojson`（WGS84），转换为 BD09 并按校园区域裁剪。
pub fn import_roads_geojson(
    text: &str,
    campus: &Campus,
) -> Result<ImportedRoads, ImportFailure> {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let fc = match parse_feature_collection(text) {
        Ok(fc) => fc,
        Err(e) => {
            errors.push(e);
            return Err(ImportFailure { errors, warnings });
        }
    };
    if campus.area.is_empty() {
        errors.push("请先导入校园区域（campus.geojson），再导入路网".into());
        return Err(ImportFailure { errors, warnings });
    }
    let projection = Projection::for_polygon(&campus.area);
    let mut roads = Vec::new();
    let mut kept_lines = 0usize;
    let mut dropped_lines = 0usize;

    for (index, feature) in fc.features.iter().enumerate() {
        let no = index + 1;
        let name = feature_name(feature);
        let label = if name.is_empty() {
            format!("路网要素 #{no}")
        } else {
            format!("路网要素 #{no}「{name}」")
        };
        let Some(geometry) = feature.geometry.as_ref() else {
            errors.push(format!("{label} 缺少 geometry"));
            continue;
        };
        let lines: Vec<Vec<Position>> = match &geometry.value {
            GeometryValue::LineString { coordinates } => vec![coordinates.clone()],
            GeometryValue::MultiLineString { coordinates } => coordinates.clone(),
            other => {
                warnings.push(format!(
                    "{label}：几何类型为 {}，路网只接受 LineString，已忽略",
                    other.type_name()
                ));
                continue;
            }
        };
        for (line_index, line) in lines.iter().enumerate() {
            let line_label = if lines.len() > 1 {
                format!("{label} 第 {} 段", line_index + 1)
            } else {
                label.clone()
            };
            let Some(path) = parse_line(line, &line_label, &mut errors) else {
                continue;
            };
            let runs = clip_path_to_area(&path, &campus.area, &projection);
            if runs.is_empty() {
                dropped_lines += 1;
                warnings.push(format!("{line_label} 完全在校园区域外，已丢弃"));
                continue;
            }
            kept_lines += 1;
            for run in runs {
                roads.push(Road {
                    id: roads.len() as u64 + 1,
                    name: name.clone(),
                    path: run,
                });
            }
        }
    }

    if !errors.is_empty() {
        return Err(ImportFailure { errors, warnings });
    }
    if roads.is_empty() {
        warnings.push("没有可用路网（所有折线都在校园区域外或没有有效顶点）".into());
    }
    Ok(ImportedRoads {
        roads,
        warnings,
        kept_lines,
        dropped_lines,
    })
}

/// 重新导入校园区域后，把旧路网按新区域裁剪保留。
pub fn carry_over_roads(old: &Campus, new: &mut Campus) -> Vec<String> {
    if old.roads.is_empty() {
        return Vec::new();
    }
    if new.area.is_empty() {
        new.roads.clear();
        return vec!["新校园区域为空，已清空原路网".into()];
    }
    let projection = Projection::for_polygon(&new.area);
    let mut warnings = Vec::new();
    let mut roads = Vec::new();
    let mut dropped = 0usize;
    for road in &old.roads {
        let runs = clip_path_to_area(&road.path, &new.area, &projection);
        if runs.is_empty() {
            dropped += 1;
            let label = display_name(&road.name, road.id);
            warnings.push(format!("原路网「{label}」不在新校园区域内，已丢弃"));
            continue;
        }
        for run in runs {
            let id = roads.len() as u64 + 1;
            roads.push(Road {
                id,
                name: road.name.clone(),
                path: run,
            });
        }
    }
    let kept = roads.len();
    new.roads = roads;
    if dropped > 0 {
        warnings.push(format!("重新导入校园区域后保留 {kept} 段路网，丢弃 {dropped} 条不在区域内的折线"));
    }
    warnings
}

// ── 解析辅助 ────────────────────────────────────────────────

fn parse_feature_collection(text: &str) -> Result<FeatureCollection, String> {
    let geojson: GeoJson = text
        .parse()
        .map_err(|e| format!("GeoJSON 解析失败：{e}"))?;
    match geojson {
        GeoJson::FeatureCollection(fc) => Ok(fc),
        GeoJson::Feature(_) => Err("顶层是单个 Feature，需要 FeatureCollection（QGIS/GeoJSON.io 导出时选择集合）".into()),
        GeoJson::Geometry(_) => Err("顶层是 Geometry，需要 FeatureCollection".into()),
    }
}

fn feature_kind(feature: &Feature) -> Result<String, String> {
    match feature.property("kind") {
        Some(Value::String(kind)) if !kind.trim().is_empty() => {
            Ok(kind.trim().to_ascii_lowercase())
        }
        Some(Value::String(_)) => Err("properties.kind 为空字符串".into()),
        Some(_) => Err("properties.kind 必须是字符串".into()),
        None => Err("缺少 properties.kind（area/blocked/corridor/poi_*）".into()),
    }
}

fn feature_name(feature: &Feature) -> String {
    for key in ["name", "Name", "NAME", "名称"] {
        if let Some(Value::String(name)) = feature.property(key) {
            let name = name.trim();
            if !name.is_empty() {
                return name.to_string();
            }
        }
    }
    String::new()
}

fn feature_label(no: usize, kind: &str, name: &str) -> String {
    if name.is_empty() {
        format!("要素 #{no}（{kind}）")
    } else {
        format!("要素 #{no}（{kind}「{name}」）")
    }
}

fn display_name(name: &str, id: u64) -> String {
    if name.trim().is_empty() {
        format!("#{id}")
    } else {
        name.trim().to_string()
    }
}

fn geometry_polygons(geometry: &Geometry) -> Result<Vec<Vec<Vec<Position>>>, String> {
    match &geometry.value {
        GeometryValue::Polygon { coordinates } => Ok(vec![coordinates.clone()]),
        GeometryValue::MultiPolygon { coordinates } => Ok(coordinates.clone()),
        other => Err(format!(
            "几何类型为 {}，需要 Polygon 或 MultiPolygon",
            other.type_name()
        )),
    }
}

fn parse_polygon(
    rings: &[Vec<Position>],
    label: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Option<Polygon> {
    if rings.is_empty() {
        errors.push(format!("{label}：Polygon 没有坐标环"));
        return None;
    }
    let mut parsed = Vec::with_capacity(rings.len());
    for (index, raw) in rings.iter().enumerate() {
        let ring_label = if index == 0 {
            format!("{label} 外环")
        } else {
            format!("{label} 内环 {index}")
        };
        let Some(ring) = convert_ring(raw, &ring_label, errors, warnings) else {
            return None;
        };
        parsed.push(ring);
    }
    let outer = parsed.remove(0);
    Some(Polygon {
        outer,
        holes: parsed,
    })
}

fn convert_ring(
    raw: &[Position],
    label: &str,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Option<Vec<LngLat>> {
    if raw.len() < 4 {
        errors.push(format!(
            "{label}：顶点数 {} 不足 4 个（GeoJSON 多边形必须闭合）",
            raw.len()
        ));
        return None;
    }
    let mut converted = Vec::with_capacity(raw.len());
    for (index, position) in raw.iter().enumerate() {
        if position.len() < 2 {
            errors.push(format!("{label} 第 {} 个坐标不足 2 维", index + 1));
            return None;
        }
        let (lng, lat) = (position[0], position[1]);
        if let Err(e) = check_wgs84(lng, lat) {
            errors.push(format!("{label} 第 {} 个坐标：{e}", index + 1));
            return None;
        }
        converted.push(wgs84_to_bd09(lng, lat));
    }
    let first = converted[0];
    let last = *converted.last().expect("converted 非空");
    if !bd_close(first, last) {
        errors.push(format!(
            "{label} 未闭合：首点 ({:.6},{:.6}) 与末点 ({:.6},{:.6}) 不重合",
            first[0], first[1], last[0], last[1]
        ));
        return None;
    }
    converted.pop();

    let before = converted.len();
    converted.dedup_by(|a, b| bd_close(*a, *b));
    let removed = before.saturating_sub(converted.len());
    if removed > 0 {
        warnings.push(format!("{label}：已去除 {removed} 个重复相邻点"));
    }
    if converted.len() < 3 {
        errors.push(format!("{label}：去重后有效顶点 {} 个，少于 3 个", converted.len()));
        return None;
    }
    Some(converted)
}

fn parse_point(
    position: &Position,
    label: &str,
    errors: &mut Vec<String>,
) -> Option<LngLat> {
    if position.len() < 2 {
        errors.push(format!("{label}：坐标不足 2 维"));
        return None;
    }
    let (lng, lat) = (position[0], position[1]);
    if let Err(e) = check_wgs84(lng, lat) {
        errors.push(format!("{label}：{e}"));
        return None;
    }
    Some(wgs84_to_bd09(lng, lat))
}

fn parse_line(
    raw: &[Position],
    label: &str,
    errors: &mut Vec<String>,
) -> Option<Vec<LngLat>> {
    if raw.len() < 2 {
        errors.push(format!("{label}：折线顶点不足 2 个"));
        return None;
    }
    let mut converted = Vec::with_capacity(raw.len());
    for (index, position) in raw.iter().enumerate() {
        if position.len() < 2 {
            errors.push(format!("{label} 第 {} 个坐标不足 2 维", index + 1));
            return None;
        }
        let (lng, lat) = (position[0], position[1]);
        if let Err(e) = check_wgs84(lng, lat) {
            errors.push(format!("{label} 第 {} 个坐标：{e}", index + 1));
            return None;
        }
        converted.push(wgs84_to_bd09(lng, lat));
    }
    converted.dedup_by(|a, b| bd_close(*a, *b));
    if converted.len() < 2 {
        errors.push(format!("{label}：去重后折线顶点不足 2 个"));
        return None;
    }
    Some(converted)
}

fn check_wgs84(lng: f64, lat: f64) -> Result<(), String> {
    if !lng.is_finite() || !lat.is_finite() {
        return Err("坐标不是有限数值".into());
    }
    if !(-180.0..=180.0).contains(&lng) || !(-90.0..=90.0).contains(&lat) {
        return Err(format!(
            "坐标 ({lng:.6}, {lat:.6}) 超出经纬度范围"
        ));
    }
    if !(73.0..=135.5).contains(&lng) || !(3.0..=53.6).contains(&lat) {
        return Err(format!(
            "坐标 ({lng:.6}, {lat:.6}) 不在中国范围：请确认坐标顺序为 [经度, 纬度] 且使用 WGS84"
        ));
    }
    Ok(())
}

fn bd_close(a: LngLat, b: LngLat) -> bool {
    (a[0] - b[0]).abs() <= CLOSE_EPS_DEG && (a[1] - b[1]).abs() <= CLOSE_EPS_DEG
}

fn push_poi(pois: &mut Pois, kind: PoiKind, name: String, pos: LngLat) {
    let list = pois.get_mut(kind);
    let id = list.len() as u64 + 1;
    list.push(Poi { id, name, pos });
}

// ── 校验 ────────────────────────────────────────────────────

fn validate_area(area: &Polygon, projection: &Projection, errors: &mut Vec<String>) {
    let area_m2 = polygon_area_m2(area, projection);
    if area_m2 < MIN_CAMPUS_AREA_M2 {
        errors.push(format!(
            "校园区域面积 {:.0} m² 小于下限 {:.0} m²（{:.2} 公顷）",
            area_m2,
            MIN_CAMPUS_AREA_M2,
            area_m2 / 10_000.0
        ));
    }
    for (ring_index, ring) in area.rings().enumerate() {
        let label = if ring_index == 0 {
            "校园区域外环".to_string()
        } else {
            format!("校园区域内环 #{ring_index}")
        };
        check_ring_self_intersections(ring, &label, projection, errors);
    }
    for (hole_index, hole) in area.holes.iter().enumerate() {
        let label = format!("校园区域内环 #{}", hole_index + 1);
        for (point_index, point) in hole.iter().enumerate() {
            if !point_in_ring(*point, &area.outer, projection) {
                errors.push(format!(
                    "{label} 顶点 #{} 位于外环之外",
                    point_index + 1
                ));
                break;
            }
        }
        if let Some((hole_edge, outer_edge)) = rings_properly_cross(hole, &area.outer, projection)
        {
            errors.push(format!(
                "{label} 边 #{} 与外环边 #{} 交叉",
                hole_edge + 1,
                outer_edge + 1
            ));
        }
    }
}

fn check_ring_self_intersections(
    ring: &[LngLat],
    label: &str,
    projection: &Projection,
    errors: &mut Vec<String>,
) {
    let hits = super::geo::ring_self_intersections(ring, projection);
    if let Some((first, second)) = hits.first() {
        errors.push(format!(
            "{label} 自交：边 #{} 与边 #{} 相交（请检查顶点顺序，多边形不能自交）",
            first + 1,
            second + 1
        ));
    }
}

fn validate_obstacle(
    polygon: &Polygon,
    label: &str,
    area: &Polygon,
    projection: &Projection,
    errors: &mut Vec<String>,
) {
    for (ring_index, ring) in polygon.rings().enumerate() {
        let ring_label = if ring_index == 0 {
            format!("{label} 外环")
        } else {
            format!("{label} 内环 #{ring_index}")
        };
        check_ring_self_intersections(ring, &ring_label, projection, errors);
    }
    let polygon_area = polygon_area_m2(polygon, projection);
    if polygon_area < 1.0 {
        errors.push(format!(
            "{label} 面积过小（{polygon_area:.2} m²），无法形成有效约束"
        ));
    }
    for (hole_index, hole) in polygon.holes.iter().enumerate() {
        let hole_label = format!("{label} 内环 #{}", hole_index + 1);
        for (point_index, point) in hole.iter().enumerate() {
            if !point_in_ring(*point, &polygon.outer, projection) {
                errors.push(format!(
                    "{hole_label} 顶点 #{} 位于外环之外",
                    point_index + 1
                ));
                break;
            }
        }
    }

    // 所有顶点与边中点必须在校园允许区域内（blocked 不能压到区域边界外）
    let mut outside: Option<(usize, usize)> = None;
    'rings: for (ring_index, ring) in polygon.rings().enumerate() {
        for (point_index, point) in ring.iter().enumerate() {
            if !point_in_polygon(*point, area, projection) {
                outside = Some((ring_index, point_index));
                break 'rings;
            }
            let next = ring[(point_index + 1) % ring.len()];
            if !point_in_polygon(midpoint(*point, next), area, projection) {
                outside = Some((ring_index, point_index));
                break 'rings;
            }
        }
    }
    if let Some((ring_index, point_index)) = outside {
        let ring_label = if ring_index == 0 {
            "外环".to_string()
        } else {
            format!("内环 #{}", ring_index)
        };
        errors.push(format!(
            "{label} {ring_label} 边 #{} 越出校园允许区域（禁行区/走廊必须完整落在 area 内）",
            point_index + 1
        ));
        return;
    }

    for (ring_index, ring) in polygon.rings().enumerate() {
        for (area_ring_index, area_ring) in area.rings().enumerate() {
            if let Some((edge, area_edge)) = rings_properly_cross(ring, area_ring, projection) {
                errors.push(format!(
                    "{label} {}边 #{} 与校园区域{}边 #{} 交叉",
                    if ring_index == 0 { "外环" } else { "内环 " },
                    edge + 1,
                    if area_ring_index == 0 { "外环" } else { "内环 " },
                    area_edge + 1
                ));
                return;
            }
        }
    }
}

fn validate_pois(
    pois: &Pois,
    area: &Polygon,
    blocked: &[BlockedArea],
    projection: &Projection,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    for (kind, poi) in pois.all() {
        let label = format!("{} POI「{}」", kind.label(), display_name(&poi.name, poi.id));
        if !point_in_polygon(poi.pos, area, projection) {
            errors.push(format!("{label} 位于校园允许区域外"));
            continue;
        }
        if let Some(hit) = blocked
            .iter()
            .find(|blocked_area| point_in_polygon(poi.pos, &blocked_area.polygon, projection))
        {
            errors.push(format!(
                "{label} 位于禁行区「{}」内",
                display_name(&hit.name, hit.id)
            ));
        }
    }

    // 近似重合的 POI 只提示，不阻断导入
    let all: Vec<(PoiKind, &Poi)> = pois.all().collect();
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            if projection.distance_m(all[i].1.pos, all[j].1.pos) < 1.0 {
                warnings.push(format!(
                    "{} POI 与 {} POI 相距不足 1 m，请确认是否重复标注",
                    all[i].0.label(),
                    all[j].0.label()
                ));
            }
        }
    }

    for kind in PoiKind::ALL {
        let count = pois.get(kind).len();
        if !(2..=4).contains(&count) {
            warnings.push(format!(
                "{} POI 数量 {count}，建议每类 2–4 个",
                kind.label()
            ));
        }
    }
}

/// 把折线按校园区域裁剪为若干段（顶点/边中点都必须在区域内）。
pub fn clip_path_to_area(path: &[LngLat], area: &Polygon, projection: &Projection) -> Vec<Vec<LngLat>> {
    let mut runs: Vec<Vec<LngLat>> = Vec::new();
    let mut current: Vec<LngLat> = Vec::new();
    for point in path {
        if !point_in_polygon(*point, area, projection) {
            if current.len() >= 2 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }
        if let Some(previous) = current.last().copied() {
            if !point_in_polygon(midpoint(previous, *point), area, projection) {
                if current.len() >= 2 {
                    runs.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
        }
        current.push(*point);
    }
    if current.len() >= 2 {
        runs.push(current);
    }
    runs
}

/// 路网总长度（米），供界面统计。
pub fn roads_length_m(roads: &[Road], projection: &Projection) -> f64 {
    roads
        .iter()
        .map(|road| polyline_length_m(&road.path, projection))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LNG: f64 = 114.297;
    const LAT: f64 = 30.498;

    fn rect(lng0: f64, lat0: f64, d_lng: f64, d_lat: f64) -> Vec<(f64, f64)> {
        vec![
            (lng0, lat0),
            (lng0 + d_lng, lat0),
            (lng0 + d_lng, lat0 + d_lat),
            (lng0, lat0 + d_lat),
            (lng0, lat0),
        ]
    }

    fn polygon_feature(kind: &str, name: &str, rings: &[Vec<(f64, f64)>]) -> Value {
        let coordinates: Vec<Value> = rings
            .iter()
            .map(|ring| {
                Value::Array(
                    ring.iter()
                        .map(|(lng, lat)| json!([lng, lat]))
                        .collect(),
                )
            })
            .collect();
        json!({
            "type": "Feature",
            "properties": { "kind": kind, "name": name },
            "geometry": { "type": "Polygon", "coordinates": coordinates },
        })
    }

    fn point_feature(kind: &str, name: &str, lng: f64, lat: f64) -> Value {
        json!({
            "type": "Feature",
            "properties": { "kind": kind, "name": name },
            "geometry": { "type": "Point", "coordinates": [lng, lat] },
        })
    }

    fn line_feature(name: &str, points: &[(f64, f64)]) -> Value {
        let coordinates: Vec<Value> = points.iter().map(|(lng, lat)| json!([lng, lat])).collect();
        json!({
            "type": "Feature",
            "properties": { "name": name },
            "geometry": { "type": "LineString", "coordinates": coordinates },
        })
    }

    fn collection(features: Vec<Value>) -> String {
        json!({ "type": "FeatureCollection", "features": features }).to_string()
    }

    fn default_area() -> Value {
        polygon_feature("area", "测试大学", &[rect(LNG, LAT, 0.006, 0.004)])
    }

    fn valid_features() -> Vec<Value> {
        vec![
            default_area(),
            polygon_feature("blocked", "图书馆", &[rect(114.2985, 30.4990, 0.001, 0.0008)]),
            polygon_feature("corridor", "连廊", &[rect(114.3000, 30.5005, 0.0008, 0.0006)]),
            point_feature("poi_canteen", "一食堂", 114.3005, 30.4995),
            point_feature("poi_canteen", "二食堂", 114.3015, 30.5015),
            point_feature("poi_dorm", "一号楼", 114.2978, 30.5010),
            point_feature("poi_dorm", "二号楼", 114.3020, 30.4985),
            point_feature("poi_gate", "东门", 114.3025, 30.5005),
            point_feature("poi_gate", "西门", 114.2975, 30.4955 + 0.004),
        ]
    }

    #[test]
    fn imports_valid_campus_and_converts_wgs84_to_bd09() {
        let outcome = import_campus_geojson(&collection(valid_features()), 1_700_000_000_000)
            .unwrap_or_else(|failure| panic!("导入失败：{:?}", failure.errors));
        assert_eq!(outcome.campus.blocked.len(), 1);
        assert_eq!(outcome.campus.corridors.len(), 1);
        assert_eq!(outcome.campus.poi_total(), 6);
        assert_eq!(outcome.stats.canteen, 2);
        assert_eq!(outcome.stats.dorm, 2);
        assert_eq!(outcome.stats.gate, 2);
        assert!((outcome.stats.area_m2 - 257_000.0).abs() < 30_000.0, "area={}", outcome.stats.area_m2);
        assert_eq!(outcome.campus.area.outer.len(), 4);
        assert_eq!(outcome.campus.created_ms, 1_700_000_000_000);
        // 百度坐标相对 WGS84 有数百米偏移
        let bd = outcome.campus.area.outer[0];
        assert!((bd[0] - LNG).abs() > 0.001, "bd={bd:?}");
        assert!((bd[1] - LAT).abs() > 0.001, "bd={bd:?}");
        assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    }

    #[test]
    fn rejects_unclosed_polygon_with_clear_message() {
        let mut ring = rect(LNG, LAT, 0.006, 0.004);
        ring.pop();
        let text = collection(vec![polygon_feature("area", "未闭合", &[ring])]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(failure.errors.iter().any(|e| e.contains("未闭合")), "{:?}", failure.errors);
    }

    #[test]
    fn rejects_self_intersecting_area() {
        let bow_tie = vec![
            (LNG, LAT),
            (LNG + 0.006, LAT + 0.004),
            (LNG + 0.006, LAT),
            (LNG, LAT + 0.004),
            (LNG, LAT),
        ];
        let text = collection(vec![polygon_feature("area", "蝴蝶结", &[bow_tie])]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(failure.errors.iter().any(|e| e.contains("自交")), "{:?}", failure.errors);
    }

    #[test]
    fn rejects_area_below_minimum_size() {
        let text = collection(vec![polygon_feature(
            "area",
            "太小",
            &[rect(LNG, LAT, 0.0005, 0.0005)],
        )]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(failure.errors.iter().any(|e| e.contains("面积")), "{:?}", failure.errors);
    }

    #[test]
    fn rejects_blocked_polygon_outside_campus() {
        let text = collection(vec![
            default_area(),
            polygon_feature("blocked", "越界楼", &[rect(114.290, 30.498, 0.001, 0.001)]),
        ]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(
            failure.errors.iter().any(|e| e.contains("越出校园允许区域")),
            "{:?}",
            failure.errors
        );
    }

    #[test]
    fn rejects_poi_inside_blocked_or_outside_area() {
        let text = collection(vec![
            default_area(),
            polygon_feature("blocked", "图书馆", &[rect(114.2985, 30.4990, 0.001, 0.0008)]),
            point_feature("poi_canteen", "压楼食堂", 114.2990, 30.4994),
        ]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(failure.errors.iter().any(|e| e.contains("禁行区")), "{:?}", failure.errors);

        let text = collection(vec![
            default_area(),
            point_feature("poi_gate", "区域外校门", 114.310, 30.500),
        ]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(
            failure.errors.iter().any(|e| e.contains("校园允许区域外")),
            "{:?}",
            failure.errors
        );
    }

    #[test]
    fn rejects_swapped_longitude_latitude_order() {
        let text = collection(vec![
            default_area(),
            point_feature("poi_gate", "坐标反了", 30.500, 114.300),
        ]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(
            failure
                .errors
                .iter()
                .any(|e| e.contains("不在中国范围") || e.contains("超出经纬度范围")),
            "{:?}",
            failure.errors
        );
    }

    #[test]
    fn rejects_duplicate_area_and_missing_kind() {
        let text = collection(vec![
            default_area(),
            default_area(),
        ]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(failure.errors.iter().any(|e| e.contains("只允许 1 个 area")), "{:?}", failure.errors);

        let mut feature = default_area();
        feature["properties"] = json!({ "name": "无类别" });
        let failure = import_campus_geojson(&collection(vec![feature]), 1).unwrap_err();
        assert!(failure.errors.iter().any(|e| e.contains("properties.kind")), "{:?}", failure.errors);
    }

    #[test]
    fn unknown_kind_is_warning_and_does_not_block_import() {
        let mut extra = default_area();
        extra["properties"] = json!({ "kind": "unknown_kind" });
        let mut features = valid_features();
        features.push(extra);
        let outcome = import_campus_geojson(&collection(features), 1)
            .unwrap_or_else(|failure| panic!("导入失败：{:?}", failure.errors));
        assert!(
            outcome.warnings.iter().any(|w| w.contains("未识别")),
            "{:?}",
            outcome.warnings
        );
    }

    #[test]
    fn holes_are_imported_and_points_in_holes_are_rejected() {
        let outer = rect(LNG, LAT, 0.006, 0.004);
        let hole = rect(114.3000, 30.4995, 0.0008, 0.0006);
        let text = collection(vec![
            polygon_feature("area", "带水面", &[outer, hole]),
            point_feature("poi_gate", "水里校门", 114.3004, 30.4998),
        ]);
        let failure = import_campus_geojson(&text, 1).unwrap_err();
        assert!(
            failure.errors.iter().any(|e| e.contains("校园允许区域外")),
            "{:?}",
            failure.errors
        );

        let text = collection(vec![polygon_feature("area", "带水面", &[
            rect(LNG, LAT, 0.006, 0.004),
            rect(114.3000, 30.4995, 0.0008, 0.0006),
        ])]);
        let outcome = import_campus_geojson(&text, 1).unwrap();
        assert_eq!(outcome.campus.area.holes.len(), 1);
        assert!(outcome.stats.area_m2 < 257_000.0);
    }

    #[test]
    fn roads_are_clipped_to_campus_area() {
        let campus = import_campus_geojson(&collection(valid_features()), 1)
            .unwrap()
            .campus;
        // 首末点在区域外，中间两个点在区域内：应裁剪成一段
        let inside = collection(vec![line_feature(
            "主路",
            &[
                (114.2969, 30.5000),
                (114.2975, 30.5000),
                (114.3000, 30.5000),
                (114.3032, 30.5000),
            ],
        )]);
        let outcome = import_roads_geojson(&inside, &campus).unwrap();
        assert_eq!(outcome.kept_lines, 1);
        assert_eq!(outcome.roads.len(), 1);
        let projection = Projection::for_polygon(&campus.area);
        assert!(outcome.roads[0]
            .path
            .iter()
            .all(|p| point_in_polygon(*p, &campus.area, &projection)));
        assert!(outcome.roads[0].path.len() >= 2);

        // 完全在区域外：丢弃并提示
        let outside = collection(vec![line_feature(
            "校外路",
            &[(114.285, 30.500), (114.315, 30.500)],
        )]);
        let outcome = import_roads_geojson(&outside, &campus).unwrap();
        assert_eq!(outcome.roads.len(), 0);
        assert_eq!(outcome.dropped_lines, 1);
        assert!(outcome.warnings.iter().any(|w| w.contains("完全在校园区域外")));
    }

    #[test]
    fn carry_over_roads_clips_old_roads_to_new_area() {
        let mut old = import_campus_geojson(&collection(valid_features()), 1)
            .unwrap()
            .campus;
        old.roads = vec![Road {
            id: 1,
            name: "老路".into(),
            path: vec![[114.3000, 30.5000], [114.3002, 30.5000]],
        }];
        let mut new = Campus::new(
            Polygon::new(vec![
                [114.2995, 30.4995],
                [114.302, 30.4995],
                [114.302, 30.502],
                [114.2995, 30.502],
            ]),
            2,
        );
        let warnings = carry_over_roads(&old, &mut new);
        assert_eq!(new.roads.len(), 1);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn sample_files_are_valid() {
        let campus_text = include_str!("../../docs/sample/campus.geojson");
        let outcome = import_campus_geojson(campus_text, 1)
            .unwrap_or_else(|failure| panic!("示例校园导入失败：{:?}", failure.errors));
        assert!(outcome.stats.area_m2 > 1_000_000.0, "area={}", outcome.stats.area_m2);
        assert_eq!(outcome.stats.canteen, 2);
        assert_eq!(outcome.stats.dorm, 2);
        assert_eq!(outcome.stats.gate, 2);
        assert!(outcome.stats.blocked_count >= 3);

        let roads_text = include_str!("../../docs/sample/roads.geojson");
        let roads = import_roads_geojson(roads_text, &outcome.campus)
            .unwrap_or_else(|failure| panic!("示例路网导入失败：{:?}", failure.errors));
        assert!(roads.roads.len() >= 3, "roads={}", roads.roads.len());
        let projection = Projection::for_polygon(&outcome.campus.area);
        assert!(roads_length_m(&roads.roads, &projection) > 1_000.0);
    }

    #[test]
    fn road_length_matches_projection() {
        let projection = Projection::at(30.5, 114.3);
        let road = Road {
            id: 1,
            name: String::new(),
            path: vec![[114.3, 30.5], [114.31, 30.5]],
        };
        let length = roads_length_m(std::slice::from_ref(&road), &projection);
        assert!((length - 959.0).abs() < 5.0, "length={length}");
    }
}
