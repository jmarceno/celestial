import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import app.celeste

ColumnLayout {
    id: root
    property CelesteTheme theme: CelesteTheme {}
    property string label: ""
    property alias text: field.text
    property bool password: false
    property string placeholder: ""
    spacing: 6
    implicitHeight: labelItem.implicitHeight + field.implicitHeight + 6

    Label {
        id: labelItem
        text: root.label
        color: root.theme.textSecondary
        font.pixelSize: 12
        font.bold: true
        visible: root.label.length > 0
    }
    TextField {
        id: field
        Layout.fillWidth: true
        placeholderText: root.placeholder
        echoMode: root.password ? TextInput.Password : TextInput.Normal
        color: root.theme.textPrimary
        placeholderTextColor: root.theme.textDim
        selectByMouse: true
        leftPadding: 12
        rightPadding: 12
        implicitHeight: 38
        background: Rectangle {
            radius: root.theme.radiusSm
            color: root.theme.inputBg
            border.color: field.activeFocus ? root.theme.accent : root.theme.borderSubtle
            border.width: 1
        }
    }
}
