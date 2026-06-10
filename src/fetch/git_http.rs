// SPDX-License-Identifier: EUPL-1.2

use std::{
    any::Any,
    error::Error,
    io::{
        self,
        BufRead,
        BufReader,
        Cursor,
        Read,
        Write,
    },
    sync::{
        Arc,
        Mutex,
    },
};

use eyre::ContextCompat as _;
use gix_transport::{
    IsSpuriousError as _,
    client::blocking_io::{
        Transport as BlockingTransport,
        http::{
            Error as HttpError,
            GetResponse,
            Http,
            PostBodyDataKind,
            PostResponse,
            Transport as HttpTransport,
            connect_http,
        },
    },
};
use ureq::{
    Agent,
    Body,
    Error as UreqError,
    RequestBuilder,
    http::{
        HeaderMap,
        Response,
    },
};

use super::{
    auth,
    http,
};

type UreqTransport = HttpTransport<UreqHttp>;

pub(super) fn connect(parsed_url: gix::Url) -> UreqTransport {
    connect_http(
        UreqHttp::default(),
        parsed_url,
        gix_transport::Protocol::V2,
        false,
    )
}

pub(super) fn boxed(parsed_url: gix::Url) -> Box<dyn BlockingTransport + Send> {
    Box::new(connect(parsed_url))
}

#[derive(Clone, Copy)]
pub(super) struct UreqHttp {
    agent: &'static Agent,
}

impl Default for UreqHttp {
    fn default() -> Self {
        Self {
            agent: http::agent(),
        }
    }
}

impl Http for UreqHttp {
    type Headers = LazyHeaders;
    type PostBody = LazyPostBody;
    type ResponseBody = LazyResponseBody;

    fn get(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<GetResponse<Self::Headers, Self::ResponseBody>, HttpError> {
        let response = send_ureq(self.agent, Method::Get, url, headers, &[])?;
        Ok(GetResponse {
            headers: LazyHeaders::ready(response.headers),
            body:    LazyResponseBody::ready(response.body),
        })
    }

    fn post(
        &mut self,
        url: &str,
        _base_url: &str,
        headers: impl IntoIterator<Item = impl AsRef<str>>,
        _body: PostBodyDataKind,
    ) -> Result<PostResponse<Self::Headers, Self::ResponseBody, Self::PostBody>, HttpError> {
        let state = Arc::new(Mutex::new(PendingPost {
            agent:        self.agent,
            url:          url.to_owned(),
            headers:      headers
                .into_iter()
                .map(|header| header.as_ref().to_owned())
                .collect(),
            request_body: Vec::new(),
            response:     None,
        }));
        Ok(PostResponse {
            post_body: LazyPostBody {
                state: Arc::clone(&state),
            },
            headers:   LazyHeaders::pending(Arc::clone(&state)),
            body:      LazyResponseBody::pending(state),
        })
    }

    fn configure(
        &mut self,
        _config: &dyn Any,
    ) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
}

impl Method {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

struct UreqResponse {
    headers: Result<Vec<u8>, HttpFailure>,
    body:    LazyBody,
}

#[derive(Clone)]
struct HttpFailure {
    kind:    io::ErrorKind,
    message: String,
}

impl HttpFailure {
    fn from_status(method: Method, url: &str, status: u16) -> Self {
        let kind = if status == 401 {
            io::ErrorKind::PermissionDenied
        } else if status >= 500 {
            io::ErrorKind::ConnectionAborted
        } else {
            io::ErrorKind::Other
        };
        let detail = match status {
            401 => "authentication required".to_owned(),
            403 => "permission denied".to_owned(),
            500.. => "remote server error".to_owned(),
            _ => format!("unexpected HTTP status {status}"),
        };
        Self {
            kind,
            message: format!(
                "git HTTP {} {url}: {detail} (HTTP {status})",
                method.as_str()
            ),
        }
    }

