//! ONVIF client: the official, documented way to talk to Tapo cameras.
//!
//! Covers what Tapestry needs and nothing more:
//! * device info + clock sync (so WS-Security digests are accepted),
//! * media profiles and RTSP stream URIs,
//! * PTZ: continuous move / stop / relative move / presets,
//! * events: pull-point subscription for motion.
//!
//! Tapo serves ONVIF on port 2020 and authenticates with the *Camera Account*
//! created in the Tapo app (not the TP-Link cloud login).

use crate::crypto::{base64_encode, nonce16, sha1};
use crate::http;
use crate::time::{iso8601_utc, unix_from_utc, unix_now};
use crate::xml::{self, Element};
use std::fmt;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 2020;

#[derive(Debug, Clone, PartialEq)]
pub enum OnvifError {
    /// Couldn't reach the camera at all.
    Network(String),
    /// Camera rejected the username/password (or the clock is way off).
    Auth(String),
    /// Camera understood us but said no (SOAP fault).
    Fault(String),
    /// Camera replied with something we couldn't make sense of.
    Parse(String),
    /// Camera accepted the request, then hung up without answering. Tapo does
    /// this to event long-polls that have nothing to report.
    NoReply,
}

impl fmt::Display for OnvifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnvifError::Network(m) => write!(f, "can't reach camera: {m}"),
            OnvifError::Auth(m) => write!(f, "camera rejected the login: {m}"),
            OnvifError::Fault(m) => write!(f, "camera refused: {m}"),
            OnvifError::Parse(m) => write!(f, "unexpected reply: {m}"),
            OnvifError::NoReply => write!(f, "camera {}", http::NO_REPLY),
        }
    }
}

impl std::error::Error for OnvifError {}

pub type Result<T> = std::result::Result<T, OnvifError>;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub token: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub encoding: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Preset {
    pub token: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub address: String,
}

/// One event from the camera.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// e.g. `tns1:RuleEngine/CellMotionDetector/Motion`
    pub topic: String,
    /// `Initialized`, `Changed`, `Deleted`, or empty.
    pub operation: String,
    /// Data items, e.g. `("IsMotion", "true")`.
    pub data: Vec<(String, String)>,
}

impl Notification {
    /// If this is a detector event, returns (kind, active) — e.g. ("Motion", true).
    pub fn detection(&self) -> Option<(String, bool)> {
        let (_, value) = self
            .data
            .iter()
            .find(|(k, v)| (k.starts_with("Is") || k == "State" || k == "MotionActive") && parse_bool(v).is_some())?;
        let active = parse_bool(value)?;
        let last = self.topic.rsplit('/').next().unwrap_or(&self.topic);
        let last = last.rsplit(':').next().unwrap_or(last);
        let kind = if last.to_ascii_lowercase().contains("motion") { "Motion".to_string() } else { last.to_string() };
        Some((kind, active))
    }

    pub fn is_motion(&self) -> Option<bool> {
        match self.detection() {
            Some((k, a)) if k == "Motion" => Some(a),
            _ => None,
        }
    }
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

/// Base64(SHA1(nonce + created + password)) — the WS-Security PasswordDigest.
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut buf = Vec::with_capacity(nonce.len() + created.len() + password.len());
    buf.extend_from_slice(nonce);
    buf.extend_from_slice(created.as_bytes());
    buf.extend_from_slice(password.as_bytes());
    base64_encode(&sha1(&buf))
}

const NS: &str = concat!(
    r#"xmlns:s="http://www.w3.org/2003/05/soap-envelope" "#,
    r#"xmlns:tds="http://www.onvif.org/ver10/device/wsdl" "#,
    r#"xmlns:trt="http://www.onvif.org/ver10/media/wsdl" "#,
    r#"xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" "#,
    r#"xmlns:tev="http://www.onvif.org/ver10/events/wsdl" "#,
    r#"xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" "#,
    r#"xmlns:wsa="http://www.w3.org/2005/08/addressing" "#,
    r#"xmlns:tt="http://www.onvif.org/ver10/schema""#
);

#[derive(Debug, Clone)]
pub struct OnvifClient {
    pub host: String,
    pub port: u16,
    username: String,
    password: String,
    /// camera clock minus our clock, seconds.
    pub time_offset: i64,
    pub timeout: Duration,
    pub device_url: String,
    pub media_url: Option<String>,
    pub ptz_url: Option<String>,
    pub events_url: Option<String>,
    /// Main (highest quality) profile, used for PTZ.
    pub profile_token: Option<String>,
    /// That profile's picture size, if the camera reported one.
    pub main_size: Option<(u32, u32)>,
}

