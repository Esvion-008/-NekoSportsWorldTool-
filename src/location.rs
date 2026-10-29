//! 运行位置的统一数据结构与校验。
//!
//! 设备锚点、点位请求和跑步记录使用同一套纬度/经度语义。

use serde::{Deserialize, Serialize};

/// 未配置时的占位锚点（中立值，与作者及任何学校的实际位置无关）。
const DEFAULT_ANCHOR_LAT: f64 = 39.916527;
const DEFAULT_ANCHOR_LON: f64 = 116.397128;
/// 未配置时的占位城市名。刻意取不可能与真实城市重名的名称，
/// 这样「用户填了城市」和「仍未配置」可以严格区分。
const DEFAULT_CITY: &str = "示例市";

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: f64,
}

impl Coordinate {
    pub fn new(latitude: f64, longitude: f64, accuracy: f64) -> Result<Self, String> {
        let coordinate = Self {
            latitude,
            longitude,
            accuracy,
        };
        coordinate.validate()?;
        Ok(coordinate)
    }

    pub fn validate(self) -> Result<(), String> {
        if !self.latitude.is_finite() || !(-90.0..=90.0).contains(&self.latitude) {
            return Err(format!("纬度超出范围: {}", self.latitude));
        }
        if !self.longitude.is_finite() || !(-180.0..=180.0).contains(&self.longitude) {
            return Err(format!("经度超出范围: {}", self.longitude));
        }
        if !self.accuracy.is_finite() || self.accuracy < 0.0 {
            return Err(format!("定位精度无效: {}", self.accuracy));
        }
        Ok(())
    }

    pub fn is_near(self, other: Self, tolerance: f64) -> bool {
        (self.latitude - other.latitude).abs() <= tolerance
            && (self.longitude - other.longitude).abs() <= tolerance
    }

    pub fn is_legacy_default(self) -> bool {
        self.is_near(
            Self {
                latitude: DEFAULT_ANCHOR_LAT,
                longitude: DEFAULT_ANCHOR_LON,
                accuracy: 0.0,
            },
            1e-6,
        )
    }
}

pub fn default_anchor() -> Coordinate {
    Coordinate {
        latitude: DEFAULT_ANCHOR_LAT,
        longitude: DEFAULT_ANCHOR_LON,
        accuracy: 0.0,
    }
}

pub fn default_city() -> &'static str {
    DEFAULT_CITY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_bounds_and_accuracy() {
        assert!(Coordinate::new(39.9, 116.4, 5.0).is_ok());
        assert!(Coordinate::new(91.0, 116.4, 5.0).is_err());
        assert!(Coordinate::new(39.9, 181.0, 5.0).is_err());
        assert!(Coordinate::new(39.9, 116.4, -1.0).is_err());
    }

    #[test]
    fn detects_legacy_default_location() {
        assert!(default_anchor().is_legacy_default());
        assert!(!Coordinate::new(30.5, 114.3, 0.0)
            .unwrap()
            .is_legacy_default());
    }
}
