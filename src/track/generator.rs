//! 自然轨迹生成器。
//!
//! 画像驱动：打卡点拟合闭合环（Catmull-Rom，每段 18 采样）→ 弧长表；
//! 采样间隔主 5s（80%）；速度曲线 = ramp × 疲劳 × 三正弦 × 余弦凹陷 × 微噪；
//! 正常点按速度曲线分配位移并归一到精确总距离；异常点（-1）零位移/跳变；
//! 哨兵点（首 type∈{0,7}、索引1 type=5、末 type=6）；结尾断崖；点位吸附。
#![allow(non_snake_case)]

use super::geom::{fmt_gain_time, make_point_ring, ring_point_at, round_to, to_bd, Rng, MET_PER_DEG_LAT, MET_PER_DEG_LNG};
use super::model::{GenPoint, Segment, TenWindow, Track};
use super::postfix::apply_post_fixes;

/// 有效配速窗口（判定规则 2'21"-10'00"/km ≈ 1.667-7.092 m/s），硬边界留余量。
pub const SPEED_FLOOR: f64 = 1.10;
/// 手持手机的高频抖动幅度（米，标准差）。真人每次记录的相邻点海拔差都在 1m 上下，
/// 只做低频 OU 会画出一条平滑曲线，一看就不是「手机在手里」。
pub const ALT_JITTER_M: f64 = 0.75;
/// 海拔允许偏离基准的最大幅度（米）。示例基准 119.5m → 区间 117–122m。
pub const ALT_BAND_M: f64 = 2.5;

/// 按经纬度粗略估基准海拔（米）。
///
/// 原来基准硬编码 82m，在北方城市（真实海拔 116–122m）生成的轨迹海拔整整差 40m，
/// 和 App 里其它记录一比就露馅。手机 GPS 高度本身误差就有 ±20–50m，
/// 所以这里只求「量级对」，按大区给经验值即可。
pub fn estimate_altitude(lat: f64, lng: f64) -> f64 {
    if !(15.0..=55.0).contains(&lat) || !(70.0..=140.0).contains(&lng) {
        return 82.0; // 不在中国范围：保留原来的中立值
    }
    // 东北地区：100–160
    if lat >= 40.0 && lng >= 118.0 {
        return 119.5;
    }
    // 西北（陕甘宁青新）：800–2500
    if lat >= 32.0 && lng < 110.0 {
        return 1100.0;
    }
    // 华北平原（京津冀鲁豫）：20–80
    if lat >= 35.0 {
        return 45.0;
    }
    // 西南（云贵川渝）：500–2000
    if (98.0..110.0).contains(&lng) {
        return 900.0;
    }
    // 华中（鄂湘赣皖）：40–120
    if (108.0..116.0).contains(&lng) {
        return 60.0;
    }
    // 华东（沪苏浙）：3–40
    if lat >= 27.0 {
        return 14.0;
    }
    // 华南（粤桂闽琼）：10–80
    35.0
}
pub const SPEED_CEIL: f64 = 6.30;

/// 等比缩放逐点速度至目标总距：越界点钳在窗口边界，剩余差量由未饱和点分摊（迭代收敛）。
/// 与整体等比缩放的区别：任何一点的瞬时配速都不会越出有效窗口。
fn fit_speeds(w: &mut [f64], dts: &[f64], target: f64) {
    for _ in 0..24 {
        let cur: f64 = w.iter().zip(dts).map(|(x, dt)| x * dt).sum();
        if (cur - target).abs() <= 1.0 {
            break;
        }
        let k = target / cur;
        for x in w.iter_mut() {
            *x = (*x * k).clamp(SPEED_FLOOR, SPEED_CEIL);
        }
    }
    for x in w.iter_mut() {
        *x = x.clamp(SPEED_FLOOR, SPEED_CEIL);
    }
}

