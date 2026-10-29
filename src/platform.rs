//! Platform services shared by the existing UI and the Android host.

#[derive(Clone, Copy, Debug)]
pub enum InputKind {
    Text,
    Password,
    Integer,
    Decimal,
}

pub fn data_dir() -> std::path::PathBuf {
    // 显式覆盖：自动化测试 / 命令行跑真实流程时，指定数据目录用。
    if let Ok(dir) = std::env::var("ESVIM_DATA_DIR") {
        if !dir.trim().is_empty() {
            return std::path::PathBuf::from(dir);
        }
    }
    #[cfg(test)]
    if let Some(path) = TEST_DATA_DIR.with(|p| p.borrow().clone()) {
        return path;
    }
    #[cfg(target_os = "android")]
    {
        crate::android::data_dir()
    }
    #[cfg(not(target_os = "android"))]
    {
        desktop_data_dir()
    }
}

/// 目录是否真的可写（权限/安全软件锁文件时，create_dir_all 会成功但写入被拒）。
#[cfg(not(target_os = "android"))]
fn is_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(".write_probe");
    match std::fs::write(&probe, b"1") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// 桌面端数据目录解析顺序：
///   ① `%APPDATA%\GreenlandData`（用户目录，永远可写）
///   ② `ESVIM_LEGACY_DATA_DIR` 指定的历史目录（旧版本把数据放在独立盘符目录，需要兼容时设置）
///   ③ exe 所在目录
///
/// 以前只判断「目录能不能创建」，创建成功就用历史目录，结果该目录被安全软件锁住写入时，
/// 账号/勾选全部静默不落盘。现在加真实写探测。
#[cfg(not(target_os = "android"))]
fn desktop_data_dir() -> std::path::PathBuf {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // 历史数据目录：旧版本固定放在一个独立盘符目录下。开源版本不写死具体路径，
        // 需要搬迁旧数据的用户把 ESVIM_LEGACY_DATA_DIR 指过去即可。
        let legacy_override = std::env::var_os("ESVIM_LEGACY_DATA_DIR")
            .map(std::path::PathBuf::from)
            .filter(|dir| dir.is_dir());
        // ① 首选 %APPDATA%\GreenlandData
        //
        // 为什么不放独立盘符：普通权限下写入会被拒，于是数据目录回退到 exe 目录 ——
        // 结果「普通运行」和「管理员运行」各读各的一份 accounts.json，看到的账号
        // 名单完全不同。%APPDATA% 对普通权限和提权后的同一用户都是同一个路径，天然一致。
        if let Ok(appdata) = std::env::var("APPDATA") {
            let alt = std::path::Path::new(&appdata).join("GreenlandData");
            // 只判断「目录能不能建出来」，不再做写探测：
            // 写探测会被安全软件/沙箱误判，反而把数据目录踢到 exe 旁边，
            // 造成「普通运行 / 管理员运行」各读一份。真写不进去会有 persist_error.log。
            if std::fs::create_dir_all(&alt).is_ok() {
                // 品牌改名前的旧数据目录：先整份搬过来（只复制不覆盖）。
                // 漏了 identity.json 会让老用户变成「新设备」，直接踩 10121 设备风险。
                let old_brand = std::path::Path::new(&appdata).join("Esvim定制款刷跑Data");
                migrate_legacy_configs(&old_brand, &alt);
                // 再从历史目录搬数据，最后从 exe 目录搬（都只复制不覆盖）
                if let Some(legacy) = legacy_override.as_deref() {
                    migrate_legacy_configs(legacy, &alt);
                }
                migrate_legacy_configs(&exe_dir, &alt);
                return alt;
            }
        }
        // ② %APPDATA% 也不可用才退回历史目录（通常是提权后运行）
        if let Some(legacy) = legacy_override.as_deref() {
            if is_writable(legacy) {
                migrate_legacy_configs(&exe_dir, legacy);
                mark_hidden(legacy);
                return legacy.to_path_buf();
            }
        }
        // ③ 最后退回 exe 目录
        exe_dir
    })
    .clone()
}

/// 把旧的 exe 同目录配置复制到新数据目录（只复制不删除，已有文件不覆盖）。
#[cfg(not(target_os = "android"))]
fn migrate_legacy_configs(legacy: &std::path::Path, primary: &std::path::Path) {
    if legacy == primary {
        return;
    }
    for name in [
        "session.json",
        "accounts.json",
        "config.json",
        "identity.json",
        "points_cache.json",
        "ai_sports.json",
        // 校园地图：漏了它会导致「校园和路网消失」（新目录里没有 campus.json）
        "campus.json",
        "campus_new.geojson",
        "roads_for_qgis.geojson",
        "roads_qgis_v2.geojson",
    ] {
        let from = legacy.join(name);
        let to = primary.join(name);
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

/// 给目录打 Windows 隐藏属性（启动时刷一次即可，进程内只执行一回）。
#[cfg(all(not(target_os = "android"), windows))]
fn mark_hidden(path: &std::path::Path) {
    let _ = std::process::Command::new("attrib").arg("+h").arg(path).status();
}

#[cfg(all(not(target_os = "android"), not(windows)))]
fn mark_hidden(_path: &std::path::Path) {}

pub fn edit_text(id: u64, value: &str, kind: InputKind) {
    #[cfg(target_os = "android")]
    crate::android::edit_text(id, value, kind);
    #[cfg(not(target_os = "android"))]
    let _ = (id, value, kind);
}

pub fn take_edited_text(id: u64) -> Option<String> {
    #[cfg(target_os = "android")]
    { crate::android::take_edited_text(id) }
    #[cfg(not(target_os = "android"))]
    { let _ = id; None }
}

pub fn set_keep_screen_on(enabled: bool) {
    #[cfg(target_os = "android")]
    crate::android::set_keep_screen_on(enabled);
    #[cfg(not(target_os = "android"))]
    let _ = enabled;
}

/// 当前程序版本：桌面读 Cargo 版本，Android 读 manifest versionName
/// （形如 0.2.5-android.3，与 Release tag 对齐）。
pub fn version_name() -> String {
    #[cfg(target_os = "android")]
    { crate::android::version_name() }
    #[cfg(not(target_os = "android"))]
    { env!("CARGO_PKG_VERSION").to_string() }
}

/// Android：把已下载的 APK 交给系统安装器（用户在系统弹窗确认安装）。
pub fn install_apk(path: &str) {
    #[cfg(target_os = "android")]
    crate::android::install_apk(path);
    #[cfg(not(target_os = "android"))]
    let _ = path;
}

#[cfg(feature = "gui")]
pub fn sync_clipboard(context: &egui::Context) {
    #[cfg(target_os = "android")]
    {
        let text = context.output(|output| output.copied_text.clone());
        if !text.is_empty() { crate::android::copy_text(&text); }
    }
    #[cfg(not(target_os = "android"))]
    let _ = context;
}

#[cfg(feature = "gui")]
pub fn apply_safe_area(context: &egui::Context, input: &mut egui::RawInput) {
    #[cfg(target_os = "android")]
    if let Some(rect) = input.screen_rect.as_mut() {
        let [left, top, right, bottom] = crate::android::safe_insets();
        let scale = context.pixels_per_point();
        rect.min.x += left as f32 / scale;
        rect.min.y += top as f32 / scale;
        rect.max.x -= right as f32 / scale;
        rect.max.y -= bottom as f32 / scale;
    }
    #[cfg(not(target_os = "android"))]
    let _ = (context, input);
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_DATA_DIR: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
