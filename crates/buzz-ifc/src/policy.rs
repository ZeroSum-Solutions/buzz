use std::collections::BTreeSet;
use std::marker::PhantomData;

use ifc_core::{EgressError, FlowSnapshot, FlowState};
use serde::Serialize;

use crate::declassification::{GrantReplayStore, VerifiedDeclassificationGrant};
use crate::domain::{
    CurrentDomain, ExecutionDomain, OperationEffect, PublicationScope, PublicationTarget,
    ResourceContext, ResourceLabel,
};
use crate::label::{Principal, RealmId};

/// The result of evaluating one IFC rule.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[must_use = "an IFC decision must be enforced"]
pub(crate) struct RuleDecision {
    allowed: bool,
    reason: &'static str,
}

impl RuleDecision {
    fn allow(reason: &'static str) -> Self {
        Self {
            allowed: true,
            reason,
        }
    }

    fn deny(reason: &'static str) -> Self {
        Self {
            allowed: false,
            reason,
        }
    }

    /// Whether the operation is admitted by policy.
    pub(crate) fn allowed(&self) -> bool {
        self.allowed
    }

    /// Stable explanation intended for logs and operator diagnostics.
    pub(crate) fn reason(&self) -> &'static str {
        self.reason
    }
}

/// Pure IFC rule evaluation.
pub(crate) struct RuleEvaluator;

impl RuleEvaluator {
    /// Evaluate `read(D, x) ⇔ A(D) ⊆ R(x) ∧ ContextPolicy(D, x)`.
    ///
    /// This is the read rule specified in [Appendix D of the design
    /// paper](../../../docs/practical-information-flow-for-buzz-agents.md#appendix-d-broker-interface-and-enforcement).
    pub(crate) fn read(
        bound_domain: &ExecutionDomain,
        current: CurrentDomain<'_>,
        resource: &ResourceLabel,
    ) -> RuleDecision {
        let domain = current.domain();
        if bound_domain != domain {
            return RuleDecision::deny("execution domain is no longer current");
        }
        if !resource.is_current_for(domain) {
            return RuleDecision::deny("resource was labeled under a different membership epoch");
        }
        if !resource.confidentiality.can_flow_to(&domain.audience) {
            return RuleDecision::deny("destination audience includes an unauthorized reader");
        }
        if !domain.context.permits(&resource.context) {
            return RuleDecision::deny("resource belongs to a different context");
        }
        RuleDecision::allow("audience, context, and epoch permit the read")
    }