/// 轨迹生成主入口。points_bd 为 BD 系打卡点。
pub fn build(
    dist: f64,
    dur: i64,
    seed: u64,
    center: (f64, f64),
    start_ms: i64,
    points_bd: &[(f64, f64)],
) -> Track {
    let mut rng = Rng::new(seed);
    let dur_f = dur as f64;
    let (dense, arcs, pc) = make_point_ring(points_bd);
    let (c_lat, c_lng) = pc;
    let direction: f64 = rng.choice(&[1.0, -1.0]);
    // 打卡点规则：必须从第一个打卡点起跑（环原点已对齐第一个点），
    // 之后的环绕方向 / 速度曲线 / 抖动 / 海拔全部随机。
    let s0 = 0.0f64;
    let phase_v = rng.uniform(0.0, std::f64::consts::TAU);
    let phase_l = rng.uniform(0.0, std::f64::consts::TAU);

    let mut times = Vec::new();
    let mut t = 0.0;
    while t < dur_f {
        times.push(t);
        t += if rng.random() < 0.80 {
            5.0
        } else {
            rng.choice(&[1.0, 2.0, 3.0, 4.0, 6.0, 7.0, 8.0])
        };
    }
    let n = times.len();

    // 「走一段」：1–2 次，持续 40–90 秒，速度降到 38–55%（配速 12'–18'）。
    // 用余弦核过渡，进出都是平滑的，不会出现配速断崖。
    let n_dips = rng.choice(&[1u32, 1, 2]);
    let mut dips = Vec::new();
    for _ in 0..n_dips {
        dips.push((
            rng.uniform(0.12, 0.80) * dur_f,
            rng.uniform(20.0, 45.0),
            rng.uniform(0.38, 0.55),
        ));
    }
    let dip_factor = |t: f64| -> f64 {
        let mut f = 1.0;
        for &(c, hw, d) in &dips {
            if (t - c).abs() < hw {
                f *= 1.0 - d * 0.5 * (1.0 + std::f64::consts::PI * (t - c) / hw).cos();
            }
        }
        f
    };
    let base = dist / dur_f;
    let mut w = Vec::with_capacity(n);
    for &tt in &times {
        let ramp = if tt < 8.0 {
            (0.85 + 0.15 * (tt / 1.0f64.max(8.0f64.min(dur_f / 20.0)))).min(1.0)
        } else {
            1.0
        } * if tt > dur_f - 8.0 {
            1.0 + 0.03 * (tt - (dur_f - 8.0)) / 8.0
        } else {
            1.0
        };
        let km_done = (tt / dur_f) * dist / 1000.0;
        let fatigue = if km_done <= 0.4 { 1.03 } else { (1.03 - 0.11 * (km_done - 0.4)).max(0.74) };
        let wave = (1.0 + 0.020 * (std::f64::consts::TAU * tt / 115.0 + phase_v).sin()
            + 0.050 * (std::f64::consts::TAU * tt / 47.0 + phase_v * 2.3).sin()
            + 0.022 * (std::f64::consts::TAU * tt / 19.0 + phase_v * 3.7).sin())
            * dip_factor(tt);
        let noise = 1.0 + rng.gauss(0.0, 0.008);
        w.push(base * ramp * fatigue * wave * noise);
    }
    let mut dts: Vec<f64> = (0..n - 1).map(|i| times[i + 1] - times[i]).collect();
    dts.push(1.0f64.max(dur_f - times[n - 1]));
    // 逐点速度全部约束在有效配速窗口内，并精确命中目标距离
    fit_speeds(&mut w, &dts, dist);
    let seg_dist: Vec<f64> = (0..n).map(|i| w[i] * dts[i]).collect();
    let speeds: Vec<f64> = w.clone();

    let tl_w = [((-1i64, 4i64), 54u32), ((-1, 1), 27), ((-1, 12), 13), ((-1, 5), 3), ((-1, 6), 2)];
    let mut kinds: Vec<(i64, i64)> = Vec::with_capacity(n);
    for _ in 0..n {
        let u = rng.random();
        if u < 0.39 {
            kinds.push((3, 1));
        } else if u < 0.93 {
            kinds.push((0, 1));
        } else if u < 0.96 {
            kinds.push(rng.weighted(&tl_w));
        } else {
            kinds.push((rng.choice(&[1, 1, 1, 1, 2, 2]), 1));
        }
    }
    for i in 1..kinds.len() {
        let prev_drift = (-1 == kinds[i - 1].0) || kinds[i - 1].0 == 5 || kinds[i - 1].0 == 6;
        if kinds[i].0 != -1 && prev_drift && rng.random() < 0.08 {
            kinds[i] = (if rng.random() < 0.75 { 7 } else { 8 }, 1);
        }
    }
    for i in 1..n {
        if kinds[i].0 == -1 && kinds[i - 1].0 == -1 {
            kinds[i] = (rng.choice(&[3, 0]), 1);
        }
    }
    let normal_idx: Vec<usize> =
        (0..n).filter(|&i| kinds[i].0 != -1 && i > 1).collect();
    let share: f64 = normal_idx.iter().map(|&i| seg_dist[i]).sum();
    let share = if share == 0.0 { 1.0 } else { share };
    let mut disp_of = vec![0.0f64; n];
    for &i in &normal_idx {
        disp_of[i] = seg_dist[i] * (dist / share);
    }

    let mut locs: Vec<GenPoint> = Vec::with_capacity(n);
    let mut s = s0;
    let mut t_acc = 0.0f64;
    let mut dist_acc = 0.0f64;
    let mut steps_acc = 0.0f64;
    let mut jx = 0.0f64;
    let mut jy = 0.0f64;
    // 基准海拔跟经纬度走，别再一律 82m
    let base_alt = estimate_altitude(center.0, center.1);
    // 真人的海拔曲线是「密集锯齿 + 缓慢起伏」，不是一条平滑曲线：
    // 手机拿在手里每跑一步都上下颠 1–2m，所以相邻点的 bdA 差就有 1m 上下。
    // 这里拆成两层：alt_drift = 低频地形起伏（OU），ALT_JITTER_M = 每点独立的手持抖动。
    let mut alt_drift = 0.0f64;
    let n_est = 1.max((dur_f / 5.0) as i64);
    // 真人海拔轨迹是 1.3–6m 的锯齿（GPS 高度噪声），不是一条平滑直线。
    // OU 稳态标准差 ≈ sigma/sqrt(2*0.04)，要 1.5m 左右就把 sigma 提到 0.4 上下。
    let alt_sigma = rng.uniform(10.0, 15.0) / (0.40 * n_est as f64).max(1.0);
    // 步频掉档状态：真人会停下来等红灯/系鞋带，一次持续十几秒，
    // 不是单个点掉一下（那样会被 10 秒窗平均掉，最小值根本下不来）。
    let mut cad_drop_left = 0.0f64;
    let mut cad_drop_factor = 1.0f64;
    let mut ten_t = 0.0f64;
    // 10 秒窗距离必须用「实际加的噪声坐标」算：真人 App 的 speedPerTenSec 就是
    // 相邻 GPS 点的直线距离累计，抖动起来能从 4m/10s 跳到 80m/10s。
    // 用计划速度×dt 只会得到一条几乎直线的曲线（和真人差一个量级）。
    let mut ten_d = 0.0f64;
    let mut ten_st = 0.0f64;
    let (mut prev_px, mut prev_py, mut has_prev) = (0.0f64, 0.0f64, false);
    let mut ten_speed: Vec<TenWindow> = Vec::new();
    let mut ten_steps: Vec<TenWindow> = Vec::new();

    for i in 0..n {
        let dt = dts[i];
        let (typ, lt) = kinds[i];
        t_acc += dt;
        let pos = |ss: f64| ring_point_at(&dense, &arcs, ss);
        let mut d_step = 0.0f64;
        let px;
        let py;
        let x;
        let y;
        let rad;
        let state;
        if typ != -1 {
            d_step = disp_of[i];
            s += direction * d_step;
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            jx = 0.72 * jx + rng.gauss(0.0, 0.75);
            jy = 0.72 * jy + rng.gauss(0.0, 0.75);
            px = bx + jx;
            py = by + jy;
            rad = round_to(
                if typ == 3 { rng.uniform(1.4, 5.1) } else { rng.uniform(1.4, 2.4) },
                2,
            );
            state = if typ == 0 {
                rng.weighted(&[(1, 145), (2, 45), (3, 164)])
            } else {
                rng.weighted(&[(1, 145), (2, 256), (3, 151)])
            };
        } else {
            match lt {
                4 => {
                    if rng.random() >= 0.68 {
                        // 真人 GPS 抽风的跳变量级是 30–60m，原来上到 250m，
                        // 10 秒窗能凑出 100m+，配速直接冲到 1'30"（比世界纪录还快）。
                        d_step = if rng.random() < 0.95 { rng.uniform(2.0, 25.0) } else { rng.uniform(25.0, 45.0) };
                    }
                }
                1 => {
                    if rng.random() >= 0.83 {
                        d_step = rng.uniform(0.5, 36.0);
                    }
                }
                12 => {
                    d_step = if rng.random() < 0.9 { rng.uniform(5.0, 28.0) } else { rng.uniform(28.0, 50.0) };
                }
                5 => d_step = rng.uniform(5.0, 60.0),
                _ => d_step = rng.uniform(100.0, 300.0),
            }
            let (bx, by) = pos(s);
            x = bx;
            y = by;
            if d_step > 0.0 {
                let ang = rng.uniform(0.0, std::f64::consts::TAU);
                px = x + d_step * ang.sin();
                py = y + d_step * ang.cos();
            } else if let Some(last) = locs.last() {
                // 零位移：精确复制上一点坐标（BD→平面）
                py = (last.gLat - c_lat) * MET_PER_DEG_LAT;
                px = (last.gLng - c_lng) * MET_PER_DEG_LNG;
            } else {
                px = x + jx;
                py = y + jy;
            }
            rad = if lt == 4 {
                round_to(if rng.random() < 0.75 { rng.uniform(30.0, 100.0) } else { rng.uniform(100.0, 550.0) }, 2)
            } else if lt == 1 {
                round_to(if rng.random() < 0.75 { rng.uniform(1.6, 12.0) } else { rng.uniform(12.0, 95.0) }, 2)
            } else if lt == 12 {
                round_to(rng.uniform(30.0, 125.0), 2)
            } else if lt == 5 {
                round_to(rng.uniform(25.0, 300.0), 2)
            } else {
                550.0
            };
            state = rng.weighted(&[(1, 102), (2, 124), (3, 136)]);
        }
        if typ != -1 {
            dist_acc += d_step; // 异常点漂移不计入累计距离
        }
        let (lat, lng) = to_bd(px, py, c_lat, c_lng);
        alt_drift += 0.04 * (0.0 - alt_drift) + rng.gauss(0.0, alt_sigma);
        // 真人样本上限 122、下限 116–117：整体硬钳在基准 ±2.5m 之内，
        // 免得极值飘到 124 或者掉到 116 以下（App 记录里看不到这种数）。
        let alt = (base_alt + alt_drift + rng.gauss(0.0, ALT_JITTER_M))
            .clamp(base_alt - ALT_BAND_M, base_alt + ALT_BAND_M);
        let v_now = dist / dur_f;
        let stride_target = 0.62 + 0.17 * v_now
            + 0.03 * (std::f64::consts::TAU * t_acc / 200.0 + phase_l).sin()
            + rng.gauss(0.0, 0.008);
        // 真人步频不是「速度/步幅」算出来的匀速机器：它自己有慢波 + 噪声，
        // 还会偶发掉档（等红灯、系鞋带、走一段）。所以柱状图在 100–170 之间跳。
        let v_cad = v_now
            * (1.0
                + 0.12 * (std::f64::consts::TAU * t_acc / 70.0 + phase_v).sin()
                + 0.05 * (std::f64::consts::TAU * t_acc / 23.0 + phase_v * 2.9).sin());
        // 掉档：每点 0.4% 概率触发一次，持续 6–20 秒，幅度 28–62%。
        // 真人记录里步频最小值 18–42（就是停下来了），主体 100–160，最大 168–198。
        if cad_drop_left <= 0.0 && rng.random() < 0.012 {
            cad_drop_left = rng.uniform(6.0, 20.0);
            cad_drop_factor = rng.uniform(0.28, 0.62);
        }
        let cad_drop = if cad_drop_left > 0.0 {
            cad_drop_left -= dt;
            cad_drop_factor
        } else {
            1.0
        };
        let cad = (v_cad / stride_target * 60.0 * cad_drop + rng.gauss(0.0, 3.0))
            .clamp(40.0, 180.0);
        steps_acc += cad / 60.0 * dt;
        let nxt = pos(s + direction * 2.0);
        let brg = ((nxt.0 - x).atan2(nxt.1 - y).to_degrees() + rng.gauss(0.0, 35.0)).rem_euclid(360.0);
        // 异常点（-1）：avgSpeed 为累计均值（真人与此一致，不为 0）；
        // GPS 瞬时速度多为低速，偶发 15-46 km/h 漂移尖峰
        let (avg_sp, gps_speed) = if typ == -1 {
            let avg = round_to(dist_acc / t_acc.max(1.0), 4);
            let gps = if rng.random() < 0.12 {
                rng.uniform(15.0, 46.0)
            } else {
                rng.uniform(0.5, 6.0)
            };
            (avg, round_to(gps, 4))
        } else {
            let avg = round_to(d_step / dt, 4);
            let kmh = avg * 3.6;
            let sigma = (kmh * 0.08).max(0.05);
            let gps = if rng.random() < 0.20 {
                0.0
            } else {
                round_to((kmh + rng.gauss(0.0, sigma)).max(0.0), 4)
            };
            (avg, gps)
        };

        ten_t += dt;
        let moved = if has_prev {
            ((px - prev_px).powi(2) + (py - prev_py).powi(2)).sqrt()
        } else {
            0.0
        };
        prev_px = px;
        prev_py = py;
        has_prev = true;
        ten_d += moved;
        ten_st += cad / 60.0 * dt;
        while ten_t >= 10.0 {
            let k = 10.0 / ten_t; // 只取前 10s 的量，余量留给下一窗
            let (out_d, out_st) = (ten_d * k, ten_st * k);
            // 下限 4.5m/10s ≈ 配速 37'（真人最慢 40'），上限 82m/10s = 2'02"（真人最快）。
            // 上限 68m/10s ≈ 配速 2'27"：这个速度配上 230cm 步幅刚好 177 步/分，
            // 和「步频最大 160 上下」对得上；再快就得 200+ 步频，用户明确不要。
                        // 下限 9m/10s ≈ 配速 18'30"：真人「走一段」最快也就 12'–15'，
            // 再慢就不是走、是站着了，曲线会掉到 30' 以上。
            let distance = out_d.clamp(9.0, 45.0);
            // 步幅 = 距离 / 步数。真人记录里步幅落在 45–250cm（平均 ~105），
            // 距离和步数各自漂移后会凑出 5m 或 6m 的步幅，这里按步幅反算步数。
            let mut steps_window = out_st;
            let stride = distance / steps_window.max(0.1);
            if stride > 2.30 {
                steps_window = distance / 2.30;
            } else if stride < 0.50 {
                steps_window = distance / 0.50;
            }
            ten_speed.push(TenWindow { time: 10, value: round_to(distance, 2) });
            ten_steps.push(TenWindow { time: 10, value: round_to(steps_window, 0) });
            ten_t -= 10.0;
            ten_d -= out_d;
            ten_st -= out_st;
        }
        locs.push(GenPoint {
            id: i as i64 + 1,
            flag: start_ms,
            lat: -1.0,
            lng: -1.0,
            gLat: round_to(lat, 7),
            gLng: round_to(lng, 7),
            speed: round_to(gps_speed, 4),
            avgSpeed: avg_sp,
            radius: rad,
            accuracy: rad,
            ptype: typ,
            locType: lt,
            hasAltitude: true,
            totalTime: round_to(t_acc, 0) as i64,
            totalDis: round_to(dist_acc, 4),
            validDis: round_to(dist_acc, 4),
            validTime: round_to(t_acc, 0) as i64,
            steps: steps_acc as i64,
            stepDistance: 0.0,
            gainTime: fmt_gain_time(start_ms + (t_acc * 1000.0) as i64),
            gainTimeMs: start_ms + (t_acc * 1000.0) as i64,
            queueNum: 0,
            coorType: "gcj02".into(),
            bdA: round_to(alt, 2),
            bdD: round_to(brg, 2),
            bdS: round_to((avg_sp * rng.uniform(0.6, 0.95)).max(0.0), 3),
            bdG: rng.choice(&[1, 1, 1, -1]),
            count: rng.randint(20, 88),
            dtr: 0.0,
            state,
            locationId: String::new(),
        });
    }
    // 配速曲线平滑：相邻 10 秒窗的距离变化限制在 0.62–1.60 倍。
    // 真人从跑切换到走是几十秒渐变，不会在一两个窗口里把配速翻三倍。
    for index in 1..ten_speed.len() {
        let previous = ten_speed[index - 1].value;
        let value = ten_speed[index]
            .value
            .clamp(previous * 0.62, previous * 1.60);
        ten_speed[index].value = round_to(value, 2);
    }
    if ten_t > 1.0 {
        // 尾窗：限制放大倍数 ≤1.4
        let k = (10.0 / ten_t).min(1.4);
        ten_speed.push(TenWindow { time: 10, value: round_to(ten_d * k, 2) });
        ten_steps.push(TenWindow { time: 10, value: round_to(ten_st * k, 0) });
    }

    let mut segments: Vec<Segment> = Vec::new();
    let (mut seg_t, mut seg_d, mut seg_n) = (0.0f64, 0.0f64, 0i64);
    let mut seg_v: Vec<f64> = Vec::new();
    for i in 0..n {
        seg_t += dts[i];
        seg_d += seg_dist[i];
        seg_v.push(speeds[i]);
        seg_n += 1;
        if seg_t >= 60.0 || i == n - 1 {
            segments.push(Segment {
                totalTime: round_to(seg_t, 0) as i64,
                distance: round_to(seg_d, 0) as i64,
                startTime: round_to((times[i] - seg_t) * 1000.0, 0) as i64,
                endTime: round_to(times[i] * 1000.0, 0) as i64,
                avgSpeed: round_to(seg_v.iter().sum::<f64>() / seg_v.len() as f64, 3),
                avgStep: round_to(steps_acc / 1.0f64.max(t_acc) * 60.0, 0) as i64,
                state: 0,
            });
            seg_t = 0.0;
            seg_d = 0.0;
            seg_v.clear();
            seg_n = 0;
        }
    }
    let _ = seg_n;

    apply_post_fixes(&mut locs, &mut rng, start_ms);

    // 点位吸附：<40m 精确落位
    for pl in points_bd {
        let mut best_i = None;
        let mut best_d = 1e18f64;
        for (i, q) in locs.iter().enumerate() {
            let dd = ((q.gLat - pl.0) * MET_PER_DEG_LAT).powi(2)
                + ((q.gLng - pl.1) * MET_PER_DEG_LNG).powi(2);
            if dd < best_d {
                best_d = dd;
                best_i = Some(i);
            }
        }
        if let Some(i) = best_i {
            if best_d < 40.0 * 40.0 {
                locs[i].gLat = round_to(pl.0, 7);
                locs[i].gLng = round_to(pl.1, 7);
            }
        }
    }

    let total_dis = round_to(dist, 3);
    Track {
        totalTime: round_to(t_acc, 0) as i64,
        totalDistance: total_dis,
        validDistance: total_dis,
        validTime: round_to(t_acc, 0) as i64,
        startTime: start_ms,
        startLatitude: locs[0].gLat,
        startLongitude: locs[0].gLng,
        totalSteps: steps_acc as i64,
        locations: locs,
        speedPerTenSec: ten_speed,
        stepsPerTenSec: ten_steps,
        segments,
    }
}

