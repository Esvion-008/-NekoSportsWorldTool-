//! 校园地图页：GeoJSON 导入、校验报告、矢量预览、参数与版本快照。
//!
//! 页面自身负责文件读写与 campus.json 持久化；地图数据统一存 BD09，
//! 画布只使用内部米制投影，不请求在线瓦片。

use super::{mobile, theme, App};
use crate::campus::import::{self, ImportedCampus};
use crate::campus::store::{self, SnapshotMeta};
use crate::campus::{geo, Campus, CampusStats, LngLat, PoiKind, SNAPSHOT_KEEP};
use crate::platform::InputKind;
use chrono::{Local, TimeZone};
use eframe::egui;
use egui::{Align2, Color32, FontId, Pos2, Sense, Stroke, Vec2};
use std::path::{Path, PathBuf};

const CANVAS_HEIGHT: f32 = 380.0;
const CANVAS_HEIGHT_COMPACT: f32 = 280.0;
const CANVAS_BG: Color32 = Color32::from_rgb(250, 252, 251);

#[derive(Clone, Debug)]
struct ViewState {
    zoom: f32,
    pan: Vec2,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: Vec2::ZERO,
        }
    }
}

/// 校园地图页状态。
pub struct CampusPage {
    pub campus: Option<Campus>,
    pub stats: Option<CampusStats>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub status: String,
    pub import_path: String,
    pub roads_path: String,
    pub base_altitude: String,
    pub clearance_min: String,
    pub clearance_max: String,
    pub allow_shrink: String,
    pub show_roads: bool,
    pub show_corridors: bool,
    pub show_playgrounds: bool,
    pub show_labels: bool,
    pub snapshots: Vec<SnapshotMeta>,
    view: ViewState,
    fit_on_next_frame: bool,
    /// POI 管理：当前新建类别。
    pub poi_kind: PoiKind,
    pub poi_name: String,
    pub poi_lng: String,
    pub poi_lat: String,
    /// 正在编辑的 POI（类别 + 在类别列表中的下标）。
    pub poi_editing: Option<(PoiKind, usize)>,
    /// 待确认删除的 POI（类别 + 下标 + 显示名）。
    pub poi_delete_pending: Option<(PoiKind, usize, String)>,
    /// 拾取模式：下一次画布单击写入坐标输入框。
    pub poi_pick_mode: bool,
    /// 画布上最近一次拾取/选中的坐标标记。
    pub poi_pick_marker: Option<LngLat>,
    pub poi_message: String,
    pub poi_message_ok: bool,
}

impl Default for CampusPage {
    fn default() -> Self {
        Self {
            campus: None,
            stats: None,
            errors: Vec::new(),
            warnings: Vec::new(),
            status: String::new(),
            import_path: String::new(),
            roads_path: String::new(),
            base_altitude: String::new(),
            clearance_min: String::new(),
            clearance_max: String::new(),
            allow_shrink: String::new(),
            show_roads: true,
            show_corridors: true,
            show_playgrounds: true,
            show_labels: true,
            snapshots: Vec::new(),
            view: ViewState::default(),
            fit_on_next_frame: true,
            poi_kind: PoiKind::Canteen,
            poi_name: String::new(),
            poi_lng: String::new(),
            poi_lat: String::new(),
            poi_editing: None,
            poi_delete_pending: None,
            poi_pick_mode: false,
            poi_pick_marker: None,
            poi_message: String::new(),
            poi_message_ok: false,
        }
    }
}

impl CampusPage {
    /// 启动时从 campus.json 恢复；文件损坏只在页面上报错，不阻断程序启动。
    pub fn load() -> Self {
        let mut page = Self::default();
        match store::load() {
            Ok(Some(campus)) => {
                let stats = campus.stats();
                page.status = format!(
                    "已载入 campus.json：{} 个禁行区、{} 个 POI、{} 段路网",
                    stats.blocked_count,
                    stats.poi_total(),
                    stats.road_count
                );
                page.apply_campus(campus, stats, Vec::new());
            }
            Ok(None) => {
                page.status = "尚未导入校园地图，请先导入 campus.geojson".into();
            }
            Err(e) => {
                page.status = "campus.json 读取失败".into();
                page.errors.push(e);
            }
        }
        page.refresh_snapshots();
        page
    }

    fn apply_campus(&mut self, campus: Campus, stats: CampusStats, warnings: Vec<String>) {
        self.sync_settings(&campus);
        self.campus = Some(campus);
        self.stats = Some(stats);
        self.warnings = warnings;
        self.fit_on_next_frame = true;
        self.refresh_snapshots();
    }

    fn sync_settings(&mut self, campus: &Campus) {
        self.base_altitude = format!("{:.1}", campus.base_altitude_m);
        self.clearance_min = format!("{:.2}", campus.clearance_min);
        self.clearance_max = format!("{:.2}", campus.clearance_max);
        self.allow_shrink = format!("{:.1}", campus.allow_shrink_m);
    }

    fn refresh_snapshots(&mut self) {
        self.snapshots = store::list_snapshots(&crate::platform::data_dir());
    }

    fn new_poi_form(&mut self) {
        self.poi_editing = None;
        self.poi_delete_pending = None;
        self.poi_name.clear();
        self.poi_lng.clear();
        self.poi_lat.clear();
        self.poi_pick_mode = false;
        self.poi_pick_marker = None;
        self.poi_message = "新建 POI：填写名称/坐标，或点击画布拾取".into();
        self.poi_message_ok = true;
    }

    fn select_poi(&mut self, kind: PoiKind, index: usize) {
        let Some((name, pos)) = self.campus.as_ref().and_then(|campus| {
            campus
                .pois
                .get(kind)
                .get(index)
                .map(|poi| (poi.name.clone(), poi.pos))
        }) else {
            return;
        };
        self.poi_editing = Some((kind, index));
        self.poi_kind = kind;
        self.poi_name = name;
        self.poi_lng = format!("{:.7}", pos[0]);
        self.poi_lat = format!("{:.7}", pos[1]);
        self.poi_pick_marker = Some(pos);
        self.poi_pick_mode = false;
        self.poi_delete_pending = None;
        self.poi_message = format!("正在编辑 {} POI", kind.label());
        self.poi_message_ok = true;
    }

    fn poi_position_input(&self) -> Result<LngLat, String> {
        let lng = parse_coordinate("BD 经度", &self.poi_lng)?;
        let lat = parse_coordinate("BD 纬度", &self.poi_lat)?;
        Ok([lng, lat])
    }

