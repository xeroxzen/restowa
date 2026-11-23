import RestowaKit
import SwiftUI

struct SettingsView: View {
    static let windowID = "settings"
    static let terminals = [("ghostty", "Ghostty"), ("terminal", "Terminal.app")]
    static let intervals = [(60, "1 minute"), (120, "2 minutes"), (300, "5 minutes")]

    let vm: SettingsModel
    @Bindable var settings: SettingsStore
    @FocusState private var pathFieldFocused: Bool

    var body: some View {
        Form {
            Section("Restore") {
                Picker("Terminal", selection: terminalBinding) {
                    ForEach(Self.terminals, id: \.0) { Text($0.1).tag($0.0) }
                }
                Picker("Scan every", selection: intervalBinding) {
                    ForEach(Self.intervals, id: \.0) { Text($0.1).tag($0.0) }
                }
            }
            .disabled(vm.config == nil)
            Section("App") {
                Toggle("Launch at login", isOn: Binding(
                    get: { vm.launchAtLogin }, set: { vm.setLaunchAtLogin($0) }))
                ForEach(Tool.allCases) { tool in
                    Toggle("Show \(tool.displayName)", isOn: Binding(
                        get: { settings.isToolShown(tool) },
                        set: { settings.setToolShown(tool, $0) }))
                }
            }
            Section("restowa binary") {
                TextField("Path override", text: $settings.binaryOverride,
                          prompt: Text("Auto-detect"))
                    .focused($pathFieldFocused)
                LabeledContent("Using", value: settings.resolvedBinaryPath ?? "not found")
                    .foregroundStyle(settings.resolvedBinaryPath == nil ? .orange : .secondary)
            }
            doctorSection
            if let note = vm.note {
                Text(note.text).foregroundStyle(note.isError ? .orange : .secondary)
            }
        }
        .formStyle(.grouped)
        .frame(width: 460)
        .frame(minHeight: 520)
        .defaultFocus($pathFieldFocused, false)
        .task { await vm.load() }
    }

    private var doctorSection: some View {
        Section("Diagnostics") {
            HStack {
                Button("Run doctor") { Task { await vm.runDoctor() } }
                    .disabled(vm.runningDoctor)
                if vm.runningDoctor { ProgressView().controlSize(.small) }
            }
            if let doctor = vm.doctor {
                ForEach(doctor.checks) { check in
                    LabeledContent {
                        Text(check.detail).font(.caption).textSelection(.enabled)
                    } label: {
                        Label(check.name, systemImage: icon(for: check.status))
                    }
                }
            }
        }
    }

    private func icon(for status: String) -> String {
        switch status {
        case "ok": "checkmark.circle"
        case "warn": "exclamationmark.triangle"
        default: "xmark.octagon"
        }
    }

    private var terminalBinding: Binding<String> {
        Binding(get: { vm.config?.terminal ?? "ghostty" },
                set: { value in Task { await vm.save(terminal: value) } })
    }

    private var intervalBinding: Binding<Int> {
        Binding(get: { vm.config?.scanIntervalSecs ?? 120 },
                set: { value in Task { await vm.save(interval: value) } })
    }
}