impl OnvifClient {
    pub fn new(host: &str, port: u16, username: &str, password: &str) -> Self {
        OnvifClient {
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            time_offset: 0,
            timeout: Duration::from_secs(6),
            device_url: format!("http://{}/onvif/device_service", host_port(host, port)),
            media_url: None,
            ptz_url: None,
            events_url: None,
            profile_token: None,
            main_size: None,
        }
    }

    /// Full handshake: clock sync, service discovery, profiles, device info.
    pub fn connect(&mut self) -> Result<DeviceInfo> {
        // Clock sync is best-effort; Tapo syncs via NTP so offset is usually ~0.
        let _ = self.sync_time();
        let info = self.device_information()?;
        self.discover_services()?;
        let profiles = self.profiles()?;
        self.profile_token = profiles.first().map(|p| p.token.clone());
        self.main_size = profiles.first().filter(|p| p.width > 0 && p.height > 0).map(|p| (p.width, p.height));
        Ok(info)
    }

    // ----- low-level SOAP -------------------------------------------------

    fn envelope(&self, body: &str, auth: bool, addressing: Option<(&str, &str)>) -> String {
        let mut header = String::new();
        if let Some((action, to)) = addressing {
            header.push_str(&format!("<wsa:Action>{}</wsa:Action><wsa:To>{}</wsa:To>", xml::escape(action), xml::escape(to)));
        }
        if auth {
            let nonce = nonce16();
            let created = iso8601_utc(unix_now() + self.time_offset);
            let digest = password_digest(&nonce, &created, &self.password);
            header.push_str(&format!(
                concat!(
                    r#"<wsse:Security s:mustUnderstand="1" "#,
                    r#"xmlns:wsse="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd" "#,
                    r#"xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">"#,
                    "<wsse:UsernameToken><wsse:Username>{}</wsse:Username>",
                    r#"<wsse:Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{}</wsse:Password>"#,
                    r#"<wsse:Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</wsse:Nonce>"#,
                    "<wsu:Created>{}</wsu:Created></wsse:UsernameToken></wsse:Security>"
                ),
                xml::escape(&self.username),
                digest,
                base64_encode(&nonce),
                created
            ));
        }
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope {NS}><s:Header>{header}</s:Header><s:Body>{body}</s:Body></s:Envelope>"#
        )
    }

    fn call_with(
        &self,
        url: &str,
        body: &str,
        auth: bool,
        addressing: Option<(&str, &str)>,
        timeout: Duration,
    ) -> Result<Element> {
        let env = self.envelope(body, auth, addressing);
        let resp = http::post(url, "application/soap+xml; charset=utf-8", &env, timeout).map_err(|e| {
            if e == http::NO_REPLY {
                OnvifError::NoReply
            } else {
                OnvifError::Network(e)
            }
        })?;
        let root = xml::parse(&resp.body);
        match root {
            Ok(root) => {
                if let Some(fault) = root.find("Fault") {
                    return Err(classify_fault(fault, resp.status));
                }
                if resp.status >= 400 {
                    return Err(status_error(resp.status));
                }
                root.find("Body").cloned().ok_or_else(|| OnvifError::Parse("no SOAP Body".into()))
            }
            Err(e) if resp.status >= 400 => {
                let _ = e;
                Err(status_error(resp.status))
            }
            Err(e) => Err(OnvifError::Parse(e.to_string())),
        }
    }

    fn call(&self, url: &str, body: &str) -> Result<Element> {
        self.call_with(url, body, true, None, self.timeout)
    }

    fn ptz(&self) -> Result<(&str, &str)> {
        let url = self.ptz_url.as_deref().ok_or_else(|| OnvifError::Fault("camera has no PTZ service".into()))?;
        let tok = self.profile_token.as_deref().ok_or_else(|| OnvifError::Fault("no media profile yet".into()))?;
        Ok((url, tok))
    }

    // ----- device ---------------------------------------------------------

    /// Ask the camera for its UTC clock (no auth needed) and remember the offset.
    pub fn sync_time(&mut self) -> Result<i64> {
        let body = self.call_with(&self.device_url.clone(), "<tds:GetSystemDateAndTime/>", false, None, self.timeout)?;
        let utc = body.find("UTCDateTime").ok_or_else(|| OnvifError::Parse("no UTCDateTime".into()))?;
        let num = |n: &str| -> Result<i64> {
            utc.find_text(n).and_then(|t| t.parse::<i64>().ok()).ok_or_else(|| OnvifError::Parse(format!("missing {n}")))
        };
        let cam = unix_from_utc(
            num("Year")?,
            num("Month")? as u32,
            num("Day")? as u32,
            num("Hour")? as u32,
            num("Minute")? as u32,
            num("Second")? as u32,
        );
        self.time_offset = cam - unix_now();
        Ok(self.time_offset)
    }

    pub fn device_information(&self) -> Result<DeviceInfo> {
        let body = self.call(&self.device_url, "<tds:GetDeviceInformation/>")?;
        let t = |n: &str| body.find_text(n).unwrap_or_default();
        Ok(DeviceInfo {
            manufacturer: t("Manufacturer"),
            model: t("Model"),
            firmware: t("FirmwareVersion"),
            serial: t("SerialNumber"),
        })
    }

    /// Find the media / PTZ / events service URLs.
    pub fn discover_services(&mut self) -> Result<()> {
        let body = self.call(&self.device_url, "<tds:GetCapabilities><tds:Category>All</tds:Category></tds:GetCapabilities>")?;
        let xaddr = |svc: &str| {
            body.find("Capabilities")
                .and_then(|c| c.child(svc))
                .and_then(|s| s.find_text("XAddr"))
                .filter(|s| !s.is_empty())
                .map(|u| self.fix_host(&u))
        };
        let (media, ptz, events) = (xaddr("Media"), xaddr("PTZ"), xaddr("Events"));
        self.media_url = media;
        self.ptz_url = ptz;
        self.events_url = events;
        // Tapo serves everything from one endpoint; fall back to it if the camera is coy.
        let fallback = format!("http://{}/onvif/service", host_port(&self.host, self.port));
        if self.media_url.is_none() {
            self.media_url = Some(fallback.clone());
        }
        if self.events_url.is_none() {
            self.events_url = Some(fallback);
        }
        Ok(())
    }

    /// Cameras sometimes advertise URLs with an internal or stale IP. If the
    /// path looks right, point it back at the host we actually reached.
    fn fix_host(&self, url: &str) -> String {
        match http::Url::parse(url) {
            Ok(mut u) => {
                if u.host != self.host {
                    u.host = self.host.clone();
                }
                u.to_string()
            }
            Err(_) => url.to_string(),
        }
    }

    // ----- media ----------------------------------------------------------

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let url = self.media_url.clone().unwrap_or_else(|| self.device_url.clone());
        let body = self.call(&url, "<trt:GetProfiles/>")?;
        let mut out = Vec::new();
        for p in body.find_all("Profiles") {
            let Some(token) = p.attr("token") else { continue };
            let enc = p.find("VideoEncoderConfiguration");
            let res = enc.and_then(|e| e.find("Resolution"));
            out.push(Profile {
                token: token.to_string(),
                name: p.child("Name").map(|n| n.text.trim().to_string()).unwrap_or_default(),
                width: res.and_then(|r| r.find_text("Width")).and_then(|w| w.parse().ok()).unwrap_or(0),
                height: res.and_then(|r| r.find_text("Height")).and_then(|h| h.parse().ok()).unwrap_or(0),
                encoding: enc.and_then(|e| e.find_text("Encoding")).unwrap_or_default(),
            });
        }
        if out.is_empty() {
            return Err(OnvifError::Parse("camera returned no media profiles".into()));
        }
        Ok(out)
    }

    pub fn stream_uri(&self, profile_token: &str) -> Result<String> {
        let url = self.media_url.clone().unwrap_or_else(|| self.device_url.clone());
        let body = self.call(
            &url,
            &format!(
                "<trt:GetStreamUri><trt:StreamSetup><tt:Stream>RTP-Unicast</tt:Stream><tt:Transport><tt:Protocol>RTSP</tt:Protocol></tt:Transport></trt:StreamSetup><trt:ProfileToken>{}</trt:ProfileToken></trt:GetStreamUri>",
                xml::escape(profile_token)
            ),
        )?;
        body.find_text("Uri").filter(|u| !u.is_empty()).ok_or_else(|| OnvifError::Parse("no stream Uri".into()))
    }

    // ----- PTZ ------------------------------------------------------------

    pub fn has_ptz(&self) -> bool {
        self.ptz_url.is_some() && self.profile_token.is_some()
    }

    /// Start moving. `pan`/`tilt` are velocities in -1.0..=1.0 (right/up positive).
    pub fn continuous_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (url, tok) = self.ptz()?;
        self.call(
            url,
            &format!(
                r#"<tptz:ContinuousMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Velocity><tt:PanTilt x="{:.3}" y="{:.3}"/></tptz:Velocity></tptz:ContinuousMove>"#,
                xml::escape(tok),
                pan.clamp(-1.0, 1.0),
                tilt.clamp(-1.0, 1.0)
            ),
        )
        .map(|_| ())
    }

    pub fn stop(&self) -> Result<()> {
        let (url, tok) = self.ptz()?;
        self.call(
            url,
            &format!(
                "<tptz:Stop><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PanTilt>true</tptz:PanTilt><tptz:Zoom>false</tptz:Zoom></tptz:Stop>",
                xml::escape(tok)
            ),
        )
        .map(|_| ())
    }

    /// Nudge by a relative amount in the camera's normalised space (-1.0..=1.0).
    pub fn relative_move(&self, pan: f32, tilt: f32) -> Result<()> {
        let (url, tok) = self.ptz()?;
        self.call(
            url,
            &format!(
                r#"<tptz:RelativeMove><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:Translation><tt:PanTilt x="{:.4}" y="{:.4}"/></tptz:Translation></tptz:RelativeMove>"#,
                xml::escape(tok),
                pan.clamp(-1.0, 1.0),
                tilt.clamp(-1.0, 1.0)
            ),
        )
        .map(|_| ())
    }

    pub fn presets(&self) -> Result<Vec<Preset>> {
        let (url, tok) = self.ptz()?;
        let body = self.call(
            url,
            &format!("<tptz:GetPresets><tptz:ProfileToken>{}</tptz:ProfileToken></tptz:GetPresets>", xml::escape(tok)),
        )?;
        Ok(body
            .find_all("Preset")
            .into_iter()
            .filter_map(|p| {
                let token = p.attr("token")?.to_string();
                let name = p.find_text("Name").filter(|n| !n.is_empty()).unwrap_or_else(|| format!("Preset {token}"));
                Some(Preset { token, name })
            })
            .collect())
    }

    pub fn goto_preset(&self, preset_token: &str) -> Result<()> {
        let (url, tok) = self.ptz()?;
        self.call(
            url,
            &format!(
                "<tptz:GotoPreset><tptz:ProfileToken>{}</tptz:ProfileToken><tptz:PresetToken>{}</tptz:PresetToken></tptz:GotoPreset>",
                xml::escape(tok),
                xml::escape(preset_token)
            ),
        )
        .map(|_| ())
    }

    // ----- events ---------------------------------------------------------

    pub fn subscribe(&self, lifetime_secs: u32) -> Result<Subscription> {
        let url = self.events_url.clone().ok_or_else(|| OnvifError::Fault("camera has no events service".into()))?;
        let body = self.call(
            &url,
            &format!(
                "<tev:CreatePullPointSubscription><tev:InitialTerminationTime>PT{lifetime_secs}S</tev:InitialTerminationTime></tev:CreatePullPointSubscription>"
            ),
        )?;
        let address = body
            .find("SubscriptionReference")
            .and_then(|r| r.find_text("Address"))
            .filter(|a| !a.is_empty())
            .ok_or_else(|| OnvifError::Parse("no subscription address".into()))?;
        Ok(Subscription { address: self.fix_host(&address) })
    }

    /// Long-poll for events. Blocks up to `wait_secs` on the camera side.
    pub fn pull(&self, sub: &Subscription, wait_secs: u32) -> Result<Vec<Notification>> {
        let body = self.call_with(
            &sub.address,
            &format!("<tev:PullMessages><tev:Timeout>PT{wait_secs}S</tev:Timeout><tev:MessageLimit>32</tev:MessageLimit></tev:PullMessages>"),
            true,
            Some(("http://www.onvif.org/ver10/events/wsdl/PullPointSubscription/PullMessagesRequest", &sub.address)),
            self.timeout + Duration::from_secs(wait_secs as u64),
        )?;
        Ok(parse_notifications(&body))
    }

    pub fn renew(&self, sub: &Subscription, lifetime_secs: u32) -> Result<()> {
        self.call_with(
            &sub.address,
            &format!("<wsnt:Renew><wsnt:TerminationTime>PT{lifetime_secs}S</wsnt:TerminationTime></wsnt:Renew>"),
            true,
            Some(("http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/RenewRequest", &sub.address)),
            self.timeout,
        )
        .map(|_| ())
    }

    pub fn unsubscribe(&self, sub: &Subscription) -> Result<()> {
        self.call_with(
            &sub.address,
            "<wsnt:Unsubscribe/>",
            true,
            Some(("http://docs.oasis-open.org/wsn/bw-2/SubscriptionManager/UnsubscribeRequest", &sub.address)),
            self.timeout,
        )
        .map(|_| ())
    }
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn status_error(status: u16) -> OnvifError {
    match status {
        401 | 403 => OnvifError::Auth(format!("HTTP {status}")),
        _ => OnvifError::Fault(format!("HTTP {status}")),
    }
}

