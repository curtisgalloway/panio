// Copyright 2026 Curtis Galloway
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Intel AMT WS-Management client: SOAP over verified HTTPS and HTTP Digest.
//!
//! Plain HTTP is available only by explicit opt-in. Every power and KVM
//! management operation uses the same configured transport; no TLS failure
//! can cause a downgrade to HTTP.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io;
use std::net::Ipv6Addr;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use md5::{Digest as _, Md5};

/// Per-request budget for a standalone call. The ME answers fast (it is
/// firmware, not the OS); this mostly bounds how long a wrong/dead address
/// stalls socket I/O. The platform DNS resolver can exceed this budget.
/// Retrying callers shorten it to what is left of their own
/// deadline (see `attempt_timeout` in main.rs).
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Floor for any single HTTP exchange, so a call issued right at a deadline
/// still gets a real chance to answer instead of failing on a zero timeout.
pub const MIN_CALL_TIMEOUT: Duration = Duration::from_secs(1);

const HTTP_PORT: u16 = 16992;
const HTTPS_PORT: u16 = 16993;

const XMLNS_ENV: &str = "http://www.w3.org/2003/05/soap-envelope";
const XMLNS_ADDR: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing";
const XMLNS_WSMAN: &str = "http://schemas.dmtf.org/wbem/wsman/1/wsman.xsd";
const ANON: &str = "http://schemas.xmlsoap.org/ws/2004/08/addressing/role/anonymous";
const TRANSFER_GET: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get";
const TRANSFER_PUT: &str = "http://schemas.xmlsoap.org/ws/2004/09/transfer/Put";
const CIM: &str = "http://schemas.dmtf.org/wbem/wscim/1/cim-schema/2/";
const IPS: &str = "http://intel.com/wbem/wscim/1/ips-schema/1/";

/// `CIM_KVMRedirectionSAP.EnabledState`: the KVM service access point is
/// serving. AMT also reports 6 ("Enabled but Offline") once redirection is on
/// with no session attached, which for our purposes counts as enabled.
pub const KVM_ENABLED: u16 = 2;
/// `CIM_KVMRedirectionSAP.EnabledState`: KVM redirection is off.
pub const KVM_DISABLED: u16 = 3;
/// `EnabledState` 6 — enabled, no session attached.
pub const KVM_ENABLED_OFFLINE: u16 = 6;

/// The order `IPS_KVMRedirectionSettingData` elements must appear in on a Put.
///
/// AMT returns them **alphabetically** on a Get but validates a Put against an
/// `xsd:sequence` in **inheritance order**, so a Get response cannot be sent
/// back unchanged — it is rejected as `InvalidRepresentation` with an empty
/// fault Detail. This order is the one the firmware accepts (verified against
/// AMT 12.0.24). `OptInPolicyTimeout` belongs here even though the Intel
/// reference's *request* struct omits it; this firmware's schema matches their
/// *response* struct.
const KVM_PUT_ORDER: &[&str] = &[
    "ElementName",
    "InstanceID",
    "EnabledByMEBx",
    "BackToBackFbMode",
    "Is5900PortEnabled",
    "OptInPolicy",
    "OptInPolicyTimeout",
    "SessionTimeout",
    "RFBPassword",
    "DefaultScreen",
];

/// `CIM_PowerManagementService.RequestPowerStateChange` PowerState values
/// (DMTF CIM PowerState value map). AMT implements 2/5/8/10; this helper
/// commands 2 and 8 and builds `cycle` as off → delay → on so the off-hold
/// duration is controllable (the fixed CIM power-cycle state 5 is not).
pub const PS_ON: u16 = 2;
pub const PS_OFF_SOFT: u16 = 8;

/// Human name for a `PowerState` value (DMTF CIM value map), for `status`
/// output and error messages.
pub fn power_state_name(ps: u16) -> &'static str {
    match ps {
        1 => "Other",
        2 => "On",
        3 => "Sleep - Light",
        4 => "Sleep - Deep",
        5 => "Power Cycle (Off - Soft)",
        6 => "Off - Hard",
        7 => "Hibernate (Off - Soft)",
        8 => "Off - Soft",
        9 => "Power Cycle (Off - Hard)",
        10 => "Master Bus Reset",
        11 => "Diagnostic Interrupt (NMI)",
        12 => "Off - Soft Graceful",
        13 => "Off - Hard Graceful",
        14 => "Master Bus Reset Graceful",
        15 => "Power Cycle (Off - Soft Graceful)",
        16 => "Power Cycle (Off - Hard Graceful)",
        _ => "unknown",
    }
}

/// Marker context on errors that may clear on their own. Observed on this
/// bench: the machine's NIC drops link for a few seconds around host power
/// transitions (PHY renegotiation as the host side comes up or down), which
/// surfaces as "no route to host" mid-command. Callers use [`is_transient`]
/// to keep polling through that window instead of failing the hook.
#[derive(Debug)]
pub struct Transient;

/// The subset of `IPS_KVMRedirectionSettingData` this helper reports.
///
/// `RFBPassword` is deliberately absent: a Get always returns it empty, so
/// there is nothing truthful to put here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvmSettings {
    /// Whether the ME serves RFB to standard VNC clients on port 5900.
    pub port_5900_enabled: bool,
    /// Whether a person at the machine must consent before a session opens.
    /// Must be false for a headless bench target to be reachable.
    pub opt_in_policy: bool,
    /// Idle minutes before the ME drops a KVM session; 0 disables the timeout.
    pub session_timeout: u16,
    /// Whether KVM is enabled at the firmware level in MEBx. When false,
    /// nothing this helper writes will bring the port up.
    pub enabled_by_mebx: bool,
}

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("transient AMT transport error")
    }
}

/// Whether an error carries the [`Transient`] marker (a transport-level
/// failure worth retrying, as opposed to an auth failure or a CIM error).
pub fn is_transient(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Transient>().is_some()
}

/// Whether a ureq transport failure is of a kind that can clear on its own:
/// the connection failing (no route / refused / reset while the NIC
/// renegotiates), an I/O error or timeout mid-exchange, or a DNS lookup
/// failing while the resolver is unreachable. Everything else — a malformed
/// URL or scheme, an unparseable status line or header, a proxy problem —
/// is deterministic, and retrying it only burns the confirm budget.
pub fn transient_kind(kind: ureq::ErrorKind) -> bool {
    matches!(
        kind,
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io | ureq::ErrorKind::Dns
    )
}

/// Validated endpoint and trust configuration, built before obtaining credentials.
pub struct Connection {
    url: String,
    agent: ureq::Agent,
    tls: Arc<crate::tls::DeadlineTls>,
    pub insecure_http: bool,
}

