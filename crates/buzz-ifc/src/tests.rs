use std::collections::BTreeSet;

use uuid::Uuid;

use super::*;
use crate::domain::{DomainAuthority, DomainError};
use crate::policy::{ProcessState, PublicationCommit, PublicationCommitError, RuleEvaluator};
use crate::test_support::*;

fn readers(values: &[u8]) -> BTreeSet<Principal> {
    values.iter().copied().map(principal).collect()
}

fn realm() -> RealmId {
    RealmId::from_relay_url("wss://buzz.example").expect("canonical test realm")
}

fn label(values: &[u8]) -> ConfidentialityLabel {
    ConfidentialityLabel::restricted(realm(), readers(values)).expect("non-empty readers")
}

fn non_egressing<const N: usize>(names: [&str; N]) -> CapabilitySet {
    CapabilitySet::from_operations(names.map(|name| (name, OperationEffect::NonEgressing)))
}

fn conversation_capabilities() -> CapabilitySet {
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

fn conversation(values: &[u8], channel_id: Uuid, epoch: &str) -> ExecutionDomain {
    ExecutionDomain::new(
        principal(9),
        Some(principal(1)),
        label(values),
        DomainContext::Conversation {
            realm: realm(),
            channel_id,
        },
        MembershipEpoch::new(epoch),
        conversation_capabilities(),
    )
    .expect("coherent conversation domain")
}

fn conversation_target(values: &[u8], channel_id: Uuid, epoch: &str) -> PublicationTarget {
    PublicationTarget::new(
        label(values),
        DomainContext::Conversation {
            realm: realm(),
            channel_id,
        },
        MembershipEpoch::new(epoch),
    )
    .expect("coherent conversation target")
}

fn public_target(realm: RealmId, epoch: &str) -> PublicationTarget {
    PublicationTarget::new(
        ConfidentialityLabel::public(realm.clone()),
        DomainContext::RealmPublic(realm),
        MembershipEpoch::new(epoch),
    )
    .expect("coherent public target")
}

fn process_for(domain: ExecutionDomain) -> (DomainAuthority, ProcessState) {
    let authority = DomainAuthority::new(domain);
    let mut state = ProcessState::default();
    assert!(state.enter(authority.current()).allowed());
    (authority, state)
}

#[test]
fn principals_must_be_valid_x_only_secp256k1_points() {
    for invalid in ["00".repeat(32), "ff".repeat(32), format!("{:064x}", 5)] {
        assert_eq!(
            Principal::from_hex(&invalid),
            Err(PrincipalError::InvalidPublicKey)
        );
    }
}

#[test]
fn realm_ids_canonicalize_equivalent_relay_urls() {
    let canonical = RealmId::from_relay_url("wss://buzz.example").expect("valid URL");
    let equivalent = RealmId::from_relay_url("WSS://BUZZ.EXAMPLE:443/").expect("valid URL");
    let path = RealmId::from_relay_url("wss://buzz.example/relay/").expect("valid URL");
    let path_without_slash =
        RealmId::from_relay_url("wss://buzz.example/relay").expect("valid URL");

    assert_eq!(canonical, equivalent);
    assert_eq!(path, path_without_slash);
    assert_ne!(
        canonical,
        RealmId::from_relay_url("wss://other.example").expect("valid URL")
    );
}

#[test]
fn realm_ids_reject_connection_details_and_non_websocket_urls() {
    assert_eq!(
        RealmId::from_relay_url("https://buzz.example"),
        Err(RealmError::UnsupportedScheme)
    );
    assert_eq!(
        RealmId::from_relay_url("wss://user@buzz.example"),
        Err(RealmError::Credentials)
    );
    assert_eq!(
        RealmId::from_relay_url("wss://buzz.example?token=secret"),
        Err(RealmError::QueryOrFragment)
    );
}

#[test]
fn combining_inputs_intersects_authorized_readers() {
    let combined = label(&[1, 2]).join(&label(&[1, 3])).expect("same realm");
    assert!(combined.can_flow_to(&label(&[1])));
    assert!(!combined.can_flow_to(&label(&[1, 2])));
}

#[test]
fn labels_never_flow_across_realms() {
    let first = ConfidentialityLabel::public(
        RealmId::from_relay_url("wss://one.example").expect("valid URL"),
    );
    let second = ConfidentialityLabel::public(
        RealmId::from_relay_url("wss://two.example").expect("valid URL"),
    );
    assert!(!first.can_flow_to(&second));
    assert_eq!(first.join(&second), Err(LabelError::CrossUniverse));
}

#[test]
fn read_requires_current_domain_audience_context_and_epoch() {
    let channel = Uuid::from_u128(1);
    let domain = conversation(&[1, 2], channel, "v2");
    let authority = DomainAuthority::new(domain.clone());
    let wrong_audience =
        ResourceLabel::conversation(realm(), channel, readers(&[1]), MembershipEpoch::new("v2"))
            .expect("non-empty readers");
    let wrong_context = ResourceLabel::conversation(
        realm(),
        Uuid::from_u128(2),
        readers(&[1, 2]),
        MembershipEpoch::new("v2"),
    )
    .expect("non-empty readers");
    let stale = ResourceLabel::conversation(
        realm(),
        channel,
        readers(&[1, 2]),
        MembershipEpoch::new("v1"),
    )
    .expect("non-empty readers");

    assert!(!RuleEvaluator::read(&domain, authority.current(), &wrong_audience).allowed());
    assert!(!RuleEvaluator::read(&domain, authority.current(), &wrong_context).allowed());
    assert!(!RuleEvaluator::read(&domain, authority.current(), &stale).allowed());
    assert!(RuleEvaluator::read(&domain, authority.current(), &domain.resource_label()).allowed());
}

#[test]
fn owner_private_context_aggregates_only_current_owner_readable_inputs() {
    let owner = principal(1);
    let domain = ExecutionDomain::owner_private(
        principal(9),
        realm(),
        owner,
        MembershipEpoch::new("owner-v1"),
        CapabilitySet::default(),
    );
    let authority = DomainAuthority::new(domain.clone());
    let readable = ResourceLabel::conversation(
        realm(),
        Uuid::from_u128(1),
        readers(&[1, 2]),
        MembershipEpoch::new("owner-v1"),
    )
    .expect("non-empty readers");
    let unreadable = ResourceLabel::conversation(
        realm(),
        Uuid::from_u128(2),
        readers(&[2, 3]),
        MembershipEpoch::new("owner-v1"),
    )
    .expect("non-empty readers");

    assert!(RuleEvaluator::read(&domain, authority.current(), &readable).allowed());
    assert!(!RuleEvaluator::read(&domain, authority.current(), &unreadable).allowed());
}

#[test]
fn capability_intersection_keeps_names_and_narrowest_publication_scope() {
    let bot = CapabilitySet::from_operations([
        (
            "buzz.post",
            OperationEffect::Publication(PublicationScope::WithinRealm),
        ),
        ("email.read", OperationEffect::NonEgressing),
    ]);
    let requester = CapabilitySet::from_operations([(
        "buzz.post",
        OperationEffect::Publication(PublicationScope::SameContext),
    )]);
    let domain = CapabilitySet::from_operations([
        ("buzz.post", OperationEffect::NonEgressing),
        ("drive.read", OperationEffect::NonEgressing),
    ]);

    assert_eq!(
        CapabilitySet::effective(&bot, &requester, &domain),
        CapabilitySet::from_operations([(
            "buzz.post",
            OperationEffect::Publication(PublicationScope::SameContext),
        )])
    );
}

#[test]
fn domain_derivation_grants_personal_tools_only_in_owner_private_work() {
    let owner = principal(1);
    let agent = principal(9);
    let policy = CapabilityPolicy::new(
        non_egressing(["buzz.read.current", "email.read"]),
        non_egressing(["buzz.read.current"]),
    );
    let owner_dm = derive_execution_domain(
        DomainFacts {
            realm: realm(),
            channel_id: Uuid::from_u128(1),
            kind: ConversationKind::DirectMessage,
            epoch: MembershipEpoch::new("membership:v1"),
            members: BTreeSet::from([agent.clone(), owner.clone()]),
            executing_agent: agent.clone(),
            requesters: BTreeSet::from([owner.clone()]),
            system_principal: None,
            owner: Some(owner.clone()),
        },
        &policy,
    )
    .expect("owner DM domain");
    assert!(owner_dm.context().is_owner_private_for(&owner));
    let (owner_authority, owner_state) = process_for(owner_dm);
    assert!(owner_state
        .authorize_non_egressing_call(owner_authority.current(), "email.read")
        .allowed());

    let public = derive_execution_domain(
        DomainFacts {
            realm: realm(),
            channel_id: Uuid::from_u128(2),
            kind: ConversationKind::Public,
            epoch: MembershipEpoch::new("community:v1"),
            members: BTreeSet::new(),
            executing_agent: agent,
            requesters: BTreeSet::from([owner.clone()]),
            system_principal: None,
            owner: Some(owner),
        },
        &policy,
    )
    .expect("public domain");
    let (public_authority, public_state) = process_for(public);
    assert!(!public_state
        .authorize_non_egressing_call(public_authority.current(), "email.read")
        .allowed());
}

#[test]
fn restricted_domain_derivation_checks_agent_and_requester_membership() {
    let owner = principal(1);
    let agent = principal(9);
    let outsider = principal(8);
    let policy = CapabilityPolicy::new(
        non_egressing(["buzz.read.current"]),
        non_egressing(["buzz.read.current"]),
    );
    let facts = |members, requesters| DomainFacts {
        realm: realm(),
        channel_id: Uuid::from_u128(1),
        kind: ConversationKind::Restricted,
        epoch: MembershipEpoch::new("membership:v1"),
        members,
        executing_agent: agent.clone(),
        requesters,
        system_principal: None,
        owner: Some(owner.clone()),
    };

    assert_eq!(
        derive_execution_domain(
            facts(
                BTreeSet::from([owner.clone()]),
                BTreeSet::from([owner.clone()]),
            ),
            &policy,
        ),
        Err(DerivationError::AgentNotMember)
    );
    assert_eq!(
        derive_execution_domain(
            facts(
                BTreeSet::from([owner.clone(), agent.clone()]),
                BTreeSet::from([outsider]),
            ),
            &policy,
        ),
        Err(DerivationError::RequesterNotMember)
    );
}

#[test]
fn authority_change_prevents_stale_reuse_reads_and_calls() {
    let stale = conversation(&[1, 2], Uuid::from_u128(1), "v1");
    let current = conversation(&[1, 2], Uuid::from_u128(1), "v2");
    let mut authority = DomainAuthority::new(stale.clone());
    let mut state = ProcessState::default();
    assert!(state.enter(authority.current()).allowed());
    authority.replace(current);

    assert!(!RuleEvaluator::reuse(&stale, authority.current()).allowed());
    assert!(!RuleEvaluator::read(&stale, authority.current(), &stale.resource_label()).allowed());
    assert!(!state
        .authorize_non_egressing_call(authority.current(), "buzz.read.current")
        .allowed());
    assert!(!state.enter(authority.current()).allowed());
}

#[test]
fn domain_id_has_a_canonical_golden_value() {
    let domain = conversation(&[1, 2], Uuid::from_u128(1), "membership:event-1");
    assert_eq!(
        domain.id(),
        "28eb67f09769771c78666c0085468632c500cea713f399a29880f4b6b232ca9d"
    );
}

#[test]
fn domain_shape_and_compartment_profile_are_consistent() {
    let result = ExecutionDomain::new(
        principal(9),
        Some(principal(1)),
        ConfidentialityLabel::public(realm()),
        DomainContext::Conversation {
            realm: realm(),
            channel_id: Uuid::from_u128(1),
        },
        MembershipEpoch::new("v1"),
        CapabilitySet::default(),
    );
    assert_eq!(result, Err(DomainError::AudienceContextMismatch));

    let public = ExecutionDomain::public(
        principal(9),
        Some(principal(1)),
        realm(),
        MembershipEpoch::new("v1"),
        CapabilitySet::default(),
    );
    assert_eq!(
        public.compartment_profile(),
        CompartmentProfile::SharedPublic
    );
    assert_eq!(
        conversation(&[1], Uuid::from_u128(1), "v1").compartment_profile(),
        CompartmentProfile::DomainConfined
    );
}

#[test]
fn publication_target_rejects_incoherent_audience_and_context() {
    let other_realm = RealmId::from_relay_url("wss://other.example").expect("valid URL");
    assert_eq!(
        PublicationTarget::new(
            ConfidentialityLabel::public(realm()),
            DomainContext::RealmPublic(other_realm),
            MembershipEpoch::new("v1"),
        ),
        Err(PublicationTargetError::ContextRealmMismatch)
    );
    assert_eq!(
        PublicationTarget::new(
            ConfidentialityLabel::public(realm()),
            DomainContext::Conversation {
                realm: realm(),
                channel_id: Uuid::from_u128(2),
            },
            MembershipEpoch::new("v1"),
        ),
        Err(PublicationTargetError::AudienceContextMismatch)
    );
}

#[test]
fn post_can_cross_contexts_but_reply_cannot() {
    let domain = conversation(&[1, 2], Uuid::from_u128(1), "v1");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(2), "v1");
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());

    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .is_ok());
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.reply",
            &destination,
            &[7; 32],
            None,
        )
        .is_err());
}

