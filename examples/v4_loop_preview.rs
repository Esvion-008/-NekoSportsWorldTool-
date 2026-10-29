//! 操场轨迹对比生成器：跑「旧 = 过 5 点圆锥曲线椭圆」与「新 = 标准跑道 + 分道 + GPS 噪声」，
//! 各自输出几何采样点与协议轨迹点，供可视化脚本画图对比。
//!
//! 用法：
//! ```text
//! cargo run --release --offline --example v4_loop_preview
//! ```
//! 读 `docs/playground_case.json`，写 `docs/v4_loop_compare.json`。

use greenland::campus::{geo, BlockedArea, Campus, LngLat, PlaygroundArea, Polygon};
use greenland::route::{self, playground, v4};

const CASE: &str = "docs/playground_case.json";
const OUT: &str = "docs/v4_loop_compare.json";
/// 目标里程（与 flow.rs 一致：max(设置里程, 2000)）。
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
    let text = std::fs::read_to_string(CASE).expect("先跑 tools/make_playground_case.py");
    let case: serde_json::Value = serde_json::from_str(&text).expect("用例 JSON 无效");

    let mut campus = Campus::new(ring(&case["campus_area"]), 1);
    campus.allow_shrink_m = 1.0;
    for (index, blocked) in case["blocked"].as_array().expect("blocked 应为数组").iter().enumerate() {
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
    let checkpoints: Vec<LngLat> = case["checkpoints"]
        .as_array()
        .expect("checkpoints 应为数组")
        .iter()
        .map(|point| {
            let pair = point.as_array().unwrap();
            [pair[0].as_f64().unwrap(), pair[1].as_f64().unwrap()]
        })
        .collect();
    let projection = geo::Projection::for_polygon(&campus.area);

    let mut out = serde_json::Map::new();
    out.insert("checkpoints".into(), serde_json::json!(checkpoints));
    out.insert(
        "playground".into(),
        serde_json::json!(case["playground"]),
    );
    out.insert("blocked".into(), serde_json::json!(case["blocked"]));
    out.insert("target_m".into(), serde_json::json!(TARGET_M));

    for (label, seed) in [("new", 20251026u64), ("new_seed2", 777u64)] {
        let (candidate, lap_starts) =
            playground::track_loop_with_laps(&campus, &checkpoints, TARGET_M, seed)
                .expect("新跑道算法应成功");
        let track = route::build_track_points(&campus, &candidate, PACE_SEC_PER_KM, seed, 1_760_000_000_000);
        let fitted = playground::fit_stadium_shrunk(&campus, &campus.playgrounds[0], &projection, 10.0);
        if let Some((stadium, steps)) = fitted {
            println!(
                "[{label}] 跑道 R={radius:.1}m 直道={straight:.1}m 周长={perimeter:.1}m 收缩={steps}",
                radius = stadium.radius,
                straight = stadium.half_straight * 2.0,
                perimeter = stadium.perimeter(),
            );
        }
        let stadium = playground::fit_stadium(&campus.playgrounds[0], &projection);
        out.insert(
            label.into(),
            serde_json::json!({
                "seed": seed,
                "total_m": candidate.total_m,
                "straight_ratio": candidate.straight_ratio,
                "radius": stadium.map(|s| s.radius),
                "half_straight": stadium.map(|s| s.half_straight),
                "fitted_perimeter": fitted.map(|(s, _)| s.perimeter()),
                "shrink_steps": fitted.map(|(_, n)| n),
                "lap_starts": lap_starts,
                "route": candidate.points,
                "track": track,
            }),
        );
        println!(
            "[{label}] seed={seed} 总长 {:.1}m 路线点 {} 轨迹点 {} 圈数 {}",
            candidate.total_m,
            candidate.points.len(),
            track.len(),
            lap_starts.len()
        );
    }

    // 旧算法：过 5 点圆锥曲线椭圆（同一个目标距离）。
    match v4::playground_ellipse(&campus, &checkpoints, TARGET_M) {
        Ok(candidate) => {
            let track = route::build_track_points(&campus, &candidate, PACE_SEC_PER_KM, 1, 1_760_000_000_000);
            println!(
                "[old] 总长 {:.1}m 路线点 {} 轨迹点 {}",
                candidate.total_m,
                candidate.points.len(),
                track.len()
            );
            out.insert(
                "old".into(),
                serde_json::json!({
                    "total_m": candidate.total_m,
                    "route": candidate.points,
                    "track": track,
                }),
            );
        }
        Err(error) => {
            println!("[old] 旧椭圆算法失败：{error}");
            out.insert("old_error".into(), serde_json::json!(error));
        }
    }

    std::fs::write(
        OUT,
        serde_json::to_string(&serde_json::Value::Object(out)).expect("序列化"),
    )
    .expect("写对比 JSON");
    println!("写入 {OUT}");
}
