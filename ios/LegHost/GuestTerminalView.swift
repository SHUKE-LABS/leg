import SwiftUI
import UIKit
import WebKit

/// The terminal the guest program runs in.
struct GuestTerminalView: UIViewRepresentable {
    let controller: GuestHostController

    func makeUIView(context: Context) -> GuestTerminalUIView {
        let view = GuestTerminalUIView()
        view.attach(to: controller)
        controller.attach(sink: view)
        return view
    }

    func updateUIView(_ uiView: GuestTerminalUIView, context: Context) {}
}

/// The web view that draws the terminal.
///
/// The keyboard belongs to the host, which forwards it to the guest, so the
/// web view never takes the focus. Without this a tap on the terminal gives
/// the keyboard to the page, and the input field loses it again.
final class GuestTerminalWebView: WKWebView {
    override var canBecomeFirstResponder: Bool { false }
}

/// The view that hosts hterm and forwards the keyboard to the guest.
///
/// The keyboard goes into a hidden text field under the terminal, so the
/// software keyboard types into the guest terminal. The control combinations
/// and the cursor keys are handled here, because the field would use them for
/// editing its own text.
final class GuestTerminalUIView: UIView, GuestTerminalSink, UITextFieldDelegate {
    weak var controller: GuestHostController?

    private let webView: WKWebView
    /// The field that takes the keyboard. The terminal is a web view, so the
    /// keyboard goes through a text field that forwards everything to the
    /// guest.
    private let inputField = UITextField()
    private var focusObserver: NSObjectProtocol?
    /// The font the terminal currently uses.
    private var appliedFontSize = GuestTerminalUIView.maximumFontSize
    /// Whether the style was already written to the terminal.
    private var styleApplied = false
    /// Whether the terminal is being fitted to the size the guest program
    /// needs.
    private var isFitting = false

    /// The grid the guest program asks for.
    private static let minimumColumns = GuestTerminalSize.minimumColumns
    private static let minimumRows = GuestTerminalSize.minimumRows
    /// The largest font the terminal uses, and the smallest one.
    private static let maximumFontSize = 13.0
    private static let minimumFontSize = 7.0

    private static let handlerNames = [
        "load", "log", "sendInput", "resize", "propUpdate",
        "focus", "syncFocus", "newScrollHeight", "newScrollTop", "openLink",
    ]

    /// The control keys a terminal program understands.
    private static let controlKeys: [String: String] = [
        "a": "\u{01}", "b": "\u{02}", "c": GuestTerminalKey.interrupt, "d": "\u{04}",
        "e": "\u{05}", "f": "\u{06}", "g": "\u{07}", "h": "\u{08}", "i": "\u{09}",
        "j": "\u{0a}", "k": "\u{0b}", "l": "\u{0c}", "m": "\u{0d}", "n": "\u{0e}",
        "o": "\u{0f}", "p": "\u{10}", "q": "\u{11}", "r": "\u{12}", "s": "\u{13}",
        "t": "\u{14}", "u": "\u{15}", "v": "\u{16}", "w": "\u{17}", "x": "\u{18}",
        "y": "\u{19}", "z": "\u{1a}", "[": "\u{1b}", "\\": "\u{1c}", "]": "\u{1d}",
        "^": "\u{1e}", "_": "\u{1f}", " ": "\u{00}", "?": "\u{7f}",
    ]

    /// The keys that a terminal program reads as escape sequences.
    private static let specialKeys: [String: String] = [
        UIKeyCommand.inputEscape: GuestTerminalKey.escape,
        "\t": GuestTerminalKey.tab,
        UIKeyCommand.inputUpArrow: GuestTerminalKey.up,
        UIKeyCommand.inputDownArrow: GuestTerminalKey.down,
        UIKeyCommand.inputRightArrow: GuestTerminalKey.right,
        UIKeyCommand.inputLeftArrow: GuestTerminalKey.left,
        UIKeyCommand.inputHome: "\u{1b}[H",
        UIKeyCommand.inputEnd: "\u{1b}[F",
        UIKeyCommand.inputPageUp: "\u{1b}[5~",
        UIKeyCommand.inputPageDown: "\u{1b}[6~",
        UIKeyCommand.inputDelete: "\u{1b}[3~",
    ]