#[test]
fn publication_capability_cannot_use_non_egressing_path_or_widen_audience() {
    let domain = conversation(&[1], Uuid::from_u128(1), "v1");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(2), "target-v1");
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());

    assert!(state
        .authorize_non_egressing_call(authority.current(), "buzz.read.current")
        .allowed());
    assert!(!state
        .authorize_non_egressing_call(authority.current(), "buzz.post")
        .allowed());
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.read.current",
            &destination,
            &[7; 32],
            None,
        )
        .is_err());
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .is_err());
}

#[test]
fn cross_context_or_unknown_input_taints_publication() {
    let domain = conversation(&[1, 2], Uuid::from_u128(1), "v1");
    let destination =
        PublicationTarget::from_current(DomainAuthority::new(domain.clone()).current());
    let (authority, mut state) = process_for(domain);
    let other = ResourceLabel::conversation(
        realm(),
        Uuid::from_u128(2),
        readers(&[1, 2]),
        MembershipEpoch::new("v1"),
    )
    .expect("non-empty readers");
    state.observe(&other);

    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .is_err());
    state.mark_unknown();
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .is_err());
}

#[test]
fn stale_epoch_input_taints_publication_even_after_a_denied_read() {
    let domain = conversation(&[1, 2], Uuid::from_u128(1), "v2");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(1), "v2");
    let stale = ResourceLabel::conversation(
        realm(),
        Uuid::from_u128(1),
        readers(&[1, 2]),
        MembershipEpoch::new("v1"),
    )
    .expect("non-empty readers");
    let (authority, mut state) = process_for(domain.clone());

    assert!(!RuleEvaluator::read(&domain, authority.current(), &stale).allowed());
    state.observe(&stale);
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .is_err());
}

