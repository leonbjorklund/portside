//! HTTPS requests through WinHTTP, which brings the system's proxy and
//! certificate settings, and the SHA-256 (CNG) that checks what they download.

use crate::wide;
use std::{
    ffi::c_void,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE,
        WINHTTP_QUERY_CUSTOM, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
        WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    },
    Security::Cryptography::{BCRYPT_SHA256_ALG_HANDLE, BCryptHash},
};

const AGENT: &str = concat!("Portside/", env!("CARGO_PKG_VERSION"));

pub struct Reply {
    pub status: u32,
    pub retry_after: Option<u32>,
    /// The body of a 200 response. Any other leaves it empty.
    pub body: Vec<u8>,
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> Option<Self> {
        (!raw.is_null()).then(|| Self(raw))
    }

    /// The status code, or the numeric header called `name`.
    fn number(&self, name: Option<&str>) -> Option<u32> {
        let name = name.map(wide);
        let query = match name {
            Some(_) => WINHTTP_QUERY_CUSTOM,
            None => WINHTTP_QUERY_STATUS_CODE,
        };
        let (mut value, mut size) = (0u32, 4u32);
        let found = unsafe {
            WinHttpQueryHeaders(
                self.0,
                query | WINHTTP_QUERY_FLAG_NUMBER,
                name.as_ref().map_or(null(), |name| name.as_ptr()),
                (&raw mut value).cast(),
                &mut size,
                null_mut(),
            )
        };
        (found != 0).then_some(value)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// Requests one of the updater's own URLs with the extra `headers`
/// (CRLF-terminated lines). A 200 body over `limit` bytes fails.
pub fn get(url: &str, headers: &str, limit: usize) -> Option<Reply> {
    let (secure, rest) = match url.strip_prefix("https://") {
        Some(rest) => (true, rest),
        None => (false, url.strip_prefix("http://")?),
    };
    // Plain HTTP is only for the test server.
    if !secure && !cfg!(any(test, feature = "test-update-server")) {
        return None;
    }
    let (authority, path) = rest.split_at(rest.find('/')?);
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, if secure { 443 } else { 80 }),
    };
    unsafe {
        let access = if secure {
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY
        } else {
            WINHTTP_ACCESS_TYPE_NO_PROXY
        };
        let session = Handle::new(WinHttpOpen(wide(AGENT).as_ptr(), access, null(), null(), 0))?;
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000);
        let connection = Handle::new(WinHttpConnect(session.0, wide(host).as_ptr(), port, 0))?;
        let flags = if secure { WINHTTP_FLAG_SECURE } else { 0 };
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            wide("GET").as_ptr(),
            wide(path).as_ptr(),
            null(),
            null(),
            null(),
            flags,
        ))?;
        let headers = wide(headers);
        // -1 lets WinHTTP measure the null-terminated header text.
        if WinHttpSendRequest(request.0, headers.as_ptr(), u32::MAX, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, null_mut()) == 0
        {
            return None;
        }
        let mut reply = Reply {
            status: request.number(None)?,
            retry_after: request.number(Some("Retry-After")),
            body: Vec::new(),
        };
        if reply.status != 200 {
            return Some(reply);
        }
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return None;
            }
            if read == 0 {
                return Some(reply);
            }
            reply.body.extend_from_slice(&chunk[..read as usize]);
            if reply.body.len() > limit {
                return None;
            }
        }
    }
}

/// The SHA-256 of `data` in lowercase hex, as GitHub writes asset digests.
pub fn sha256(data: &[u8]) -> Option<String> {
    let mut digest = [0u8; 32];
    let status = unsafe {
        BCryptHash(
            BCRYPT_SHA256_ALG_HANDLE,
            null(),
            0,
            data.as_ptr(),
            data.len() as u32,
            digest.as_mut_ptr(),
            32,
        )
    };
    (status >= 0).then(|| digest.iter().map(|b| format!("{b:02x}")).collect())
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
    fn serve(response: &'static [u8]) -> String {
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
            let _ = stream.write_all(response);
        });
        url
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256(b"abc").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn reads_the_body_of_a_200_only() {
        let url = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
        let reply = get(&url, "", 100).unwrap();
        assert_eq!((reply.status, reply.body.as_slice()), (200, &b"hello"[..]));
        let url = serve(b"HTTP/1.1 403 Forbidden\r\nRetry-After: 120\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope");
        let reply = get(&url, "", 100).unwrap();
        assert_eq!(
            (reply.status, reply.retry_after, reply.body.len()),
            (403, Some(120), 0)
        );
    }

    #[test]
    fn refuses_a_response_over_the_limit() {
        let url = serve(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nhello");
        assert!(get(&url, "", 4).is_none());
    }
}
