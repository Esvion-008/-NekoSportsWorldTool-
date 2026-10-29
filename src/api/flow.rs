//! 全链编排：policy → 实时点位 → 轨迹生成 → 提交 → OBS → 详情验证。
//! 由 UI 后台线程调用，log 闭包回传日志。

use super::client::ApiClient;
use super::model::Session;
use super::points;
use super::policy::fetch_policy;
use super::records::fetch_one_record;
use super::submit::{submit_record, SubmitParams, SubmitResult};
use crate::track::generator::build as gen_track;
use crate::track::wire::{build_obs_object, five_point_wrapper, obs_keys};
use crate::location::Coordinate;
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RouteEngine {
    V1,
    V2,
    V3,
    #[default]
    V4,
}

#[derive(Clone, Copy)]
pub struct RunParams {
    /// 距离（米）与时长（秒）已由 UI 参数解析。
    pub dist: f64,
    pub dur: i64,
    /// 开始时间（毫秒）。
    pub start_ms: i64,
    pub face_check: i64,
    /// 用户手动填写的绝对海拔（米）；None 使用生成器海拔。
    pub manual_altitude: Option<f64>,
    /// 用户手动填写的海拔范围；与单值字段兼容，范围优先。
    pub manual_altitude_range: Option<crate::track::altitude::AltitudeRange>,
    pub seed: u64,
    pub route_engine: RouteEngine,
}

pub struct RunOutcome {
    pub result: SubmitResult,
    pub obs_ok: usize,
    pub detail_ok: bool,
}

fn sleep_secs(s: u64) {
    std::thread::sleep(std::time::Duration::from_secs(s));
}

/// 同一账号在同一基础种子上稳定；不同账号/不同人得到不同路线。
fn account_seed(base: u64, username: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in username.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash ^ base.rotate_left(17)
}

/// 操场模式：按账号 seed 轻微扭曲环线形状。第一个点保持原坐标（必须在
/// 第一个打卡点起跑），其余点移动控制在几米内，仍落在 10m 到达圈内。
fn warp_playground_points(points: &[(f64, f64)], seed: u64) -> Vec<(f64, f64)> {
    if points.len() < 2 {
        return points.to_vec();
    }
    let center_lat = points.iter().map(|point| point.0).sum::<f64>() / points.len() as f64;
    let center_lng = points.iter().map(|point| point.1).sum::<f64>() / points.len() as f64;
    let phase = (seed % 6283) as f64 / 1000.0;
    let rotation = ((seed / 6283) % 101) as f64 / 100.0 - 0.5;
    let shortcut_index = 1 + ((seed / 97) as usize % (points.len().saturating_sub(1).max(1)));
    let shortcut_scale = 0.93 + ((seed / 7) % 41) as f64 / 1000.0;
    // 跑者绕操场时上下两个圆角不会完全对称；每次按账号 seed 取不同幅度。
    let top_arc_scale = 1.02 + ((seed / 11) % 7) as f64 / 100.0;
    let bottom_arc_scale = 0.93 + ((seed / 13) % 6) as f64 / 100.0;
    let cos_lat = center_lat.to_radians().cos();
    let mut result = Vec::with_capacity(points.len());
    for (index, (lat, lng)) in points.iter().enumerate() {
        if index == 0 {
            result.push((*lat, *lng));
            continue;
        }
        let mut x = (lng - center_lng) * 111_320.0 * cos_lat;
        let mut y = (lat - center_lat) * 111_132.0;
        y *= if y >= 0.0 { top_arc_scale } else { bottom_arc_scale };
        let angle = y.atan2(x);
        let mut scale = 1.0 + 0.025 * (2.0 * angle + phase).sin() + 0.012 * (3.0 * angle - phase).sin();
        if index == shortcut_index {
            // 像学生抄一点内侧近路：只把该点向场内收几米，仍在 10m 到达圈内。
            scale *= shortcut_scale;
        }
        x *= scale;
        y *= scale;
        let (sin_r, cos_r) = rotation.sin_cos();
        let rx = x * cos_r - y * sin_r;
        let ry = x * sin_r + y * cos_r;
        result.push((
            center_lat + ry / 111_132.0,
            center_lng + rx / (111_320.0 * cos_lat),
        ));
    }
    result
}