    /// `Other`, not `PermissionDenied`, so gix does not start its own
    /// credential cascade (askpass).
    fn no_usable_credential(host: &str) -> Self {
        Self {
            kind:    io::ErrorKind::Other,
            message: format!(
                "authentication required for {host}: no usable credential (tried nix.conf \
                 access-tokens, ~/.netrc, git credential helper)"
            ),
        }
    }

    fn from_http_error(url: &str, error: &HttpError) -> Self {
        let kind = if error.is_spurious() {
            io::ErrorKind::ConnectionAborted
        } else {
            io::ErrorKind::Other
        };
        Self {
            kind,
            message: format!("git HTTP POST {url}: {error}"),
        }
    }

    fn into_error(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }
}

enum LazyBody {
    Ready(Box<dyn Read + Send>),
    Error(Option<HttpFailure>),
}

impl Read for LazyBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            Self::Ready(ref mut body) => body.read(buf),
            Self::Error(ref mut failure) => {
                Err(failure.take().map_or_else(
                    || io::Error::other("response body already failed"),
                    HttpFailure::into_error,
                ))
            },
        }
    }
}

struct PendingResponse {
    headers: Result<Vec<u8>, HttpFailure>,
    body:    Option<LazyBody>,
}

struct PendingPost {
    agent:        &'static Agent,
    url:          String,
    headers:      Vec<String>,
    request_body: Vec<u8>,
    response:     Option<Result<PendingResponse, HttpFailure>>,
}

pub(super) struct LazyPostBody {
    state: Arc<Mutex<PendingPost>>,
}

impl Write for LazyPostBody {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| io::Error::other("poisoned post body"))?;
        if guard.response.is_some() {
            return Err(io::Error::other(
                "git HTTP POST body written after the request was sent",
            ));
        }
        guard.request_body.extend_from_slice(buf);
        drop(guard);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct LazyResponseBody {
    pending: Option<Arc<Mutex<PendingPost>>>,
    reader:  Option<BufReader<LazyBody>>,
}

impl LazyResponseBody {
    fn ready(body: LazyBody) -> Self {
        Self {
            pending: None,
            reader:  Some(BufReader::new(body)),
        }
    }

    const fn pending(state: Arc<Mutex<PendingPost>>) -> Self {
        Self {
            pending: Some(state),
            reader:  None,
        }
    }

    fn reader(&mut self) -> io::Result<&mut BufReader<LazyBody>> {
        if self.reader.is_none() {
            let state = self
                .pending
                .take()
                .context("lazy response has no pending request")
                .map_err(io::Error::other)?;
            let body = take_pending_body(&state)?;
            self.reader = Some(BufReader::new(body));
        }
        self.reader
            .as_mut()
            .context("lazy response was not initialized")
            .map_err(io::Error::other)
    }
}

impl Read for LazyResponseBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader()?.read(buf)
    }
}

impl BufRead for LazyResponseBody {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.reader()?.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        if let Ok(reader) = self.reader() {
            reader.consume(amount);
        }
    }
}

pub(super) struct LazyHeaders {
    pending: Option<Arc<Mutex<PendingPost>>>,
    cursor:  Option<Cursor<Vec<u8>>>,
    failure: Option<HttpFailure>,
}

impl LazyHeaders {
    fn ready(headers: Result<Vec<u8>, HttpFailure>) -> Self {
        match headers {
            Ok(bytes) => {
                Self {
                    pending: None,
                    cursor:  Some(Cursor::new(bytes)),
                    failure: None,
                }
            },
            Err(failure) => {
                Self {
                    pending: None,
                    cursor:  None,
                    failure: Some(failure),
                }
            },
        }
    }

    const fn pending(state: Arc<Mutex<PendingPost>>) -> Self {
        Self {
            pending: Some(state),
            cursor:  None,
            failure: None,
        }
    }

    fn cursor(&mut self) -> io::Result<&mut Cursor<Vec<u8>>> {
        if let Some(failure) = self.failure.take() {
            return Err(failure.into_error());
        }
        if self.cursor.is_none() {
            let state = self
                .pending
                .take()
                .context("lazy headers have no pending request")
                .map_err(io::Error::other)?;
            match pending_headers(&state)? {
                Ok(headers) => self.cursor = Some(Cursor::new(headers)),
                Err(failure) => return Err(failure.into_error()),
            }
        }
        self.cursor
            .as_mut()
            .context("lazy headers were not initialized")
            .map_err(io::Error::other)
    }
}