    /// 统一 POI 变更入口：校验 → 写 campus.json → 更新统计/快照/警告。
    /// 失败时回滚内存数据，保证界面与磁盘一致。
    fn update_pois<F>(&mut self, success: &str, update: F) -> bool
    where
        F: FnOnce(&mut Campus) -> Result<(), String>,
    {
        let Some(mut campus) = self.campus.take() else {
            self.poi_message = "尚未导入校园地图".into();
            self.poi_message_ok = false;
            return false;
        };
        let backup = campus.clone();
        if let Err(e) = update(&mut campus) {
            self.campus = Some(backup);
            self.poi_message = format!("操作被拒绝：{e}");
            self.poi_message_ok = false;
            return false;
        }
        let stats = campus.stats();
        match store::save(&mut campus) {
            Ok(outcome) => {
                let mut warnings: Vec<String> = self
                    .warnings
                    .iter()
                    .filter(|warning| !warning.contains("POI 数量"))
                    .cloned()
                    .collect();
                warnings.extend(poi_count_warnings(&campus));
                warnings.extend(outcome.warnings);
                self.warnings = warnings;
                self.errors.clear();
                self.campus = Some(campus);
                self.stats = Some(stats);
                self.refresh_snapshots();
                self.poi_message = success.to_string();
                self.poi_message_ok = true;
                self.status = success.to_string();
                true
            }
            Err(e) => {
                self.campus = Some(backup);
                self.errors = vec![format!("campus.json 保存失败：{e}")];
                self.poi_message = format!("保存失败：{e}");
                self.poi_message_ok = false;
                false
            }
        }
    }

    fn add_poi(&mut self) {
        let kind = self.poi_kind;
        let name = self.poi_name.trim().to_string();
        let pos = match self.poi_position_input() {
            Ok(pos) => pos,
            Err(e) => {
                self.poi_message = e;
                self.poi_message_ok = false;
                return;
            }
        };
        let labeled = if name.is_empty() {
            format!("{} POI", kind.label())
        } else {
            name.clone()
        };
        let success = format!("已添加 {} POI「{}」", kind.label(), labeled);
        let ok = self.update_pois(&success, |campus| {
            campus.add_poi(kind, name, pos).map(|_| ())
        });
        if ok {
            self.poi_editing = None;
            self.poi_name.clear();
            self.poi_lng.clear();
            self.poi_lat.clear();
            self.poi_pick_mode = false;
            self.poi_pick_marker = Some(pos);
        }
    }

    fn save_poi_edit(&mut self) {
        let Some((kind, index)) = self.poi_editing else {
            self.poi_message = "请先选择要编辑的 POI".into();
            self.poi_message_ok = false;
            return;
        };
        let name = self.poi_name.trim().to_string();
        let pos = match self.poi_position_input() {
            Ok(pos) => pos,
            Err(e) => {
                self.poi_message = e;
                self.poi_message_ok = false;
                return;
            }
        };
        let ok = self.update_pois("POI 修改已保存", |campus| {
            campus.update_poi(kind, index, name, pos)
        });
        if ok {
            self.select_poi(kind, index);
            self.poi_message = "POI 修改已保存".into();
            self.poi_message_ok = true;
        }
    }

    fn delete_poi(&mut self, kind: PoiKind, index: usize, name: &str) {
        let success = format!("已删除 {} POI「{}」", kind.label(), name);
        let ok = self.update_pois(&success, |campus| {
            campus.remove_poi(kind, index).map(|_| ())
        });
        if ok {
            self.poi_editing = None;
            self.poi_delete_pending = None;
            self.poi_pick_marker = None;
            self.poi_name.clear();
            self.poi_lng.clear();
            self.poi_lat.clear();
        }
    }

