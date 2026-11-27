//! A fake engine for the tests of the verbs that talk to one (`bench`,
//! `eval`): a home pointed at it, and a server that answers a list of
//! replies in order.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

/// A home whose golem.toml names `port`, so `Base::from_home` points the
/// run at a fake engine rather than the real one. `tag` names the temp
/// folder, and must be unique across the test binary.
pub fn home(tag: &str, port: u16) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("golem-rs-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp");
    std::fs::write(dir.join("golem.toml"), format!("LOCAL_AI_PORT=\"{port}\"\n")).expect("write");
    dir
}

/// A fake engine that answers `replies` in order, one connection each: a
/// body under `200 OK`, or a reply that is already a whole response.
pub fn serve(replies: Vec<String>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else { return };
            read_request(&mut stream);
            let head = if reply.starts_with("HTTP/1.1 ") {
                String::new()
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", reply.len())
            };
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

/// Read one whole request: the head, then `Content-Length` bytes of body.
/// A bench's prefill body is ~80 KB and an eval's haystack ~1 MB at 200K
/// tokens, and a server that answered before reading it would reset the
/// connection in the middle of the client's write.
fn read_request(stream: &mut TcpStream) {
    let mut data: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while data.len() < head_end + length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
}
