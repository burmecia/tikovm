//! TCP mode of the proxy, for the Postgres wire protocol.
//!
//! A stock Postgres client speaks length-prefixed frames from the first
//! byte, so the accept loop (`server.rs`) can sniff it unambiguously. Only
//! the startup phase is handled here:
//!
//! 1. SSLRequest / GSSENCRequest are answered with `N` (no TLS or GSSAPI
//!    encryption through the proxy); a default `sslmode=prefer` client then
//!    retries in plaintext and sends the StartupMessage.
//! 2. The StartupMessage carries the proxy JWT either in a standalone
//!    `tikovm_token` parameter or — for stock libpq clients, which cannot
//!    send arbitrary startup parameters and fold `options`/`PGOPTIONS` into
//!    a single server-parsed string — as `-c tikovm_token=<jwt>` inside the
//!    `options` parameter (e.g. `psql "... options='-c tikovm_token=<jwt>'"`).
//!    The token is verified once (`proto: "tcp"` claims) and stripped —
//!    a stock Postgres would reject the unknown parameter/GUC — then the
//!    rewritten StartupMessage is forwarded to `<guest_ip>:<port>`.
//! 3. Both directions are then spliced with `copy_bidirectional`; nothing
//!    beyond the first frame needs parsing, so the extended-query protocol,
//!    COPY, etc. all work untouched.
//!
//! Startup-phase failures are reported as a Postgres ErrorResponse (FATAL),
//! so psql prints a clean server error instead of hanging.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::debug;

use super::server::ProxyState;
use super::target::resolve_target;
use super::token::Proto;
use crate::vmm::vm::VmId;

/// StartupMessage parameter carrying the proxy JWT.
const TOKEN_PARAM: &str = "tikovm_token";
/// TOKEN_PARAM with the `=` suffix, for matching inside the options string.
const TOKEN_PARAM_EQ: &str = "tikovm_token=";

const PROTOCOL_V3: u32 = 196608; // 3.0, the only version our images speak
const SSL_REQUEST: u32 = 80877103;
const GSSENC_REQUEST: u32 = 80877104;
const CANCEL_REQUEST: u32 = 80877102;

/// Largest first frame accepted: a StartupMessage carrying a JWT is well
/// under 1 KiB; anything bigger is not a Postgres startup we handle.
const MAX_STARTUP_FRAME: usize = 16 * 1024;

/// Decide from the first bytes of a connection (peeked, not consumed)
/// whether it speaks the Postgres wire protocol. The frame length must be
/// plausible and the protocol code one of the known startup codes. HTTP
/// requests start with an ASCII method, which decodes to an absurdly large
/// "length", so the two protocols never collide in practice.
pub(crate) fn looks_like_postgres(prefix: &[u8]) -> bool {
    if prefix.len() < 8 {
        return false;
    }
    let len = u32::from_be_bytes(prefix[0..4].try_into().unwrap()) as usize;
    if !(8..=MAX_STARTUP_FRAME).contains(&len) {
        return false;
    }
    let code = u32::from_be_bytes(prefix[4..8].try_into().unwrap());
    matches!(
        code,
        PROTOCOL_V3 | SSL_REQUEST | GSSENC_REQUEST | CANCEL_REQUEST
    )
}

/// Entry point for a sniffed Postgres connection. Startup-phase failures
/// are sent to the client as a Postgres ErrorResponse before closing.
pub(crate) async fn handle(state: ProxyState, mut client: TcpStream) {
    if let Err(message) = run(&state, &mut client).await {
        debug!(error = %message, "tcp proxy connection rejected");
        let _ = client.write_all(&error_response(&message)).await;
    }
}

