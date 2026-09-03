use std::collections::BTreeSet;

use nostr::secp256k1::Message;
use nostr::Keys;

use crate::{
    DeclassificationGrant, DeclassificationGrantPayload, GrantId, GrantReplayStore, Principal,
    PublicationTarget, VerifiedDeclassificationGrant,
};

pub(crate) const NOW: u64 = 1_000;
pub(crate) const EXPIRES_AT: u64 = 2_000;

pub(crate) fn keys(value: u8) -> Keys {
    Keys::parse(&format!("{value:064x}")).expect("test secret key")
}

pub(crate) fn principal(value: u8) -> Principal {
    Principal::from_public_key(&keys(value).public_key()).expect("test principal")
}

pub(crate) fn grant_payload(
    approver: Principal,
    nonce: u8,
    operation: &str,
    source_domain_id: &str,
    destination: PublicationTarget,
    content_digest: [u8; 32],
    expires_at: u64,
) -> DeclassificationGrantPayload {
    DeclassificationGrantPayload::new(
        approver,
        GrantId::from_bytes([nonce; 32]),
        operation,
        source_domain_id,
        destination,
        content_digest,
        expires_at,
    )
}

pub(crate) fn sign_payload(payload: &DeclassificationGrantPayload, signer: &Keys) -> [u8; 64] {
    signer
        .sign_schnorr(&Message::from_digest(payload.signing_digest()))
        .serialize()
}

pub(crate) fn verified_grant(
    owner_value: u8,
    nonce: u8,
    operation: &str,
    source_domain_id: &str,
    destination: PublicationTarget,
    content_digest: [u8; 32],
) -> VerifiedDeclassificationGrant {
    let owner_keys = keys(owner_value);
    let owner = principal(owner_value);
    let payload = grant_payload(
        owner.clone(),
        nonce,
        operation,
        source_domain_id,
        destination,
        content_digest,
        EXPIRES_AT,
    );
    let signature = sign_payload(&payload, &owner_keys);
    DeclassificationGrant::new(payload, signature)
        .verify(NOW)
        .expect("valid owner grant")
}

#[derive(Default)]
pub(crate) struct MemoryReplayStore(BTreeSet<[u8; 32]>);

impl GrantReplayStore for MemoryReplayStore {
    fn consume_if_unused(&mut self, grant_digest: &[u8; 32], _expires_at: u64) -> bool {
        self.0.insert(*grant_digest)
    }
}
