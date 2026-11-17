use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::{log::Logger, project};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub fn secret_path(dir: &Path) -> PathBuf {
    dir.join("webhook-secret")
}

/// Returns the secret, creating a random one readable only by the owner if there is none.
pub fn ensure_secret(dir: &Path) -> Result<(String, bool)> {
    if let Ok(secret) = load_secret(dir) {
        return Ok((secret, false));
    }
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .context("cannot read /dev/urandom")?;
    let secret = hex(&bytes);
    let path = secret_path(dir);
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut f| f.write_all(secret.as_bytes()))
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok((secret, true))
}

/// Creates the secret if needed and logs where it is and what GitHub needs.
pub fn ensure_secret_logged(dir: &Path, log: &Logger) -> Result<String> {
    let (secret, created) = ensure_secret(dir)?;
    if created {
        log.event(&format!(
            "created the webhook secret {}; in GitHub, add a webhook under Settings > Webhooks \
             with content type application/json, the push event, and that secret",
            secret_path(dir).display()
        ));
    }
    Ok(secret)
}

pub fn load_secret(dir: &Path) -> Result<String> {
    let path = secret_path(dir);
    let secret = fs::read_to_string(&path)
        .with_context(|| format!("cannot read the webhook secret {}", path.display()))?;
    Ok(secret.trim().to_string())
}

/// Which pushes wake the watcher: those to BRANCH, and to REPOSITORY when it is set.
pub struct Filter {
    pub branch: String,
    pub repository: Option<String>,
}

impl Filter {
    fn matches(&self, body: &[u8]) -> Result<(), &'static str> {
        let payload: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| "ignored: not a JSON push payload")?;
        if payload["ref"].as_str() != Some(&format!("refs/heads/{}", self.branch)) {
            return Err("ignored: another branch");
        }
        if let Some(expected) = &self.repository {
            let repo = &payload["repository"];
            let urls = ["html_url", "clone_url", "ssh_url"].map(|k| repo[k].as_str());
            if !urls
                .into_iter()
                .flatten()
                .any(|url| project::same_repository(url, expected))
            {
                return Err("ignored: another repository");
            }
        }
        Ok(())
    }
}

/// Binds now, so a bad address fails at startup, then serves on a background thread.
/// Each signed push to the watched branch sends one wake-up.
pub fn listen(
    addr: &str,
    secret: String,
    filter: Filter,
    log: Logger,
    wake: Sender<()>,
) -> Result<()> {
    let listener = TcpListener::bind(addr).with_context(|| format!("cannot listen on {addr}"))?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (status, msg) = handle(&stream, secret.as_bytes(), &filter, &wake);
            if status >= 400 {
                log.info(&format!("webhook: rejected request ({status} {msg})"));
            }
            respond(stream, status, msg);
        }
    });
    Ok(())
}

fn handle(
    stream: &TcpStream,
    secret: &[u8],
    filter: &Filter,
    wake: &Sender<()>,
) -> (u16, &'static str) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let req = match read_request(stream) {
        Ok(req) => req,
        Err(status) => return status,
    };
    if req.method != "POST" {
        return (405, "method not allowed");
    }
    let signature = req.header("x-hub-signature-256").unwrap_or_default();
    if !signature_valid(secret, &req.body, signature) {
        return (401, "bad signature");
    }
    match req.header("x-github-event") {
        Some("ping") => (200, "pong"),
        Some("push") => match filter.matches(&req.body) {
            Ok(()) => {
                let _ = wake.send(());
                (202, "deploy check queued")
            }
            Err(msg) => (200, msg),
        },
        _ => (200, "ignored"),
    }
}