impl Read for LazyHeaders {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.cursor()?.read(buf)
    }
}

impl BufRead for LazyHeaders {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.cursor()?.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        if let Ok(cursor) = self.cursor() {
            cursor.consume(amount);
        }
    }
}

fn ensure_pending_response(guard: &mut PendingPost) {
    if guard.response.is_none() {
        let response = send_ureq(
            guard.agent,
            Method::Post,
            &guard.url,
            guard.headers.iter(),
            &guard.request_body,
        )
        .map(|response| {
            PendingResponse {
                headers: response.headers,
                body:    Some(response.body),
            }
        })
        .map_err(|err| HttpFailure::from_http_error(&guard.url, &err));
        guard.response = Some(response);
    }
}

fn pending_headers(state: &Arc<Mutex<PendingPost>>) -> io::Result<Result<Vec<u8>, HttpFailure>> {
    let mut guard = state
        .lock()
        .map_err(|_| io::Error::other("poisoned post headers"))?;
    ensure_pending_response(&mut guard);
    guard
        .response
        .as_ref()
        .expect("response was just initialized")
        .as_ref()
        .map(|response| response.headers.clone())
        .map_err(|err| err.clone().into_error())
}

fn take_pending_body(state: &Arc<Mutex<PendingPost>>) -> io::Result<LazyBody> {
    let mut guard = state
        .lock()
        .map_err(|_| io::Error::other("poisoned post response"))?;
    ensure_pending_response(&mut guard);
    guard
        .response
        .as_mut()
        .expect("response was just initialized")
        .as_mut()
        .map_err(|err| err.clone().into_error())?
        .body
        .take()
        .context("post response body was already consumed")
        .map_err(io::Error::other)
}

enum SendOutcome {
    Response(Response<Body>),
    Unauthorized,
    Failed(HttpFailure),
}

fn send_once(
    agent: &'static Agent,
    method: Method,
    url: &str,
    header_lines: &[String],
    body: &[u8],
) -> Result<SendOutcome, HttpError> {
    let sent = match method {
        Method::Get => apply_headers(agent.get(url), header_lines).call(),
        Method::Post => apply_headers(agent.post(url), header_lines).send(body),
    };
    let response = match sent {
        Ok(response) => response,
        Err(UreqError::StatusCode(401)) => return Ok(SendOutcome::Unauthorized),
        Err(UreqError::StatusCode(status)) => {
            return Ok(SendOutcome::Failed(HttpFailure::from_status(
                method, url, status,
            )));
        },
        Err(err) => {
            return Err(HttpError::InitHttpClient {
                source: Box::new(err),
            });
        },
    };
    let status = response.status();
    if status == 401 {
        return Ok(SendOutcome::Unauthorized);
    }
    if !status.is_success() {
        return Ok(SendOutcome::Failed(HttpFailure::from_status(
            method,
            url,
            status.as_u16(),
        )));
    }
    Ok(SendOutcome::Response(response))
}

fn request_host(url: &str) -> Option<String> {
    let parsed = gix::Url::try_from(url).ok()?;
    parsed.host().map(str::to_owned)
}

fn has_authorization(header_lines: &[String]) -> bool {
    header_lines.iter().any(|header| {
        header
            .split_once(':')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
    })
}

fn ok_response(response: Response<Body>) -> UreqResponse {
    let formatted_headers = format_headers(response.headers());
    let (_parts, response_body) = response.into_parts();
    UreqResponse {
        headers: Ok(formatted_headers),
        body:    LazyBody::Ready(Box::new(response_body.into_reader())),
    }
}

fn failed_response(failure: HttpFailure) -> UreqResponse {
    UreqResponse {
        headers: Err(failure.clone()),
        body:    LazyBody::Error(Some(failure)),
    }
}

