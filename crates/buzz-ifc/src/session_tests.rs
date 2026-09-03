use std::collections::BTreeSet;

use uuid::Uuid;

use crate::test_support::{principal, verified_grant, MemoryReplayStore, NOW};
use crate::*;

fn realm() -> RealmId {
    RealmId::from_relay_url("wss://buzz.example").expect("canonical test realm")
}

fn readers(values: &[u8]) -> BTreeSet<Principal> {
    values.iter().copied().map(principal).collect()
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

fn domain(owner: u8, audience: &[u8], channel: u128, epoch: &str) -> ExecutionDomain {
    ExecutionDomain::new(
        principal(9),
        Some(principal(owner)),
        ConfidentialityLabel::restricted(realm(), readers(audience)).expect("non-empty audience"),
        DomainContext::Conversation {
            realm: realm(),
            channel_id: Uuid::from_u128(channel),
        },
        MembershipEpoch::new(epoch),
        capabilities(),
    )
    .expect("coherent domain")
}

fn public_target(epoch: &str) -> PublicationTarget {
    PublicationTarget::new(
        ConfidentialityLabel::public(realm()),
        DomainContext::RealmPublic(realm()),
        MembershipEpoch::new(epoch),
    )
    .expect("coherent public target")
}

#[test]
fn broker_workflow_is_session_read_call_publish() {
    let source = domain(1, &[1, 2], 1, "v1");
    let input = source.resource_label();
    let mut session = IfcSession::enter(source);

    session.read(&input).expect("input is readable");
    assert!(session.has_observed_input());
    session
        .call("buzz.read.current")
        .expect("non-egressing capability is allowed");
    assert!(session.call("buzz.post").is_err());

    let destination = session.current_target();
    let content = b"same-context reply";
    let authorization = session
        .publish(
            PublicationRequest::new("buzz.reply", &destination, content, NOW),
            None,
            &mut MemoryReplayStore::default(),
        )
        .expect("same-context publication is allowed");

    assert_eq!(authorization.operation(), "buzz.reply");
    assert_eq!(authorization.destination(), &destination);
    assert_eq!(authorization.content(), content);
    assert_eq!(authorization.content_digest(), &publication_digest(content));
}

#[test]
fn domain_refresh_permanently_invalidates_a_changed_session() {
    let original = domain(1, &[1, 2], 1, "v1");
    let original_input = original.resource_label();
    let changed = domain(1, &[1], 1, "v2");
    let mut session = IfcSession::enter(original.clone());

    assert_eq!(
        session.refresh(changed),
        Err(IfcError::Denied(
            "agent process has already entered a different domain"
        ))
    );
    assert!(session.refresh(original).is_err());
    assert!(session.read(&original_input).is_err());
    assert!(session.call("buzz.read.current").is_err());
}

#[test]
fn supplied_grant_must_match_even_when_ordinary_flow_would_allow() {
    let source = domain(1, &[1, 2], 1, "v1");
    let source_id = source.id();
    let input = source.resource_label();
    let mut session = IfcSession::enter(source);
    session.read(&input).expect("input is readable");
    let destination = session.current_target();
    let signed_content = b"approved output";
    let presented_content = b"different output";
    let grant = verified_grant(
        1,
        1,
        "buzz.reply",
        &source_id,
        destination.clone(),
        publication_digest(signed_content),
    );

    assert_eq!(
        session.publish(
            PublicationRequest::new("buzz.reply", &destination, presented_content, NOW),
            Some(&grant),
            &mut MemoryReplayStore::default(),
        ),
        Err(IfcError::Denied(
            "declassification grant does not match this publication"
        ))
    );
}

#[test]
fn only_the_domain_owner_can_declassify() {
    let source = domain(1, &[1], 1, "v1");
    let source_id = source.id();
    let input = source.resource_label();
    let mut session = IfcSession::enter(source);
    session.read(&input).expect("input is readable");
    let destination = public_target("v1");
    let content = b"owner-approved output";
    let requester_grant = verified_grant(
        2,
        1,
        "buzz.post",
        &source_id,
        destination.clone(),
        publication_digest(content),
    );

    assert_eq!(
        session.publish(
            PublicationRequest::new("buzz.post", &destination, content, NOW),
            Some(&requester_grant),
            &mut MemoryReplayStore::default(),
        ),
        Err(IfcError::Denied(
            "declassification grant was not approved by the agent owner"
        ))
    );
}

#[test]
fn replay_consumption_is_scoped_to_the_complete_signed_grant() {
    let first_source = domain(1, &[1], 1, "v1");
    let second_source = domain(2, &[2], 2, "v1");
    let first_input = first_source.resource_label();
    let second_input = second_source.resource_label();
    let first_source_id = first_source.id();
    let second_source_id = second_source.id();
    let mut first_session = IfcSession::enter(first_source);
    let mut second_session = IfcSession::enter(second_source);
    first_session
        .read(&first_input)
        .expect("first input is readable");
    second_session
        .read(&second_input)
        .expect("second input is readable");
    let destination = public_target("v1");
    let content = b"independently approved output";
    let first_grant = verified_grant(
        1,
        7,
        "buzz.post",
        &first_source_id,
        destination.clone(),
        publication_digest(content),
    );
    let second_grant = verified_grant(
        2,
        7,
        "buzz.post",
        &second_source_id,
        destination.clone(),
        publication_digest(content),
    );
    let mut replay = MemoryReplayStore::default();

    let first_authorization = first_session
        .publish(
            PublicationRequest::new("buzz.post", &destination, content, NOW),
            Some(&first_grant),
            &mut replay,
        )
        .expect("first signed grant is unused");
    assert_eq!(first_authorization.source_domain_id(), first_source_id);
    let second_authorization = second_session
        .publish(
            PublicationRequest::new("buzz.post", &destination, content, NOW),
            Some(&second_grant),
            &mut replay,
        )
        .expect("a different signed grant may reuse the nonce");
    assert_eq!(second_authorization.source_domain_id(), second_source_id);
    assert_eq!(
        first_session.publish(
            PublicationRequest::new("buzz.post", &destination, content, NOW),
            Some(&first_grant),
            &mut replay,
        ),
        Err(IfcError::GrantReplayed)
    );
}

#[test]
fn unknown_input_permanently_blocks_publication() {
    let source = domain(1, &[1], 1, "v1");
    let input = source.resource_label();
    let mut session = IfcSession::enter(source);
    session.read(&input).expect("input is readable");
    session.mark_unknown_input();
    let destination = session.current_target();
    let content = b"blocked output";

    assert!(session
        .publish(
            PublicationRequest::new("buzz.reply", &destination, content, NOW),
            None,
            &mut MemoryReplayStore::default(),
        )
        .is_err());
}
