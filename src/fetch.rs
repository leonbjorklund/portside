//! HTTPS requests through WinHTTP, which brings the system's proxy and
//! certificate settings, and the SHA-256 (CNG) that checks what they download.

use crate::wide;
use std::{
    fs::File,
    io::Write,
    path::Path,
    ptr::{null, null_mut},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::GetLastError,
    Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_CUSTOM, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    },
    Security::Cryptography::{
        BCRYPT_SHA256_ALGORITHM, BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash,
        BCryptFinishHash, BCryptHashData, BCryptOpenAlgorithmProvider,
    },
};

const AGENT: &str = concat!("Portside/", env!("CARGO_PKG_VERSION"));
/// A download that is still running after this long is dropped.
const TOTAL: Duration = Duration::from_secs(600);
/// Plain HTTP to the loopback address is for tests only.
const LOOPBACK_HTTP: bool = cfg!(any(test, feature = "test-update-server"));

/// The response head. The body goes to the caller's sink.
pub struct Reply {
    pub status: u32,
    pub etag: Option<String>,
    pub retry_after: Option<u64>,
    pub remaining: Option<u64>,
    pub reset: Option<u64>,
}

struct Handle(*mut std::ffi::c_void);

impl Handle {
    fn new(raw: *mut std::ffi::c_void, call: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(format!("{call} failed: {}", unsafe { GetLastError() }))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

#[derive(Debug, PartialEq)]
struct Target {
    secure: bool,
    host: String,
    port: u16,
    path: String,
}

fn parse_url(url: &str) -> Result<Target, String> {
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (_, Some(rest)) => (false, rest),
        _ => return Err(format!("unsupported URL: {url}")),
    };
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().map_err(|_| "bad port".to_string())?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    let name = host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if host.is_empty() || !name || port == 0 {
        return Err(format!("bad host in {url}"));
    }
    if !(secure || LOOPBACK_HTTP && host == "127.0.0.1") {
        return Err("plain HTTP is not allowed".into());
    }
    Ok(Target {
        secure,
        host: host.into(),
        port,
        path: path.into(),
    })
}

fn header(request: *mut std::ffi::c_void, name: &str) -> Option<String> {
    let name = wide(name);
    let mut bytes = 0u32;
    unsafe {
        WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_CUSTOM,
            name.as_ptr(),
            null_mut(),
            &mut bytes,
            null_mut(),
        );
        if bytes == 0 {
            return None;
        }
        let mut text = vec![0u16; bytes as usize / 2 + 1];
        bytes = (text.len() * 2) as u32;
        let found = WinHttpQueryHeaders(
            request,
            WINHTTP_QUERY_CUSTOM,
            name.as_ptr(),
            text.as_mut_ptr().cast(),
            &mut bytes,
            null_mut(),
        );
        (found != 0).then(|| String::from_utf16_lossy(&text[..bytes as usize / 2]))
    }
}

/// Requests `url` with the extra `headers` (CRLF-terminated lines). The body of
/// a 200 response goes to `sink`, up to `limit` bytes; any other response has
/// its body ignored.
pub fn get(
    url: &str,
    headers: &str,
    limit: u64,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<Reply, String> {
    let target = parse_url(url)?;
    let started = Instant::now();
    unsafe {
        let access = if LOOPBACK_HTTP && target.host == "127.0.0.1" {
            WINHTTP_ACCESS_TYPE_NO_PROXY
        } else {
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
        };
        let session = Handle::new(
            WinHttpOpen(wide(AGENT).as_ptr(), access, null(), null(), 0),
            "WinHttpOpen",
        )?;
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000);
        let connection = Handle::new(
            WinHttpConnect(session.0, wide(&target.host).as_ptr(), target.port, 0),
            "WinHttpConnect",
        )?;
        let flags = if target.secure {
            WINHTTP_FLAG_SECURE
        } else {
            0
        };
        let request = Handle::new(
            WinHttpOpenRequest(
                connection.0,
                wide("GET").as_ptr(),
                wide(&target.path).as_ptr(),
                null(),
                null(),
                null(),
                flags,
            ),
            "WinHttpOpenRequest",
        )?;
        let headers = wide(headers);
        // -1 lets WinHTTP measure the null-terminated header text.
        if WinHttpSendRequest(request.0, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0
        {
            return Err(format!("request failed: {}", GetLastError()));
        }
        let mut status = 0u32;
        let mut size = 4u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            null(),
            (&raw mut status).cast(),
            &mut size,
            null_mut(),
        ) == 0
        {
            return Err(format!("no status: {}", GetLastError()));
        }
        let number = |name| header(request.0, name).and_then(|text| text.trim().parse().ok());
        let reply = Reply {
            status,
            etag: header(request.0, "ETag"),
            retry_after: number("Retry-After"),
            remaining: number("X-RateLimit-Remaining"),
            reset: number("X-RateLimit-Reset"),
        };
        if status != 200 {
            return Ok(reply);
        }
        if number("Content-Length").is_some_and(|length| length > limit) {
            return Err("response is larger than expected".into());
        }
        let mut chunk = [0u8; 16 * 1024];
        let mut total = 0u64;
        loop {
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err(format!("read failed: {}", GetLastError()));
            }
            if read == 0 {
                return Ok(reply);
            }
            total += u64::from(read);
            if total > limit {
                return Err("response is larger than expected".into());
            }
            if started.elapsed() > TOTAL {
                return Err("download took too long".into());
            }
            sink(&chunk[..read as usize])?;
        }
    }
}