#[test]
fn publication_commit_rejects_operation_destination_and_content_substitution() {
    let domain = conversation(&[1, 2], Uuid::from_u128(1), "v1");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(2), "v1");
    let changed_destination = conversation_target(&[1, 2], Uuid::from_u128(2), "v2");
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());
    let mut replay = MemoryReplayStore::default();

    let permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .expect("authorized publication");
    assert_eq!(
        state.commit_publication(
            permit,
            PublicationCommit::new(
                authority.current(),
                "buzz.reply",
                &destination,
                [7; 32],
                NOW,
            ),
            &mut replay,
        ),
        Err(PublicationCommitError::OperationChanged)
    );

    let permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .expect("authorized publication");
    assert_eq!(
        state.commit_publication(
            permit,
            PublicationCommit::new(
                authority.current(),
                "buzz.post",
                &changed_destination,
                [7; 32],
                NOW,
            ),
            &mut replay,
        ),
        Err(PublicationCommitError::DestinationChanged)
    );

    let permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .expect("authorized publication");
    assert_eq!(
        state.commit_publication(
            permit,
            PublicationCommit::new(authority.current(), "buzz.post", &destination, [8; 32], NOW,),
            &mut replay,
        ),
        Err(PublicationCommitError::ContentChanged)
    );
}

