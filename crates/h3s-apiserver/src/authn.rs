//! Peer identity. A trusted client certificate is the only credential today;
//! bearer tokens and impersonation are rejected rather than silently ignored.
use crate::{transport::Peer, Failure, Result};
use axum::{body::Body, http::Request};
use h3s_auth::User;

pub(crate) fn forbid_impersonation(request: &Request<Body>) -> Result<()> {
    if request
        .headers()
        .keys()
        .any(|h| h.as_str().starts_with("impersonate-"))
    {
        return Err(Failure::new(
            403,
            "Forbidden",
            "impersonation is not enabled",
        ));
    }
    Ok(())
}

pub(crate) fn authenticate(peer: Peer, request: &Request<Body>) -> Result<User> {
    let user = peer.0.ok_or_else(|| {
        Failure::new(
            401,
            "Unauthorized",
            "a trusted client certificate is required",
        )
    })?;
    // Invalid bearer credentials must not silently fall back to another identity.
    if request.headers().contains_key("authorization") {
        return Err(Failure::new(
            401,
            "Unauthorized",
            "bearer authentication is not yet implemented",
        ));
    }
    Ok(user)
}
