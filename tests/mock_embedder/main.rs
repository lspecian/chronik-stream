// Tiny mock embedding server for HP-2.9 benchmarks.
// Responds to POST /embed with `{"embeddings":[[...], ...]}`.
// Each returned vector is deterministic per input text (sha-like hash of bytes
// projected to `dims`) so the hot vector index can actually dedup meaningfully.
// No external API calls. Zero cost. ~sub-ms latency.
//
// Run: PORT=8099 DIMS=64 ./target/release/mock-embedder

use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::net::SocketAddr;

#[derive(Deserialize)]
struct EmbedRequest {
    texts: Vec<String>,
}

#[derive(Serialize)]
struct EmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

fn hash_embed(text: &str, dims: usize) -> Vec<f32> {
    // Simple FNV-1a mixed with per-dimension salt so the vectors aren't
    // uniform across all inputs. Not crypto — just deterministic noise.
    let bytes = text.as_bytes();
    let mut out = vec![0.0_f32; dims];
    for (i, out_i) in out.iter_mut().enumerate().take(dims) {
        let mut h: u64 = 0xcbf29ce484222325_u64.wrapping_add(i as u64 * 0x100000001b3);
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        // Map to [-1, 1]
        *out_i = ((h as i64).wrapping_rem(2_000_001)) as f32 / 1_000_000.0;
    }
    // Normalize so cosine distance behaves
    let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut out {
            *x /= norm;
        }
    }
    out
}

async fn handle(dims: usize, req: Request<Body>) -> Result<Response<Body>, Infallible> {
    if req.method() != Method::POST || req.uri().path() != "/embed" {
        return Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap());
    }
    let full = match hyper::body::to_bytes(req.into_body()).await {
        Ok(b) => b,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::empty())
                .unwrap());
        }
    };
    let body: EmbedRequest = match serde_json::from_slice(&full) {
        Ok(b) => b,
        Err(e) => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Body::from(format!("bad json: {}", e)))
                .unwrap());
        }
    };
    let vecs: Vec<Vec<f32>> = body.texts.iter().map(|t| hash_embed(t, dims)).collect();
    let resp = EmbedResponse { embeddings: vecs };
    let json = serde_json::to_vec(&resp).unwrap();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(json))
        .unwrap())
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8099);
    let dims: usize = std::env::var("DIMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();

    let make_svc = make_service_fn(move |_| async move {
        Ok::<_, Infallible>(service_fn(move |req| handle(dims, req)))
    });

    eprintln!(
        "mock-embedder listening on http://{} dims={}",
        addr, dims
    );
    if let Err(e) = Server::bind(&addr).serve(make_svc).await {
        eprintln!("server error: {}", e);
    }
}
