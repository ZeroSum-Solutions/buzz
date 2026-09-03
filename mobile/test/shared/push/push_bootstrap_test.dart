import 'package:buzz/shared/push/dev_push_lease.dart';
import 'package:buzz/shared/community/community.dart';
import 'package:buzz/shared/push/push_bootstrap.dart';
import 'package:buzz/shared/push/push_subscription.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  test('gateway cleanup retries exponentially and then stops', () {
    expect(
      [
        for (var failure = 1; failure <= 7; failure++)
          buzzPushGatewayInitializationRetryDelay(failure),
      ],
      const [
        Duration(seconds: 5),
        Duration(seconds: 10),
        Duration(seconds: 20),
        Duration(seconds: 40),
        Duration(seconds: 80),
        Duration(seconds: 160),
        null,
      ],
    );
    expect(
      () => buzzPushGatewayInitializationRetryDelay(0),
      throwsArgumentError,
    );
  });

  test('failed bootstrap attempt becomes retryable after the delay', () async {
    final gate = BuzzPushAttemptGate(retryDelay: Duration.zero);
    addTearDown(gate.dispose);
    var retries = 0;

    expect(gate.tryBegin('attempt'), isTrue);
    gate.failed('attempt', retry: () => retries += 1);
    await Future<void>.delayed(Duration.zero);

    expect(retries, 1);
    expect(gate.tryBegin('attempt'), isTrue);
  });

  test('a new attempt cancels an obsolete scheduled retry', () async {
    final gate = BuzzPushAttemptGate(retryDelay: Duration.zero);
    addTearDown(gate.dispose);
    var retries = 0;

    expect(gate.tryBegin('old'), isTrue);
    gate.failed('old', retry: () => retries += 1);
    expect(gate.tryBegin('new'), isTrue);
    await Future<void>.delayed(Duration.zero);

    expect(retries, 0);
    expect(gate.tryBegin('new'), isFalse);
  });

  test('successful bootstrap becomes retryable at renewal time', () async {
    final gate = BuzzPushAttemptGate(retryDelay: Duration.zero);
    addTearDown(gate.dispose);
    var retries = 0;

    expect(gate.tryBegin('attempt'), isTrue);
    gate.retryAfter('attempt', delay: Duration.zero, retry: () => retries += 1);
    await Future<void>.delayed(Duration.zero);

    expect(retries, 1);
    expect(gate.tryBegin('attempt'), isTrue);
  });

  test('completed bootstrap attempt can run again for later work', () {
    final gate = BuzzPushAttemptGate();
    addTearDown(gate.dispose);

    expect(gate.tryBegin('attempt'), isTrue);
    gate.complete('attempt');
    expect(gate.tryBegin('attempt'), isTrue);
  });

  test('publication attempt changes when the relay executor rotates', () {
    final subscription = BuzzPushSubscription(
      filter: BuzzPushFilter(kinds: const [9], pTags: [_hex('a')]),
      notificationClass: 'default',
    );
    final original = buzzPushPublicationAttemptKey(
      communityId: 'community',
      relayBaseUrl: 'https://relay.example',
      token: 'token',
      descriptor: _descriptor(keyId: 'relay-v1', pubkey: _hex('b')),
      subscriptions: [subscription],
    );

    expect(
      buzzPushPublicationAttemptKey(
        communityId: 'community',
        relayBaseUrl: 'https://relay.example',
        token: 'token',
        descriptor: _descriptor(keyId: 'relay-v2', pubkey: _hex('b')),
        subscriptions: [subscription],
      ),
      isNot(original),
    );
    expect(
      buzzPushPublicationAttemptKey(
        communityId: 'community',
        relayBaseUrl: 'https://relay.example',
        token: 'token',
        descriptor: _descriptor(keyId: 'relay-v1', pubkey: _hex('c')),
        subscriptions: [subscription],
      ),
      isNot(original),
    );
  });

  test('relay capability alone does not activate push without opt-in', () {
    final disabled = Community.create(
      name: 'Team',
      relayUrl: 'wss://relay.example',
    );
    final enabled = disabled.copyWith(pushNotificationsEnabled: true);

    expect(
      buzzPushLifecycleEnabled(
        community: disabled,
        descriptor: _descriptor(keyId: 'relay-v1', pubkey: _hex('b')),
      ),
      isFalse,
    );
    expect(
      buzzPushLifecycleEnabled(
        community: enabled,
        descriptor: _descriptor(keyId: 'relay-v1', pubkey: _hex('b')),
      ),
      isTrue,
    );
    expect(
      buzzPushLifecycleEnabled(community: enabled, descriptor: null),
      isFalse,
    );
  });

  test('gateway migration includes inactive enabled communities', () {
    final active = Community.create(
      name: 'Active',
      relayUrl: 'https://active.example/path',
    ).copyWith(pushNotificationsEnabled: true);
    final inactive = Community.create(
      name: 'Inactive',
      relayUrl: 'wss://inactive.example',
    ).copyWith(pushNotificationsEnabled: true);
    final disabled = Community.create(
      name: 'Disabled',
      relayUrl: 'wss://disabled.example',
    );

    expect(
      buzzPushCommunitiesRequiringGatewayMigration(
        communities: [active, inactive, disabled],
        retiredRelayOrigins: const {
          'wss://active.example',
          'wss://inactive.example',
          'wss://disabled.example',
        },
        targetGatewayOrigin: 'https://push.example',
      ).map((community) => community.name),
      ['Active', 'Inactive'],
    );
  });

  testWidgets(
    'inactive migration work starts APNs registration through production boundary',
    (tester) async {
      final inactive = Community.create(
        name: 'Inactive',
        relayUrl: 'wss://inactive.example',
      ).copyWith(pushNotificationsEnabled: true);
      final migrationCommunities = buzzPushCommunitiesRequiringGatewayMigration(
        communities: [inactive],
        retiredRelayOrigins: const {'wss://inactive.example'},
        targetGatewayOrigin: 'https://push.example',
      );
      var registrations = 0;

      await tester.pumpWidget(
        MaterialApp(
          home: BuzzPushRegistrationBootstrap(
            shouldRegister: migrationCommunities.isNotEmpty,
            attemptKey: 'migration:${inactive.id}',
            startRegistration: () async => registrations += 1,
            child: const SizedBox(),
          ),
        ),
      );
      await tester.pump();

      expect(registrations, 1);
    },
  );

  test('gateway migration skips a durably checkpointed replacement', () {
    final community =
        Community.create(
          name: 'Migrated',
          relayUrl: 'wss://relay.example',
        ).copyWith(
          pushNotificationsEnabled: true,
          pushSubscriptionState: BuzzPushLeaseSubscriptionState.accepted(
            desired: const [],
            acceptedSubscriptions: const [],
            acceptedGeneration: 2,
            acceptedGatewayOrigin: 'https://push.example',
          ),
        );

    expect(
      buzzPushCommunitiesRequiringGatewayMigration(
        communities: [community],
        retiredRelayOrigins: const {'wss://relay.example'},
        targetGatewayOrigin: 'https://push.example',
      ),
      isEmpty,
    );
  });

  test('same-gateway rotation forces a durably checkpointed replacement', () {
    final community =
        Community.create(
          name: 'Rotated',
          relayUrl: 'wss://relay.example',
        ).copyWith(
          pushNotificationsEnabled: true,
          pushSubscriptionState: BuzzPushLeaseSubscriptionState.accepted(
            desired: const [],
            acceptedSubscriptions: const [],
            acceptedGeneration: 2,
            acceptedGatewayOrigin: 'https://push.example',
          ),
        );

    expect(
      buzzPushCommunitiesRequiringGatewayMigration(
        communities: [community],
        retiredRelayOrigins: const {'wss://relay.example'},
        replacementRelayOrigins: const {'wss://relay.example'},
        targetGatewayOrigin: 'https://push.example',
      ),
      [community],
    );
  });

  test('pending opt-out tombstone keeps active push lifecycle disabled', () {
    final subscription = BuzzPushSubscription(
      filter: BuzzPushFilter(kinds: const [9], pTags: [_hex('a')]),
      notificationClass: 'default',
    );
    final community =
        Community.create(
          name: 'Team',
          relayUrl: 'wss://relay.example',
        ).copyWith(
          pushNotificationsEnabled: false,
          pushSubscriptionState:
              BuzzPushLeaseSubscriptionState.desired(desired: [subscription])
                  .withAccepted(subscriptions: [subscription], generation: 3)
                  .withPendingTombstone(4),
        );

    expect(
      buzzPushLifecycleEnabled(
        community: community,
        descriptor: _descriptor(keyId: 'relay-v1', pubkey: _hex('b')),
      ),
      isFalse,
    );
  });

  test(
    'relay commit followed by local failure retries at a newer generation',
    () async {
      var durableCursor = 0;
      var relayGeneration = 0;
      var acceptedGeneration = 0;
      var failLocalSave = true;

      Future<int> reserve() async => ++durableCursor;
      Future<void> publish(int generation) async {
        expect(generation, greaterThan(relayGeneration));
        relayGeneration = generation;
      }

      Future<void> markAccepted(int generation) async {
        if (failLocalSave) {
          failLocalSave = false;
          throw StateError('injected local persistence failure');
        }
        acceptedGeneration = generation;
      }

      await expectLater(
        publishBuzzPushLeaseRecoverably(
          reserveGeneration: reserve,
          publish: publish,
          markAccepted: markAccepted,
        ),
        throwsStateError,
      );
      expect(relayGeneration, 1);
      expect(acceptedGeneration, 0);

      await publishBuzzPushLeaseRecoverably(
        reserveGeneration: reserve,
        publish: publish,
        markAccepted: markAccepted,
      );
      expect(relayGeneration, 2);
      expect(acceptedGeneration, 2);
    },
  );
}

BuzzPushLeaseDescriptor _descriptor({
  required String keyId,
  required String pubkey,
}) => BuzzPushLeaseDescriptor(
  origin: 'wss://relay.example',
  executorKeyId: keyId,
  executorPubkey: pubkey,
  transport: 'apns',
  maxLeaseTtlSeconds: 3600,
  maxContentLength: 4096,
  maxPlaintextLength: 4096,
  maxEndpointLength: 2048,
  maxStringLength: 512,
);

String _hex(String character) => List.filled(64, character).join();
