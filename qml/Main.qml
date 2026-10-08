import QtQuick
import app.celestial

// QtObject has no default property. Every child is bound to a property.
QtObject {
    id: root

    property AppController controller: AppController {}

    property Shell shell: Shell {
        controller: root.controller
    }

    property TrayPopup trayPopup: TrayPopup {
        controller: root.controller
        onOpenRequested: {
            root.controller.showWindow()
            root.shell.show()
            root.shell.raise()
            root.shell.requestActivate()
        }
    }

    property Timer tickTimer: Timer {
        interval: 50
        running: true
        repeat: true
        onTriggered: root.controller.tick()
    }

    property Connections windowWatcher: Connections {
        target: root.controller
        function onWindow_visibleChanged() {
            if (root.controller.window_visible) {
                root.shell.show()
                root.shell.raise()
                root.shell.requestActivate()
            } else {
                root.shell.hide()
            }
        }
        function onTrayActivated(x, y) {
            root.trayPopup.toggleAt(x, y)
        }
        function onActivated() {
            root.shell.show()
            root.shell.raise()
            root.shell.requestActivate()
        }
    }

    Component.onCompleted: {
        Qt.application.quitOnLastWindowClosed = false
        root.controller.bootstrap()
    }
}