fn send_ureq(
    agent: &'static Agent,
    method: Method,
    url: &str,
    headers: impl IntoIterator<Item = impl AsRef<str>>,
    body: &[u8],
) -> Result<UreqResponse, HttpError> {
    let mut header_lines = headers
        .into_iter()
        .map(|header| header.as_ref().to_owned())
        .collect::<Vec<String>>();
    let host = request_host(url);

    let mut sent_auth = has_authorization(&header_lines);
    if !sent_auth
        && let Some(known_host) = host.as_deref()
        && let Some(cred) = auth::cached_http_credential(known_host)
    {
        header_lines.push(format!("Authorization: {}", cred.authorization()));
        sent_auth = true;
    }

    match send_once(agent, method, url, &header_lines, body)? {
        SendOutcome::Response(response) => Ok(ok_response(response)),
        SendOutcome::Failed(failure) => Ok(failed_response(failure)),
        SendOutcome::Unauthorized => {
            retry_with_auth(
                agent,
                method,
                url,
                header_lines,
                body,
                host.as_deref(),
                sent_auth,
            )
        },
    }
}

fn retry_with_auth(
    agent: &'static Agent,
    method: Method,
    url: &str,
    mut header_lines: Vec<String>,
    body: &[u8],
    host: Option<&str>,
    sent_auth: bool,
) -> Result<UreqResponse, HttpError> {
    let label = host.unwrap_or("the remote");
    let resolved = host
        .filter(|_| !sent_auth)
        .and_then(auth::resolve_http_credential);
    let Some(cred) = resolved else {
        return Ok(failed_response(HttpFailure::no_usable_credential(label)));
    };
    header_lines.push(format!("Authorization: {}", cred.authorization()));
    match send_once(agent, method, url, &header_lines, body)? {
        SendOutcome::Response(response) => Ok(ok_response(response)),
        SendOutcome::Failed(failure) => Ok(failed_response(failure)),
        SendOutcome::Unauthorized => Ok(failed_response(HttpFailure::no_usable_credential(label))),
    }
}

fn apply_headers<B>(mut request: RequestBuilder<B>, headers: &[String]) -> RequestBuilder<B> {
    for header in headers {
        if let Some((name, value)) = header.split_once(':') {
            request = request.header(name.trim(), value.trim());
        }
    }
    request
}

