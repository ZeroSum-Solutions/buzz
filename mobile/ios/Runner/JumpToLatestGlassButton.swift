import Flutter
import UIKit

final class JumpToLatestGlassButtonFactory: NSObject, FlutterPlatformViewFactory {
  private let messenger: FlutterBinaryMessenger

  init(messenger: FlutterBinaryMessenger) {
    self.messenger = messenger
    super.init()
  }

  func createArgsCodec() -> FlutterMessageCodec & NSObjectProtocol {
    FlutterStandardMessageCodec.sharedInstance()
  }

  func create(
    withFrame frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?
  ) -> FlutterPlatformView {
    JumpToLatestGlassButtonPlatformView(
      frame: frame,
      viewIdentifier: viewId,
      arguments: args,
      messenger: messenger
    )
  }
}

private final class JumpToLatestGlassButton: UIButton {
  private static let hitTargetExpansion: CGFloat = 4

  override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
    bounds
      .insetBy(
        dx: -Self.hitTargetExpansion,
        dy: -Self.hitTargetExpansion
      )
      .contains(point)
  }
}

final class NavigationGlassButton: UIButton {
  var hitTargetInsets = UIEdgeInsets.zero

  override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
    guard isEnabled, isUserInteractionEnabled, !isHidden, alpha > 0.01 else {
      return false
    }
    return bounds
      .inset(by: UIEdgeInsets(
        top: -hitTargetInsets.top,
        left: -hitTargetInsets.left,
        bottom: -hitTargetInsets.bottom,
        right: -hitTargetInsets.right
      ))
      .contains(point)
  }
}

private struct NavigationGlassMenuItem {
  let id: String
  let title: String
  let image: UIImage?
  let preservesImageColor: Bool
  let isSelected: Bool
  let isDestructive: Bool
}

private final class NavigationGlassMenuCell: UITableViewCell {
  static let reuseIdentifier = "NavigationGlassMenuCell"

  private let itemImageView = UIImageView()
  private let itemLabel = UILabel()
  private let checkImageView = UIImageView(
    image: UIImage(
      systemName: "checkmark",
      withConfiguration: UIImage.SymbolConfiguration(pointSize: 14, weight: .semibold)
    )
  )

  override init(style: UITableViewCell.CellStyle, reuseIdentifier: String?) {
    super.init(style: style, reuseIdentifier: reuseIdentifier)
    backgroundColor = .clear
    contentView.backgroundColor = .clear
    let selectedBackground = UIView()
    selectedBackground.backgroundColor = .tertiarySystemFill
    selectedBackground.layer.cornerRadius = 12
    selectedBackground.layer.cornerCurve = .continuous
    selectedBackgroundView = selectedBackground

    itemImageView.translatesAutoresizingMaskIntoConstraints = false
    itemImageView.contentMode = .scaleAspectFit
    itemImageView.clipsToBounds = true
    itemLabel.translatesAutoresizingMaskIntoConstraints = false
    itemLabel.font = UIFont.preferredFont(forTextStyle: .body)
    itemLabel.numberOfLines = 1
    itemLabel.lineBreakMode = .byTruncatingTail
    checkImageView.translatesAutoresizingMaskIntoConstraints = false
    checkImageView.contentMode = .center
    checkImageView.tintColor = .label

    contentView.addSubview(itemImageView)
    contentView.addSubview(itemLabel)
    contentView.addSubview(checkImageView)
    NSLayoutConstraint.activate([
      itemImageView.leadingAnchor.constraint(equalTo: contentView.leadingAnchor, constant: 10),
      itemImageView.centerYAnchor.constraint(equalTo: contentView.centerYAnchor),
      itemImageView.widthAnchor.constraint(equalToConstant: 28),
      itemImageView.heightAnchor.constraint(equalToConstant: 28),
      itemLabel.leadingAnchor.constraint(equalTo: itemImageView.trailingAnchor, constant: 10),
      itemLabel.centerYAnchor.constraint(equalTo: contentView.centerYAnchor),
      itemLabel.trailingAnchor.constraint(lessThanOrEqualTo: checkImageView.leadingAnchor, constant: -10),
      checkImageView.trailingAnchor.constraint(equalTo: contentView.trailingAnchor, constant: -10),
      checkImageView.centerYAnchor.constraint(equalTo: contentView.centerYAnchor),
      checkImageView.widthAnchor.constraint(equalToConstant: 20),
      checkImageView.heightAnchor.constraint(equalToConstant: 28),
    ])
  }

  required init?(coder: NSCoder) {
    nil
  }

  func apply(_ item: NavigationGlassMenuItem) {
    itemImageView.image = item.preservesImageColor
      ? item.image?.withRenderingMode(.alwaysOriginal)
      : item.image?.withRenderingMode(.alwaysTemplate)
    itemImageView.tintColor = item.isDestructive ? .systemRed : .label
    itemLabel.text = item.title
    itemLabel.textColor = item.isDestructive ? .systemRed : .label
    checkImageView.isHidden = !item.isSelected
    accessibilityTraits = item.isSelected ? [.button, .selected] : [.button]
  }
}

