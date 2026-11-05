// A default URL handler that diverts only T3 Code's GitHub PR links to PR Inbox.
// Every other link goes directly to the browser saved before enabling the router.
import AppKit
import CoreServices

let routerID = "io.github.i-zaykov.pr-inbox.link-router"
let inboxID = "io.github.i-zaykov.pr-inbox"
let support = FileManager.default.homeDirectoryForCurrentUser
    .appendingPathComponent("Library/Application Support/PR Inbox Link Router")
let configURL = support.appendingPathComponent("browsers.json")

struct Browser: Codable {
    let bundleID: String
    let path: String
}

func canonicalPR(_ url: URL) -> URL? {
    let scheme = url.scheme?.lowercased() ?? ""
    guard ["https", "http"].contains(scheme),
          url.host?.lowercased() == "github.com",
          url.user == nil, url.password == nil,
          url.port == nil || url.port == (scheme == "https" ? 443 : 80) else { return nil }
    let path = url.path
    guard let match = path.range(of: #"^/[A-Za-z0-9_-]+/[A-Za-z0-9_.-]+/pull/[1-9][0-9]*(?:/|$)"#,
                                options: .regularExpression) else { return nil }
    let parts = path[match].split(separator: "/")
    return URL(string: "https://github.com/\(parts[0])/\(parts[1])/pull/\(parts[3])")
}

func inboxURL(_ url: URL, source: String?) -> URL? {
    guard source == "com.t3tools.t3code",
          let pr = canonicalPR(url) else { return nil }
    var link = URLComponents()
    link.scheme = "pr-inbox"
    link.host = "open"
    link.queryItems = [URLQueryItem(name: "url", value: pr.absoluteString)]
    return link.url
}

func currentBrowser(_ scheme: String) -> Browser? {
    guard let url = NSWorkspace.shared.urlForApplication(toOpen: URL(string: "\(scheme)://example.com")!),
          let bundleID = Bundle(url: url)?.bundleIdentifier else { return nil }
    return Browser(bundleID: bundleID, path: url.path)
}

func readBrowsers() throws -> [String: Browser] {
    try JSONDecoder().decode([String: Browser].self, from: Data(contentsOf: configURL))
}

func writeBrowsers(_ browsers: [String: Browser]) throws {
    try FileManager.default.createDirectory(at: support, withIntermediateDirectories: true)
    try JSONEncoder().encode(browsers).write(to: configURL, options: .atomic)
}

func requiresBrowserDefault(_ browser: Browser) -> Bool {
    let key = "DefaultBrowserSettingEnabled" as CFString
    let appID = browser.bundleID as CFString
    return CFPreferencesAppValueIsForced(key, appID)
        && (CFPreferencesCopyAppValue(key, appID) as? Bool) == true
}

func setHandler(_ scheme: String, _ bundleID: String) throws {
    if currentBrowser(scheme)?.bundleID == bundleID { return }
    guard let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: bundleID) else {
        throw NSError(domain: "PRInboxLinkRouter", code: 3, userInfo: [
            NSLocalizedDescriptionKey: "Application \(bundleID) is not installed."
        ])
    }
    var result: Result<Void, Error>?
    NSWorkspace.shared.setDefaultApplication(at: app, toOpenURLsWithScheme: scheme) { error in
        DispatchQueue.main.async {
            result = error.map { .failure($0) } ?? .success(())
        }
    }
    while result == nil { RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.05)) }
    try result!.get()
    guard currentBrowser(scheme)?.bundleID == bundleID else {
        throw NSError(domain: "PRInboxLinkRouter", code: 4, userInfo: [
            NSLocalizedDescriptionKey: "The \(scheme) handler did not change to \(bundleID)."
        ])
    }
}

func enable() throws {
    var saved = (try? readBrowsers()) ?? [:]
    for scheme in ["http", "https"] {
        guard let current = currentBrowser(scheme) else {
            throw NSError(domain: "PRInboxLinkRouter", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "No current browser for \(scheme); refusing to replace it."
            ])
        }
        if current.bundleID != routerID { saved[scheme] = current }
        guard let fallback = saved[scheme], fallback.bundleID != routerID else {
            throw NSError(domain: "PRInboxLinkRouter", code: 2, userInfo: [
                NSLocalizedDescriptionKey: "No saved browser for \(scheme)."
            ])
        }
        guard !requiresBrowserDefault(fallback) else {
            throw NSError(domain: "PRInboxLinkRouter", code: 5, userInfo: [
                NSLocalizedDescriptionKey: "Cannot enable the router: a managed policy requires \(fallback.bundleID) to be the default browser. Replacing it causes repeated macOS browser prompts. Keep that browser as the default."
            ])
        }
    }
    try writeBrowsers(saved)
    do {
        for scheme in ["http", "https"] { try setHandler(scheme, routerID) }
    } catch {
        for scheme in ["http", "https"] where currentBrowser(scheme)?.bundleID == routerID {
            try? setHandler(scheme, saved[scheme]!.bundleID)
        }
        throw error
    }
    print("Enabled. T3 Code PR links open in PR Inbox; other links use the saved browsers.")
}

