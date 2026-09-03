//! Generated tests for the IFC invariants that must hold across combinations
//! and operation sequences, rather than only for individual examples.

use std::collections::BTreeSet;

use proptest::prelude::*;
use uuid::Uuid;

use crate::test_support::{principal, MemoryReplayStore, NOW};
use crate::*;

const SOURCE_CHANNEL: u128 = 1;
const OTHER_CHANNEL: u128 = 2;
const CURRENT_EPOCH: &str = "v1";
const SOURCE_READERS: Audience = Audience::Restricted(0b0000_0011);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Audience {
    Public,
    Restricted(u8),
}

impl Audience {
    fn label(self, realm: RealmId) -> ConfidentialityLabel {
        match self {
            Self::Public => ConfidentialityLabel::public(realm),
            Self::Restricted(mask) => ConfidentialityLabel::restricted(realm, principals(mask))
                .expect("generated restricted audiences are non-empty"),
        }
    }

    fn can_flow_to(self, destination: Self) -> bool {
        match (self, destination) {
            (Self::Public, _) => true,
            (Self::Restricted(_), Self::Public) => false,
            (Self::Restricted(source), Self::Restricted(destination)) => destination & !source == 0,
        }
    }

    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Public, value) | (value, Self::Public) => value,
            (Self::Restricted(left), Self::Restricted(right)) => Self::Restricted(left & right),
        }
    }
}

fn realm() -> RealmId {
    RealmId::from_relay_url("wss://buzz.example").expect("canonical test realm")
}

fn other_realm() -> RealmId {
    RealmId::from_relay_url("wss://other.example").expect("canonical test realm")
}

fn principals(mask: u8) -> BTreeSet<Principal> {
    (0..6)
        .filter(|bit| mask & (1 << bit) != 0)
        .map(|bit| principal(bit + 1))
        .collect()
}

fn capabilities() -> CapabilitySet {
    CapabilitySet::from_operations([
        ("buzz.read.current", OperationEffect::NonEgressing),
        (
            "buzz.post",
            OperationEffect::Publication(PublicationScope::WithinRealm),
        ),
        (
            "buzz.reply",
            OperationEffect::Publication(PublicationScope::SameContext),
        ),
    ])
}

fn domain_with(
    agent: u8,
    owner: u8,
    audience: Audience,
    channel: u128,
    epoch: &str,
    capabilities: CapabilitySet,
) -> ExecutionDomain {
    ExecutionDomain::new(
        principal(agent),
        Some(principal(owner)),
        audience.label(realm()),
        DomainContext::Conversation {
            realm: realm(),
            channel_id: Uuid::from_u128(channel),
        },
        MembershipEpoch::new(epoch),
        capabilities,
    )
    .expect("coherent generated domain")
}

fn base_domain() -> ExecutionDomain {
    domain_with(
        9,
        1,
        SOURCE_READERS,
        SOURCE_CHANNEL,
        CURRENT_EPOCH,
        capabilities(),
    )
}

