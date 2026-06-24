//! ArkSIM trackId normalization — FireAtTarget requires `<platform>:<number>`.

/// FireAtTarget 要求 `<platform>:<number>`；evt 日志常写成 `<platform>.<number>`。
pub fn normalize_track_id(track_id: &str) -> String {
    let tid = track_id.trim();
    if tid.contains(':') {
        return tid.to_string();
    }
    if let Some((name, num)) = tid.rsplit_once('.') {
        if !name.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
            return format!("{name}:{num}");
        }
    }
    tid.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colon_format_unchanged() {
        assert_eq!(normalize_track_id("self:1"), "self:1");
    }

    #[test]
    fn dot_format_becomes_colon() {
        assert_eq!(normalize_track_id("self.1"), "self:1");
        assert_eq!(normalize_track_id("self.5"), "self:5");
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(normalize_track_id("  self.2  "), "self:2");
    }
}
