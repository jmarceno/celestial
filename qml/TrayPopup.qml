import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import app.celestial

// Compact status shown when the tray icon is clicked: total progress and the
// latest synchronized files. On Wayland the compositor chooses where it
// appears; on X11 it opens next to the icon.
Window {
    id: root
    property AppController controller
    property CelestialTheme theme: CelestialTheme {}
    property var snapshot: ({ remotes: [] })
    property var progress: ({ pairs: [], total: null })
    property int clock: 0
    // When focus loss last hid the window: the click on the icon that
    // caused it must not show it again.
    property double hiddenAt: 0

    signal openRequested()

    width: 380
    height: Math.min(560, content.implicitHeight + 32)
    minimumHeight: 160
    visible: false
    flags: Qt.Tool | Qt.FramelessWindowHint | Qt.WindowStaysOnTopHint
    color: root.theme.windowBg
    title: "Celestial"

    function parse(value, fallback) {
        try { return JSON.parse(value) } catch (error) { return fallback }
    }
    function ago(at, clock) {
        var seconds = Math.max(0, Math.floor(Date.now() / 1000) - at)
        if (seconds < 60) return "just now"
        if (seconds < 3600) return Math.floor(seconds / 60) + " min ago"
        if (seconds < 86400) return Math.floor(seconds / 3600) + " h ago"
        return Math.floor(seconds / 86400) + " d ago"
    }
    function actionGlyph(action) {
        if (action === "uploaded") return "↑"
        if (action === "downloaded") return "↓"
        if (action === "conflict") return "⇄"
        return "✕"
    }
    // The latest changes of every pair, newest first.
    function recent() {
        var all = []
        var remotes = root.snapshot.remotes || []
        for (var i = 0; i < remotes.length; i++) {
            var dirs = remotes[i].dirs || []
            for (var j = 0; j < dirs.length; j++)
                all = all.concat(dirs[j].recent || [])
        }
        all.sort(function(a, b) { return b.at - a.at })
        return all.slice(0, 8)
    }
    function toggleAt(x, y) {
        if (root.visible) {
            root.hide()
            return
        }
        if (Date.now() - root.hiddenAt < 400)
            return
        root.snapshot = root.parse(root.controller.snapshot_json, { remotes: [] })
        root.progress = root.parse(root.controller.progress_json, { pairs: [], total: null })
        if (x > 0 || y > 0) {
            var area = root.screen
            var left = area ? area.virtualX : 0
            var top = area ? area.virtualY : 0
            var right = area ? left + area.width : x + root.width
            var bottom = area ? top + area.height : y + root.height
            root.x = Math.max(left + 8, Math.min(x - root.width / 2, right - root.width - 8))
            // A panel at the bottom opens the window above the icon.
            root.y = y > (top + bottom) / 2 ? Math.max(top + 8, y - root.height - 12) : Math.min(y + 12, bottom - root.height - 8)
        }
        root.show()
        root.raise()
        root.requestActivate()
    }

    onActiveChanged: {
        if (!root.active && root.visible) {
            root.hiddenAt = Date.now()
            root.hide()
        }
    }

    property Connections watcher: Connections {
        target: root.controller
        function onSnapshot_jsonChanged() {
            if (root.visible)
                root.snapshot = root.parse(root.controller.snapshot_json, { remotes: [] })
        }
        function onProgress_jsonChanged() {
            if (root.visible)
                root.progress = root.parse(root.controller.progress_json, { pairs: [], total: null })
        }
    }

    property Timer clockTimer: Timer {
        interval: 30000
        running: root.visible
        repeat: true
        onTriggered: root.clock++
    }

    Rectangle {
        anchors.fill: parent
        color: "transparent"
        border.color: root.theme.borderSubtle
        border.width: 1
        radius: root.theme.radius
    }

    ColumnLayout {
        id: content
        anchors.fill: parent
        anchors.margins: 16
        spacing: 10

        RowLayout {
            Layout.fillWidth: true
            spacing: 8
            Image {
                source: "qrc:/qt/qml/app/celestial/assets/com.unboundmark.Celestial-regular.svg"
                sourceSize: Qt.size(48, 48)
                Layout.preferredWidth: 24
                Layout.preferredHeight: 24
                fillMode: Image.PreserveAspectFit
            }
            Label {
                text: "Celestial"
                color: root.theme.textPrimary
                font.pixelSize: 15
                font.bold: true
                Layout.fillWidth: true
            }
            CloseButton {
                theme: root.theme
                onClicked: root.hide()
            }
        }
        // Status and recent changes scroll; the header and button stay put.
        Flickable {
            id: trayFlick
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.preferredHeight: trayBody.implicitHeight
            contentHeight: trayBody.implicitHeight
            clip: true
            boundsBehavior: Flickable.StopAtBounds
            ScrollBar.vertical: VScrollBar { theme: root.theme }

            ColumnLayout {
                id: trayBody
                width: trayFlick.width - (trayFlick.ScrollBar.vertical.visible ? 12 : 0)
                spacing: 4

                Label {
                    text: root.snapshot.trayMessage || "Awaiting sync checks..."
                    color: root.theme.textMuted
                    font.pixelSize: 12
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                }
                TransferProgress {
                    visible: root.progress.total !== null && root.progress.total !== undefined
                    live: root.progress.total
                    showCurrent: false
                    theme: root.theme
                    Layout.fillWidth: true
                }
                Repeater {
                    model: (root.progress.pairs || []).length
                    Label {
                        required property int index
                        // Counted, not a list: an update keeps the items instead of recreating them
                        readonly property var entry: (root.progress.pairs || [])[index] || ({})
                        text: entry.current ? entry.current : entry.localPath
                        color: root.theme.textDim
                        font.pixelSize: 11
                        elide: Text.ElideMiddle
                        Layout.fillWidth: true
                    }
                }
                Label {
                    text: "Recent changes"
                    color: root.theme.textSecondary
                    font.pixelSize: 11
                    font.bold: true
                    Layout.topMargin: 2
                    visible: recentList.count > 0
                }
                Label {
                    text: "Nothing synchronized yet."
                    color: root.theme.textDim
                    font.pixelSize: 12
                    visible: recentList.count === 0
                }
                ListView {
                    id: recentList
                    Layout.fillWidth: true
                    spacing: 2
                    clip: true
                    model: root.recent().length
                    delegate: RecentItem {
                        required property int index
                        readonly property var entry: root.recent()[index] || ({})
                        item: entry
                        theme: root.theme
                        when: root.ago(entry.at, root.clock)
                        glyph: root.actionGlyph(entry.action)
                        width: ListView.view.width
                    }
                }
            }
        }
        PrimaryButton {
            text: "Open Celestial"
            Layout.fillWidth: true
            Layout.topMargin: 4
            onClicked: {
                root.hide()
                root.openRequested()
            }
        }
    }
}