#[derive(Clone, Copy, Debug)]
enum ReadCase {
    TrustedConfiguration,
    Public,
    ExactAudience,
    WiderSourceAudience,
    TooNarrow,
    WrongContext,
    StaleEpoch,
    CrossRealm,
    OwnerPrivate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceContextModel {
    TrustedConfiguration,
    RealmPublic,
    Conversation(u128),
    OwnerPrivate,
}

#[derive(Clone, Copy, Debug)]
struct ResourceModel {
    same_realm: bool,
    audience: Audience,
    context: ResourceContextModel,
    current_epoch: bool,
}

impl ReadCase {
    fn model(self) -> ResourceModel {
        match self {
            Self::TrustedConfiguration => ResourceModel {
                same_realm: true,
                audience: Audience::Public,
                context: ResourceContextModel::TrustedConfiguration,
                current_epoch: true,
            },
            Self::Public => ResourceModel {
                same_realm: true,
                audience: Audience::Public,
                context: ResourceContextModel::RealmPublic,
                current_epoch: true,
            },
            Self::ExactAudience => ResourceModel {
                same_realm: true,
                audience: SOURCE_READERS,
                context: ResourceContextModel::Conversation(SOURCE_CHANNEL),
                current_epoch: true,
            },
            Self::WiderSourceAudience => ResourceModel {
                same_realm: true,
                audience: Audience::Restricted(0b0000_0111),
                context: ResourceContextModel::Conversation(SOURCE_CHANNEL),
                current_epoch: true,
            },
            Self::TooNarrow => ResourceModel {
                same_realm: true,
                audience: Audience::Restricted(0b0000_0001),
                context: ResourceContextModel::Conversation(SOURCE_CHANNEL),
                current_epoch: true,
            },
            Self::WrongContext => ResourceModel {
                same_realm: true,
                audience: SOURCE_READERS,
                context: ResourceContextModel::Conversation(OTHER_CHANNEL),
                current_epoch: true,
            },
            Self::StaleEpoch => ResourceModel {
                same_realm: true,
                audience: SOURCE_READERS,
                context: ResourceContextModel::Conversation(SOURCE_CHANNEL),
                current_epoch: false,
            },
            Self::CrossRealm => ResourceModel {
                same_realm: false,
                audience: Audience::Public,
                context: ResourceContextModel::RealmPublic,
                current_epoch: true,
            },
            Self::OwnerPrivate => ResourceModel {
                same_realm: true,
                audience: Audience::Restricted(0b0000_0001),
                context: ResourceContextModel::OwnerPrivate,
                current_epoch: true,
            },
        }
    }