private final class NavigationGlassMenuViewController: UIViewController,
  UITableViewDataSource,
  UITableViewDelegate,
  UIPopoverPresentationControllerDelegate
{
  private static let menuWidth: CGFloat = 286
  private static let rowHeight: CGFloat = 48
  private static let outerInset: CGFloat = 8
  private static let maximumHeight: CGFloat = 400

  private let tableView = UITableView(frame: .zero, style: .plain)
  private let onSelect: (String) -> Void
  private let onDismiss: () -> Void
  private var items: [NavigationGlassMenuItem]

  init(
    items: [NavigationGlassMenuItem],
    onSelect: @escaping (String) -> Void,
    onDismiss: @escaping () -> Void
  ) {
    self.items = items
    self.onSelect = onSelect
    self.onDismiss = onDismiss
    super.init(nibName: nil, bundle: nil)
    modalPresentationStyle = .popover
    updatePreferredContentSize()
  }

  required init?(coder: NSCoder) {
    nil
  }

  override func loadView() {
    let effectView: UIVisualEffectView
    if #available(iOS 26.0, *) {
      effectView = UIVisualEffectView(effect: UIGlassEffect(style: .regular))
    } else {
      effectView = UIVisualEffectView(effect: UIBlurEffect(style: .systemMaterial))
    }
    effectView.backgroundColor = .clear
    effectView.isOpaque = false
    effectView.clipsToBounds = true
    effectView.layer.cornerRadius = 20
    effectView.layer.cornerCurve = .continuous
    view = effectView

    tableView.translatesAutoresizingMaskIntoConstraints = false
    tableView.backgroundColor = .clear
    tableView.separatorStyle = .none
    tableView.rowHeight = Self.rowHeight
    tableView.showsVerticalScrollIndicator = false
    tableView.dataSource = self
    tableView.delegate = self
    tableView.register(
      NavigationGlassMenuCell.self,
      forCellReuseIdentifier: NavigationGlassMenuCell.reuseIdentifier
    )
    effectView.contentView.addSubview(tableView)
    NSLayoutConstraint.activate([
      tableView.topAnchor.constraint(
        equalTo: effectView.contentView.topAnchor,
        constant: Self.outerInset
      ),
      tableView.leadingAnchor.constraint(
        equalTo: effectView.contentView.leadingAnchor,
        constant: Self.outerInset
      ),
      tableView.trailingAnchor.constraint(
        equalTo: effectView.contentView.trailingAnchor,
        constant: -Self.outerInset
      ),
      tableView.bottomAnchor.constraint(
        equalTo: effectView.contentView.bottomAnchor,
        constant: -Self.outerInset
      ),
    ])
  }

  func update(items: [NavigationGlassMenuItem]) {
    self.items = items
    updatePreferredContentSize()
    tableView.reloadData()
  }

  func tableView(_ tableView: UITableView, numberOfRowsInSection section: Int) -> Int {
    items.count
  }

  func tableView(
    _ tableView: UITableView,
    cellForRowAt indexPath: IndexPath
  ) -> UITableViewCell {
    guard let cell = tableView.dequeueReusableCell(
      withIdentifier: NavigationGlassMenuCell.reuseIdentifier,
      for: indexPath
    ) as? NavigationGlassMenuCell else {
      return UITableViewCell()
    }
    cell.apply(items[indexPath.row])
    return cell
  }

  func tableView(_ tableView: UITableView, didSelectRowAt indexPath: IndexPath) {
    tableView.deselectRow(at: indexPath, animated: true)
    let id = items[indexPath.row].id
    dismiss(animated: true) { [onSelect] in onSelect(id) }
  }

  func adaptivePresentationStyle(
    for controller: UIPresentationController
  ) -> UIModalPresentationStyle {
    .none
  }

  func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
    onDismiss()
  }

  private func updatePreferredContentSize() {
    let contentHeight =
      CGFloat(items.count) * Self.rowHeight + Self.outerInset * 2
    preferredContentSize = CGSize(
      width: Self.menuWidth,
      height: min(contentHeight, Self.maximumHeight)
    )
  }
}

final class JumpToLatestGlassButtonPlatformView: NSObject, FlutterPlatformView {
  private let containerView: UIView
  private let channel: FlutterMethodChannel
  private let button = JumpToLatestGlassButton(type: .system)