    override init(frame: CGRect) {
        let configuration = WKWebViewConfiguration()
        webView = GuestTerminalWebView(frame: .zero, configuration: configuration)
        super.init(frame: frame)
        setUp()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not used")
    }

    deinit {
        if let focusObserver {
            NotificationCenter.default.removeObserver(focusObserver)
        }
    }

    private func setUp() {
        backgroundColor = .black
        accessibilityIdentifier = "guest-terminal"
        accessibilityLabel = "Guest terminal"
        webView.translatesAutoresizingMaskIntoConstraints = false
        webView.isOpaque = false
        webView.backgroundColor = .clear
        webView.scrollView.backgroundColor = .clear
        webView.scrollView.isScrollEnabled = false
        webView.navigationDelegate = self
        addSubview(webView)
        NSLayoutConstraint.activate([
            webView.leadingAnchor.constraint(equalTo: leadingAnchor),
            webView.trailingAnchor.constraint(equalTo: trailingAnchor),
            webView.topAnchor.constraint(equalTo: topAnchor),
            webView.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])

        let tap = UITapGestureRecognizer(target: self, action: #selector(focusTerminal))
        addGestureRecognizer(tap)

        // The keyboard goes into a text field that forwards everything to the
        // guest. The field sits under the terminal, so it never covers the
        // screen, but it is the first responder while the terminal is in use.
        inputField.delegate = self
        inputField.alpha = 0.01
        inputField.textColor = .clear
        inputField.tintColor = .clear
        inputField.backgroundColor = .clear
        inputField.autocorrectionType = .no
        inputField.autocapitalizationType = .none
        inputField.spellCheckingType = .no
        inputField.accessibilityIdentifier = "guest-input"
        inputField.translatesAutoresizingMaskIntoConstraints = false
        insertSubview(inputField, at: 0)
        NSLayoutConstraint.activate([
            inputField.leadingAnchor.constraint(equalTo: leadingAnchor),
            inputField.topAnchor.constraint(equalTo: topAnchor),
            inputField.widthAnchor.constraint(equalToConstant: 1),
            inputField.heightAnchor.constraint(equalToConstant: 1),
        ])

        focusObserver = NotificationCenter.default.addObserver(
            forName: .guestTerminalNeedsFocus, object: nil, queue: .main) { [weak self] _ in
                self?.focusTerminal()
            }
    }

    /// Connects the view to the host and loads the terminal page.
    func attach(to controller: GuestHostController) {
        self.controller = controller
        for name in Self.handlerNames {
            webView.configuration.userContentController.add(controller, name: name)
        }

        if let url = Bundle.main.url(forResource: "term", withExtension: "html") {
            webView.loadFileURL(url, allowingReadAccessTo: url.deletingLastPathComponent())
        } else {
            NSLog("[LegHost] the hterm page is missing from the app bundle")
        }
    }

    @objc private func focusTerminal() {
        let focused = inputField.becomeFirstResponder()
        NSLog("[LegHost] the terminal took the keyboard focus: %d", focused)
    }

    // MARK: - Keyboard

    /// The text field hands every key to the guest.
    ///
    /// The field keeps its own text empty: the terminal shows the characters,
    /// not the field. The delete key arrives as the delete character, which
    /// the guest expects as the delete key.
    func textField(_ textField: UITextField,
                   shouldChangeCharactersIn range: NSRange,
                   replacementString string: String) -> Bool {
        let text = string == "\u{8}" ? "\u{7f}" : string
        controller?.sendInput(text)
        return false
    }

    // MARK: - Hardware keyboard

    /// The hardware keyboard has no software control key, so the control
    /// combinations and the cursor keys are turned into the sequences a
    /// terminal program expects.
    override var keyCommands: [UIKeyCommand]? {
        var commands: [UIKeyCommand] = []
        for key in Self.controlKeys.keys.sorted() {
            let command = UIKeyCommand(input: key, modifierFlags: .control,
                                       action: #selector(sendControlKey(_:)))
            command.wantsPriorityOverSystemBehavior = true
            commands.append(command)
        }
        for key in Self.specialKeys.keys.sorted() {
            let command = UIKeyCommand(input: key, modifierFlags: [],
                                       action: #selector(sendSpecialKey(_:)))
            command.wantsPriorityOverSystemBehavior = true
            commands.append(command)
        }
        return commands
    }

    @objc private func sendControlKey(_ sender: UIKeyCommand) {
        guard let key = sender.input?.lowercased(),
              let sequence = Self.controlKeys[key] else { return }
        controller?.sendInput(sequence)
    }

    @objc private func sendSpecialKey(_ sender: UIKeyCommand) {
        guard let input = sender.input,
              let sequence = Self.specialKeys[input] else { return }
        controller?.sendInput(sequence)
    }

    // MARK: - Terminal sink

    func write(_ text: String) {
        guard let literal = Self.jsonStringLiteral(text) else { return }
        webView.evaluateJavaScript("exports.write(\(literal))", completionHandler: nil)
    }

    /// Reports the size of the terminal to the host.
    ///
    /// The terminal is made big enough for the guest program first: the font
    /// is scaled down until the grid holds the size the guest asks for. A
    /// guest program that needs 80 columns and 24 rows would otherwise
    /// refuse to run on a phone screen.
    func reportSize() {
        guard !isFitting else { return }
        isFitting = true
        applyStyle(fontSize: appliedFontSize) { [weak self] in
            self?.fitTheTerminal { [weak self] in
                self?.isFitting = false
                self?.sendSize()
            }
        }
    }

    /// Scales the font until the grid holds the size the guest program asks
    /// for.
    ///
    /// The grid grows when the font gets smaller, but not by the same amount
    /// in both directions, so the font is measured instead of calculated: the
    /// terminal reports its size after every change.
    private func fitTheTerminal(_ done: @escaping () -> Void) {
        measureSize { [weak self] columns, rows in
            guard let self else { return done() }
            if columns < Self.minimumColumns || rows < Self.minimumRows {
                // The grid is too small: a smaller font makes it bigger.
                guard self.appliedFontSize > Self.minimumFontSize else { return done() }
                self.applyStyle(fontSize: self.appliedFontSize - 1) {
                    self.fitTheTerminal(done)
                }
                return
            }
            // The grid is much bigger than the guest program needs. A larger
            // font is easier to read, as long as the grid stays big enough.
            guard self.appliedFontSize < Self.maximumFontSize,
                  rows >= Self.minimumRows * 2 || columns >= Self.minimumColumns * 2
            else { return done() }
            let smaller = self.appliedFontSize
            self.applyStyle(fontSize: smaller + 1) { [weak self] in
                self?.measureSize { columns, rows in
                    guard columns >= Self.minimumColumns, rows >= Self.minimumRows else {
                        // The larger font made the grid too small: keep the
                        // font that worked.
                        self?.applyStyle(fontSize: smaller, completion: done)
                        return
                    }
                    self?.fitTheTerminal(done)
                }
            }
        }
    }

    /// The size of the terminal grid, in character cells.
    private func measureSize(_ done: @escaping (Int, Int) -> Void) {
        webView.evaluateJavaScript("exports.getSize()") { result, _ in
            guard let grid = result as? [NSNumber], grid.count == 2 else {
                return done(0, 0)
            }
            done(grid[0].intValue, grid[1].intValue)
        }
    }

    /// Writes the style of the terminal. hterm starts with a transparent
    /// terminal, so the host decides how it looks.
    private func applyStyle(fontSize: Double, completion: @escaping () -> Void) {
        guard !styleApplied || fontSize != appliedFontSize else {
            return completion()
        }
        appliedFontSize = fontSize
        styleApplied = true
        let style = """
            {"foregroundColor": "#e8e8ea", "backgroundColor": "#101014", "fontFamily": "Menlo", \
            "fontSize": \(Int(fontSize)), "blinkCursor": true, "cursorShape": "block"}
            """
        webView.evaluateJavaScript("exports.updateStyle(\(style))") { _, _ in
            completion()
        }
    }

    private func sendSize() {
        webView.evaluateJavaScript("exports.getSize()") { [weak self] result, _ in
            guard let size = result as? [NSNumber], size.count == 2 else { return }
            self?.controller?.resize(columns: size[0].intValue, rows: size[1].intValue)
        }
    }

    private static func jsonStringLiteral(_ text: String) -> String? {
        guard let data = try? JSONEncoder().encode(text) else { return nil }
        return String(data: data, encoding: .utf8)
    }
}

extension GuestTerminalUIView: WKNavigationDelegate {
    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        // The style is written when the terminal reports its size, but the
        // terminal has to look right even if it never does.
        applyStyle(fontSize: appliedFontSize, completion: {})
    }
}
