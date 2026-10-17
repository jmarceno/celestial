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
    property var progress: ({ pairs: [], total: null })
    property var estimate: ({})
    // Bumped every 30 s so relative times ("5 min ago") stay current.
    property int clock: 0
    property string selectedName: ""
    property var detail: null
    // A folder just added: open its page once the snapshot lists it.
    property var pendingDetail: null
    property string exclusionDraft: ""
    // Remote folder browser state. `browsePath` is "" for the remote root.
    property string browsePath: ""
    property var browseDirs: []
    property bool browseLoading: false
    property string browseError: ""
    property string popupMode: ""
    property string popupTitle: ""
    property string popupBody: ""
    property var popupRef: ({})
    property string loginKind: "gdrive"
    property bool totpEnabled: false
    // Sync issues split by severity: real errors stay actionable,
    // limitation warnings (symlinks, special files, non-UTF-8 names,
    // leftover partials) are dismissable and stay dismissed.
    readonly property var errorEntries: (root.detail && root.detail.errors ? root.detail.errors : [])
        .filter(function (e) { return e.warning !== true })
    readonly property var warningEntries: (root.detail && root.detail.errors ? root.detail.errors : [])
        .filter(function (e) { return e.warning === true })

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
    function openDetail(entry) {
        root.detail = entry
        exclusionArea.text = (entry.exclusions || []).join("\n")
        detailFlick.contentY = 0
        if (entry && root.selectedName.length > 0) {
            root.controller.requestStats(root.selectedName, entry.localPath, entry.remotePath)
        }
    }
    function openPendingDetail() {
        if (!root.pendingDetail) return
        var remote = root.selectedRemote()
        var dirs = remote ? (remote.dirs || []) : []
        for (var i = 0; i < dirs.length; i++) {
            if (dirs[i].localPath === root.pendingDetail.local && dirs[i].remotePath === root.pendingDetail.remotePath) {
                root.pendingDetail = null
                root.openDetail(dirs[i])
                return
            }
        }
    }
    function resumeDir(entry) {
        root.controller.resumeDir(root.selectedName, entry.localPath, entry.remotePath)
    }
    function phaseColor(phase) {
        if (phase === "done") return root.theme.statusGreen
        if (phase === "syncing") return root.theme.accent
        if (phase === "warning") return root.theme.warning
        return root.theme.textDim
    }
    function progressFor(remote, dir) {
        if (!dir) return null
        var pairs = root.progress.pairs || []
        for (var i = 0; i < pairs.length; i++) {
            var pair = pairs[i]
            if (pair.remote === remote && pair.localPath === dir.localPath && pair.remotePath === dir.remotePath)
                return pair
        }
        return null
    }
    function ago(at, clock) {
        var seconds = Math.max(0, Math.floor(Date.now() / 1000) - at)
        if (seconds < 60) return "just now"
        if (seconds < 3600) return Math.floor(seconds / 60) + " min ago"
        if (seconds < 86400) return Math.floor(seconds / 3600) + " h ago"
        return Math.floor(seconds / 86400) + " d ago"
    }
    function actionGlyph(action) {
        if (action === "uploaded") return "↑"
        if (action === "downloaded") return "↓"
        if (action === "conflict") return "⇄"
        return "✕"
    }
    function openMessage(title, body) {
        root.popupMode = "message"
        root.popupTitle = title
        root.popupBody = body
        dialog.open()
    }
    // Close, ×, and Escape: back out of the dialog without confirming it.
    function dismissDialog() {
        if (root.popupMode === "auth") root.controller.cancelAuth()
        if (root.popupMode === "fatal") root.controller.quit()
        if (root.popupMode === "login" && root.remotes().length === 0) root.controller.quit()
        root.closeDialog()
    }
    function closeDialog() {
        if (root.popupMode === "add") {
            root.estimateTimer.stop()
            root.controller.cancelEstimate()
        }
        dialog.close()
        root.popupMode = ""
    }

    property Shortcut escapeShortcut: Shortcut {
        sequence: "Escape"
        enabled: dialog.opened && !remoteBrowser.opened
        onActivated: root.dismissDialog()
    }

    property Timer clockTimer: Timer {
        interval: 30000
        running: root.visible
        repeat: true
        onTriggered: root.clock++
    }

    // Measure the folder pair shortly after the person stops typing.
    property Timer estimateTimer: Timer {
        interval: 700
        onTriggered: {
            if (root.popupMode === "add" && localField.text.length > 0 && root.selectedName.length > 0)
                root.controller.estimateDir(root.selectedName, localField.text, remoteField.text)
        }
    }
    function estimateCurrent() {
        return root.popupMode === "add" && root.estimate.local === localField.text
            && root.estimate.remotePath === remoteField.text && root.estimate.remote === root.selectedName
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
            root.openPendingDetail()
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
        function onEstimate_jsonChanged() {
            root.estimate = root.parse(root.controller.estimate_json, {})
        }
        function onProgress_jsonChanged() {
            root.progress = root.parse(root.controller.progress_json, { pairs: [], total: null })
        }
        function onFolders_jsonChanged() {
            var reply = root.parse(root.controller.folders_json, {})
            // Ignore replies for a folder the browser has already left.
            if (reply.remote !== root.selectedName || reply.path !== root.browsePath) return
            root.browseDirs = reply.dirs || []
            root.browseError = reply.error || ""
            root.browseLoading = false
        }
        function onNotice(kind, title, body) {
            if (kind === "local-folder") {
                localField.text = body
                return
            }
            if (kind === "local-folder-fallback") {
                folderDialog.open()
                return
            }
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
            if (kind === "dir-added") {
                root.pendingDetail = root.parse(body, null)
                root.openPendingDetail()
                return
            }
            if (kind === "auth-done") {
                if (root.popupMode === "auth") root.closeDialog()
                return
            }
            if (kind === "conflict-info" || kind === "confirm-dismiss" || kind === "confirm-space") {
                root.popupMode = kind
                root.popupTitle = title
                root.popupRef = root.parse(body, {})
                root.popupBody = kind === "confirm-dismiss" ? "" : (root.popupRef.text || "")
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
            localField.text = decodeURIComponent(path)
        }
    }

    function cleanRemotePath(path) {
        return path.split("/").filter(function(part) { return part.length > 0 }).join("/")
    }
    function browseTo(path) {
        root.browsePath = root.cleanRemotePath(path)
        root.browseDirs = []
        root.browseError = ""
        root.browseLoading = true
        browserList.currentIndex = -1
        root.controller.listDirs(root.selectedName, root.browsePath)
    }
    function browseUp() {
        var slash = root.browsePath.lastIndexOf("/")
        root.browseTo(slash >= 0 ? root.browsePath.substring(0, slash) : "")
    }
    function browseChild(name) {
        return root.browsePath.length > 0 ? root.browsePath + "/" + name : name
    }
    function browseSelection() {
        if (browserList.currentIndex >= 0 && browserList.currentIndex < root.browseDirs.length)
            return root.browseChild(root.browseDirs[browserList.currentIndex])
        return root.browsePath
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
                    maximumLineCount: 3
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
                Label {
                    text: root.progress.total ? root.progress.total.text : ""
                    visible: root.progress.total !== null && root.progress.total !== undefined
                    color: root.theme.accentStrong
                    font.pixelSize: 11
                    wrapMode: Text.WordWrap
                    maximumLineCount: 3
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
                Item { Layout.preferredHeight: 8 }
                ListView {
                    id: serverList
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true
                    spacing: 4
                    boundsBehavior: Flickable.StopAtBounds
                    model: root.remotes().length
                    ScrollBar.vertical: VScrollBar { theme: root.theme }
                    delegate: SidebarItem {
                        required property int index
                        // Counted, not a list: an update keeps the items instead of recreating them
                        readonly property var entry: root.remotes()[index] || ({})
                        theme: root.theme
                        text: entry.name
                        selected: entry.name === root.selectedName
                        width: serverList.width - (serverList.ScrollBar.vertical.visible ? 10 : 0)
                        onClicked: {
                            root.selectedName = entry.name
                            root.detail = null
                        }
                    }
                }
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
                        elide: Text.ElideRight
                    }
                    PrimaryButton {
                        text: "Add folder"
                        visible: root.selectedRemote() !== null
                        onClicked: {
                            localField.text = ""
                            remoteField.text = "/"
                            root.popupMode = "add"
                            dialog.open()
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
                    id: dirFlick
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    contentHeight: dirCol.implicitHeight
                    clip: true
                    boundsBehavior: Flickable.StopAtBounds
                    ScrollBar.vertical: VScrollBar { theme: root.theme }
                    Column {
                        id: dirCol
                        width: dirFlick.width - (dirFlick.ScrollBar.vertical.visible ? 14 : 0)
                        spacing: 8
                        Repeater {
                            model: (root.selectedRemote() ? root.selectedRemote().dirs : []).length
                            Rectangle {
                                id: dirCard
                                required property int index
                                readonly property var entry: (root.selectedRemote() ? root.selectedRemote().dirs : [])[index] || ({})
                                property var live: root.progressFor(root.selectedName, entry)
                                width: dirCol.width
                                height: live ? 112 : 72
                                radius: root.theme.radius
                                color: root.theme.cardBg
                                border.color: root.theme.borderSubtle
                                border.width: 1
                                RowLayout {
                                    anchors.fill: parent
                                    anchors.margins: 14
                                    spacing: 12
                                    // Above the card's MouseArea so the start button gets its clicks.
                                    z: 1
                                    Rectangle {
                                        width: 10
                                        height: 10
                                        radius: 5
                                        color: root.phaseColor(entry.phase)
                                    }
                                    ColumnLayout {
                                        Layout.fillWidth: true
                                        spacing: 2
                                        Label {
                                            text: entry.localDisplay + "   →   " + entry.remoteDisplay
                                            color: root.theme.textPrimary
                                            font.pixelSize: 13
                                            elide: Text.ElideMiddle
                                            Layout.fillWidth: true
                                        }
                                        Label {
                                            text: (entry.errorStatus ? entry.errorStatus + " " : "") + (entry.status || "")
                                            color: entry.errorStatus ? root.theme.danger : root.theme.textMuted
                                            font.pixelSize: 12
                                            elide: Text.ElideRight
                                            Layout.fillWidth: true
                                        }
                                        TransferProgress {
                                            visible: dirCard.live !== null
                                            live: dirCard.live
                                            theme: root.theme
                                            Layout.fillWidth: true
                                            Layout.topMargin: 4
                                        }
                                    }
                                    PrimaryButton {
                                        text: "▶  Start"
                                        visible: dirCard.entry.paused === true
                                        onClicked: root.resumeDir(dirCard.entry)
                                    }
                                    Label { text: "›"; color: root.theme.textDim; font.pixelSize: 18 }
                                }
                                MouseArea {
                                    anchors.fill: parent
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: root.openDetail(dirCard.entry)
                                }
                            }
                        }
                    }
                }
            }

            Flickable {
                id: detailFlick
                anchors.fill: parent
                visible: root.detail !== null
                clip: true
                contentWidth: width
                contentHeight: detailCol.implicitHeight + 22
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: VScrollBar { theme: root.theme }

                function ensureVisible(item, rect) {
                    var top = item.mapToItem(detailFlick.contentItem, rect.x, rect.y).y
                    var bottom = top + rect.height
                    if (top < contentY) contentY = Math.max(0, top - 12)
                    else if (bottom > contentY + height) contentY = Math.min(contentHeight - height, bottom - height + 12)
                }

                ColumnLayout {
                    id: detailCol
                    x: 22
                    y: 22
                    width: detailFlick.width - 44 - (detailFlick.ScrollBar.vertical.visible ? 10 : 0)
                    spacing: 6
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
                            text: "▶  Start syncing"
                            visible: root.detail !== null && root.detail.paused === true
                            onClicked: {
                                // Unsaved exclusion edits apply before the first pass.
                                if (exclusionArea.text !== (root.detail.exclusions || []).join("\n"))
                                    root.controller.saveExclusions(root.detail.localPath, exclusionArea.text)
                                root.resumeDir(root.detail)
                            }
                        }
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
                        wrapMode: Text.WrapAnywhere
                    }
                    Label {
                        text: root.detail ? root.detail.status : ""
                        color: root.theme.textMuted
                        font.pixelSize: 13
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    TransferProgress {
                        property var current: root.progressFor(root.selectedName, root.detail)
                        visible: current !== null
                        live: current
                        theme: root.theme
                        Layout.fillWidth: true
                    }
                    // Folder stats
                    Label {
                        text: "Folder stats"
                        color: root.theme.textSecondary
                        font.pixelSize: 12
                        font.bold: true
                        visible: root.detail && root.estimate.local === root.detail.localPath && root.estimate.remotePath === root.detail.remotePath && root.estimate.done
                    }
                    Label {
                        text: root.estimate.text ? root.estimate.text : ""
                        color: root.theme.textMuted
                        font.pixelSize: 13
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                        visible: root.detail && root.estimate.local === root.detail.localPath && root.estimate.remotePath === root.detail.remotePath && root.estimate.done
                    }
                    Label {
                        text: "Recent changes"
                        color: root.theme.textSecondary
                        font.pixelSize: 12
                        font.bold: true
                        visible: root.detail && root.detail.recent && root.detail.recent.length > 0
                    }
                    ListView {
                        Layout.fillWidth: true
                        implicitHeight: contentHeight
                        interactive: false
                        spacing: 2
                        clip: true
                        model: (root.detail && root.detail.recent ? root.detail.recent.slice(0, 5) : []).length
                        delegate: RecentItem {
                            required property int index
                            readonly property var entry: (root.detail && root.detail.recent ? root.detail.recent.slice(0, 5) : [])[index] || ({})
                            item: entry
                            theme: root.theme
                            when: root.ago(entry.at, root.clock)
                            glyph: root.actionGlyph(entry.action)
                            width: ListView.view.width
                        }
                    }
                    Label {
                        text: "Sync errors"
                        color: root.theme.textSecondary
                        font.pixelSize: 12
                        font.bold: true
                        visible: root.errorEntries.length > 0
                    }
                    ListView {
                        Layout.fillWidth: true
                        implicitHeight: contentHeight
                        interactive: false
                        spacing: 2
                        clip: true
                        model: root.errorEntries.length
                        delegate: Rectangle {
                            required property int index
                            readonly property var entry: root.errorEntries[index] || ({})
                            width: ListView.view.width
                            implicitHeight: errorCol.implicitHeight + 8
                            radius: root.theme.radiusSm
                            color: root.theme.cardBg
                            border.color: root.theme.borderSubtle
                            Column {
                                id: errorCol
                                anchors.left: parent.left
                                anchors.right: parent.right
                                anchors.top: parent.top
                                anchors.margins: 6
                                spacing: 1
                                Label { text: entry.title; color: root.theme.textPrimary; font.pixelSize: 12; wrapMode: Text.WordWrap; width: errorCol.width }
                                Label { text: entry.detail; color: root.theme.textMuted; font.pixelSize: 11; visible: entry.detail && entry.detail.length > 0; wrapMode: Text.WordWrap; width: errorCol.width }
                            }
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: Qt.PointingHandCursor
                                onClicked: root.controller.activateError(root.selectedName, root.detail.localPath, root.detail.remotePath, entry.id)
                            }
                        }
                    }
                    Label {
                        text: "Warnings"
                        color: root.theme.textSecondary
                        font.pixelSize: 12
                        font.bold: true
                        visible: root.warningEntries.length > 0
                    }
                    ListView {
                        Layout.fillWidth: true
                        implicitHeight: contentHeight
                        interactive: false
                        spacing: 2
                        clip: true
                        model: root.warningEntries.length
                        delegate: Rectangle {
                            required property int index
                            readonly property var entry: root.warningEntries[index] || ({})
                            width: ListView.view.width
                            implicitHeight: warningRow.implicitHeight + 8
                            radius: root.theme.radiusSm
                            color: root.theme.warningBg
                            border.color: root.theme.warning
                            RowLayout {
                                id: warningRow
                                anchors.left: parent.left
                                anchors.right: parent.right
                                anchors.top: parent.top
                                anchors.margins: 6
                                spacing: 4
                                Column {
                                    Layout.fillWidth: true
                                    spacing: 1
                                    Label { text: entry.title; color: root.theme.textPrimary; font.pixelSize: 12; wrapMode: Text.WordWrap; width: parent.width }
                                    Label { text: entry.detail; color: root.theme.textMuted; font.pixelSize: 11; visible: entry.detail && entry.detail.length > 0; wrapMode: Text.WordWrap; width: parent.width }
                                }
                                Button {
                                    text: "Dismiss"
                                    flat: true
                                    contentItem: Label { text: parent.text; color: root.theme.warning; font.pixelSize: 12 }
                                    background: Rectangle { color: "transparent" }
                                    onClicked: root.controller.dismissError(root.selectedName, root.detail.localPath, root.detail.remotePath, entry.id)
                                }
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
                        model: (root.detail && root.detail.recovery ? root.detail.recovery.slice(0, 8) : []).length
                        Rectangle {
                            required property int index
                            readonly property var entry: (root.detail && root.detail.recovery ? root.detail.recovery.slice(0, 8) : [])[index] || ({})
                            Layout.fillWidth: true
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
                                    Label { text: entry.rel + " (" + entry.side + ")"; color: root.theme.textPrimary; font.pixelSize: 13; elide: Text.ElideMiddle; width: parent.width }
                                    Label { text: entry.reason + " · " + entry.when; color: root.theme.textMuted; font.pixelSize: 12; elide: Text.ElideRight; width: parent.width }
                                }
                                Button {
                                    text: "Restore"
                                    flat: true
                                    contentItem: Label { text: parent.text; color: root.theme.accentStrong; font.pixelSize: 13 }
                                    background: Rectangle { color: "transparent" }
                                    onClicked: root.controller.restoreVersion(root.selectedName, root.detail.localPath, root.detail.remotePath, entry.id)
                                }
                            }
                        }
                    }
                    Label { text: "File/folder exclusions"; color: root.theme.textSecondary; font.pixelSize: 13; font.bold: true }
                    Label {
                        text: "Edit or remove any starting rule and save. One rule per line: glob paths (~/ is home, / is absolute), regex: followed by a regular expression, or builtin:rust-target for Rust build folders. Regex paths are relative to this folder."
                        color: root.theme.textDim
                        font.pixelSize: 12
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    // Grows with its rules; the page scrolls, so every line stays reachable.
                    TextArea {
                        id: exclusionArea
                        Layout.fillWidth: true
                        Layout.preferredHeight: Math.max(160, implicitHeight)
                        color: root.theme.textPrimary
                        wrapMode: TextEdit.WrapAnywhere
                        font.family: "monospace"
                        font.pixelSize: 13
                        selectByMouse: true
                        padding: 10
                        background: Rectangle { color: root.theme.inputBg; radius: root.theme.radiusSm; border.color: exclusionArea.activeFocus ? root.theme.accent : root.theme.borderSubtle }
                        // Keep the caret visible while typing near the bottom of the page.
                        onCursorRectangleChanged: if (activeFocus) detailFlick.ensureVisible(exclusionArea, cursorRectangle)
                    }
                    PrimaryButton {
                        text: "Save exclusions"
                        Layout.bottomMargin: 22
                        onClicked: root.controller.saveExclusions(root.detail.localPath, exclusionArea.text)
                    }
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
        height: Math.min(implicitHeight, root.height - 40)
        // Escape goes through dismissDialog() so it does what Close does.
        closePolicy: Popup.NoAutoClose
        background: Rectangle {
            color: root.theme.cardBg
            radius: root.theme.radius
            border.color: root.theme.borderSubtle
            border.width: 1
        }
        contentItem: ColumnLayout {
            spacing: 12

            RowLayout {
                Layout.fillWidth: true
                Layout.leftMargin: 18
                Layout.rightMargin: 12
                Layout.topMargin: 14
                spacing: 8
                Label {
                    text: root.popupMode === "login" ? "Log in" : (root.popupMode === "add" ? "Sync a folder" : root.popupTitle)
                    color: root.theme.textPrimary
                    font.pixelSize: 18
                    font.bold: true
                    Layout.fillWidth: true
                    Layout.topMargin: 4
                    wrapMode: Text.WordWrap
                }
                CloseButton {
                    theme: root.theme
                    Layout.alignment: Qt.AlignTop
                    onClicked: root.dismissDialog()
                }
            }

            // Long forms and messages scroll; the title and buttons stay put.
            Flickable {
                id: dialogFlick
                Layout.fillWidth: true
                Layout.fillHeight: true
                Layout.preferredHeight: dialogBody.implicitHeight
                contentHeight: dialogBody.implicitHeight
                clip: true
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: VScrollBar { theme: root.theme }

                ColumnLayout {
                    id: dialogBody
                    x: 18
                    width: dialogFlick.width - 36
                    spacing: 12

                    Label {
                        text: root.popupBody
                        color: root.theme.textSecondary
                        font.pixelSize: 13
                        wrapMode: Text.WordWrap
                        visible: root.popupBody.length > 0 && root.popupMode !== "login" && root.popupMode !== "add"
                        Layout.fillWidth: true
                    }

                    ColumnLayout {
                        visible: root.popupMode === "login"
                        Layout.fillWidth: true
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
                        spacing: 10
                        RowLayout {
                            Layout.fillWidth: true
                            spacing: 8
                            Field {
                                id: localField
                                label: "Local folder"
                                placeholder: "/home/you/Documents"
                                Layout.fillWidth: true
                                onTextChanged: root.estimateTimer.restart()
                            }
                            PrimaryButton {
                                text: "Browse…"
                                Layout.alignment: Qt.AlignBottom
                                onClicked: root.controller.pickLocalFolder(localField.text)
                            }
                        }
                        RowLayout {
                            Layout.fillWidth: true
                            spacing: 8
                            Field {
                                id: remoteField
                                label: "Remote folder"
                                Layout.fillWidth: true
                                onTextChanged: root.estimateTimer.restart()
                            }
                            PrimaryButton {
                                text: "Browse…"
                                Layout.alignment: Qt.AlignBottom
                                enabled: root.selectedName.length > 0
                                onClicked: {
                                    remoteBrowser.open()
                                    root.browseTo(remoteField.text)
                                }
                            }
                        }
                        Label {
                            text: root.estimateCurrent() ? (root.estimate.text || "") : ""
                            visible: text.length > 0
                            color: root.theme.textSecondary
                            font.pixelSize: 12
                            wrapMode: Text.WordWrap
                            Layout.fillWidth: true
                        }
                        Label {
                            text: root.estimateCurrent() && root.estimate.warning ? root.estimate.warning : ""
                            visible: text.length > 0
                            color: root.theme.warning
                            font.pixelSize: 12
                            wrapMode: Text.WordWrap
                            Layout.fillWidth: true
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
                    onClicked: root.dismissDialog()
                }
                PrimaryButton {
                    text: root.popupMode === "login" ? "Log in" : (root.popupMode === "add" ? "Add" : (root.popupMode === "conflict-info" ? "Acknowledge" : (root.popupMode === "confirm-space" ? "Sync anyway" : "Confirm")))
                    visible: root.popupMode === "login" || root.popupMode === "add" || root.popupMode === "confirm-remote" || root.popupMode === "confirm-dir" || root.popupMode === "confirm-dismiss" || root.popupMode === "conflict-info" || root.popupMode === "confirm-space"
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
                            root.controller.addDir(root.selectedName, localField.text, remoteField.text)
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
                        } else if (root.popupMode === "confirm-dismiss" || root.popupMode === "confirm-space") {
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

    // Remote folder browser: one folder level at a time, like a file dialog.
    Popup {
        id: remoteBrowser
        modal: true
        focus: true
        anchors.centerIn: parent
        width: Math.min(560, root.width - 60)
        height: Math.min(520, root.height - 60)
        padding: 16
        closePolicy: Popup.CloseOnEscape
        onOpened: browserList.forceActiveFocus()
        background: Rectangle {
            color: root.theme.cardBg
            radius: root.theme.radiusSm
            border.color: root.theme.borderSubtle
            border.width: 1
        }

        function choose() {
            remoteField.text = "/" + root.browseSelection()
            remoteBrowser.close()
        }

        contentItem: ColumnLayout {
            spacing: 10

            RowLayout {
                Layout.fillWidth: true
                Label {
                    text: "Choose a remote folder"
                    color: root.theme.textPrimary
                    font.pixelSize: 15
                    font.bold: true
                    Layout.fillWidth: true
                }
                CloseButton {
                    theme: root.theme
                    onClicked: remoteBrowser.close()
                }
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Button {
                    id: upButton
                    text: "Up"
                    enabled: root.browsePath.length > 0
                    implicitHeight: 32
                    onClicked: root.browseUp()
                    contentItem: Label {
                        text: upButton.text
                        color: upButton.enabled ? root.theme.textPrimary : root.theme.textDim
                        font.pixelSize: 13
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    background: Rectangle {
                        radius: 4
                        color: upButton.down ? root.theme.borderSubtle : (upButton.hovered && upButton.enabled ? root.theme.cardBgRaised : root.theme.inputBg)
                        border.color: root.theme.borderSubtle
                    }
                }
                TextField {
                    id: browserPath
                    Layout.fillWidth: true
                    implicitHeight: 32
                    text: root.selectedName + ":/" + root.browsePath
                    readOnly: true
                    selectByMouse: true
                    color: root.theme.textSecondary
                    font.pixelSize: 13
                    leftPadding: 8
                    background: Rectangle {
                        radius: 4
                        color: root.theme.inputBg
                        border.color: root.theme.borderSubtle
                    }
                }
            }

            Rectangle {
                Layout.fillWidth: true
                Layout.fillHeight: true
                color: root.theme.inputBg
                border.color: root.theme.borderSubtle
                radius: 4

                ListView {
                    id: browserList
                    anchors.fill: parent
                    anchors.margins: 1
                    clip: true
                    focus: true
                    model: root.browseDirs
                    currentIndex: -1
                    boundsBehavior: Flickable.StopAtBounds
                    keyNavigationEnabled: true
                    ScrollBar.vertical: VScrollBar { theme: root.theme }
                    Keys.onReturnPressed: {
                        if (currentIndex >= 0) root.browseTo(root.browseChild(root.browseDirs[currentIndex]))
                    }
                    Keys.onEnterPressed: {
                        if (currentIndex >= 0) root.browseTo(root.browseChild(root.browseDirs[currentIndex]))
                    }
                    Keys.onPressed: function(event) {
                        if (event.key === Qt.Key_Backspace && root.browsePath.length > 0) {
                            root.browseUp()
                            event.accepted = true
                        }
                    }
                    delegate: Rectangle {
                        id: row
                        required property string modelData
                        required property int index
                        width: browserList.width - (browserList.ScrollBar.vertical.visible ? browserList.ScrollBar.vertical.width : 0)
                        height: 30
                        color: browserList.currentIndex === row.index ? root.theme.accentMuted : (rowArea.containsMouse ? root.theme.cardBgRaised : "transparent")
                        Label {
                            anchors.fill: parent
                            anchors.leftMargin: 10
                            anchors.rightMargin: 10
                            text: row.modelData
                            color: root.theme.textPrimary
                            font.pixelSize: 13
                            elide: Text.ElideRight
                            verticalAlignment: Text.AlignVCenter
                        }
                        MouseArea {
                            id: rowArea
                            anchors.fill: parent
                            hoverEnabled: true
                            onClicked: {
                                browserList.currentIndex = row.index
                                browserList.forceActiveFocus()
                            }
                            onDoubleClicked: root.browseTo(root.browseChild(row.modelData))
                        }
                    }
                }

                Label {
                    anchors.centerIn: parent
                    width: parent.width - 32
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.WordWrap
                    visible: root.browseLoading || root.browseError.length > 0 || root.browseDirs.length === 0
                    text: root.browseLoading ? "Loading…" : (root.browseError.length > 0 ? "Could not list this folder: " + root.browseError : "This folder has no subfolders.")
                    color: root.browseError.length > 0 && !root.browseLoading ? root.theme.danger : root.theme.textMuted
                    font.pixelSize: 13
                }
            }

            Label {
                Layout.fillWidth: true
                text: "Double-click a folder to open it. Selected: /" + root.browseSelection()
                color: root.theme.textMuted
                font.pixelSize: 12
                elide: Text.ElideMiddle
            }

            RowLayout {
                Layout.fillWidth: true
                spacing: 8
                Item { Layout.fillWidth: true }
                Button {
                    id: browserCancel
                    text: "Cancel"
                    implicitHeight: 34
                    onClicked: remoteBrowser.close()
                    contentItem: Label {
                        text: browserCancel.text
                        color: root.theme.textPrimary
                        font.pixelSize: 13
                        horizontalAlignment: Text.AlignHCenter
                        verticalAlignment: Text.AlignVCenter
                    }
                    background: Rectangle {
                        radius: 4
                        color: browserCancel.down ? root.theme.borderSubtle : (browserCancel.hovered ? root.theme.cardBgRaised : root.theme.inputBg)
                        border.color: root.theme.borderSubtle
                    }
                }
                PrimaryButton {
                    text: "Select"
                    implicitHeight: 34
                    enabled: !root.browseLoading && root.browseError.length === 0
                    onClicked: remoteBrowser.choose()
                }
            }
        }
    }
}
