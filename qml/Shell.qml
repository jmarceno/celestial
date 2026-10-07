import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import QtQuick.Dialogs
import app.celestial

ApplicationWindow {
    id: root
    property AppController controller
    property CelestialTheme theme: CelestialTheme {}
    width: 980
    height: 680
    minimumWidth: 860
    minimumHeight: 560
    visible: false
    title: "Celestial"
    color: theme.windowBg

    property var snapshot: ({ remotes: [] })
    property string selectedName: ""
    property var detail: null
    property string exclusionDraft: ""
    property var suggestions: []
    property string popupMode: ""
    property string popupTitle: ""
    property string popupBody: ""
    property var popupRef: ({})
    property string loginKind: "gdrive"
    property bool totpEnabled: false
    property string localDraft: ""
    property string remoteDraft: "/"

    function parse(value, fallback) {
        try { return JSON.parse(value) } catch (error) { return fallback }
    }
    function remotes() {
        return root.snapshot.remotes || []
    }
    function selectedRemote() {
        var list = root.remotes()
        for (var i = 0; i < list.length; i++) {
            if (list[i].name === root.selectedName) return list[i]
        }
        return list.length > 0 ? list[0] : null
    }
    function phaseColor(phase) {
        if (phase === "done") return root.theme.statusGreen
        if (phase === "syncing") return root.theme.accent
        if (phase === "warning") return root.theme.warning
        return root.theme.textDim
    }
    function openMessage(title, body) {
        root.popupMode = "message"
        root.popupTitle = title
        root.popupBody = body
        dialog.open()
    }
    function closeDialog() {
        dialog.close()
        root.popupMode = ""
    }

    onClosing: function(close) {
        close.accepted = false
        root.controller.hideWindow()
    }

    property Connections snapshotWatcher: Connections {
        target: root.controller
        function onSnapshot_jsonChanged() {
            root.snapshot = root.parse(root.controller.snapshot_json, { remotes: [] })
            if (root.selectedName.length === 0 && root.remotes().length > 0)
                root.selectedName = root.remotes()[0].name
            if (!root.detail)
                return
            var remote = root.selectedRemote()
            if (!remote) {
                root.detail = null
                return
            }
            var dirs = remote.dirs || []
            for (var i = 0; i < dirs.length; i++) {
                if (dirs[i].localPath === root.detail.localPath && dirs[i].remotePath === root.detail.remotePath) {
                    root.detail = dirs[i]
                    return
                }
            }
            root.detail = null
        }
        function onSuggestions_jsonChanged() {
            root.suggestions = root.parse(root.controller.suggestions_json, [])
        }
        function onNotice(kind, title, body) {
            if (kind === "login") {
                root.popupMode = "login"
                dialog.open()
                return
            }
            if (kind === "login-ok") {
                root.selectedName = body
                root.closeDialog()
                return
            }
            if (kind === "auth") {
                root.popupMode = "auth"
                root.popupTitle = title
                root.popupBody = body
                dialog.open()
                return
            }
            if (kind === "auth-done") {
                if (root.popupMode === "auth") root.closeDialog()
                return
            }
            if (kind === "conflict-info" || kind === "confirm-dismiss") {
                root.popupMode = kind
                root.popupTitle = title
                root.popupRef = root.parse(body, {})
                root.popupBody = kind === "conflict-info" ? (root.popupRef.text || "") : ""
                dialog.open()
                return
            }
            if (kind === "fatal") {
                root.popupMode = "fatal"
                root.popupTitle = title
                root.popupBody = body
                dialog.open()
                return
            }
            if (kind === "error" || kind === "info" || kind === "code") {
                root.openMessage(title, body)
            }
        }
    }

    property FolderDialog folderDialog: FolderDialog {
        title: "Local folder"
        onAccepted: {
            var path = folderDialog.selectedFolder.toString()
            if (path.indexOf("file://") === 0) path = path.substring(7)
            root.localDraft = decodeURIComponent(path)
        }
    }

    RowLayout {
        anchors.fill: parent
        spacing: 0

        Rectangle {
            Layout.fillHeight: true
            Layout.preferredWidth: root.theme.sidebarWidth
            color: root.theme.windowBg
            Rectangle { width: 1; height: parent.height; x: parent.width - 1; color: root.theme.borderSubtle }

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 14
                spacing: 4
                RowLayout {
                    Layout.fillWidth: true
                    Layout.bottomMargin: 18
                    spacing: 10
                    Image {
                        source: "qrc:/qt/qml/app/celestial/assets/com.unboundmark.Celestial-regular.svg"
                        sourceSize: Qt.size(80, 80)
                        Layout.preferredWidth: 40
                        Layout.preferredHeight: 40
                        fillMode: Image.PreserveAspectFit
                    }
                    Label {
                        text: "Celestial"
                        color: root.theme.textPrimary
                        font.pixelSize: 19
                        font.bold: true
                    }
                }
                Label {
                    text: "SERVERS"
                    color: root.theme.textDim
                    font.pixelSize: 10
                    font.bold: true
                    font.letterSpacing: 1.1
                }
                Label {
                    text: root.snapshot.trayMessage || "Awaiting sync checks..."
                    color: root.theme.textMuted
                    font.pixelSize: 11
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                }
                Item { Layout.preferredHeight: 8 }
                Repeater {
                    model: root.remotes()
                    SidebarItem {
                        required property var modelData
                        text: modelData.name
                        selected: modelData.name === root.selectedName
                        width: root.theme.sidebarWidth - 28
                        onClicked: {
                            root.selectedName = modelData.name
                            root.detail = null
                        }
                    }
                }
                Item { Layout.fillHeight: true }
                PrimaryButton {
                    text: "Add server"
                    Layout.fillWidth: true
                    onClicked: {
                        root.popupMode = "login"
                        dialog.open()
                    }
                }
                Button {
                    text: "Quit"
                    flat: true
                    Layout.fillWidth: true
                    contentItem: Label { text: "Quit"; color: root.theme.textMuted; font.pixelSize: 12; horizontalAlignment: Text.AlignHCenter }
                    background: Rectangle { color: "transparent" }
                    onClicked: root.controller.quit()
                }
            }
        }

        Rectangle {
            Layout.fillWidth: true
            Layout.fillHeight: true
            color: root.theme.windowBg

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 22
                spacing: 14
                visible: root.detail === null

                RowLayout {
                    Layout.fillWidth: true
                    Label {
                        text: root.selectedRemote() ? root.selectedRemote().name : "Celestial"
                        color: root.theme.textPrimary
                        font.pixelSize: 22
                        font.bold: true
                        Layout.fillWidth: true
                    }
                    PrimaryButton {
                        text: "Add folder"
                        visible: root.selectedRemote() !== null
                        onClicked: {
                            root.localDraft = ""
                            root.remoteDraft = "/"
                            root.suggestions = []
                            root.popupMode = "add"
                            dialog.open()
                            root.controller.listDirs(root.selectedName, "/")
                        }
                    }
                    PrimaryButton {
                        text: "Remove server"
                        danger: true
                        visible: root.selectedRemote() !== null
                        onClicked: {
                            root.popupMode = "confirm-remote"
                            root.popupTitle = "Are you sure you want to delete this remote?"
                            root.popupBody = "All the directories associated with this remote will also stop syncing."
                            dialog.open()
                        }
                    }
                }
                Label {
                    text: "Directories"
                    color: root.theme.textSecondary
                    font.pixelSize: 13
                    font.bold: true
                    visible: root.selectedRemote() !== null
                }
                Label {
                    text: "Add a server to start syncing a folder."
                    color: root.theme.textMuted
                    font.pixelSize: 14
                    visible: root.remotes().length === 0
                }
                Flickable {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    contentHeight: dirCol.implicitHeight
                    clip: true
                    Column {
                        id: dirCol
                        width: parent.width
                        spacing: 8
                        Repeater {
                            model: root.selectedRemote() ? root.selectedRemote().dirs : []
                            Rectangle {
                                required property var modelData
                                width: dirCol.width
                                height: 72
                                radius: root.theme.radius
                                color: root.theme.cardBg
                                border.color: root.theme.borderSubtle
                                border.width: 1
                                RowLayout {
                                    anchors.fill: parent
                                    anchors.margins: 14
                                    spacing: 12
                                    Rectangle {
                                        width: 10
                                        height: 10
                                        radius: 5
                                        color: root.phaseColor(modelData.phase)
                                    }
                                    ColumnLayout {
                                        Layout.fillWidth: true
                                        spacing: 2
                                        Label {
                                            text: modelData.localDisplay + "   →   " + modelData.remoteDisplay
                                            color: root.theme.textPrimary
                                            font.pixelSize: 13
                                            elide: Text.ElideMiddle
                                            Layout.fillWidth: true
                                        }
                                        Label {
                                            text: (modelData.errorStatus ? modelData.errorStatus + " " : "") + (modelData.status || "")
                                            color: modelData.errorStatus ? root.theme.danger : root.theme.textMuted
                                            font.pixelSize: 12
                                            elide: Text.ElideRight
                                            Layout.fillWidth: true
                                        }
                                    }
                                    Label { text: "›"; color: root.theme.textDim; font.pixelSize: 18 }
                                }
                                MouseArea {
                                    anchors.fill: parent
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: {
                                        root.detail = modelData
                                        exclusionArea.text = (modelData.exclusions || []).join("\n")
                                    }
                                }
                            }
                        }
                    }
                }
            }

            ColumnLayout {
                anchors.fill: parent
                anchors.margins: 22
                spacing: 12
                visible: root.detail !== null
                RowLayout {
                    Layout.fillWidth: true
                    Button {
                        text: "‹  Back"
                        flat: true
                        contentItem: Label { text: parent.text; color: root.theme.accentStrong; font.pixelSize: 13 }
                        background: Rectangle { color: "transparent" }
                        onClicked: root.detail = null
                    }
                    Item { Layout.fillWidth: true }
                    PrimaryButton {
                        text: "Stop syncing"
                        danger: true
                        onClicked: {
                            root.popupMode = "confirm-dir"
                            root.popupTitle = "Stop syncing this directory?"
                            root.popupBody = root.detail.localDisplay + " → " + root.detail.remoteDisplay
                            dialog.open()
                        }
                    }
                }
                Label {
                    text: root.detail ? (root.detail.localDisplay + "  →  " + root.detail.remoteDisplay) : ""
                    color: root.theme.textPrimary
                    font.pixelSize: 18
                    font.bold: true
                    Layout.fillWidth: true
                    elide: Text.ElideMiddle
                }
                Label {
                    text: root.detail ? root.detail.status : ""
                    color: root.theme.textMuted
                    font.pixelSize: 13
                }
                Label {
                    text: "Sync errors"
                    color: root.theme.textSecondary
                    font.pixelSize: 13
                    font.bold: true
                    visible: root.detail && root.detail.errors && root.detail.errors.length > 0
                }
                Repeater {
                    model: root.detail && root.detail.errors ? root.detail.errors : []
                    Rectangle {
                        required property var modelData
                        width: root.width - root.theme.sidebarWidth - 44
                        implicitHeight: errorCol.implicitHeight + 20
                        radius: root.theme.radiusSm
                        color: root.theme.cardBg
                        border.color: root.theme.borderSubtle
                        Column {
                            id: errorCol
                            anchors.left: parent.left
                            anchors.right: parent.right
                            anchors.top: parent.top
                            anchors.margins: 10
                            spacing: 2
                            Label { text: modelData.title; color: root.theme.textPrimary; font.pixelSize: 13; wrapMode: Text.WordWrap; width: errorCol.width }
                            Label { text: modelData.detail; color: root.theme.textMuted; font.pixelSize: 12; visible: modelData.detail && modelData.detail.length > 0; wrapMode: Text.WordWrap; width: errorCol.width }
                        }
                        MouseArea {
                            anchors.fill: parent
                            cursorShape: Qt.PointingHandCursor
                            onClicked: root.controller.activateError(root.selectedName, root.detail.localPath, root.detail.remotePath, modelData.id)
                        }
                    }
                }
                Label {
                    text: "Preserved versions"
                    color: root.theme.textSecondary
                    font.pixelSize: 13
                    font.bold: true
                    visible: root.detail && root.detail.recovery && root.detail.recovery.length > 0
                }
                Repeater {
                    model: root.detail && root.detail.recovery ? root.detail.recovery.slice(0, 8) : []
                    Rectangle {
                        required property var modelData
                        width: root.width - root.theme.sidebarWidth - 44
                        implicitHeight: recoveryRow.implicitHeight + 16
                        radius: root.theme.radiusSm
                        color: root.theme.cardBg
                        border.color: root.theme.borderSubtle
                        RowLayout {
                            id: recoveryRow
                            anchors.left: parent.left
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
                            anchors.margins: 8
                            spacing: 8
                            Column {
                                Layout.fillWidth: true
                                spacing: 2
                                Label { text: modelData.rel + " (" + modelData.side + ")"; color: root.theme.textPrimary; font.pixelSize: 13; elide: Text.ElideMiddle; width: parent.width }
                                Label { text: modelData.reason + " · " + modelData.when; color: root.theme.textMuted; font.pixelSize: 12; elide: Text.ElideRight; width: parent.width }
                            }
                            Button {
                                text: "Restore"
                                flat: true
                                contentItem: Label { text: parent.text; color: root.theme.accentStrong; font.pixelSize: 13 }
                                background: Rectangle { color: "transparent" }
                                onClicked: root.controller.restoreVersion(root.selectedName, root.detail.localPath, root.detail.remotePath, modelData.id)
                            }
                        }
                    }
                }
                Label { text: "File/folder exclusions"; color: root.theme.textSecondary; font.pixelSize: 13; font.bold: true }
                Label {
                    text: "One glob per line, stored in .sync-exclude.lst inside the local folder."
                    color: root.theme.textDim
                    font.pixelSize: 12
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                }
                ScrollView {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    TextArea {
                        id: exclusionArea
                        color: root.theme.textPrimary
                        wrapMode: TextEdit.NoWrap
                        font.family: "monospace"
                        font.pixelSize: 13
                        selectByMouse: true
                        background: Rectangle { color: root.theme.inputBg; radius: root.theme.radiusSm; border.color: root.theme.borderSubtle }
                    }
                }
                PrimaryButton {
                    text: "Save exclusions"
                    onClicked: root.controller.saveExclusions(root.detail.localPath, exclusionArea.text)
                }
            }
        }
    }

    Popup {
        id: dialog
        modal: true
        focus: true
        anchors.centerIn: parent
        width: Math.min(520, root.width - 80)
        padding: 0
        closePolicy: Popup.CloseOnEscape
        background: Rectangle {
            color: root.theme.cardBg
            radius: root.theme.radius
            border.color: root.theme.borderSubtle
            border.width: 1
        }
        contentItem: ColumnLayout {
            spacing: 12
            width: dialog.width
            anchors.margins: 18

            Label {
                text: root.popupMode === "login" ? "Log in" : (root.popupMode === "add" ? "Sync a folder" : root.popupTitle)
                color: root.theme.textPrimary
                font.pixelSize: 18
                font.bold: true
                Layout.fillWidth: true
                Layout.leftMargin: 18
                Layout.rightMargin: 18
                Layout.topMargin: 18
                wrapMode: Text.WordWrap
            }
            Label {
                text: root.popupBody
                color: root.theme.textSecondary
                font.pixelSize: 13
                wrapMode: Text.WordWrap
                visible: root.popupBody.length > 0 && root.popupMode !== "login" && root.popupMode !== "add"
                Layout.fillWidth: true
                Layout.leftMargin: 18
                Layout.rightMargin: 18
            }

            ColumnLayout {
                visible: root.popupMode === "login"
                Layout.fillWidth: true
                Layout.leftMargin: 18
                Layout.rightMargin: 18
                spacing: 10
                Label { text: "Server type"; color: root.theme.textSecondary; font.pixelSize: 12; font.bold: true }
                ComboBox {
                    id: kindBox
                    Layout.fillWidth: true
                    model: ["Google Drive", "Dropbox", "pCloud", "Nextcloud", "Owncloud", "Proton Drive", "WebDAV"]
                    currentIndex: 0
                    onActivated: {
                        var kinds = ["gdrive", "dropbox", "pcloud", "nextcloud", "owncloud", "protondrive", "webdav"]
                        root.loginKind = kinds[currentIndex]
                    }
                    delegate: ItemDelegate {
                        width: kindBox.width
                        text: modelData
                        contentItem: Label { text: modelData; color: root.theme.textPrimary; font.pixelSize: 13; leftPadding: 10; verticalAlignment: Text.AlignVCenter }
                        background: Rectangle { color: highlighted ? root.theme.cardBgRaised : root.theme.cardBg }
                    }
                    contentItem: Label { text: kindBox.displayText; color: root.theme.textPrimary; font.pixelSize: 13; leftPadding: 10; verticalAlignment: Text.AlignVCenter }
                    background: Rectangle { color: root.theme.inputBg; radius: root.theme.radiusSm; border.color: root.theme.borderSubtle }
                }
                Field { id: nameField; label: "Name"; Layout.fillWidth: true }
                Field { id: urlField; label: "Server URL"; Layout.fillWidth: true; visible: root.loginKind === "nextcloud" || root.loginKind === "owncloud" || root.loginKind === "webdav" }
                Field { id: userField; label: "Username"; Layout.fillWidth: true; visible: root.loginKind === "nextcloud" || root.loginKind === "owncloud" || root.loginKind === "webdav" || root.loginKind === "protondrive" }
                Field { id: passField; label: "Password"; password: true; Layout.fillWidth: true; visible: userField.visible }
                RowLayout {
                    visible: root.loginKind === "protondrive"
                    CheckBox {
                        id: totpCheck
                        checked: root.totpEnabled
                        onToggled: root.totpEnabled = checked
                        indicator: Rectangle {
                            implicitWidth: 18
                            implicitHeight: 18
                            radius: 4
                            border.color: root.theme.accent
                            color: totpCheck.checked ? root.theme.accent : "transparent"
                        }
                    }
                    Label { text: "This account uses a 2FA code"; color: root.theme.textSecondary; font.pixelSize: 13 }
                }
                Field { id: totpField; label: "2FA code"; Layout.fillWidth: true; visible: root.loginKind === "protondrive" && root.totpEnabled; placeholder: "6 digits" }
            }

            ColumnLayout {
                visible: root.popupMode === "add"
                Layout.fillWidth: true
                Layout.leftMargin: 18
                Layout.rightMargin: 18
                spacing: 10
                Field { id: localField; label: "Local folder"; Layout.fillWidth: true; text: root.localDraft }
                PrimaryButton { text: "Browse…"; onClicked: folderDialog.open() }
                Field {
                    id: remoteField
                    label: "Remote folder"
                    Layout.fillWidth: true
                    text: root.remoteDraft
                    onTextChanged: {
                        root.remoteDraft = remoteField.text
                        if (root.selectedName.length > 0)
                            root.controller.listDirs(root.selectedName, remoteField.text)
                    }
                }
                Repeater {
                    model: root.suggestions
                    Button {
                        required property string modelData
                        text: modelData
                        Layout.fillWidth: true
                        flat: true
                        contentItem: Label { text: modelData; color: root.theme.accentStrong; font.pixelSize: 13; leftPadding: 8 }
                        background: Rectangle { color: root.theme.inputBg; radius: root.theme.radiusSm }
                        onClicked: {
                            var current = remoteField.text
                            var slash = current.lastIndexOf("/")
                            var prefix = slash >= 0 ? current.substring(0, slash + 1) : ""
                            remoteField.text = prefix + modelData + "/"
                        }
                    }
                }
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.leftMargin: 18
                Layout.rightMargin: 18
                Layout.bottomMargin: 18
                Item { Layout.fillWidth: true }
                Button {
                    text: root.popupMode === "auth" ? "Cancel" : (root.popupMode === "fatal" ? "Quit" : "Close")
                    flat: true
                    contentItem: Label { text: parent.text; color: root.theme.textSecondary; font.pixelSize: 13 }
                    background: Rectangle { color: "transparent" }
                    onClicked: {
                        if (root.popupMode === "auth") root.controller.cancelAuth()
                        if (root.popupMode === "fatal") root.controller.quit()
                        if (root.popupMode === "login" && root.remotes().length === 0) root.controller.quit()
                        root.closeDialog()
                    }
                }
                PrimaryButton {
                    text: root.popupMode === "login" ? "Log in" : (root.popupMode === "add" ? "Sync" : (root.popupMode === "conflict-info" ? "Acknowledge" : "Confirm"))
                    visible: root.popupMode === "login" || root.popupMode === "add" || root.popupMode === "confirm-remote" || root.popupMode === "confirm-dir" || root.popupMode === "confirm-dismiss" || root.popupMode === "conflict-info"
                    onClicked: {
                        if (root.popupMode === "login") {
                            root.controller.submitLogin(JSON.stringify({
                                kind: root.loginKind,
                                name: nameField.text,
                                url: urlField.text,
                                username: userField.text,
                                password: passField.text,
                                totp: totpField.text,
                                totpEnabled: root.totpEnabled
                            }))
                        } else if (root.popupMode === "add") {
                            root.controller.addDir(root.selectedName, localField.text.length > 0 ? localField.text : root.localDraft, remoteField.text)
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-remote") {
                            root.controller.deleteRemote(root.selectedName)
                            root.selectedName = ""
                            root.detail = null
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-dir") {
                            root.controller.stopDir(root.selectedName, root.detail.localPath, root.detail.remotePath)
                            root.detail = null
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-dismiss") {
                            root.controller.dismissError(root.popupRef.remote, root.popupRef.local, root.popupRef.remotePath, root.popupRef.id)
                            root.closeDialog()
                        } else if (root.popupMode === "conflict-info") {
                            root.controller.dismissError(root.popupRef.remote, root.popupRef.local, root.popupRef.remotePath, root.popupRef.id)
                            root.closeDialog()
                        }
                    }
                }
                PrimaryButton {
                    text: "Delete everywhere"
                    visible: root.popupMode === "conflict-info" && root.popupRef && root.popupRef.canDelete === true
                    onClicked: {
                        root.controller.confirmDeletion(root.popupRef.remote, root.popupRef.local, root.popupRef.remotePath, root.popupRef.conflictId, root.popupRef.token)
                        root.closeDialog()
                    }
                }
            }
        }
    }
}
