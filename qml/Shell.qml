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
    flags: Qt.Window | Qt.FramelessWindowHint

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
    // Opening a folder seeds its settings; seeding must not save them back.
    property bool seedingDetail: false
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

    // Clicks the sync worker has not applied yet, by key. The worker may
    // be in the middle of a pass, so a click shows here at once and the
    // snapshot or a notice clears it. Each entry is { settled, byNotice,
    // label }: `settled()` says the new snapshot shows the result,
    // `byNotice` clears it on the next dialog notice. An error clears all.
    property var pending: ({})
    readonly property bool detailStarting: !!root.detail
        && root.isPending(root.pairKey("resume", root.selectedName, root.detail.localPath, root.detail.remotePath))

    function setPending(key, entry) {
        var next = Object.assign({}, root.pending)
        if (entry) next[key] = entry
        else delete next[key]
        root.pending = next
    }
    function isPending(key) {
        return root.pending[key] !== undefined
    }
    function pairKey(kind, remote, local, remotePath) {
        return kind + "|" + remote + "|" + local + "|" + remotePath
    }
    function settlePending(byNotice, everything) {
        var next = {}
        var changed = false
        for (var key in root.pending) {
            var entry = root.pending[key]
            var done = everything || (byNotice && entry.byNotice)
                || (!!entry.settled && entry.settled())
            if (done) changed = true
            else next[key] = entry
        }
        if (changed) root.pending = next
    }
    function findRemote(name) {
        var list = root.remotes()
        for (var i = 0; i < list.length; i++) {
            if (list[i].name === name) return list[i]
        }
        return null
    }
    function findDir(remote, local, remotePath) {
        var entry = root.findRemote(remote)
        var dirs = entry ? (entry.dirs || []) : []
        for (var i = 0; i < dirs.length; i++) {
            if (dirs[i].localPath === local && dirs[i].remotePath === remotePath) return dirs[i]
        }
        return null
    }
    function hasId(list, id) {
        return (list || []).some(function (item) { return item.id === id })
    }

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
        // A server being removed is not shown in its own place.
        for (var j = 0; j < list.length; j++) {
            if (!root.isPending("remove|" + list[j].name)) return list[j]
        }
        return null
    }
    function openDetail(entry) {
        root.detail = entry
        // Seeding the settings must not save them back.
        root.seedingDetail = true
        exclusionArea.text = (entry.exclusions || []).join("\n")
        recoveryMaxField.text = String(Math.round((entry.recoveryMaxBytes !== undefined ? entry.recoveryMaxBytes : 100000000) / 1000000))
        root.setIntervalFields(entry.syncIntervalSecs || 0)
        root.seedingDetail = false
        detailFlick.contentY = 0
        if (entry && root.selectedName.length > 0) {
            root.controller.requestStats(root.selectedName, entry.localPath, entry.remotePath)
        }
    }
    // The single number the interval is stored as, shown with its unit.
    function setIntervalFields(secs) {
        intervalCheck.checked = secs > 0
        if (secs <= 0) {
            intervalField.text = "3"
            intervalUnit.currentIndex = 1
        } else if (secs % 86400 === 0) {
            intervalField.text = String(secs / 86400)
            intervalUnit.currentIndex = 2
        } else if (secs % 3600 === 0) {
            intervalField.text = String(secs / 3600)
            intervalUnit.currentIndex = 1
        } else {
            intervalField.text = String(Math.max(1, Math.round(secs / 60)))
            intervalUnit.currentIndex = 0
        }
    }
    function intervalSeconds() {
        var count = parseInt(intervalField.text)
        if (isNaN(count) || count < 1) return 0
        var unit = intervalUnit.currentIndex === 0 ? 60 : (intervalUnit.currentIndex === 1 ? 3600 : 86400)
        return count * unit
    }
    function applyInterval() {
        if (!root.detail) return
        var secs = intervalCheck.checked ? root.intervalSeconds() : 0
        root.controller.setSyncInterval(root.selectedName, root.detail.localPath, root.detail.remotePath, secs)
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
        var remote = root.selectedName
        var local = entry.localPath
        var remotePath = entry.remotePath
        root.setPending(root.pairKey("resume", remote, local, remotePath), {
            settled: function () {
                var dir = root.findDir(remote, local, remotePath)
                return !dir || dir.paused !== true
            }
        })
        root.controller.resumeDir(remote, local, remotePath)
    }
    // A folder on a schedule synchronizes now; the request is taken once the
    // worker shows the pair waiting for or running its pass.
    function syncNow(entry) {
        var remote = root.selectedName
        var local = entry.localPath
        var remotePath = entry.remotePath
        var before = entry.status
        root.setPending(root.pairKey("syncnow", remote, local, remotePath), {
            settled: function () {
                var dir = root.findDir(remote, local, remotePath)
                return !dir || dir.paused === true || dir.phase === "syncing" || dir.status !== before
            }
        })
        root.controller.syncNow(remote, local, remotePath)
    }
    function syncNowBusy(entry) {
        return !!entry && (entry.phase === "syncing"
            || root.isPending(root.pairKey("syncnow", root.selectedName, entry.localPath, entry.remotePath)))
    }
    function stopDir(remote, local, remotePath) {
        root.setPending(root.pairKey("stop", remote, local, remotePath), {
            settled: function () { return !root.findDir(remote, local, remotePath) }
        })
        root.controller.stopDir(remote, local, remotePath)
    }
    function deleteRemote(name) {
        root.setPending("remove|" + name, {
            settled: function () { return !root.findRemote(name) }
        })
        root.controller.deleteRemote(name)
    }
    // Dismissing, retrying, or acknowledging one sync issue.
    function dismissError(remote, local, remotePath, id) {
        root.setPending(root.pairKey("error", remote, local, remotePath) + "|" + id, {
            settled: function () {
                var dir = root.findDir(remote, local, remotePath)
                return !dir || !root.hasId(dir.errors, id)
            }
        })
        root.controller.dismissError(remote, local, remotePath, id)
    }
    // Opening a sync issue asks the worker for its dialog.
    function activateError(remote, local, remotePath, id) {
        var key = root.pairKey("error", remote, local, remotePath) + "|" + id
        if (root.isPending(key)) return
        root.setPending(key, {
            byNotice: true,
            settled: function () {
                var dir = root.findDir(remote, local, remotePath)
                return !dir || !root.hasId(dir.errors, id)
            }
        })
        root.controller.activateError(remote, local, remotePath, id)
    }
    function saveExclusions(local, text) {
        root.exclusionsSavedTimer.stop()
        root.setPending("exclusions|" + local, { byNotice: true })
        root.controller.saveExclusions(local, text)
    }
    function errorPending(id) {
        return !!root.detail && root.isPending(root.pairKey("error", root.selectedName, root.detail.localPath, root.detail.remotePath) + "|" + id)
    }
    function allVersionsPending() {
        return !!root.detail && root.isPending(root.pairKey("versions", root.selectedName, root.detail.localPath, root.detail.remotePath))
    }
    function versionPending(id) {
        return root.allVersionsPending() || (!!root.detail
            && root.isPending(root.pairKey("version", root.selectedName, root.detail.localPath, root.detail.remotePath) + "|" + id))
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
    // Shows "Saved" beside the exclusions button for a moment.
    property Timer exclusionsSavedTimer: Timer {
        interval: 2500
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
            root.settlePending(false, false)
            if (root.selectedName.length === 0 && root.selectedRemote())
                root.selectedName = root.selectedRemote().name
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
            if (kind.indexOf("local-folder") === 0) {
                root.setPending("pick", null)
                if (kind === "local-folder") localField.text = body
                if (kind === "local-folder-fallback") folderDialog.open()
                return
            }
            root.settlePending(true, kind === "error" || kind === "fatal")
            if (kind === "exclusions-saved") {
                if (root.detail && root.detail.localPath === body) exclusionsSavedTimer.restart()
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

    TitleBar {
        id: titleBar
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        window: root
        onCloseRequested: root.controller.hideWindow()
    }

    RowLayout {
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: titleBar.bottom
        anchors.bottom: parent.bottom
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
                    objectName: "serverList"
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    clip: true
                    spacing: 3
                    boundsBehavior: Flickable.StopAtBounds
                    model: root.remotes().length
                    ScrollBar.vertical: VScrollBar { theme: root.theme }
                    delegate: SidebarItem {
                        required property int index
                        // Counted, not a list: an update keeps the items instead of recreating them
                        readonly property var entry: root.remotes()[index] || ({})
                        readonly property bool removing: root.isPending("remove|" + entry.name)
                        theme: root.theme
                        text: removing ? entry.name + "  · removing…" : entry.name
                        enabled: !removing
                        opacity: removing ? 0.5 : 1
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
                    objectName: "dirFlick"
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    contentHeight: dirCol.implicitHeight
                    clip: true
                    boundsBehavior: Flickable.StopAtBounds
                    ScrollBar.vertical: VScrollBar { theme: root.theme }
                    Column {
                        id: dirCol
                        objectName: "dirCol"
                        width: dirFlick.width - (dirFlick.ScrollBar.vertical.visible ? 14 : 0)
                        spacing: 6
                        // The folder just added, while the worker checks it.
                        Rectangle {
                            id: addingCard
                            readonly property var adding: root.selectedRemote() ? root.pending["add|" + root.selectedRemote().name] : undefined
                            visible: adding !== undefined
                            width: dirCol.width
                            height: 54
                            radius: root.theme.radius
                            color: root.theme.cardBg
                            border.color: root.theme.accentMuted
                            border.width: 1
                            RowLayout {
                                anchors.fill: parent
                                anchors.leftMargin: 12
                                anchors.rightMargin: 10
                                spacing: 10
                                BusyIndicator {
                                    running: addingCard.visible
                                    Layout.preferredWidth: 18
                                    Layout.preferredHeight: 18
                                    padding: 0
                                }
                                ColumnLayout {
                                    Layout.fillWidth: true
                                    spacing: 0
                                    Label {
                                        text: addingCard.adding ? addingCard.adding.label : ""
                                        color: root.theme.textPrimary
                                        font.pixelSize: 13
                                        elide: Text.ElideMiddle
                                        Layout.fillWidth: true
                                    }
                                    Label {
                                        text: "Adding folder…"
                                        color: root.theme.textMuted
                                        font.pixelSize: 11
                                        Layout.fillWidth: true
                                    }
                                }
                            }
                        }
                        Repeater {
                            model: (root.selectedRemote() ? root.selectedRemote().dirs : []).length
                            Rectangle {
                                id: dirCard
                                required property int index
                                readonly property var entry: (root.selectedRemote() ? root.selectedRemote().dirs : [])[index] || ({})
                                property var live: root.progressFor(root.selectedName, entry)
                                readonly property bool stopping: root.isPending(root.pairKey("stop", root.selectedName, entry.localPath, entry.remotePath))
                                readonly property bool starting: root.isPending(root.pairKey("resume", root.selectedName, entry.localPath, entry.remotePath))
                                width: dirCol.width
                                opacity: stopping ? 0.5 : 1
                                // Compact two-line rows: 54 px idle, 84 px while a
                                // progress bar is shown.
                                height: live ? 84 : 54
                                radius: root.theme.radius
                                color: root.theme.cardBg
                                border.color: root.theme.borderSubtle
                                border.width: 1
                                RowLayout {
                                    anchors.fill: parent
                                    anchors.leftMargin: 12
                                    anchors.rightMargin: 10
                                    anchors.topMargin: 7
                                    anchors.bottomMargin: 7
                                    spacing: 10
                                    // Above the card's MouseArea so the start button gets its clicks.
                                    z: 1
                                    Rectangle {
                                        width: 8
                                        height: 8
                                        radius: 4
                                        color: root.phaseColor(entry.phase)
                                        Layout.alignment: Qt.AlignVCenter
                                    }
                                    ColumnLayout {
                                        Layout.fillWidth: true
                                        spacing: 0
                                        Label {
                                            text: entry.localDisplay + "   →   " + entry.remoteDisplay
                                            color: root.theme.textPrimary
                                            font.pixelSize: 13
                                            elide: Text.ElideMiddle
                                            Layout.fillWidth: true
                                        }
                                        Label {
                                            text: dirCard.stopping ? "Stopping synchronization…"
                                                : (dirCard.starting ? "Starting synchronization…"
                                                : (entry.errorStatus ? entry.errorStatus + " " : "") + (entry.status || ""))
                                            color: entry.errorStatus && !dirCard.stopping && !dirCard.starting ? root.theme.danger : root.theme.textMuted
                                            font.pixelSize: 11
                                            elide: Text.ElideRight
                                            Layout.fillWidth: true
                                        }
                                        TransferProgress {
                                            visible: dirCard.live !== null
                                            live: dirCard.live
                                            theme: root.theme
                                            // The status line above already names the
                                            // current file; do not repeat it.
                                            showCurrent: false
                                            Layout.fillWidth: true
                                            Layout.topMargin: 2
                                        }
                                    }
                                    // One slot that is always in the row and as wide as
                                    // the button it shows: in Qt 6.4 the RowLayout does
                                    // not always rearrange when a child appears.
                                    Item {
                                        readonly property bool showStart: dirCard.entry.paused === true && !dirCard.stopping
                                        readonly property bool showSyncNow: dirCard.entry.paused !== true && (dirCard.entry.syncIntervalSecs || 0) > 0 && !dirCard.stopping
                                        objectName: "dirCardActions"
                                        Layout.preferredWidth: showStart ? cardStart.implicitWidth : (showSyncNow ? cardSyncNow.implicitWidth : 0)
                                        Layout.preferredHeight: cardStart.implicitHeight
                                        Layout.alignment: Qt.AlignVCenter
                                        PrimaryButton {
                                            id: cardStart
                                            text: dirCard.starting ? "Starting…" : "▶  Start"
                                            busy: dirCard.starting
                                            visible: parent.showStart
                                            anchors.fill: parent
                                            onClicked: root.resumeDir(dirCard.entry)
                                        }
                                        PrimaryButton {
                                            id: cardSyncNow
                                            text: root.syncNowBusy(dirCard.entry) ? "Syncing…" : "⟳  Sync now"
                                            busy: root.syncNowBusy(dirCard.entry)
                                            visible: parent.showSyncNow
                                            anchors.fill: parent
                                            onClicked: root.syncNow(dirCard.entry)
                                        }
                                    }
                                    Label {
                                        text: "›"
                                        color: root.theme.textDim
                                        font.pixelSize: 16
                                        Layout.alignment: Qt.AlignVCenter
                                    }
                                }
                                MouseArea {
                                    anchors.fill: parent
                                    enabled: !dirCard.stopping
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
                objectName: "detailFlick"
                anchors.fill: parent
                visible: root.detail !== null
                clip: true
                contentWidth: width
                contentHeight: detailCol.implicitHeight + 22
                boundsBehavior: Flickable.StopAtBounds
                ScrollBar.vertical: VScrollBar { theme: root.theme }

                ColumnLayout {
                    id: detailCol
                    objectName: "detailCol"
                    x: 22
                    y: 22
                    width: detailFlick.width - 44 - (detailFlick.ScrollBar.vertical.visible ? 10 : 0)
                    spacing: 6
                    // Anchored, not a RowLayout: in Qt 6.4 the layout does not
                    // always rearrange when a button appears while the page is
                    // hidden, which drew Sync now over Back or Stop syncing.
                    Item {
                        objectName: "detailHeader"
                        Layout.fillWidth: true
                        implicitHeight: stopDirButton.implicitHeight
                        Button {
                            text: "‹  Back"
                            flat: true
                            anchors.left: parent.left
                            anchors.verticalCenter: parent.verticalCenter
                            contentItem: Label { text: parent.text; color: root.theme.accentStrong; font.pixelSize: 13 }
                            background: Rectangle { color: "transparent" }
                            onClicked: root.detail = null
                        }
                        // Start and Sync now never show together: one is for a
                        // paused folder, the other for a running one.
                        PrimaryButton {
                            text: root.detailStarting ? "Starting…" : "▶  Start syncing"
                            busy: root.detailStarting
                            visible: root.detail !== null && root.detail.paused === true
                            anchors.right: stopDirButton.left
                            anchors.rightMargin: 8
                            anchors.verticalCenter: parent.verticalCenter
                            onClicked: {
                                // Unsaved exclusion edits apply before the first pass.
                                if (exclusionArea.text !== (root.detail.exclusions || []).join("\n"))
                                    root.saveExclusions(root.detail.localPath, exclusionArea.text)
                                root.resumeDir(root.detail)
                            }
                        }
                        PrimaryButton {
                            text: root.syncNowBusy(root.detail) ? "Syncing…" : "⟳  Sync now"
                            busy: root.syncNowBusy(root.detail)
                            visible: root.detail !== null && root.detail.paused !== true && (root.detail.syncIntervalSecs || 0) > 0
                            anchors.right: stopDirButton.left
                            anchors.rightMargin: 8
                            anchors.verticalCenter: parent.verticalCenter
                            onClicked: root.syncNow(root.detail)
                        }
                        PrimaryButton {
                            id: stopDirButton
                            text: "Stop syncing"
                            danger: true
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
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
                        text: root.detailStarting ? "Starting synchronization…" : (root.detail ? root.detail.status : "")
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
                        visible: !!root.detail && root.estimate.local === root.detail.localPath && root.estimate.remotePath === root.detail.remotePath && root.estimate.done
                    }
                    Label {
                        text: root.estimate.text ? root.estimate.text : ""
                        color: root.theme.textMuted
                        font.pixelSize: 13
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                        visible: !!root.detail && root.estimate.local === root.detail.localPath && root.estimate.remotePath === root.detail.remotePath && root.estimate.done
                    }
                    Label {
                        text: "Recent changes"
                        color: root.theme.textSecondary
                        font.pixelSize: 12
                        font.bold: true
                        visible: !!root.detail && !!root.detail.recent && root.detail.recent.length > 0
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
                            opacity: root.errorPending(entry.id) ? 0.5 : 1
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
                                Label { text: entry.detail; color: root.theme.textMuted; font.pixelSize: 11; visible: !!entry.detail && entry.detail.length > 0; wrapMode: Text.WordWrap; width: errorCol.width }
                            }
                            BusyIndicator {
                                anchors.right: parent.right
                                anchors.verticalCenter: parent.verticalCenter
                                anchors.rightMargin: 6
                                width: 16
                                height: 16
                                padding: 0
                                visible: root.errorPending(entry.id)
                                running: visible
                            }
                            MouseArea {
                                anchors.fill: parent
                                cursorShape: root.errorPending(entry.id) ? Qt.BusyCursor : Qt.PointingHandCursor
                                onClicked: root.activateError(root.selectedName, root.detail.localPath, root.detail.remotePath, entry.id)
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
                            opacity: root.errorPending(entry.id) ? 0.5 : 1
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
                                    Label { text: entry.detail; color: root.theme.textMuted; font.pixelSize: 11; visible: !!entry.detail && entry.detail.length > 0; wrapMode: Text.WordWrap; width: parent.width }
                                }
                                Button {
                                    text: root.errorPending(entry.id) ? "Dismissing…" : "Dismiss"
                                    flat: true
                                    enabled: !root.errorPending(entry.id)
                                    contentItem: Label { text: parent.text; color: parent.enabled ? root.theme.warning : root.theme.textMuted; font.pixelSize: 12 }
                                    background: Rectangle { color: "transparent" }
                                    onClicked: root.dismissError(root.selectedName, root.detail.localPath, root.detail.remotePath, entry.id)
                                }
                            }
                        }
                    }
                    Label {
                        text: "Preserved versions"
                        color: root.theme.textSecondary
                        font.pixelSize: 13
                        font.bold: true
                        visible: !!root.detail && !!root.detail.recovery && root.detail.recovery.length > 0
                    }
                    Repeater {
                        model: (root.detail && root.detail.recovery ? root.detail.recovery.slice(0, 8) : []).length
                        Rectangle {
                            id: versionRow
                            required property int index
                            readonly property var entry: (root.detail && root.detail.recovery ? root.detail.recovery.slice(0, 8) : [])[index] || ({})
                            Layout.fillWidth: true
                            readonly property bool restoring: !!root.detail && root.isPending(root.pairKey("restore", root.selectedName, root.detail.localPath, root.detail.remotePath) + "|" + entry.id)
                            readonly property bool deleting: root.versionPending(entry.id)
                            implicitHeight: recoveryRow.implicitHeight + 16
                            opacity: deleting ? 0.5 : 1
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
                                    text: versionRow.restoring ? "Restoring…" : "Restore"
                                    flat: true
                                    enabled: !versionRow.restoring && !versionRow.deleting
                                    contentItem: Label { text: parent.text; color: parent.enabled ? root.theme.accentStrong : root.theme.textMuted; font.pixelSize: 13 }
                                    background: Rectangle { color: "transparent" }
                                    onClicked: {
                                        root.setPending(root.pairKey("restore", root.selectedName, root.detail.localPath, root.detail.remotePath) + "|" + entry.id, { byNotice: true })
                                        root.controller.restoreVersion(root.selectedName, root.detail.localPath, root.detail.remotePath, entry.id)
                                    }
                                }
                                Button {
                                    text: versionRow.deleting ? "Deleting…" : "Delete"
                                    flat: true
                                    enabled: !versionRow.restoring && !versionRow.deleting
                                    contentItem: Label { text: parent.text; color: root.theme.textMuted; font.pixelSize: 13 }
                                    background: Rectangle { color: "transparent" }
                                    onClicked: {
                                        root.popupMode = "confirm-version"
                                        root.popupTitle = "Delete preserved version?"
                                        root.popupBody = "The preserved " + entry.side + " copy of '" + entry.rel + "' (" + entry.when + ") is deleted for good and can no longer be restored."
                                        root.popupRef = { remote: root.selectedName, local: root.detail.localPath, remotePath: root.detail.remotePath, id: entry.id }
                                        dialog.open()
                                    }
                                }
                            }
                        }
                    }
                    Button {
                        text: root.allVersionsPending() ? "Deleting all preserved versions…" : "Delete all preserved versions"
                        enabled: !root.allVersionsPending()
                        flat: true
                        visible: !!root.detail && !!root.detail.recovery && root.detail.recovery.length > 0
                        contentItem: Label { text: parent.text; color: root.theme.textMuted; font.pixelSize: 13 }
                        background: Rectangle { color: "transparent" }
                        onClicked: {
                            root.popupMode = "confirm-all-versions"
                            root.popupTitle = "Delete all preserved versions?"
                            root.popupBody = "Every preserved version of this folder is deleted for good and can no longer be restored. Versions an unresolved conflict still needs are kept."
                            root.popupRef = { remote: root.selectedName, local: root.detail.localPath, remotePath: root.detail.remotePath }
                            dialog.open()
                        }
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 10
                        Label { text: "Keep preserved versions for"; color: root.theme.textSecondary; font.pixelSize: 13 }
                        ComboBox {
                            id: keepBox
                            readonly property var days: [1, 7, 30, 90, 365, 0]
                            Layout.preferredWidth: 160
                            model: ["1 day", "7 days", "30 days", "90 days", "1 year", "Forever"]
                            currentIndex: Math.max(0, days.indexOf(root.detail && root.detail.keepDays !== undefined ? root.detail.keepDays : 30))
                            onActivated: root.controller.setKeepVersions(root.selectedName, root.detail.localPath, root.detail.remotePath, days[currentIndex])
                            delegate: ItemDelegate {
                                width: keepBox.width
                                text: modelData
                                contentItem: Label { text: modelData; color: root.theme.textPrimary; font.pixelSize: 13; leftPadding: 10; verticalAlignment: Text.AlignVCenter }
                                background: Rectangle { color: highlighted ? root.theme.cardBgRaised : root.theme.cardBg }
                            }
                            contentItem: Label { text: keepBox.displayText; color: root.theme.textPrimary; font.pixelSize: 13; leftPadding: 10; verticalAlignment: Text.AlignVCenter }
                            background: Rectangle { color: root.theme.inputBg; radius: root.theme.radiusSm; border.color: root.theme.borderSubtle }
                        }
                        Item { Layout.fillWidth: true }
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 10
                        Label { text: "Keep a copy of files up to"; color: root.theme.textSecondary; font.pixelSize: 13 }
                        TextField {
                            id: recoveryMaxField
                            Layout.preferredWidth: 96
                            implicitHeight: 34
                            horizontalAlignment: TextInput.AlignRight
                            validator: IntValidator { bottom: 0; top: 1000000 }
                            color: root.theme.textPrimary
                            font.pixelSize: 13
                            selectByMouse: true
                            leftPadding: 10
                            rightPadding: 10
                            background: Rectangle {
                                radius: root.theme.radiusSm
                                color: root.theme.inputBg
                                border.color: recoveryMaxField.activeFocus ? root.theme.accent : root.theme.borderSubtle
                            }
                            onEditingFinished: {
                                var megabytes = parseInt(text)
                                if (isNaN(megabytes) || megabytes < 0) megabytes = 0
                                if (root.detail) root.controller.setRecoveryMaxMb(root.selectedName, root.detail.localPath, root.detail.remotePath, megabytes)
                            }
                        }
                        Label { text: "MB"; color: root.theme.textMuted; font.pixelSize: 13 }
                        Item { Layout.fillWidth: true }
                    }
                    Label {
                        text: "A bigger file is replaced or deleted without a preserved copy, so one large file cannot fill the recovery folder. 0 keeps no copies at all."
                        color: root.theme.textDim
                        font.pixelSize: 12
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 10
                        CheckBox {
                            id: intervalCheck
                            // A bare box: the label is a sibling, like the 2FA
                            // checkbox. A custom contentItem would be drawn
                            // over the indicator, which has no reserved
                            // padding of its own.
                            checked: false
                            Layout.alignment: Qt.AlignVCenter
                            onToggled: if (!root.seedingDetail) root.applyInterval()
                            indicator: Rectangle {
                                implicitWidth: 18
                                implicitHeight: 18
                                radius: 4
                                border.color: root.theme.accent
                                border.width: 1
                                color: intervalCheck.checked ? root.theme.accent : "transparent"
                            }
                        }
                        Label {
                            text: "Only synchronize this folder every"
                            color: root.theme.textSecondary
                            font.pixelSize: 13
                            Layout.alignment: Qt.AlignVCenter
                        }
                        TextField {
                            id: intervalField
                            Layout.preferredWidth: 72
                            implicitHeight: 34
                            enabled: intervalCheck.checked
                            horizontalAlignment: TextInput.AlignRight
                            validator: IntValidator { bottom: 1; top: 100000 }
                            text: "3"
                            color: intervalCheck.checked ? root.theme.textPrimary : root.theme.textDim
                            font.pixelSize: 13
                            selectByMouse: true
                            leftPadding: 10
                            rightPadding: 10
                            background: Rectangle {
                                radius: root.theme.radiusSm
                                color: root.theme.inputBg
                                border.color: intervalField.activeFocus ? root.theme.accent : root.theme.borderSubtle
                            }
                            onEditingFinished: if (!root.seedingDetail) root.applyInterval()
                        }
                        ComboBox {
                            id: intervalUnit
                            Layout.preferredWidth: 130
                            enabled: intervalCheck.checked
                            model: ["minutes", "hours", "days"]
                            currentIndex: 1
                            onActivated: if (!root.seedingDetail) root.applyInterval()
                            delegate: ItemDelegate {
                                width: intervalUnit.width
                                text: modelData
                                contentItem: Label { text: modelData; color: root.theme.textPrimary; font.pixelSize: 13; leftPadding: 10; verticalAlignment: Text.AlignVCenter }
                                background: Rectangle { color: highlighted ? root.theme.cardBgRaised : root.theme.cardBg }
                            }
                            contentItem: Label { text: intervalUnit.displayText; color: intervalUnit.enabled ? root.theme.textPrimary : root.theme.textDim; font.pixelSize: 13; leftPadding: 10; verticalAlignment: Text.AlignVCenter }
                            background: Rectangle { color: root.theme.inputBg; radius: root.theme.radiusSm; border.color: root.theme.borderSubtle }
                        }
                        Item { Layout.fillWidth: true }
                    }
                    Label {
                        text: "A folder on a schedule is checked only that often: local changes do not start a pass in between, and a file that is still changing is uploaded as it was when the pass started instead of waiting. Off by default."
                        color: root.theme.textDim
                        font.pixelSize: 12
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    Label { text: "File/folder exclusions"; color: root.theme.textSecondary; font.pixelSize: 13; font.bold: true }
                    Label {
                        text: "Edit or remove any starting rule and save. One rule per line: glob paths (~/ is home, / is absolute), regex: followed by a regular expression, or builtin:rust-target for Rust build folders. Regex paths are relative to this folder."
                        color: root.theme.textDim
                        font.pixelSize: 12
                        wrapMode: Text.WordWrap
                        Layout.fillWidth: true
                    }
                    // Fixed-height editor with its own scroll bar: a long
                    // exclusion list must not stretch the settings page.
                    Rectangle {
                        id: exclusionFrame
                        Layout.fillWidth: true
                        Layout.preferredHeight: 200
                        color: root.theme.inputBg
                        radius: root.theme.radiusSm
                        border.width: 1
                        border.color: exclusionArea.activeFocus ? root.theme.accent : root.theme.borderSubtle
                        clip: true

                        // Keep the caret visible inside the box instead of scrolling the page.
                        function ensureVisible(item, rect) {
                            var top = item.mapToItem(exclusionFlick.contentItem, rect.x, rect.y).y
                            var bottom = top + rect.height
                            if (top < exclusionFlick.contentY) exclusionFlick.contentY = Math.max(0, top - 10)
                            else if (bottom > exclusionFlick.contentY + exclusionFlick.height) exclusionFlick.contentY = Math.min(exclusionFlick.contentHeight - exclusionFlick.height, bottom - exclusionFlick.height + 10)
                        }

                        Flickable {
                            id: exclusionFlick
                            objectName: "exclusionFlick"
                            anchors.fill: parent
                            anchors.margins: 1
                            clip: true
                            contentWidth: width
                            contentHeight: exclusionArea.implicitHeight + 2
                            boundsBehavior: Flickable.StopAtBounds
                            ScrollBar.vertical: VScrollBar { theme: root.theme }
                            TextArea {
                                id: exclusionArea
                                objectName: "exclusionArea"
                                // Always leave room for the scroll bar so wrapping
                                // does not toggle its own visibility.
                                width: exclusionFlick.width - 8
                                color: root.theme.textPrimary
                                wrapMode: TextEdit.WrapAnywhere
                                font.family: "monospace"
                                font.pixelSize: 13
                                selectByMouse: true
                                padding: 10
                                background: null
                                onCursorRectangleChanged: if (activeFocus) exclusionFrame.ensureVisible(exclusionArea, cursorRectangle)
                            }
                        }
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        Layout.bottomMargin: 22
                        spacing: 10
                        Label {
                            objectName: "exclusionCount"
                            Layout.fillWidth: true
                            text: exclusionArea.text.split("\n").filter(function (line) { return line.trim().length > 0 }).length
                                  + " rules · scroll inside the box to see them all"
                            color: root.theme.textDim
                            font.pixelSize: 11
                        }
                        Label {
                            text: "Saved"
                            visible: root.exclusionsSavedTimer.running
                            color: root.theme.statusGreen
                            font.pixelSize: 12
                        }
                        PrimaryButton {
                            readonly property bool saving: !!root.detail && root.isPending("exclusions|" + root.detail.localPath)
                            text: saving ? "Saving…" : "Save exclusions"
                            busy: saving
                            onClicked: root.saveExclusions(root.detail.localPath, exclusionArea.text)
                        }
                    }
                }
            }
        }
    }

    // Resize handles (the window is frameless).
    MouseArea {
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        anchors.bottomMargin: 12
        width: 5
        cursorShape: Qt.SizeHorCursor
        onPressed: root.startSystemResize(Qt.RightEdge)
    }
    MouseArea {
        anchors.left: parent.left
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        width: 5
        cursorShape: Qt.SizeHorCursor
        onPressed: root.startSystemResize(Qt.LeftEdge)
    }
    MouseArea {
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.rightMargin: 12
        height: 5
        cursorShape: Qt.SizeVerCursor
        onPressed: root.startSystemResize(Qt.BottomEdge)
    }
    MouseArea {
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        width: 12
        height: 12
        cursorShape: Qt.SizeFDiagCursor
        onPressed: root.startSystemResize(Qt.RightEdge | Qt.BottomEdge)
    }

    // Thin frame: frameless windows otherwise blend into dark desktops.
    Rectangle {
        anchors.fill: parent
        color: "transparent"
        border.width: 1
        border.color: root.theme.borderSubtle
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
                                busy: root.isPending("pick")
                                Layout.alignment: Qt.AlignBottom
                                onClicked: {
                                    root.setPending("pick", {})
                                    root.controller.pickLocalFolder(localField.text)
                                }
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
                    busy: root.popupMode === "login" && root.isPending("login")
                    text: root.popupMode === "login" ? (busy ? "Logging in…" : "Log in") : (root.popupMode === "add" ? "Add" : (root.popupMode === "conflict-info" ? "Acknowledge" : (root.popupMode === "confirm-space" ? "Sync anyway" : (root.popupMode === "confirm-version" || root.popupMode === "confirm-all-versions" ? "Delete" : "Confirm"))))
                    visible: root.popupMode === "login" || root.popupMode === "add" || root.popupMode === "confirm-remote" || root.popupMode === "confirm-dir" || root.popupMode === "confirm-dismiss" || root.popupMode === "conflict-info" || root.popupMode === "confirm-space" || root.popupMode === "confirm-version" || root.popupMode === "confirm-all-versions"
                    onClicked: {
                        if (root.popupMode === "login") {
                            root.setPending("login", { byNotice: true })
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
                            root.setPending("add|" + root.selectedName, {
                                byNotice: true,
                                label: localField.text + "   →   " + remoteField.text
                            })
                            root.controller.addDir(root.selectedName, localField.text, remoteField.text)
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-remote") {
                            root.deleteRemote(root.selectedRemote().name)
                            root.selectedName = ""
                            root.detail = null
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-dir") {
                            root.stopDir(root.selectedName, root.detail.localPath, root.detail.remotePath)
                            root.detail = null
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-dismiss" || root.popupMode === "confirm-space") {
                            root.dismissError(root.popupRef.remote, root.popupRef.local, root.popupRef.remotePath, root.popupRef.id)
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-all-versions") {
                            var ref = root.popupRef
                            var count = (root.findDir(ref.remote, ref.local, ref.remotePath) || {}).recovery
                            count = count ? count.length : 0
                            root.setPending(root.pairKey("versions", ref.remote, ref.local, ref.remotePath), {
                                // Versions a conflict still needs are kept and reported.
                                byNotice: true,
                                settled: function () {
                                    var dir = root.findDir(ref.remote, ref.local, ref.remotePath)
                                    return !dir || (dir.recovery || []).length !== count
                                }
                            })
                            root.controller.deleteAllVersions(ref.remote, ref.local, ref.remotePath)
                            root.closeDialog()
                        } else if (root.popupMode === "confirm-version") {
                            var version = root.popupRef
                            root.setPending(root.pairKey("version", version.remote, version.local, version.remotePath) + "|" + version.id, {
                                settled: function () {
                                    var dir = root.findDir(version.remote, version.local, version.remotePath)
                                    return !dir || !root.hasId(dir.recovery, version.id)
                                }
                            })
                            root.controller.deleteVersion(version.remote, version.local, version.remotePath, version.id)
                            root.closeDialog()
                        } else if (root.popupMode === "conflict-info") {
                            root.dismissError(root.popupRef.remote, root.popupRef.local, root.popupRef.remotePath, root.popupRef.id)
                            root.closeDialog()
                        }
                    }
                }
                PrimaryButton {
                    text: "Delete everywhere"
                    visible: root.popupMode === "conflict-info" && !!root.popupRef && root.popupRef.canDelete === true
                    onClicked: {
                        root.setPending(root.pairKey("error", root.popupRef.remote, root.popupRef.local, root.popupRef.remotePath) + "|" + root.popupRef.id, { byNotice: true })
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