async fn run(state: &ProxyState, client: &mut TcpStream) -> Result<(), String> {
    // Negotiate until the real StartupMessage arrives.
    let frame = loop {
        let frame = read_frame(client).await?;
        let code = u32::from_be_bytes(frame[4..8].try_into().unwrap());
        match code {
            // Refuse encryption: the client retries in plaintext.
            SSL_REQUEST | GSSENC_REQUEST => {
                client
                    .write_all(b"N")
                    .await
                    .map_err(|e| format!("write encryption refusal: {e}"))?;
            }
            // References a backend secret that does not exist behind a proxy.
            CANCEL_REQUEST => {
                return Err("cancel requests are not supported through the proxy".to_string());
            }
            PROTOCOL_V3 => break frame,
            other => return Err(format!("unsupported postgres protocol code {other}")),
        }
    };

    let params = parse_startup(&frame).ok_or("malformed startup message")?;
    // The token arrives either as a standalone tikovm_token parameter or as
    // `-c tikovm_token=<jwt>` inside the `options` parameter (the only way
    // for stock libpq/psql). Strip both forms before forwarding: a stock
    // Postgres would reject the unknown parameter/GUC.
    let mut token = None;
    let mut forwarded: Vec<(String, String)> = Vec::with_capacity(params.len());
    for (key, value) in params {
        if key == TOKEN_PARAM {
            token = Some(value);
        } else if key == "options" {
            let (from_options, rest) = extract_from_options(&value);
            match from_options {
                Some(found) => {
                    token = Some(found);
                    if !rest.is_empty() {
                        forwarded.push((key, rest));
                    }
                }
                None => forwarded.push((key, value)),
            }
        } else {
            forwarded.push((key, value));
        }
    }
    let token = token.ok_or(format!(
        "missing proxy token (pass it as options='-c {TOKEN_PARAM}=<jwt>')"
    ))?;

    let claims = state.tokens.verify(&token).map_err(|e| e.to_string())?;
    if claims.proto != Proto::Tcp {
        return Err("token is not valid for TCP proxying".to_string());
    }

    // Track the session for auto-suspend (activity signal + the gate's
    // in-flight count); the guard lives until the splice ends below.
    let guard = state.vmm.activity().track(&VmId::from(claims.vm_id.clone()));
    let guest_ip = resolve_target(state, &claims)
        .await
        .map_err(|e| e.message)?;

    let mut upstream = TcpStream::connect((guest_ip, claims.port))
        .await
        .map_err(|e| format!("failed to reach guest port {}: {e}", claims.port))?;

    upstream
        .write_all(&build_startup(&forwarded))
        .await
        .map_err(|e| format!("forward startup message: {e}"))?;

    // From here on the connection is a plain byte splice.
    let _guard = guard;
    match tokio::io::copy_bidirectional(client, &mut upstream).await {
        Ok((to_guest, to_client)) => {
            debug!(to_guest, to_client, "tcp proxy session ended")
        }
        Err(e) => debug!(error = %e, "tcp proxy session ended with error"),
    }
    Ok(())
}

/// Read exactly one length-prefixed startup-phase frame (first 4 bytes are
/// the frame length, including themselves).
async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read frame length: {e}"))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if !(8..=MAX_STARTUP_FRAME).contains(&len) {
        return Err(format!("implausible startup frame length {len}"));
    }
    let mut frame = len_buf.to_vec();
    frame.resize(len, 0);
    stream
        .read_exact(&mut frame[4..])
        .await
        .map_err(|e| format!("read frame body: {e}"))?;
    Ok(frame)
}

/// Parse a protocol-3.0 StartupMessage frame (including its 4-byte length)
/// into its key/value parameters.
fn parse_startup(frame: &[u8]) -> Option<Vec<(String, String)>> {
    let mut rest = &frame[8..]; // skip length + protocol version
    let mut params = Vec::new();
    loop {
        let nul = rest.iter().position(|&b| b == 0)?;
        let key = &rest[..nul];
        rest = &rest[nul + 1..];
        if key.is_empty() {
            // Empty key = the terminator.
            return Some(params);
        }
        let nul = rest.iter().position(|&b| b == 0)?;
        let value = &rest[..nul];
        rest = &rest[nul + 1..];
        params.push((
            String::from_utf8_lossy(key).into_owned(),
            String::from_utf8_lossy(value).into_owned(),
        ));
    }
}

