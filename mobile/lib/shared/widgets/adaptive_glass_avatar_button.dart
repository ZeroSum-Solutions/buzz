import 'dart:ui';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';

import '../theme/theme.dart';
import 'avatar_image.dart';
import 'ios_glass_navigation_button.dart';

/// A compact avatar control that uses native Liquid Glass on iOS and the
/// matching composited glass treatment on other platforms.
class AdaptiveGlassAvatarButton extends StatelessWidget {
  const AdaptiveGlassAvatarButton({
    super.key,
    required this.imageUrl,
    required this.fallbackText,
    required this.semanticLabel,
    required this.onPressed,
    required this.width,
    this.label,
    this.iosMenuItems = const [],
    this.onIosMenuSelected,
  });

  final String? imageUrl;
  final String fallbackText;
  final String semanticLabel;
  final VoidCallback onPressed;
  final double width;
  final String? label;
  final List<IosGlassNavigationMenuItem> iosMenuItems;
  final ValueChanged<String>? onIosMenuSelected;

  static const double height = 48;
  static const double avatarSize = 34;

  @override
  Widget build(BuildContext context) {
    if (defaultTargetPlatform == TargetPlatform.iOS) {
      return IosGlassNavigationButton(
        icon: IosGlassNavigationIcon.avatar,
        label: label,
        semanticLabel: semanticLabel,
        onPressed: onPressed,
        width: width,
        height: height,
        controlSize: height,
        fillWidth: true,
        foregroundColor: context.colors.onSurface,
        avatarImageUrl: imageUrl,
        avatarFallback: fallbackText,
        menuItems: iosMenuItems,
        onMenuSelected: onIosMenuSelected,
      );
    }

    final radius = BorderRadius.circular(height / 2);
    return Semantics(
      button: true,
      label: semanticLabel,
      child: ClipRRect(
        borderRadius: radius,
        child: BackdropFilter(
          filter: ImageFilter.blur(sigmaX: 18, sigmaY: 18),
          child: Material(
            color: context.colors.surface.withValues(alpha: 0.68),
            child: InkWell(
              onTap: onPressed,
              child: Container(
                width: width,
                height: height,
                padding: const EdgeInsets.all(6),
                decoration: BoxDecoration(
                  borderRadius: radius,
                  border: Border.all(
                    color: context.colors.inverseSurface.withValues(
                      alpha: 0.08,
                    ),
                  ),
                ),
                child: Stack(
                  alignment: Alignment.centerLeft,
                  children: [
                    AvatarImage(
                      imageUrl: imageUrl,
                      radius: avatarSize / 2,
                      backgroundColor: context.colors.primaryContainer,
                      fallback: Text(
                        fallbackText,
                        style: context.textTheme.labelMedium?.copyWith(
                          color: context.colors.onPrimaryContainer,
                          fontWeight: FontWeight.w600,
                        ),
                      ),
                    ),
                    if (label != null)
                      Positioned(
                        left: avatarSize + Grid.xxs,
                        right: Grid.xxs,
                        child: Text(
                          label!,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: context.textTheme.titleMedium?.copyWith(
                            color: navigationPrimaryForeground(context),
                            fontWeight: FontWeight.w600,
                          ),
                        ),
                      ),
                  ],
                ),
              ),
            ),
          ),
        ),
      ),
    );
  }
}
