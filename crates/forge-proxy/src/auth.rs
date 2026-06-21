use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use futures::future::BoxFuture;
use tower::{Layer, Service};

const UNAUTHORIZED_BODY: &str =
    r#"{"jsonrpc":"2.0","error":{"code":-32001,"message":"Unauthorized"},"id":null}"#;

fn unauthorized_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(UNAUTHORIZED_BODY))
        .expect("valid unauthorized response")
}

#[derive(Clone)]
pub struct AuthLayer {
    token: Option<String>,
}

impl AuthLayer {
    pub fn new(token: Option<String>) -> Self {
        Self { token }
    }
}

impl<S> Layer<S> for AuthLayer {
    type Service = AuthMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AuthMiddleware {
            inner,
            token: self.token.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AuthMiddleware<S> {
    inner: S,
    token: Option<String>,
}

impl<S> Service<Request<Body>> for AuthMiddleware<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // Only exact well-known discovery paths are exempt from auth (RFC 7235).
        let path = req.uri().path();
        if path == "/.well-known/mcp" || path == "/.well-known/mcp-servers.json" {
            let fut = self.inner.call(req);
            return Box::pin(fut);
        }

        let required_token = match &self.token {
            None => {
                // Auth disabled — pass through.
                let fut = self.inner.call(req);
                return Box::pin(fut);
            }
            Some(t) => t.clone(),
        };

        // RFC 7235 §1.2: auth scheme names are case-insensitive ("bearer" == "Bearer").
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                let lower = s.to_ascii_lowercase();
                if lower.starts_with("bearer ") {
                    Some(s[7..].to_owned())
                } else {
                    None
                }
            });

        let provided = match provided {
            Some(p) => p,
            None => {
                return Box::pin(async move { Ok(unauthorized_response()) });
            }
        };

        // Constant-time comparison to mitigate timing attacks.
        let token_bytes = required_token.as_bytes();
        let provided_bytes = provided.as_bytes();
        let matches = token_bytes.len() == provided_bytes.len()
            && token_bytes
                .iter()
                .zip(provided_bytes.iter())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;

        if !matches {
            return Box::pin(async move { Ok(unauthorized_response()) });
        }

        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}
