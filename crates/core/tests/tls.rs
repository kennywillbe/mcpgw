//! The guard that keeps https upstreams working.
//!
//! Every other network-touching test in this repo dials `http://127.0.0.1`,
//! which is how a client compiled with no TLS backend at all shipped once and
//! was found only by dogfooding: rmcp declares reqwest with
//! `default-features = false`, so the workspace has to name a TLS feature
//! itself. Deleting that name from the workspace manifest still compiled and
//! still passed the whole suite, so prose was the only thing protecting it.
//!
//! This file is the missing failure. `reqwest` is a dev-dependency of this
//! crate declared with no features of its own, so cargo's feature unification
//! gives it exactly the feature set the shipped `rmcp` dependency asks for —
//! and the assertion below is on an item that only exists when a rustls
//! backend is compiled in. Drop `"reqwest"` from the workspace `rmcp` entry
//! and this stops compiling, which is the cheapest honest way to say it.

/// A compile-time assertion, spelled as a test so it is obvious what it is
/// for. `ClientBuilder::tls_backend_rustls` is `#[cfg(feature = "__rustls")]`
/// in reqwest, so naming it at all is the check; calling it proves nothing
/// extra and would need a runtime client.
#[test]
fn the_http_client_is_built_with_a_tls_backend() {
    let _: fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder =
        reqwest::ClientBuilder::tls_backend_rustls;
}

/// The compile-time check above says a backend is linked; this one says an
/// `https` URL actually reaches it. Without TLS, reqwest rejects the scheme
/// before it ever opens a socket, so the failure is a builder/request error
/// rather than the connection error a real dial to a closed port gives.
///
/// Port 1 on loopback is closed on every platform CI runs, so there is no
/// server to start and no network to reach — the assertion is only about how
/// far the request got.
#[tokio::test]
async fn an_https_url_gets_as_far_as_the_connection() {
    let error = reqwest::Client::new()
        .get("https://127.0.0.1:1/")
        .send()
        .await
        .expect_err("nothing is listening on loopback port 1");
    assert!(
        error.is_connect(),
        "an https request should fail at the connection, not before it: {error:?}"
    );
}
