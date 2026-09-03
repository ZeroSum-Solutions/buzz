use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::hash::{hash_field, short_fingerprint};
use crate::label::{
    stable_hash_label, stable_hash_readers, ConfidentialityLabel, LabelError, Principal, ReaderSet,
    RealmId,
};

/// Which retained context a worker belongs to.
///
/// Audience answers who may read information; context answers which
/// conversation history and managed memory a worker may retain. These are
/// independent boundaries. Two conversations with identical participants do
/// not implicitly share state, while public conversations deliberately use one
/// realm-wide public context. Owner-private state is likewise distinct from
/// ordinary conversation state, even within the same realm.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DomainContext {
    /// Shared state for public conversations in one realm.
    RealmPublic(RealmId),
    /// State retained for one specific restricted conversation.
    Conversation {
        /// The Buzz community containing the conversation.
        realm: RealmId,
        /// The channel, DM, or group-DM identifier.
        channel_id: Uuid,
    },
    /// State visible only to the bot owner.
    OwnerPrivate {
        /// The Buzz community containing the owner relationship.
        realm: RealmId,
        /// The bot owner.
        owner: Principal,
    },
}

/// Runtime placement required by an execution domain.
///
/// The IFC rules do not implement an OS sandbox. They tell the harness whether
/// a worker may use the shared public runtime or must be placed in a compartment
/// dedicated to one complete execution domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompartmentProfile {
    /// Realm-public conversations may share a worker, public memory, and public
    /// tools. The worker must still be unable to reach broker secrets or private
    /// compartments.
    SharedPublic,
    /// A restricted conversation or owner-private task requires a worker whose
    /// writable state and output paths are confined to the exact domain.
    DomainConfined,
}

impl CompartmentProfile {
    /// Return the stable wire and log representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SharedPublic => "shared_public",
            Self::DomainConfined => "domain_confined",
        }
    }
}

impl DomainContext {
    /// Return the realm containing this context.
    pub fn realm(&self) -> &RealmId {
        match self {
            Self::RealmPublic(realm)
            | Self::Conversation { realm, .. }
            | Self::OwnerPrivate { realm, .. } => realm,
        }
    }

    /// Return a stable context category for logs and protocol responses.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::RealmPublic(_) => "public",
            Self::Conversation { .. } => "conversation",
            Self::OwnerPrivate { .. } => "owner_private",
        }
    }

    /// Whether this is the private aggregation context for `owner`.
    pub fn is_owner_private_for(&self, owner: &Principal) -> bool {
        matches!(self, Self::OwnerPrivate { owner: candidate, .. } if candidate == owner)
    }

    fn resource_context(&self) -> ResourceContext {
        match self {
            Self::RealmPublic(realm) => ResourceContext::RealmPublic(realm.clone()),
            Self::Conversation { realm, channel_id } => ResourceContext::Conversation {
                realm: realm.clone(),
                channel_id: *channel_id,
            },
            Self::OwnerPrivate { realm, owner } => ResourceContext::OwnerPrivate {
                realm: realm.clone(),
                owner: owner.clone(),
            },
        }
    }

    pub(crate) fn permits(&self, resource: &ResourceContext) -> bool {
        match resource {
            ResourceContext::TrustedConfiguration => true,
            ResourceContext::RealmPublic(resource_realm) => self.realm() == resource_realm,
            ResourceContext::Conversation {
                realm: resource_realm,
                channel_id: resource_channel,
            } => match self {
                Self::Conversation { realm, channel_id } => {
                    realm == resource_realm && channel_id == resource_channel
                }
                Self::OwnerPrivate { realm, .. } => realm == resource_realm,
                Self::RealmPublic(_) => false,
            },
            ResourceContext::OwnerPrivate {
                realm: resource_realm,
                owner: resource_owner,
            } => matches!(
                self,
                Self::OwnerPrivate { realm, owner }
                    if realm == resource_realm && owner == resource_owner
            ),
        }
    }

    pub(crate) fn stable_hash(&self, hasher: &mut Sha256) {
        match self {
            Self::RealmPublic(realm) => {
                hash_field(hasher, b"realm-public");
                realm.stable_hash(hasher);
            }
            Self::Conversation { realm, channel_id } => {
                hash_field(hasher, b"conversation");
                realm.stable_hash(hasher);
                hash_field(hasher, channel_id.as_bytes());
            }
            Self::OwnerPrivate { realm, owner } => {
                hash_field(hasher, b"owner-private");
                realm.stable_hash(hasher);
                hash_field(hasher, owner.0.as_bytes());
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ResourceContext {
    TrustedConfiguration,
    RealmPublic(RealmId),
    Conversation { realm: RealmId, channel_id: Uuid },
    OwnerPrivate { realm: RealmId, owner: Principal },
}

/// Whether invoking an operation can publish information outside the worker.
///
/// This classification belongs to trusted policy configuration, not to the
/// agent's call request. Publication operations always require both capability
/// admission and an information-flow decision before they may execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationScope {
    /// The operation may publish only to its source context.
    SameContext,
    /// The operation may select another context in the same realm, subject to
    /// the ordinary audience-flow or declassification rules.
    WithinRealm,
}

impl PublicationScope {
    fn most_restrictive(self, other: Self) -> Self {
        if self == Self::SameContext || other == Self::SameContext {
            Self::SameContext
        } else {
            Self::WithinRealm
        }
    }

    fn stable_name(self) -> &'static [u8] {
        match self {
            Self::SameContext => b"same-context",
            Self::WithinRealm => b"within-realm",
        }
    }
}

/// Whether invoking an operation can publish information outside the worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationEffect {
    /// The operation cannot publish information outside the worker boundary.
    NonEgressing,
    /// The operation publishes information to a broker-resolved destination.
    Publication(PublicationScope),
}

