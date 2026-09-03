import Foundation

/// Gateway-neutral recovery material retained by the pre-gateway-origin schema.
public struct BuzzPushLegacyRecoveryInventory: Equatable {
  public let relayOrigins: [String]
  public let endpointGrants: [String]

  public init(relayOrigins: [String], endpointGrants: [String]) {
    self.relayOrigins = relayOrigins
    self.endpointGrants = endpointGrants
  }

  private struct LegacyGrant: Decodable {
    let relayOrigin: String
    let endpointGrant: String
  }

  private struct LegacyPending: Decodable {
    let relayOrigin: String
  }

  /// Extracts only gateway-neutral relay origins and opaque gateway proofs.
  /// No gateway origin or App Attest key is inferred for legacy records.
  public static func decode(grants: Data?, pending: Data?) throws -> Self {
    let legacyGrants =
      try grants.map {
        try JSONDecoder().decode([LegacyGrant].self, from: $0)
      } ?? []
    let legacyPending =
      try pending.map {
        try JSONDecoder().decode([LegacyPending].self, from: $0)
      } ?? []
    let relayOrigins = try (legacyGrants.map(\.relayOrigin) + legacyPending.map(\.relayOrigin))
      .map { origin -> String in
        guard origin.utf8.count <= 2_048,
          var components = URLComponents(string: origin),
          components.host?.isEmpty == false,
          components.user == nil,
          components.password == nil,
          components.query == nil,
          components.fragment == nil,
          components.path.isEmpty || components.path == "/"
        else {
          throw CocoaError(.coderInvalidValue)
        }
        switch components.scheme?.lowercased() {
        case "https": components.scheme = "wss"
        case "http": components.scheme = "ws"
        case "wss", "ws": break
        default: throw CocoaError(.coderInvalidValue)
        }
        components.path = ""
        guard let canonical = components.string, canonical.utf8.count <= 2_048 else {
          throw CocoaError(.coderInvalidValue)
        }
        return canonical
      }
    let endpointGrants = legacyGrants.map(\.endpointGrant)
    guard endpointGrants.allSatisfy({ !$0.isEmpty && $0.utf8.count <= 4_096 }) else {
      throw CocoaError(.coderInvalidValue)
    }
    return Self(
      relayOrigins: Array(Set(relayOrigins)).sorted(),
      endpointGrants: Array(Set(endpointGrants)).sorted()
    )
  }
}
