use anyhow::Context;
use base64::Engine;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;

pub(crate) fn verifier() -> String {
    format!(
        "{}{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

pub(crate) fn challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

pub(crate) fn wait_for_callback(
    listener: TcpListener,
    options: &CallbackOptions<'_>,
) -> anyhow::Result<(String, String)> {
    let (mut stream, _) = listener.accept()?;
    let mut buf = [0_u8; 8192];
    let len = stream.read(&mut buf)?;
    let request = String::from_utf8_lossy(&buf[..len]);
    let result =
        parse_callback_request_with_state(&request, options.error_context, options.expected_state);
    let (status, body) = match &result {
        Ok(_) => ("200 OK", options.success_body),
        Err(_) => ("400 Bad Request", options.failure_body),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    result
}

pub(crate) struct CallbackOptions<'a> {
    pub(crate) success_body: &'a str,
    pub(crate) failure_body: &'a str,
    pub(crate) error_context: &'a str,
    pub(crate) expected_state: Option<&'a str>,
}

pub(crate) fn parse_callback_request(
    request: &str,
    error_context: &str,
) -> anyhow::Result<(String, String)> {
    let line = request
        .lines()
        .next()
        .with_context(|| format!("empty {error_context} callback request"))?;
    let target = line
        .split_whitespace()
        .nth(1)
        .with_context(|| format!("malformed {error_context} callback request"))?;
    let url = url::Url::parse(&format!("http://localhost{target}"))?;
    let code = url
        .query_pairs()
        .find(|(key, _)| key == "code")
        .map(|(_, value)| value.into_owned())
        .with_context(|| format!("{error_context} callback missing code"))?;
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .with_context(|| format!("{error_context} callback missing state"))?;
    Ok((code, state))
}

pub(crate) fn parse_callback_request_with_state(
    request: &str,
    error_context: &str,
    expected_state: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let (code, state) = parse_callback_request(request, error_context)?;
    if let Some(expected_state) = expected_state
        && state != expected_state
    {
        anyhow::bail!("{error_context} state mismatch");
    }
    Ok((code, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_uses_s256_url_safe_no_pad() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn parse_callback_request_validates_expected_state() {
        let request =
            "GET /auth/callback?code=AUTH-CODE&state=STATE HTTP/1.1\r\nHost: localhost\r\n\r\n";

        assert_eq!(
            parse_callback_request_with_state(request, "OAuth", Some("STATE")).unwrap(),
            ("AUTH-CODE".to_string(), "STATE".to_string())
        );
        let err = parse_callback_request_with_state(request, "OAuth", Some("OTHER")).unwrap_err();
        assert!(err.to_string().contains("state mismatch"));
    }

    #[test]
    fn wait_for_callback_renders_failure_for_state_mismatch() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            write!(
                stream,
                "GET /auth/callback?code=AUTH-CODE&state=BAD HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });

        let err = wait_for_callback(
            listener,
            &CallbackOptions {
                success_body: "success",
                failure_body: "failure",
                error_context: "OAuth",
                expected_state: Some("GOOD"),
            },
        )
        .unwrap_err();
        let response = client.join().unwrap();

        assert!(err.to_string().contains("state mismatch"));
        assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(response.ends_with("failure"));
    }
}
