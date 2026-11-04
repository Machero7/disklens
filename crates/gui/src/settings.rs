//! Settings in `~/.config/disklens/settings.json`, shared with the Python version.
//! Unknown keys (ones only the Python version uses) are kept when saving.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

pub struct Settings {
    raw: Map<String, Value>,
}

fn path() -> PathBuf {
    gtk::glib::user_config_dir().join("disklens").join("settings.json")
}

impl Settings {
    pub fn load() -> Self {
        let raw = std::fs::read_to_string(path())
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        Settings { raw }
    }

    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.raw.get(key).and_then(Value::as_bool).unwrap_or(default)
    }

    pub fn int(&self, key: &str, default: i64) -> i64 {
        self.raw.get(key).and_then(Value::as_f64).map(|v| v as i64).unwrap_or(default)
    }

    pub fn set(&mut self, key: &str, v: impl Into<Value>) {
        self.raw.insert(key.to_string(), v.into());
    }

    pub fn save(&self) {
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(&json!(self.raw)) {
            // write a temp file and rename it, so a crash can't leave a half-written file
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, s + "\n").is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}