    fn import_campus(&mut self, path: &Path) {
        if path.as_os_str().is_empty() {
            self.status = "请先填写 campus.geojson 路径".into();
            return;
        }
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                self.errors = vec![format!("读取 {} 失败：{e}", path.display())];
                self.warnings.clear();
                self.status = "导入失败：campus.json 未改动".into();
                return;
            }
        };
        let imported: ImportedCampus = match import::import_campus_geojson(
            &text,
            crate::crypto::envelope::now_ms(),
        ) {
            Ok(imported) => imported,
            Err(failure) => {
                self.errors = failure.errors;
                self.warnings = failure.warnings;
                self.status = "导入失败：campus.json 未改动".into();
                return;
            }
        };
        let ImportedCampus {
            mut campus,
            stats: _,
            mut warnings,
        } = imported;

        // 重新导入校园区域时，尽量保留旧路网（按新区域裁剪）
        if let Some(old) = self.campus.take() {
            warnings.extend(import::carry_over_roads(&old, &mut campus));
        }
        // 路网可能在上面被保留/裁剪，统计必须以最终数据为准
        let stats = campus.stats();

        let save_warnings = match store::save(&mut campus) {
            Ok(outcome) => {
                self.status = format!(
                    "导入成功：{} 个禁行区、{} 个走廊、{} 个操场、{} 个广场、{} 个 POI；已保存 campus.json",
                    campus.blocked.len(),
                    campus.corridors.len(),
                    campus.playgrounds.len(),
                    campus.plazas.len(),
                    campus.poi_total()
                );
                self.errors.clear();
                outcome.warnings
            }
            Err(e) => {
                self.errors = vec![format!("campus.json 保存失败：{e}")];
                self.status = "导入成功但保存失败（重启后会丢失）".into();
                Vec::new()
            }
        };
        warnings.extend(save_warnings);
        self.import_path = path.display().to_string();
        self.apply_campus(campus, stats, warnings);
    }

    fn import_roads(&mut self, path: &Path) {
        if path.as_os_str().is_empty() {
            self.status = "请先填写 roads.geojson 路径".into();
            return;
        }
        let Some(campus) = self.campus.as_ref() else {
            self.status = "请先导入校园区域（campus.geojson），再导入路网".into();
            return;
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => {
                self.errors = vec![format!("读取 {} 失败：{e}", path.display())];
                self.warnings.clear();
                self.status = "路网导入失败".into();
                return;
            }
        };
        let imported = match import::import_roads_geojson(&text, campus) {
            Ok(imported) => imported,
            Err(failure) => {
                self.errors = failure.errors;
                self.warnings = failure.warnings;
                self.status = "路网导入失败：campus.json 未改动".into();
                return;
            }
        };
        let Some(mut campus) = self.campus.take() else {
            return;
        };
        campus.roads = imported.roads;
        let stats = campus.stats();
        let mut warnings = imported.warnings;
        match store::save(&mut campus) {
            Ok(outcome) => {
                self.status = format!(
                    "路网导入成功：保留 {} 段、丢弃 {} 条，共 {:.2} km；已保存 campus.json",
                    imported.kept_lines,
                    imported.dropped_lines,
                    stats.road_length_m / 1000.0
                );
                self.errors.clear();
                warnings.extend(outcome.warnings);
            }
            Err(e) => {
                self.errors = vec![format!("campus.json 保存失败：{e}")];
                self.status = "路网导入成功但保存失败（重启后会丢失）".into();
            }
        }
        self.roads_path = path.display().to_string();
        self.sync_settings(&campus);
        self.campus = Some(campus);
        self.stats = Some(stats);
        self.warnings = warnings;
        self.refresh_snapshots();
    }

    fn save_settings(&mut self) {
        let Some(mut campus) = self.campus.take() else {
            self.status = "尚未导入校园地图".into();
            return;
        };
        let mut errors = Vec::new();
        let base_altitude = parse_number("基准海拔", &self.base_altitude, &mut errors);
        let clearance_min = parse_number("净空下限", &self.clearance_min, &mut errors);
        let clearance_max = parse_number("净空上限", &self.clearance_max, &mut errors);
        let allow_shrink = parse_number("允许区域内缩", &self.allow_shrink, &mut errors);
        if let Some(value) = base_altitude {
            if !(-500.0..=9000.0).contains(&value) {
                errors.push("基准海拔应在 -500 – 9000 m 之间".into());
            }
        }
        if let (Some(min), Some(max)) = (clearance_min, clearance_max) {
            if !(0.0..=10.0).contains(&min) || !(0.0..=10.0).contains(&max) {
                errors.push("净空应在 0–10 m 之间".into());
            }
            if min > max {
                errors.push("净空下限不能大于上限".into());
            }
        }
        if let Some(value) = allow_shrink {
            if !(0.0..=50.0).contains(&value) {
                errors.push("允许区域内缩应在 0–50 m 之间".into());
            }
        }
        if !errors.is_empty() {
            self.errors = errors;
            self.status = "参数未保存".into();
            self.campus = Some(campus);
            return;
        }
        campus.base_altitude_m = base_altitude.unwrap_or(campus.base_altitude_m);
        campus.clearance_min = clearance_min.unwrap_or(campus.clearance_min);
        campus.clearance_max = clearance_max.unwrap_or(campus.clearance_max);
        campus.allow_shrink_m = allow_shrink.unwrap_or(campus.allow_shrink_m);
        let stats = campus.stats();
        let mut warnings = Vec::new();
        match store::save(&mut campus) {
            Ok(outcome) => {
                self.errors.clear();
                self.status = "校园参数已保存".into();
                warnings.extend(outcome.warnings);
            }
            Err(e) => {
                self.errors = vec![format!("campus.json 保存失败：{e}")];
                self.status = "参数未保存".into();
            }
        }
        self.sync_settings(&campus);
        self.campus = Some(campus);
        self.stats = Some(stats);
        self.warnings = warnings;
        self.refresh_snapshots();
    }

    /// 页面入口。
    pub fn show(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.heading("校园地图");
                ui.label(
                    egui::RichText::new(
                        "V2 路线生成的基础数据：校园区域、禁行区、走廊、POI、路网。\
                         导入 WGS84 GeoJSON，内部转换为 BD09 并保存到 campus.json。",
                    )
                    .small()
                    .color(theme::text_dim()),
                );
                let status_color = if !self.errors.is_empty() {
                    theme::err()
                } else if !self.warnings.is_empty() {
                    theme::warn()
                } else {
                    theme::text_dim()
                };
                let status_text = if self.errors.is_empty() {
                    self.status.clone()
                } else {
                    format!(
                        "{}（{} 条错误，见下方 ③ 导入校验）",
                        self.status,
                        self.errors.len()
                    )
                };
                ui.colored_label(status_color, status_text);
                ui.add_space(8.0);

                egui::CollapsingHeader::new("① 导入 GeoJSON")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label("campus.geojson：Polygon area / blocked / corridor / playground / plaza + Point poi_canteen / poi_dorm / poi_gate");
                        self.draw_campus_import_row(ui);
                        ui.add_space(8.0);
                        ui.label("roads.geojson：LineString / MultiLineString，导入后按校园区域裁剪");
                        self.draw_roads_import_row(ui);
                    });

                if let Some(campus) = &self.campus {
                    let updated = fmt_time(campus.updated_ms);
                    ui.label(
                        egui::RichText::new(format!("当前 campus.json 更新时间：{updated}"))
                            .small()
                            .color(theme::text_dim()),
                    );
                }

                if self.stats.is_some() {
                    ui.add_space(4.0);
                    egui::CollapsingHeader::new("② 数据概览")
                        .default_open(true)
                        .show(ui, |ui| self.draw_stats(ui));
                }

                if !self.errors.is_empty() || !self.warnings.is_empty() {
                    ui.add_space(4.0);
                    let title = format!(
                        "③ 导入校验：错误 {} 条 / 警告 {} 条",
                        self.errors.len(),
                        self.warnings.len()
                    );
                    egui::CollapsingHeader::new(title)
                        .default_open(!self.errors.is_empty())
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("campus_messages")
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    for error in &self.errors {
                                        ui.colored_label(theme::err(), format!("✗ {error}"));
                                    }
                                    for warning in &self.warnings {
                                        ui.colored_label(theme::warn(), format!("⚠ {warning}"));
                                    }
                                });
                        });
                }

                ui.add_space(4.0);
                if self.campus.is_some() {
                    egui::CollapsingHeader::new("④ POI 管理")
                        .default_open(true)
                        .show(ui, |ui| self.draw_poi_management(ui));
                }

                ui.add_space(4.0);
                egui::CollapsingHeader::new("⑤ 矢量预览")
                    .default_open(true)
                    .show(ui, |ui| {
                        mobile::row(ui, |ui| {
                            ui.checkbox(&mut self.show_roads, "路网");
                            ui.checkbox(&mut self.show_corridors, "走廊");
                            ui.checkbox(&mut self.show_playgrounds, "操场");
                            ui.checkbox(&mut self.show_labels, "POI 名称");
                            if ui
                                .toggle_value(&mut self.poi_pick_mode, "拾取 POI 坐标")
                                .changed()
                            {
                                self.poi_message = if self.poi_pick_mode {
                                    "拾取模式已开启：单击画布取点".into()
                                } else {
                                    "已退出拾取模式".into()
                                };
                                self.poi_message_ok = true;
                            }
                            if ui.small_button("复位视图").clicked() {
                                self.fit_on_next_frame = true;
                            }
                        });
                        if self.poi_pick_mode {
                            ui.colored_label(
                                theme::warn(),
                                "拾取模式：单击画布确定坐标；拖拽仍可平移，滚轮缩放。",
                            );
                        }
                        ui.add_space(4.0);
                        self.draw_canvas(ui);
                    });

                ui.add_space(4.0);
                egui::CollapsingHeader::new("⑥ 参数")
                    .default_open(false)
                    .show(ui, |ui| self.draw_settings(ui));

                ui.add_space(4.0);
                egui::CollapsingHeader::new("⑦ 版本快照")
                    .default_open(false)
                    .show(ui, |ui| self.draw_snapshots(ui));

                ui.add_space(8.0);
                let color = if !self.errors.is_empty() {
                    theme::err()
                } else if !self.warnings.is_empty() {
                    theme::warn()
                } else {
                    theme::text_dim()
                };
                ui.colored_label(color, &self.status);
                ui.add_space(6.0);
            });
    }

    fn draw_campus_import_row(&mut self, ui: &mut egui::Ui) {
        let compact = mobile::compact_ui(ui);
        let mut picked: Option<PathBuf> = None;
        mobile::row(ui, |ui| {
            #[cfg(not(target_os = "android"))]
            {
                if ui.add(theme::primary_btn("选择 campus.geojson…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("GeoJSON", &["geojson", "json"])
                        .pick_file()
                    {
                        picked = Some(path);
                    }
                }
            }
            let width = if compact { 170.0 } else { 380.0 };
            mobile::text_edit(
                ui,
                "campus_import_path",
                &mut self.import_path,
                InputKind::Text,
                width,
            );
            if ui.button("从路径导入").clicked() {
                picked = Some(PathBuf::from(self.import_path.trim()));
            }
        });
        if let Some(path) = picked {
            self.import_campus(&path);
        }
    }

    fn draw_roads_import_row(&mut self, ui: &mut egui::Ui) {
        let compact = mobile::compact_ui(ui);
        let mut picked: Option<PathBuf> = None;
        mobile::row(ui, |ui| {
            #[cfg(not(target_os = "android"))]
            {
                if ui.add(theme::primary_btn("选择 roads.geojson…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("GeoJSON", &["geojson", "json"])
                        .pick_file()
                    {
                        picked = Some(path);
                    }
                }
            }
            let width = if compact { 170.0 } else { 380.0 };
            mobile::text_edit(
                ui,
                "campus_roads_path",
                &mut self.roads_path,
                InputKind::Text,
                width,
            );
            if ui.button("导入路网").clicked() {
                picked = Some(PathBuf::from(self.roads_path.trim()));
            }
        });
        if let Some(path) = picked {
            self.import_roads(&path);
        }
    }

    fn draw_stats(&mut self, ui: &mut egui::Ui) {
        let Some(stats) = self.stats.as_ref() else {
            return;
        };
        egui::Grid::new("campus_stats_grid")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                ui.strong("允许区域");
                ui.label(format!(
                    "{:.2} 公顷（{:.0} m²）· {} 顶点 · 周长 {:.0} m",
                    stats.area_m2 / 10_000.0,
                    stats.area_m2,
                    stats.area_vertices,
                    stats.area_perimeter_m
                ));
                ui.end_row();
                ui.strong("禁行区");
                ui.label(format!(
                    "{} 个 · {:.2} 公顷 · 可用区域 {:.2} 公顷",
                    stats.blocked_count,
                    stats.blocked_area_m2 / 10_000.0,
                    stats.allowed_area_m2 / 10_000.0
                ));
                ui.end_row();
                ui.strong("走廊");
                ui.label(format!("{} 个", stats.corridor_count));
                ui.end_row();
                ui.strong("POI");
                ui.label(format!(
                    "食堂 {} · 宿舍 {} · 校门 {}（共 {}）",
                    stats.canteen,
                    stats.dorm,
                    stats.gate,
                    stats.poi_total()
                ));
                ui.end_row();
                ui.strong("路网");
                ui.label(if stats.road_count == 0 {
                    "未导入".to_string()
                } else {
                    format!(
                        "{} 段 · 总长 {:.2} km",
                        stats.road_count,
                        stats.road_length_m / 1000.0
                    )
                });
                ui.end_row();
                if let Some(campus) = &self.campus {
                    ui.strong("生成参数");
                    ui.label(format!(
                        "净空 {:.2}–{:.2} m · 区域内缩 {:.1} m · 基准海拔 {:.1} m",
                        campus.clearance_min,
                        campus.clearance_max,
                        campus.allow_shrink_m,
                        campus.base_altitude_m
                    ));
                    ui.end_row();
                }
                ui.strong("版本快照");
                ui.label(format!("{} 份（保留最近 {SNAPSHOT_KEEP} 份）", self.snapshots.len()));
                ui.end_row();
            });
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        if self.campus.is_none() {
            ui.label("导入校园数据后可修改生成参数。");
            return;
        }
        mobile::row(ui, |ui| {
            ui.label("基准海拔（m）：");
            mobile::text_edit(
                ui,
                "campus_base_altitude",
                &mut self.base_altitude,
                InputKind::Decimal,
                100.0,
            );
            ui.label("净空下限（m）：");
            mobile::text_edit(
                ui,
                "campus_clearance_min",
                &mut self.clearance_min,
                InputKind::Decimal,
                90.0,
            );
            ui.label("净空上限（m）：");
            mobile::text_edit(
                ui,
                "campus_clearance_max",
                &mut self.clearance_max,
                InputKind::Decimal,
                90.0,
            );
        });
        mobile::row(ui, |ui| {
            ui.label("允许区域内缩（m）：");
            mobile::text_edit(
                ui,
                "campus_allow_shrink",
                &mut self.allow_shrink,
                InputKind::Decimal,
                90.0,
            );
            if ui.add(theme::primary_btn("保存参数")).clicked() {
                self.save_settings();
            }
            ui.label("修改后写入 campus.json 并生成快照");
        });
    }

    fn draw_poi_management(&mut self, ui: &mut egui::Ui) {
        let Some(campus) = self.campus.as_ref() else {
            ui.label("导入校园数据后可管理 POI。");
            return;
        };
        let mut rows: Vec<(PoiKind, usize, String, LngLat)> = Vec::new();
        for kind in PoiKind::ALL {
            for (index, poi) in campus.pois.get(kind).iter().enumerate() {
                rows.push((kind, index, poi.name.clone(), poi.pos));
            }
        }
        ui.label(format!(
            "共 {} 个 POI（食堂 {} / 宿舍 {} / 校门 {}）。选择一条编辑，或删除后保存并生成快照。",
            rows.len(),
            campus.pois.canteen.len(),
            campus.pois.dorm.len(),
            campus.pois.gate.len()
        ));
        ui.add_space(4.0);

        let selected = self.poi_editing;
        let mut select: Option<(PoiKind, usize)> = None;
        let mut delete: Option<(PoiKind, usize, String)> = None;
        if rows.is_empty() {
            ui.label("暂无 POI。可在下方新建，或点击画布拾取坐标。");
        } else if mobile::compact_ui(ui) {
            for (kind, index, name, pos) in &rows {
                let is_selected = selected == Some((*kind, *index));
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.strong(kind.label());
                        ui.label(if name.trim().is_empty() {
                            format!("#{}", index + 1)
                        } else {
                            name.clone()
                        });
                        if is_selected {
                            ui.colored_label(theme::ok(), "编辑中");
                        }
                    });
                    ui.monospace(format!("BD 经度 {:.7} · 纬度 {:.7}", pos[0], pos[1]));
                    ui.horizontal(|ui| {
                        if ui.small_button("编辑").clicked() {
                            select = Some((*kind, *index));
                        }
                        if ui.small_button("删除").clicked() {
                            delete = Some((*kind, *index, name.clone()));
                        }
                    });
                });
                ui.add_space(4.0);
            }
        } else {
            egui::Grid::new("campus_poi_grid")
                .num_columns(5)
                .spacing([14.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.strong("类别");
                    ui.strong("名称");
                    ui.strong("BD 经度");
                    ui.strong("BD 纬度");
                    ui.strong("操作");
                    ui.end_row();
                    for (kind, index, name, pos) in &rows {
                        let is_selected = selected == Some((*kind, *index));
                        ui.label(kind.label());
                        ui.label(format!(
                            "{}{}",
                            if is_selected { "▶ " } else { "" },
                            if name.trim().is_empty() {
                                format!("#{}", index + 1)
                            } else {
                                name.clone()
                            }
                        ));
                        ui.monospace(format!("{:.7}", pos[0]));
                        ui.monospace(format!("{:.7}", pos[1]));
                        ui.horizontal(|ui| {
                            if ui.small_button("编辑").clicked() {
                                select = Some((*kind, *index));
                            }
                            if ui.small_button("删除").clicked() {
                                delete = Some((*kind, *index, name.clone()));
                            }
                        });
                        ui.end_row();
                    }
                });
        }

        if let Some((kind, index)) = select {
            self.select_poi(kind, index);
        }
        if let Some((kind, index, name)) = delete {
            self.poi_delete_pending = Some((kind, index, name));
            self.poi_message.clear();
        }

        if let Some((kind, index, name)) = self.poi_delete_pending.clone() {
            ui.add_space(4.0);
            ui.colored_label(
                theme::err(),
                format!("确认删除 {} POI「{}」？", kind.label(), name),
            );
            let mut confirm = false;
            let mut cancel = false;
            mobile::row(ui, |ui| {
                if ui.add(theme::primary_btn("确认删除")).clicked() {
                    confirm = true;
                }
                if ui.button("取消").clicked() {
                    cancel = true;
                }
            });
            if confirm {
                self.delete_poi(kind, index, &name);
            } else if cancel {
                self.poi_delete_pending = None;
            }
        }

        ui.add_space(6.0);
        ui.separator();
        let editing = self.poi_editing;
        let edit_name = editing
            .and_then(|(kind, index)| {
                self.campus
                    .as_ref()
                    .and_then(|campus| campus.pois.get(kind).get(index))
                    .map(|poi| poi.name.clone())
            })
            .unwrap_or_default();
        let title = match editing {
            Some((kind, _)) => format!(
                "编辑 {} POI{}",
                kind.label(),
                if edit_name.trim().is_empty() {
                    String::new()
                } else {
                    format!("「{edit_name}」")
                }
            ),
            None => format!("新建 {} POI", self.poi_kind.label()),
        };
        ui.strong(title);
        ui.add_space(4.0);

        mobile::row(ui, |ui| {
            ui.label("类别：");
            if editing.is_some() {
                ui.label(self.poi_kind.label());
            } else {
                egui::ComboBox::from_id_salt("campus_poi_kind")
                    .selected_text(self.poi_kind.label())
                    .show_ui(ui, |ui| {
                        for kind in PoiKind::ALL {
                            ui.selectable_value(&mut self.poi_kind, kind, kind.label());
                        }
                    });
            }
            ui.label("名称：");
            mobile::text_edit(
                ui,
                "campus_poi_name",
                &mut self.poi_name,
                InputKind::Text,
                180.0,
            );
        });
        mobile::row(ui, |ui| {
            ui.label("BD 经度：");
            mobile::text_edit(
                ui,
                "campus_poi_lng",
                &mut self.poi_lng,
                InputKind::Decimal,
                120.0,
            );
            ui.label("BD 纬度：");
            mobile::text_edit(
                ui,
                "campus_poi_lat",
                &mut self.poi_lat,
                InputKind::Decimal,
                120.0,
            );
            let pick_label = if self.poi_pick_mode {
                "拾取中…（点击画布）"
            } else {
                "点击画布取坐标"
            };
            if ui.add(theme::primary_btn(pick_label)).clicked() {
                self.poi_pick_mode = !self.poi_pick_mode;
                self.poi_message = if self.poi_pick_mode {
                    "拾取模式已开启：到下方「矢量预览」画布单击取点".into()
                } else {
                    "已退出拾取模式".into()
                };
                self.poi_message_ok = true;
            }
        });

        let mut do_add = false;
        let mut do_save = false;
        let mut do_new = false;
        mobile::row(ui, |ui| {
            if editing.is_some() {
                if ui.add(theme::primary_btn("保存修改")).clicked() {
                    do_save = true;
                }
                if ui.button("取消编辑 / 新建").clicked() {
                    do_new = true;
                }
            } else {
                if ui.add(theme::primary_btn("添加 POI")).clicked() {
                    do_add = true;
                }
                if ui.button("清空输入").clicked() {
                    do_new = true;
                }
            }
            ui.label("坐标按校园区域与禁行区校验，不合规不会写入");
        });
        if do_new {
            self.new_poi_form();
        }
        if do_save {
            self.save_poi_edit();
        }
        if do_add {
            self.add_poi();
        }
        if !self.poi_message.is_empty() {
            let color = if self.poi_message_ok {
                theme::ok()
            } else {
                theme::err()
            };
            ui.colored_label(color, &self.poi_message);
        }
    }

    fn draw_snapshots(&mut self, ui: &mut egui::Ui) {
        if self.snapshots.is_empty() {
            ui.label("暂无快照（导入/保存 campus.json 后自动生成）");
            return;
        }
        ui.label(format!(
            "共 {} 份，保留最近 {SNAPSHOT_KEEP} 份：",
            self.snapshots.len()
        ));
        for snapshot in self.snapshots.iter().take(8) {
            ui.label(format!(
                "{} · {} · {:.1} KB",
                fmt_time(snapshot.updated_ms),
                snapshot.name,
                snapshot.bytes as f64 / 1024.0
            ));
        }
        if self.snapshots.len() > 8 {
            ui.label(format!("……另有 {} 份", self.snapshots.len() - 8));
        }
    }

    /// 矢量画布：拖拽平移、滚轮缩放、悬停显示 BD 坐标。
    fn draw_canvas(&mut self, ui: &mut egui::Ui) {
        let height = if mobile::compact_ui(ui) {
            CANVAS_HEIGHT_COMPACT
        } else {
            CANVAS_HEIGHT
        };
        let size = egui::vec2(ui.available_width().max(160.0), height);
        let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        let rect = response.rect;
        painter.rect_filled(rect, egui::Rounding::same(6.0), CANVAS_BG);
        let painter = painter.with_clip_rect(rect);

        let Some(campus) = self.campus.as_ref() else {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "尚未导入校园地图",
                FontId::proportional(15.0),
                theme::text_dim(),
            );
            return;
        };

        if self.fit_on_next_frame {
            self.view = ViewState::default();
            self.fit_on_next_frame = false;
        }
        // 交互只改视图状态，不影响下面的数据借用
        if response.dragged() {
            self.view.pan += response.drag_delta();
        }
        if response.hovered() {
            let scroll = ui.input(|input| input.raw_scroll_delta.y);
            if scroll.abs() > 0.1 {
                let factor = (scroll * 0.0015).exp();
                self.view.zoom = (self.view.zoom * factor).clamp(0.2, 20.0);
            }
        }

        let projection = geo::Projection::for_polygon(&campus.area);
        let bounds = geo::polygon_bounds(&campus.area, &projection);
        if bounds.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "校园区域为空",
                FontId::proportional(15.0),
                theme::err(),
            );
            return;
        }
        let pad = 20.0_f32;
        let fit_scale = ((rect.width() - pad * 2.0) / bounds.width().max(1.0) as f32)
            .min((rect.height() - pad * 2.0) / bounds.height().max(1.0) as f32)
            .max(0.0001);
        let scale = (fit_scale * self.view.zoom).max(0.00001);
        let center_xy = bounds.center();
        let pan = self.view.pan;
        let to_screen = |point: LngLat| -> Pos2 {
            canvas_lnglat_to_pos(&projection, center_xy, rect.center(), scale, pan, point)
        };
        let to_lnglat = |pos: Pos2| -> LngLat {
            canvas_pos_to_lnglat(&projection, center_xy, rect.center(), scale, pan, pos)
        };

        if self.poi_pick_mode && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        }
        if self.poi_pick_mode && response.clicked() {
            if let Some(pointer) = response.interact_pointer_pos() {
                if rect.contains(pointer) {
                    let bd = to_lnglat(pointer);
                    self.poi_lng = format!("{:.7}", bd[0]);
                    self.poi_lat = format!("{:.7}", bd[1]);
                    self.poi_pick_marker = Some(bd);
                    self.poi_pick_mode = false;
                    self.poi_message = format!(
                        "已拾取 BD 坐标：经度 {:.7}，纬度 {:.7}；{}",
                        bd[0],
                        bd[1],
                        if self.poi_editing.is_some() {
                            "点「保存修改」生效"
                        } else {
                            "点「添加 POI」生效"
                        }
                    );
                    self.poi_message_ok = true;
                }
            }
        }
        if self.poi_pick_mode {
            painter.text(
                Pos2::new(rect.center().x, rect.top() + 8.0),
                Align2::CENTER_TOP,
                "拾取模式：单击画布确定 POI 坐标",
                FontId::proportional(12.0),
                theme::err(),
            );
        }

        // 允许区域（内环先不填，用画布底色挖洞）
        let area_color = Color32::from_rgb(13, 148, 136);
        let area_fill = Color32::from_rgba_unmultiplied(13, 148, 136, 30);
        let area_points: Vec<Pos2> = campus.area.outer.iter().map(|p| to_screen(*p)).collect();
        fill_polygon(&painter, &area_points, area_fill);
        painter.add(egui::Shape::closed_line(
            area_points,
            Stroke::new(1.6_f32, area_color),
        ));
        for hole in &campus.area.holes {
            let points: Vec<Pos2> = hole.iter().map(|p| to_screen(*p)).collect();
            fill_polygon(&painter, &points, CANVAS_BG);
            painter.add(egui::Shape::closed_line(
                points,
                Stroke::new(1.2_f32, area_color),
            ));
        }

        // 走廊
        if self.show_corridors {
            let corridor_fill = Color32::from_rgba_unmultiplied(217, 119, 6, 32);
            let corridor_stroke = Color32::from_rgb(180, 118, 0);
            for corridor in &campus.corridors {
                let points: Vec<Pos2> = corridor.polygon.outer.iter().map(|p| to_screen(*p)).collect();
                fill_polygon(&painter, &points, corridor_fill);
                painter.add(egui::Shape::closed_line(
                    points,
                    Stroke::new(1.1_f32, corridor_stroke),
                ));
            }
        }

        // 操场
        if self.show_playgrounds {
            let playground_fill = Color32::from_rgba_unmultiplied(34, 197, 94, 28);
            let playground_stroke = Color32::from_rgb(22, 163, 74);
            for playground in &campus.playgrounds {
                let points: Vec<Pos2> = playground
                    .polygon
                    .outer
                    .iter()
                    .map(|p| to_screen(*p))
                    .collect();
                fill_polygon(&painter, &points, playground_fill);
                painter.add(egui::Shape::closed_line(
                    points,
                    Stroke::new(1.3_f32, playground_stroke),
                ));
            }
            let plaza_fill = Color32::from_rgba_unmultiplied(56, 189, 248, 28);
            let plaza_stroke = Color32::from_rgb(2, 132, 199);
            for plaza in &campus.plazas {
                let points: Vec<Pos2> = plaza
                    .polygon
                    .outer
                    .iter()
                    .map(|p| to_screen(*p))
                    .collect();
                fill_polygon(&painter, &points, plaza_fill);
                painter.add(egui::Shape::closed_line(
                    points,
                    Stroke::new(1.3_f32, plaza_stroke),
                ));
            }
        }

        // 禁行区
        let blocked_color = Color32::from_rgb(196, 48, 48);
        let blocked_fill = Color32::from_rgba_unmultiplied(196, 48, 48, 40);
        for blocked in &campus.blocked {
            let points: Vec<Pos2> = blocked
                .polygon
                .outer
                .iter()
                .map(|p| to_screen(*p))
                .collect();
            fill_polygon(&painter, &points, blocked_fill);
            painter.add(egui::Shape::closed_line(
                points,
                Stroke::new(1.2_f32, blocked_color),
            ));
            for hole in &blocked.polygon.holes {
                let points: Vec<Pos2> = hole.iter().map(|p| to_screen(*p)).collect();
                fill_polygon(&painter, &points, CANVAS_BG);
                painter.add(egui::Shape::closed_line(
                    points,
                    Stroke::new(1.0_f32, blocked_color),
                ));
            }
        }

        // 路网
        if self.show_roads {
            let road_color = Color32::from_rgb(100, 116, 139);
            for road in &campus.roads {
                if road.path.len() < 2 {
                    continue;
                }
                let points: Vec<Pos2> = road.path.iter().map(|p| to_screen(*p)).collect();
                painter.add(egui::Shape::line(points, Stroke::new(1.3_f32, road_color)));
            }
        }

        // POI
        for (kind, poi) in campus.pois.all() {
            let color = match kind {
                PoiKind::Canteen => Color32::from_rgb(234, 88, 12),
                PoiKind::Dorm => Color32::from_rgb(37, 99, 235),
                PoiKind::Gate => Color32::from_rgb(124, 58, 237),
            };
            let position = to_screen(poi.pos);
            painter.circle_filled(position, 4.5, color);
            painter.circle_stroke(position, 4.5, Stroke::new(1.0_f32, Color32::WHITE));
            if self.show_labels || self.view.zoom > 1.6 {
                let name = if poi.name.trim().is_empty() {
                    format!("{}#{}", kind.label(), poi.id)
                } else {
                    poi.name.clone()
                };
                painter.text(
                    position + egui::vec2(6.0, -6.0),
                    Align2::LEFT_BOTTOM,
                    name,
                    FontId::proportional(11.0),
                    theme::text(),
                );
            }
        }

        // 选中的 POI：外圈高亮
        if let Some((kind, index)) = self.poi_editing {
            if let Some(poi) = campus.pois.get(kind).get(index) {
                let position = to_screen(poi.pos);
                painter.circle_stroke(
                    position,
                    9.0,
                    Stroke::new(2.0_f32, Color32::from_rgb(13, 148, 136)),
                );
            }
        }

        // 最近一次拾取/选中的坐标标记
        if let Some(marker) = self.poi_pick_marker {
            let position = to_screen(marker);
            let color = Color32::from_rgb(13, 148, 136);
            painter.circle_stroke(position, 6.0, Stroke::new(1.5_f32, color));
            painter.line_segment(
                [
                    position - egui::vec2(11.0, 0.0),
                    position + egui::vec2(11.0, 0.0),
                ],
                Stroke::new(1.0_f32, color),
            );
            painter.line_segment(
                [
                    position - egui::vec2(0.0, 11.0),
                    position + egui::vec2(0.0, 11.0),
                ],
                Stroke::new(1.0_f32, color),
            );
            painter.text(
                position + egui::vec2(9.0, 9.0),
                Align2::LEFT_TOP,
                "拾取点",
                FontId::proportional(10.0),
                color,
            );
        }

        // 比例尺
        let meters_per_px = 1.0 / scale as f64;
        let target_meters = 90.0 * meters_per_px;
        let bar_meters = nice_scale_m(target_meters as f32);
        let bar_px = bar_meters * scale;
        let bar_origin = Pos2::new(rect.left() + 14.0, rect.bottom() - 16.0);
        painter.line_segment(
            [bar_origin, bar_origin + egui::vec2(bar_px, 0.0)],
            Stroke::new(2.0_f32, theme::plain()),
        );
        painter.text(
            bar_origin + egui::vec2(0.0, -4.0),
            Align2::LEFT_BOTTOM,
            if bar_meters >= 1000.0 {
                format!("{:.1} km", bar_meters / 1000.0)
            } else {
                format!("{bar_meters:.0} m")
            },
            FontId::proportional(11.0),
            theme::plain(),
        );

        // 悬停坐标（BD09，经度在前）
        if let Some(hover) = response.hover_pos() {
            if rect.contains(hover) {
                let bd = to_lnglat(hover);
                painter.text(
                    Pos2::new(rect.left() + 10.0, rect.top() + 8.0),
                    Align2::LEFT_TOP,
                    format!("BD 经度 {:.6} · 纬度 {:.6}", bd[0], bd[1]),
                    FontId::monospace(11.0),
                    theme::text_dim(),
                );
            }
        }
    }
}