impl Connection {
    pub fn new(device: &str, allow_insecure_http: bool, ca_file: Option<&Path>) -> Result<Self> {
        let (url, insecure_http) = parse_device(device, allow_insecure_http)?;
        if insecure_http && ca_file.is_some() {
            bail!("--tls-ca-file / AMT_TLS_CA_FILE cannot be used with unencrypted HTTP");
        }
        let mut roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        if let Some(path) = ca_file {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading TLS CA file {}", path.display()))?;
            let certs = rustls_pemfile::certs(&mut bytes.as_slice())
                .collect::<io::Result<Vec<_>>>()
                .with_context(|| format!("parsing TLS CA file {}", path.display()))?;
            if certs.is_empty() {
                bail!("TLS CA file {} contains no certificates", path.display());
            }
            for cert in certs {
                roots.add(cert).with_context(|| {
                    format!("invalid certificate in TLS CA file {}", path.display())
                })?;
            }
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = Arc::new(crate::tls::DeadlineTls::new(tls));
        let agent = ureq::AgentBuilder::new()
            .tls_connector(tls.clone())
            .https_only(!insecure_http)
            .redirects(0)
            .try_proxy_from_env(false)
            .build();
        Ok(Self {
            url,
            agent,
            tls,
            insecure_http,
        })
    }
}

/// An AMT machine addressed over the explicitly configured WS-Man transport.
pub struct Client {
    url: String,
    agent: ureq::Agent,
    tls: Arc<crate::tls::DeadlineTls>,
    /// The request path, as it appears in the Digest `uri` parameter.
    path: &'static str,
    user: String,
    password: String,
    /// The `Server:` header seen on the digest challenge (e.g. "Intel(R)
    /// Active Management Technology 12.0.24.1314"), captured for `status`.
    server: RefCell<Option<String>>,
}

/// Resolve a narrow host[:port] address; ports never select the transport.
/// Credentials, paths (except a trailing slash), queries and fragments are rejected.
fn parse_device(device: &str, allow_insecure_http: bool) -> Result<(String, bool)> {
    let d = device.trim().trim_end_matches('/');
    let (d, insecure_http) = if let Some(host) = d.strip_prefix("https://") {
        if allow_insecure_http {
            bail!("HTTPS device address conflicts with --allow-insecure-http");
        }
        (host, false)
    } else if let Some(host) = d.strip_prefix("http://") {
        if !allow_insecure_http {
            bail!("HTTP device address requires --allow-insecure-http; prefer verified HTTPS");
        }
        (host, true)
    } else {
        (d, allow_insecure_http)
    };
    if d.is_empty() {
        bail!("empty device address");
    }
    if let Some(c) = d
        .chars()
        .find(|c| c.is_whitespace() || "@/?#<>&\"".contains(*c))
    {
        bail!(
            "invalid device address {device:?}: unexpected {c:?} — expected \
             host[:port], an IPv4 address, or a bracketed IPv6 literal like [fe80::1]"
        );
    }
    let (host, port_part) = if let Some(rest) = d.strip_prefix('[') {
        let (addr, after) = rest.split_once(']').ok_or_else(|| {
            anyhow!("invalid device address {device:?}: unterminated '[' in IPv6 literal")
        })?;
        let v6: Ipv6Addr = addr.parse().map_err(|_| {
            anyhow!("invalid device address {device:?}: {addr:?} is not an IPv6 address")
        })?;
        (format!("[{v6}]"), after)
    } else if d.matches(':').count() > 1 {
        bail!(
            "invalid device address {device:?}: an IPv6 literal must be bracketed, \
             e.g. [fe80::1] or [fe80::1]:16992"
        );
    } else {
        match d.split_once(':') {
            Some((host, _)) => (host.to_string(), &d[host.len()..]),
            None => (d.to_string(), ""),
        }
    };
    if host.is_empty() {
        bail!("invalid device address {device:?}: empty host");
    }
    if !host.starts_with('[')
        && !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        bail!("invalid device address {device:?}: {host:?} is not a hostname or IPv4 address");
    }
    let port = match port_part {
        "" => {
            if insecure_http {
                HTTP_PORT
            } else {
                HTTPS_PORT
            }
        }
        p => match p.strip_prefix(':') {
            Some(n) => match n.parse::<u16>() {
                Ok(0) | Err(_) => bail!(
                    "invalid device address {device:?}: port {n:?} is not a number in 1..=65535"
                ),
                Ok(n) => n,
            },
            None => bail!(
                "invalid device address {device:?}: unexpected {p:?} after the host \
                 (only :port may follow)"
            ),
        },
    };
    let scheme = if insecure_http { "http" } else { "https" };
    Ok((format!("{scheme}://{host}:{port}/wsman"), insecure_http))
}

impl Client {
    /// Attach credentials only after endpoint and trust validation have succeeded.
    pub fn new(connection: Connection, user: &str, password: &str) -> Self {
        Client {
            url: connection.url,
            agent: connection.agent,
            tls: connection.tls,
            path: "/wsman",
            user: user.to_string(),
            password: password.to_string(),
            server: RefCell::new(None),
        }
    }

    /// The AMT firmware identity from the HTTP `Server:` header, if a
    /// request has been made.
    pub fn server_ident(&self) -> Option<String> {
        self.server.borrow().clone()
    }

    /// Current system power state: `transfer/Get` on
    /// `CIM_AssociatedPowerManagementService`, whose `PowerState` property is
    /// the ME's live view of the host (readable in any host state — the ME
    /// runs even when the host is off).
    pub fn power_state(&self) -> Result<u16> {
        self.power_state_within(CALL_TIMEOUT)
    }

    /// [`Client::power_state`] with an explicit HTTP budget for this call.
    pub fn power_state_within(&self, budget: Duration) -> Result<u16> {
        let uri = format!("{CIM}CIM_AssociatedPowerManagementService");
        let body = self.post(&envelope(&self.url, TRANSFER_GET, &uri, "", ""), budget)?;
        let text = xml_text(&body, "PowerState")
            .ok_or_else(|| anyhow!("no PowerState in WS-Man response"))?;
        text.parse()
            .with_context(|| format!("unparseable PowerState {text:?}"))
    }

