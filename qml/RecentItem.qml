import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import app.celestial

// One recently synchronized file. Clicking opens it; the folder button opens
// the folder that contains it.
Rectangle {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    property var item: ({})
    property string when: ""
    property string glyph: ""
    readonly property bool deleted: item.action === "deleted_local" || item.action === "deleted_remote"
    implicitHeight: row.implicitHeight + 6

    function fileUrl(path) {
        return "file://" + path.split("/").map(encodeURIComponent).join("/")
    }
    radius: root.theme.radiusSm
    color: area.containsMouse && !root.deleted ? root.theme.cardBgRaised : root.theme.cardBg
    border.color: root.theme.borderSubtle

    RowLayout {
        id: row
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
        anchors.leftMargin: 6
        anchors.rightMargin: 4
        spacing: 6
        Label {
            text: root.glyph
            color: root.deleted ? root.theme.textDim : root.theme.accentStrong
            font.pixelSize: 12
            font.bold: true
            Layout.preferredWidth: 12
            horizontalAlignment: Text.AlignHCenter
        }
        Column {
            Layout.fillWidth: true
            spacing: 0
            Label {
                text: root.item.name || ""
                color: root.deleted ? root.theme.textMuted : root.theme.textPrimary
                font.pixelSize: 12
                elide: Text.ElideMiddle
                width: parent.width
            }
            Label {
                text: [root.item.label, root.item.sizeText, root.when].filter(function(part) { return part && part.length > 0 }).join(" · ")
                color: root.theme.textMuted
                font.pixelSize: 10
                elide: Text.ElideRight
                width: parent.width
            }
        }
        Button {
            text: "Folder"
            flat: true
            visible: root.item.folder && root.item.folder.length > 0
            contentItem: Label { text: parent.text; color: root.theme.accentStrong; font.pixelSize: 11 }
            background: Rectangle { color: "transparent" }
            onClicked: Qt.openUrlExternally(root.fileUrl(root.item.folder))
        }
    }
    MouseArea {
        id: area
        anchors.fill: parent
        anchors.rightMargin: 56
        hoverEnabled: true
        cursorShape: root.deleted ? Qt.ArrowCursor : Qt.PointingHandCursor
        onClicked: {
            if (!root.deleted)
                Qt.openUrlExternally(root.fileUrl(root.item.path))
        }
    }
}
