import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import app.celestial

// Progress of one pair, or of all of them, from the controller's progress JSON.
ColumnLayout {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    property var live: null
    property bool showCurrent: true
    spacing: 4

    Rectangle {
        Layout.fillWidth: true
        implicitHeight: 6
        radius: 3
        color: root.theme.cardBgRaised
        Rectangle {
            width: parent.width * Math.max(0, Math.min(1, root.live ? root.live.fraction : 0))
            height: parent.height
            radius: 3
            color: root.theme.accent
            Behavior on width { NumberAnimation { duration: 400 } }
        }
    }
    Label {
        text: root.live ? root.live.text : ""
        color: root.theme.textSecondary
        font.pixelSize: 12
        elide: Text.ElideRight
        Layout.fillWidth: true
    }
    Label {
        text: root.live && root.live.unknownSize > 0 ? ("+" + root.live.unknownSize + " files of unknown size") : ""
        visible: text.length > 0
        color: root.theme.textDim
        font.pixelSize: 11
        Layout.fillWidth: true
    }
    Label {
        text: root.live && root.live.current ? root.live.current : ""
        visible: root.showCurrent && text.length > 0
        color: root.theme.textDim
        font.pixelSize: 11
        elide: Text.ElideMiddle
        Layout.fillWidth: true
    }
}
