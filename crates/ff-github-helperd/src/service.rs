use crate::{
    authority::{AuthorityError, AuthorityStore},
    protocol::{self, DenialCode, Request, Response},
    socket::PeerIdentity,
};

pub trait ReadPrTransport: Send + Sync {
    fn available(&self) -> bool;
}

pub struct UnavailableTransport;
impl ReadPrTransport for UnavailableTransport {
    fn available(&self) -> bool {
        false
    }
}

pub struct Service<T> {
    store: AuthorityStore,
    transport: T,
}

impl<T: ReadPrTransport> Service<T> {
    pub fn new(store: AuthorityStore, transport: T) -> Self {
        Self { store, transport }
    }

    pub async fn handle(&self, peer: &PeerIdentity, request: Request) -> Response {
        if !protocol::validate(&request) {
            return denied(DenialCode::InvalidRequest);
        }
        match request {
            Request::Ready { .. } => Response::Ready,
            Request::Issue {
                work_item_id,
                repo_id,
                operation,
                request_digest,
                ..
            } => match self
                .store
                .issue(work_item_id, repo_id, &operation, &request_digest, peer)
                .await
            {
                Ok(issued) => Response::Issued {
                    nonce: issued.nonce,
                    expires_at: issued.expires_at.to_rfc3339(),
                },
                Err(error) => denied(map_error(error)),
            },
            Request::Execute {
                nonce,
                work_item_id,
                repo_id,
                operation,
                request_digest,
                ..
            } => {
                // SEC-D2A deliberately has no production transport. Persist the
                // deterministic result atomically with nonce consumption.
                let _transport_available = self.transport.available();
                match self
                    .store
                    .consume_unavailable(
                        &nonce,
                        work_item_id,
                        repo_id,
                        &operation,
                        &request_digest,
                        peer,
                    )
                    .await
                {
                    Ok(()) => denied(DenialCode::BackendUnavailable),
                    Err(error) => denied(map_error(error)),
                }
            }
        }
    }
}

fn denied(code: DenialCode) -> Response {
    Response::Denied { code }
}

fn map_error(error: AuthorityError) -> DenialCode {
    match error {
        AuthorityError::Mismatch => DenialCode::AuthorityMismatch,
        AuthorityError::Replay => DenialCode::Replay,
        AuthorityError::Expired => DenialCode::Expired,
        AuthorityError::Database => DenialCode::BackendUnavailable,
    }
}