fn format_headers(headers: &HeaderMap) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in headers {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.to_str().unwrap_or_default().as_bytes());
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::{
        io::{
            ErrorKind,
            Read as _,
            Write as _,
        },
        net::{
            TcpListener,
            TcpStream,
        },
        thread,
        time::Duration,
    };

    use gix_transport::client::blocking_io::http::{
        Http as _,
        PostBodyDataKind,
    };

    use super::{
        Method,
        UreqHttp,
        auth,
        http,
        send_ureq,
    };

    fn read_request(stream: &mut TcpStream) -> Option<String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        let mut buf = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buf).ok()?;
            if read == 0 {
                return (!request.is_empty())
                    .then(|| String::from_utf8_lossy(&request).into_owned());
            }
            request.extend_from_slice(&buf[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let headers_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map_or(request.len(), |idx| idx + 4);
        let headers = String::from_utf8_lossy(&request[..headers_end]).into_owned();
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|&(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len().saturating_sub(headers_end) < content_length {
            let read = stream.read(&mut buf).ok()?;
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buf[..read]);
        }
        Some(String::from_utf8_lossy(&request).into_owned())
    }

    fn write_response(stream: &mut TcpStream, head: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    fn serve_basic_auth_gate(bind_host: &str) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind((bind_host, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/repo.git/info/refs?service=git-upload-pack");
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2_u8 {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let Some(request) = read_request(&mut stream) else {
                    break;
                };
                let authorized = request
                    .lines()
                    .any(|line| line.to_ascii_lowercase().starts_with("authorization:"));
                if authorized {
                    write_response(&mut stream, "200 OK", "refs");
                    requests.push(request);
                    break;
                }
                write_response(
                    &mut stream,
                    "401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"",
                    "",
                );
                requests.push(request);
            }
            requests
        });
        (url, handle)
    }

    #[test]
    fn unauthorized_request_resolves_credential_and_retries_with_authorization() {
        let host = "127.0.0.2";
        auth::seed_resolvable_credential(host, "atagen", "s3cr3t");
        let (url, server) = serve_basic_auth_gate(host);

        let response =
            send_ureq(http::agent(), Method::Get, &url, Vec::<String>::new(), &[]).unwrap();

        assert!(response.headers.is_ok());

        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0]
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
        );
        let retry = requests[1].to_ascii_lowercase();
        assert!(retry.contains("authorization: basic yxrhz2vuonmzy3izda=="));
    }

    fn serve_once(
        status: &str,
        response_body: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let status_line = status.to_owned();
        let url = format!(
            "http://{}/repo.git/git-upload-pack",
            listener.local_addr().unwrap()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0_u8; 1024];
            loop {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let headers_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map_or(request.len(), |idx| idx + 4);
            let headers = String::from_utf8_lossy(&request[..headers_end]);
            let content_length = headers
                .lines()
                .find_map(|line| line.split_once(':'))
                .filter(|&(name, _)| name.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len().saturating_sub(headers_end) < content_length {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }

            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nX-Test: yes\r\nConnection: \
                 close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn status_401_without_credentials_avoids_gix_credential_cascade() {
        let (url, server) = serve_once("401 Unauthorized", "");

        let response =
            send_ureq(http::agent(), Method::Get, &url, Vec::<String>::new(), &[]).unwrap();
        let failure = response.headers.err().unwrap();
        let err = failure.into_error();

        assert_eq!(err.kind(), ErrorKind::Other);
        assert!(err.to_string().contains("no usable credential"));
        assert!(server.join().unwrap().starts_with("GET "));
    }

    #[test]
    fn post_response_is_sent_once_when_headers_are_read_first() {
        let (url, server) = serve_once("200 OK", "ok");
        let mut http = UreqHttp::default();
        let response = http
            .post(
                &url,
                &url,
                Vec::<String>::new(),
                PostBodyDataKind::BoundedAndFitsIntoMemory,
            )
            .unwrap();
        let mut post_body = response.post_body;
        post_body.write_all(b"want abc\n").unwrap();
        drop(post_body);
        let mut headers = response.headers;
        let mut body = response.body;

        let mut header_bytes = Vec::new();
        headers.read_to_end(&mut header_bytes).unwrap();
        let mut response_text = String::new();
        body.read_to_string(&mut response_text).unwrap();
        let request = server.join().unwrap();

        assert!(
            String::from_utf8(header_bytes)
                .unwrap()
                .contains("x-test: yes")
        );
        assert_eq!(response_text, "ok");
        assert_eq!(request.matches("POST ").count(), 1);
        assert!(request.ends_with("want abc\n"));
    }

    #[test]
    fn post_response_is_sent_once_when_body_is_read_first() {
        let (url, server) = serve_once("200 OK", "ok");
        let mut http = UreqHttp::default();
        let response = http
            .post(
                &url,
                &url,
                Vec::<String>::new(),
                PostBodyDataKind::BoundedAndFitsIntoMemory,
            )
            .unwrap();
        let mut post_body = response.post_body;
        post_body.write_all(b"want abc\n").unwrap();
        drop(post_body);
        let mut headers = response.headers;
        let mut body = response.body;

        let mut response_text = String::new();
        body.read_to_string(&mut response_text).unwrap();
        let mut header_bytes = Vec::new();
        headers.read_to_end(&mut header_bytes).unwrap();
        let request = server.join().unwrap();

        assert_eq!(response_text, "ok");
        assert!(
            String::from_utf8(header_bytes)
                .unwrap()
                .contains("x-test: yes")
        );
        assert_eq!(request.matches("POST ").count(), 1);
        assert!(request.ends_with("want abc\n"));
    }
}