impl OperationEffect {
    fn most_restrictive(self, other: Self) -> Self {
        match (self, other) {
            (Self::NonEgressing, Self::NonEgressing) => Self::NonEgressing,
            (Self::Publication(left), Self::Publication(right)) => {
                Self::Publication(left.most_restrictive(right))
            }
            (Self::Publication(scope), Self::NonEgressing)
            | (Self::NonEgressing, Self::Publication(scope)) => Self::Publication(scope),
        }
    }

    fn stable_hash(self, hasher: &mut Sha256) {
        match self {
            Self::NonEgressing => hash_field(hasher, b"non-egressing"),
            Self::Publication(scope) => {
                hash_field(hasher, b"publication");
                hash_field(hasher, scope.stable_name());
            }
        }
    }
}

/// The complete set of operations admitted for one execution domain.
///
/// Raw membership is intentionally private because it is not an authorization
/// decision:
///
/// ```compile_fail
/// let capabilities = buzz_ifc::CapabilitySet::default();
/// let _ = capabilities.contains("buzz.post");
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilitySet(BTreeMap<String, OperationEffect>);

impl CapabilitySet {
    /// Build a set from stable operation names and trusted effect classes.
    ///
    /// If an operation is repeated with different effects, publication wins so
    /// conflicting configuration cannot downgrade an egressing operation.
    pub fn from_operations<I, S>(operations: I) -> Self
    where
        I: IntoIterator<Item = (S, OperationEffect)>,
        S: Into<String>,
    {
        let mut capabilities: BTreeMap<String, OperationEffect> = BTreeMap::new();
        for (name, effect) in operations {
            capabilities
                .entry(name.into())
                .and_modify(|existing| *existing = existing.most_restrictive(effect))
                .or_insert(effect);
        }
        Self(capabilities)
    }

    /// Compute the operations admitted by every independent capability
    /// ceiling: the bot, the authenticated requester, and the execution
    /// domain.
    ///
    /// An operation missing from any ceiling is denied. When the same
    /// operation has conflicting effect classifications, the result preserves
    /// the most restrictive classification: publication wins over
    /// non-egressing, and same-context publication wins over within-realm
    /// publication. No policy layer can accidentally downgrade an egressing
    /// operation into an unchecked call.
    pub(crate) fn effective(bot: &Self, requester: &Self, domain: &Self) -> Self {
        let mut effective = BTreeMap::new();
        for (name, bot_effect) in &bot.0 {
            let (Some(requester_effect), Some(domain_effect)) =
                (requester.0.get(name), domain.0.get(name))
            else {
                continue;
            };
            effective.insert(
                name.clone(),
                bot_effect
                    .most_restrictive(*requester_effect)
                    .most_restrictive(*domain_effect),
            );
        }
        Self(effective)
    }

    pub(crate) fn effect(&self, operation: &str) -> Option<OperationEffect> {
        self.0.get(operation).copied()
    }