func restore() throws {
    let saved = try readBrowsers()
    for scheme in ["http", "https"] where currentBrowser(scheme)?.bundleID == routerID {
        guard let browser = saved[scheme] else { continue }
        try setHandler(scheme, browser.bundleID)
    }
    print("Restored the saved browser defaults. Other browser changes were left alone.")
}

func selfTest() {
    let t3 = "com.t3tools.t3code"
    let valid = "https://github.com/org/repo/pull/42"
    let cases: [(String, String?, Bool)] = [
        (valid, t3, true), (valid + "/files#diff-123", t3, true),
        (valid + "?tab=commits", t3, true), ("http://github.com/org/repo/pull/42/", t3, true),
        ("HTTPS://github.com:443/org/repo/pull/42", t3, true),
        ("HTTP://github.com:80/org/repo/pull/42", t3, true),
        (valid, "com.google.Chrome", false), (valid, nil, false),
        (valid, "com.t3tools.t3code.fake", false), ("https://github.com/org/repo", t3, false),
        ("https://github.com/org/repo/issues/42", t3, false),
        ("https://github.com/org/repo/pull/0", t3, false),
        ("https://github.com/org/repo/pull/42oops", t3, false),
        ("https://github.com.evil.test/org/repo/pull/42", t3, false),
        ("https://github.com@evil.test/org/repo/pull/42", t3, false),
        ("https://user@github.com/org/repo/pull/42", t3, false),
        ("https://github.com:8443/org/repo/pull/42", t3, false),
        ("file:///org/repo/pull/42", t3, false)
    ]
    for (raw, source, expected) in cases {
        precondition((inboxURL(URL(string: raw)!, source: source) != nil) == expected, raw)
    }
    let deep = inboxURL(URL(string: valid + "/files?x=y#z")!, source: t3)!
    precondition(URLComponents(url: deep, resolvingAgainstBaseURL: false)?.queryItems?.first?.value == valid)
    print("Passed \(cases.count + 1) routing checks.")
}

final class Router: NSObject, NSApplicationDelegate {
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSAppleEventManager.shared().setEventHandler(self, andSelector: #selector(handleURL(_:reply:)),
                                                     forEventClass: AEEventClass(kInternetEventClass),
                                                     andEventID: AEEventID(kAEGetURL))
    }

    @objc func handleURL(_ event: NSAppleEventDescriptor, reply: NSAppleEventDescriptor) {
        guard let raw = event.paramDescriptor(forKeyword: AEKeyword(keyDirectObject))?.stringValue,
              let url = URL(string: raw), ["http", "https"].contains(url.scheme?.lowercased() ?? "") else { return }
        let source = event.attributeDescriptor(forKeyword: AEKeyword(keySenderPIDAttr))
            .flatMap { NSRunningApplication(processIdentifier: pid_t($0.int32Value))?.bundleIdentifier }
        if let deepLink = inboxURL(url, source: source),
           let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: inboxID) {
            record(source: source, destination: "PR Inbox")
            NSWorkspace.shared.open([deepLink], withApplicationAt: app,
                                    configuration: NSWorkspace.OpenConfiguration()) { [weak self] _, error in
                if error != nil { self?.openBrowser(url, source: source) }
            }
        } else {
            openBrowser(url, source: source)
        }
    }

    private func openBrowser(_ url: URL, source: String?) {
        // Explicit application selection prevents forwarding back into this default handler.
        let browser = (try? readBrowsers())?[url.scheme?.lowercased() ?? "https"]
        let installed = browser.flatMap { NSWorkspace.shared.urlForApplication(withBundleIdentifier: $0.bundleID) }
        let savedPath = browser.map { URL(fileURLWithPath: $0.path) }
            .flatMap { FileManager.default.fileExists(atPath: $0.path) ? $0 : nil }
        let app = installed ?? savedPath ?? NSWorkspace.shared.urlForApplication(withBundleIdentifier: "com.apple.Safari")
        guard let app, Bundle(url: app)?.bundleIdentifier != routerID else {
            NSLog("PR Inbox Link Router: no fallback browser available")
            return
        }
        record(source: source, destination: Bundle(url: app)?.bundleIdentifier ?? "browser")
        NSWorkspace.shared.open([url], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration()) { _, error in
            if let error { NSLog("PR Inbox Link Router: could not open browser: %@", error.localizedDescription) }
        }
    }

    private func record(source: String?, destination: String) {
        // Keep just the last decision for diagnosis, without URLs or browsing history.
        let entry = ["source": source ?? "unknown", "destination": destination,
                     "time": ISO8601DateFormatter().string(from: Date())]
        if let data = try? JSONEncoder().encode(entry) {
            try? data.write(to: support.appendingPathComponent("last-route.json"), options: .atomic)
        }
    }
}

do {
    switch CommandLine.arguments.dropFirst().first {
    case "--enable": try enable()
    case "--restore": try restore()
    case "--status":
        for scheme in ["http", "https"] { print("\(scheme): \(currentBrowser(scheme)?.bundleID ?? "none")") }
    case "--self-test": selfTest()
    default:
        let app = NSApplication.shared
        let router = Router()
        app.delegate = router
        app.setActivationPolicy(.accessory)
        withExtendedLifetime(router) { app.run() }
    }
} catch {
    fputs("\(error.localizedDescription)\n", stderr)
    exit(1)
}