  init(
    frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?,
    messenger: FlutterBinaryMessenger
  ) {
    containerView = UIView(frame: frame)
    channel = FlutterMethodChannel(
      name: "buzz/jump_to_latest_glass/\(viewId)",
      binaryMessenger: messenger
    )
    super.init()

    containerView.backgroundColor = .clear
    containerView.isOpaque = false
    applyBrightness(from: args)

    var configuration: UIButton.Configuration
    if #available(iOS 26.0, *) {
      configuration = .glass()
    } else {
      configuration = .gray()
      configuration.baseBackgroundColor = UIColor.secondarySystemBackground
    }
    configuration.cornerStyle = .capsule
    configuration.baseForegroundColor = .label
    configuration.image = UIImage(
      systemName: "arrow.down",
      withConfiguration: UIImage.SymbolConfiguration(
        pointSize: 16,
        weight: .semibold
      )
    )
    button.configuration = configuration
    button.accessibilityLabel = "Jump to latest message"
    button.translatesAutoresizingMaskIntoConstraints = false
    button.addAction(
      UIAction { [weak self] _ in
        self?.channel.invokeMethod("pressed", arguments: nil)
      },
      for: .touchUpInside
    )

    channel.setMethodCallHandler { [weak self] call, result in
      guard call.method == "setBrightness" else {
        result(FlutterMethodNotImplemented)
        return
      }
      self?.applyBrightness(from: call.arguments)
      result(nil)
    }

    containerView.addSubview(button)
    NSLayoutConstraint.activate([
      button.centerXAnchor.constraint(equalTo: containerView.centerXAnchor),
      button.bottomAnchor.constraint(equalTo: containerView.bottomAnchor),
      button.widthAnchor.constraint(equalToConstant: 40),
      button.heightAnchor.constraint(equalToConstant: 40),
    ])
  }

  func view() -> UIView {
    containerView
  }

  private func applyBrightness(from value: Any?) {
    let brightness = (value as? [String: Any])?["brightness"] as? String
      ?? value as? String
    let interfaceStyle: UIUserInterfaceStyle = brightness == "dark" ? .dark : .light
    containerView.overrideUserInterfaceStyle = interfaceStyle
    button.overrideUserInterfaceStyle = interfaceStyle
    button.setNeedsUpdateConfiguration()
  }

  deinit {
    channel.setMethodCallHandler(nil)
  }
}

final class NavigationGlassButtonFactory: NSObject, FlutterPlatformViewFactory {
  private let messenger: FlutterBinaryMessenger

  init(messenger: FlutterBinaryMessenger) {
    self.messenger = messenger
    super.init()
  }

  func createArgsCodec() -> FlutterMessageCodec & NSObjectProtocol {
    FlutterStandardMessageCodec.sharedInstance()
  }

  func create(
    withFrame frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?
  ) -> FlutterPlatformView {
    NavigationGlassButtonPlatformView(
      frame: frame,
      viewIdentifier: viewId,
      arguments: args,
      messenger: messenger
    )
  }
}

final class NavigationGlassButtonPlatformView: NSObject, FlutterPlatformView {
  private static let shutterIconRatio: CGFloat = 80.0 / 115.0
  private static let shutterInsetRatio: CGFloat = 20.0 / 115.0
  private static let channelIconSize: CGFloat = 12
  private static let menuAvatarSize: CGFloat = 28
  private let containerView: UIView
  private let channel: FlutterMethodChannel
  private let button = NavigationGlassButton(type: .system)
  private let activityIndicator = UIActivityIndicatorView(style: .medium)
  private let swatchView = UIView()
  private var buttonLabel: String?
  private var buttonSubtitle: String?
  private var buttonIconName = "chevron.backward"
  private var contentIcon = "back"
  private var buttonImage: UIImage?
  private var isBusy = false
  private var controlSize: CGFloat = 40
  private var avatarImageURL: String?
  private var avatarFallback = "?"
  private var avatarLoadTask: URLSessionDataTask?
  private var menuItemDefinitions: [[String: Any]] = []
  private var menuAvatarImages: [String: UIImage] = [:]
  private var menuAvatarLoadTasks: [String: URLSessionDataTask] = [:]
  private weak var presentedMenuController: NavigationGlassMenuViewController?

  init(
    frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?,
    messenger: FlutterBinaryMessenger
  ) {
    containerView = UIView(frame: frame)
    channel = FlutterMethodChannel(
      name: "buzz/navigation_glass/\(viewId)",
      binaryMessenger: messenger
    )
    super.init()

    containerView.backgroundColor = .clear
    containerView.isOpaque = false
    let arguments = args as? [String: Any]
    let buttonCenterX =
      (arguments?["buttonCenterX"] as? NSNumber)?.doubleValue ?? 24
    let hitTargetWidth =
      (arguments?["hitTargetWidth"] as? NSNumber)?.doubleValue ?? 48
    let hitTargetHeight =
      (arguments?["hitTargetHeight"] as? NSNumber)?.doubleValue ?? 48
    let controlWidth =
      (arguments?["controlWidth"] as? NSNumber)?.doubleValue ?? 40
    controlSize =
      (arguments?["controlSize"] as? NSNumber)?.doubleValue ?? 40
    let fillWidth = arguments?["fillWidth"] as? Bool ?? false

    var configuration: UIButton.Configuration
    if #available(iOS 26.0, *) {
      configuration = .glass()
    } else {
      configuration = .gray()
      configuration.baseBackgroundColor = UIColor.secondarySystemBackground
    }
    configuration.cornerStyle = .capsule
    button.configuration = configuration
    button.clipsToBounds = true
    applyContent(from: arguments)
    button.titleLabel?.numberOfLines = 1
    button.titleLabel?.lineBreakMode = .byClipping
    button.hitTargetInsets = UIEdgeInsets(
      top: max(0, (hitTargetHeight - controlSize) / 2),
      left: max(0, buttonCenterX - controlWidth / 2),
      bottom: max(0, (hitTargetHeight - controlSize) / 2),
      right: max(0, hitTargetWidth - buttonCenterX - controlWidth / 2)
    )
    button.accessibilityLabel = arguments?["accessibilityLabel"] as? String ?? "Back"
    button.translatesAutoresizingMaskIntoConstraints = false
    button.addAction(
      UIAction { [weak self] _ in
        self?.activate()
      },
      for: .touchUpInside
    )

