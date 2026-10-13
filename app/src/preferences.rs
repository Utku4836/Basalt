//! Owned appearance preferences; reuse the backend's bounded/atomic file store.
use crate::typography::normalize_percent;
use basalt_core::{Error, ErrorKind, Result, storage::DataRoot};

pub const PALETTE_COUNT: i32 = 8;

#[derive(Clone)]
pub struct AppearancePreferences {
    root: DataRoot,
}
impl AppearancePreferences {
    pub fn new(root: DataRoot) -> Self {
        Self { root }
    }
    pub fn data_root(&self) -> DataRoot {
        self.root.clone()
    }
    fn document(&self) -> Result<serde_json::Value> {
        let Some(bytes) = self.root.read_limited("settings.json", 64 * 1024)? else {
            return Ok(serde_json::json!({"version":1}));
        };
        let document: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| Error::new(ErrorKind::CorruptData, error.to_string()))?;
        if !document.is_object()
            || document.get("version").and_then(|value| value.as_u64()) != Some(1)
        {
            return Err(Error::new(
                ErrorKind::CorruptData,
                "Unsupported appearance preference document",
            ));
        }
        Ok(document)
    }
    pub fn font_percent(&self) -> Result<i32> {
        let document = self.document()?;
        match document.get("font_magnification_percent") {
            None => Ok(100),
            Some(value) => value
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .map(normalize_percent)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::CorruptData,
                        "Invalid font magnification preference",
                    )
                }),
        }
    }
    pub fn set_font_percent(&self, percent: i32) -> Result<i32> {
        let mut document = self.document()?;
        let percent = normalize_percent(percent);
        document["font_magnification_percent"] = percent.into();
        self.root.write_json("settings.json", &document)?;
        Ok(percent)
    }
    pub fn terminal_font_percent(&self) -> Result<i32> {
        match self.document()?.get("terminal_font_magnification_percent") {
            None => Ok(100),
            Some(value) => value
                .as_i64()
                .and_then(|v| i32::try_from(v).ok())
                .map(normalize_percent)
                .ok_or_else(|| {
                    Error::new(ErrorKind::CorruptData, "Invalid terminal font preference")
                }),
        }
    }
    pub fn set_terminal_font_percent(&self, percent: i32) -> Result<i32> {
        let mut document = self.document()?;
        let percent = normalize_percent(percent);
        document["terminal_font_magnification_percent"] = percent.into();
        self.root.write_json("settings.json", &document)?;
        Ok(percent)
    }
    pub fn reset(&self) -> Result<()> {
        let mut document = self.document()?;
        document["font_magnification_percent"] = 100.into();
        document["terminal_font_magnification_percent"] = 100.into();
        document["color_palette"] = 0.into();
        self.root.write_json("settings.json", &document)
    }
    pub fn color_palette(&self) -> Result<i32> {
        match self.document()?.get("color_palette") {
            None => Ok(0),
            Some(value) => value
                .as_i64()
                .filter(|id| (0..i64::from(PALETTE_COUNT)).contains(id))
                .map(|id| id as i32)
                .ok_or_else(|| {
                    Error::new(ErrorKind::CorruptData, "Invalid color palette preference")
                }),
        }
    }
    pub fn set_color_palette(&self, palette: i32) -> Result<()> {
        if !(0..PALETTE_COUNT).contains(&palette) {
            return Err(Error::new(ErrorKind::InvalidInput, "Invalid color palette"));
        }
        let mut document = self.document()?;
        document["color_palette"] = palette.into();
        self.root.write_json("settings.json", &document)
    }
}