impl App {
    pub fn draw_campus(&mut self, ui: &mut egui::Ui) {
        self.campus_page.show(ui);
    }
}

fn parse_number(label: &str, text: &str, errors: &mut Vec<String>) -> Option<f64> {
    match text.trim().parse::<f64>() {
        Ok(value) if value.is_finite() => Some(value),
        Ok(_) => {
            errors.push(format!("{label}不是有限数值"));
            None
        }
        Err(_) => {
            errors.push(format!("{label}不是数字：{text:?}"));
            None
        }
    }
}

fn parse_coordinate(label: &str, text: &str) -> Result<f64, String> {
    let value = text
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("{label}不是数字：{text:?}"))?;
    if !value.is_finite() {
        return Err(format!("{label}不是有限数值"));
    }
    Ok(value)
}

/// BD09 坐标 → 画布屏幕坐标（以校园质心投影为原点，Y 轴向上为北）。
fn canvas_lnglat_to_pos(
    projection: &geo::Projection,
    center_xy: [f64; 2],
    rect_center: Pos2,
    scale: f32,
    pan: Vec2,
    point: LngLat,
) -> Pos2 {
    let xy = projection.xy(point);
    Pos2::new(
        rect_center.x + (xy[0] - center_xy[0]) as f32 * scale + pan.x,
        rect_center.y - (xy[1] - center_xy[1]) as f32 * scale + pan.y,
    )
}

