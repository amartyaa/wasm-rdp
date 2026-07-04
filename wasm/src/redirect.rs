//! RDSTLS (MS-RDPBCGR §2.2.17) — the small auth protocol used to complete a
//! deferred RDP Server Redirection handoff (e.g. GNOME Remote Desktop's
//! headless "Remote Login" mode handing off from its greeter-level daemon to
//! a per-user session). Runs directly over the already-upgraded TLS stream,
//! no X.224/MCS framing. We never decrypt the password blob — it and the
//! redirection GUID are forwarded verbatim from the Server Redirection Packet
//! into the Authentication Request PDU, exactly as the spec describes.

use anyhow::bail;
use base64::Engine as _;

const RDSTLS_VERSION_1: u16 = 1;
const RDSTLS_TYPE_CAPABILITIES: u16 = 1;
const RDSTLS_TYPE_AUTHREQ: u16 = 2;
const RDSTLS_TYPE_AUTHRSP: u16 = 4;
const RDSTLS_DATA_PASSWORD_CREDS: u16 = 1;
const RDSTLS_RESULT_SUCCESS: u32 = 0;

/// Parses the RDSTLS Capabilities PDU (server -> client, always 8 bytes) and
/// confirms the server supports RDSTLS_VERSION_1 (the only version we speak).
pub(crate) fn parse_capabilities(data: &[u8]) -> anyhow::Result<()> {
    if data.len() < 8 {
        bail!("RDSTLS Capabilities PDU too short: {} bytes", data.len());
    }
    let version = u16::from_le_bytes([data[0], data[1]]);
    let pdu_type = u16::from_le_bytes([data[2], data[3]]);
    let supported_versions = u16::from_le_bytes([data[6], data[7]]);
    if pdu_type != RDSTLS_TYPE_CAPABILITIES {
        bail!("expected RDSTLS_TYPE_CAPABILITIES, got {pdu_type:#06x}");
    }
    if version != RDSTLS_VERSION_1 || supported_versions & RDSTLS_VERSION_1 == 0 {
        bail!("server does not support RDSTLS_VERSION_1 (version={version:#06x} supported={supported_versions:#06x})");
    }
    Ok(())
}

/// Builds the RDSTLS Authentication Request PDU with Password Credentials.
/// `redirection_guid`/`user_name`/`domain`/`password` are the raw (opaque)
/// byte fields taken verbatim from the Server Redirection Packet — this PDU
/// exists specifically to replay them, not to reinterpret them.
pub(crate) fn build_auth_request(redirection_guid: &[u8], user_name: &[u8], domain: &[u8], password: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        8 /* fixed header */ + 8 /* 4 length prefixes */
            + redirection_guid.len() + user_name.len() + domain.len() + password.len(),
    );
    buf.extend_from_slice(&RDSTLS_VERSION_1.to_le_bytes());
    buf.extend_from_slice(&RDSTLS_TYPE_AUTHREQ.to_le_bytes());
    buf.extend_from_slice(&RDSTLS_DATA_PASSWORD_CREDS.to_le_bytes());
    for field in [redirection_guid, user_name, domain, password] {
        buf.extend_from_slice(&(field.len() as u16).to_le_bytes());
        buf.extend_from_slice(field);
    }
    buf
}

/// Parses the RDSTLS Authentication Response PDU (server -> client, always 10
/// bytes: Version + PduType + DataType + ResultCode). `Ok(())` on
/// `RDSTLS_RESULT_SUCCESS`, `Err` with a human-readable reason otherwise
/// (e.g. bad credentials, account disabled).
pub(crate) fn parse_auth_response(data: &[u8]) -> anyhow::Result<()> {
    if data.len() < 10 {
        bail!("RDSTLS Authentication Response PDU too short: {} bytes", data.len());
    }
    let pdu_type = u16::from_le_bytes([data[2], data[3]]);
    if pdu_type != RDSTLS_TYPE_AUTHRSP {
        bail!("expected RDSTLS_TYPE_AUTHRSP, got {pdu_type:#06x}");
    }
    let result_code = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
    if result_code != RDSTLS_RESULT_SUCCESS {
        bail!(
            "RDSTLS authentication failed: {} ({result_code:#010x})",
            describe_result_code(result_code)
        );
    }
    Ok(())
}

