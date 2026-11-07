pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import Quickshell
import "services"
import "modules"

// MinkaSyu — updates, browse, and installed packages in one window.
//
// A thin view over the core: checking, searching, and listing happen
// in the Rust core; every mutation (upgrade, install, remove) happens
// in paru inside a terminal the core opens. Unticking an update only
// means "--ignore it this once" -- selections live here, in memory,
// and are never persisted by either side.
ShellRoot {
    FloatingWindow {
        id: win
        title: "MinkaSyu"

        // Closing the window ENDS the app (a Quickshell config otherwise
        // outlives its windows as an invisible process per launch).
        onClosed: Qt.quit()
        implicitWidth: 760
        implicitHeight: 640
        color: Theme.ground

        property int currentTab: 0      // 0 updates, 1 browse, 2 installed

        // Selection sets, name -> true. Objects rather than arrays so
        // toggling is O(1) and membership is one lookup in delegates;
        // reassigned on change so bindings notice.
        property var ignoredUpdates: ({})
        property var installSelection: ({})
        property var removeSelection: ({})

        // Written out per set rather than win[setName] = next: an
        // indexed write lands as a JS expando on the QML object and
        // never touches the declared property the bindings read, so
        // ticking silently did nothing (found headless, 7/11/2026).
        function toggledCopy(set, name) {
            const next = Object.assign({}, set);
            if (next[name])
                delete next[name];
            else
                next[name] = true;
            return next;
        }
        function toggleIn(setName, name) {
            if (setName === "ignoredUpdates")
                ignoredUpdates = toggledCopy(ignoredUpdates, name);
            else if (setName === "installSelection")
                installSelection = toggledCopy(installSelection, name);
            else if (setName === "removeSelection")
                removeSelection = toggledCopy(removeSelection, name);
        }
        function selectionNames(setName) {
            return Object.keys(win[setName]);
        }
        function formatSize(bytes) {
            if (!bytes)
                return "";
            if (bytes >= 1073741824)
                return (bytes / 1073741824).toFixed(2) + " GiB";
            if (bytes >= 1048576)
                return (bytes / 1048576).toFixed(1) + " MiB";
            return Math.round(bytes / 1024) + " KiB";
        }

        readonly property var updates: Syu.state ? (Syu.state.updates ?? []) : []
        readonly property int checkedAt: Syu.state ? (Syu.state.checked_at ?? 0) : 0
        readonly property int repoChanges: Syu.state ? (Syu.state.repo_change_count ?? 0) : 0
        readonly property var repoUpdates: updates.filter(u => u.candidate_repo !== "aur")
        readonly property var aurUpdates: updates.filter(u => u.candidate_repo === "aur")
        readonly property var ignoredNames: updates.filter(u => ignoredUpdates[u.name]).map(u => u.name)
        readonly property int selectedUpdateCount: updates.length - ignoredNames.length

        // A recheck can make unticked names vanish (applied, or the
        // update itself went away); prune so a stale name can never
        // ride into a later --ignore list.
        onUpdatesChanged: {
            const keep = {};
            let changed = false;
            for (const u of updates)
                if (ignoredUpdates[u.name])
                    keep[u.name] = true;
            for (const k of Object.keys(ignoredUpdates))
                if (!keep[k])
                    changed = true;
            if (changed)
                ignoredUpdates = keep;
        }

        property string installedFilter: ""
        property bool explicitOnly: true
        readonly property var visibleInstalled: {
            const needle = installedFilter.trim().toLowerCase();
            return Syu.installedList.filter(p =>
                (!explicitOnly || p.explicit)
                && (needle.length === 0
                    || p.name.toLowerCase().includes(needle)
                    || (p.description ?? "").toLowerCase().includes(needle)));
        }

        // Bumped once a minute so "checked N min ago" stays honest
        // while the window sits open.
        property int nowSecs: Math.floor(Date.now() / 1000)
        Timer {
            interval: 60000
            running: true
            repeat: true
            onTriggered: win.nowSecs = Math.floor(Date.now() / 1000)
        }

        readonly property string checkedLabel: {
            if (!Syu.state)
                return "waiting for the core…";
            if (checkedAt <= 0)
                return "never checked";
            const age = Math.max(0, nowSecs - checkedAt);
            if (age < 90)
                return "checked just now";
            if (age < 5400)
                return "checked " + Math.round(age / 60) + " min ago";
            if (age < 36 * 3600)
                return "checked " + Math.round(age / 3600) + " h ago";
            return "checked " + Math.round(age / 86400) + " d ago";
        }

        ColumnLayout {
            anchors.fill: parent
            anchors.margins: 18
            spacing: 12

            // Header: title block, tabs, recheck.
            RowLayout {
                Layout.fillWidth: true
                spacing: 10

                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 2

                    Text {
                        Layout.fillWidth: true
                        text: win.currentTab === 0 ? "System updates"
                            : win.currentTab === 1 ? "Browse packages" : "Installed packages"
                        color: Theme.text
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize + 4
                        font.bold: true
                    }
                    Text {
                        Layout.fillWidth: true
                        text: {
                            if (win.currentTab === 1)
                                return Syu.searching ? "searching repos + AUR…"
                                    : Syu.searched ? Syu.searchResults.length + " result" + (Syu.searchResults.length === 1 ? "" : "s") + (Syu.searchTruncated ? " (truncated — refine the query)" : "")
                                    : "search the sync repos and the AUR";
                            if (win.currentTab === 2)
                                return Syu.loadingInstalled ? "reading the local database…"
                                    : Syu.installedLoaded ? win.visibleInstalled.length + " of " + Syu.installedList.length + " packages shown"
                                    : "not loaded yet";
                            if (Syu.checking)
                                return "checking…";
                            if (win.updates.length === 0)
                                return win.checkedAt > 0 ? "everything is up to date" : "no check yet";
                            let s = win.updates.length + " update" + (win.updates.length === 1 ? "" : "s") + " available";
                            if (win.repoChanges > 0)
                                s += " — " + win.repoChanges + " change" + (win.repoChanges === 1 ? "s" : "") + " repository";
                            if (win.ignoredNames.length > 0)
                                s += " · " + win.ignoredNames.length + " unticked";
                            return s;
                        }
                        color: win.currentTab === 0 && win.repoChanges > 0 ? Theme.warnAmber : Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 1
                    }
                    Text {
                        Layout.fillWidth: true
                        visible: win.currentTab === 0
                        text: win.checkedLabel
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                }

                PushButton {
                    label: "Updates"
                    toggled: win.currentTab === 0
                    onClicked: win.currentTab = 0
                }
                PushButton {
                    label: "Browse"
                    toggled: win.currentTab === 1
                    onClicked: win.currentTab = 1
                }
                PushButton {
                    label: "Installed"
                    toggled: win.currentTab === 2
                    onClicked: {
                        win.currentTab = 2;
                        if (!Syu.installedLoaded)
                            Syu.loadInstalled();
                    }
                }
                PushButton {
                    visible: win.currentTab === 0
                    label: Syu.checking ? "Checking…" : "Recheck"
                    enabled: !Syu.checking
                    onClicked: Syu.recheck()
                }
            }

            // Error bar: failures keep the old data on screen, so the
            // failure has to be visible somewhere that is not the list.
            Rectangle {
                visible: Syu.lastError.length > 0
                Layout.fillWidth: true
                radius: 5
                color: Theme.surface
                border.width: 1
                border.color: Theme.redDim
                implicitHeight: errorText.implicitHeight + 12

                Text {
                    id: errorText
                    anchors.fill: parent
                    anchors.margins: 8
                    text: Syu.lastError
                    color: Theme.red
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 1
                    wrapMode: Text.Wrap
                    verticalAlignment: Text.AlignVCenter
                }
            }

            StackLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                currentIndex: win.currentTab

                // ---- Updates -------------------------------------
                ColumnLayout {
                    spacing: 10

                    RowLayout {
                        visible: win.updates.length > 0
                        spacing: 8
                        PushButton {
                            label: "Tick all"
                            onClicked: win.ignoredUpdates = ({})
                        }
                        PushButton {
                            label: "Untick all"
                            onClicked: {
                                const next = {};
                                for (const u of win.updates)
                                    next[u.name] = true;
                                win.ignoredUpdates = next;
                            }
                        }
                        Text {
                            text: win.selectedUpdateCount + " of " + win.updates.length + " ticked — unticked are ignored for this run only"
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                        }
                    }

                    Flickable {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        contentHeight: updatesColumn.implicitHeight
                        boundsBehavior: Flickable.StopAtBounds

                        ColumnLayout {
                            id: updatesColumn
                            width: parent.width
                            spacing: 7

                            Text {
                                visible: win.repoUpdates.length > 0
                                text: "REPOSITORIES"
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 3
                                font.bold: true
                                Layout.topMargin: 4
                            }
                            Repeater {
                                model: win.repoUpdates
                                UpdateRow {
                                    required property var modelData
                                    update: modelData
                                    selected: !win.ignoredUpdates[modelData.name]
                                    onToggled: win.toggleIn("ignoredUpdates", modelData.name)
                                }
                            }

                            Text {
                                visible: win.aurUpdates.length > 0
                                text: "AUR"
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 3
                                font.bold: true
                                Layout.topMargin: 10
                            }
                            Repeater {
                                model: win.aurUpdates
                                UpdateRow {
                                    required property var modelData
                                    update: modelData
                                    selected: !win.ignoredUpdates[modelData.name]
                                    onToggled: win.toggleIn("ignoredUpdates", modelData.name)
                                }
                            }

                            Text {
                                visible: win.updates.length === 0 && !Syu.checking
                                text: win.checkedAt > 0 ? "Nothing to update."
                                                         : "No check has run yet — hit Recheck."
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize
                                Layout.topMargin: 24
                                Layout.alignment: Qt.AlignHCenter
                            }
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 12

                        Text {
                            Layout.fillWidth: true
                            text: win.ignoredNames.length > 0
                                ? "Opens rio: paru -Syu --ignore " + win.ignoredNames.join(", ")
                                : "Opens rio running paru -Syu --sudoloop — review and confirm there."
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                            wrapMode: Text.Wrap
                        }

                        PushButton {
                            label: "Update…"
                            primary: true
                            enabled: win.selectedUpdateCount > 0 && !Syu.checking
                            onClicked: Syu.apply(win.ignoredNames)
                        }
                    }
                }
                // ---- Browse --------------------------------------
                ColumnLayout {
                    spacing: 10

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 8

                        SearchField {
                            id: browseSearch
                            Layout.fillWidth: true
                            placeholder: "Search name or description…"
                            onAccepted: Syu.search(text)
                        }
                        PushButton {
                            label: Syu.searching ? "Searching…" : "Search"
                            enabled: !Syu.searching && browseSearch.text.trim().length > 0
                            onClicked: Syu.search(browseSearch.text)
                        }
                    }

                    Text {
                        visible: !Syu.searched && !Syu.searching
                        text: "Tick packages to install them together. Installed versions are marked."
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 1
                        Layout.alignment: Qt.AlignHCenter
                        Layout.topMargin: 18
                    }
                    Text {
                        visible: Syu.searched && !Syu.searching && Syu.searchResults.length === 0
                        text: "No matches."
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 1
                        Layout.alignment: Qt.AlignHCenter
                        Layout.topMargin: 18
                    }

                    ListView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        spacing: 7
                        model: Syu.searchResults
                        delegate: PackageRow {
                            required property var modelData
                            width: ListView.view.width
                            name: modelData.name
                            version: modelData.version
                            repo: modelData.repo
                            description: modelData.description ?? ""
                            marker: modelData.installed_version ? "installed " + modelData.installed_version : ""
                            selected: !!win.installSelection[modelData.name]
                            onToggled: win.toggleIn("installSelection", modelData.name)
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 12

                        Text {
                            Layout.fillWidth: true
                            text: win.selectionNames("installSelection").length > 0
                                ? win.selectionNames("installSelection").length + " selected — opens rio: paru -S " + win.selectionNames("installSelection").join(" ")
                                : "Nothing selected. Repo results reflect the last check's databases."
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                            wrapMode: Text.Wrap
                        }
                        PushButton {
                            label: "Clear"
                            enabled: win.selectionNames("installSelection").length > 0
                            onClicked: win.installSelection = ({})
                        }
                        PushButton {
                            label: "Install…"
                            primary: true
                            enabled: win.selectionNames("installSelection").length > 0
                            onClicked: Syu.installPackages(win.selectionNames("installSelection"))
                        }
                    }
                }

                // ---- Installed -----------------------------------
                ColumnLayout {
                    spacing: 10

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 8

                        SearchField {
                            Layout.fillWidth: true
                            placeholder: "Filter installed packages…"
                            onEdited: text => { win.installedFilter = text; }
                            onAccepted: win.installedFilter = text
                        }
                        PushButton {
                            label: "Explicit only"
                            toggled: win.explicitOnly
                            onClicked: win.explicitOnly = !win.explicitOnly
                        }
                        PushButton {
                            label: Syu.loadingInstalled ? "Loading…" : "Reload"
                            enabled: !Syu.loadingInstalled
                            onClicked: Syu.loadInstalled()
                        }
                    }

                    Text {
                        visible: Syu.installedLoaded && win.visibleInstalled.length === 0 && !Syu.loadingInstalled
                        text: "No installed packages match."
                        color: Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 1
                        Layout.alignment: Qt.AlignHCenter
                        Layout.topMargin: 18
                    }

                    ListView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        spacing: 7
                        model: win.visibleInstalled
                        delegate: PackageRow {
                            required property var modelData
                            width: ListView.view.width
                            name: modelData.name
                            version: modelData.version
                            repo: modelData.repo
                            description: modelData.description ?? ""
                            marker: modelData.explicit ? win.formatSize(modelData.size_bytes) : "dependency"
                            selected: !!win.removeSelection[modelData.name]
                            onToggled: win.toggleIn("removeSelection", modelData.name)
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 12

                        Text {
                            Layout.fillWidth: true
                            text: win.selectionNames("removeSelection").length > 0
                                ? win.selectionNames("removeSelection").length + " selected — opens rio: paru -Rns " + win.selectionNames("removeSelection").join(" ") + " (confirm there; Reload after)"
                                : "Tick packages to remove. Dependencies can be unticked out with Explicit only."
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                            wrapMode: Text.Wrap
                        }
                        PushButton {
                            label: "Clear"
                            enabled: win.selectionNames("removeSelection").length > 0
                            onClicked: win.removeSelection = ({})
                        }
                        PushButton {
                            label: "Remove…"
                            primary: true
                            enabled: win.selectionNames("removeSelection").length > 0
                            onClicked: Syu.removePackages(win.selectionNames("removeSelection"))
                        }
                    }
                }
            }
        }
    }
}
