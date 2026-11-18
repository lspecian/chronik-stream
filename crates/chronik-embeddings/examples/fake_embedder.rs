//! A deterministic embedding endpoint for end-to-end tests, speaking the
//! `external` provider's protocol (`POST {"texts": [...]}` →
//! `{"embeddings": [[f32; dims], ...]}`).
//!
//! While the file named by `--fail-file` exists it answers 503, so a test can
//! make the provider fail and recover without restarting anything.
//!
//! Usage: fake_embedder <port> <dims> [--fail-file <path>]
//!
//! Each vector is derived from the text's bytes (a seeded hash per dimension,
//! normalized), so the same text always embeds the same.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).expect("usage: fake_embedder <port> <dims> [--fail-file <path>]");
    let dims: usize = args.get(2).and_then(|d| d.parse().ok()).expect("dims");
    let fail_file = args.iter().position(|a| a == "--fail-file").and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
    eprintln!("fake_embedder listening on 127.0.0.1:{port}, dims={dims}");
    for stream in listener.incoming().flatten() {
        let fail_file = fail_file.clone();
        std::thread::spawn(move || {
            let _ = serve(stream, dims, fail_file.as_deref());
        });
    }
}

fn serve(stream: TcpStream, dims: usize, fail_file: Option<&std::path::Path>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let (status, payload) = if fail_file.is_some_and(|f| f.exists()) {
            ("503 Service Unavailable", r#"{"error":"embedder down (fail file present)"}"#.to_string())
        } else {
            match serde_json::from_slice::<serde_json::Value>(&body) {
                Ok(req) => {
                    let texts: Vec<String> = req["texts"]
                        .as_array()
                        .map(|a| a.iter().map(|t| t.as_str().unwrap_or("").to_string()).collect())
                        .unwrap_or_default();
                    let embeddings: Vec<Vec<f32>> = texts.iter().map(|t| embed(t, dims)).collect();
                    ("200 OK", serde_json::json!({ "embeddings": embeddings }).to_string())
                }
                Err(e) => ("400 Bad Request", serde_json::json!({ "error": e.to_string() }).to_string()),
            }
        };
        let mut out = stream.try_clone()?;
        write!(
            out,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
            payload.len()
        )?;
        out.flush()?;
    }
}

fn embed(text: &str, dims: usize) -> Vec<f32> {
    let mut v: Vec<f32> = (0..dims)
        .map(|d| {
            // FNV-1a over the text, seeded by the dimension.
            let mut h: u64 = 0xcbf29ce484222325 ^ (d as u64).wrapping_mul(0x9e3779b97f4a7c15);
            for b in text.bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            (h % 2001) as f32 / 1000.0 - 1.0
        })
        .collect();
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}
