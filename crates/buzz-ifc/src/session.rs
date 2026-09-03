use crate::declassification::{GrantReplayStore, VerifiedDeclassificationGrant};
use crate::domain::{DomainAuthority, ExecutionDomain, PublicationTarget, ResourceLabel};
use crate::hash::hash_field;
use crate::policy::{
    CommittedPublication, ProcessState, PublicationCommit, PublicationCommitError, RuleDecision,
};
use sha2::{Digest, Sha256};

/// The broker-facing IFC state for one actual worker process.
///
/// A session owns the worker's execution domain and monotonically accumulates
/// every admitted input label. The broker should create one session per worker,
/// call [`IfcSession::read`] before delivering an input, and require
/// [`IfcSession::publish`] to succeed before invoking an egressing sink.
/// These are the broker enforcement points described in [Appendix D of the
/// design paper](../../../docs/practical-information-flow-for-buzz-agents.md#appendix-d-broker-interface-and-enforcement).
///
/// ```
/// # use buzz_ifc::{AuthorizedPublication, ExecutionDomain,
/// #     GrantReplayStore, IfcError, IfcSession, PublicationRequest,
/// #     PublicationTarget, ResourceLabel, VerifiedDeclassificationGrant};
/// # fn consume(_: AuthorizedPublication<'_>) {}
/// # fn broker_flow<R: GrantReplayStore>(
/// #     domain: ExecutionDomain,
/// #     resource: &ResourceLabel,
/// #     destination: &PublicationTarget,
/// #     content: &[u8],
/// #     grant: Option<&VerifiedDeclassificationGrant>,
/// #     replay_store: &mut R,
/// # ) -> Result<(), IfcError> {
/// let mut session = IfcSession::enter(domain);
/// session.read(resource)?;
/// session.call("buzz.read.current")?;
/// let authorization = session.publish(
///     PublicationRequest::new("buzz.post", destination, content, 1_000),
///     grant,
///     replay_store,
/// )?;
/// consume(authorization);
/// # Ok(())
/// # }
/// ```
///
/// Low-level policy machinery is deliberately not part of the crate API:
///
/// ```compile_fail
/// use buzz_ifc::{ProcessState, RuleEvaluator};
/// ```
#[derive(Debug)]
pub struct IfcSession {
    worker_domain: ExecutionDomain,
    authority: DomainAuthority,
    process: ProcessState,
}

impl IfcSession {
    /// Bind a fresh worker process to one verified execution domain.
    pub fn enter(domain: ExecutionDomain) -> Self {
        let worker_domain = domain.clone();
        let authority = DomainAuthority::new(domain);
        let process = ProcessState::for_domain(authority.current());
        Self {
            worker_domain,
            authority,
            process,
        }
    }

    /// Apply the latest verified domain for this worker.
    ///
    /// A membership, audience, context, epoch, or capability change
    /// permanently makes this session unusable. The broker must discard the
    /// worker rather than refreshing it back to an older domain.
    pub fn refresh(&mut self, current: ExecutionDomain) -> Result<(), IfcError> {
        self.authority.replace(current);
        enforce(self.process.enter(self.authority.current()))
    }

    /// Return the immutable domain to which this worker was initially bound.
    pub fn domain(&self) -> &ExecutionDomain {
        &self.worker_domain
    }

    /// Return a current publication target for this worker's own context.
    pub fn current_target(&self) -> PublicationTarget {
        PublicationTarget::from_current(self.authority.current())
    }

    /// Admit one labeled input and add its restrictions to this process.
    ///
    /// On success the taint is recorded before control returns, so the broker
    /// cannot authorize a read and forget the corresponding observation. On
    /// failure the broker must not deliver the input to the worker.
    pub fn read(&mut self, resource: &ResourceLabel) -> Result<(), IfcError> {
        enforce(self.process.authorize_read(
            &self.worker_domain,
            self.authority.current(),
            resource,
        ))?;
        self.process.observe(resource);
        Ok(())
    }

    /// Conservatively record input whose provenance could not be established.
    ///
    /// This permanently prevents the session from publishing.
    pub fn mark_unknown_input(&mut self) {
        self.process.mark_unknown();
    }

    #[cfg(test)]
    pub(crate) fn has_observed_input(&self) -> bool {
        self.process.has_observed_input()
    }

    /// Authorize an operation classified by trusted policy as non-egressing.
    pub fn call(&self, operation: &str) -> Result<(), IfcError> {
        enforce(
            self.process
                .authorize_non_egressing_call(self.authority.current(), operation),
        )
    }