    fn stable_hash(&self, hasher: &mut Sha256) {
        hash_field(hasher, &(self.0.len() as u64).to_be_bytes());
        for (name, effect) in &self.0 {
            hash_field(hasher, name.as_bytes());
            effect.stable_hash(hasher);
        }
    }
}

/// Capability ceilings used while deriving an invocation's effective set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityPolicy {
    bot: CapabilitySet,
    conversation: CapabilitySet,
}

impl CapabilityPolicy {
    /// Construct policy from the bot's full ceiling and the ceiling permitted
    /// in shared Buzz conversations.
    pub fn new(bot: CapabilitySet, conversation: CapabilitySet) -> Self {
        Self { bot, conversation }
    }
}

/// The membership or policy version under which state was created.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MembershipEpoch(String);

impl MembershipEpoch {
    /// Construct an epoch from a stable, verifier-controlled identifier.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Return a short identifier suitable for logs.
    pub fn fingerprint(&self) -> String {
        short_fingerprint(&self.0)
    }

    pub(crate) fn stable_hash(&self, hasher: &mut Sha256) {
        hash_field(hasher, self.0.as_bytes());
    }
}

/// Buzz conversation classification after signed metadata verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationKind {
    /// Realm-wide public channel. All public channels intentionally share one
    /// execution domain.
    Public,
    /// Invite-only channel or group DM with conversation-specific state.
    Restricted,
    /// A DM. A two-party owner/bot DM becomes owner-private; group DMs remain
    /// conversation-specific.
    DirectMessage,
}

/// Verified Buzz facts from which the shared policy derives an execution
/// domain.
///
/// A trusted adapter constructs this only after checking trigger signatures,
/// channel binding, and the relay signature on metadata and membership.
pub struct DomainFacts {
    /// Community realm selected by the trusted Buzz connection.
    pub realm: RealmId,
    /// Channel, DM, or group-DM identifier that triggered the invocation.
    pub channel_id: Uuid,
    /// Verified conversation classification.
    pub kind: ConversationKind,
    /// Relay-controlled membership or community policy version.
    pub epoch: MembershipEpoch,
    /// Complete verified roster. Public derivation does not consume this set.
    pub members: BTreeSet<Principal>,
    /// Managed Buzz identity whose work the execution domain contains.
    pub executing_agent: Principal,
    /// Authors whose events are included in this invocation.
    pub requesters: BTreeSet<Principal>,
    /// Optional relay principal allowed to author trusted workflow events.
    pub system_principal: Option<Principal>,
    /// Optional human owner of the executing agent.
    pub owner: Option<Principal>,
}

/// Domain derivation failed despite the adapter's claim that its facts were
/// already verified.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DerivationError {
    /// Every invocation must contain at least one authenticated requester.
    #[error("invocation has no authenticated requester")]
    EmptyRequesters,
    /// Restricted conversations must include the executing agent in their
    /// verified roster.
    #[error("executing agent is absent from channel membership")]
    AgentNotMember,
    /// A non-system requester is absent from restricted membership.
    #[error("requester is absent from channel membership")]
    RequesterNotMember,
    /// Removing the executing processor left no authorized recipient.
    #[error("restricted conversation has no recipient audience")]
    EmptyRestrictedAudience,
    /// The derived audience and context violated a domain invariant.
    #[error("derived execution domain is inconsistent")]
    InvalidDomain,
}

