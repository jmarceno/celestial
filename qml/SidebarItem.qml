import QtQuick
import QtQuick.Controls
import app.celestial

Button {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    property bool selected: false
    implicitHeight: 38
    leftPadding: 14
    rightPadding: 12
    background: Rectangle {
        radius: root.theme.radiusSm
        color: root.selected ? root.theme.accentSoft : (root.hovered ? root.theme.cardBgRaised : "transparent")
        border.color: root.selected ? root.theme.accentMuted : "transparent"
        border.width: 1
    }
    contentItem: Label {
        text: root.text
        color: root.selected ? root.theme.textPrimary : root.theme.textSecondary
        font.pixelSize: 13
        font.bold: root.selected
        verticalAlignment: Text.AlignVCenter
        elide: Text.ElideRight
        leftPadding: 4
    }
}
