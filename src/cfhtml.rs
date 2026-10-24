//! Windows "HTML Format" (CF_HTML) clipboard encoding.
#![allow(dead_code)]

pub fn encode(html: &str) -> Vec<u8> {
    let lower = html.to_ascii_lowercase();
    let body = match (lower.find("<body"), lower.rfind("</body>")) {
        (Some(b), Some(e)) => match html[b..].find('>') {
            Some(gt) if b + gt < e => &html[b + gt + 1..e],
            _ => html,
        },
        _ => html,
    };
    let pre = "<html><body>\r\n<!--StartFragment-->";
    let post = "<!--EndFragment-->\r\n</body></html>";
    let hdr_len = "Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000000000\r\nStartFragment:0000000000\r\nEndFragment:0000000000\r\n".len();
    let start_frag = hdr_len + pre.len();
    let end_frag = start_frag + body.len();
    let end = end_frag + post.len();
    let mut s = format!(
        "Version:0.9\r\nStartHTML:{hdr_len:010}\r\nEndHTML:{end:010}\r\nStartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n"
    );
    s.push_str(pre);
    s.push_str(body);
    s.push_str(post);
    let mut v = s.into_bytes();
    v.push(0);
    v
}

pub fn decode(raw: &[u8]) -> Option<String> {
    let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
    let s = String::from_utf8_lossy(raw);
    let num = |k: &str| -> Option<usize> {
        let i = s.find(k)? + k.len();
        s[i..].split(['\r', '\n']).next()?.trim().parse().ok()
    };
    let (a, b) = match (num("StartFragment:"), num("EndFragment:")) {
        (Some(a), Some(b)) => (a, b),
        _ => (num("StartHTML:")?, num("EndHTML:")?),
    };
    let frag = raw.get(a..b.min(raw.len()))?;
    Some(format!("<meta charset=\"utf-8\">{}", String::from_utf8_lossy(frag)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let enc = super::encode("<html><head><meta charset=utf-8></head><body><b>héllo</b> ✓</body></html>");
        let s = String::from_utf8_lossy(&enc);
        let start: usize = s.split("StartFragment:").nth(1).unwrap()[..10].parse().unwrap();
        let end: usize = s.split("EndFragment:").nth(1).unwrap()[..10].parse().unwrap();
        assert_eq!(&enc[start..end], "<b>héllo</b> ✓".as_bytes());
        assert_eq!(super::decode(&enc).unwrap(), "<meta charset=\"utf-8\"><b>héllo</b> ✓");
    }
}