/// 画布屏幕坐标 → BD09 坐标（拾取点用，与 [`canvas_lnglat_to_pos`] 互逆）。
fn canvas_pos_to_lnglat(
    projection: &geo::Projection,
    center_xy: [f64; 2],
    rect_center: Pos2,
    scale: f32,
    pan: Vec2,
    pos: Pos2,
) -> LngLat {
    let x = center_xy[0] + ((pos.x - rect_center.x - pan.x) / scale) as f64;
    let y = center_xy[1] - ((pos.y - rect_center.y - pan.y) / scale) as f64;
    projection.lnglat([x, y])
}

fn poi_count_warnings(campus: &Campus) -> Vec<String> {
    let mut warnings = Vec::new();
    for kind in PoiKind::ALL {
        let count = campus.pois.get(kind).len();
        if !(2..=4).contains(&count) {
            warnings.push(format!(
                "{} POI 数量 {count}，建议每类 2–4 个",
                kind.label()
            ));
        }
    }
    warnings
}

fn fmt_time(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".into())
}

fn nice_scale_m(target: f32) -> f32 {
    for meters in [10.0, 20.0, 50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0] {
        if meters >= target {
            return meters;
        }
    }
    2000.0
}

/// 用耳切法三角化简单多边形（egui 凸多边形填充不支持凹多边形）。
fn fill_polygon(painter: &egui::Painter, points: &[Pos2], color: Color32) {
    if points.len() < 3 {
        return;
    }
    let triangles = ear_clip(points);
    if triangles.is_empty() {
        return;
    }
    let mut mesh = egui::Mesh::default();
    for point in points {
        mesh.colored_vertex(*point, color);
    }
    for triangle in triangles {
        mesh.add_triangle(triangle[0], triangle[1], triangle[2]);
    }
    painter.add(egui::Shape::mesh(mesh));
}