    fn resource(self) -> ResourceLabel {
        let model = self.model();
        let resource_realm = if model.same_realm {
            realm()
        } else {
            other_realm()
        };
        let epoch = MembershipEpoch::new(if model.current_epoch {
            CURRENT_EPOCH
        } else {
            "v2"
        });
        match model.context {
            ResourceContextModel::TrustedConfiguration => {
                ResourceLabel::trusted_configuration(resource_realm)
            }
            ResourceContextModel::RealmPublic => ResourceLabel::realm_public(resource_realm, epoch),
            ResourceContextModel::Conversation(channel) => ResourceLabel::conversation(
                resource_realm,
                Uuid::from_u128(channel),
                match model.audience {
                    Audience::Restricted(mask) => principals(mask),
                    Audience::Public => unreachable!("conversation resources are restricted"),
                },
                epoch,
            )
            .expect("generated conversation audience is non-empty"),
            ResourceContextModel::OwnerPrivate => {
                ResourceLabel::owner_private(resource_realm, principal(1), epoch)
            }
        }
    }
}

fn read_case_strategy() -> impl Strategy<Value = ReadCase> {
    prop_oneof![
        Just(ReadCase::TrustedConfiguration),
        Just(ReadCase::Public),
        Just(ReadCase::ExactAudience),
        Just(ReadCase::WiderSourceAudience),
        Just(ReadCase::TooNarrow),
        Just(ReadCase::WrongContext),
        Just(ReadCase::StaleEpoch),
        Just(ReadCase::CrossRealm),
        Just(ReadCase::OwnerPrivate),
    ]
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DomainVariant {
    Same,
    Agent,
    Owner,
    Audience,
    Context,
    Epoch,
    Capabilities,
}

fn domain_variant_strategy() -> impl Strategy<Value = DomainVariant> {
    prop_oneof![
        3 => Just(DomainVariant::Same),
        1 => Just(DomainVariant::Agent),
        1 => Just(DomainVariant::Owner),
        1 => Just(DomainVariant::Audience),
        1 => Just(DomainVariant::Context),
        1 => Just(DomainVariant::Epoch),
        1 => Just(DomainVariant::Capabilities),
    ]
}

fn domain_variant(variant: DomainVariant) -> ExecutionDomain {
    match variant {
        DomainVariant::Same => base_domain(),
        DomainVariant::Agent => domain_with(
            8,
            1,
            SOURCE_READERS,
            SOURCE_CHANNEL,
            CURRENT_EPOCH,
            capabilities(),
        ),
        DomainVariant::Owner => domain_with(
            9,
            2,
            SOURCE_READERS,
            SOURCE_CHANNEL,
            CURRENT_EPOCH,
            capabilities(),
        ),
        DomainVariant::Audience => domain_with(
            9,
            1,
            Audience::Restricted(0b0000_0001),
            SOURCE_CHANNEL,
            CURRENT_EPOCH,
            capabilities(),
        ),
        DomainVariant::Context => domain_with(
            9,
            1,
            SOURCE_READERS,
            OTHER_CHANNEL,
            CURRENT_EPOCH,
            capabilities(),
        ),
        DomainVariant::Epoch => {
            domain_with(9, 1, SOURCE_READERS, SOURCE_CHANNEL, "v2", capabilities())
        }
        DomainVariant::Capabilities => domain_with(
            9,
            1,
            SOURCE_READERS,
            SOURCE_CHANNEL,
            CURRENT_EPOCH,
            CapabilitySet::from_operations([("buzz.read.current", OperationEffect::NonEgressing)]),
        ),
    }
}

#[derive(Clone, Copy, Debug)]
enum PublishCase {
    SameContext,
    NarrowerAudience,
    WiderAudience,
    OtherContextPost,
    OtherContextReply,
    PublicDestination,
    CrossRealm,
    NonEgressingOperation,
    UnknownOperation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetContextModel {
    SourceConversation,
    OtherConversation,
    RealmPublic,
}

#[derive(Clone, Copy, Debug)]
struct PublicationModel {
    operation: &'static str,
    same_realm: bool,
    audience: Audience,
    context: TargetContextModel,
}

impl PublishCase {
    fn model(self) -> PublicationModel {
        match self {
            Self::SameContext => PublicationModel {
                operation: "buzz.reply",
                same_realm: true,
                audience: SOURCE_READERS,
                context: TargetContextModel::SourceConversation,
            },
            Self::NarrowerAudience => PublicationModel {
                operation: "buzz.reply",
                same_realm: true,
                audience: Audience::Restricted(0b0000_0001),
                context: TargetContextModel::SourceConversation,
            },
            Self::WiderAudience => PublicationModel {
                operation: "buzz.reply",
                same_realm: true,
                audience: Audience::Restricted(0b0000_0111),
                context: TargetContextModel::SourceConversation,
            },
            Self::OtherContextPost => PublicationModel {
                operation: "buzz.post",
                same_realm: true,
                audience: SOURCE_READERS,
                context: TargetContextModel::OtherConversation,
            },
            Self::OtherContextReply => PublicationModel {
                operation: "buzz.reply",
                same_realm: true,
                audience: SOURCE_READERS,
                context: TargetContextModel::OtherConversation,
            },
            Self::PublicDestination => PublicationModel {
                operation: "buzz.post",
                same_realm: true,
                audience: Audience::Public,
                context: TargetContextModel::RealmPublic,
            },
            Self::CrossRealm => PublicationModel {
                operation: "buzz.post",
                same_realm: false,
                audience: Audience::Public,
                context: TargetContextModel::RealmPublic,
            },
            Self::NonEgressingOperation => PublicationModel {
                operation: "buzz.read.current",
                same_realm: true,
                audience: SOURCE_READERS,
                context: TargetContextModel::SourceConversation,
            },
            Self::UnknownOperation => PublicationModel {
                operation: "buzz.unknown",
                same_realm: true,
                audience: SOURCE_READERS,
                context: TargetContextModel::SourceConversation,
            },
        }
    }

    fn target(self) -> PublicationTarget {
        let model = self.model();
        let destination_realm = if model.same_realm {
            realm()
        } else {
            other_realm()
        };
        let context = match model.context {
            TargetContextModel::SourceConversation => DomainContext::Conversation {
                realm: destination_realm.clone(),
                channel_id: Uuid::from_u128(SOURCE_CHANNEL),
            },
            TargetContextModel::OtherConversation => DomainContext::Conversation {
                realm: destination_realm.clone(),
                channel_id: Uuid::from_u128(OTHER_CHANNEL),
            },
            TargetContextModel::RealmPublic => {
                DomainContext::RealmPublic(destination_realm.clone())
            }
        };
        PublicationTarget::new(
            model.audience.label(destination_realm),
            context,
            MembershipEpoch::new(CURRENT_EPOCH),
        )
        .expect("coherent generated publication target")
    }
}

fn publish_case_strategy() -> impl Strategy<Value = PublishCase> {
    prop_oneof![
        Just(PublishCase::SameContext),
        Just(PublishCase::NarrowerAudience),
        Just(PublishCase::WiderAudience),
        Just(PublishCase::OtherContextPost),
        Just(PublishCase::OtherContextReply),
        Just(PublishCase::PublicDestination),
        Just(PublishCase::CrossRealm),
        Just(PublishCase::NonEgressingOperation),
        Just(PublishCase::UnknownOperation),
    ]
}

#[derive(Clone, Debug)]
enum TraceAction {
    Read(ReadCase),
    Refresh(DomainVariant),
    MarkUnknown,
    Publish(PublishCase),
}

fn trace_action_strategy() -> impl Strategy<Value = TraceAction> {
    prop_oneof![
        4 => read_case_strategy().prop_map(TraceAction::Read),
        2 => domain_variant_strategy().prop_map(TraceAction::Refresh),
        1 => Just(TraceAction::MarkUnknown),
        4 => publish_case_strategy().prop_map(TraceAction::Publish),
    ]
}

#[derive(Debug)]
struct SessionModel {
    valid_domain: bool,
    observed: Option<Audience>,
    unknown_input: bool,
}

impl Default for SessionModel {
    fn default() -> Self {
        Self {
            valid_domain: true,
            observed: None,
            unknown_input: false,
        }
    }
}

impl SessionModel {
    fn read(&mut self, resource: ResourceModel) -> bool {
        let context_allowed = matches!(
            resource.context,
            ResourceContextModel::TrustedConfiguration
                | ResourceContextModel::RealmPublic
                | ResourceContextModel::Conversation(SOURCE_CHANNEL)
        );
        let allowed = self.valid_domain
            && resource.same_realm
            && resource.audience.can_flow_to(SOURCE_READERS)
            && context_allowed
            && resource.current_epoch;
        if allowed {
            self.observed = Some(match self.observed {
                Some(existing) => existing.join(resource.audience),
                None => resource.audience,
            });
        }
        allowed
    }

    fn refresh(&mut self, variant: DomainVariant) -> bool {
        let allowed = self.valid_domain && variant == DomainVariant::Same;
        if variant != DomainVariant::Same {
            self.valid_domain = false;
        }
        allowed
    }

    fn publish(&self, publication: PublicationModel) -> bool {
        let Some(observed) = self.observed else {
            return false;
        };
        let operation_allowed = match publication.operation {
            "buzz.post" => true,
            "buzz.reply" => publication.context == TargetContextModel::SourceConversation,
            _ => false,
        };
        self.valid_domain
            && !self.unknown_input
            && operation_allowed
            && publication.same_realm
            && SOURCE_READERS.can_flow_to(publication.audience)
            && observed.can_flow_to(publication.audience)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Compares arbitrary session histories with the small reference model
    /// above. The generated histories mix reads, domain refreshes, unknown
    /// input, and publication attempts instead of checking each operation only
    /// from a freshly created session.
    ///
    /// The invariant is that every operation accepted by the implementation is
    /// also accepted by a model containing only reader-set inclusion, context,
    /// realm, membership epoch, capability scope, monotonic taint, and
    /// permanent domain invalidation. Rejected reads must not change taint,
    /// unknown input can never be forgotten, and a session that has seen a
    /// different domain can never regain authority by returning to its first
    /// domain.
    ///
    /// This catches order-dependent state bugs: a failed refresh becoming
    /// recoverable, rejected input changing later decisions, same-context
    /// operations crossing contexts, or publication succeeding without any
    /// admitted input. In particular, this test found the regression where a
    /// restored domain remained blocked for calls and publications but could
    /// read again.
    /// The modeled operations correspond to the enforcement rules in Appendix
    /// D of `docs/practical-information-flow-for-buzz-agents.md`.
    #[test]
    fn generated_session_traces_match_the_reference_model(
        actions in prop::collection::vec(trace_action_strategy(), 1..80),
    ) {
        let mut session = IfcSession::enter(base_domain());
        let mut model = SessionModel::default();
        let mut replay_store = MemoryReplayStore::default();

        for (index, action) in actions.iter().enumerate() {
            match *action {
                TraceAction::Read(case) => {
                    let expected = model.read(case.model());
                    let actual = session.read(&case.resource()).is_ok();
                    prop_assert_eq!(actual, expected, "step {}: {:?}", index, action);
                }
                TraceAction::Refresh(variant) => {
                    let expected = model.refresh(variant);
                    let actual = session.refresh(domain_variant(variant)).is_ok();
                    prop_assert_eq!(actual, expected, "step {}: {:?}", index, action);
                }
                TraceAction::MarkUnknown => {
                    model.unknown_input = true;
                    session.mark_unknown_input();
                }
                TraceAction::Publish(case) => {
                    let publication = case.model();
                    let target = case.target();
                    let content = (index as u64).to_be_bytes();
                    let expected = model.publish(publication);
                    let actual = session
                        .publish(
                            PublicationRequest::new(
                                publication.operation,
                                &target,
                                &content,
                                NOW,
                            ),
                            None,
                            &mut replay_store,
                        )
                        .is_ok();
                    prop_assert_eq!(actual, expected, "step {}: {:?}", index, action);
                }
            }
        }
    }
}

/// Changes each security-relevant domain component independently: agent,
/// owner, audience, context, membership epoch, and capabilities.
///
/// The invariant is that every component contributes to the domain identifier,
/// changing any one component prevents worker reuse, and restoring the old
/// value cannot make that session valid again. Once a process may have observed
/// information under two domains, the broker must discard it rather than infer
/// that its current domain describes everything it has seen.
///
/// This catches an omitted identity component and any lifecycle path that lets
/// a process regain authority after crossing a domain boundary.
/// The component list comes from the execution-domain model in Appendix B of
/// `docs/practical-information-flow-for-buzz-agents.md`.
#[test]
fn every_domain_component_changes_identity_and_invalidates_reuse() {
    let original = base_domain();
    let variants = [
        (DomainVariant::Agent, domain_variant(DomainVariant::Agent)),
        (DomainVariant::Owner, domain_variant(DomainVariant::Owner)),
        (
            DomainVariant::Audience,
            domain_variant(DomainVariant::Audience),
        ),
        (
            DomainVariant::Context,
            domain_variant(DomainVariant::Context),
        ),
        (DomainVariant::Epoch, domain_variant(DomainVariant::Epoch)),
        (
            DomainVariant::Capabilities,
            domain_variant(DomainVariant::Capabilities),
        ),
    ];

    for (component, changed) in variants {
        assert_ne!(
            original.id(),
            changed.id(),
            "{component:?} must affect the domain identifier"
        );
        let mut session = IfcSession::enter(original.clone());
        assert!(
            session.refresh(changed).is_err(),
            "{component:?} must prevent worker reuse"
        );
        assert!(
            session.refresh(original.clone()).is_err(),
            "{component:?} change must invalidate the session permanently"
        );
    }
}