/// 跑步全链。
pub fn run_full_flow(
    client: &mut ApiClient,
    params: &RunParams,
    log: &mut dyn FnMut(&str),
) -> Result<RunOutcome, String> {
    let sess: Session = client.login.clone().ok_or("未登录")?;

    // ① policy
    log("[policy] 拉取跑步策略…");
    let pol = fetch_policy(client)?;
    log(&format!(
        "√ [policy] ts={} policy={} minDistance={} validTime={}",
        pol.timestamp, pol.policy, pol.min_distance, pol.valid_time
    ));
    sleep_secs(2);

    // ② 实时点位（拒绝本地样本兜底）
    log("[points] 拉取实时点位…");
    if client.identity.has_unconfigured_default_location() {
        return Err("请先在设备信息页填写本次跑步所在城市和定位锚点，不能使用未配置的占位默认值".into());
    }
    let anchor: Coordinate = client.identity.anchor_coordinate()?;
    // 批量刷跑时同锚点只真正请求一次（点位接口 5 分钟限 3 次）
    let pts = points::fetch_points_shared(client, anchor, log)?;
    if pts.is_empty() {
        return Err("实时点位为空 —— 拒绝本地样本兜底".into());
    }
    log(&format!("√ [points] {} 个点位", pts.len()));
    for p in pts.iter().take(5) {
        log(&format!(
            "  [points] {} BD=({:.6},{:.6}) GCJ=({},{})",
            p.get("pointName").and_then(|v| v.as_str()).unwrap_or(""),
            p.get("lat").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("lon").and_then(|v| v.as_f64()).unwrap_or(0.0),
            p.get("glat").map(|v| v.to_string()).unwrap_or_default(),
            p.get("glon").map(|v| v.to_string()).unwrap_or_default(),
        ));
    }

    // ③ 轨迹生成（打卡点拟合环）
    // 注意：打卡点顺序不能动——轨迹必须从服务端给的第一个打卡点起跑，
    // 随机性由生成器在「起点之后」施加（方向 / 速度曲线 / 抖动 / 海拔）。
    let pts_bd = points::points_bd(&pts);
    log(&format!(
        "[track] 起点 = 第一个打卡点（{}），方向与速度曲线随机",
        pts_bd
            .first()
            .map(|(lat, lon)| format!("{lat:.6},{lon:.6}"))
            .unwrap_or_default()
    ));
    // 平均配速须落在有效窗口内（否则逐点速度无法全窗内），越界时修正时长
    let mut params = *params;    let avg = params.dist / params.dur as f64;
    let fixed_avg = avg.clamp(
        crate::track::generator::SPEED_FLOOR + 0.1,
        crate::track::generator::SPEED_CEIL - 0.1,
    );
    if (fixed_avg - avg).abs() > 1e-6 {
        let fixed_dur = (params.dist / fixed_avg).round() as i64;
        log(&format!(
            "[track] 平均配速 {} m/s 超出有效窗口，时长 {} -> {}s",
            (avg * 100.0).round() / 100.0,
            params.dur,
            fixed_dur
        ));
        params.dur = fixed_dur;
    }
    log(&format!(
        "[track] 生成轨迹 {:.0}m / {}s（{} 点位拟合环）…",
        params.dist,
        params.dur,
        pts_bd.len()
    ));
    // 随机 0-4 秒偏移（终端上报的 flag 与首点差 <5s），轨迹/提交/OBS/五点统一使用
    let start_ms = params.start_ms + (rand::random::<i64>() % 5) * 1000;
    let mut track = match params.route_engine {
        RouteEngine::V1 => gen_track(
            params.dist,
            params.dur,
            params.seed,
            (anchor.latitude, anchor.longitude),
            start_ms,
            &pts_bd,
        ),
        RouteEngine::V2 => {
            let campus = crate::campus::store::load()?
                .ok_or("route_engine=v2：请先在「校园地图」页导入 campus.json")?;
            let checkpoints: Vec<crate::campus::LngLat> = pts_bd
                .iter()
                .map(|(lat, lng)| [*lng, *lat])
                .collect();
            log(&format!(
                "[route] V2 生成：{} 个打卡点，目标 2000m，种子 {}",
                checkpoints.len(),
                params.seed
            ));
            let candidates = crate::route::generate(
                &campus,
                &checkpoints,
                crate::route::RouteOptions {
                    target_m: 2000.0,
                    max_candidates: 12,
                    seed: params.seed,
                },
            )?;
            let candidate = candidates
                .into_iter()
                .next()
                .ok_or("V2 没有可用候选路线")?;
            log(&format!(
                "√ [route] V2 候选：总长 {:.1}m / 巡回路 {:.1}m / 终点 {} / 转角 {} / 直曲 {:.0}%/{:.0}%",
                candidate.total_m,
                candidate.tour_m,
                candidate.end_poi.as_deref().unwrap_or("末打卡点"),
                candidate.corners,
                candidate.straight_ratio * 100.0,
                candidate.curve_ratio * 100.0
            ));
            let pace = params.dur as f64 / (params.dist / 1000.0).max(0.001);
            crate::route::build_track(&campus, &candidate, pace, params.seed, start_ms)
        }
        RouteEngine::V3 => {
            let campus = crate::campus::store::load()?
                .ok_or("route_engine=v3：请先在「校园地图」页导入 campus.json")?;
            let checkpoints: Vec<crate::campus::LngLat> = pts_bd
                .iter()
                .map(|(lat, lng)| [*lng, *lat])
                .collect();
            let route_seed = account_seed(params.seed, &sess.username);
            if crate::route::v3::is_playground_in_campus(&campus, &checkpoints, 180.0) {
                let dist = params.dist.max(2000.0) + (route_seed % 101) as f64;
                let warped = warp_playground_points(&pts_bd, route_seed);
                log(&format!(
                    "[route] 检测到操场点位，切换绕圈模式：{} 个点，目标 {:.0}m，账号种子 {}，轨迹形状已按账号变形（广场只作为可穿行区域）",
                    checkpoints.len(),
                    dist,
                    route_seed
                ));
                let mut track = gen_track(
                    dist,
                    params.dur,
                    route_seed,
                    (anchor.latitude, anchor.longitude),
                    start_ms,
                    &warped,
                );
                let safe = track
                    .locations
                    .iter()
                    .all(|location| campus.route_point_ok([location.gLng, location.gLat], 0.2));
                if !safe {
                    log("[route] 操场轨迹贴近建筑/边界，收紧环线后重试");
                    track = gen_track(
                        dist,
                        params.dur,
                        route_seed,
                        (anchor.latitude, anchor.longitude),
                        start_ms,
                        &pts_bd,
                    );
                }
                let safe = track
                    .locations
                    .iter()
                    .all(|location| campus.route_point_ok([location.gLng, location.gLat], 0.2));
                if !safe {
                    return Err("操场绕圈轨迹无法避开建筑/边界，拒绝生成".into());
                }
                track
            } else {
                log(&format!(
                    "[route] V3 生成：使用服务器下发的 {} 个打卡点，严格顺序，道路优先，账号种子 {}",
                    checkpoints.len(),
                    route_seed
                ));
                let candidates = crate::route::v3::generate(
                    &campus,
                    &checkpoints,
                    crate::route::v3::V3Rules {
                        seed: route_seed,
                        ..Default::default()
                    },
                )?;
                let candidate = candidates
                    .into_iter()
                    .next()
                    .ok_or("V3 没有可用候选路线")?;
                log(&format!(
                    "√ [route] V3 候选：总长 {:.1}m / P5 {:.1}m / P5后 {:.1}m / 道路覆盖 {:.0}% / 离路 {:.1}m / 掉头 {} / 转向 {} / 评分 {:.1}",
                    candidate.distance_m,
                    candidate.p5_distance_m,
                    candidate.tail_distance_m,
                    candidate.road_coverage * 100.0,
                    candidate.offroad_m,
                    candidate.immediate_backtracks,
                    candidate.turns,
                    candidate.score
                ));
                for diagnostic in &candidate.diagnostics {
                    log(&format!("[route] {diagnostic}"));
                }
                let pace = params.dur as f64 / (params.dist / 1000.0).max(0.001);
                crate::route::v3::build_track(&campus, &candidate, pace, route_seed, start_ms)
            }
        }
        RouteEngine::V4 => {
                let campus = crate::campus::store::load()?
                    .ok_or("route_engine=v4：请先在「校园地图」页导入 campus.json")?;
                let checkpoints: Vec<crate::campus::LngLat> = pts_bd
                    .iter()
                    .map(|(lat, lng)| [*lng, *lat])
                    .collect();
                let route_seed = account_seed(params.seed, &sess.username);
                let pace = params.dur as f64 / (params.dist / 1000.0).max(0.001);

                // 先检查：服务端偶尔会下发落在建筑内部的点位（实例：点位3 落在「培训中心」里）。
                // 这种点打不了，提前说清楚，别让人以为是软件丢了点。
                {
                    let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
                    for (index, task) in checkpoints.iter().enumerate() {
                        for blocked in &campus.blocked {
                            if crate::campus::geo::point_in_polygon(
                                *task,
                                &blocked.polygon,
                                &projection,
                            ) {
                                log(&format!(
                                    "⚠ [points] P{}（{:.6},{:.6}）落在建筑「{}」内部 —— 这个点打不了，服务端点位配置有问题",
                                    index + 1,
                                    task[1],
                                    task[0],
                                    blocked.name
                                ));
                            }
                        }
                    }
                }

                // 先试操场绕圈。点位里混进操场外的点（服务端会下发「多个操场点位 + 操场外点位」
                // 这种混合组合）、或跑道放不下时，**直接改走校园路网**——
                // 绝不回退「过点椭圆」：那会拉出一条穿过全部点、伸出操场一百多米的巨椭圆。
                // 先分类：全在操场 → 操场绕圈；全在操场外 → 校园 A*；混合 → 混合模式。
                let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
                let in_playground: Vec<bool> = checkpoints
                    .iter()
                    .map(|task| {
                        campus.playgrounds.iter().any(|playground| {
                            crate::campus::geo::point_in_polygon(*task, &playground.polygon, &projection)
                                || crate::campus::geo::distance_to_polygon_boundary(
                                    *task,
                                    &playground.polygon,
                                    &projection,
                                ) <= 12.0
                        })
                    })
                    .collect();
                let inside_count = in_playground.iter().filter(|v| **v).count();
                let mixed = inside_count > 0 && inside_count < checkpoints.len();
                if mixed {
                    log(&format!(
                        "[route] V4 混合模式：{} 个点里 {} 个在操场内、{} 个在操场外 —— 操场内段沿跑道弧长，操场外段 A* 绕建筑，任何一段不穿楼",
                        checkpoints.len(),
                        inside_count,
                        checkpoints.len() - inside_count
                    ));
                    let candidate = crate::route::v4::generate_mixed(
                        &campus,
                        &checkpoints,
                        params.dist.max(2000.0),
                        route_seed,
                    )?;
                    for warning in &candidate.warnings {
                        log(&format!("[route] ⚠ {warning}"));
                    }
                    log(&format!(
                        "√ [route] V4 混合路线：总长 {:.1}m，{} 个路线点",
                        candidate.total_m,
                        candidate.points.len()
                    ));
                    crate::route::build_track_realistic(&campus, &candidate, pace, route_seed, start_ms)
                } else {
                let playground_candidate = if crate::route::v3::is_playground_in_campus(
                    &campus,
                    &checkpoints,
                    180.0,
                ) {
                    let dist = params.dist.max(2000.0) + (route_seed % 101) as f64;
                    log(&format!(
                        "[route] V4 操场模式：{} 个点在操场，按标准跑道几何绕圈（1.22m/道、圈间换道），目标 {:.0}m，种子 {}",
                        checkpoints.len(),
                        dist,
                        route_seed
                    ));
                    match crate::route::playground::track_loop(&campus, &checkpoints, dist, route_seed) {
                        Ok(candidate) => Some(candidate),
                        Err(error) => {
                            log(&format!("[route] 跑道拟合不可用（{error}），改走校园路网模式"));
                            None
                        }
                    }
                } else {
                    None
                };

                let track = if let Some(candidate) = playground_candidate {
                    log(&format!(
                        "√ [route] V4 操场轨迹：穿过 {} 个打卡点，总长 {:.1}m，{} 个路线点，直道 {:.0}% / 弯道 {:.0}%，离建筑 ≥8m（含噪声后 ≥5m）",
                        candidate.checkpoints.len(),
                        candidate.total_m,
                        candidate.points.len(),
                        candidate.straight_ratio * 100.0,
                        candidate.curve_ratio * 100.0
                    ));
                    for warning in &candidate.warnings {
                        log(&format!("[route] ⚠ {warning}"));
                    }
                    let track =
                        crate::route::build_track_realistic(&campus, &candidate, pace, route_seed, start_ms);
                    let safe = track
                        .locations
                        .iter()
                        .all(|location| campus.route_point_ok([location.gLng, location.gLat], 0.2));
                    if !safe {
                        return Err("V4 操场轨迹贴近围墙/建筑，拒绝生成".into());
                    }
                    track
                } else {
                    log(&format!(
                        "[route] V4 校园路线：{} 个打卡点，严格 P1→P5 顺序，A* 避建筑 + 重复惩罚绕行（走过的格子加价，回程不原路重叠），种子 {}",
                        checkpoints.len(),
                        route_seed
                    ));
                    let candidate = crate::route::v4::generate(
                        &campus,
                        &checkpoints,
                        params.dist.max(2000.0),
                        route_seed,
                    )?;
                    // 重复度：每 8 个采样点取一个，看它离「前后 40 点之外」的路径最近多远。
                    let projection = crate::campus::geo::Projection::for_polygon(&campus.area);
                    let xy: Vec<[f64; 2]> = candidate
                        .points
                        .iter()
                        .map(|point| projection.xy(*point))
                        .collect();
                    let (mut dup, mut sampled) = (0usize, 0usize);
                    for index in (0..xy.len()).step_by(8) {
                        let low = index.saturating_sub(40);
                        let high = (index + 40).min(xy.len());
                        let mut best = f64::INFINITY;
                        for (other, point) in xy.iter().enumerate() {
                            if other >= low && other < high {
                                continue;
                            }
                            let distance = ((point[0] - xy[index][0]).powi(2)
                                + (point[1] - xy[index][1]).powi(2))
                            .sqrt();
                            if distance < best {
                                best = distance;
                            }
                        }
                        if best.is_finite() {
                            sampled += 1;
                            if best < 2.0 {
                                dup += 1;
                            }
                        }
                    }
                    log(&format!(
                        "√ [route] V4 校园路线：总长 {:.1}m，{} 个路线点，拐角 {} 个，重复路段 {:.0}%（旧算法是 100%），离建筑 ≥8m（含噪声后 ≥5m）",
                        candidate.total_m,
                        candidate.points.len(),
                        candidate.corners,
                        if sampled > 0 {
                            100.0 * dup as f64 / sampled as f64
                        } else {
                            0.0
                        }
                    ));
                    for warning in &candidate.warnings {
                        log(&format!("[route] ⚠ {warning}"));
                    }
                    crate::route::build_track_realistic(&campus, &candidate, pace, route_seed, start_ms)
                };
                // —— 打卡点强制校验：轨迹必须真的经过每一个打卡点 ——
                // 服务端判有效就是看这个；漏一个等于白跑。宁可报错也不提交半成品。
                let projection_check = crate::campus::geo::Projection::for_polygon(&campus.area);
                let mut worst = 0.0f64;
                let mut worst_index = 0usize;
                for (i, task) in checkpoints.iter().enumerate() {
                    let best = track
                        .locations
                        .iter()
                        .map(|loc| projection_check.distance_m(*task, [loc.gLng, loc.gLat]))
                        .fold(f64::INFINITY, f64::min);
                    if best > worst {
                        worst = best;
                        worst_index = i;
                    }
                }
                if worst > 15.0 {
                    return Err(format!(
                        "轨迹没有经过 P{}（最近 {:.0}m，要求 ≤15m），拒绝提交——这个点的轨迹被建筑/截断挤掉了",
                        worst_index + 1,
                        worst
                    ));
                }
                log(&format!(
                    "√ [track] 打卡点校验通过：{} 个点全部经过，最大偏差 {:.1}m",
                    checkpoints.len(),
                    worst
                ));
                track
                }
            }
    };
    if let Some(range) = params.manual_altitude_range {
        crate::track::altitude::override_bd_a_range(&mut track, range)?;
        log(&format!("√ [track] 已将海拔曲线映射到 {:.2}-{:.2}m，覆盖 {} 个点，爬升/圈数据将按覆盖值计算", range.min_m, range.max_m, track.locations.len()));
    } else if let Some(altitude_m) = params.manual_altitude {
        crate::track::altitude::override_bd_a(&mut track, altitude_m)?;
        log(&format!("√ [track] 已用手动海拔 {:.2}m 覆盖 {} 个点，爬升/圈数据将按覆盖值计算", altitude_m, track.locations.len()));
    }
    log(&format!(
        "√ [track] {} 点 totalDis={:.0}m steps={} 起点={}",
        track.locations.len(),
        track.totalDistance,
        track.totalSteps,
        chrono::Local.timestamp_millis_opt(params.start_ms).single()
            .map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default(),
    ));

    // ④ 五点 wrapper（跑完态）
    let five = five_point_wrapper(&pts, track.startTime);
    let _ = &five;

    // ⑤ 提交（sportType=1）
    log("[record] 提交跑步记录（sportType=1）…");
    let sp = SubmitParams {
        track,
        uid: sess.uid,
        selected_unid: sess.unid.parse().unwrap_or(0),
        policy: pol.policy,
        policy_ts: pol.timestamp,
        min_distance: pol.min_distance,
        weight: if sess.weight > 0.0 { sess.weight } else { 68.0 },
        face_check: params.face_check,
        five_point_json: five,
        address: client.identity.city.clone(),
    };
    let result = submit_record(client, &sp, log)?;
    sleep_secs(1);

    // ⑥ OBS 上传（双 key）
    log("[obs] 上传 OBS 对象（gzip+base64，10 键）…");
    // 从提交结果回填 track.startTime（含随机秒偏移），保证 body/OBS/flag 全链一致
    let mut track_for_obs = sp.track.clone();
    track_for_obs.startTime = result.start_ms;
    let obj = build_obs_object(&track_for_obs, result.rrid, &result.uuid, sess.uid, &pts);
    let payload = obj.to_string().into_bytes();
    let keys = obs_keys(&track_for_obs, result.rrid, &result.uuid);
    let obs_ok = super::obs::upload_both_keys(client, &keys, &payload, log);
    if obs_ok == 2 {
        log("√ [obs] 双 key 上传成功");
    } else {
        log(&format!("⚠ [obs] 上传成功 {obs_ok}/2"));
    }

    // ⑦ 详情验证
    sleep_secs(2);
    log("[verify] 拉取详情验证…");
    let detail_ok = match fetch_one_record(client, result.rrid) {
        Ok(d) => {
            log(&format!(
                "√ [verify] rrid={} complete={:?} dis={:?} time={:?}",
                result.rrid,
                d.get("complete").and_then(|v| v.as_bool()),
                d.get("totalDis"),
                d.get("totalTime"),
            ));
            if let Ok(mut slot) = VERIFY_DETAIL.lock() {
                slot.replace(d.clone());
            }
            true
        }
        Err(e) => {
            log(&format!("⚠ [verify] 详情拉取失败（提交已成功 rrid={}）：{e}", result.rrid));
            false
        }
    };
    Ok(RunOutcome { result, obs_ok, detail_ok })
}

