use greenland::route::v3::{generate, generate_tasks, V3Rules};

fn write_route(path: &str, points: &[[f64; 2]], tasks: &[[f64; 2]], metrics: &str) {
    let mut text = format!("{{\"type\":\"Feature\",\"properties\":{{{}}},\"geometry\":{{\"type\":\"LineString\",\"coordinates\":[", metrics);
    for (index, point) in points.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        text.push_str(&format!("[{:.8},{:.8}]", point[0], point[1]));
    }
    text.push_str("]}}");
    std::fs::write(path, text).unwrap();
    let mut task_text = String::from("{\"tasks\":[");
    for (index, task) in tasks.iter().enumerate() {
        if index > 0 {
            task_text.push(',');
        }
        task_text.push_str(&format!("[{:.8},{:.8}]", task[0], task[1]));
    }
    task_text.push_str("]}");
    std::fs::write(path.replace("route", "tasks"), task_text).unwrap();
}

/// 验证点由环境变量提供：`ESVIM_V3_POINTS="lng,lat;lng,lat;…"`，
/// 取值要落在本地 `campus.json` 的校园范围内；未设置时整条用例跳过。
/// 这样仓库里不需要写死任何真实校园坐标。
fn fixed_points() -> Option<Vec<[f64; 2]>> {
    let raw = std::env::var("ESVIM_V3_POINTS").ok()?;
    let mut points = Vec::new();
    for item in raw.split(';').filter(|item| !item.trim().is_empty()) {
        let mut parts = item.split(',');
        let lng: f64 = parts.next()?.trim().parse().ok()?;
        let lat: f64 = parts.next()?.trim().parse().ok()?;
        points.push([lng, lat]);
    }
    (points.len() >= 2).then_some(points)
}

/// 产物写到系统临时目录，避免把开发机路径写进仓库。
fn out_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("greenland-v3");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

#[test]
fn v3_fixed_and_generated_multi_seed() {
    let Ok(Some(campus)) = greenland::campus::store::load() else {
        eprintln!("跳过：本地没有 campus.json 验证数据");
        return;
    };
    if campus.roads.len() < 2 {
        eprintln!("跳过：当前 campus.json 没有足够路网（roads={}），无法验证道路 V3", campus.roads.len());
        return;
    }
    let Some(fixed) = fixed_points() else {
        eprintln!("跳过：未设置 ESVIM_V3_POINTS（格式 \"lng,lat;lng,lat;…\"，取本地 campus.json 内的验证点）");
        return;
    };
    let seeds = [12345u64, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    let begin = std::time::Instant::now();
    let mut generated_ok = 0usize;
    for seed in seeds {
        let rules = V3Rules {
            seed,
            ..Default::default()
        };
        let fixed_begin = std::time::Instant::now();
        match generate(&campus, &fixed, rules) {
            Ok(candidates) => {
                let first = &candidates[0];
                println!(
                    "FIXED seed={} ok n={} t={}ms dist={:.1} p5={:.1} tail={:.1} road={:.1}% off={:.1} repeat={:.1} back={} turns={} clear={:.1} score={:.1}",
                    seed,
                    candidates.len(),
                    fixed_begin.elapsed().as_millis(),
                    first.distance_m,
                    first.p5_distance_m,
                    first.tail_distance_m,
                    first.road_coverage * 100.0,
                    first.offroad_m,
                    first.repeated_road_m,
                    first.immediate_backtracks,
                    first.turns,
                    first.min_clearance_m,
                    first.score
                );
                if seed == 12345 {
                    let metrics = format!(
                        "\"distance_m\":{:.2},\"p5_m\":{:.2},\"tail_m\":{:.2},\"road_coverage\":{:.4},\"offroad_m\":{:.2},\"repeated_m\":{:.2},\"backtracks\":{},\"turns\":{},\"score\":{:.2}",
                        first.distance_m,
                        first.p5_distance_m,
                        first.tail_distance_m,
                        first.road_coverage,
                        first.offroad_m,
                        first.repeated_road_m,
                        first.immediate_backtracks,
                        first.turns,
                        first.score
                    );
                    write_route(&out_dir().join("v3_fixed_route.json").to_string_lossy(), &first.points, &fixed, &metrics);
                }
            }
            Err(error) => println!(
                "FIXED seed={} FAIL t={}ms reason={}",
                seed,
                fixed_begin.elapsed().as_millis(),
                error
            ),
        }

        let gen_begin = std::time::Instant::now();
        match generate_tasks(&campus, 5, rules).and_then(|tasks| {
            generate(&campus, &tasks, rules).map(|candidates| (tasks, candidates))
        }) {
            Ok((tasks, candidates)) => {
                generated_ok += 1;
                let first = &candidates[0];
                println!(
                    "GEN seed={} ok n={} t={}ms tasks={:?} dist={:.1} p5={:.1} tail={:.1} road={:.1}% off={:.1} repeat={:.1} back={} turns={} clear={:.1} score={:.1}",
                    seed,
                    candidates.len(),
                    gen_begin.elapsed().as_millis(),
                    tasks.iter().map(|t| [t[0], t[1]]).collect::<Vec<_>>(),
                    first.distance_m,
                    first.p5_distance_m,
                    first.tail_distance_m,
                    first.road_coverage * 100.0,
                    first.offroad_m,
                    first.repeated_road_m,
                    first.immediate_backtracks,
                    first.turns,
                    first.min_clearance_m,
                    first.score
                );
                if seed == 12345 {
                    let again = generate(&campus, &tasks, rules).unwrap();
                    assert_eq!(first.points, again[0].points, "同 seed 路线不一致");
                    println!("GEN seed=12345 reproducible");
                    let metrics = format!(
                        "\"distance_m\":{:.2},\"p5_m\":{:.2},\"tail_m\":{:.2},\"road_coverage\":{:.4},\"offroad_m\":{:.2},\"repeated_m\":{:.2},\"backtracks\":{},\"turns\":{},\"score\":{:.2}",
                        first.distance_m,
                        first.p5_distance_m,
                        first.tail_distance_m,
                        first.road_coverage,
                        first.offroad_m,
                        first.repeated_road_m,
                        first.immediate_backtracks,
                        first.turns,
                        first.score
                    );
                    write_route(&out_dir().join("v3_generated_route.json").to_string_lossy(), &first.points, &tasks, &metrics);
                }
            }
            Err(error) => println!(
                "GEN seed={} FAIL t={}ms reason={}",
                seed,
                gen_begin.elapsed().as_millis(),
                error
            ),
        }
    }
    println!("total elapsed {:?}", begin.elapsed());
    assert_eq!(generated_ok, seeds.len(), "生成任务多 seed 并非全部成功");
}
