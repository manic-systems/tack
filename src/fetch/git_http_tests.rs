// SPDX-License-Identifier: EUPL-1.2

use std::{
    io::{
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

use super::{
    Method,
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
            return (!request.is_empty()).then(|| String::from_utf8_lossy(&request).into_owned());
        }
        request.extend_from_slice(&buf[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&request).into_owned());
        }
    }
}

fn write_response(stream: &mut TcpStream, head: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).unwrap();
}

#[test]
fn unauthorized_request_resolves_credential_and_retries_with_authorization() {
    let host = "127.0.0.2";
    auth::seed_resolvable_credential(host, "atagen", "s3cr3t");
    let listener = TcpListener::bind((host, 0)).unwrap();
    let url = format!(
        "http://{}/repo.git/info/refs?service=git-upload-pack",
        listener.local_addr().unwrap()
    );
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..2_u8 {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream).unwrap();
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

    let response = send_ureq(http::agent(), Method::Get, &url, Vec::<String>::new(), &[]).unwrap();

    assert!(response.headers.is_ok());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[0]
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("authorization:"))
    );
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("authorization: basic yxrhz2vuonmzy3izda==")
    );
}
