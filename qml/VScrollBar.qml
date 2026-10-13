import QtQuick
import QtQuick.Controls
import app.celestial

// Vertical scroll bar that stays visible whenever the content overflows.
ScrollBar {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    orientation: Qt.Vertical
    policy: root.size < 1.0 ? ScrollBar.AlwaysOn : ScrollBar.AlwaysOff
    minimumSize: 0.08
    contentItem: Rectangle {
        implicitWidth: 6
        radius: 3
        color: root.pressed ? root.theme.textMuted : root.theme.textDim
    }
    background: Rectangle {
        implicitWidth: 6
        radius: 3
        color: root.theme.cardBgRaised
    }
}