    /// Invoke `CIM_PowerManagementService.RequestPowerStateChange` with the
    /// given `PowerState`, addressed at the managed host system, with an
    /// explicit HTTP budget for this call. Errors on a non-zero CIM
    /// ReturnValue.
    pub fn request_power_state_within(&self, state: u16, budget: Duration) -> Result<()> {
        let uri = format!("{CIM}CIM_PowerManagementService");
        let action = format!("{uri}/RequestPowerStateChange");
        // The AMT instance keys for the power service and the managed system
        // (fixed values, per the Intel AMT SDK).
        let selectors = "\n  <w:SelectorSet>\
             \n   <w:Selector Name=\"CreationClassName\">CIM_PowerManagementService</w:Selector>\
             \n   <w:Selector Name=\"Name\">Intel(r) AMT Power Management Service</w:Selector>\
             \n   <w:Selector Name=\"SystemCreationClassName\">CIM_ComputerSystem</w:Selector>\
             \n   <w:Selector Name=\"SystemName\">Intel(r) AMT</w:Selector>\
             \n  </w:SelectorSet>";
        let body = format!(
            "<r:RequestPowerStateChange_INPUT xmlns:r=\"{uri}\">\
             <r:PowerState>{state}</r:PowerState>\
             <r:ManagedElement>\
             <a:Address>{ANON}</a:Address>\
             <a:ReferenceParameters>\
             <w:ResourceURI>{CIM}CIM_ComputerSystem</w:ResourceURI>\
             <w:SelectorSet>\
             <w:Selector Name=\"CreationClassName\">CIM_ComputerSystem</w:Selector>\
             <w:Selector Name=\"Name\">ManagedSystem</w:Selector>\
             </w:SelectorSet>\
             </a:ReferenceParameters>\
             </r:ManagedElement>\
             </r:RequestPowerStateChange_INPUT>"
        );
        let resp = self.post(
            &envelope(&self.url, &action, &uri, selectors, &body),
            budget,
        )?;
        let rv = xml_text(&resp, "ReturnValue")
            .ok_or_else(|| anyhow!("no ReturnValue in RequestPowerStateChange response"))?;
        match rv {
            "0" => Ok(()),
            code => bail!(
                "RequestPowerStateChange({state}) failed: ReturnValue {code} ({})",
                return_value_name(code)
            ),
        }
    }

    /// Read `IPS_KVMRedirectionSettingData`, returning both the parsed view and
    /// the raw response — [`Client::set_kvm`] needs the raw instance to seed a
    /// Put with the fields it does not itself change.
    fn kvm_settings_raw(&self) -> Result<(KvmSettings, String)> {
        let uri = format!("{IPS}IPS_KVMRedirectionSettingData");
        let body = self.post(
            &envelope(&self.url, TRANSFER_GET, &uri, "", ""),
            CALL_TIMEOUT,
        )?;
        let flag = |name: &str| xml_text(&body, name).map(|v| v == "true").unwrap_or(false);
        let settings = KvmSettings {
            port_5900_enabled: flag("Is5900PortEnabled"),
            opt_in_policy: flag("OptInPolicy"),
            session_timeout: xml_text(&body, "SessionTimeout")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            enabled_by_mebx: flag("EnabledByMEBx"),
        };
        Ok((settings, body))
    }

    /// The KVM redirection settings as the ME currently reports them.
    pub fn kvm_settings(&self) -> Result<KvmSettings> {
        Ok(self.kvm_settings_raw()?.0)
    }

    /// `CIM_KVMRedirectionSAP.EnabledState`.
    ///
    /// Fetched **without a SelectorSet**. The instance's own keys
    /// (`Name=CIM_KVMRedirectionSAP`, `SystemName=ManagedSystem`) are reported
    /// by the Get but addressing the singleton with them fails routing, so
    /// unkeyed is the only form that works here.
    pub fn kvm_enabled_state(&self) -> Result<u16> {
        let uri = format!("{CIM}CIM_KVMRedirectionSAP");
        let body = self.post(
            &envelope(&self.url, TRANSFER_GET, &uri, "", ""),
            CALL_TIMEOUT,
        )?;
        xml_text(&body, "EnabledState")
            .ok_or_else(|| anyhow!("no EnabledState in CIM_KVMRedirectionSAP response"))?
            .parse()
            .context("unparseable EnabledState")
    }

    /// Write the KVM redirection settings.
    ///
    /// `rfb_password` is required the first time port 5900 is enabled — AMT
    /// refuses to open the port without one — and must be exactly 8 characters
    /// with an upper, a lower, a digit and a special character. Passing `None`
    /// leaves whatever is stored (a Get never returns it).
    pub fn set_kvm(
        &self,
        rfb_password: Option<&str>,
        port_5900: bool,
        opt_in: bool,
        session_timeout: u16,
    ) -> Result<()> {
        let (_, current) = self.kvm_settings_raw()?;
        let uri = format!("{IPS}IPS_KVMRedirectionSettingData");
        let body = kvm_put_body(&current, rfb_password, port_5900, opt_in, session_timeout);

        let selectors = "\n  <w:SelectorSet>\
             \n   <w:Selector Name=\"InstanceID\">Intel(r) KVM Redirection Settings</w:Selector>\
             \n  </w:SelectorSet>";
        self.post(
            &envelope(&self.url, TRANSFER_PUT, &uri, selectors, &body),
            CALL_TIMEOUT,
        )?;
        Ok(())
    }

    /// Invoke `CIM_KVMRedirectionSAP.RequestStateChange`, unkeyed (see
    /// [`Client::kvm_enabled_state`] for why). Errors on a non-zero ReturnValue.
    pub fn kvm_request_state(&self, state: u16) -> Result<()> {
        let uri = format!("{CIM}CIM_KVMRedirectionSAP");
        let action = format!("{uri}/RequestStateChange");
        let body = format!(
            "<r:RequestStateChange_INPUT xmlns:r=\"{uri}\">\
             <r:RequestedState>{state}</r:RequestedState>\
             </r:RequestStateChange_INPUT>"
        );
        let resp = self.post(&envelope(&self.url, &action, &uri, "", &body), CALL_TIMEOUT)?;
        let rv = xml_text(&resp, "ReturnValue")
            .ok_or_else(|| anyhow!("no ReturnValue in RequestStateChange response"))?;
        match rv {
            "0" => Ok(()),
            code => bail!(
                "RequestStateChange({state}) failed: ReturnValue {code} ({})",
                return_value_name(code)
            ),
        }
    }

