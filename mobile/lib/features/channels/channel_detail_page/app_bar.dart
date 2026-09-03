part of '../channel_detail_page.dart';

const _dmHeaderAvatarSize = 32.0;
const _channelHeaderAvatarSize = 40.0;
const _iosChannelHeaderControlHeight = buzzNavigationActionSize;
const _iosChannelHeaderIconSize = 12.0;
const _iosChannelHeaderLeadingInset = 12.0;
const _iosChannelHeaderTrailingInset = 16.0;
const _iosChannelHeaderIconSpacing = 8.0;
const _dmPresenceDotRatio = 8 / 14;

bool _showsMembersAction(Channel channel) {
  if (!channel.isDm) return true;
  final participants = channel.participantPubkeys
      .map((pubkey) => pubkey.toLowerCase())
      .toSet();
  return participants.length != 2;
}

double _scaledTextHeight(BuildContext context, TextStyle style) {
  final scaledFontSize = MediaQuery.textScalerOf(
    context,
  ).scale(style.fontSize ?? 0);
  return scaledFontSize * (style.height ?? 1);
}

double _twoLineAppBarTitleContentHeight(
  BuildContext context, {
  required bool isDm,
}) {
  final titleStyle = context.textTheme.titleSmall;
  final subtitleStyle = isDm
      ? context.textTheme.bodyMedium
      : context.textTheme.bodySmall;
  final avatarSize = isDm ? _dmHeaderAvatarSize : _channelHeaderAvatarSize;
  if (titleStyle == null || subtitleStyle == null) {
    return avatarSize;
  }
  final textHeight =
      _scaledTextHeight(context, titleStyle) +
      _scaledTextHeight(context, subtitleStyle);
  return textHeight > avatarSize ? textHeight : avatarSize;
}

class _ChannelAppBarTitle extends ConsumerWidget {
  const _ChannelAppBarTitle({
    required this.channel,
    required this.onTap,
    this.nativeViewSuppressed,
  });

  final Channel channel;
  final VoidCallback onTap;
  final ValueListenable<bool>? nativeViewSuppressed;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final membersAsync = ref.watch(channelMembersProvider(channel.id));
    final memberCount = membersAsync.value?.length ?? channel.memberCount;
    final memberLabel =
        '$memberCount ${memberCount == 1 ? 'member' : 'members'}';

    if (defaultTargetPlatform == TargetPlatform.iOS) {
      final systemIconName = channel.isPrivate
          ? 'lock.fill'
          : channel.isForum
          ? 'bubble.left.and.bubble.right.fill'
          : 'number';
      return LayoutBuilder(
        builder: (context, constraints) {
          double measure(String text, TextStyle? style) {
            final painter = TextPainter(
              text: TextSpan(text: text, style: style),
              maxLines: 1,
              textDirection: Directionality.of(context),
              textScaler: MediaQuery.textScalerOf(context),
            )..layout();
            return painter.width;
          }

          final textWidth = max(
            measure(
              channel.name,
              context.textTheme.titleSmall?.copyWith(
                fontWeight: FontWeight.w600,
              ),
            ),
            measure(memberLabel, context.textTheme.bodySmall),
          );
          final naturalWidth =
              _iosChannelHeaderLeadingInset +
              _iosChannelHeaderTrailingInset +
              _iosChannelHeaderIconSize +
              _iosChannelHeaderIconSpacing +
              textWidth;
          final controlWidth = min(naturalWidth, constraints.maxWidth);
          return Align(
            alignment: Alignment.centerLeft,
            child: IosGlassNavigationButton(
              key: const ValueKey('channel-header-settings-trigger'),
              icon: IosGlassNavigationIcon.channel,
              label: channel.name,
              subtitle: memberLabel,
              systemIconName: systemIconName,
              semanticLabel: 'Open settings for ${channel.name}, $memberLabel',
              onPressed: onTap,
              width: controlWidth,
              height: constraints.maxHeight,
              controlSize: _iosChannelHeaderControlHeight,
              fillWidth: true,
              foregroundColor: context.colors.primary,
              nativeViewSuppressed: nativeViewSuppressed,
            ),
          );
        },
      );
    }

