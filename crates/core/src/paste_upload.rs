//! Platform-independent half of the clipboard paste upload: the URL allow-list, the form body and the HTTP round-trip.
//! The clipboard, ShellExecute and the background thread live in eve_maj_app::paste_upload.

use crate::log::Scope;

const SLOG: Scope = Scope::new("paste_upload");
const USER_AGENT: &str = "EVE-Maj-Preview";
/// Matches std.http.Client's default redirect limit in the Zig build.
const MAX_REDIRECTS: u32 = 3;

/// Percent-encodes `text` as an application/x-www-form-urlencoded value (space becomes '+', per that format's convention rather than plain URI escaping).
pub fn form_url_encode(text: &[u8]) -> String {
    let mut out = String::with_capacity(text.len());
    for &c in text {
        match c {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(c as char),
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{c:02X}");
            }
        }
    }
    out
}

/// The form body aDashboard's paste-intake form expects, carrying `text` as the paste.
pub fn build_upload_body(text: &[u8]) -> String {
    format!("Paste+anything={}&submit=new", form_url_encode(text))
}

/// The clipboard may hold anything (passwords included), so it's only ever sent over HTTPS to the one paste site this feature is for - checked here, not just in the config dialog that normally sets the URL.
pub fn is_allowed_upload_url(url: &str) -> bool {
    let Ok(uri) = url::Url::parse(url) else { return false };
    if !uri.scheme().eq_ignore_ascii_case("https") {
        return false;
    }
    let Some(host) = uri.host_str() else { return false };
    host.eq_ignore_ascii_case("adashboard.info")
}

/// POSTs `body` to `url`, following the Post/Redirect/Get response paste sites use, and returns the final landing page's URL.
pub fn post_and_follow_redirect(url: &str, body: &str) -> Result<String, Box<ureq::Error>> {
    let agent = ureq::AgentBuilder::new().user_agent(USER_AGENT).redirects(MAX_REDIRECTS).build();
    let result = agent.post(url).set("Content-Type", "application/x-www-form-urlencoded").send_string(body);

    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            SLOG.warn(format_args!("Upload to {url} returned status {code}"));
            r
        }
        Err(err) => return Err(Box::new(err)),
    };

    let final_url = response.get_url().to_owned();
    if let Err(err) = std::io::copy(&mut response.into_reader(), &mut std::io::sink()) {
        SLOG.debug(format_args!("Failed to discard remaining response body: {err}"));
    }
    Ok(final_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_url_encode_matches_form_convention() {
        assert_eq!(form_url_encode(b"Hello World"), "Hello+World");
        assert_eq!(form_url_encode(b"a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(form_url_encode(b"a+b&c=d/e"), "a%2Bb%26c%3Dd%2Fe");
        assert_eq!(form_url_encode(b"line1\r\nline2\t"), "line1%0D%0Aline2%09");
        assert_eq!(form_url_encode("\u{e9}".as_bytes()), "%C3%A9");
        assert_eq!(form_url_encode(b""), "");
    }

    #[test]
    fn upload_body_shape() {
        assert_eq!(build_upload_body(b"1 Tritanium"), "Paste+anything=1+Tritanium&submit=new");
    }

    #[test]
    fn only_https_adashboard_is_allowed() {
        assert!(is_allowed_upload_url("https://adashboard.info/intel/dscan"));
        assert!(is_allowed_upload_url("HTTPS://ADashboard.Info/"));
        assert!(is_allowed_upload_url("https://adashboard.info"));
        assert!(!is_allowed_upload_url("http://adashboard.info/"));
        assert!(!is_allowed_upload_url("https://evil.example/adashboard.info"));
        assert!(!is_allowed_upload_url("https://adashboard.info.evil.example/"));
        assert!(!is_allowed_upload_url("https://adashboard.info@evil.example/"));
        assert!(!is_allowed_upload_url("https://sub.adashboard.info/"));
        assert!(!is_allowed_upload_url("adashboard.info"));
        assert!(!is_allowed_upload_url(""));
    }
}