    /// The one place every WS-Man request is issued, including the HTTP
    /// Digest handshake: POST unauthenticated, answer the 401 challenge,
    /// retry once. `budget` bounds the whole exchange — the second leg of the
    /// handshake only gets what the first left over (floored at
    /// [`MIN_CALL_TIMEOUT`]) — so a hung target cannot stretch one call past
    /// what the caller allotted. Returns the response body.
    fn post(&self, xml: &str, budget: Duration) -> Result<String> {
        let deadline = Instant::now() + budget;
        self.tls.set_deadline(deadline);
        let req = || {
            self.agent
                .post(&self.url)
                .set("Content-Type", "application/soap+xml;charset=UTF-8")
                .timeout(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .max(MIN_CALL_TIMEOUT),
                )
        };
        match req().send_string(xml) {
            Ok(resp) => self.response_body(resp),
            Err(ureq::Error::Status(401, resp)) => {
                *self.server.borrow_mut() = resp.header("Server").map(str::to_string);
                let header = resp
                    .header("WWW-Authenticate")
                    .ok_or_else(|| anyhow!("HTTP 401 without a WWW-Authenticate challenge"))?;
                let ch = parse_challenge(header)?;
                let authz = self.authorization(&ch, "POST");
                match req().set("Authorization", &authz).send_string(xml) {
                    Ok(resp) => self.response_body(resp),
                    Err(ureq::Error::Status(401, resp)) => {
                        Err(auth_failed(resp.header("WWW-Authenticate")))
                    }
                    Err(ureq::Error::Status(code, resp)) => Err(http_error(code, resp)),
                    Err(ureq::Error::Transport(t)) => Err(self.transport_error(t)),
                }
            }
            Err(ureq::Error::Status(code, resp)) => Err(http_error(code, resp)),
            Err(ureq::Error::Transport(t)) => Err(self.transport_error(t)),
        }
    }

    fn transport_error(&self, t: ureq::Transport) -> anyhow::Error {
        let transient =
            transient_kind(t.kind()) && (t.kind() == ureq::ErrorKind::Dns || retryable_io(&t));
        self.network_error(t, transient)
    }

    fn network_error<E: std::error::Error + Send + Sync + 'static>(
        &self,
        error: E,
        transient: bool,
    ) -> anyhow::Error {
        if let Some(tls) = tls_error(&error) {
            return anyhow!(
                "TLS verification or negotiation failed at {}: {tls}. \
                 Use the certified hostname and --tls-ca-file for a private issuer; \
                 the firmware must support compatible TLS 1.2 or newer algorithms",
                self.url
            );
        }
        let hint = if self.url.starts_with("https:") {
            "; HTTPS is required by default (16993). For intentionally unencrypted legacy HTTP, \
             explicitly select --allow-insecure-http; a port alone does not select HTTP"
        } else {
            "; a TLS-only machine requires HTTPS: remove --allow-insecure-http"
        };
        let err = anyhow!("AMT transport failure at {}: {error}{hint}", self.url);
        if transient {
            err.context(Transient)
        } else {
            err
        }
    }

    fn response_body(&self, resp: ureq::Response) -> Result<String> {
        if (300..400).contains(&resp.status()) {
            bail!(
                "WS-Man redirect (HTTP {}) refused; configure the endpoint directly",
                resp.status()
            );
        }
        resp.into_string().map_err(|error| {
            let transient = retryable_io(&error);
            self.network_error(error, transient)
                .context("reading WS-Man response")
        })
    }

    /// Build the `Authorization:` header value answering a digest challenge.
    fn authorization(&self, ch: &Challenge, method: &str) -> String {
        let nc = "00000001";
        let cnonce = cnonce();
        let response = digest_response(
            &self.user,
            &ch.realm,
            &self.password,
            method,
            self.path,
            &ch.nonce,
            ch.qop_auth.then_some((nc, cnonce.as_str())),
        );
        let mut h = format!(
            "Digest username={}, realm=\"{}\", nonce=\"{}\", uri=\"{}\"",
            quote_param(&self.user),
            ch.realm,
            ch.nonce,
            self.path
        );
        if ch.qop_auth {
            let _ = write!(h, ", qop=auth, nc={nc}, cnonce=\"{cnonce}\"");
        }
        let _ = write!(h, ", response=\"{response}\"");
        if let Some(o) = &ch.opaque {
            let _ = write!(h, ", opaque=\"{o}\"");
        }
        h
    }
}

/// io::Error::source can skip its immediate inner error; inspect get_ref too.
fn tls_error<'a>(err: &'a (dyn std::error::Error + 'static)) -> Option<&'a rustls::Error> {
    if let Some(tls) = err.downcast_ref::<rustls::Error>() {
        return Some(tls);
    }
    if let Some(inner) = err.downcast_ref::<io::Error>().and_then(io::Error::get_ref) {
        if let Some(tls) = tls_error(inner) {
            return Some(tls);
        }
    }
    err.source().and_then(tls_error)
}

fn retryable_io(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(io) = err.downcast_ref::<io::Error>() {
        return matches!(
            io.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::NotConnected
                | io::ErrorKind::BrokenPipe
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::HostUnreachable
                | io::ErrorKind::NetworkUnreachable
        ) || io.get_ref().is_some_and(|inner| retryable_io(inner));
    }
    err.source().is_some_and(retryable_io)
}

/// CIM `RequestPowerStateChange` ReturnValue names (DMTF value map subset).
fn return_value_name(code: &str) -> &'static str {
    match code {
        "1" => "Not Supported",
        "2" => "Unknown or Unspecified Error",
        "3" => "Cannot complete within Timeout Period",
        "4" => "Failed",
        "5" => "Invalid Parameter",
        "6" => "In Use",
        "4096" => "Job Started",
        "4097" => "Invalid State Transition",
        "4098" => "Use of Timeout Parameter Not Supported",
        "4099" => "Busy",
        _ => "unrecognized code",
    }
}

/// The error for a 401 that survives the digest response. A re-challenge
/// carrying `stale=true` means AMT rejected the *nonce* (expired or already
/// used), not the credentials — a different problem from a wrong password,
/// so the message says which one it saw.
fn auth_failed(rechallenge: Option<&str>) -> anyhow::Error {
    let stale = rechallenge
        .and_then(|h| parse_challenge(h).ok())
        .is_some_and(|ch| ch.stale);
    if stale {
        anyhow!(
            "authentication failed (HTTP 401 after digest response) — the re-challenge \
             carries stale=true, i.e. AMT rejected the nonce rather than the \
             credentials; retry the command"
        )
    } else {
        anyhow!(
            "authentication failed (HTTP 401 after digest response) — \
             check the AMT password (AMT_PASSWORD, --password-file or \
             --password-command) and the username"
        )
    }
}

