//! A small, forgiving XML reader for SOAP responses.
//!
//! Namespaces are ignored on purpose: we match on *local* names
//! (`tt:Profile` and `trt:Profile` are both `Profile`), which is how most
//! ONVIF clients cope with cameras that use creative prefixes.

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Element {
    /// Local name, prefix stripped.
    pub name: String,
    /// Attributes as (local name, value).
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// Concatenated text directly inside this element.
    pub text: String,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// First direct child with this local name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Follow a path of direct children.
    pub fn path(&self, names: &[&str]) -> Option<&Element> {
        let mut cur = self;
        for n in names {
            cur = cur.child(n)?;
        }
        Some(cur)
    }

    /// First descendant (depth-first, including self) with this local name.
    pub fn find(&self, name: &str) -> Option<&Element> {
        if self.name == name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// All descendants (including self) with this local name.
    pub fn find_all<'a>(&'a self, name: &str) -> Vec<&'a Element> {
        let mut out = Vec::new();
        self.collect(name, &mut out);
        out
    }

    fn collect<'a>(&'a self, name: &str, out: &mut Vec<&'a Element>) {
        if self.name == name {
            out.push(self);
        }
        for c in &self.children {
            c.collect(name, out);
        }
    }

    /// Trimmed text of the first descendant with this name.
    pub fn find_text(&self, name: &str) -> Option<String> {
        self.find(name).map(|e| e.text.trim().to_string())
    }
}

#[derive(Debug, PartialEq)]
pub struct XmlError(pub String);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "XML error: {}", self.0)
    }
}

fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Decode the five standard entities plus numeric character references.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(end) = rest.find(';') {
            let ent = &rest[1..end];
            let decoded = match ent {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ if ent.starts_with("#x") || ent.starts_with("#X") => {
                    u32::from_str_radix(&ent[2..], 16).ok().and_then(char::from_u32)
                }
                _ if ent.starts_with('#') => ent[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(c) = decoded {
                out.push(c);
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

/// Escape text for inclusion in XML element content or attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Parse a document and return its root element.
pub fn parse(input: &str) -> Result<Element, XmlError> {
    let bytes = input.as_bytes();
    let mut i = 0;
    // Stack of open elements; index 0 is a synthetic document node.
    let mut stack: Vec<Element> = vec![Element::default()];

    while i < bytes.len() {
        if bytes[i] == b'<' {
            if input[i..].starts_with("<!--") {
                let end = input[i..].find("-->").ok_or_else(|| XmlError("unterminated comment".into()))?;
                i += end + 3;
            } else if input[i..].starts_with("<![CDATA[") {
                let end = input[i..].find("]]>").ok_or_else(|| XmlError("unterminated CDATA".into()))?;
                let text = &input[i + 9..i + end];
                stack.last_mut().unwrap().text.push_str(text);
                i += end + 3;
            } else if input[i..].starts_with("<?") {
                let end = input[i..].find("?>").ok_or_else(|| XmlError("unterminated declaration".into()))?;
                i += end + 2;
            } else if input[i..].starts_with("<!") {
                let end = input[i..].find('>').ok_or_else(|| XmlError("unterminated doctype".into()))?;
                i += end + 1;
            } else if input[i..].starts_with("</") {
                let end = input[i..].find('>').ok_or_else(|| XmlError("unterminated end tag".into()))?;
                let name = local(input[i + 2..i + end].trim());
                if stack.len() < 2 {
                    return Err(XmlError(format!("unexpected </{name}>")));
                }
                let el = stack.pop().unwrap();
                if el.name != name {
                    return Err(XmlError(format!("expected </{}>, found </{name}>", el.name)));
                }
                stack.last_mut().unwrap().children.push(el);
                i += end + 1;
            } else {
                // Start tag. Find the closing '>' while respecting quoted attribute values.
                let mut j = i + 1;
                let mut quote: Option<u8> = None;
                while j < bytes.len() {
                    let c = bytes[j];
                    match quote {
                        Some(q) if c == q => quote = None,
                        Some(_) => {}
                        None if c == b'"' || c == b'\'' => quote = Some(c),
                        None if c == b'>' => break,
                        None => {}
                    }
                    j += 1;
                }
                if j >= bytes.len() {
                    return Err(XmlError("unterminated start tag".into()));
                }
                let mut inner = &input[i + 1..j];
                let self_closing = inner.ends_with('/');
                if self_closing {
                    inner = &inner[..inner.len() - 1];
                }
                let el = parse_tag(inner)?;
                if self_closing {
                    stack.last_mut().unwrap().children.push(el);
                } else {
                    stack.push(el);
                }
                i = j + 1;
            }
        } else {
            let end = input[i..].find('<').map(|e| i + e).unwrap_or(bytes.len());
            let text = &input[i..end];
            if stack.len() > 1 {
                stack.last_mut().unwrap().text.push_str(&unescape(text));
            } else if !text.trim().is_empty() {
                return Err(XmlError("text outside root element".into()));
            }
            i = end;
        }
    }

    if stack.len() != 1 {
        return Err(XmlError(format!("unclosed <{}>", stack.last().unwrap().name)));
    }
    let mut doc = stack.pop().unwrap();
    match doc.children.len() {
        0 => Err(XmlError("empty document".into())),
        _ => Ok(doc.children.remove(0)),
    }
}

fn parse_tag(inner: &str) -> Result<Element, XmlError> {
    let inner = inner.trim();
    let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() {
        return Err(XmlError("empty tag name".into()));
    }
    let mut el = Element { name: local(name).to_string(), ..Default::default() };
    let mut rest = inner[name_end..].trim_start();
    while !rest.is_empty() {
        let eq = rest.find('=').ok_or_else(|| XmlError(format!("bad attribute in <{name}>")))?;
        let key = rest[..eq].trim();
        rest = rest[eq + 1..].trim_start();
        let q = rest.chars().next().ok_or_else(|| XmlError("missing attribute value".into()))?;
        if q != '"' && q != '\'' {
            return Err(XmlError(format!("unquoted attribute {key}")));
        }
        let close = rest[1..].find(q).ok_or_else(|| XmlError("unterminated attribute".into()))?;
        let val = &rest[1..1 + close];
        // Keep xmlns declarations out of the way; they are noise for us.
        if key != "xmlns" && !key.starts_with("xmlns:") {
            el.attrs.push((local(key).to_string(), unescape(val)));
        }
        rest = rest[close + 2..].trim_start();
    }
    Ok(el)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_soap_with_prefixes() {
        let doc = r#"<?xml version="1.0" encoding="UTF-8"?>
<SOAP-ENV:Envelope xmlns:SOAP-ENV="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="x">
  <!-- comment -->
  <SOAP-ENV:Body>
    <trt:GetProfilesResponse>
      <trt:Profiles token="profile_1" fixed="true"><tt:Name>mainStream</tt:Name></trt:Profiles>
      <trt:Profiles token='profile_2'><tt:Name>minor &amp; stream</tt:Name><tt:Empty/></trt:Profiles>
    </trt:GetProfilesResponse>
  </SOAP-ENV:Body>
</SOAP-ENV:Envelope>"#;
        let root = parse(doc).unwrap();
        assert_eq!(root.name, "Envelope");
        let profiles = root.find_all("Profiles");
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].attr("token"), Some("profile_1"));
        assert_eq!(profiles[1].attr("token"), Some("profile_2"));
        assert_eq!(profiles[1].find_text("Name").unwrap(), "minor & stream");
        assert!(root.path(&["Body", "GetProfilesResponse"]).is_some());
    }

    #[test]
    fn attribute_with_gt_inside_quotes() {
        let root = parse(r#"<a x="1>2"><b/></a>"#).unwrap();
        assert_eq!(root.attr("x"), Some("1>2"));
        assert_eq!(root.children.len(), 1);
    }

    #[test]
    fn mismatched_tags_error() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("<a>").is_err());
    }

    #[test]
    fn escape_roundtrip() {
        let s = r#"p@ss<w>&"rd'"#;
        assert_eq!(unescape(&escape(s)), s);
        assert_eq!(unescape("&#65;&#x42;&unknown;"), "AB&unknown;");
    }
}
