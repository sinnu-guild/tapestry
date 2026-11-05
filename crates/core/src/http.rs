//! Just enough HTTP/1.1 to talk SOAP to a camera on the local network.
//! (Tapo's ONVIF service is plain HTTP on port 2020.)

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

/// Error text when the server hangs up without sending anything.
pub const NO_REPLY: &str = "closed the connection without replying";

#[derive(Debug, Clone, PartialEq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Result<Url, String> {
        let rest = url.strip_prefix("http://").ok_or_else(|| format!("only http:// URLs are supported, got {url}"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        // Drop any userinfo.
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            // IPv6 literal
            let end = stripped.find(']').ok_or("bad IPv6 host")?;
            let host = &stripped[..end];
            let port = stripped[end + 1..].strip_prefix(':').map(|p| p.parse::<u16>()).transpose();
            (host.to_string(), port.map_err(|_| "bad port")?.unwrap_or(80))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?),
                None => (authority.to_string(), 80),
            }
        };
        if host.is_empty() {
            return Err(format!("missing host in {url}"));
        }
        Ok(Url { host, port, path: path.to_string() })
    }

    pub fn to_string(&self) -> String {
        format!("http://{}:{}{}", self.host, self.port, self.path)
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// POST `body` and return the response. Always `Connection: close`.
pub fn post(url: &str, content_type: &str, body: &str, timeout: Duration) -> Result<Response, String> {
    let u = Url::parse(url)?;
    let addr = (u.host.as_str(), u.port)
        .to_socket_addrs()
        .map_err(|e| format!("can't resolve {}: {e}", u.host))?
        .next()
        .ok_or_else(|| format!("no address for {}", u.host))?;
    let mut stream =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("can't connect to {}:{}: {e}", u.host, u.port))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let host_header = if u.port == 80 { u.host.clone() } else { format!("{}:{}", u.host, u.port) };
    let req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nUser-Agent: Tapestry/0.1\r\n\r\n",
        u.path,
        host_header,
        content_type,
        body.len()
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("send failed: {e}"))?;
    stream.write_all(body.as_bytes()).map_err(|e| format!("send failed: {e}"))?;
    stream.flush().ok();

    read_response(BufReader::new(stream))
}

/// Parse an HTTP/1.x response from a reader (separate for testing).
pub fn read_response<R: BufRead>(mut r: R) -> Result<Response, String> {
    let mut line = String::new();
    let n = r.read_line(&mut line).map_err(|e| format!("read failed: {e}"))?;
    if n == 0 {
        return Err(NO_REPLY.into());
    }
    let mut parts = line.split_whitespace();
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(format!("not an HTTP response: {:?}", line.trim()));
    }
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).ok_or("bad status line")?;

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    loop {
        line.clear();
        let n = r.read_line(&mut line).map_err(|e| format!("read failed: {e}"))?;
        let l = line.trim_end();
        if n == 0 || l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim();
            if k == "content-length" {
                content_length = v.parse().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }

    let mut body = Vec::new();
    if chunked {
        loop {
            line.clear();
            r.read_line(&mut line).map_err(|e| format!("read failed: {e}"))?;
            let size_str = line.trim().split(';').next().unwrap_or("");
            let size = usize::from_str_radix(size_str, 16).map_err(|_| format!("bad chunk size {size_str:?}"))?;
            if size == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + size, 0);
            r.read_exact(&mut body[start..]).map_err(|e| format!("read failed: {e}"))?;
            line.clear();
            r.read_line(&mut line).ok(); // CRLF after chunk
        }
    } else if let Some(len) = content_length {
        body.resize(len, 0);
        r.read_exact(&mut body).map_err(|e| format!("read failed: {e}"))?;
    } else {
        r.read_to_end(&mut body).map_err(|e| format!("read failed: {e}"))?;
    }

    Ok(Response { status, body: String::from_utf8_lossy(&body).into_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parse_urls() {
        assert_eq!(
            Url::parse("http://192.168.1.5:2020/onvif/service").unwrap(),
            Url { host: "192.168.1.5".into(), port: 2020, path: "/onvif/service".into() }
        );
        assert_eq!(Url::parse("http://cam").unwrap().port, 80);
        assert_eq!(Url::parse("http://[fe80::1]:2020/x").unwrap().host, "fe80::1");
        assert!(Url::parse("https://cam/").is_err());
    }

    #[test]
    fn content_length_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let r = read_response(Cursor::new(raw)).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "hello");
    }

    #[test]
    fn chunked_body() {
        let raw = "HTTP/1.1 500 Internal\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;x=y\r\npedia\r\n0\r\n\r\n";
        let r = read_response(Cursor::new(raw)).unwrap();
        assert_eq!(r.status, 500);
        assert_eq!(r.body, "Wikipedia");
    }

    #[test]
    fn hang_up_is_no_reply() {
        assert_eq!(read_response(Cursor::new("")).unwrap_err(), NO_REPLY);
    }

    #[test]
    fn read_to_close() {
        let raw = "HTTP/1.0 200 OK\r\n\r\n<a/>";
        assert_eq!(read_response(Cursor::new(raw)).unwrap().body, "<a/>");
    }
}
