import QtQuick
import QtQuick.Controls
import app.celeste

Button {
    id: root
    property CelesteTheme theme: CelesteTheme {}
    property bool danger: false
    implicitHeight: 36
    leftPadding: 16
    rightPadding: 16
    background: Rectangle {
        radius: root.theme.radiusSm
        color: !root.enabled ? root.theme.cardBgRaised : (root.danger ? root.theme.danger : (root.hovered ? root.theme.accentStrong : root.theme.accent))
    }
    contentItem: Label {
        text: root.text
        color: root.theme.textPrimary
        font.pixelSize: 13
        font.bold: true
        horizontalAlignment: Text.AlignHCenter
        verticalAlignment: Text.AlignVCenter
    }
}
