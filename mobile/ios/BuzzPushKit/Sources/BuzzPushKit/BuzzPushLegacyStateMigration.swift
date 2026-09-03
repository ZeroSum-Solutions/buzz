import Foundation

/// Decodes the pre-gateway-origin Keychain schema using the artifact's configured gateway.
public enum BuzzPushLegacyStateMigration {
  private struct LegacyGrant: Decodable {
    let relayOrigin: String
    let relayPubkey: String
    let relayMetadataPubkey: String?
    let gatewayInstallationHandle: String?
    let installationId: String
    let endpointGrant: String
    let endpointHash: String
    let appProfile: String
    let endpointEpoch: Int64
    let generation: Int64
    let expiresAt: Int64
  }

  private struct LegacyPending: Decodable {
    let relayOrigin: String
    let relayPubkey: String
    let endpointHash: String
    let appProfile: String
    let expiresAt: Int64
    let installationId: String
    let gatewayInstallationHandle: String?
    let challengeId: String?
    let challenge: String?
    let keyId: String?
    let attestation: String?
    let delegationGeneration: Int64
  }

  /// Migrates legacy opaque grants under the gateway configured by the upgrading artifact.
  public static func grants(
    from data: Data,
    gatewayOrigin: String,
    appAttestKeyId: String
  ) throws -> [BuzzPushEndpointGrantRecord] {
    try JSONDecoder().decode([LegacyGrant].self, from: data).map { record in
      BuzzPushEndpointGrantRecord(
        gatewayOrigin: gatewayOrigin,
        relayOrigin: record.relayOrigin,
        relayPubkey: record.relayPubkey,
        relayMetadataPubkey: record.relayMetadataPubkey,
        gatewayInstallationHandle: record.gatewayInstallationHandle,
        appAttestKeyId: appAttestKeyId,
        installationId: record.installationId,
        endpointGrant: record.endpointGrant,
        endpointHash: record.endpointHash,
        appProfile: record.appProfile,
        endpointEpoch: record.endpointEpoch,
        generation: record.generation,
        expiresAt: record.expiresAt
      )
    }
  }

  /// Migrates legacy response-loss journals for later replay or cleanup.
  public static func pendingEnrollments(
    from data: Data,
    gatewayOrigin: String
  ) throws -> [BuzzPushPendingEnrollmentRecord] {
    try JSONDecoder().decode([LegacyPending].self, from: data).map { pending in
      BuzzPushPendingEnrollmentRecord(
        gatewayOrigin: gatewayOrigin,
        relayOrigin: pending.relayOrigin,
        relayPubkey: pending.relayPubkey,
        endpointHash: pending.endpointHash,
        appProfile: pending.appProfile,
        expiresAt: pending.expiresAt,
        installationId: pending.installationId,
        gatewayInstallationHandle: pending.gatewayInstallationHandle,
        challengeId: pending.challengeId,
        challenge: pending.challenge,
        keyId: pending.keyId,
        attestation: pending.attestation,
        delegationGeneration: pending.delegationGeneration
      )
    }
  }
}
