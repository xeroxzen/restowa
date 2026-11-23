import AppKit
import SwiftUI

/// Reports when the hosting window becomes and stops being key. The
/// MenuBarExtra panel is key exactly while the menu is open, which is a more
/// reliable signal than onAppear/onDisappear for a window that is reused.
struct WindowVisibility: NSViewRepresentable {
    let onOpen: @MainActor () -> Void
    let onClose: @MainActor () -> Void

    func makeNSView(context: Context) -> TrackingView {
        let view = TrackingView()
        view.onOpen = onOpen
        view.onClose = onClose
        return view
    }

    func updateNSView(_ nsView: TrackingView, context: Context) {
        nsView.onOpen = onOpen
        nsView.onClose = onClose
    }

    final class TrackingView: NSView {
        var onOpen: (@MainActor () -> Void)?
        var onClose: (@MainActor () -> Void)?
        private var observers: [NSObjectProtocol] = []
        private var isOpen = false

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            observers.forEach(NotificationCenter.default.removeObserver)
            observers.removeAll()
            guard let window else {
                setOpen(false)
                return
            }
            let center = NotificationCenter.default
            observers = [
                center.addObserver(forName: NSWindow.didBecomeKeyNotification, object: window,
                                   queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.setOpen(true) }
                },
                center.addObserver(forName: NSWindow.didResignKeyNotification, object: window,
                                   queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated { self?.setOpen(false) }
                },
            ]
            setOpen(window.isKeyWindow)
        }

        private func setOpen(_ open: Bool) {
            guard open != isOpen else { return }
            isOpen = open
            open ? onOpen?() : onClose?()
        }
    }
}