fn describe_result_code(code: u32) -> &'static str {
    match code {
        0x0000_0005 => "access denied",
        0x0000_052e => "logon failure (unknown username or bad password)",
        0x0000_0530 => "invalid logon hours",
        0x0000_0532 => "password expired",
        0x0000_0533 => "account disabled",
        0x0000_0773 => "password must change",
        0x0000_0775 => "account locked out",
        _ => "unknown error",
    }
}

/// Decodes a UTF-16LE, null-terminated byte string — the wire encoding the
/// Server Redirection Packet uses for its UserName/Domain fields — into a
/// Rust `String`.
pub(crate) fn utf16le_to_string(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Best-effort certificate pin check (MS-RDPBCGR §2.2.13.3: the client SHOULD
/// verify the redirected TLS certificate matches `TargetCertificate`). Per
/// project decision this only warns on mismatch — matching `mstsc`'s own
/// default "Continue With Insecure Connection" behavior — rather than
/// aborting the connection.
///
// ponytail: `target_certificate` is a Base64-encoded UTF-16 "Target
// Certificate Container" (MS-RDPBCGR 2.2.13.1.2), not a raw DER blob. Rather
// than fully parsing that container, we base64-decode it and check whether
// the presented DER appears as a substring — enough for a warn-only signal.
// Upgrade to full container parsing if this ever needs to become enforcing.
pub(crate) fn check_cert_pin(target_certificate: Option<&[u8]>, presented_der: &[u8]) {
    let Some(target_certificate) = target_certificate else {
        return;
    };
    let text = utf16le_to_string(target_certificate);
    match base64::engine::general_purpose::STANDARD.decode(text.trim()) {
        Ok(decoded) if contains_subslice(&decoded, presented_der) => {
            crate::log("[Redirect] cert pin check: OK (redirected TLS cert matches TargetCertificate)");
        }
        Ok(_) => {
            crate::log_error(
                "[Redirect] cert pin check: MISMATCH — redirected TLS cert does not match TargetCertificate \
                 from the Server Redirection Packet. Proceeding anyway (matches mstsc's default \
                 'Continue With Insecure Connection' behavior).",
            );
        }
        Err(e) => {
            crate::log_error(&format!("[Redirect] cert pin check: could not decode TargetCertificate ({e}), skipping"));
        }
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && needle.len() <= haystack.len() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Fields from a `RdpServerRedirectionPacket` needed to complete the reconnect,
/// extracted once and stashed across the WASM/JS reconnect hop (the fork type
/// itself isn't `wasm_bindgen`-friendly and there's no reason to carry fields
/// we never use, like TargetNetAddress or TSV URL).
pub(crate) struct PendingRedirect {
    pub routing_token: Option<Vec<u8>>,
    pub user_name: Vec<u8>,
    pub domain: Vec<u8>,
    pub password: Vec<u8>,
    pub redirection_guid: Vec<u8>,
    pub target_certificate: Option<Vec<u8>>,
}

impl PendingRedirect {
    pub(crate) fn from_packet(packet: &ironrdp::pdu::rdp::headers::RdpServerRedirectionPacket) -> Self {
        Self {
            routing_token: packet.load_balance_info.clone(),
            user_name: packet.user_name.clone().unwrap_or_default(),
            domain: packet.domain.clone().unwrap_or_default(),
            password: packet.password.clone().unwrap_or_default(),
            redirection_guid: packet.redirection_guid.clone().unwrap_or_default(),
            target_certificate: packet.target_certificate.clone(),
        }
    }
}