#[test]
fn publication_commit_rejects_source_membership_change() {
    let stale = conversation(&[1, 2], Uuid::from_u128(1), "v1");
    let current = conversation(&[1, 2], Uuid::from_u128(1), "v2");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(2), "v1");
    let (mut authority, mut state) = process_for(stale.clone());
    state.observe(&stale.resource_label());
    let permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .expect("authorized publication");
    authority.replace(current);

    assert_eq!(
        state.commit_publication(
            permit,
            PublicationCommit::new(authority.current(), "buzz.post", &destination, [7; 32], NOW,),
            &mut MemoryReplayStore::default(),
        ),
        Err(PublicationCommitError::SourceChanged)
    );
}

#[test]
fn publication_commit_rejects_new_process_taint() {
    let domain = conversation(&[1, 2], Uuid::from_u128(1), "v1");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(2), "v1");
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());
    let permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[7; 32],
            None,
        )
        .expect("authorized publication");
    state.mark_unknown();

    assert_eq!(
        state.commit_publication(
            permit,
            PublicationCommit::new(authority.current(), "buzz.post", &destination, [7; 32], NOW,),
            &mut MemoryReplayStore::default(),
        ),
        Err(PublicationCommitError::ProcessStateChanged)
    );
}

#[test]
fn canonical_grant_signature_rejects_every_mutable_field() {
    let owner_keys = keys(1);
    let owner = principal(1);
    let source = conversation(&[1], Uuid::from_u128(1), "v1");
    let destination = conversation_target(&[1, 2], Uuid::from_u128(2), "target-v1");
    let original = grant_payload(
        owner.clone(),
        1,
        "buzz.post",
        &source.id(),
        destination.clone(),
        [9; 32],
        EXPIRES_AT,
    );
    let signature = sign_payload(&original, &owner_keys);

    let mutations = [
        grant_payload(
            owner.clone(),
            2,
            "buzz.post",
            &source.id(),
            destination.clone(),
            [9; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.reply",
            &source.id(),
            destination.clone(),
            [9; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.post",
            "different-source",
            destination.clone(),
            [9; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.post",
            &source.id(),
            conversation_target(&[1], Uuid::from_u128(2), "target-v1"),
            [9; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.post",
            &source.id(),
            conversation_target(&[1, 2], Uuid::from_u128(3), "target-v1"),
            [9; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.post",
            &source.id(),
            conversation_target(&[1, 2], Uuid::from_u128(2), "target-v2"),
            [9; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.post",
            &source.id(),
            destination.clone(),
            [8; 32],
            EXPIRES_AT,
        ),
        grant_payload(
            owner.clone(),
            1,
            "buzz.post",
            &source.id(),
            destination.clone(),
            [9; 32],
            EXPIRES_AT + 1,
        ),
    ];
    for mutation in mutations {
        assert!(matches!(
            DeclassificationGrant::new(mutation, signature).verify(NOW),
            Err(GrantError::InvalidSignature)
        ));
    }

    let other_owner = principal(2);
    let changed_approver = grant_payload(
        other_owner.clone(),
        1,
        "buzz.post",
        &source.id(),
        destination,
        [9; 32],
        EXPIRES_AT,
    );
    assert!(matches!(
        DeclassificationGrant::new(changed_approver, signature).verify(NOW),
        Err(GrantError::InvalidSignature)
    ));
}

#[test]
fn declassification_is_exact_and_cannot_cross_realms() {
    let domain = conversation(&[1], Uuid::from_u128(1), "v1");
    let destination = public_target(realm(), "v1");
    let grant = verified_grant(
        1,
        1,
        "buzz.post",
        &domain.id(),
        destination.clone(),
        [9; 32],
    );
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());

    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[8; 32],
            Some(&grant)
        )
        .is_err());
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[9; 32],
            Some(&grant)
        )
        .is_ok());

    let other_realm = RealmId::from_relay_url("wss://other.example").expect("valid URL");
    let cross_realm = public_target(other_realm, "v1");
    let cross_grant = verified_grant(
        1,
        2,
        "buzz.post",
        &domain.id(),
        cross_realm.clone(),
        [9; 32],
    );
    assert!(state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &cross_realm,
            &[9; 32],
            Some(&cross_grant)
        )
        .is_err());
}