#[cfg(test)]
mod fidelity_tests {
    use super::*;

    fn pace(sec_per_km: f64) -> String {
        if !sec_per_km.is_finite() || sec_per_km <= 0.0 {
            return "--".into();
        }
        let total = sec_per_km.round() as i64;
        format!("{}'{:02}\"", total / 60, total % 60)
    }

    #[test]
    fn altitude_estimate_matches_city_scale() {
        // 北方城市样本在 116–122m
        let north = estimate_altitude(45.0, 125.0);
        assert!((110.0..=130.0).contains(&north), "北方估到 {north}m");
        assert!((30.0..=60.0).contains(&estimate_altitude(39.9, 116.4)), "北京");
        assert!(estimate_altitude(31.2, 121.5) < 40.0, "上海");
        assert!(estimate_altitude(34.3, 108.9) > 300.0, "西安");
        assert!(estimate_altitude(25.0, 102.7) > 500.0, "昆明/成都量级");
    }

    /// 和真人 App 数据对齐：配速要有 3–5 倍极差、步频 100–200 且偶发掉档、
    /// 步幅 0.5–2.5m、海拔锯齿。
    #[test]
    fn generated_track_matches_real_app_statistics() {
        let points = [
            (39.918256, 116.406199),
            (39.918888, 116.405747),
            (39.917447, 116.405658),
        ];
        let mut global_min = f64::INFINITY;
        for seed in [1u64, 7, 42] {
            let track = build(2000.0, 860, seed, (39.916, 116.397), 1_760_000_000_000, &points);
            let speeds: Vec<f64> = track
                .speedPerTenSec
                .iter()
                .map(|w| w.value)
                .filter(|v| *v > 0.0)
                .collect();
            let steps: Vec<f64> = track
                .stepsPerTenSec
                .iter()
                .map(|w| w.value)
                .filter(|v| *v >= 0.0)
                .collect();
            let avg_v = speeds.iter().sum::<f64>() / speeds.len() as f64;
            let min_v = speeds.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_v = speeds.iter().cloned().fold(0.0f64, f64::max);
            let avg_s = steps.iter().sum::<f64>() / steps.len() as f64;
            let min_s = steps.iter().cloned().fold(f64::INFINITY, f64::min);
            let max_s = steps.iter().cloned().fold(0.0f64, f64::max);
            // 步幅（cm）= 10秒窗距离 / 10秒窗步数
            let mut strides: Vec<f64> = speeds
                .iter()
                .zip(steps.iter())
                .filter(|(_, st)| **st > 0.1)
                .map(|(d, st)| d / st * 100.0)
                .collect();
            strides.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let (stride_lo, stride_hi) = (
                strides.first().copied().unwrap_or(0.0),
                strides.last().copied().unwrap_or(0.0),
            );
            let alts: Vec<f64> = track.locations.iter().map(|p| p.bdA).collect();
            let alt_lo = alts.iter().cloned().fold(f64::INFINITY, f64::min);
            let alt_hi = alts.iter().cloned().fold(0.0f64, f64::max);
            // 海拔锯齿密度：相邻点差值的中位数（真人 ≈1m）
            let alt_vals: Vec<f64> = track.locations.iter().map(|p| p.bdA).collect();
            let mut deltas: Vec<f64> = alt_vals
                .windows(2)
                .map(|w| (w[1] - w[0]).abs())
                .collect();
            deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let alt_step = deltas.get(deltas.len() / 2).copied().unwrap_or(0.0);
            println!(
                "seed={seed:>3} 窗={:>3} 配速 avg {} | 最快 {} | 最慢 {} | 步频 avg {:.0} [{:.0}..{:.0}] | 步幅 avg {:.0} [{:.0}..{:.0}]cm | 海拔 {:.1}~{:.1} ({:.1}m, 锯齿中位 {:.2}m)",
                speeds.len(),
                pace(10000.0 / avg_v),
                pace(10000.0 / max_v),
                pace(10000.0 / min_v),
                avg_s * 6.0,
                min_s * 6.0,
                max_s * 6.0,
                avg_v / avg_s.max(0.1) * 100.0,
                stride_lo,
                stride_hi,
                alt_lo,
                alt_hi,
                alt_hi - alt_lo,
                alt_step
            );
            // 相邻 10 秒窗的配速不能突变（真人走/跑切换也是几十秒渐变）
            let mut worst_jump = 1.0f64;
            for pair in speeds.windows(2) {
                let ratio = (pair[1] / pair[0]).max(pair[0] / pair[1]);
                worst_jump = worst_jump.max(ratio);
            }
            println!("           相邻窗最大变化 {worst_jump:.2} 倍");
            assert!(worst_jump < 2.2, "相邻窗口配速突变 {worst_jump:.2} 倍，曲线不平滑");
            // 真人步幅 45–250cm（平均 ~105）
            assert!(stride_lo >= 45.0, "步幅最小 {stride_lo:.0}cm 太小");
            assert!(stride_hi <= 235.0, "步幅最大 {stride_hi:.0}cm 太大");
            // 真人相邻点海拔差中位数 ≈1m：锯齿得够密，不能是平滑曲线
            assert!(alt_step >= 0.45, "海拔锯齿太稀：相邻点差中位 {alt_step:.2}m");
            assert!(
                (2.5..=9.0).contains(&(alt_hi - alt_lo)),
                "海拔总波动 {:.1}m 超出真人范围(1.3–6.1m)",
                alt_hi - alt_lo
            );
            // 硬约束：配速不能是一条直线（真人极差 ≥2 倍）
            assert!(max_v / min_v >= 1.8, "配速极差只有 {:.2} 倍，太平滑", max_v / min_v);
            // 真人步频：主体 100–160、最小 18–42、最大 168–198
            assert!(max_s * 6.0 <= 180.0, "步频最大 {:.0} 太高", max_s * 6.0);
            assert!(max_s - min_s >= 8.0, "步频波动太小：{:.1}..{:.1}", min_s * 6.0, max_s * 6.0);
            global_min = global_min.min(min_s * 6.0);
        }
    }
}
