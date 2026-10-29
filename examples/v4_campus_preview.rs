//! 校园（非操场）路线对比生成器：跑一遍 V4 校园算法，输出几何 + 协议轨迹点。
//!
//! 用法：
//! ```text
//! cargo run --release --offline --example v4_campus_preview
//! ```
//! 读 `docs/playground_case.json`（校园边界 + 建筑），写 `docs/v4_campus_compare.json`。

use greenland::campus::{geo, BlockedArea, Campus, LngLat, Polygon};
use greenland::route::{self, v4};

const CASE: &str = "docs/playground_case.json";
const OUT: &str = "docs/v4_campus_compare.json";
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
    let projection = geo::Projection::for_polygon(&campus.area);

    // 5 个「不在操场内」的打卡点：分散在校区各角落（真实校园跑的形态）。
    let tasks: Vec<LngLat> = vec![
        [114.302311, 30.497511],
        [114.301018, 30.498315],
        [114.302801, 30.499035],
        [114.304046, 30.498135],
        [114.301245, 30.499410],
    ];

    let mut out = serde_json::Map::new();
    out.insert("points".into(), serde_json::json!(tasks));
    out.insert("target_m".into(), serde_json::json!(TARGET_M));
    out.insert(
        "blocked".into(),
        serde_json::json!(case["blocked"]),
    );
    out.insert(
        "campus_area".into(),
        serde_json::json!(case["campus_area"]),
    );

    for (label, seed) in [("campus_a", 20251027u64), ("campus_b", 90210u64)] {
        match v4::generate(&campus, &tasks, TARGET_M, seed) {
            Ok(candidate) => {
                let track = route::build_track_points(&campus, &candidate, PACE_SEC_PER_KM, seed, 1_760_000_000_000);
                // 采样点离校区边界的最近距离（判断有没有贴围墙）
                let min_clear = candidate
                    .points
                    .iter()
                    .map(|p| geo::distance_to_polygon_boundary(*p, &campus.area, &projection))
                    .fold(f64::INFINITY, f64::min);
                println!(
                    "[{label}] seed={seed} 总长 {:.0}m 路线点 {} 轨迹点 {} 拐角 {} 最近围墙 {:.1}m",
                    candidate.total_m,
                    candidate.points.len(),
                    track.len(),
                    candidate.corners,
                    min_clear
                );
                out.insert(
                    label.into(),
                    serde_json::json!({
                        "seed": seed,
                        "total_m": candidate.total_m,
                        "corners": candidate.corners,
                        "route": candidate.points,
                        "track": track,
                    }),
                );
            }
            Err(error) => {
                println!("[{label}] 生成失败：{error}");
                out.insert(format!("{label}_error"), serde_json::json!(error));
            }
        }
    }

    std::fs::write(OUT, serde_json::to_string(&serde_json::Value::Object(out)).expect("序列化"))
        .expect("写对比 JSON");
    println!("写入 {OUT}");
}