    /// Require a worker's bound domain to match the authority's current value.
    pub(crate) fn reuse(existing: &ExecutionDomain, current: CurrentDomain<'_>) -> RuleDecision {
        if existing == current.domain() {
            RuleDecision::allow("complete execution domain matches")
        } else {
            RuleDecision::deny("agent process has already entered a different domain")
        }
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
struct ConfinementState {
    flow: FlowState<RealmId, Principal>,
    contexts: BTreeSet<ResourceContext>,
    epochs: BTreeSet<crate::domain::MembershipEpoch>,
}

impl ConfinementState {
    fn observe(&mut self, resource: &ResourceLabel) {
        self.contexts.insert(resource.context.clone());
        self.epochs.extend(resource.epoch.iter().cloned());
        self.flow.observe(&resource.confidentiality);
    }

    fn snapshot(&self) -> ConfinementSnapshot {
        ConfinementSnapshot {
            flow: self.flow.snapshot(),
            contexts: self.contexts.clone(),
            epochs: self.epochs.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConfinementSnapshot {
    flow: FlowSnapshot<RealmId, Principal>,
    contexts: BTreeSet<ResourceContext>,
    epochs: BTreeSet<crate::domain::MembershipEpoch>,
}

/// Conservative policy state for one actual agent process.
///
/// This type is deliberately not `Clone`: there must be exactly one monotonic
/// taint record for the corresponding worker process. Session invalidation
/// does not reset information the surrounding process has observed.
/// This preserves the confinement invariant described in [Appendix G of the
/// design paper](../../../docs/practical-information-flow-for-buzz-agents.md#confinement-invariant).
///
/// ```compile_fail
/// fn fork_taint(state: buzz_ifc::ProcessState) {
///     let _stale_copy = state.clone();
/// }
/// ```
#[derive(Debug, Default)]
pub(crate) struct ProcessState {
    entered_domains: Vec<ExecutionDomain>,
    confinement: ConfinementState,
}

impl ProcessState {
    pub(crate) fn for_domain(current: CurrentDomain<'_>) -> Self {
        Self {
            entered_domains: vec![current.domain().clone()],
            confinement: ConfinementState::default(),
        }
    }

    /// Record entry into the authority's current domain and decide whether this
    /// process is reusable.
    pub(crate) fn enter(&mut self, current: CurrentDomain<'_>) -> RuleDecision {
        let requested = current.domain();
        let decision = match self.entered_domains.as_slice() {
            [] => RuleDecision::allow("fresh process has no prior execution domain"),
            [existing] => RuleEvaluator::reuse(existing, current),
            _ => RuleDecision::deny("agent process has entered multiple execution domains"),
        };
        if !self
            .entered_domains
            .iter()
            .any(|domain| domain == requested)
        {
            self.entered_domains.push(requested.clone());
        }
        decision
    }

    /// Record an input that actually entered the process.
    pub(crate) fn observe(&mut self, resource: &ResourceLabel) {
        self.confinement.observe(resource);
    }

    /// Record input whose provenance could not be established.
    pub(crate) fn mark_unknown(&mut self) {
        self.confinement.flow.mark_unknown();
    }

    #[cfg(test)]
    pub(crate) fn has_observed_input(&self) -> bool {
        self.confinement.flow.has_observed_input()
    }

    /// Authorize a read only while this process remains confined to its one
    /// original execution domain.
    pub(crate) fn authorize_read(
        &self,
        bound_domain: &ExecutionDomain,
        current: CurrentDomain<'_>,
        resource: &ResourceLabel,
    ) -> RuleDecision {
        if let Err(decision) = self.active_domain(current) {
            return decision;
        }
        RuleEvaluator::read(bound_domain, current, resource)
    }

    /// Authorize an operation that trusted policy classifies as non-egressing.
    pub(crate) fn authorize_non_egressing_call(
        &self,
        current: CurrentDomain<'_>,
        operation: &str,
    ) -> RuleDecision {
        let source_domain = match self.active_domain(current) {
            Ok(domain) => domain,
            Err(decision) => return decision,
        };
        match source_domain.capabilities.effect(operation) {
            Some(OperationEffect::NonEgressing) => {
                RuleDecision::allow("non-egressing operation is in the effective capability set")
            }
            Some(OperationEffect::Publication(_)) => {
                RuleDecision::deny("publication operation requires an information-flow permit")
            }
            None => RuleDecision::deny("operation is absent from the effective capability set"),
        }
    }

    /// Authorize a publication and return a permit bound to every checked input.
    ///
    /// A permit is not sufficient to publish until
    /// [`ProcessState::commit_publication`] rechecks process taint plus source
    /// and destination freshness immediately before execution.
    pub(crate) fn authorize_publication(
        &self,
        current: CurrentDomain<'_>,
        operation: &str,
        destination: &PublicationTarget,
        content_digest: &[u8; 32],
        grant: Option<&VerifiedDeclassificationGrant>,
    ) -> Result<PublicationPermit, RuleDecision> {
        let source_domain = self.active_domain(current)?;
        let scope = match source_domain.capabilities.effect(operation) {
            Some(OperationEffect::Publication(scope)) => scope,
            Some(OperationEffect::NonEgressing) => {
                return Err(RuleDecision::deny(
                    "operation is not classified for publication",
                ));
            }
            None => {
                return Err(RuleDecision::deny(
                    "operation is absent from the effective capability set",
                ));
            }
        };
        if source_domain.context.realm() != destination.audience().universe() {
            return Err(RuleDecision::deny("publication cannot cross Buzz realms"));
        }
        if scope == PublicationScope::SameContext && &source_domain.context != destination.context()
        {
            return Err(RuleDecision::deny(
                "operation may publish only to its source context",
            ));
        }
        if self.confinement.flow.has_unresolved_input() {
            return Err(RuleDecision::deny(
                "process state contains unresolved input provenance",
            ));
        }
        if self
            .confinement
            .contexts
            .iter()
            .any(|context| !source_domain.context.permits(context))
        {
            return Err(RuleDecision::deny(
                "process state contains input from another context",
            ));
        }
        if self
            .confinement
            .epochs
            .iter()
            .any(|epoch| epoch != &source_domain.epoch)
        {
            return Err(RuleDecision::deny(
                "process state contains input from another membership epoch",
            ));
        }

        let grant_use = match grant {
            Some(grant) => {
                if source_domain.owner() != Some(grant.payload().approver()) {
                    return Err(RuleDecision::deny(
                        "declassification grant was not approved by the agent owner",
                    ));
                }
                if !grant.matches(operation, &source_domain.id(), destination, content_digest) {
                    return Err(RuleDecision::deny(
                        "declassification grant does not match this publication",
                    ));
                }
                Some(GrantUse {
                    digest: grant.payload().signing_digest(),
                    expires_at: grant.payload().expires_at(),
                })
            }
            None => None,
        };

        if grant_use.is_none() {
            if !source_domain.audience.can_flow_to(destination.audience()) {
                return Err(RuleDecision::deny(
                    "destination is broader than the execution domain",
                ));
            }
            if !self.confinement.flow.has_observed_input() {
                return Err(RuleDecision::deny(
                    "output would widen the accumulated reader set",
                ));
            }
            match self.confinement.flow.check_egress(destination.audience()) {
                Ok(()) => {}
                Err(EgressError::UnresolvedInput) => {
                    return Err(RuleDecision::deny(
                        "process state contains unresolved input provenance",
                    ));
                }
                Err(EgressError::DestinationWidensReaders) => {
                    return Err(RuleDecision::deny(
                        "output would widen the accumulated reader set",
                    ));
                }
            }
        }

        Ok(PublicationPermit {
            source_domain_id: source_domain.id(),
            operation: operation.to_owned(),
            destination: destination.clone(),
            content_digest: *content_digest,
            grant: grant_use,
            confinement: self.confinement.snapshot(),
        })
    }

    /// Commit a permit against the process's current taint and broker state.
    ///
    /// Any intervening change to the process's IFC state invalidates the
    /// permit.
    pub(crate) fn commit_publication<'a, R: GrantReplayStore>(
        &'a self,
        permit: PublicationPermit,
        commit: PublicationCommit<'a>,
        replay_store: &mut R,
    ) -> Result<CommittedPublication<'a>, PublicationCommitError> {
        if self.active_domain(commit.current).is_err()
            || permit.source_domain_id != commit.current.domain().id()
        {
            return Err(PublicationCommitError::SourceChanged);
        }
        if permit.confinement != self.confinement.snapshot() {
            return Err(PublicationCommitError::ProcessStateChanged);
        }
        permit.commit(commit, replay_store)
    }

    fn active_domain(&self, current: CurrentDomain<'_>) -> Result<&ExecutionDomain, RuleDecision> {
        let [source_domain] = self.entered_domains.as_slice() else {
            return Err(RuleDecision::deny(
                "process state is not confined to one execution domain",
            ));
        };
        if source_domain != current.domain() {
            return Err(RuleDecision::deny("execution domain is no longer current"));
        }
        Ok(source_domain)
    }
}

#[derive(Debug)]
struct GrantUse {
    digest: [u8; 32],
    expires_at: u64,
}

/// Broker state and exact output presented for final publication commit.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PublicationCommit<'a> {
    current: CurrentDomain<'a>,
    operation: &'a str,
    destination: &'a PublicationTarget,
    content_digest: [u8; 32],
    now: u64,
}

impl<'a> PublicationCommit<'a> {
    /// Bundle the values revalidated immediately before publication.
    pub(crate) fn new(
        current: CurrentDomain<'a>,
        operation: &'a str,
        destination: &'a PublicationTarget,
        content_digest: [u8; 32],
        now: u64,
    ) -> Self {
        Self {
            current,
            operation,
            destination,
            content_digest,
            now,
        }
    }
}

/// One pending publication bound to the exact authorization inputs.
#[derive(Debug)]
#[must_use = "a publication permit must be committed against fresh state"]
pub(crate) struct PublicationPermit {
    source_domain_id: String,
    operation: String,
    destination: PublicationTarget,
    content_digest: [u8; 32],
    grant: Option<GrantUse>,
    confinement: ConfinementSnapshot,
}

impl PublicationPermit {
    fn commit<'a, R: GrantReplayStore>(
        self,
        commit: PublicationCommit<'a>,
        replay_store: &mut R,
    ) -> Result<CommittedPublication<'a>, PublicationCommitError> {
        if self.operation != commit.operation {
            return Err(PublicationCommitError::OperationChanged);
        }
        if &self.destination != commit.destination {
            return Err(PublicationCommitError::DestinationChanged);
        }
        if self.content_digest != commit.content_digest {
            return Err(PublicationCommitError::ContentChanged);
        }
        if let Some(grant) = &self.grant {
            if commit.now >= grant.expires_at {
                return Err(PublicationCommitError::GrantExpired);
            }
            if !replay_store.consume_if_unused(&grant.digest, grant.expires_at) {
                return Err(PublicationCommitError::GrantReplayed);
            }
        }
        Ok(CommittedPublication {
            source_domain_id: self.source_domain_id,
            operation: commit.operation,
            destination: commit.destination,
            content_digest: self.content_digest,
            _source_guard: commit.current,
            _process_guard: PhantomData,
        })
    }
}