fn screen_cross(a: Pos2, b: Pos2, c: Pos2) -> f32 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn point_in_triangle(p: Pos2, a: Pos2, b: Pos2, c: Pos2) -> bool {
    let d1 = screen_cross(a, b, p);
    let d2 = screen_cross(b, c, p);
    let d3 = screen_cross(c, a, p);
    let has_negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_negative && has_positive)
}

fn ear_clip(points: &[Pos2]) -> Vec<[u32; 3]> {
    let n = points.len();
    if n < 3 {
        return Vec::new();
    }
    // 去掉相邻重复点与闭合重复点
    let mut indices: Vec<usize> = Vec::with_capacity(n);
    for index in 0..n {
        if indices
            .last()
            .map(|last| points[*last].distance(points[index]) > 0.01)
            .unwrap_or(true)
        {
            indices.push(index);
        }
    }
    if indices.len() > 1 {
        let first = *indices.first().expect("非空");
        let last = *indices.last().expect("非空");
        if points[first].distance(points[last]) <= 0.01 {
            indices.pop();
        }
    }
    if indices.len() < 3 {
        return Vec::new();
    }
    let count = indices.len();

    // 凸多边形直接扇形三角化（校园外边界常见且顶点多）
    let mut sign = 0.0_f32;
    let mut convex = true;
    for i in 0..count {
        let a = points[indices[i]];
        let b = points[indices[(i + 1) % count]];
        let c = points[indices[(i + 2) % count]];
        let cross = screen_cross(a, b, c);
        if cross.abs() < 1e-4 {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            convex = false;
            break;
        }
    }
    if convex {
        let mut triangles = Vec::with_capacity(count.saturating_sub(2));
        for i in 1..(count - 1) {
            triangles.push([indices[0] as u32, indices[i] as u32, indices[i + 1] as u32]);
        }
        return triangles;
    }
    // 顶点过多的凹多边形只描边，避免每帧耳切开销
    if count > 256 {
        return Vec::new();
    }

    let mut double_area = 0.0_f32;
    for i in 0..indices.len() {
        let a = points[indices[i]];
        let b = points[indices[(i + 1) % indices.len()]];
        double_area += a.x * b.y - b.x * a.y;
    }
    let counter_clockwise = double_area > 0.0;
    let mut triangles = Vec::new();
    let mut guard = 0usize;
    while indices.len() > 3 && guard < n * n {
        guard += 1;
        let m = indices.len();
        let mut clipped = false;
        for i in 0..m {
            let previous = indices[(i + m - 1) % m];
            let current = indices[i];
            let next = indices[(i + 1) % m];
            let a = points[previous];
            let b = points[current];
            let c = points[next];
            let cross = screen_cross(a, b, c);
            if (counter_clockwise && cross <= 1e-4) || (!counter_clockwise && cross >= -1e-4) {
                continue;
            }
            let mut contains = false;
            for &candidate in &indices {
                if candidate == previous || candidate == current || candidate == next {
                    continue;
                }
                if point_in_triangle(points[candidate], a, b, c) {
                    contains = true;
                    break;
                }
            }
            if contains {
                continue;
            }
            triangles.push([previous as u32, current as u32, next as u32]);
            indices.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            break;
        }
    }
    if indices.len() == 3 {
        triangles.push([
            indices[0] as u32,
            indices[1] as u32,
            indices[2] as u32,
        ]);
    }
    triangles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_scale_is_readable() {
        assert_eq!(nice_scale_m(80.0), 100.0);
        assert_eq!(nice_scale_m(210.0), 500.0);
        assert_eq!(nice_scale_m(5000.0), 2000.0);
    }

    #[test]
    fn ear_clipping_handles_square_and_concave_polygon() {
        let square = [
            Pos2::new(0.0, 0.0),
            Pos2::new(10.0, 0.0),
            Pos2::new(10.0, 10.0),
            Pos2::new(0.0, 10.0),
        ];
        assert_eq!(ear_clip(&square).len(), 2);

        let concave = [
            Pos2::new(0.0, 0.0),
            Pos2::new(10.0, 0.0),
            Pos2::new(10.0, 10.0),
            Pos2::new(5.0, 3.0),
            Pos2::new(0.0, 10.0),
        ];
        let triangles = ear_clip(&concave);
        assert_eq!(triangles.len(), 3);
        for triangle in triangles {
            for index in triangle {
                assert!((index as usize) < concave.len());
            }
        }
    }

    #[test]
    fn parse_number_rejects_garbage() {
        let mut errors = Vec::new();
        assert_eq!(parse_number("海拔", " 82.5 ", &mut errors), Some(82.5));
        assert!(errors.is_empty());
        assert_eq!(parse_number("海拔", "NaN", &mut errors), None);
        assert!(errors[0].contains("有限数值"));
        assert_eq!(parse_number("净空", "abc", &mut errors), None);
    }

    #[test]
    fn canvas_click_mapping_round_trips_bd_coordinates() {
        let projection = geo::Projection::at(30.5, 114.3);
        let center_xy = [0.0, 0.0];
        let rect_center = Pos2::new(500.0, 300.0);
        let scale = 1.25_f32;
        let pan = Vec2::new(12.0, -8.0);
        let bd = [114.3012, 30.5011];
        let pos = canvas_lnglat_to_pos(&projection, center_xy, rect_center, scale, pan, bd);
        let back = canvas_pos_to_lnglat(&projection, center_xy, rect_center, scale, pan, pos);
        assert!((back[0] - bd[0]).abs() < 1e-9, "lng={}", back[0]);
        assert!((back[1] - bd[1]).abs() < 1e-9, "lat={}", back[1]);

        // 校区质心投影到画布中心 + 平移量
        let center_bd = projection.lnglat(center_xy);
        let center_pos =
            canvas_lnglat_to_pos(&projection, center_xy, rect_center, scale, pan, center_bd);
        assert!((center_pos.x - (rect_center.x + pan.x)).abs() < 1e-3);
        assert!((center_pos.y - (rect_center.y + pan.y)).abs() < 1e-3);
    }

    #[test]
    fn poi_page_add_edit_delete_persists_and_rejects_invalid() {
        use crate::campus::{BlockedArea, Polygon};
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "greenland-poi-page-{}-{nanos}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::platform::TEST_DATA_DIR.with(|slot| *slot.borrow_mut() = Some(dir.clone()));

        let mut campus = Campus::new(
            Polygon::new(vec![
                [114.298, 30.498],
                [114.302, 30.498],
                [114.302, 30.502],
                [114.298, 30.502],
            ]),
            1_000,
        );
        campus.blocked.push(BlockedArea {
            id: 1,
            name: "图书馆".into(),
            polygon: Polygon::new(vec![
                [114.2990, 30.4990],
                [114.3000, 30.4990],
                [114.3000, 30.5000],
                [114.2990, 30.5000],
            ]),
        });
        let stats = campus.stats();
        let mut page = CampusPage::default();
        page.apply_campus(campus, stats, Vec::new());

        page.poi_kind = PoiKind::Canteen;
        page.poi_name = "一食堂".into();
        page.poi_lng = "114.2985".into();
        page.poi_lat = "30.4985".into();
        page.add_poi();
        assert!(page.poi_message_ok, "{}", page.poi_message);
        assert_eq!(page.campus.as_ref().unwrap().pois.canteen.len(), 1);
        let saved = store::load_from(&dir).unwrap().unwrap();
        assert_eq!(saved.pois.canteen.len(), 1);
        assert_eq!(saved.pois.canteen[0].name, "一食堂");

        // 禁行区内的坐标必须被拒绝且不写盘
        page.poi_name = "压楼食堂".into();
        page.poi_lng = "114.2995".into();
        page.poi_lat = "30.4995".into();
        page.add_poi();
        assert!(!page.poi_message_ok);
        assert!(page.poi_message.contains("禁行区「图书馆」"), "{}", page.poi_message);
        assert_eq!(page.campus.as_ref().unwrap().pois.canteen.len(), 1);
        assert_eq!(store::load_from(&dir).unwrap().unwrap().pois.canteen.len(), 1);

        // 编辑名称/坐标
        page.select_poi(PoiKind::Canteen, 0);
        page.poi_name = "中心食堂".into();
        page.poi_lng = "114.3015".into();
        page.poi_lat = "30.5015".into();
        page.save_poi_edit();
        assert!(page.poi_message_ok, "{}", page.poi_message);
        let saved = store::load_from(&dir).unwrap().unwrap();
        assert_eq!(saved.pois.canteen[0].name, "中心食堂");
        assert_eq!(saved.pois.canteen[0].pos, [114.3015, 30.5015]);

        // 删除
        page.delete_poi(PoiKind::Canteen, 0, "中心食堂");
        assert!(page.poi_message_ok, "{}", page.poi_message);
        assert!(page.campus.as_ref().unwrap().pois.canteen.is_empty());
        assert!(store::load_from(&dir).unwrap().unwrap().pois.canteen.is_empty());

        crate::platform::TEST_DATA_DIR.with(|slot| *slot.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
