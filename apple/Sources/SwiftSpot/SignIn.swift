import AppKit
import SwiftUI

/// Whether text has the shape of a Spotify Client ID: 32 hexadecimal digits.
func looksLikeClientId(_ text: String) -> Bool {
    let id = text.trimmingCharacters(in: .whitespacesAndNewlines)
    return id.count == 32 && id.allSatisfy(\.isHexDigit)
}

private let dashboard = URL(string: "https://developer.spotify.com/dashboard")!
private let fallbackRedirectUri = "http://127.0.0.1:8989/login"

// MARK: Signing in

struct SignInView: View {
    @Environment(AppModel.self) private var model
    @State private var clientId = ""
    @State private var showSteps: Bool?

    var body: some View {
        let hello = model.hello
        GeometryReader { window in
            ScrollView {
                VStack(spacing: 22) {
                    VStack(spacing: 10) {
                        Image(nsImage: NSApp.applicationIconImage)
                            .resizable()
                            .frame(width: 72, height: 72)
                        Text("Sign in to SwiftSpot").font(.largeTitle.weight(.bold))
                    }
                    switch hello?.auth {
                    case "waiting_for_browser":
                        waiting(hello)
                    case "connecting", "starting":
                        ProgressView().controlSize(.large)
                    default:
                        form(hello)
                    }
                }
                .frame(maxWidth: 440)
                .padding(40)
                .frame(maxWidth: .infinity, minHeight: window.size.height)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .onAppear { clientId = hello?.clientId ?? "" }
        .onChange(of: hello?.clientId) { _, saved in
            if clientId.isEmpty, let saved { clientId = saved }
        }
        .task {
            // The auth event says when it is done; asking keeps the page
            // current through the browser round trip too.
            while !Task.isCancelled, model.hello?.signedIn != true {
                if let hello = try? await model.call("hello", as: Hello.self) {
                    model.hello = hello
                    if hello.signedIn { await model.refreshAll() }
                }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func waiting(_ hello: Hello?) -> some View {
        VStack(spacing: 14) {
            ProgressView().controlSize(.large)
            Text("Finish signing in in your browser.")
                .font(.title3)
            if let url = hello?.signInUrl.flatMap(URL.init(string:)) {
                Link("Open the sign-in page again", destination: url)
            }
            Text(
                "If Spotify says “Invalid redirect URI”, add \(hello?.redirectUri ?? fallbackRedirectUri) to your app exactly, save, and try again."
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.center)
            .textSelection(.enabled)
            Button("Cancel") { model.send("auth.cancel") }
                .controlSize(.large)
        }
    }

    private func form(_ hello: Hello?) -> some View {
        let saved = hello?.clientId.map(looksLikeClientId) ?? false
        let problem = hello?.signInProblem
        let needsSetup =
            !saved || ["malformed_client_id", "unknown_client", "shared_retired"].contains(problem)
        let steps = showSteps ?? needsSetup
        return VStack(spacing: 18) {
            if let error = hello?.authError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                    .multilineTextAlignment(.leading)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if steps {
                SpotifyAppSteps(clientId: $clientId, redirectUri: hello?.redirectUri)
            } else {
                Text("SwiftSpot signs in through your own Spotify app.")
                    .foregroundStyle(.secondary)
            }
            VStack(spacing: 10) {
                Button {
                    model.hello?.auth = "starting"
                    model.signIn(clientId: clientId.trimmingCharacters(in: .whitespacesAndNewlines))
                } label: {
                    Text("Sign In with Spotify").frame(maxWidth: .infinity)
                }
                .buttonStyle(.glassProminent)
                .controlSize(.extraLarge)
                .disabled(!looksLikeClientId(clientId))
                .keyboardShortcut(.defaultAction)
                Text("Your password goes only to Spotify, in your browser. Playback needs Premium.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                Button(steps ? "Hide setup steps" : "Use a different Spotify app") {
                    withAnimation { showSteps = !steps }
                }
                .buttonStyle(.link)
                .opacity(needsSetup ? 0 : 1)
                .disabled(needsSetup)
            }
        }
    }
}

// MARK: Setting up a Spotify app

/// The steps for making a Spotify app of one's own, ending with its Client
/// ID typed into `clientId`.
struct SpotifyAppSteps: View {
    @Binding var clientId: String
    var redirectUri: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Step(number: 1) {
                Text("Open Spotify’s developer dashboard and choose Create app. Any name will do.")
                Link(destination: dashboard) {
                    Label("developer.spotify.com/dashboard", systemImage: "arrow.up.right.square")
                }
            }
            Step(number: 2) {
                Text("Under Redirect URIs, add exactly:")
                CopyField(text: redirectUri ?? fallbackRedirectUri)
            }
            Step(number: 3) {
                Text("Under APIs used, tick Web API, accept the terms, and save.")
            }
            Step(number: 4) {
                Text("Paste the app’s Client ID, shown on its Basic Information page.")
                ClientIdField(text: $clientId)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct Step<Content: View>: View {
    var number: Int
    @ViewBuilder var content: Content

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: "\(number).circle.fill")
                .font(.title3)
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 8) {
                content
            }
            .fixedSize(horizontal: false, vertical: true)
        }
    }
}

/// Text to paste somewhere else, with a button that copies it.
struct CopyField: View {
    var text: String
    @State private var copied = false

    var body: some View {
        HStack(spacing: 8) {
            Text(text)
                .font(.body.monospaced())
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
                copied = true
                Task {
                    try? await Task.sleep(for: .seconds(1.5))
                    copied = false
                }
            } label: {
                Label(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc")
                    .frame(minWidth: 64)
            }
            .controlSize(.small)
        }
        .padding(.leading, 10)
        .padding(.trailing, 6)
        .padding(.vertical, 6)
        .background(.quaternary.opacity(0.6), in: .rect(cornerRadius: 8))
    }
}

/// A field for a Spotify Client ID that says when what is typed can't be one.
struct ClientIdField: View {
    @Binding var text: String

    var body: some View {
        let typed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        VStack(alignment: .leading, spacing: 4) {
            TextField("Client ID", text: $text, prompt: Text("Client ID"))
                .textFieldStyle(.roundedBorder)
                .autocorrectionDisabled()
                .labelsHidden()
            if !typed.isEmpty && !looksLikeClientId(typed) {
                Text("A Client ID is 32 letters and digits.")
                    .font(.callout)
                    .foregroundStyle(.red)
            }
        }
    }
}

// MARK: Settings

/// Settings ▸ Account: who is signed in, and the Spotify app they sign in
/// through.
struct AccountSettings: View {
    @Environment(AppModel.self) private var model
    @State private var changing = false

    var body: some View {
        let hello = model.hello
        Form {
            if let user = hello?.user {
                Section {
                    HStack(spacing: 12) {
                        Artwork(url: user.image, size: 40, placeholder: "person.fill", circle: true)
                        VStack(alignment: .leading) {
                            Text(user.name).font(.headline)
                            if let product = user.product {
                                Text("Spotify \(product.capitalized)").foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        Button("Sign Out") { model.signOut() }
                    }
                }
            }
            Section("Spotify app") {
                if hello?.sharedGrant == true && hello?.webApp == nil {
                    Label(
                        "You’re signed in through the shared app SwiftSpot used before. Spotify can stop accepting it at any time, so set up your own.",
                        systemImage: "exclamationmark.triangle.fill"
                    )
                    .foregroundStyle(.orange)
                }
                LabeledContent("Client ID") {
                    Text(hello?.webApp ?? hello?.clientId ?? "None")
                        .font(.body.monospaced())
                        .textSelection(.enabled)
                        .foregroundStyle(hello?.webApp == nil ? .secondary : .primary)
                }
                HStack {
                    Spacer()
                    Button(hello?.webApp == nil ? "Set Up…" : "Change…") { changing = true }
                }
            }
        }
        .formStyle(.grouped)
        .task {
            // A new app is verified after the browser hands it back.
            while !Task.isCancelled {
                if let hello = try? await model.call("hello", as: Hello.self) { model.hello = hello }
                try? await Task.sleep(for: .seconds(2))
            }
        }
        .sheet(isPresented: $changing) {
            ChangeSpotifyApp(current: hello?.webApp ?? hello?.clientId, redirectUri: hello?.redirectUri)
        }
    }
}

/// The steps again, for moving to another Spotify app while signed in. The
/// browser opens to authorize it, as signing in does.
private struct ChangeSpotifyApp: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    var current: String?
    var redirectUri: String?
    @State private var clientId = ""
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            Text("Use a different Spotify app").font(.title2.weight(.bold))
            SpotifyAppSteps(clientId: $clientId, redirectUri: redirectUri)
            if let error {
                Label(error, systemImage: "exclamationmark.triangle.fill").foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Authorize in Browser") {
                    Task {
                        do {
                            _ = try await model.call(
                                "auth.use_client_id",
                                ["client_id": clientId.trimmingCharacters(in: .whitespacesAndNewlines)],
                                as: OK.self)
                            dismiss()
                        } catch {
                            self.error = error.localizedDescription
                        }
                    }
                }
                .buttonStyle(.glassProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(
                    !looksLikeClientId(clientId)
                        || clientId.trimmingCharacters(in: .whitespacesAndNewlines) == current)
            }
        }
        .padding(24)
        .frame(width: 460)
    }
}

private struct OK: Decodable {}
