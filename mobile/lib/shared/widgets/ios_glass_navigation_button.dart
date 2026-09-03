import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter/rendering.dart' show PlatformViewHitTestBehavior;
import 'package:flutter/services.dart';
import 'package:flutter_hooks/flutter_hooks.dart';

import '../theme/theme.dart';
import 'avatar_image.dart';

/// The navigation glyph displayed by [IosGlassNavigationButton].
enum IosGlassNavigationIcon {
  back,
  close,
  camera,
  photoLibrary,
  palette,
  droplet,
  emoji,
  person,
  frame,
  rotateCamera,
  shutter,
  colorSwatch,
  sun,
  moon,
  systemAppearance,
  avatar,
  channel,
  headphones,
}

/// A native contextual-menu item attached to an iOS glass navigation button.
class IosGlassNavigationMenuItem {
  const IosGlassNavigationMenuItem({
    required this.id,
    required this.label,
    this.selected = false,
    this.destructive = false,
    this.avatarImageUrl,
    this.avatarFallback,
    this.systemIconName,
  });

  final String id;
  final String label;
  final bool selected;
  final bool destructive;
  final String? avatarImageUrl;
  final String? avatarFallback;
  final String? systemIconName;

  Map<String, Object> toJson() {
    final json = <String, Object>{
      'id': id,
      'label': label,
      'selected': selected,
      'destructive': destructive,
    };
    if (avatarImageUrl != null) json['avatarImageUrl'] = avatarImageUrl!;
    if (avatarFallback != null) json['avatarFallback'] = avatarFallback!;
    if (systemIconName != null) json['systemIconName'] = systemIconName!;
    return json;
  }
}

/// Leading width used by iOS channel-style headers.
const iosGlassChannelHeaderLeadingWidth = 58.0;

/// Horizontal center of the native button inside a channel-style leading.
const iosGlassChannelHeaderButtonCenterX = 38.0;

/// Space between the leading region and a channel-style title.
const iosGlassChannelHeaderTitleSpacing = Grid.xs;

/// A native iOS navigation control using the system glass button treatment.
///
/// Callers should only insert this widget on iOS and retain their existing
/// Flutter control on other platforms.
class IosGlassNavigationButton extends HookWidget {
  const IosGlassNavigationButton({
    super.key,
    required this.icon,
    required this.semanticLabel,
    required this.onPressed,
    this.label,
    this.subtitle,
    this.width = 48,
    this.height = 48,
    this.controlSize = 40,
    this.fillWidth = false,
    this.buttonCenterX,
    this.foregroundColor,
    this.swatchColor,
    this.isBusy = false,
    this.isSelected = false,
    this.nativeViewSuppressed,
    this.avatarImageUrl,
    this.avatarFallback,
    this.systemIconName,
    this.menuItems = const [],
    this.onMenuSelected,
  });

  static const viewType = 'buzz/navigation_glass';

  /// The SF Symbol-style glyph rendered by the native control.
  final IosGlassNavigationIcon icon;

  /// Optional text rendered by the native control instead of [icon].
  final String? label;

  /// Optional secondary text rendered beneath [label].
  final String? subtitle;

  /// Accessibility label exposed by the native view or Flutter fallback.
  final String semanticLabel;

  /// Invoked when the enabled control is activated.
  final VoidCallback? onPressed;

  /// Width of the platform-view hit target.
  final double width;

  /// Height of the platform-view hit target.
  final double height;

  /// Diameter of the visual glass control inside its hit target.
  final double controlSize;

  /// Whether the native visual control fills the available width.
  final bool fillWidth;

  /// Horizontal center for the visual control within its hit target.
  final double? buttonCenterX;

  /// Optional foreground tint for the native control and Flutter fallback.
  final Color? foregroundColor;

  /// Optional inset color swatch used by [IosGlassNavigationIcon.colorSwatch].
  final Color? swatchColor;

  /// Whether the native control presents its busy state.
  final bool isBusy;

  /// Whether the native control exposes its selected state.
  final bool isSelected;