/// Final, non-cloneable authorization passed to the broker's publication sink.
///
/// The token borrows the process state and current source-domain proof, so
/// neither can change before the sink consumes it.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "the authorization must be consumed by the publication sink"]
pub(crate) struct CommittedPublication<'a> {
    source_domain_id: String,
    operation: &'a str,
    destination: &'a PublicationTarget,
    content_digest: [u8; 32],
    _source_guard: CurrentDomain<'a>,
    _process_guard: PhantomData<&'a ProcessState>,
}

impl CommittedPublication<'_> {
    /// Return the freshly revalidated source domain identifier.
    pub(crate) fn source_domain_id(&self) -> &str {
        &self.source_domain_id
    }

    /// Return the authorized operation.
    pub(crate) fn operation(&self) -> &str {
        self.operation
    }

    /// Return the freshly revalidated destination.
    pub(crate) fn destination(&self) -> &PublicationTarget {
        self.destination
    }

    /// Return the digest the publication sink must match.
    pub(crate) fn content_digest(&self) -> &[u8; 32] {
        &self.content_digest
    }
}

/// A publication changed after its initial IFC authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum PublicationCommitError {
    /// Membership or policy changed for the source worker.
    #[error("publication source domain changed before commit")]
    SourceChanged,
    /// The worker observed additional input after authorization.
    #[error("process information-flow state changed before commit")]
    ProcessStateChanged,
    /// A different operation was presented at commit time.
    #[error("publication operation changed before commit")]
    OperationChanged,
    /// Destination membership, audience, or context changed before commit.
    #[error("publication destination changed before commit")]
    DestinationChanged,
    /// The payload digest changed after authorization.
    #[error("publication content changed before commit")]
    ContentChanged,
    /// The declassification grant expired before commit.
    #[error("declassification grant expired before publication commit")]
    GrantExpired,
    /// Durable replay protection reports that this grant was already consumed.
    #[error("declassification grant was already consumed")]
    GrantReplayed,
}
