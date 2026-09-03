//! Deterministic information-flow policy for Buzz agent execution.
//!
//! This crate contains no relay, ACP, process, or storage code. A trusted Buzz
//! adapter verifies events and membership, supplies [`DomainFacts`], derives an
//! [`ExecutionDomain`], and enters one [`IfcSession`] per worker. The session's
//! `read`, `call`, and `publish` methods are the broker enforcement boundary.
//! Keeping the policy pure lets local ACP and remote harnesses apply the same
//! rules without sharing an agent implementation.
//!
//! The generic reader-set lattice and monotonic flow state live in the
//! dependency-free `ifc-core` crate. This crate specializes those primitives
//! with Buzz realms, Nostr principals, conversation contexts, membership
//! epochs, capabilities, publication targets, and signed declassification.
//!
//! The broker, not the agent, derives every label and execution domain from
//! verified Buzz state. A session accumulates the restrictions of every input
//! admitted to its worker. It may publish normally only when every recipient
//! at the destination was already authorized to read both the domain's data
//! and all accumulated inputs. Audience, retained conversation context,
//! membership epoch, and effective capabilities are all part of the domain,
//! so changing any one of them requires a different worker.
//!
//! An owner may make a deliberate exception with a signed declassification
//! grant. The grant approves one exact operation, source domain, destination,
//! content digest, and expiry, and durable replay protection makes that
//! approval single-use. It does not give the worker a signing key or standing
//! authority to release later output.
//!
//! The complete threat model, broker design, and formal information-flow model
//! are in [Practical information-flow for Buzz agents].
//!
//! [Practical information-flow for Buzz agents]: ../../../docs/practical-information-flow-for-buzz-agents.md

mod declassification;
mod domain;
mod hash;
mod label;
mod policy;
mod session;

pub use declassification::{
    DeclassificationGrant, DeclassificationGrantPayload, GrantError, GrantId, GrantReplayStore,
    VerifiedDeclassificationGrant,
};
pub use domain::{
    derive_execution_domain, CapabilityPolicy, CapabilitySet, CompartmentProfile, ConversationKind,
    DerivationError, DomainContext, DomainFacts, DomainKey, ExecutionDomain, MembershipEpoch,
    OperationEffect, PublicationScope, PublicationTarget, PublicationTargetError, ResourceLabel,
};
pub use label::{ConfidentialityLabel, LabelError, Principal, PrincipalError, RealmError, RealmId};
pub use session::{
    publication_digest, AuthorizedPublication, IfcError, IfcSession, PublicationRequest,
};

#[cfg(test)]
mod security_property_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