    activityIndicator.translatesAutoresizingMaskIntoConstraints = false
    activityIndicator.hidesWhenStopped = true
    activityIndicator.isUserInteractionEnabled = false
    button.addSubview(activityIndicator)
    NSLayoutConstraint.activate([
      activityIndicator.centerXAnchor.constraint(equalTo: button.centerXAnchor),
      activityIndicator.centerYAnchor.constraint(equalTo: button.centerYAnchor),
      activityIndicator.widthAnchor.constraint(equalToConstant: 24),
      activityIndicator.heightAnchor.constraint(equalToConstant: 24),
    ])

    swatchView.translatesAutoresizingMaskIntoConstraints = false
    swatchView.isUserInteractionEnabled = false
    swatchView.isHidden = true
    swatchView.layer.cornerCurve = .continuous
    button.addSubview(swatchView)
    let swatchSize = max(0, controlSize - 8)
    NSLayoutConstraint.activate([
      swatchView.centerXAnchor.constraint(equalTo: button.centerXAnchor),
      swatchView.centerYAnchor.constraint(equalTo: button.centerYAnchor),
      swatchView.widthAnchor.constraint(equalToConstant: swatchSize),
      swatchView.heightAnchor.constraint(equalToConstant: swatchSize),
    ])
    swatchView.layer.cornerRadius = swatchSize / 2

    applyAppearance(from: args)
    channel.setMethodCallHandler { [weak self] call, result in
      if call.method == "setContent" {
        self?.setContent(from: call.arguments)
        result(nil)
        return
      }
      guard call.method == "setAppearance" else {
        result(FlutterMethodNotImplemented)
        return
      }
      self?.applyAppearance(from: call.arguments)
      result(nil)
    }