/// Downloads `url` into a new file at `path`, at most `limit` bytes, and
/// returns what arrived: its length and SHA-256.
pub fn download(url: &str, path: &Path, limit: u64) -> Result<(u64, [u8; 32]), String> {
    let mut file = File::create(path).map_err(|e| format!("could not create {path:?}: {e}"))?;
    let mut hash = Sha256::new()?;
    let mut length = 0u64;
    let reply = get(url, "", limit, &mut |chunk| {
        length += chunk.len() as u64;
        hash.update(chunk)?;
        file.write_all(chunk)
            .map_err(|e| format!("could not write {path:?}: {e}"))
    })?;
    if reply.status != 200 {
        return Err(format!("download answered HTTP {}", reply.status));
    }
    file.sync_all()
        .map_err(|e| format!("could not write {path:?}: {e}"))?;
    Ok((length, hash.finish()?))
}

pub struct Sha256 {
    algorithm: *mut std::ffi::c_void,
    hash: *mut std::ffi::c_void,
}

impl Sha256 {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let (mut algorithm, mut hash) = (null_mut(), null_mut());
            if BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_SHA256_ALGORITHM, null(), 0) < 0 {
                return Err("SHA-256 is unavailable".into());
            }
            // BCrypt allocates the hash object when none is passed.
            if BCryptCreateHash(algorithm, &mut hash, null_mut(), 0, null(), 0, 0) < 0 {
                BCryptCloseAlgorithmProvider(algorithm, 0);
                return Err("could not start a SHA-256 hash".into());
            }
            Ok(Self { algorithm, hash })
        }
    }

    pub fn update(&mut self, data: &[u8]) -> Result<(), String> {
        for part in data.chunks(1 << 30) {
            if unsafe { BCryptHashData(self.hash, part.as_ptr(), part.len() as u32, 0) } < 0 {
                return Err("could not hash the download".into());
            }
        }
        Ok(())
    }

    pub fn finish(self) -> Result<[u8; 32], String> {
        let mut digest = [0u8; 32];
        if unsafe { BCryptFinishHash(self.hash, digest.as_mut_ptr(), 32, 0) } < 0 {
            return Err("could not finish the hash".into());
        }
        Ok(digest)
    }
}

impl Drop for Sha256 {
    fn drop(&mut self) {
        unsafe {
            BCryptDestroyHash(self.hash);
            BCryptCloseAlgorithmProvider(self.algorithm, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    /// Answers one request with `response`, then closes the connection.
    fn serve(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://127.0.0.1:{}/x",
            listener.local_addr().unwrap().port()
        );
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let mut seen = 0;
            while !request[..seen].windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut request[seen..]) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => seen += n,
                }
            }
            let _ = stream.write_all(&response);
        });
        url
    }

    fn collect(url: &str, limit: u64) -> (Result<Reply, String>, Vec<u8>) {
        let mut body = Vec::new();
        let reply = get(url, "", limit, &mut |chunk| {
            body.extend_from_slice(chunk);
            Ok(())
        });
        (reply, body)
    }

    #[test]
    fn sha256_matches_known_vector() {
        let mut hash = Sha256::new().unwrap();
        hash.update(b"a").unwrap();
        hash.update(b"bc").unwrap();
        let hex: String = hash
            .finish()
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reads_headers_and_body_of_a_200() {
        let url = serve(
            b"HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nX-RateLimit-Remaining: 7\r\nX-RateLimit-Reset: 99\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                .to_vec(),
        );
        let (reply, body) = collect(&url, 100);
        let reply = reply.unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(reply.etag.as_deref(), Some("\"v1\""));
        assert_eq!(
            (reply.remaining, reply.reset, reply.retry_after),
            (Some(7), Some(99), None)
        );
        assert_eq!(body, b"hello");
    }

    #[test]
    fn ignores_the_body_of_other_statuses() {
        let url = serve(
            b"HTTP/1.1 403 Forbidden\r\nRetry-After: 120\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope"
                .to_vec(),
        );
        let (reply, body) = collect(&url, 100);
        assert_eq!(
            (reply.as_ref().unwrap().status, reply.unwrap().retry_after),
            (403, Some(120))
        );
        assert!(body.is_empty());
    }

    #[test]
    fn refuses_a_response_over_the_limit() {
        let url = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello".to_vec(),
        );
        assert!(collect(&url, 4).0.is_err());
        let url = serve(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello".to_vec());
        assert!(collect(&url, 4).0.is_err());
    }

    #[test]
    fn reports_a_server_that_hangs_up() {
        assert!(collect(&serve(Vec::new()), 100).0.is_err());
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", closed.local_addr().unwrap().port());
        drop(closed);
        assert!(collect(&url, 100).0.is_err());
    }

    #[test]
    fn only_https_or_test_loopback_urls_pass() {
        let target = parse_url("https://api.github.com/repos/a/b?x=1").unwrap();
        assert_eq!(
            target,
            Target {
                secure: true,
                host: "api.github.com".into(),
                port: 443,
                path: "/repos/a/b?x=1".into()
            }
        );
        assert_eq!(parse_url("http://127.0.0.1:8080").unwrap().path, "/");
        for url in [
            "http://github.com/x",
            "http://localhost:80/x",
            "ftp://github.com/x",
            "https:///x",
            "https://bad host/x",
            "https://github.com:0/x",
            "https://github.com:http/x",
        ] {
            assert!(parse_url(url).is_err(), "{url}");
        }
    }
}