/// Derive a complete execution domain from verified Buzz facts.
///
/// The adapter supplies authenticated requesters, verified conversation
/// membership, and a relay-controlled membership epoch; the agent cannot
/// assert any of these values. Public conversations receive the realm-wide
/// audience and shared public context. Restricted conversations receive the
/// verified member audience, excluding the executing agent, and retain state
/// only for their channel. A two-party owner/agent DM instead receives the
/// owner's private context. Effective capabilities are the intersection of
/// the bot, requester, and context ceilings.
///
/// Agent identity, owner, audience, context, epoch, and capabilities all feed
/// the domain identifier used for worker placement. A change to any component
/// therefore selects a different domain rather than silently reusing a worker
/// that may remember information admitted under older authority. This mapping
/// is shared by local ACP and remote agent harnesses.
pub fn derive_execution_domain(
    facts: DomainFacts,
    policy: &CapabilityPolicy,
) -> Result<ExecutionDomain, DerivationError> {
    if facts.requesters.is_empty() {
        return Err(DerivationError::EmptyRequesters);
    }

    if facts.kind == ConversationKind::Public {
        let context = DomainContext::RealmPublic(facts.realm.clone());
        let capabilities = effective_capabilities(&context, &facts, policy);
        return Ok(ExecutionDomain::public(
            facts.executing_agent,
            facts.owner,
            facts.realm,
            facts.epoch,
            capabilities,
        ));
    }

    if !facts.members.contains(&facts.executing_agent) {
        return Err(DerivationError::AgentNotMember);
    }
    if facts.requesters.iter().any(|requester| {
        facts.system_principal.as_ref() != Some(requester) && !facts.members.contains(requester)
    }) {
        return Err(DerivationError::RequesterNotMember);
    }

    let mut readers = facts.members.clone();
    readers.remove(&facts.executing_agent);
    if readers.is_empty() {
        return Err(DerivationError::EmptyRestrictedAudience);
    }

    let context = match (&facts.owner, facts.kind) {
        (Some(owner), ConversationKind::DirectMessage)
            if readers.len() == 1 && readers.contains(owner) =>
        {
            DomainContext::OwnerPrivate {
                realm: facts.realm.clone(),
                owner: owner.clone(),
            }
        }
        _ => DomainContext::Conversation {
            realm: facts.realm.clone(),
            channel_id: facts.channel_id,
        },
    };
    let capabilities = effective_capabilities(&context, &facts, policy);
    let audience = ConfidentialityLabel::restricted(facts.realm, readers)
        .map_err(|_| DerivationError::EmptyRestrictedAudience)?;
    ExecutionDomain::new(
        facts.executing_agent,
        facts.owner,
        audience,
        context,
        facts.epoch,
        capabilities,
    )
    .map_err(|_| DerivationError::InvalidDomain)
}

fn effective_capabilities(
    context: &DomainContext,
    facts: &DomainFacts,
    policy: &CapabilityPolicy,
) -> CapabilitySet {
    let requester_is_owner = facts
        .owner
        .as_ref()
        .is_some_and(|owner| facts.requesters.iter().all(|requester| requester == owner));
    let requester = if requester_is_owner {
        &policy.bot
    } else {
        &policy.conversation
    };
    let domain = if matches!(context, DomainContext::OwnerPrivate { .. }) {
        &policy.bot
    } else {
        &policy.conversation
    };
    CapabilitySet::effective(&policy.bot, requester, domain)
}

/// Opaque routing key for one complete execution domain.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct DomainKey(String);

impl DomainKey {
    /// Return a short identifier suitable for logs.
    pub fn fingerprint(&self) -> String {
        short_fingerprint(&self.0)
    }

    /// Return the full stable identifier used as a worker-pool routing key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `D = (Agent, Audience, Context, Epoch, Capabilities)`.
///
/// This is the executable form of the domain model in [Appendix B of the
/// design paper](../../../docs/practical-information-flow-for-buzz-agents.md#appendix-b-formal-execution-domains).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionDomain {
    agent: Principal,
    owner: Option<Principal>,
    pub(crate) audience: ConfidentialityLabel,
    pub(crate) context: DomainContext,
    pub(crate) epoch: MembershipEpoch,
    pub(crate) capabilities: CapabilitySet,
}

/// Trusted holder of the currently verified execution domain.
///
/// The adapter replaces this value whenever membership or policy changes.
/// Authorization APIs accept only a borrowed [`CurrentDomain`], preventing an
/// arbitrary stored `ExecutionDomain` from being passed as its own freshness
/// proof.
#[derive(Debug)]
pub(crate) struct DomainAuthority {
    current: ExecutionDomain,
}

impl DomainAuthority {
    /// Install the domain derived from the adapter's current verified state.
    pub(crate) fn new(current: ExecutionDomain) -> Self {
        Self { current }
    }

    /// Replace the current domain after verified membership or policy changes.
    ///
    /// Rust's borrow rules prevent replacement while a `CurrentDomain` token
    /// from this authority remains usable.
    pub(crate) fn replace(&mut self, current: ExecutionDomain) {
        self.current = current;
    }

    /// Borrow an authority-minted proof of the current domain.
    pub(crate) fn current(&self) -> CurrentDomain<'_> {
        CurrentDomain {
            domain: &self.current,
        }
    }
}

/// Unforgeable-by-construction borrow of a [`DomainAuthority`]'s current value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CurrentDomain<'a> {
    domain: &'a ExecutionDomain,
}

impl<'a> CurrentDomain<'a> {
    /// Return the currently verified execution domain.
    pub(crate) fn domain(self) -> &'a ExecutionDomain {
        self.domain
    }
}