/// Quote a Digest parameter value as an RFC 2617 quoted-string: `"` and `\`
/// inside it must be backslash-escaped, or a username containing either
/// would corrupt the header (the digest itself is computed over the raw
/// value, per the RFC).
/// Build the `IPS_KVMRedirectionSettingData` Put body from a Get response.
///
/// Seeded from `current` so fields this helper does not manage are echoed back
/// unchanged, but re-emitted in [`KVM_PUT_ORDER`] rather than the alphabetical
/// order AMT returns — see that constant for why the distinction matters. A
/// field absent from `current` is omitted: this firmware's schema is whatever
/// it reported, and inventing elements is another way to earn
/// `InvalidRepresentation`.
fn kvm_put_body(
    current: &str,
    rfb_password: Option<&str>,
    port_5900: bool,
    opt_in: bool,
    session_timeout: u16,
) -> String {
    let uri = format!("{IPS}IPS_KVMRedirectionSettingData");
    let mut body = format!("<h:IPS_KVMRedirectionSettingData xmlns:h=\"{uri}\">");
    for name in KVM_PUT_ORDER {
        // Every arm yields text that is already XML-ready. Values echoed from
        // the Get are encoded as the firmware sent them -- `xml_text` returns
        // the raw span and decodes nothing -- so escaping them again would
        // turn `A&amp;B` into `A&amp;amp;B` and corrupt a value on round trip.
        // Only the password is raw input, so only it is escaped here; the
        // booleans and integers cannot contain a metacharacter.
        let encoded = match *name {
            "Is5900PortEnabled" => Some(port_5900.to_string()),
            "OptInPolicy" => Some(opt_in.to_string()),
            "SessionTimeout" => Some(session_timeout.to_string()),
            "RFBPassword" => rfb_password.map(xml_escape),
            other => xml_text(current, other).map(str::to_owned),
        };
        if let Some(v) = encoded {
            let _ = write!(body, "<h:{name}>{v}</h:{name}>");
        }
    }
    body.push_str("</h:IPS_KVMRedirectionSettingData>");
    body
}

/// Escape *raw* text for an XML text node. The RFB password is chosen by a
/// human and must contain a special character, so `&`, `<` and `>` are live
/// possibilities — unescaped, they make the Put a malformed document and AMT
/// reports the useless `The XML content is not valid.`
///
/// Only ever apply this to input that is not already encoded: text read back
/// out of a response with [`xml_text`] arrives encoded, and escaping it a
/// second time changes the value.
fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn quote_param(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Map a non-2xx WS-Man response to an error, surfacing the SOAP fault
/// reason text when the body carries one.
fn http_error(code: u16, resp: ureq::Response) -> anyhow::Error {
    let body = resp.into_string().unwrap_or_default();
    match xml_text(&body, "Text").filter(|t| !t.is_empty()) {
        Some(reason) => anyhow!("AMT returned HTTP {code}: {reason}"),
        None => anyhow!("AMT returned HTTP {code}"),
    }
}

/// A WS-Man SOAP envelope. `to` is the endpoint URL; `selectors` is header
/// XML (the `SelectorSet` naming the service instance, or empty); `body` is
/// the `s:Body` content.
fn envelope(to: &str, action: &str, resource_uri: &str, selectors: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         \n<s:Envelope xmlns:s=\"{XMLNS_ENV}\" xmlns:a=\"{XMLNS_ADDR}\" xmlns:w=\"{XMLNS_WSMAN}\">\
         \n <s:Header>\
         \n  <a:Action s:mustUnderstand=\"true\">{action}</a:Action>\
         \n  <a:To s:mustUnderstand=\"true\">{to}</a:To>\
         \n  <w:ResourceURI s:mustUnderstand=\"true\">{resource_uri}</w:ResourceURI>\
         \n  <a:MessageID s:mustUnderstand=\"true\">{}</a:MessageID>\
         \n  <a:ReplyTo><a:Address>{ANON}</a:Address></a:ReplyTo>\
         \n  <w:OperationTimeout>PT60.000S</w:OperationTimeout>{selectors}\
         \n </s:Header>\
         \n <s:Body>{body}</s:Body>\
         \n</s:Envelope>",
        message_id()
    )
}

/// A unique-enough WS-Addressing MessageID (uuid-shaped, from the clock and
/// pid — no RNG dependency; uniqueness only needs to hold per conversation).
fn message_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let hi = (nanos >> 64) as u64;
    let lo = (nanos as u64) ^ ((process::id() as u64) << 48);
    format!(
        "uuid:{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        lo as u32,
        (lo >> 32) as u16,
        (lo >> 48) as u16 & 0xfff,
        hi as u16 & 0xfff,
        (hi >> 12) & 0xffff_ffff_ffff
    )
}

/// The fields of a `WWW-Authenticate: Digest` challenge this client uses.
#[derive(Debug)]
struct Challenge {
    realm: String,
    nonce: String,
    /// `qop=auth` was offered (RFC 2617); `false` means the challenge carried
    /// no `qop` at all and the legacy RFC 2069 response form applies.
    qop_auth: bool,
    opaque: Option<String>,
    /// The server flagged the previous nonce as stale (expired/used).
    stale: bool,
}

/// Parse a `WWW-Authenticate: Digest` header value: comma-separated
/// `key=value` pairs, values optionally quoted (AMT emits e.g.
/// `Digest realm="Digest:4BB9...", nonce="...",stale="false",qop="auth"`).
/// Errors on anything this client cannot honor rather than answering with a
/// response the server would compute differently: an `algorithm` other than
/// MD5, or a `qop` list that does not offer `auth`.
fn parse_challenge(header: &str) -> Result<Challenge> {
    let rest = header
        .strip_prefix("Digest")
        .ok_or_else(|| anyhow!("not a Digest challenge: {header:?}"))?;
    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut opaque = None;
    let mut algorithm = None;
    let mut stale = None;
    for (key, value) in parse_kv_list(rest) {
        match key.as_str() {
            "realm" => realm = Some(value),
            "nonce" => nonce = Some(value),
            "qop" => qop = Some(value),
            "opaque" => opaque = Some(value),
            "algorithm" => algorithm = Some(value),
            "stale" => stale = Some(value),
            _ => {}
        }
    }
    if let Some(alg) = algorithm.as_deref().map(str::trim) {
        if !alg.eq_ignore_ascii_case("MD5") {
            bail!(
                "digest challenge asks for algorithm {alg:?} — this helper implements \
                 MD5 only (what AMT advertises)"
            );
        }
    }
    let qop_auth = match qop.as_deref() {
        None => false,
        Some(q) if q.split(',').any(|v| v.trim() == "auth") => true,
        Some(q) => bail!(
            "digest challenge offers qop {q:?} only — this helper answers qop=auth \
             (or a challenge without qop)"
        ),
    };
    Ok(Challenge {
        realm: realm.ok_or_else(|| anyhow!("digest challenge without realm: {header:?}"))?,
        nonce: nonce.ok_or_else(|| anyhow!("digest challenge without nonce: {header:?}"))?,
        qop_auth,
        opaque,
        stale: stale
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("true")),
    })
}

