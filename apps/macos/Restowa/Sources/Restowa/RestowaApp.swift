import AppKit
import SwiftUI

// View state lives in @Observable classes instead of @State: SwiftUI's @State
// is a macro whose plugin ships only with Xcode, and this package must build
// with the Command Line Tools alone.
@main
struct RestowaApp: App {
    private let settings: SettingsStore
    private let model: AppModel
    private let menuState = MenuState()
    private let settingsModel: SettingsModel

    init() {
        let settings = SettingsStore()
        let model = AppModel(settings: settings)
        self.settings = settings
        self.model = model
        settingsModel = SettingsModel(model: model)
        // LSUIElement covers the bundled app; this covers `swift run`.
        NSApplication.shared.setActivationPolicy(.accessory)
    }

    var body: some Scene {
        MenuBarExtra("Restowa", systemImage: "clock.arrow.circlepath") {
            MenuContentView(model: model, settings: settings, ui: menuState)
        }
        .menuBarExtraStyle(.window)

        Window("Restowa Settings", id: SettingsView.windowID) {
            SettingsView(vm: settingsModel, settings: settings)
        }
        .windowResizability(.contentSize)
    }
}

enum RowMode: Equatable {
    case normal
    case renaming
    case confirmingForget
}

/// Transient UI state for the menu panel. Only one row is edited at a time.
@MainActor
@Observable
final class MenuState {
    var query = ""
    var hoveredKey: String?
    var editingKey: String?
    var editMode = RowMode.normal
    var draftTitle = ""

    func mode(for key: String) -> RowMode {
        editingKey == key ? editMode : .normal
    }

    func begin(_ mode: RowMode, key: String, title: String = "") {
        editingKey = key
        editMode = mode
        draftTitle = title
    }

    func endEditing() {
        editingKey = nil
        editMode = .normal
    }
}
