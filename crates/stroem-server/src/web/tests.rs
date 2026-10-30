use super::span_uri;
use axum::http::Uri;

fn span(uri: &str) -> String {
    span_uri(&uri.parse::<Uri>().unwrap())
}

#[test]
fn span_uri_hides_the_upload_token() {
    assert_eq!(span("/api/uploads/0a1b2c3d4e5f"), "/api/uploads/{token}");
    assert_eq!(
        span("/api/uploads/0a1b2c3d4e5f?x=1"),
        "/api/uploads/{token}?x=1"
    );
    assert_eq!(
        span("/api/uploads/0a1b2c3d4e5f/more"),
        "/api/uploads/{token}/more"
    );
    assert_eq!(span("/api/uploads/"), "/api/uploads/{token}");
    assert_eq!(
        span("http://stroem.test/api/uploads/0a1b2c3d4e5f?x=1"),
        "http://stroem.test/api/uploads/{token}?x=1"
    );
}

#[test]
fn span_uri_keeps_every_other_uri() {
    for uri in [
        "/api/uploads",
        "/api/uploadsx/0a1b2c3d4e5f",
        "/api/jobs/1?limit=5",
        "/worker/jobs/claim",
        "/x/api/uploads/0a1b2c3d4e5f",
        "/",
    ] {
        assert_eq!(span(uri), uri);
    }
}
