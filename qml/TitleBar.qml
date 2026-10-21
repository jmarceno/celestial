import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Window
import app.celestial

// Title bar for the frameless main window, the same as VerFSNext's.
Rectangle {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    required property Window window
    property string title: "Celestial"
    signal closeRequested()

    implicitHeight: 44
    color: root.theme.windowBg

    MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.LeftButton
        onPressed: root.window.startSystemMove()
        onDoubleClicked: {
            if (root.window.visibility === Window.Maximized)
                root.window.showNormal()
            else
                root.window.showMaximized()
        }
    }

    RowLayout {
        anchors.fill: parent
        anchors.leftMargin: 14
        anchors.rightMargin: 10
        spacing: 10

        Image {
            source: "qrc:/qt/qml/app/celestial/assets/com.unboundmark.Celestial-regular.svg"
            sourceSize: Qt.size(44, 44)
            Layout.preferredWidth: 22
            Layout.preferredHeight: 22
            fillMode: Image.PreserveAspectFit
        }

        Label {
            text: root.title
            color: root.theme.textPrimary
            font.pixelSize: 15
            font.bold: true
            Layout.fillWidth: true
        }

        Row {
            spacing: 4
            Repeater {
                model: ["min", "max", "close"]
                delegate: Rectangle {
                    required property string modelData
                    width: 32
                    height: 28
                    radius: 6
                    color: winBtn.containsMouse
                           ? (modelData === "close" ? "#e35d6a" : root.theme.cardBgRaised)
                           : "transparent"
                    Label {
                        anchors.centerIn: parent
                        text: modelData === "min" ? "—" : (modelData === "max" ? "□" : "✕")
                        color: root.theme.textSecondary
                        font.pixelSize: 12
                    }
                    MouseArea {
                        id: winBtn
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: {
                            if (modelData === "min")
                                root.window.showMinimized()
                            else if (modelData === "max") {
                                if (root.window.visibility === Window.Maximized)
                                    root.window.showNormal()
                                else
                                    root.window.showMaximized()
                            } else
                                root.closeRequested()
                        }
                    }
                }
            }
        }
    }
}