    containerView.addSubview(button)
    NSLayoutConstraint.activate([
      button.centerYAnchor.constraint(equalTo: containerView.centerYAnchor),
      button.heightAnchor.constraint(equalToConstant: controlSize),
    ])
    if fillWidth {
      button.centerXAnchor.constraint(equalTo: containerView.centerXAnchor).isActive = true
      button.widthAnchor.constraint(equalTo: containerView.widthAnchor).isActive = true
    } else {
      button.centerXAnchor.constraint(
        equalTo: containerView.leadingAnchor,
        constant: buttonCenterX
      ).isActive = true
      button.widthAnchor.constraint(equalToConstant: controlWidth).isActive = true
    }
  }

  func view() -> UIView {
    containerView
  }

  /// Returns the pre-iOS-26 surface that maximizes contrast with the requested
  /// foreground while preserving the system surface when it already passes.
  static func fallbackBackgroundColor(
    foregroundColor: UIColor?,
    interfaceStyle: UIUserInterfaceStyle
  ) -> UIColor {
    let traits = UITraitCollection(userInterfaceStyle: interfaceStyle)
    let backgroundColor = UIColor.secondarySystemBackground.resolvedColor(with: traits)
    guard interfaceStyle != .dark, let foregroundColor else { return backgroundColor }
    let resolvedForeground = foregroundColor.resolvedColor(with: traits)
    if contrastRatio(foregroundColor: resolvedForeground, backgroundColor: backgroundColor) >= 3 {
      return backgroundColor
    }
    return .black
  }

  static func contrastRatio(
    foregroundColor: UIColor,
    backgroundColor: UIColor
  ) -> CGFloat {
    guard
      let foregroundLuminance = relativeLuminance(of: foregroundColor),
      let backgroundLuminance = relativeLuminance(of: backgroundColor)
    else { return 1 }
    let lighter = max(foregroundLuminance, backgroundLuminance)
    let darker = min(foregroundLuminance, backgroundLuminance)
    return (lighter + 0.05) / (darker + 0.05)
  }

  private static func relativeLuminance(of color: UIColor) -> CGFloat? {
    var red: CGFloat = 0
    var green: CGFloat = 0
    var blue: CGFloat = 0
    var alpha: CGFloat = 0
    guard color.getRed(&red, green: &green, blue: &blue, alpha: &alpha) else {
      return nil
    }

    func linearize(_ component: CGFloat) -> CGFloat {
      component <= 0.04045
        ? component / 12.92
        : pow((component + 0.055) / 1.055, 2.4)
    }

    return 0.2126 * linearize(red) +
      0.7152 * linearize(green) +
      0.0722 * linearize(blue)
  }

  private func applyAppearance(from value: Any?) {
    let arguments = value as? [String: Any]
    let brightness = arguments?["brightness"] as? String
    let interfaceStyle: UIUserInterfaceStyle = brightness == "dark" ? .dark : .light
    let colorValue = (arguments?["foregroundColor"] as? NSNumber)?.uint32Value
    let foregroundColor = colorValue.map(Self.color(from:))
    let enabled = arguments?["enabled"] as? Bool ?? true
    let busy = arguments?["busy"] as? Bool ?? false
    let selected = arguments?["selected"] as? Bool ?? false
    let swatchColorValue = (arguments?["swatchColor"] as? NSNumber)?.uint32Value

    containerView.overrideUserInterfaceStyle = interfaceStyle
    button.overrideUserInterfaceStyle = interfaceStyle
    button.isEnabled = enabled
    button.isSelected = selected
    isBusy = busy
    if selected {
      button.accessibilityTraits.insert(.selected)
    } else {
      button.accessibilityTraits.remove(.selected)
    }
    button.configuration?.showsActivityIndicator = false
    if let foregroundColor {
      // Glass uses the view tint for its selected treatment. Keep it aligned
      // with the Buzz theme instead of falling back to the system blue tint.
      button.tintColor = foregroundColor
      button.configuration?.baseForegroundColor = foregroundColor
      activityIndicator.color = foregroundColor
    }
    if let swatchColorValue {
      swatchView.backgroundColor = Self.color(from: swatchColorValue)
    }
    if #unavailable(iOS 26.0) {
      button.configuration?.baseBackgroundColor = Self.fallbackBackgroundColor(
        foregroundColor: foregroundColor,
        interfaceStyle: interfaceStyle
      )
    }
    updateDisplayedContent()
    button.setNeedsUpdateConfiguration()
  }

  private func applyContent(from value: Any?) {
    let arguments = value as? [String: Any]
    let icon = arguments?["icon"] as? String
    contentIcon = icon ?? "back"
    buttonLabel = arguments?["label"] as? String
    buttonSubtitle = arguments?["subtitle"] as? String
    avatarFallback = arguments?["avatarFallback"] as? String ?? "?"
    if let accessibilityLabel = arguments?["accessibilityLabel"] as? String {
      button.accessibilityLabel = accessibilityLabel
    }
    switch icon {
    case "close": buttonIconName = "xmark"
    case "camera": buttonIconName = "camera"
    case "photoLibrary": buttonIconName = "photo.on.rectangle.angled"
    case "palette": buttonIconName = "paintpalette"
    case "droplet": buttonIconName = "drop.fill"
    case "emoji": buttonIconName = "face.smiling"
    case "person": buttonIconName = "person"
    case "frame": buttonIconName = "rectangle.stack"
    case "rotateCamera": buttonIconName = "arrow.triangle.2.circlepath.camera"
    case "shutter": buttonIconName = "circle.fill"
    case "sun": buttonIconName = "sun.max"
    case "moon": buttonIconName = "moon"
    case "systemAppearance": buttonIconName = "circle.lefthalf.filled"
    case "colorSwatch": buttonIconName = "circle.fill"
    case "channel": buttonIconName = arguments?["systemIconName"] as? String ?? "number"
    case "headphones": buttonIconName = "headphones"
    default: buttonIconName = "chevron.backward"
    }
    if contentIcon == "avatar" || contentIcon == "channel" {
      button.contentHorizontalAlignment = .leading
      button.configuration?.contentInsets = NSDirectionalEdgeInsets(
        top: contentIcon == "avatar" ? 6 : 4,
        leading: contentIcon == "avatar" ? 6 : 12,
        bottom: contentIcon == "avatar" ? 6 : 4,
        trailing: contentIcon == "avatar" ? (buttonLabel == nil ? 6 : 8) : 12
      )
      button.configuration?.imagePadding = buttonLabel == nil ? 0 : 8
      button.configuration?.titleLineBreakMode = .byTruncatingTail
      let usesChannelTypography = contentIcon == "channel"
      button.configuration?.titleTextAttributesTransformer =
        UIConfigurationTextAttributesTransformer { incoming in
          var outgoing = incoming
          let preferred = UIFont.preferredFont(
            forTextStyle: usesChannelTypography ? .subheadline : .headline
          )
          outgoing.font = UIFont.systemFont(
            ofSize: preferred.pointSize,
            weight: .semibold
          )
          return outgoing
        }
      if contentIcon == "avatar" {
        updateAvatar(from: arguments?["avatarImageUrl"] as? String)
      } else {
        avatarLoadTask?.cancel()
        avatarImageURL = nil
        buttonImage = UIImage(
          systemName: buttonIconName,
          withConfiguration: UIImage.SymbolConfiguration(
            pointSize: Self.channelIconSize,
            weight: .semibold
          )
        )
        button.configuration?.subtitleTextAttributesTransformer =
          UIConfigurationTextAttributesTransformer { incoming in
            var outgoing = incoming
            outgoing.font = UIFont.preferredFont(forTextStyle: .caption1)
            outgoing.foregroundColor = UIColor.secondaryLabel
            return outgoing
          }
      }
    } else if buttonLabel != nil {
      button.contentHorizontalAlignment = .center
      avatarLoadTask?.cancel()
      avatarImageURL = nil
      buttonImage = nil
      button.configuration?.contentInsets = NSDirectionalEdgeInsets(
        top: 8,
        leading: 8,
        bottom: 8,
        trailing: 8
      )
      button.configuration?.titleLineBreakMode = .byClipping
      button.configuration?.titleTextAttributesTransformer =
        UIConfigurationTextAttributesTransformer { incoming in
          var outgoing = incoming
          let preferred = UIFont.preferredFont(forTextStyle: .subheadline)
          outgoing.font = UIFont.systemFont(
            ofSize: preferred.pointSize,
            weight: .semibold
          )
          return outgoing
        }
      button.configuration?.subtitleTextAttributesTransformer = nil
    } else {
      button.contentHorizontalAlignment = .center
      avatarLoadTask?.cancel()
      avatarImageURL = nil
      button.configuration?.titleTextAttributesTransformer = nil
      button.configuration?.subtitleTextAttributesTransformer = nil
      let iconInset: CGFloat = icon == "shutter"
        ? controlSize * Self.shutterInsetRatio
        : 8
      button.configuration?.contentInsets = NSDirectionalEdgeInsets(
        top: iconInset,
        leading: iconInset,
        bottom: iconInset,
        trailing: iconInset
      )
      let pointSize: CGFloat = icon == "shutter"
        ? controlSize * Self.shutterIconRatio
        : 17
      buttonImage = UIImage(
        systemName: buttonIconName,
        withConfiguration: UIImage.SymbolConfiguration(
          pointSize: pointSize,
          weight: .semibold
        )
      )
    }
    updateMenu(from: arguments?["menuItems"] as? [[String: Any]] ?? [])
    updateDisplayedContent()
    button.setNeedsUpdateConfiguration()
  }

  private func setContent(from value: Any?) {
    let duration = UIAccessibility.isReduceMotionEnabled ? 0 : 0.12
    UIView.transition(
      with: button,
      duration: duration,
      options: [.transitionCrossDissolve, .beginFromCurrentState, .allowAnimatedContent],
      animations: { [weak self] in self?.applyContent(from: value) }
    )
  }

  private func updateDisplayedContent() {
    if isBusy {
      button.configuration?.title = nil
      button.configuration?.image = nil
      swatchView.isHidden = true
      activityIndicator.startAnimating()
    } else {
      let displaysSwatch = contentIcon == "colorSwatch"
      let displaysAvatar = contentIcon == "avatar"
      let displaysLeadingImage = displaysAvatar || contentIcon == "channel"
      button.configuration?.title = displaysSwatch ? nil : buttonLabel
      button.configuration?.subtitle = displaysSwatch ? nil : buttonSubtitle
      button.configuration?.image = displaysSwatch || (buttonLabel != nil && !displaysLeadingImage)
        ? nil
        : buttonImage
      swatchView.isHidden = !displaysSwatch
      activityIndicator.stopAnimating()
    }
  }

  private func updateAvatar(from imageURL: String?) {
    avatarLoadTask?.cancel()
    avatarLoadTask = nil
    avatarImageURL = imageURL
    buttonImage = Self.avatarFallbackImage(text: avatarFallback)

    guard let imageURL, !imageURL.isEmpty else { return }
    if let comma = imageURL.firstIndex(of: ","), imageURL.hasPrefix("data:image") {
      let encoded = String(imageURL[imageURL.index(after: comma)...])
      if let data = Data(base64Encoded: encoded), let image = UIImage(data: data) {
        buttonImage = Self.circularAvatarImage(image)
      }
      return
    }
    guard let url = URL(string: imageURL) else { return }
    let expectedURL = imageURL
    avatarLoadTask = URLSession.shared.dataTask(with: url) { [weak self] data, _, _ in
      guard let data, let image = UIImage(data: data) else { return }
      DispatchQueue.main.async {
        guard self?.avatarImageURL == expectedURL else { return }
        self?.buttonImage = Self.circularAvatarImage(image)
        self?.updateDisplayedContent()
        self?.button.setNeedsUpdateConfiguration()
      }
    }
    avatarLoadTask?.resume()
  }

  private func updateMenu(from items: [[String: Any]]) {
    menuAvatarLoadTasks.values.forEach { $0.cancel() }
    menuAvatarLoadTasks.removeAll()
    menuItemDefinitions = items
    guard !items.isEmpty else {
      button.menu = nil
      button.showsMenuAsPrimaryAction = false
      presentedMenuController?.dismiss(animated: true)
      return
    }
    button.menu = nil
    button.showsMenuAsPrimaryAction = false
    rebuildMenu()

    for item in items {
      guard
        let imageURL = item["avatarImageUrl"] as? String,
        !imageURL.isEmpty,
        menuAvatarImages[imageURL] == nil
      else { continue }
      loadMenuAvatar(from: imageURL)
    }
  }

  private func rebuildMenu() {
    presentedMenuController?.update(items: resolvedMenuItems())
  }

  private func resolvedMenuItems() -> [NavigationGlassMenuItem] {
    menuItemDefinitions.compactMap { item in
      guard
        let id = item["id"] as? String,
        let label = item["label"] as? String
      else { return nil }
      let imageURL = item["avatarImageUrl"] as? String
      let fallback = item["avatarFallback"] as? String
      let systemIconName = item["systemIconName"] as? String
      let image = imageURL.flatMap { menuAvatarImages[$0] }
        ?? fallback.map {
          Self.avatarFallbackImage(text: $0, size: Self.menuAvatarSize)
        }
        ?? systemIconName.flatMap { UIImage(systemName: $0) }
      return NavigationGlassMenuItem(
        id: id,
        title: label,
        image: image,
        preservesImageColor: imageURL != nil || fallback != nil,
        isSelected: item["selected"] as? Bool == true,
        isDestructive: item["destructive"] as? Bool == true
      )
    }
  }

  private func activate() {
    guard !menuItemDefinitions.isEmpty else {
      channel.invokeMethod("pressed", arguments: nil)
      return
    }
    guard presentedMenuController == nil else { return }
    let controller = NavigationGlassMenuViewController(
      items: resolvedMenuItems(),
      onSelect: { [weak self] id in
        self?.presentedMenuController = nil
        self?.channel.invokeMethod("selected", arguments: id)
      },
      onDismiss: { [weak self] in
        self?.presentedMenuController = nil
      }
    )
    guard
      let presenter = Self.topViewController(from: containerView.window?.rootViewController),
      let popover = controller.popoverPresentationController
    else { return }
    popover.sourceView = button
    popover.sourceRect = button.bounds
    popover.permittedArrowDirections = [.up]
    popover.backgroundColor = .clear
    popover.delegate = controller
    presentedMenuController = controller
    presenter.present(controller, animated: true)
  }

  private static func topViewController(from root: UIViewController?) -> UIViewController? {
    if let presented = root?.presentedViewController {
      return topViewController(from: presented)
    }
    if let navigation = root as? UINavigationController {
      return topViewController(from: navigation.visibleViewController)
    }
    if let tabs = root as? UITabBarController {
      return topViewController(from: tabs.selectedViewController)
    }
    return root
  }

  private func loadMenuAvatar(from imageURL: String) {
    if let comma = imageURL.firstIndex(of: ","), imageURL.hasPrefix("data:image") {
      let encoded = String(imageURL[imageURL.index(after: comma)...])
      if let data = Data(base64Encoded: encoded), let image = UIImage(data: data) {
        menuAvatarImages[imageURL] = Self.circularAvatarImage(
          image,
          size: Self.menuAvatarSize
        )
        rebuildMenu()
      }
      return
    }
    guard let url = URL(string: imageURL) else { return }
    let task = URLSession.shared.dataTask(with: url) { [weak self] data, _, _ in
      guard let data, let image = UIImage(data: data) else { return }
      DispatchQueue.main.async {
        guard self?.menuAvatarLoadTasks[imageURL] != nil else { return }
        self?.menuAvatarImages[imageURL] = Self.circularAvatarImage(
          image,
          size: Self.menuAvatarSize
        )
        self?.menuAvatarLoadTasks[imageURL] = nil
        self?.rebuildMenu()
      }
    }
    menuAvatarLoadTasks[imageURL] = task
    task.resume()
  }

  private static func circularAvatarImage(
    _ image: UIImage,
    size dimension: CGFloat = 36
  ) -> UIImage {
    let size = CGSize(width: dimension, height: dimension)
    return UIGraphicsImageRenderer(size: size).image { _ in
      UIBezierPath(ovalIn: CGRect(origin: .zero, size: size)).addClip()
      let scale = max(size.width / image.size.width, size.height / image.size.height)
      let drawSize = CGSize(width: image.size.width * scale, height: image.size.height * scale)
      image.draw(in: CGRect(
        x: (size.width - drawSize.width) / 2,
        y: (size.height - drawSize.height) / 2,
        width: drawSize.width,
        height: drawSize.height
      ))
    }.withRenderingMode(.alwaysOriginal)
  }

  private static func avatarFallbackImage(
    text: String,
    size dimension: CGFloat = 36
  ) -> UIImage {
    let size = CGSize(width: dimension, height: dimension)
    return UIGraphicsImageRenderer(size: size).image { context in
      UIColor.tertiarySystemFill.setFill()
      context.cgContext.fillEllipse(in: CGRect(origin: .zero, size: size))
      let value = String(text.prefix(1)).uppercased() as NSString
      let attributes: [NSAttributedString.Key: Any] = [
        .font: UIFont.systemFont(
          ofSize: dimension == Self.menuAvatarSize ? 13 : 15,
          weight: .semibold
        ),
        .foregroundColor: UIColor.label,
      ]
      let textSize = value.size(withAttributes: attributes)
      value.draw(
        at: CGPoint(x: (size.width - textSize.width) / 2, y: (size.height - textSize.height) / 2),
        withAttributes: attributes
      )
    }.withRenderingMode(.alwaysOriginal)
  }

  private static func color(from value: UInt32) -> UIColor {
    let alpha = CGFloat((value >> 24) & 0xFF) / 255
    let red = CGFloat((value >> 16) & 0xFF) / 255
    let green = CGFloat((value >> 8) & 0xFF) / 255
    let blue = CGFloat(value & 0xFF) / 255
    return UIColor(red: red, green: green, blue: blue, alpha: alpha)
  }

  deinit {
    avatarLoadTask?.cancel()
    menuAvatarLoadTasks.values.forEach { $0.cancel() }
    channel.setMethodCallHandler(nil)
  }
}