    return Semantics(
      button: true,
      label: 'Open settings for ${channel.name}, $memberLabel',
      child: Tooltip(
        message: 'Open channel settings',
        child: InkWell(
          key: const ValueKey('channel-header-settings-trigger'),
          borderRadius: BorderRadius.circular(Radii.md),
          onTap: onTap,
          child: Row(
            children: [
              Container(
                key: const ValueKey('channel-header-avatar'),
                width: _channelHeaderAvatarSize,
                height: _channelHeaderAvatarSize,
                decoration: BoxDecoration(
                  color: context.colors.surface,
                  shape: BoxShape.circle,
                  border: Border.fromBorderSide(
                    BorderSide(
                      color: context.colors.inverseSurface.withValues(
                        alpha: 0.07,
                      ),
                      strokeAlign: BorderSide.strokeAlignOutside,
                    ),
                  ),
                ),
                child: Icon(
                  channelIcon(channel),
                  size: 20,
                  color: context.colors.primary,
                ),
              ),
              const SizedBox(width: Grid.twelve),
              Expanded(
                child: ConstrainedBox(
                  key: const ValueKey('channel-header-text-stack'),
                  constraints: const BoxConstraints(
                    minHeight: _channelHeaderAvatarSize,
                  ),
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    crossAxisAlignment: CrossAxisAlignment.start,
                    mainAxisAlignment: MainAxisAlignment.center,
                    children: [
                      Row(
                        mainAxisSize: MainAxisSize.min,
                        children: [
                          Flexible(
                            child: Text(
                              channel.name,
                              key: const ValueKey('channel-header-name'),
                              maxLines: 1,
                              overflow: TextOverflow.ellipsis,
                              style: context.textTheme.titleSmall?.copyWith(
                                fontWeight: FontWeight.w600,
                              ),
                            ),
                          ),
                          if (channel.isEphemeral) ...[
                            const SizedBox(width: Grid.quarter),
                            _HeaderEphemeralBadge(channel: channel),
                          ],
                        ],
                      ),
                      Text(
                        memberLabel,
                        key: const ValueKey('channel-header-member-count'),
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: context.textTheme.bodySmall?.copyWith(
                          color: context.colors.onSurface.withValues(
                            alpha: 0.65,
                          ),
                        ),
                      ),
                    ],
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _MembersButton extends ConsumerWidget {
  final String channelId;
  final Channel channel;
  final String? currentPubkey;
  final ValueListenable<bool>? nativeViewSuppressed;

  const _MembersButton({
    required this.channelId,
    required this.channel,
    required this.currentPubkey,
    this.nativeViewSuppressed,
  });

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final hasWorkingBot = ref
        .watch(workingBotPubkeysProvider(channelId))
        .isNotEmpty;

    void showMembers() {
      showBuzzModalBottomSheet<void>(
        context: context,
        title: 'Members',
        isScrollControlled: true,
        showDragHandle: true,
        builder: (_) =>
            MembersSheet(channel: channel, currentPubkey: currentPubkey),
      );
    }

    if (defaultTargetPlatform == TargetPlatform.iOS) {
      return IosGlassNavigationButton(
        key: const ValueKey('channel-members-button'),
        icon: IosGlassNavigationIcon.users,
        semanticLabel: 'View members',
        onPressed: showMembers,
        controlSize: buzzNavigationActionSize,
        foregroundColor: context.colors.primary,
        nativeViewSuppressed: nativeViewSuppressed,
      );
    }

    return IconButton(
      key: const ValueKey('channel-members-button'),
      color: context.colors.primary,
      onPressed: showMembers,
      tooltip: 'View members',
      icon: Stack(
        clipBehavior: Clip.none,
        children: [
          const Icon(LucideIcons.users, size: 22),
          if (hasWorkingBot)
            Positioned(
              top: -2,
              right: -2,
              child: Container(
                width: 8,
                height: 8,
                decoration: BoxDecoration(
                  color: context.appColors.success,
                  shape: BoxShape.circle,
                  border: Border.all(color: context.colors.surface, width: 1.5),
                ),
              ),
            ),
        ],
      ),
    );
  }
}

class _ChannelActionsButton extends ConsumerWidget {
  const _ChannelActionsButton({
    required this.channel,
    this.nativeViewSuppressed,
  });

  final Channel channel;
  final ValueListenable<bool>? nativeViewSuppressed;

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    Future<void> showActions() async {
      final shouldClose = await showChannelActionsSheet(
        context: context,
        channel: channel,
        isUnread: false,
        sectionId: ref
            .read(channelSectionsProvider)
            .store
            .assignments[channel.id],
      );
      if (shouldClose == true && context.mounted) {
        Navigator.of(context).pop();
      }
    }

    if (defaultTargetPlatform == TargetPlatform.iOS) {
      return IosGlassNavigationButton(
        key: const ValueKey('channel-actions-button'),
        icon: IosGlassNavigationIcon.more,
        semanticLabel: 'Channel actions',
        onPressed: showActions,
        controlSize: buzzNavigationActionSize,
        foregroundColor: context.colors.primary,
        nativeViewSuppressed: nativeViewSuppressed,
      );
    }

    return IconButton(
      key: const ValueKey('channel-actions-button'),
      color: context.colors.primary,
      onPressed: showActions,
      tooltip: 'Channel actions',
      icon: const Icon(LucideIcons.ellipsisVertical, size: 22),
    );
  }
}

class _DmAppBarTitle extends ConsumerWidget {
  final Channel channel;
  final String? currentPubkey;
  final ValueListenable<bool>? nativeViewSuppressed;

  const _DmAppBarTitle({
    required this.channel,
    required this.currentPubkey,
    this.nativeViewSuppressed,
  });

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final normalizedCurrent = currentPubkey?.toLowerCase();

    String? otherPubkey;
    for (final pk in channel.participantPubkeys) {
      if (pk.toLowerCase() != normalizedCurrent) {
        otherPubkey = pk.toLowerCase();
        break;
      }
    }

    final profile = ref.watch(
      userCacheProvider.select(
        (profiles) => otherPubkey == null ? null : profiles[otherPubkey],
      ),
    );
    final presence = ref.watch(
      presenceCacheProvider.select(
        (presenceMap) => otherPubkey == null
            ? 'offline'
            : (presenceMap[otherPubkey] ?? 'offline'),
      ),
    );

    if (otherPubkey != null) {
      if (profile == null) {
        ref.read(userCacheProvider.notifier).preload([otherPubkey]);
      }
      ref.read(presenceCacheProvider.notifier).track([otherPubkey]);
    }

    final avatarUrl = profile?.avatarUrl;
    final isAgent =
        (otherPubkey != null &&
            ref
                .watch(agentMentionPubkeysProvider(channel.id))
                .contains(otherPubkey)) ||
        profile?.ownerPubkey != null;
    final animatedAvatar = parseAnimatedAvatarUrl(avatarUrl);
    final initial =
        profile?.initial ??
        (channel.participants.isNotEmpty
            ? channel.participants.first[0].toUpperCase()
            : '?');
    final presenceLabel = switch (presence) {
      'online' => 'Online',
      'away' => 'Away',
      _ => 'Offline',
    };
    final displayLabel = resolveDmChannelDisplayLabel(
      channel,
      currentPubkey: currentPubkey,
    );

    if (defaultTargetPlatform == TargetPlatform.iOS) {
      void openIdentity() {
        if (channel.participantPubkeys.length == 2 && otherPubkey != null) {
          showUserProfileSheet(context, otherPubkey);
          return;
        }
        showBuzzModalBottomSheet<void>(
          context: context,
          title: 'Members',
          isScrollControlled: true,
          showDragHandle: true,
          builder: (_) =>
              MembersSheet(channel: channel, currentPubkey: currentPubkey),
        );
      }

      return LayoutBuilder(
        builder: (context, constraints) {
          double measure(String text, TextStyle? style) {
            final painter = TextPainter(
              text: TextSpan(text: text, style: style),
              maxLines: 1,
              textDirection: Directionality.of(context),
              textScaler: MediaQuery.textScalerOf(context),
            )..layout();
            return painter.width;
          }

          final textWidth = max(
            measure(displayLabel, context.textTheme.titleMedium),
            measure(presenceLabel, context.textTheme.bodySmall),
          );
          final naturalWidth = 54.0 + textWidth;
          return Align(
            alignment: Alignment.centerLeft,
            child: IosGlassNavigationButton(
              key: const ValueKey('dm-header-glass-trigger'),
              icon: IosGlassNavigationIcon.avatar,
              label: displayLabel,
              subtitle: presenceLabel,
              semanticLabel: '$displayLabel, $presenceLabel',
              onPressed: openIdentity,
              width: min(naturalWidth, constraints.maxWidth),
              height: constraints.maxHeight,
              controlSize: buzzNavigationActionSize,
              fillWidth: true,
              foregroundColor: context.colors.primary,
              avatarImageUrl: animatedAvatar?.posterUrl ?? avatarUrl,
              avatarFallback: initial,
              nativeViewSuppressed: nativeViewSuppressed,
            ),
          );
        },
      );
    }

    return Row(
      children: [
        MaskedAvatarBadge(
          key: const ValueKey('dm-header-avatar'),
          size: _dmHeaderAvatarSize,
          geometry: AvatarBadgeMaskGeometry.presenceDot,
          avatar: ClipRRect(
            borderRadius: BorderRadius.circular(
              isAgent ? _dmHeaderAvatarSize * 0.3 : _dmHeaderAvatarSize / 2,
            ),
            child: ColoredBox(
              color: animatedAvatar == null
                  ? context.colors.primaryContainer
                  : Colors.transparent,
              child: AvatarImageContent(
                imageUrl: animatedAvatar?.posterUrl ?? avatarUrl,
                fallback: Text(
                  initial,
                  style: context.textTheme.labelSmall?.copyWith(
                    color: context.colors.onPrimaryContainer,
                    fontWeight: FontWeight.w600,
                  ),
                ),
              ),
            ),
          ),
          badge: Center(
            child: FractionallySizedBox(
              widthFactor: _dmPresenceDotRatio,
              heightFactor: _dmPresenceDotRatio,
              child: DecoratedBox(
                decoration: BoxDecoration(
                  color: switch (presence) {
                    'online' => context.appColors.success,
                    'away' => context.appColors.warning,
                    _ => context.colors.outline,
                  },
                  shape: BoxShape.circle,
                ),
              ),
            ),
          ),
        ),
        const SizedBox(width: Grid.xxs),
        Expanded(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            mainAxisSize: MainAxisSize.min,
            children: [
              Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  Flexible(
                    child: Text(
                      displayLabel,
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      key: const ValueKey('dm-header-name'),
                      style: context.textTheme.titleSmall,
                    ),
                  ),
                  if (channel.isEphemeral) ...[
                    const SizedBox(width: Grid.quarter),
                    _HeaderEphemeralBadge(channel: channel),
                  ],
                ],
              ),
              Text(
                presenceLabel,
                key: const ValueKey('dm-header-presence'),
                style: context.textTheme.bodyMedium?.copyWith(
                  color: context.colors.onSurfaceVariant,
                ),
              ),
            ],
          ),
        ),
      ],
    );
  }
}