struct Request {
    method: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_request(stream: impl Read) -> Result<Request, (u16, &'static str)> {
    const BAD: (u16, &str) = (400, "bad request");
    let mut reader = BufReader::new(stream);
    let mut header_bytes = 0;
    let mut next_line = || -> Result<String, (u16, &'static str)> {
        let mut line = String::new();
        let n = (&mut reader)
            .take((MAX_HEADER_BYTES - header_bytes) as u64)
            .read_line(&mut line)
            .map_err(|_| BAD)?;
        header_bytes += n;
        if !line.ends_with('\n') {
            return Err(if header_bytes >= MAX_HEADER_BYTES {
                (431, "headers too large")
            } else {
                BAD
            });
        }
        Ok(line.trim_end().to_string())
    };
    let request_line = next_line()?;
    let method = request_line
        .split(' ')
        .next()
        .unwrap_or_default()
        .to_string();
    let mut headers = Vec::new();
    loop {
        let line = next_line()?;
        if line.is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').ok_or(BAD)?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let mut req = Request {
        method,
        headers,
        body: Vec::new(),
    };
    let len: usize = match req.header("content-length") {
        Some(v) => v.parse().map_err(|_| BAD)?,
        None => 0,
    };
    if len > MAX_BODY_BYTES {
        return Err((413, "payload too large"));
    }
    req.body = vec![0; len];
    reader.read_exact(&mut req.body).map_err(|_| BAD)?;
    Ok(req)
}

fn respond(mut stream: TcpStream, status: u16, msg: &str) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Bad Request",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{msg}\n",
        msg.len() + 1
    );
}

/// Checks GitHub's `X-Hub-Signature-256: sha256=<hex HMAC of the body>` in constant time.
pub fn signature_valid(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(expected) = header.strip_prefix("sha256=").and_then(unhex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example from GitHub's "Validating webhook deliveries" docs.
    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const SIG: &str = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn accepts_githubs_example_signature() {
        assert!(signature_valid(SECRET, BODY, SIG));
    }

    #[test]
    fn rejects_bad_signatures() {
        assert!(!signature_valid(SECRET, b"Hello, World?", SIG));
        assert!(!signature_valid(b"wrong", BODY, SIG));
        assert!(!signature_valid(
            SECRET,
            BODY,
            &SIG.replace("sha256=", "sha1=")
        ));
        assert!(!signature_valid(SECRET, BODY, "sha256=zz"));
        assert!(!signature_valid(SECRET, BODY, ""));
    }

    #[test]
    fn parses_a_request() {
        let raw = b"POST /hook HTTP/1.1\r\nX-GitHub-Event: push\r\nContent-Length: 5\r\n\r\nhello";
        let req = read_request(&raw[..]).ok().unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.header("x-github-event"), Some("push"));
        assert_eq!(req.body, b"hello");
    }

    #[test]
    fn filters_pushes_by_branch_and_repository() {
        let filter = Filter {
            branch: "main".into(),
            repository: Some("https://github.com/o/app".into()),
        };
        let push = |r: &str, url: &str| {
            format!(r#"{{"ref":"{r}","repository":{{"html_url":"{url}"}}}}"#).into_bytes()
        };
        assert_eq!(
            filter.matches(&push("refs/heads/main", "https://github.com/o/app")),
            Ok(())
        );
        assert_eq!(
            filter.matches(&push("refs/heads/dev", "https://github.com/o/app")),
            Err("ignored: another branch")
        );
        assert_eq!(
            filter.matches(&push("refs/tags/main", "https://github.com/o/app")),
            Err("ignored: another branch")
        );
        assert_eq!(
            filter.matches(&push("refs/heads/main", "https://github.com/o/other")),
            Err("ignored: another repository")
        );
        assert!(filter.matches(b"not json").is_err());
        let any_repo = Filter {
            branch: "main".into(),
            repository: None,
        };
        assert_eq!(any_repo.matches(br#"{"ref":"refs/heads/main"}"#), Ok(()));
    }

    #[test]
    fn rejects_malformed_and_oversized_requests() {
        assert_eq!(
            read_request(&b"POST / HTTP/1.1\r\nnocolon\r\n\r\n"[..]).err(),
            Some((400, "bad request"))
        );
        let big = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(read_request(big.as_bytes()).err().map(|e| e.0), Some(413));
        let long = format!(
            "POST / HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(MAX_HEADER_BYTES)
        );
        assert_eq!(read_request(long.as_bytes()).err().map(|e| e.0), Some(431));
        assert!(read_request(&b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nshort"[..]).is_err());
    }
}