/// Serialize startup parameters back into a protocol-3.0 StartupMessage
/// frame with a recomputed length.
fn build_startup(params: &[(String, String)]) -> Vec<u8> {
    let mut body = PROTOCOL_V3.to_be_bytes().to_vec();
    for (key, value) in params {
        body.extend_from_slice(key.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0); // terminator
    let mut frame = ((body.len() + 4) as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}

/// Split a libpq `options` value into words the way the postmaster does:
/// whitespace-separated, `\` escapes the next byte, single/double quotes
/// group. Returns each word's span in the original string (quoting intact)
/// plus its unescaped text for comparison.
fn split_opts(options: &str) -> Vec<(usize, usize, String)> {
    let bytes = options.as_bytes();
    let mut words = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        let mut word = Vec::new();
        let mut squote = false;
        let mut dquote = false;
        while i < bytes.len() {
            let b = bytes[i];
            if !squote && !dquote && b.is_ascii_whitespace() {
                break;
            }
            if b == b'\\' && !squote && i + 1 < bytes.len() {
                word.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if b == b'\'' && !dquote {
                squote = !squote;
                i += 1;
                continue;
            }
            if b == b'"' && !squote {
                dquote = !dquote;
                i += 1;
                continue;
            }
            word.push(b);
            i += 1;
        }
        words.push((start, i, String::from_utf8_lossy(&word).into_owned()));
    }
    words
}

/// Extract `-c tikovm_token=<jwt>` from a libpq `options` value (also the
/// glued `-ctikovm_token=<jwt>` form), returning the token and the options
/// with just that setting removed. Other words keep their original text,
/// rejoined with single spaces.
fn extract_from_options(options: &str) -> (Option<String>, String) {
    let words = split_opts(options);
    let mut token = None;
    let mut dropped: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let text = &words[i].2;
        if text == "-c"
            && i + 1 < words.len()
            && let Some(value) = words[i + 1].2.strip_prefix(TOKEN_PARAM_EQ)
        {
            if token.is_none() {
                token = Some(value.to_string());
            }
            dropped.extend([i, i + 1]);
            i += 2;
            continue;
        }
        if let Some(rest) = text.strip_prefix("-c")
            && !rest.is_empty()
            && let Some(value) = rest.strip_prefix(TOKEN_PARAM_EQ)
        {
            if token.is_none() {
                token = Some(value.to_string());
            }
            dropped.push(i);
        }
        i += 1;
    }
    if dropped.is_empty() {
        return (None, options.to_string());
    }
    let rest = words
        .iter()
        .enumerate()
        .filter(|(idx, _)| !dropped.contains(idx))
        .map(|(_, (start, end, _))| &options[*start..*end])
        .collect::<Vec<_>>()
        .join(" ");
    (token, rest)
}

/// Build a Postgres ErrorResponse frame (severity FATAL, SQLSTATE 28000 =
/// invalid authorization specification) carrying `message`.
fn error_response(message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (code, value) in [
        ('S', "FATAL"),
        ('V', "FATAL"),
        ('C', "28000"),
        ('M', message),
    ] {
        body.push(code as u8);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
    }
    body.push(0); // terminator
    let mut frame = vec![b'E'];
    frame.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup_frame(params: &[(String, String)]) -> Vec<u8> {
        build_startup(params)
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn sniff_accepts_postgres_startup() {
        let frame = startup_frame(&params(&[("user", "postgres")]));
        assert!(looks_like_postgres(&frame[..8]));
    }

    #[test]
    fn sniff_accepts_ssl_and_gss_requests() {
        for code in [SSL_REQUEST, GSSENC_REQUEST, CANCEL_REQUEST] {
            let mut frame = 8u32.to_be_bytes().to_vec();
            frame.extend_from_slice(&code.to_be_bytes());
            assert!(looks_like_postgres(&frame));
        }
    }

    #[test]
    fn sniff_rejects_http() {
        assert!(!looks_like_postgres(b"GET / HT"));
        assert!(!looks_like_postgres(b"POST /ap"));
    }

    #[test]
    fn sniff_rejects_short_or_implausible() {
        assert!(!looks_like_postgres(b"\0\0\0"));
        // Length field smaller than a frame header.
        assert!(!looks_like_postgres(&4u32.to_be_bytes()));
        // Length field beyond the startup cap.
        let mut frame = (MAX_STARTUP_FRAME as u32 + 1).to_be_bytes().to_vec();
        frame.extend_from_slice(&PROTOCOL_V3.to_be_bytes());
        assert!(!looks_like_postgres(&frame));
        // Unknown protocol code.
        let mut frame = 8u32.to_be_bytes().to_vec();
        frame.extend_from_slice(&42u32.to_be_bytes());
        assert!(!looks_like_postgres(&frame));
    }

    #[test]
    fn startup_roundtrip() {
        let original = params(&[
            ("user", "postgres"),
            ("database", "postgres"),
            (TOKEN_PARAM, "jwt"),
            ("application_name", "psql"),
        ]);
        let frame = startup_frame(&original);
        let parsed = parse_startup(&frame).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn rewrite_strips_token() {
        let original = params(&[("user", "postgres"), (TOKEN_PARAM, "jwt")]);
        let frame = startup_frame(&original);
        let parsed = parse_startup(&frame).unwrap();
        let forwarded: Vec<(String, String)> = parsed
            .into_iter()
            .filter(|(key, _)| key != TOKEN_PARAM)
            .collect();
        let rewritten = build_startup(&forwarded);
        let reparsed = parse_startup(&rewritten).unwrap();
        assert_eq!(reparsed, params(&[("user", "postgres")]));
        // The recomputed length must match the frame size.
        let len = u32::from_be_bytes(rewritten[0..4].try_into().unwrap()) as usize;
        assert_eq!(len, rewritten.len());
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(parse_startup(b"\0\0\0\x0c\0\x03\0\0user").is_none());
    }

    #[test]
    fn options_extract_basic() {
        let (token, rest) = extract_from_options("-c tikovm_token=JWT");
        assert_eq!(token.as_deref(), Some("JWT"));
        assert_eq!(rest, "");
    }

    #[test]
    fn options_extract_preserves_others() {
        let (token, rest) = extract_from_options(
            "-c search_path=public -c tikovm_token=JWT --opt='a b' -c geqo=off",
        );
        assert_eq!(token.as_deref(), Some("JWT"));
        assert_eq!(rest, "-c search_path=public --opt='a b' -c geqo=off");
    }

    #[test]
    fn options_extract_glued_and_quoted() {
        let (token, rest) = extract_from_options("-ctikovm_token=JWT");
        assert_eq!(token.as_deref(), Some("JWT"));
        assert_eq!(rest, "");

        let (token, _) = extract_from_options("-c 'tikovm_token=quoted'");
        assert_eq!(token.as_deref(), Some("quoted"));
    }

    #[test]
    fn options_extract_absent() {
        let (token, rest) = extract_from_options("-c geqo=off");
        assert_eq!(token, None);
        assert_eq!(rest, "-c geqo=off");
    }

    #[test]
    fn error_frame_is_well_formed() {
        let frame = error_response("nope");
        assert_eq!(frame[0], b'E');
        let len = u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 1);
        assert_eq!(*frame.last().unwrap(), 0);
        assert!(frame.windows(4).any(|w| w == b"nope"));
    }
}
