import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import app.celestial

ColumnLayout {
    id: root
    property CelestialTheme theme: CelestialTheme {}
    property string label: ""
    property alias text: field.text
    property bool password: false
    property string placeholder: ""
    /// Enter was pressed in the field, so a dialog can confirm.
    signal accepted()
    /// Select the whole value, so the person can type over it.
    function selectValue() {
        field.selectAll()
    }
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
        onAccepted: root.accepted()
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
