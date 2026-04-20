//! Platform detection for social media URLs.

/// Which social platform a URL points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Twitter,
    Instagram,
    TikTok,
    YouTube,
    Other,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Twitter => "twitter",
            Platform::Instagram => "instagram",
            Platform::TikTok => "tiktok",
            Platform::YouTube => "youtube",
            Platform::Other => "other",
        }
    }
}

/// Classify a URL by host.
///
/// Tolerant of `http://`, `https://`, schemeless, or `www.` prefixes.
pub fn detect_platform(url: &str) -> Platform {
    let lowered = url.trim().to_ascii_lowercase();
    // Strip scheme.
    let rest = lowered
        .strip_prefix("https://")
        .or_else(|| lowered.strip_prefix("http://"))
        .unwrap_or(&lowered);
    // Extract host (up to the first `/` or `?`).
    let host_end = rest.find(|c| c == '/' || c == '?').unwrap_or(rest.len());
    let host = rest[..host_end].trim_start_matches("www.");

    if host == "x.com" || host == "twitter.com" || host.ends_with(".twitter.com") {
        Platform::Twitter
    } else if host == "instagram.com" || host.ends_with(".instagram.com") {
        Platform::Instagram
    } else if host == "tiktok.com"
        || host.ends_with(".tiktok.com")
        || host == "vm.tiktok.com"
    {
        Platform::TikTok
    } else if host == "youtube.com"
        || host.ends_with(".youtube.com")
        || host == "youtu.be"
    {
        Platform::YouTube
    } else {
        Platform::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_all_platforms() {
        assert_eq!(
            detect_platform("https://x.com/elonmusk/status/1234567890"),
            Platform::Twitter
        );
        assert_eq!(
            detect_platform("https://twitter.com/user/status/1"),
            Platform::Twitter
        );
        assert_eq!(
            detect_platform("https://www.instagram.com/reel/ABC123/"),
            Platform::Instagram
        );
        assert_eq!(
            detect_platform("https://www.tiktok.com/@user/video/7123456789"),
            Platform::TikTok
        );
        assert_eq!(
            detect_platform("https://vm.tiktok.com/ZMabc/"),
            Platform::TikTok
        );
        assert_eq!(
            detect_platform("https://www.youtube.com/shorts/abcDEF"),
            Platform::YouTube
        );
        assert_eq!(
            detect_platform("https://youtu.be/abcDEF"),
            Platform::YouTube
        );
        assert_eq!(
            detect_platform("https://example.com/something"),
            Platform::Other
        );
    }
}