  /// When true, substitutes an accessible Flutter control for the native view.
  final ValueListenable<bool>? nativeViewSuppressed;

  /// Optional avatar content used with [IosGlassNavigationIcon.avatar].
  final String? avatarImageUrl;

  /// Initial shown while an avatar image is unavailable.
  final String? avatarFallback;

  /// Optional SF Symbol used by native content such as channel identities.
  final String? systemIconName;

  /// Native menu presented directly beneath the glass control.
  final List<IosGlassNavigationMenuItem> menuItems;

  /// Invoked when a native menu item is selected.
  final ValueChanged<String>? onMenuSelected;

  @override
  Widget build(BuildContext context) {
    assert(defaultTargetPlatform == TargetPlatform.iOS);
    final nativeChannel = useState<MethodChannel?>(null);
    final onPressedRef = useRef(onPressed)..value = onPressed;
    final onMenuSelectedRef = useRef(onMenuSelected)..value = onMenuSelected;
    final brightness = context.theme.brightness.name;
    final effectiveForeground = foregroundColor ?? context.colors.primary;
    final foregroundValue = effectiveForeground.toARGB32();
    final swatchColorValue = swatchColor?.toARGB32();
    final enabled = onPressed != null;
    final routeAnimation =
        ModalRoute.of(context)?.animation ??
        const AlwaysStoppedAnimation<double>(1);
    final routeIsTransitioning = useState(
      routeAnimation.status != AnimationStatus.completed,
    );
    useEffect(() {
      void updateRouteTransition(AnimationStatus status) {
        final next = status != AnimationStatus.completed;
        if (routeIsTransitioning.value != next) {
          routeIsTransitioning.value = next;
        }
      }

      routeAnimation.addStatusListener(updateRouteTransition);
      return () => routeAnimation.removeStatusListener(updateRouteTransition);
    }, [routeAnimation]);
    final menuSignature = menuItems
        .map(
          (item) =>
              '${item.id}:${item.label}:${item.selected}:${item.destructive}:'
              '${item.avatarImageUrl}:${item.avatarFallback}:'
              '${item.systemIconName}',
        )
        .join('|');

    useEffect(() {
      final channel = nativeChannel.value;
      if (channel == null) return null;
      channel.setMethodCallHandler((call) async {
        if (call.method == 'pressed') {
          onPressedRef.value?.call();
        } else if (call.method == 'selected' && call.arguments is String) {
          onMenuSelectedRef.value?.call(call.arguments as String);
        }
      });
      return () => channel.setMethodCallHandler(null);
    }, [nativeChannel.value]);

    useEffect(
      () {
        final channel = nativeChannel.value;
        if (channel != null) {
          unawaited(
            channel.invokeMethod<void>('setAppearance', <String, Object>{
              'brightness': brightness,
              'foregroundColor': foregroundValue,
              'enabled': enabled,
              'busy': isBusy,
              'selected': isSelected,
              'swatchColor': ?swatchColorValue,
            }),
          );
        }
        return null;
      },
      [
        nativeChannel.value,
        brightness,
        foregroundValue,
        enabled,
        isBusy,
        isSelected,
        swatchColorValue,
      ],
    );

    useEffect(
      () {
        final channel = nativeChannel.value;
        if (channel != null) {
          final content = <String, Object>{
            'icon': icon.name,
            'accessibilityLabel': semanticLabel,
          };
          if (label != null) content['label'] = label!;
          if (subtitle != null) content['subtitle'] = subtitle!;
          if (avatarImageUrl != null) {
            content['avatarImageUrl'] = avatarImageUrl!;
          }
          if (avatarFallback != null) {
            content['avatarFallback'] = avatarFallback!;
          }
          if (systemIconName != null) {
            content['systemIconName'] = systemIconName!;
          }
          content['menuItems'] = menuItems
              .map((item) => item.toJson())
              .toList();
          unawaited(channel.invokeMethod<void>('setContent', content));
        }
        return null;
      },
      [
        nativeChannel.value,
        icon,
        label,
        subtitle,
        semanticLabel,
        avatarImageUrl,
        avatarFallback,
        systemIconName,
        menuSignature,
      ],
    );

    Widget buildControl({
      required bool suppressNativeView,
      required bool hideDuringRouteTransition,
    }) {
      if (suppressNativeView) {
        if (hideDuringRouteTransition) {
          return const SizedBox.expand(
            key: ValueKey('ios-glass-navigation-route-placeholder'),
          );
        }
        final resolvedButtonCenterX = buttonCenterX ?? width / 2;
        final isLeadingContent =
            icon == IosGlassNavigationIcon.avatar ||
            icon == IosGlassNavigationIcon.channel;
        final fallbackIcon = switch (icon) {
          IosGlassNavigationIcon.back => Icons.arrow_back_ios_new_rounded,
          IosGlassNavigationIcon.close => Icons.close_rounded,
          IosGlassNavigationIcon.camera => Icons.camera_alt_rounded,
          IosGlassNavigationIcon.photoLibrary => Icons.photo_library_rounded,
          IosGlassNavigationIcon.palette => Icons.palette_rounded,
          IosGlassNavigationIcon.droplet => Icons.water_drop_rounded,
          IosGlassNavigationIcon.emoji => Icons.emoji_emotions_rounded,
          IosGlassNavigationIcon.person => Icons.person_rounded,
          IosGlassNavigationIcon.frame =>
            Icons.photo_size_select_actual_rounded,
          IosGlassNavigationIcon.rotateCamera => Icons.cameraswitch_rounded,
          IosGlassNavigationIcon.shutter => Icons.circle,
          IosGlassNavigationIcon.colorSwatch => Icons.circle,
          IosGlassNavigationIcon.sun => Icons.light_mode_rounded,
          IosGlassNavigationIcon.moon => Icons.dark_mode_rounded,
          IosGlassNavigationIcon.systemAppearance =>
            Icons.brightness_auto_rounded,
          IosGlassNavigationIcon.avatar => Icons.person,
          IosGlassNavigationIcon.channel => switch (systemIconName) {
            'lock.fill' => Icons.lock_rounded,
            'bubble.left.and.bubble.right.fill' => Icons.forum_rounded,
            _ => Icons.tag_rounded,
          },
          IosGlassNavigationIcon.headphones => Icons.headphones_rounded,
        };
        return Semantics(
          container: true,
          button: true,
          enabled: enabled,
          selected: isSelected,
          label: semanticLabel,
          onTap: onPressed,
          child: ExcludeSemantics(
            child: Stack(
              key: const ValueKey('ios-glass-navigation-flutter-fallback'),
              children: [
                Positioned(
                  left: fillWidth ? 0 : resolvedButtonCenterX - controlSize / 2,
                  right: fillWidth ? 0 : null,
                  top: (height - controlSize) / 2,
                  width: fillWidth ? null : controlSize,
                  height: controlSize,
                  child: DecoratedBox(
                    decoration: BoxDecoration(
                      color: context.colors.surface.withValues(alpha: 0.72),
                      borderRadius: BorderRadius.circular(controlSize / 2),
                      border: Border.all(
                        color: context.colors.inverseSurface.withValues(
                          alpha: 0.07,
                        ),
                      ),
                    ),
                    child: isBusy
                        ? Center(
                            child: SizedBox.square(
                              dimension: 22,
                              child: CircularProgressIndicator(
                                strokeWidth: 2,
                                color: effectiveForeground,
                              ),
                            ),
                          )
                        : icon == IosGlassNavigationIcon.colorSwatch
                        ? Padding(
                            padding: const EdgeInsets.all(4),
                            child: DecoratedBox(
                              decoration: BoxDecoration(
                                color: swatchColor,
                                shape: BoxShape.circle,
                              ),
                            ),
                          )
                        : isLeadingContent && label != null
                        ? Padding(
                            padding: const EdgeInsets.symmetric(horizontal: 6),
                            child: Row(
                              children: [
                                if (icon == IosGlassNavigationIcon.avatar)
                                  AvatarImage(
                                    imageUrl: avatarImageUrl,
                                    radius: (controlSize - 12) / 2,
                                    backgroundColor:
                                        context.colors.primaryContainer,
                                    fallback: Text(
                                      avatarFallback ?? '?',
                                      style: context.textTheme.labelMedium
                                          ?.copyWith(
                                            color: context
                                                .colors
                                                .onPrimaryContainer,
                                            fontWeight: FontWeight.w600,
                                          ),
                                    ),
                                  )
                                else
                                  SizedBox.square(
                                    dimension: controlSize - 12,
                                    child: Icon(
                                      fallbackIcon,
                                      size: 20,
                                      color: effectiveForeground,
                                    ),
                                  ),
                                const SizedBox(width: Grid.xs),
                                Expanded(
                                  child: Column(
                                    mainAxisAlignment: MainAxisAlignment.center,
                                    crossAxisAlignment:
                                        CrossAxisAlignment.start,
                                    children: [
                                      Text(
                                        label!,
                                        maxLines: 1,
                                        overflow: TextOverflow.ellipsis,
                                        style: context.textTheme.labelMedium
                                            ?.copyWith(
                                              color: effectiveForeground,
                                              fontWeight: FontWeight.w600,
                                            ),
                                      ),
                                      if (subtitle != null)
                                        Text(
                                          subtitle!,
                                          maxLines: 1,
                                          overflow: TextOverflow.ellipsis,
                                          style: context.textTheme.labelSmall
                                              ?.copyWith(
                                                color: context.colors.onSurface
                                                    .withValues(alpha: 0.65),
                                              ),
                                        ),
                                    ],
                                  ),
                                ),
                              ],
                            ),
                          )
                        : label != null
                        ? Text(
                            label!,
                            maxLines: 1,
                            style: context.textTheme.labelMedium?.copyWith(
                              color: effectiveForeground,
                              fontWeight: FontWeight.w600,
                            ),
                          )
                        : Icon(
                            fallbackIcon,
                            size: icon == IosGlassNavigationIcon.shutter
                                ? controlSize * 0.72
                                : 22,
                            color: icon == IosGlassNavigationIcon.colorSwatch
                                ? swatchColor
                                : effectiveForeground,
                          ),
                  ),
                ),
              ],
            ),
          ),
        );
      }
      final creationParams = <String, Object>{
        'icon': icon.name,
        'accessibilityLabel': semanticLabel,
        'brightness': brightness,
        'foregroundColor': foregroundValue,
        'enabled': enabled,
        'busy': isBusy,
        'selected': isSelected,
        'controlSize': controlSize,
        'controlWidth': controlSize,
        'fillWidth': fillWidth,
        'buttonCenterX': buttonCenterX ?? width / 2,
        'hitTargetWidth': width,
        'hitTargetHeight': height,
        'swatchColor': ?swatchColorValue,
        'avatarImageUrl': ?avatarImageUrl,
        'avatarFallback': ?avatarFallback,
        'subtitle': ?subtitle,
        'systemIconName': ?systemIconName,
        'menuItems': menuItems.map((item) => item.toJson()).toList(),
      };
      if (label != null) creationParams['label'] = label!;
      return UiKitView(
        viewType: viewType,
        hitTestBehavior: PlatformViewHitTestBehavior.opaque,
        creationParams: creationParams,
        creationParamsCodec: const StandardMessageCodec(),
        onPlatformViewCreated: (viewId) {
          nativeChannel.value = MethodChannel('$viewType/$viewId');
        },
      );
    }

    return Tooltip(
      message: semanticLabel,
      excludeFromSemantics: true,
      child: SizedBox(
        width: width,
        height: height,
        child: nativeViewSuppressed == null
            ? buildControl(
                suppressNativeView: routeIsTransitioning.value,
                hideDuringRouteTransition: routeIsTransitioning.value,
              )
            : ValueListenableBuilder<bool>(
                valueListenable: nativeViewSuppressed!,
                builder: (context, suppressNativeView, _) => buildControl(
                  suppressNativeView:
                      suppressNativeView || routeIsTransitioning.value,
                  hideDuringRouteTransition: routeIsTransitioning.value,
                ),
              ),
      ),
    );
  }
}