/// Split `key=value, key="quoted value", ...` into pairs, honoring quotes.
fn parse_kv_list(s: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while matches!(chars.peek(), Some(' ' | '\t' | ',')) {
            chars.next();
        }
        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c == '=' {
                break;
            }
            key.push(c);
            chars.next();
        }
        if chars.next().is_none() {
            break; // no '=': end of input
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                value.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                value.push(c);
                chars.next();
            }
        }
        pairs.push((key.trim().to_ascii_lowercase(), value));
    }
    pairs
}

/// RFC 2617 digest response. `qop` carries `(nc, cnonce)` for `qop=auth`;
/// `None` selects the legacy (RFC 2069) form.
fn digest_response(
    user: &str,
    realm: &str,
    password: &str,
    method: &str,
    uri: &str,
    nonce: &str,
    qop: Option<(&str, &str)>,
) -> String {
    let ha1 = md5_hex(&format!("{user}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    match qop {
        Some((nc, cnonce)) => md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}")),
        None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// A per-invocation client nonce: clock + pid, hex. Uniqueness, not
/// unpredictability, is what qop=auth needs from the client side here.
fn cnonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:016x}", (nanos as u64) ^ ((process::id() as u64) << 32))
}

/// Text content of the first element whose *local* name matches exactly,
/// namespace prefix ignored (`<g:PowerState>2</g:PowerState>` → `"2"`).
/// Exact-match on the local name so `RequestedPowerState` or
/// `AvailableRequestedPowerStates` never satisfy a `PowerState` lookup.
/// Good enough for AMT's scalar response fields; not a general XML parser
/// (attribute values containing `>` would confuse it).
fn xml_text<'a>(xml: &'a str, local: &str) -> Option<&'a str> {
    let bytes = xml.as_bytes();
    let mut i = 0;
    while let Some(off) = xml[i..].find('<') {
        let start = i + off + 1;
        if start >= xml.len() {
            return None;
        }
        if matches!(bytes[start], b'/' | b'?' | b'!') {
            i = start;
            continue;
        }
        let mut j = start;
        while j < xml.len() && !matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/') {
            j += 1;
        }
        let name = &xml[start..j];
        let localname = name.rsplit(':').next().unwrap_or(name);
        let gt = xml[j..].find('>')? + j;
        if localname == local {
            if bytes[gt - 1] == b'/' {
                return Some(""); // self-closed element
            }
            let end = xml[gt + 1..].find('<')? + gt + 1;
            return Some(xml[gt + 1..end].trim());
        }
        i = gt + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(device: &str) -> String {
        Connection::new(device, false, None).unwrap().url
    }

    #[test]
    fn address_normalization() {
        assert_eq!(url("192.0.2.10"), "https://192.0.2.10:16993/wsman");
        assert_eq!(url("amt-host:16993"), "https://amt-host:16993/wsman");
        assert_eq!(url("https://192.0.2.10/"), "https://192.0.2.10:16993/wsman");
        assert_eq!(url("  amt-host.lab  "), "https://amt-host.lab:16993/wsman");
        assert_eq!(url("https://amt-host:1"), "https://amt-host:1/wsman");
        assert!(Connection::new("http://192.0.2.10", false, None).is_err());
        assert!(Connection::new("", false, None).is_err());
    }

    #[test]
    fn ipv6_literals_stay_bracketed_and_default_the_port() {
        assert_eq!(url("[fe80::1]"), "https://[fe80::1]:16993/wsman");
        assert_eq!(url("[fe80::1]:16993"), "https://[fe80::1]:16993/wsman");
        assert_eq!(url("https://[::1]/"), "https://[::1]:16993/wsman");
        assert_eq!(url("[2001:db8::10]"), "https://[2001:db8::10]:16993/wsman");
    }

    #[test]
    fn address_rejects_anything_beyond_host_and_port() {
        let rejected = [
            "user@192.0.2.10",  // userinfo
            "192.0.2.10/wsman", // path
            "192.0.2.10?x=1",   // query
            "192.0.2.10#frag",  // fragment
            "<192.0.2.10>",     // angle brackets
            "192.0.2.10&x",     // ampersand
            "\"192.0.2.10\"",   // quotes
            "10.0.0 5",         // whitespace inside
            "amt\thost",        // tab inside
            "192.0.2.10:",      // empty port
            "192.0.2.10:abc",   // non-numeric port
            "192.0.2.10:70000", // port out of range
            "192.0.2.10:0",     // port zero
            "fe80::1",          // unbracketed IPv6
            "[fe80::1",         // unterminated bracket
            "[not-an-address]", // bracketed non-IPv6
            "[fe80::1]x",       // junk after the literal
            "[fe80::1]:",       // bracketed with empty port
            "[]",               // empty literal
            "amt%host",         // not a hostname character
            ":16992",           // empty host
        ];
        for d in rejected {
            let err = Connection::new(d, false, None)
                .err()
                .unwrap_or_else(|| panic!("{d:?} must be rejected"));
            assert!(
                err.to_string().contains("device address"),
                "{d:?}: error should name the device address: {err}"
            );
        }
    }

    #[test]
    fn parses_amt_challenge() {
        // Verbatim from an OptiPlex 7060 (AMT 12.0.24.1314) on this bench —
        // note the missing spaces after some commas.
        let h = "Digest realm=\"Digest:4BB90000000000000000000000000000\", \
                 nonce=\"zicaAAAAAAAAAAAAa8tMnB1xbyLK5UFG\",stale=\"false\",qop=\"auth\"";
        let ch = parse_challenge(h).unwrap();
        assert_eq!(ch.realm, "Digest:4BB90000000000000000000000000000");
        assert_eq!(ch.nonce, "zicaAAAAAAAAAAAAa8tMnB1xbyLK5UFG");
        assert!(ch.qop_auth);
        assert!(ch.opaque.is_none());
        assert!(!ch.stale);
    }

    #[test]
    fn challenge_requires_digest_scheme() {
        assert!(parse_challenge("Basic realm=\"x\"").is_err());
    }

    #[test]
    fn challenge_algorithm_must_be_md5_or_absent() {
        let ok = [
            "Digest realm=\"r\", nonce=\"n\"",
            "Digest realm=\"r\", nonce=\"n\", algorithm=MD5",
            "Digest realm=\"r\", nonce=\"n\", algorithm=\"md5\"",
            "Digest realm=\"r\", nonce=\"n\", algorithm=Md5, qop=\"auth\"",
        ];
        for h in ok {
            parse_challenge(h).unwrap_or_else(|e| panic!("{h}: {e}"));
        }
        for alg in ["SHA-256", "MD5-sess", "SHA-512-256"] {
            let h = format!("Digest realm=\"r\", nonce=\"n\", algorithm={alg}");
            let err = parse_challenge(&h).expect_err(&h);
            assert!(err.to_string().contains(alg), "{err}");
        }
    }

    #[test]
    fn challenge_qop_must_offer_auth_or_be_absent() {
        assert!(
            !parse_challenge("Digest realm=\"r\", nonce=\"n\"")
                .unwrap()
                .qop_auth
        );
        assert!(
            parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=\"auth-int,auth\"")
                .unwrap()
                .qop_auth
        );
        assert!(
            parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=auth")
                .unwrap()
                .qop_auth
        );
        let err = parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=\"auth-int\"")
            .expect_err("auth-int only must not fall back to RFC 2069");
        assert!(err.to_string().contains("auth-int"), "{err}");
    }

    #[test]
    fn challenge_stale_flag() {
        for h in [
            "Digest realm=\"r\", nonce=\"n\", stale=true",
            "Digest realm=\"r\", nonce=\"n\", stale=\"TRUE\", qop=\"auth\"",
        ] {
            assert!(parse_challenge(h).unwrap().stale, "{h}");
        }
        for h in [
            "Digest realm=\"r\", nonce=\"n\", stale=\"false\"",
            "Digest realm=\"r\", nonce=\"n\"",
        ] {
            assert!(!parse_challenge(h).unwrap().stale, "{h}");
        }
    }

    #[test]
    fn second_401_reports_stale_nonce_distinctly() {
        let stale = auth_failed(Some("Digest realm=\"r\", nonce=\"n2\", stale=\"true\""));
        assert!(stale.to_string().contains("stale=true"), "{stale}");
        assert!(!stale.to_string().contains("AMT_PASSWORD"), "{stale}");
        for h in [
            Some("Digest realm=\"r\", nonce=\"n2\", stale=\"false\""),
            Some("Digest realm=\"r\", nonce=\"n2\""),
            Some("not a challenge"),
            None,
        ] {
            let err = auth_failed(h);
            assert!(err.to_string().contains("AMT_PASSWORD"), "{h:?}: {err}");
            assert!(!err.to_string().contains("stale"), "{h:?}: {err}");
        }
    }

    #[test]
    fn username_is_escaped_in_the_authorization_header() {
        assert_eq!(quote_param("admin"), "\"admin\"");
        assert_eq!(quote_param("a\"b\\c"), "\"a\\\"b\\\\c\"");

        let user = "ad\"min\\x";
        let client = Client::new(
            Connection::new("amt-host.example", false, None).unwrap(),
            user,
            "pw",
        );
        let ch = parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=\"auth\"").unwrap();
        let h = client.authorization(&ch, "POST");
        assert!(
            h.starts_with("Digest username=\"ad\\\"min\\\\x\", "),
            "header must escape the username: {h}"
        );
        // Reading the header back yields the raw username, and the digest was
        // computed over that raw value.
        let params = parse_kv_list(h.strip_prefix("Digest").unwrap());
        let get = |k: &str| {
            params
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("no {k} in {h}"))
        };
        assert_eq!(get("cnonce").len(), 16);
        let expect = digest_response(
            user,
            "r",
            "pw",
            "POST",
            "/wsman",
            "n",
            Some(("00000001", get("cnonce"))),
        );
        assert_eq!(get("response"), expect);
    }

    #[test]
    fn transient_kinds_are_the_link_flap_ones() {
        use ureq::ErrorKind::*;
        for k in [ConnectionFailed, Io, Dns] {
            assert!(transient_kind(k), "{k:?} should retry");
        }
        for k in [
            InvalidUrl,
            UnknownScheme,
            InsecureRequestHttpsOnly,
            TooManyRedirects,
            BadStatus,
            BadHeader,
            InvalidProxyUrl,
            ProxyConnect,
            ProxyUnauthorized,
            HTTP,
        ] {
            assert!(!transient_kind(k), "{k:?} must not retry");
        }
    }

    #[test]
    fn digest_rfc2617_vector() {
        // The worked example from RFC 2617 §3.5.
        let resp = digest_response(
            "Mufasa",
            "testrealm@host.com",
            "Circle Of Life",
            "GET",
            "/dir/index.html",
            "dcd98b7102dd2f0e8b11d0f600bfb0c093",
            Some(("00000001", "0a4f113b")),
        );
        assert_eq!(resp, "6629fae49393a05397450978507c4ef1");
    }

    #[test]
    fn xml_text_exact_local_name() {
        let body = "<g:CIM_AssociatedPowerManagementService \
                    xmlns:g=\"urn:x\"><g:AvailableRequestedPowerStates>2\
                    </g:AvailableRequestedPowerStates>\
                    <g:AvailableRequestedPowerStates>8\
                    </g:AvailableRequestedPowerStates>\
                    <g:PowerState>2</g:PowerState>\
                    <g:RequestedPowerState>8</g:RequestedPowerState>\
                    </g:CIM_AssociatedPowerManagementService>";
        assert_eq!(xml_text(body, "PowerState"), Some("2"));
        assert_eq!(xml_text(body, "RequestedPowerState"), Some("8"));
        assert_eq!(xml_text(body, "Missing"), None);
    }

    #[test]
    fn xml_text_return_value_and_fault() {
        let invoke = "<a:Body><g:RequestPowerStateChange_OUTPUT xmlns:g=\"urn:x\">\
                      <g:ReturnValue>0</g:ReturnValue>\
                      </g:RequestPowerStateChange_OUTPUT></a:Body>";
        assert_eq!(xml_text(invoke, "ReturnValue"), Some("0"));
        let fault = "<s:Fault><s:Reason>\
                     <s:Text xml:lang=\"en-US\">The sender was not authorized</s:Text>\
                     </s:Reason></s:Fault>";
        assert_eq!(
            xml_text(fault, "Text"),
            Some("The sender was not authorized")
        );
        let selfclosed = "<g:PowerState/>";
        assert_eq!(xml_text(selfclosed, "PowerState"), Some(""));
    }

    /// A real AMT 12.0.24 Get response: alphabetical, which is *not* the order
    /// a Put may use.
    const KVM_GET: &str = "<g:IPS_KVMRedirectionSettingData xmlns:g=\"urn:x\">\
         <g:BackToBackFbMode>false</g:BackToBackFbMode>\
         <g:DefaultScreen>0</g:DefaultScreen>\
         <g:ElementName>Intel(r) KVM Redirection Settings</g:ElementName>\
         <g:EnabledByMEBx>true</g:EnabledByMEBx>\
         <g:InstanceID>Intel(r) KVM Redirection Settings</g:InstanceID>\
         <g:Is5900PortEnabled>false</g:Is5900PortEnabled>\
         <g:OptInPolicy>true</g:OptInPolicy>\
         <g:OptInPolicyTimeout>300</g:OptInPolicyTimeout>\
         <g:RFBPassword></g:RFBPassword>\
         <g:SessionTimeout>3</g:SessionTimeout>\
         </g:IPS_KVMRedirectionSettingData>";

    /// The Put must be re-ordered into schema sequence, not echoed back in the
    /// order the Get supplied. AMT rejects the alphabetical form outright, so
    /// this is the difference between a working call and
    /// `InvalidRepresentation`.
    /// The sequence AMT 12.0.24 accepts, written out literally rather than
    /// derived from [`KVM_PUT_ORDER`]. Deriving it from the same constant the
    /// builder uses would only prove the builder is self-consistent — the
    /// firmware cares about this exact order, so the test has to state it
    /// independently to be able to catch the constant itself being wrong.
    const EXPECTED_PUT_SEQUENCE: &[&str] = &[
        "ElementName",
        "InstanceID",
        "EnabledByMEBx",
        "BackToBackFbMode",
        "Is5900PortEnabled",
        "OptInPolicy",
        "OptInPolicyTimeout",
        "SessionTimeout",
        "RFBPassword",
        "DefaultScreen",
    ];

    #[test]
    fn kvm_put_body_is_in_schema_order_not_alphabetical() {
        let body = kvm_put_body(KVM_GET, Some("Ab3!defG"), true, false, 0);
        let emitted: Vec<&str> = body
            .match_indices("<h:")
            .filter_map(|(i, _)| {
                let rest = &body[i + 3..];
                let name = &rest[..rest.find('>')?];
                // Skip the wrapper element, which carries an xmlns attribute.
                (!name.contains(' ')).then_some(name)
            })
            .collect();
        assert_eq!(
            emitted, EXPECTED_PUT_SEQUENCE,
            "Put body is not in the sequence AMT accepts: {body}"
        );
    }

    /// Values echoed from the Get are already XML-encoded; escaping them again
    /// silently rewrites them. `InstanceID` is an immutable key, so corrupting
    /// it on round trip is how a working Put starts failing.
    #[test]
    fn kvm_put_body_does_not_double_escape_echoed_fields() {
        let encoded = "<g:IPS_KVMRedirectionSettingData xmlns:g=\"urn:x\">\
             <g:ElementName>Tom &amp; Jerry</g:ElementName>\
             <g:InstanceID>a&lt;b</g:InstanceID>\
             </g:IPS_KVMRedirectionSettingData>";
        let body = kvm_put_body(encoded, None, true, false, 0);
        assert!(
            body.contains("<h:ElementName>Tom &amp; Jerry</h:ElementName>"),
            "echoed value was re-escaped: {body}"
        );
        assert!(
            body.contains("<h:InstanceID>a&lt;b</h:InstanceID>"),
            "echoed value was re-escaped: {body}"
        );
        assert!(!body.contains("&amp;amp;"), "double-escaped: {body}");
        assert!(!body.contains("&amp;lt;"), "double-escaped: {body}");
    }

    #[test]
    fn kvm_put_body_applies_overrides_and_escapes_the_password() {
        let body = kvm_put_body(KVM_GET, Some("A&b<c>1!"), true, false, 0);
        assert!(body.contains("<h:Is5900PortEnabled>true</h:Is5900PortEnabled>"));
        assert!(body.contains("<h:OptInPolicy>false</h:OptInPolicy>"));
        assert!(body.contains("<h:SessionTimeout>0</h:SessionTimeout>"));
        // Unescaped, these three characters make the document malformed and
        // AMT answers with the unhelpful "The XML content is not valid."
        assert!(body.contains("<h:RFBPassword>A&amp;b&lt;c&gt;1!</h:RFBPassword>"));
        // Untouched fields are echoed back verbatim.
        assert!(body.contains("<h:OptInPolicyTimeout>300</h:OptInPolicyTimeout>"));
        assert!(body.contains("<h:EnabledByMEBx>true</h:EnabledByMEBx>"));
    }

    /// Omitting the password must leave the stored one alone rather than
    /// writing an empty string, which would lock KVM out on the next connect.
    #[test]
    fn kvm_put_body_omits_the_password_when_not_supplied() {
        let body = kvm_put_body(KVM_GET, None, false, true, 3);
        assert!(!body.contains("RFBPassword"), "{body}");
        assert!(body.contains("<h:Is5900PortEnabled>false</h:Is5900PortEnabled>"));
    }

    /// A field this firmware does not report must not be invented.
    #[test]
    fn kvm_put_body_omits_fields_the_firmware_did_not_report() {
        let lean = "<g:IPS_KVMRedirectionSettingData xmlns:g=\"urn:x\">\
             <g:ElementName>n</g:ElementName><g:InstanceID>i</g:InstanceID>\
             </g:IPS_KVMRedirectionSettingData>";
        let body = kvm_put_body(lean, None, true, false, 0);
        assert!(!body.contains("OptInPolicyTimeout"), "{body}");
        assert!(!body.contains("BackToBackFbMode"), "{body}");
        assert!(body.contains("<h:ElementName>n</h:ElementName>"));
    }

    #[test]
    fn envelope_shape() {
        let e = envelope(
            "http://192.0.2.10:16992/wsman",
            "http://schemas.xmlsoap.org/ws/2004/09/transfer/Get",
            "urn:resource",
            "",
            "<x/>",
        );
        assert!(e.contains("transfer/Get</a:Action>"));
        assert!(e.contains("<a:To s:mustUnderstand=\"true\">http://192.0.2.10:16992/wsman"));
        assert!(e.contains("<w:ResourceURI s:mustUnderstand=\"true\">urn:resource"));
        assert!(e.contains("<a:MessageID s:mustUnderstand=\"true\">uuid:"));
        assert!(e.contains("<s:Body><x/></s:Body>"));
    }

    #[test]
    fn message_ids_are_unique() {
        assert_ne!(message_id(), message_id());
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
mod transport_tests;