/// A broker-resolved publication destination and its current membership epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationTarget {
    audience: ConfidentialityLabel,
    context: DomainContext,
    epoch: MembershipEpoch,
}

impl PublicationTarget {
    /// Construct a current destination after the broker resolves membership.
    pub fn new(
        audience: ConfidentialityLabel,
        context: DomainContext,
        epoch: MembershipEpoch,
    ) -> Result<Self, PublicationTargetError> {
        if audience.universe() != context.realm() {
            return Err(PublicationTargetError::ContextRealmMismatch);
        }
        if !audience_context_shape_matches(&audience, &context) {
            return Err(PublicationTargetError::AudienceContextMismatch);
        }
        Ok(Self {
            audience,
            context,
            epoch,
        })
    }

    /// Resolve a target from a currently verified execution domain.
    pub(crate) fn from_current(current: CurrentDomain<'_>) -> Self {
        Self {
            audience: current.domain.audience.clone(),
            context: current.domain.context.clone(),
            epoch: current.domain.epoch.clone(),
        }
    }

    /// Return the destination audience.
    pub fn audience(&self) -> &ConfidentialityLabel {
        &self.audience
    }

    /// Return the destination context.
    pub fn context(&self) -> &DomainContext {
        &self.context
    }

    /// Return the verified destination membership epoch.
    pub fn epoch(&self) -> &MembershipEpoch {
        &self.epoch
    }

    pub(crate) fn stable_hash(&self, hasher: &mut Sha256) {
        stable_hash_label(&self.audience, hasher);
        self.context.stable_hash(hasher);
        self.epoch.stable_hash(hasher);
    }
}

/// A publication target contains inconsistent audience or context data.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PublicationTargetError {
    /// The audience and context belong to different Buzz realms.
    #[error("publication target audience and context belong to different realms")]
    ContextRealmMismatch,
    /// The audience shape is invalid for the selected context kind.
    #[error("publication target audience does not match its context")]
    AudienceContextMismatch,
}

impl ExecutionDomain {
    /// Construct a domain after the trusted adapter has resolved its inputs.
    pub(crate) fn new(
        agent: Principal,
        owner: Option<Principal>,
        audience: ConfidentialityLabel,
        context: DomainContext,
        epoch: MembershipEpoch,
        capabilities: CapabilitySet,
    ) -> Result<Self, DomainError> {
        if audience.universe() != context.realm() {
            return Err(DomainError::ContextRealmMismatch);
        }
        if !audience_context_shape_matches(&audience, &context) {
            return Err(DomainError::AudienceContextMismatch);
        }
        Ok(Self {
            agent,
            owner,
            audience,
            context,
            epoch,
            capabilities,
        })
    }

    /// Construct the realm-wide public domain.
    pub(crate) fn public(
        agent: Principal,
        owner: Option<Principal>,
        realm: RealmId,
        epoch: MembershipEpoch,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            agent,
            owner,
            audience: ConfidentialityLabel::public(realm.clone()),
            context: DomainContext::RealmPublic(realm),
            epoch,
            capabilities,
        }
    }

    /// Construct the exact owner-private domain.
    #[cfg(test)]
    pub(crate) fn owner_private(
        agent: Principal,
        realm: RealmId,
        owner: Principal,
        epoch: MembershipEpoch,
        capabilities: CapabilitySet,
    ) -> Self {
        Self {
            agent,
            owner: Some(owner.clone()),
            audience: ConfidentialityLabel::restricted_to(realm.clone(), owner.clone()),
            context: DomainContext::OwnerPrivate { realm, owner },
            epoch,
            capabilities,
        }
    }

    /// Return the managed Buzz identity whose work this domain contains.
    pub fn agent(&self) -> &Principal {
        &self.agent
    }

    /// Return the human principal allowed to declassify this agent's data.
    pub fn owner(&self) -> Option<&Principal> {
        self.owner.as_ref()
    }

    /// Return the authorized audience.
    pub fn audience(&self) -> &ConfidentialityLabel {
        &self.audience
    }

    /// Return the retained-state context.
    pub fn context(&self) -> &DomainContext {
        &self.context
    }

    /// Return the runtime placement required to preserve this domain.
    pub fn compartment_profile(&self) -> CompartmentProfile {
        if self.audience.is_public() {
            CompartmentProfile::SharedPublic
        } else {
            CompartmentProfile::DomainConfined
        }
    }

    /// Return a short fingerprint of the membership or policy epoch.
    pub fn epoch_fingerprint(&self) -> String {
        self.epoch.fingerprint()
    }

    /// Return the canonical identifier for the complete domain tuple.
    pub fn id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"buzz-ifc-domain-v3");
        hash_field(&mut hasher, self.agent.0.as_bytes());
        match &self.owner {
            Some(owner) => {
                hash_field(&mut hasher, b"owner");
                hash_field(&mut hasher, owner.0.as_bytes());
            }
            None => hash_field(&mut hasher, b"no-owner"),
        }
        self.audience.universe().stable_hash(&mut hasher);
        stable_hash_readers(self.audience.reader_set(), &mut hasher);
        self.context.stable_hash(&mut hasher);
        hash_field(&mut hasher, self.epoch.0.as_bytes());
        self.capabilities.stable_hash(&mut hasher);
        hex::encode(hasher.finalize())
    }

    /// Return the opaque worker-pool routing key.
    pub fn key(&self) -> DomainKey {
        DomainKey(self.id())
    }

    /// Label information whose provenance is this domain itself.
    pub fn resource_label(&self) -> ResourceLabel {
        ResourceLabel {
            confidentiality: self.audience.clone(),
            context: self.context.resource_context(),
            epoch: Some(self.epoch.clone()),
        }
    }
}

