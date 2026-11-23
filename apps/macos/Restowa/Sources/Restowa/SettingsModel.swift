import Foundation
import Observation
import RestowaKit

/// State and CLI calls behind the Settings window.
@MainActor
@Observable
final class SettingsModel {
    private(set) var config: AppConfig?
    private(set) var launchAtLogin = LoginItem.isEnabled
    private(set) var doctor: DoctorReport?
    private(set) var runningDoctor = false
    var note: StatusLine?

    private let model: AppModel

    init(model: AppModel) {
        self.model = model
    }

    func load() async {
        launchAtLogin = LoginItem.isEnabled
        do {
            config = try await model.client().config()
        } catch {
            note = StatusLine(text: error.localizedDescription, isError: true)
        }
    }

    func save(terminal: String? = nil, interval: Int? = nil) async {
        do {
            config = try await model.client().setConfig(
                terminal: terminal, scanIntervalSecs: interval)
            note = nil
        } catch {
            note = StatusLine(text: error.localizedDescription, isError: true)
        }
    }

    func setLaunchAtLogin(_ enabled: Bool) {
        do {
            let message = try LoginItem.setEnabled(enabled)
            note = message.map { StatusLine(text: $0, isError: false) }
        } catch {
            note = StatusLine(text: error.localizedDescription, isError: true)
        }
        launchAtLogin = LoginItem.isEnabled
    }

    func runDoctor() async {
        guard !runningDoctor else { return }
        runningDoctor = true
        defer { runningDoctor = false }
        do {
            doctor = try await model.client().doctor()
        } catch {
            note = StatusLine(text: error.localizedDescription, isError: true)
        }
    }
}
