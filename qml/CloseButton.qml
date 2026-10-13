import QtQuick
import QtQuick.Controls
import app.celestial

// The "×" in a dialog's title bar.
Button {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    implicitWidth: 30
    implicitHeight: 30
    padding: 0
    Accessible.name: "Close"
    ToolTip.visible: hovered
    ToolTip.delay: 600
    ToolTip.text: "Close"
    contentItem: Label {
        text: "×"
        color: root.hovered ? root.theme.textPrimary : root.theme.textMuted
        font.pixelSize: 20
        horizontalAlignment: Text.AlignHCenter
        verticalAlignment: Text.AlignVCenter
    }
    background: Rectangle {
        radius: 4
        color: root.down ? root.theme.borderSubtle : (root.hovered ? root.theme.cardBgRaised : "transparent")
    }
}