pub(crate) fn audience_context_shape_matches(
    audience: &ConfidentialityLabel,
    context: &DomainContext,
) -> bool {
    match (audience.reader_set(), context) {
        (ReaderSet::Everyone, DomainContext::RealmPublic(_)) => true,
        (ReaderSet::Only(readers), DomainContext::Conversation { .. }) => !readers.is_empty(),
        (ReaderSet::Only(readers), DomainContext::OwnerPrivate { owner, .. }) => {
            readers.len() == 1 && readers.contains(owner)
        }
        _ => false,
    }
}

/// An execution domain contains inconsistent realms.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DomainError {
    /// The audience and retained context belong to different Buzz realms.
    #[error("execution-domain audience and context belong to different realms")]
    ContextRealmMismatch,
    /// Public, conversation, and owner-private contexts require their
    /// corresponding audience shape.
    #[error("execution-domain audience does not match its context")]
    AudienceContextMismatch,
}

/// The confidentiality and context assigned to one input resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceLabel {
    pub(crate) confidentiality: ConfidentialityLabel,
    pub(crate) context: ResourceContext,
    pub(crate) epoch: Option<MembershipEpoch>,
}

impl ResourceLabel {
    /// Label immutable configuration that is public in the supplied realm.
    pub fn trusted_configuration(realm: RealmId) -> Self {
        Self {
            confidentiality: ConfidentialityLabel::public(realm),
            context: ResourceContext::TrustedConfiguration,
            epoch: None,
        }
    }

    /// Label information already scoped to an execution domain.
    pub fn domain(domain: &ExecutionDomain) -> Self {
        domain.resource_label()
    }

    /// Label information public to every member of one realm.
    pub fn realm_public(realm: RealmId, epoch: MembershipEpoch) -> Self {
        Self {
            confidentiality: ConfidentialityLabel::public(realm.clone()),
            context: ResourceContext::RealmPublic(realm),
            epoch: Some(epoch),
        }
    }

    /// Label information belonging to one restricted conversation.
    pub fn conversation(
        realm: RealmId,
        channel_id: Uuid,
        readers: BTreeSet<Principal>,
        epoch: MembershipEpoch,
    ) -> Result<Self, LabelError> {
        Ok(Self {
            confidentiality: ConfidentialityLabel::restricted(realm.clone(), readers)?,
            context: ResourceContext::Conversation { realm, channel_id },
            epoch: Some(epoch),
        })
    }

    /// Label owner-private information such as personal memory.
    pub fn owner_private(realm: RealmId, owner: Principal, epoch: MembershipEpoch) -> Self {
        Self {
            confidentiality: ConfidentialityLabel::restricted_to(realm.clone(), owner.clone()),
            context: ResourceContext::OwnerPrivate { realm, owner },
            epoch: Some(epoch),
        }
    }

    pub(crate) fn is_current_for(&self, domain: &ExecutionDomain) -> bool {
        self.epoch
            .as_ref()
            .is_none_or(|epoch| epoch == &domain.epoch)
    }
}