#[test]
fn grant_expiration_is_checked_at_verification_and_commit() {
    let owner_keys = keys(1);
    let owner = principal(1);
    let domain = conversation(&[1], Uuid::from_u128(1), "v1");
    let destination = public_target(realm(), "v1");
    let payload = grant_payload(
        owner.clone(),
        1,
        "buzz.post",
        &domain.id(),
        destination.clone(),
        [9; 32],
        EXPIRES_AT,
    );
    let signature = sign_payload(&payload, &owner_keys);
    assert!(matches!(
        DeclassificationGrant::new(payload.clone(), signature).verify(EXPIRES_AT),
        Err(GrantError::Expired)
    ));
    let grant = DeclassificationGrant::new(payload, signature)
        .verify(NOW)
        .expect("not expired yet");
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());
    let permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[9; 32],
            Some(&grant),
        )
        .expect("grant authorizes widening");
    assert_eq!(
        state.commit_publication(
            permit,
            PublicationCommit::new(
                authority.current(),
                "buzz.post",
                &destination,
                [9; 32],
                EXPIRES_AT,
            ),
            &mut MemoryReplayStore::default(),
        ),
        Err(PublicationCommitError::GrantExpired)
    );
}

#[test]
fn reconstructed_grant_is_still_single_use_via_durable_replay_store() {
    let owner_keys = keys(1);
    let owner = principal(1);
    let domain = conversation(&[1], Uuid::from_u128(1), "v1");
    let destination = public_target(realm(), "v1");
    let payload = grant_payload(
        owner.clone(),
        1,
        "buzz.post",
        &domain.id(),
        destination.clone(),
        [9; 32],
        EXPIRES_AT,
    );
    let signature = sign_payload(&payload, &owner_keys);
    let first = DeclassificationGrant::new(payload.clone(), signature)
        .verify(NOW)
        .expect("valid grant");
    let reconstructed = DeclassificationGrant::new(payload, signature)
        .verify(NOW)
        .expect("same valid signed grant");
    let (authority, mut state) = process_for(domain.clone());
    state.observe(&domain.resource_label());
    let first_permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[9; 32],
            Some(&first),
        )
        .expect("first permit");
    let replayed_permit = state
        .authorize_publication(
            authority.current(),
            "buzz.post",
            &destination,
            &[9; 32],
            Some(&reconstructed),
        )
        .expect("authorization is side-effect free");
    let mut replay = MemoryReplayStore::default();

    let authorized = state
        .commit_publication(
            first_permit,
            PublicationCommit::new(authority.current(), "buzz.post", &destination, [9; 32], NOW),
            &mut replay,
        )
        .expect("first durable consumption");
    assert_eq!(authorized.operation(), "buzz.post");
    assert_eq!(authorized.destination(), &destination);
    assert_eq!(authorized.content_digest(), &[9; 32]);
    assert_eq!(
        state.commit_publication(
            replayed_permit,
            PublicationCommit::new(authority.current(), "buzz.post", &destination, [9; 32], NOW,),
            &mut replay,
        ),
        Err(PublicationCommitError::GrantReplayed)
    );
}
