//! 干跑一遍 flow.rs 的两条 V4 分支（操场 / 校园），不需要登录、不联网。
//!
//! ```text
//! cargo run --release --offline --example flow_dryrun
//! ```
//! 用来确认「软件里操场和校园两条路都走了新算法」——打印的日志和 GUI 后台线程一致。

use greenland::campus::{geo, BlockedArea, Campus, LngLat, PlaygroundArea, Polygon};
use greenland::route::{self, playground, v4, v3};

const CASE: &str = "docs/sample/playground_case.json";
const TARGET_M: f64 = 2000.0;
const PACE_SEC_PER_KM: f64 = 400.0;

fn ring(value: &serde_json::Value) -> Polygon {
    let outer = value
        .as_array()
        .expect("多边形应为数组")
        .iter()
        .map(|point| {
            let pair = point.as_array().expect("坐标应为数组");
            [pair[0].as_f64().unwrap(), pair[1].as_f64().unwrap()]
        })
        .collect::<Vec<LngLat>>();
    Polygon::new(outer)
}

fn main() {
    let text = std::fs::read_to_string(CASE).expect("缺少示例用例 docs/sample/playground_case.json");
    let case: serde_json::Value = serde_json::from_str(&text).expect("用例 JSON 无效");
    let mut campus = Campus::new(ring(&case["campus_area"]), 1);
    campus.allow_shrink_m = 1.0;
    for (index, blocked) in case["blocked"].as_array().unwrap().iter().enumerate() {
        campus.blocked.push(BlockedArea {
            id: index as u64 + 1,
            name: format!("建筑{}", index + 1),
            polygon: ring(blocked),
        });
    }
    campus.playgrounds.push(PlaygroundArea {
        id: 1,
        name: "田径场".into(),
        polygon: ring(&case["playground"]),
    });
    let projection = geo::Projection::for_polygon(&campus.area);
    let seed = 20251027u64;

    // ① 操场分支：5 个点都在操场里
    let playground_tasks: Vec<LngLat> = case["checkpoints"]
        .as_array()
        .unwrap()
        .iter()
        .map(|point| {
            let pair = point.as_array().unwrap();
            [pair[0].as_f64().unwrap(), pair[1].as_f64().unwrap()]
        })
        .collect();
    // ② 校园分支：5 个点分散在校区（不在操场）
    let campus_tasks: Vec<LngLat> = vec![
        [114.302311, 30.497511],
        [114.301018, 30.498315],
        [114.302801, 30.499035],
        [114.304046, 30.498135],
        [114.301245, 30.499410],
    ];

    for (label, tasks) in [("操场点位", &playground_tasks), ("校园点位", &campus_tasks)] {
        let is_playground = v3::is_playground_in_campus(&campus, tasks, 180.0);
        println!("\n===== {label}：is_playground_in_campus = {is_playground} =====");
        if is_playground {
            let dist = TARGET_M + (seed % 101) as f64;
            println!(
                "[route] V4 操场模式：{} 个点全部在操场，按标准跑道几何绕圈（1.22m/道、圈间换道），目标 {:.0}m，种子 {}",
                tasks.len(),
                dist,
                seed
            );
            match playground::track_loop(&campus, tasks, dist, seed) {
                Ok(candidate) => {
                    println!(
                        "√ [route] V4 操场轨迹：穿过 {} 个打卡点，总长 {:.1}m，{} 个路线点，直道 {:.0}% / 弯道 {:.0}%",
                        candidate.checkpoints.len(),
                        candidate.total_m,
                        candidate.points.len(),
                        candidate.straight_ratio * 100.0,
                        candidate.curve_ratio * 100.0
                    );
                    let track = route::build_track_points(&campus, &candidate, PACE_SEC_PER_KM, seed, 1_760_000_000_000);
                    println!("   → 协议轨迹 {} 点（含 GPS 噪声）", track.len());
                }
                Err(error) => println!("✗ 操场模式失败：{error}"),
            }
        } else {
            println!(
                "[route] V4 校园路线：{} 个打卡点，严格 P1→P5 顺序，A* 避建筑 + 重复惩罚绕行（走过的格子加价，回程不原路重叠），种子 {}",
                tasks.len(),
                seed
            );
            match v4::generate(&campus, tasks, TARGET_M, seed) {
                Ok(candidate) => {
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
                    println!(
                        "√ [route] V4 校园路线：总长 {:.1}m，{} 个路线点，拐角 {} 个，重复路段 {:.0}%（旧算法是 100%）",
                        candidate.total_m,
                        candidate.points.len(),
                        candidate.corners,
                        if sampled > 0 { 100.0 * dup as f64 / sampled as f64 } else { 0.0 }
                    );
                    let track = route::build_track_points(&campus, &candidate, PACE_SEC_PER_KM, seed, 1_760_000_000_000);
                    println!("   → 协议轨迹 {} 点（含 GPS 噪声）", track.len());
                }
                Err(error) => println!("✗ 校园模式失败：{error}"),
            }
        }
    }
}
