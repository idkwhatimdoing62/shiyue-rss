//! Download and decode an HTML page for a local reading snapshot.
//!
//! The GUI calls this module from a worker thread: all APIs here are blocking.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chardetng::EncodingDetector;
use encoding_rs::Encoding;
use reqwest::Url;
use reqwest::blocking::{Client, Response};
use reqwest::header::CONTENT_TYPE;
use reqwest::redirect::Policy;

/// Maximum HTML response size after HTTP content decoding (gzip, Brotli, etc.).
pub const MAX_HTML_BYTES: usize = 8 * 1024 * 1024;

/// A downloaded HTML snapshot and the URLs needed to preserve its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedWebClip {
    /// The normalized URL supplied by the user (without a fragment).
    pub original_url: String,
    /// The final response URL after redirects. Use this as the base for relative links.
    pub final_url: String,
    /// The response body decoded to Unicode.
    pub html: String,
}

/// Build the blocking HTTP client intended for webpage clipping.
pub fn client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .redirect(Policy::custom(|attempt| {
            // `previous` includes the initial URL. This permits at most ten redirects.
            if attempt.previous().len() > 10 {
                return attempt.error("网页重定向次数过多");
            }
            if let Err(message) = validate_public_url(attempt.url()) {
                return attempt.error(message);
            }
            attempt.follow()
        }))
        .user_agent(concat!(
            "Shiyue/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/idkwhatimdoing62/shiyue-rss)"
        ))
        .build()
        .context("无法创建网页下载客户端")
}

/// Fetch one HTTP(S) HTML document and decode it to Unicode.
///
/// The supplied client should normally come from [`client`]. Keeping it as a
/// parameter lets a GUI worker reuse connection pools across multiple saves.
pub fn fetch_html(client: &Client, input: &str) -> Result<FetchedWebClip> {
    let mut original = Url::parse(input.trim()).context("网页地址格式不正确")?;
    validate_public_url(&original).map_err(anyhow::Error::msg)?;
    // Fragments are local document locations and are never sent to the server.
    original.set_fragment(None);

    let response = client
        .get(original.clone())
        .send()
        .context("无法连接网页")?;
    response_to_clip(original, response)
}

fn response_to_clip(original: Url, mut response: Response) -> Result<FetchedWebClip> {
    let status = response.status();
    if !status.is_success() {
        bail!("网页返回 HTTP {}", status.as_u16());
    }

    let final_url = response.url().clone();
    validate_public_url(&final_url).map_err(anyhow::Error::msg)?;
    if let Some(peer) = response.remote_addr()
        && !is_public_ip(peer.ip())
    {
        bail!("为保护本机数据，已阻止网页连接到本机或内网地址");
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .map(|value| {
            value
                .to_str()
                .context("网页响应的 Content-Type 无效")
                .map(str::to_owned)
        })
        .transpose()?;
    if let Some(content_type) = content_type.as_deref() {
        validate_html_content_type(content_type)?;
    }

    // Reqwest's decompression middleware sits below `Response::read`, so this
    // limit applies to decompressed bytes rather than the compressed wire size.
    let bytes = read_body_limited(&mut response)?;

    // Some small/personal sites omit this header entirely. In that case the
    // response is accepted only when its first bytes actually look like HTML.
    if content_type.is_none() && !looks_like_html(&bytes) {
        bail!("该地址没有返回可识别的 HTML");
    }

    let html = decode_html(&bytes, content_type.as_deref().and_then(http_charset));
    Ok(FetchedWebClip {
        original_url: original.to_string(),
        final_url: final_url.to_string(),
        html,
    })
}

fn read_body_limited(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(64 * 1024);
    reader
        .take((MAX_HTML_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("读取网页内容失败")?;
    if bytes.len() > MAX_HTML_BYTES {
        bail!("网页 HTML 超过 8 MiB，未保存");
    }
    Ok(bytes)
}

/// Validate a network target before connecting to it.
///
/// Besides rejecting unsafe URL syntax and IP literals, domain names are
/// resolved here and the request is rejected when *any* answer points at a
/// non-public address. The redirect policy calls this function for every hop;
/// callers that download secondary resources (for example article images) can
/// reuse the same check. The connected peer is still checked separately after
/// the request to narrow the DNS-rebinding window.
pub(crate) fn validate_public_url(url: &Url) -> std::result::Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("只支持 http:// 或 https:// 网页地址".to_owned());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("网页地址不能包含用户名或密码".to_owned());
    }
    let Some(host) = url.host_str() else {
        return Err("网页地址缺少主机名".to_owned());
    };

    let normalized_host = host.trim_end_matches('.');
    if normalized_host.eq_ignore_ascii_case("localhost")
        || normalized_host.to_ascii_lowercase().ends_with(".localhost")
    {
        return Err("为保护本机数据，不能访问本机或内网地址".to_owned());
    }

    // `url` normalizes unusual IPv4 spellings (for example 2130706433 or
    // 0x7f000001) before exposing `host_str`, so parsing the normalized value
    // also covers those common loopback-filter bypasses.
    if let Ok(address) = normalized_host.parse::<IpAddr>() {
        return if is_public_ip(address) {
            Ok(())
        } else {
            Err("为保护本机数据，不能访问本机或内网地址".to_owned())
        };
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| "网页地址缺少有效端口".to_owned())?;
    let addresses = (normalized_host, port)
        .to_socket_addrs()
        .map_err(|error| format!("无法解析网页主机名：{error}"))?;
    let mut found = false;
    for address in addresses {
        found = true;
        if !is_public_ip(address.ip()) {
            return Err("为保护本机数据，网页主机名不能解析到本机或内网地址".to_owned());
        }
    }
    if !found {
        return Err("网页主机名没有可用的 IP 地址".to_owned());
    }
    Ok(())
}

/// Reject addresses that are not globally routable. This check is also used
/// immediately before image requests and after HTTP connections are created.
pub(crate) fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

fn validate_html_content_type(content_type: &str) -> Result<()> {
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if matches!(media_type.as_str(), "text/html" | "application/xhtml+xml") {
        Ok(())
    } else {
        bail!("该地址返回的不是 HTML（Content-Type: {media_type}）")
    }
}

fn looks_like_html(bytes: &[u8]) -> bool {
    let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let prefix = prefix.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let prefix = prefix.to_ascii_lowercase();
    prefix.starts_with("<!doctype html")
        || prefix.starts_with("<html")
        || prefix.starts_with("<head")
        || prefix.starts_with("<body")
        || prefix.starts_with("<!--")
}

fn http_charset(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let value = value.trim().trim_matches(['\'', '"']);
        (!value.is_empty()).then_some(value)
    })
}