fn classify_fault(fault: &Element, status: u16) -> OnvifError {
    let reason =
        fault.find("Reason").and_then(|r| r.find_text("Text")).or_else(|| fault.find_text("faultstring")).unwrap_or_default();
    let codes: Vec<String> = fault.find_all("Value").iter().map(|v| v.text.trim().to_string()).collect();
    let all = format!("{} {}", codes.join(" "), reason);
    let msg = if reason.is_empty() { codes.join(" / ") } else { reason.clone() };
    let lower = all.to_ascii_lowercase();
    if status == 401
        || lower.contains("notauthorized")
        || lower.contains("not authorized")
        || lower.contains("failedauthentication")
    {
        OnvifError::Auth(if msg.is_empty() { "not authorized".into() } else { msg })
    } else {
        OnvifError::Fault(if msg.is_empty() { format!("SOAP fault (HTTP {status})") } else { msg })
    }
}

/// Extract notifications from a PullMessagesResponse body.
pub fn parse_notifications(body: &Element) -> Vec<Notification> {
    body.find_all("NotificationMessage")
        .into_iter()
        .map(|nm| {
            let topic = nm.find_text("Topic").unwrap_or_default();
            let msg = nm.find("Message").and_then(|m| m.child("Message").or(Some(m)));
            let operation = msg.and_then(|m| m.attr("PropertyOperation")).unwrap_or("").to_string();
            let data = msg
                .and_then(|m| m.find("Data"))
                .map(|d| {
                    d.find_all("SimpleItem")
                        .iter()
                        .filter_map(|si| Some((si.attr("Name")?.to_string(), si.attr("Value")?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Notification { topic, operation, data }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_spec_example() {
        // Classic WS-Security example from the OASIS UsernameToken profile docs.
        let nonce = crate::crypto::base64_decode("LKqI6G/AikKCQrN0zqZFlg==").unwrap();
        assert_eq!(password_digest(&nonce, "2010-09-16T07:50:45Z", "userpassword"), "tuOSpGlFlIXsozq4HFNeeGeFLEI=");
    }

    #[test]
    fn parses_tapo_style_motion() {
        let doc = r#"<env:Envelope xmlns:env="e"><env:Body><tev:PullMessagesResponse>
<tev:CurrentTime>2026-11-20T11:00:00Z</tev:CurrentTime>
<wsnt:NotificationMessage>
 <wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic>
 <wsnt:Message><tt:Message UtcTime="2026-11-20T11:00:00Z" PropertyOperation="Changed">
   <tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="vsconf"/></tt:Source>
   <tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data>
 </tt:Message></wsnt:Message>
</wsnt:NotificationMessage>
<wsnt:NotificationMessage>
 <wsnt:Topic>tns1:VideoSource/MotionAlarm</wsnt:Topic>
 <wsnt:Message><tt:Message PropertyOperation="Initialized"><tt:Data><tt:SimpleItem Name="State" Value="false"/></tt:Data></tt:Message></wsnt:Message>
</wsnt:NotificationMessage>
<wsnt:NotificationMessage>
 <wsnt:Topic>tns1:RuleEngine/TamperDetector/Tamper</wsnt:Topic>
 <wsnt:Message><tt:Message><tt:Data><tt:SimpleItem Name="IsTamper" Value="true"/></tt:Data></tt:Message></wsnt:Message>
</wsnt:NotificationMessage>
</tev:PullMessagesResponse></env:Body></env:Envelope>"#;
        let root = xml::parse(doc).unwrap();
        let n = parse_notifications(&root);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0].operation, "Changed");
        assert_eq!(n[0].is_motion(), Some(true));
        assert_eq!(n[1].is_motion(), Some(false));
        assert_eq!(n[2].detection(), Some(("Tamper".to_string(), true)));
        assert_eq!(n[2].is_motion(), None);
    }

    #[test]
    fn fault_classification() {
        let doc = r#"<s:Envelope xmlns:s="x"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>ter:NotAuthorized</s:Value></s:Subcode></s:Code><s:Reason><s:Text xml:lang="en">Sender not Authorized</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#;
        let root = xml::parse(doc).unwrap();
        let e = classify_fault(root.find("Fault").unwrap(), 400);
        assert!(matches!(e, OnvifError::Auth(_)), "{e:?}");
    }

    #[test]
    fn fix_host_rewrites_foreign_ip() {
        let c = OnvifClient::new("192.168.1.50", 2020, "u", "p");
        assert_eq!(c.fix_host("http://10.0.0.1:2020/event-1_2020"), "http://192.168.1.50:2020/event-1_2020");
    }
}
