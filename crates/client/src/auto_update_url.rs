//! Normalisation rules for the auto-update server URL setting.

/// Normalises a user-supplied auto-update server URL.
///
/// Returns `Some(url)` only for `http://` or `https://` URLs that parse cleanly.
/// Trims whitespace, strips trailing slashes (path joining always uses
/// a leading `/`), and rejects empty, whitespace-only, scheme-less, or
/// non-http(s) input as `None`.
///
/// `None` from this function means "no valid server configured"; in
/// the Fork channel this disables polling outright.
pub fn normalize_update_server_url(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() {
        return None;
    }
    let url = url::Url::parse(s).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let mut out = url.to_string();
    while out.ends_with('/') {
        out.pop();
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::normalize_update_server_url;

    #[test]
    fn none_input_returns_none() {
        assert_eq!(normalize_update_server_url(None), None);
    }

    #[test]
    fn empty_string_returns_none() {
        assert_eq!(normalize_update_server_url(Some("")), None);
    }

    #[test]
    fn whitespace_only_returns_none() {
        assert_eq!(normalize_update_server_url(Some("   ")), None);
    }

    #[test]
    fn missing_scheme_returns_none() {
        assert_eq!(normalize_update_server_url(Some("intra.update.corp")), None);
    }

    #[test]
    fn non_http_scheme_returns_none() {
        assert_eq!(normalize_update_server_url(Some("ftp://x/")), None);
        assert_eq!(normalize_update_server_url(Some("ws://x/")), None);
    }

    #[test]
    fn http_no_trailing_slash_passes_through() {
        assert_eq!(
            normalize_update_server_url(Some("http://intra.update.corp")),
            Some("http://intra.update.corp".to_string()),
        );
    }

    #[test]
    fn http_single_trailing_slash_is_stripped() {
        assert_eq!(
            normalize_update_server_url(Some("http://intra.update.corp/")),
            Some("http://intra.update.corp".to_string()),
        );
    }

    #[test]
    fn http_multiple_trailing_slashes_are_stripped() {
        assert_eq!(
            normalize_update_server_url(Some("http://intra.update.corp//")),
            Some("http://intra.update.corp".to_string()),
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_then_normalised() {
        assert_eq!(
            normalize_update_server_url(Some("  http://intra.update.corp/  ")),
            Some("http://intra.update.corp".to_string()),
        );
    }
}