fn decode_html(bytes: &[u8], http_label: Option<&str>) -> String {
    // A byte-order mark is authoritative, even when the server sends a stale
    // or incorrect charset header.
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        return encoding.decode(&bytes[bom_len..]).0.into_owned();
    }

    if let Some(encoding) = http_label.and_then(|label| Encoding::for_label(label.as_bytes())) {
        return encoding.decode(bytes).0.into_owned();
    }

    if let Some(label) = meta_charset(bytes)
        && let Some(encoding) = Encoding::for_label(label.as_bytes())
    {
        return encoding.decode(bytes).0.into_owned();
    }

    let mut detector = EncodingDetector::new();
    detector.feed(bytes, true);
    detector.guess(None, true).decode(bytes).0.into_owned()
}

/// Sniff a charset declaration from `<meta ...>` elements near the document start.
fn meta_charset(bytes: &[u8]) -> Option<String> {
    // HTML's encoding declaration is expected in the first 1024 bytes. Looking
    // a little farther accommodates common generated pages without scanning an
    // arbitrarily large response.
    let prefix = &bytes[..bytes.len().min(4096)];
    let ascii_lower: Vec<u8> = prefix.iter().map(u8::to_ascii_lowercase).collect();
    let mut cursor = 0;

    while let Some(relative) = find_bytes(&ascii_lower[cursor..], b"<meta") {
        let start = cursor + relative;
        let end = ascii_lower[start..]
            .iter()
            .position(|byte| *byte == b'>')
            .map(|offset| start + offset)
            .unwrap_or(ascii_lower.len());
        let tag = &ascii_lower[start..end];
        if let Some(relative) = find_bytes(tag, b"charset") {
            let mut pos = relative + b"charset".len();
            while matches!(tag.get(pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                pos += 1;
            }
            if tag.get(pos) != Some(&b'=') {
                cursor = end.saturating_add(1);
                continue;
            }
            pos += 1;
            while matches!(tag.get(pos), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                pos += 1;
            }

            let quote = tag
                .get(pos)
                .copied()
                .filter(|byte| matches!(byte, b'\'' | b'"'));
            if quote.is_some() {
                pos += 1;
            }
            let value_start = pos;
            while let Some(byte) = tag.get(pos) {
                let done = quote.map_or_else(
                    || matches!(byte, b' ' | b'\t' | b'\r' | b'\n' | b';' | b'/' | b'>'),
                    |quote| *byte == quote,
                );
                if done {
                    break;
                }
                pos += 1;
            }
            if pos > value_start {
                return Some(String::from_utf8_lossy(&tag[value_start..pos]).into_owned());
            }
        }
        cursor = end.saturating_add(1);
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::GBK;
    use std::io::Cursor;

    #[test]
    fn rejects_non_http_and_credentials_before_connecting() {
        for target in [
            "file:///c:/secret.html",
            "https://user:secret@example.com/",
            "http://127.0.0.1/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://[::1]/",
            "http://localhost/",
            "http://reader.localhost./",
        ] {
            let url = Url::parse(target).unwrap();
            assert!(validate_public_url(&url).is_err(), "{target}");
        }

        assert!(validate_public_url(&Url::parse("https://8.8.8.8/").unwrap()).is_ok());
    }

    #[test]
    fn public_ip_filter_covers_private_special_and_documentation_ranges() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.2",
            "172.16.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "203.0.113.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn parses_quoted_http_charset() {
        assert_eq!(
            http_charset("text/html; boundary=x; Charset=\"gb18030\""),
            Some("gb18030")
        );
    }

    #[test]
    fn decodes_legacy_chinese_from_meta_charset() {
        let source = "<meta http-equiv=\"Content-Type\" content=\"text/html; charset=gb2312\"><p>旧中文编码测试</p>";
        let (bytes, _, _) = GBK.encode(source);
        assert_eq!(decode_html(&bytes, None), source);
    }

    #[test]
    fn bom_takes_priority_over_http_charset() {
        let source = "<p>拾阅</p>";
        let mut bytes = vec![0xff, 0xfe];
        for unit in source.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_html(&bytes, Some("windows-1252")), source);
    }

    #[test]
    fn refuses_more_than_the_decompressed_limit() {
        let source = vec![b'x'; MAX_HTML_BYTES + 1];
        let error = read_body_limited(&mut Cursor::new(source)).unwrap_err();
        assert!(error.to_string().contains("超过 8 MiB"));
    }
}