    /// Authorize one exact publication against current domain and process state.
    ///
    /// Authorization and commit happen in this call. A matching
    /// declassification grant is consumed atomically before the returned token
    /// can be passed to the broker's publication sink.
    pub fn publish<'a, R: GrantReplayStore>(
        &'a self,
        request: PublicationRequest<'a>,
        grant: Option<&VerifiedDeclassificationGrant>,
        replay_store: &mut R,
    ) -> Result<AuthorizedPublication<'a>, IfcError> {
        let current = self.authority.current();
        let content_digest = publication_digest(request.content);
        let permit = self
            .process
            .authorize_publication(
                current,
                request.operation,
                request.destination,
                &content_digest,
                grant,
            )
            .map_err(IfcError::from_decision)?;
        let commit = PublicationCommit::new(
            current,
            request.operation,
            request.destination,
            content_digest,
            request.now,
        );
        let committed = self
            .process
            .commit_publication(permit, commit, replay_store)
            .map_err(IfcError::from_commit)?;
        Ok(AuthorizedPublication {
            committed,
            content: request.content,
        })
    }
}

/// The exact output presented for publication.
#[derive(Clone, Copy, Debug)]
pub struct PublicationRequest<'a> {
    operation: &'a str,
    destination: &'a PublicationTarget,
    content: &'a [u8],
    now: u64,
}

impl<'a> PublicationRequest<'a> {
    /// Bind the operation, destination, content, and current time checked by
    /// [`IfcSession::publish`].
    pub fn new(
        operation: &'a str,
        destination: &'a PublicationTarget,
        content: &'a [u8],
        now: u64,
    ) -> Self {
        Self {
            operation,
            destination,
            content,
            now,
        }
    }
}

/// A final publication token passed to the broker's egress sink.
///
/// The token exposes the same content bytes whose digest was authorized, so
/// the sink does not need to reconnect a detached digest to a payload.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the authorization must be consumed by the publication sink"]
pub struct AuthorizedPublication<'a> {
    committed: CommittedPublication<'a>,
    content: &'a [u8],
}

impl AuthorizedPublication<'_> {
    /// Return the freshly revalidated source domain identifier.
    pub fn source_domain_id(&self) -> &str {
        self.committed.source_domain_id()
    }

    /// Return the authorized operation.
    pub fn operation(&self) -> &str {
        self.committed.operation()
    }

    /// Return the freshly revalidated destination.
    pub fn destination(&self) -> &PublicationTarget {
        self.committed.destination()
    }

    /// Return the exact content bytes authorized for publication.
    pub fn content(&self) -> &[u8] {
        self.content
    }

    /// Return the canonical digest covered by any declassification grant.
    pub fn content_digest(&self) -> &[u8; 32] {
        self.committed.content_digest()
    }
}

/// Compute the canonical digest used to bind publication content to a grant.
pub fn publication_digest(content: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"buzz-ifc-publication-content-v1");
    hash_field(&mut hasher, content);
    hasher.finalize().into()
}

/// A broker-facing IFC operation was denied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum IfcError {
    /// A policy check rejected the operation.
    #[error("IFC policy denied the operation: {0}")]
    Denied(&'static str),
    /// State changed between authorization and commit.
    #[error("IFC publication authorization was invalidated: {0}")]
    PublicationInvalidated(&'static str),
    /// A signed declassification grant expired before commit.
    #[error("declassification grant expired before publication")]
    GrantExpired,
    /// Durable replay protection reports that the grant was already consumed.
    #[error("declassification grant was already consumed")]
    GrantReplayed,
}

impl IfcError {
    fn from_decision(decision: RuleDecision) -> Self {
        Self::Denied(decision.reason())
    }

    fn from_commit(error: PublicationCommitError) -> Self {
        match error {
            PublicationCommitError::SourceChanged => {
                Self::PublicationInvalidated("source domain changed")
            }
            PublicationCommitError::ProcessStateChanged => {
                Self::PublicationInvalidated("process state changed")
            }
            PublicationCommitError::OperationChanged => {
                Self::PublicationInvalidated("operation changed")
            }
            PublicationCommitError::DestinationChanged => {
                Self::PublicationInvalidated("destination changed")
            }
            PublicationCommitError::ContentChanged => {
                Self::PublicationInvalidated("content changed")
            }
            PublicationCommitError::GrantExpired => Self::GrantExpired,
            PublicationCommitError::GrantReplayed => Self::GrantReplayed,
        }
    }
}

fn enforce(decision: RuleDecision) -> Result<(), IfcError> {
    if decision.allowed() {
        Ok(())
    } else {
        Err(IfcError::from_decision(decision))
    }
}