final class NativeSegmentedControlFactory: NSObject, FlutterPlatformViewFactory {
  private let messenger: FlutterBinaryMessenger

  init(messenger: FlutterBinaryMessenger) {
    self.messenger = messenger
    super.init()
  }

  func createArgsCodec() -> FlutterMessageCodec & NSObjectProtocol {
    FlutterStandardMessageCodec.sharedInstance()
  }

  func create(
    withFrame frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?
  ) -> FlutterPlatformView {
    NativeSegmentedControlPlatformView(
      frame: frame,
      viewIdentifier: viewId,
      arguments: args,
      messenger: messenger
    )
  }
}

final class NativeSegmentedControlPlatformView: NSObject, FlutterPlatformView {
  private let containerView: UIView
  private let channel: FlutterMethodChannel
  private let segmentedControl: UISegmentedControl

  init(
    frame: CGRect,
    viewIdentifier viewId: Int64,
    arguments args: Any?,
    messenger: FlutterBinaryMessenger
  ) {
    let arguments = args as? [String: Any]
    let items = arguments?["items"] as? [String] ?? []
    containerView = UIView(frame: frame)
    channel = FlutterMethodChannel(
      name: "buzz/native_segmented_control/\(viewId)",
      binaryMessenger: messenger
    )
    segmentedControl = UISegmentedControl(items: items)
    super.init()

    containerView.backgroundColor = .clear
    containerView.isOpaque = false
    segmentedControl.translatesAutoresizingMaskIntoConstraints = false
    segmentedControl.addTarget(
      self,
      action: #selector(selectionChanged),
      for: .valueChanged
    )
    applyState(from: arguments)

    channel.setMethodCallHandler { [weak self] call, result in
      guard call.method == "setState" else {
        result(FlutterMethodNotImplemented)
        return
      }
      self?.applyState(from: call.arguments)
      result(nil)
    }

    containerView.addSubview(segmentedControl)
    NSLayoutConstraint.activate([
      segmentedControl.leadingAnchor.constraint(equalTo: containerView.leadingAnchor),
      segmentedControl.trailingAnchor.constraint(equalTo: containerView.trailingAnchor),
      segmentedControl.centerYAnchor.constraint(equalTo: containerView.centerYAnchor),
    ])
  }

  func view() -> UIView {
    containerView
  }

  @objc private func selectionChanged() {
    channel.invokeMethod("changed", arguments: segmentedControl.selectedSegmentIndex)
  }

  private func applyState(from value: Any?) {
    let arguments = value as? [String: Any]
    let selectedIndex = (arguments?["selectedIndex"] as? NSNumber)?.intValue ?? 0
    let brightness = arguments?["brightness"] as? String
    let enabled = arguments?["enabled"] as? Bool ?? true
    let interfaceStyle: UIUserInterfaceStyle = brightness == "dark" ? .dark : .light

    containerView.overrideUserInterfaceStyle = interfaceStyle
    segmentedControl.overrideUserInterfaceStyle = interfaceStyle
    segmentedControl.selectedSegmentIndex = selectedIndex
    segmentedControl.isEnabled = enabled
  }

  deinit {
    channel.setMethodCallHandler(nil)
  }
}
