import QtQuick
import QtQuick.Controls
import app.celestial

Button {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    property bool danger: false
    // The click was taken and its work is under way: spin and ignore clicks
    // until the result shows.
    property bool busy: false
    implicitHeight: 36
    leftPadding: 16
    rightPadding: 16
    background: Rectangle {
        radius: root.theme.radiusSm
        color: !root.enabled || root.busy ? root.theme.cardBgRaised : (root.danger ? root.theme.danger : (root.hovered ? root.theme.accentStrong : root.theme.accent))
    }
    contentItem: Item {
        implicitWidth: content.implicitWidth
        implicitHeight: content.implicitHeight
        Row {
            id: content
            anchors.centerIn: parent
            spacing: 8
            BusyIndicator {
                visible: root.busy
                running: root.busy
                width: 16
                height: 16
                padding: 0
                anchors.verticalCenter: parent.verticalCenter
            }
            Label {
                text: root.text
                color: root.busy ? root.theme.textSecondary : root.theme.textPrimary
                font.pixelSize: 13
                font.bold: true
                anchors.verticalCenter: parent.verticalCenter
            }
        }
    }
    // Above the button, so a busy button takes no second click.
    MouseArea {
        anchors.fill: parent
        visible: root.busy
        cursorShape: Qt.BusyCursor
    }
}