/// AI 提交流（UI 线程用）。
pub fn run_ai_submit(
    client: &mut ApiClient,
    sport_id: i64,
    mode: super::ai::AiMode,
    log: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    log(&format!("[ai] 提交 sportId={sport_id} mode={mode:?}…"));
    let biz = super::ai::upload(client, sport_id, mode, None)?;
    log("√ [ai] 提交成功");
    Ok(biz)
}

/// AI 列表（UI 线程用）。
pub fn run_ai_list(client: &mut ApiClient, log: &mut dyn FnMut(&str)) -> Result<Vec<super::ai::AiSport>, String> {
    log("[ai] 拉取项目列表…");
    let list = super::ai::fetch_list(client)?;
    log(&format!("√ [ai] {} 个项目", list.len()));
    Ok(list)
}

/// 记录列表（UI 线程用）。
pub fn run_records(client: &mut ApiClient, log: &mut dyn FnMut(&str)) -> Result<Vec<super::records::RecordRow>, String> {
    log("[records] 拉取跑步记录…");
    let rows = super::records::fetch_records(client)?;
    log(&format!("√ [records] {} 条记录", rows.len()));
    Ok(rows)
}

use chrono::TimeZone as _;

/// 最近一次详情验证的完整响应（达标判定明细在 reasonList）。
pub static VERIFY_DETAIL: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
