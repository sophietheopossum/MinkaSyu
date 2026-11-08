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
        function setIgnored(name, ignored) {
            if (!!ignoredUpdates[name] === ignored)
                return;
            const next = Object.assign({}, ignoredUpdates);
            if (ignored)
                next[name] = true;
            else
                delete next[name];
            ignoredUpdates = next;
        }

        // Browse selections are repo-qualified targets, one build per
        // package: ticking another repo's row of the same package
        // moves the tick rather than selecting two builds.
        function installTargetFor(name) {
            for (const key of Object.keys(installSelection))
                if (key === name || key.endsWith("/" + name))
                    return key;
            return "";
        }
        function selectInstallTarget(target, name) {
            const next = {};
            for (const key of Object.keys(installSelection))
                if (key !== name && !key.endsWith("/" + name))
                    next[key] = true;
            next[target] = true;
            installSelection = next;
        }
        function toggleInstallTarget(target, name) {
            if (installSelection[target]) {
                const next = Object.assign({}, installSelection);
                delete next[target];
                installSelection = next;
            } else {
                selectInstallTarget(target, name);
            }
        }

        // Package detail overlay: AppStream garnish over the pacman
        // facts the row already knows. The fallback is what shows
        // while loading, and all that shows when a package has no
        // AppStream component at all.
        property bool detailVisible: false
        property string detailName: ""
        property string detailContext: ""    // updates | browse | installed
        property var detailFallback: null     // {version, repo, description}
        property string detailRepoTarget: ""
        readonly property var detailApp: Syu.appstreamFor === detailName ? Syu.appstreamData : null
        readonly property var detailRepos: Syu.reposFor === detailName ? Syu.repoOptions : []
        readonly property var detailSelectedOption: detailRepos.find(o => o.target === detailRepoTarget) ?? null

        // Manual repository choices for pending updates, name ->
        // {repo, version, target}. In memory only, like the ticks.
        property var repoOverrides: ({})

        function openDetail(name, fallback, context) {
            detailName = name;
            detailFallback = fallback;
            detailContext = context ?? "";
            detailVisible = true;
            detailRepoTarget = repoOverrides[name]?.target
                ?? installTargetFor(name)
                ?? (fallback?.repo ? fallback.repo + "/" + name : "");
            if (detailRepoTarget === "")
                detailRepoTarget = fallback?.repo ? fallback.repo + "/" + name : "";
            Syu.loadAppstream(name);
            Syu.loadRepos(name);
        }

        function defaultDetailTarget() {
            if (detailRepos.length === 0)
                return detailFallback?.repo ? detailFallback.repo + "/" + detailName : "";
            if (detailContext === "updates") {
                const ov = repoOverrides[detailName];
                if (ov && detailRepos.some(o => o.target === ov.target))
                    return ov.target;
                const u = effectiveUpdates.find(x => x.name === detailName);
                if (u) {
                    const t = u.candidate_repo + "/" + u.name;
                    if (detailRepos.some(o => o.target === t))
                        return t;
                }
            }
            if (detailContext === "browse") {
                const chosen = installTargetFor(detailName);
                if (chosen && detailRepos.some(o => o.target === chosen))
                    return chosen;
            }
            if (detailContext === "installed") {
                const inst = detailRepos.find(o => o.is_installed);
                if (inst)
                    return inst.target;
            }
            const fallback = detailFallback?.repo ? detailFallback.repo + "/" + detailName : "";
            if (fallback && detailRepos.some(o => o.target === fallback))
                return fallback;
            return detailRepos[0].target;
        }

        function syncDetailRepoSelection() {
            if (!detailVisible || !Syu.reposLoaded || Syu.reposFor !== detailName)
                return;
            if (!detailRepos.some(o => o.target === detailRepoTarget))
                detailRepoTarget = defaultDetailTarget();
        }

        function chooseDetailRepo() {
            const option = detailSelectedOption;
            if (!option)
                return;
            if (detailContext === "updates") {
                const next = Object.assign({}, repoOverrides);
                next[option.name] = { repo: option.repo, version: option.version, target: option.target };
                repoOverrides = next;
                setIgnored(option.name, false);
            } else if (detailContext === "browse") {
                selectInstallTarget(option.target, option.name);
            } else if (detailContext === "installed") {
                Syu.installPackages([option.target]);
            }
            detailVisible = false;
        }

        Connections {
            target: Syu
            function onRepoOptionsChanged() { win.syncDetailRepoSelection(); }
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

        function effectiveUpdate(u) {
            const ov = repoOverrides[u.name];
            if (!ov)
                return u;
            const copy = Object.assign({}, u);
            const defaultRepo = u.explicit_target ? (u.first_repo ?? u.candidate_repo) : u.candidate_repo;
            copy.new_version = ov.version;
            copy.candidate_repo = ov.repo;
            copy.explicit_target = ov.repo !== defaultRepo;
            if (copy.explicit_target && !copy.first_repo) {
                copy.first_repo = defaultRepo;
                copy.first_version = u.new_version;
            }
            if (!copy.explicit_target) {
                copy.first_repo = null;
                copy.first_version = null;
            }
            copy.transition = !u.installed_repo ? "unknown_source" : (u.installed_repo === ov.repo ? "same" : "changed");
            return copy;
        }

        readonly property var updates: Syu.state ? (Syu.state.updates ?? []) : []
        readonly property var effectiveUpdates: updates.map(u => win.effectiveUpdate(u))
        readonly property int checkedAt: Syu.state ? (Syu.state.checked_at ?? 0) : 0
        readonly property int repoChanges: effectiveUpdates.filter(u => u.transition === "changed").length
        readonly property var repoUpdates: effectiveUpdates.filter(u => u.candidate_repo !== "aur")
        readonly property var aurUpdates: effectiveUpdates.filter(u => u.candidate_repo === "aur")
        readonly property var ignoredNames: updates.filter(u => ignoredUpdates[u.name]).map(u => u.name)
        readonly property int selectedUpdateCount: updates.length - ignoredNames.length
        // Ticked candidates from a lower-ranked repo than pacman's
        // own: the core adds these as explicit repo/name targets.
        readonly property var explicitTargets: effectiveUpdates
            .filter(u => u.explicit_target === true && !ignoredUpdates[u.name])
            .map(u => u.candidate_repo + "/" + u.name)

        // Lower-repo candidates start UNTICKED (opt-in per package):
        // the cross-repository scan also finds cases where the newest
        // version trades a v4-optimized build for a plain one, and
        // taking that by default would be a nasty surprise. Seeding
        // is tracked per name for this window's lifetime, so a manual
        // retick is not undone by a state refresh.
        property var _seededExplicit: ({})

        // A recheck can make unticked names vanish (applied, or the
        // update itself went away); prune so a stale name can never
        // ride into a later --ignore list.
        onUpdatesChanged: {
            const keep = {};
            const seeded = {};
            let changedIgnored = false;
            let changedSeeded = false;
            for (const u of updates) {
                if (ignoredUpdates[u.name])
                    keep[u.name] = true;
                if (_seededExplicit[u.name])
                    seeded[u.name] = true;
                if (u.explicit_target === true && !seeded[u.name]) {
                    seeded[u.name] = true;
                    keep[u.name] = true;
                    changedSeeded = true;
                    changedIgnored = true;
                }
            }
            for (const k of Object.keys(ignoredUpdates))
                if (!keep[k])
                    changedIgnored = true;
            for (const k of Object.keys(_seededExplicit))
                if (!seeded[k])
                    changedSeeded = true;
            if (changedIgnored)
                ignoredUpdates = keep;
            if (changedSeeded)
                _seededExplicit = seeded;

            const keepOverrides = {};
            for (const u of updates)
                if (repoOverrides[u.name])
                    keepOverrides[u.name] = repoOverrides[u.name];
            if (Object.keys(keepOverrides).length !== Object.keys(repoOverrides).length)
                repoOverrides = keepOverrides;
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
                                    onNameClicked: win.openDetail(modelData.name, { version: modelData.new_version, repo: modelData.candidate_repo, description: "" }, "updates")
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
                                    onNameClicked: win.openDetail(modelData.name, { version: modelData.new_version, repo: modelData.candidate_repo, description: "" }, "updates")
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
                            text: {
                                if (win.ignoredNames.length === 0 && win.explicitTargets.length === 0)
                                    return "Opens rio running paru -Syu --sudoloop — review and confirm there.";
                                let s = "Opens rio: paru -Syu --sudoloop";
                                if (win.ignoredNames.length > 0)
                                    s += " --ignore=" + win.ignoredNames.join(",");
                                if (win.explicitTargets.length > 0)
                                    s += " " + win.explicitTargets.join(" ");
                                return s;
                            }
                            color: Theme.textFaint
                            font.family: Theme.fontFamily
                            font.pixelSize: Theme.fontSize - 2
                            wrapMode: Text.Wrap
                        }

                        PushButton {
                            label: "Update…"
                            primary: true
                            enabled: win.selectedUpdateCount > 0 && !Syu.checking
                            onClicked: Syu.apply(win.ignoredNames, win.explicitTargets)
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
                            selected: !!win.installSelection[modelData.repo + "/" + modelData.name]
                            onToggled: win.toggleInstallTarget(modelData.repo + "/" + modelData.name, modelData.name)
                            onNameClicked: win.openDetail(modelData.name, { version: modelData.version, repo: modelData.repo, description: modelData.description ?? "" }, "browse")
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
                            onNameClicked: win.openDetail(modelData.name, { version: modelData.version, repo: modelData.repo, description: modelData.description ?? "" }, "installed")
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

        // ---- Package detail overlay ------------------------------
        // AppStream garnish (name/summary/description/screenshot)
        // over the pacman facts from the row that was clicked. Click
        // outside, Close, or Escape dismisses it.
        Item {
            anchors.fill: parent
            visible: win.detailVisible
            z: 10

            Rectangle {
                anchors.fill: parent
                color: Theme.ground
                opacity: 0.88
                MouseArea {
                    anchors.fill: parent
                    onClicked: win.detailVisible = false
                }
            }

            Rectangle {
                id: detailPanel
                anchors.centerIn: parent
                width: Math.min(680, parent.width - 48)
                height: Math.min(560, parent.height - 48)
                radius: 8
                color: Theme.surface
                border.width: 1
                border.color: Theme.line

                MouseArea { anchors.fill: parent }   // eat clicks so the scrim doesn't close us

                ColumnLayout {
                    anchors.fill: parent
                    anchors.margins: 16
                    spacing: 10

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 12

                        Image {
                            visible: status === Image.Ready
                            source: win.detailApp?.icon_url ?? ""
                            Layout.preferredWidth: 56
                            Layout.preferredHeight: 56
                            fillMode: Image.PreserveAspectFit
                            asynchronous: true
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 1

                            Text {
                                Layout.fillWidth: true
                                text: win.detailApp?.name || win.detailName
                                color: Theme.text
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize + 3
                                font.bold: true
                                elide: Text.ElideRight
                            }
                            Text {
                                Layout.fillWidth: true
                                visible: text.length > 0
                                text: win.detailApp?.summary ?? ""
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                                wrapMode: Text.Wrap
                            }
                            Text {
                                Layout.fillWidth: true
                                text: win.detailName
                                    + (win.detailFallback?.version ? "  " + win.detailFallback.version : "")
                                    + (win.detailFallback?.repo ? "  [" + win.detailFallback.repo + "]" : "")
                                    + (win.detailApp?.id ? "  ·  " + win.detailApp.id : "")
                                color: Theme.textFaint
                                font.family: Theme.monoFamily
                                font.pixelSize: Theme.fontSize - 2
                                elide: Text.ElideRight
                            }
                        }
                        PushButton {
                            label: "Close"
                            Layout.alignment: Qt.AlignTop
                            onClicked: win.detailVisible = false
                        }
                    }

                    Flickable {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        clip: true
                        contentHeight: detailColumn.implicitHeight
                        boundsBehavior: Flickable.StopAtBounds

                        ColumnLayout {
                            id: detailColumn
                            width: parent.width
                            spacing: 10

                            Image {
                                visible: status === Image.Ready
                                source: (win.detailApp?.screenshots?.length ?? 0) > 0 ? win.detailApp.screenshots[0] : ""
                                Layout.fillWidth: true
                                Layout.preferredHeight: visible ? 240 : 0
                                fillMode: Image.PreserveAspectFit
                                asynchronous: true
                            }

                            Text {
                                visible: Syu.reposLoading || win.detailRepos.length > 0
                                text: "AVAILABLE REPOSITORIES"
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 3
                                font.bold: true
                            }
                            Text {
                                visible: Syu.reposLoading && Syu.reposFor === win.detailName
                                text: "Loading repository versions…"
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                visible: Syu.reposLoaded && Syu.reposFor === win.detailName && win.detailRepos.length === 0
                                text: "No repository builds found for this package."
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Repeater {
                                model: win.detailRepos
                                Rectangle {
                                    id: repoOptionRow
                                    required property var modelData
                                    Layout.fillWidth: true
                                    implicitHeight: 36
                                    radius: 5
                                    color: win.detailRepoTarget === repoOptionRow.modelData.target ? Theme.surfaceRaised : Theme.surface
                                    border.width: 1
                                    border.color: win.detailRepoTarget === repoOptionRow.modelData.target ? Theme.purple : Theme.line

                                    RowLayout {
                                        anchors.fill: parent
                                        anchors.leftMargin: 6
                                        anchors.rightMargin: 9
                                        spacing: 8

                                        CheckBox {
                                            checked: win.detailRepoTarget === repoOptionRow.modelData.target
                                            onToggled: win.detailRepoTarget = repoOptionRow.modelData.target
                                        }
                                        Text {
                                            text: repoOptionRow.modelData.repo
                                            color: repoOptionRow.modelData.repo === "aur" ? Theme.purple : Theme.text
                                            font.family: Theme.monoFamily
                                            font.pixelSize: Theme.fontSize - 1
                                        }
                                        Text {
                                            text: repoOptionRow.modelData.version
                                            color: Theme.textMuted
                                            font.family: Theme.monoFamily
                                            font.pixelSize: Theme.fontSize - 2
                                            elide: Text.ElideRight
                                            Layout.fillWidth: true
                                        }
                                        Text {
                                            visible: (repoOptionRow.modelData.architecture ?? "").length > 0
                                            text: repoOptionRow.modelData.architecture ?? ""
                                            color: Theme.textFaint
                                            font.family: Theme.monoFamily
                                            font.pixelSize: Theme.fontSize - 2
                                        }
                                        Text {
                                            visible: repoOptionRow.modelData.is_default
                                            text: "pacman default"
                                            color: Theme.textFaint
                                            font.family: Theme.fontFamily
                                            font.pixelSize: Theme.fontSize - 2
                                        }
                                        Text {
                                            visible: repoOptionRow.modelData.is_newest
                                            text: "newest"
                                            color: Theme.purple
                                            font.family: Theme.fontFamily
                                            font.pixelSize: Theme.fontSize - 2
                                        }
                                        Text {
                                            visible: repoOptionRow.modelData.is_installed
                                            text: "installed"
                                            color: Theme.okGreen
                                            font.family: Theme.fontFamily
                                            font.pixelSize: Theme.fontSize - 2
                                        }
                                    }
                                }
                            }

                            Text {
                                visible: Syu.appstreamLoading && Syu.appstreamFor === win.detailName
                                text: "Loading AppStream data…"
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                visible: Syu.appstreamLoaded && Syu.appstreamFor === win.detailName && !win.detailApp
                                text: "No AppStream data for this package — showing pacman data only."
                                color: Theme.textFaint
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                                wrapMode: Text.Wrap
                                Layout.fillWidth: true
                            }
                            Text {
                                visible: text.length > 0
                                text: win.detailApp?.description || ((Syu.appstreamLoaded && !win.detailApp) ? (win.detailFallback?.description ?? "") : "")
                                color: Theme.text
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                                wrapMode: Text.Wrap
                                Layout.fillWidth: true
                            }

                            Text {
                                visible: (win.detailApp?.developer ?? "").length > 0
                                text: "Developer:  " + (win.detailApp?.developer ?? "")
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                visible: (win.detailApp?.categories?.length ?? 0) > 0
                                text: "Categories:  " + (win.detailApp?.categories?.join(" · ") ?? "")
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                                wrapMode: Text.Wrap
                                Layout.fillWidth: true
                            }
                            Text {
                                visible: (win.detailApp?.license ?? "").length > 0
                                text: "Licence:  " + (win.detailApp?.license ?? "")
                                color: Theme.textMuted
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                            }
                            Text {
                                visible: (win.detailApp?.homepage ?? "").length > 0
                                text: win.detailApp?.homepage ?? ""
                                color: homepageArea.containsMouse ? Theme.purple : Theme.purpleDim
                                font.family: Theme.fontFamily
                                font.pixelSize: Theme.fontSize - 1
                                font.underline: homepageArea.containsMouse
                                elide: Text.ElideRight
                                Layout.fillWidth: true

                                MouseArea {
                                    id: homepageArea
                                    anchors.fill: parent
                                    hoverEnabled: true
                                    cursorShape: Qt.PointingHandCursor
                                    onClicked: Qt.openUrlExternally(win.detailApp?.homepage ?? "")
                                }
                            }
                        }
                    }

                    RowLayout {
                        Layout.fillWidth: true
                        spacing: 10

                        Text {
                            Layout.fillWidth: true
                            text: win.detailRepoTarget.length > 0
                                ? "Selected build: " + win.detailRepoTarget
                                : "No repository build selected."
                            color: Theme.textFaint
                            font.family: Theme.monoFamily
                            font.pixelSize: Theme.fontSize - 2
                            elide: Text.ElideRight
                        }
                        PushButton {
                            label: win.detailContext === "updates" ? "Use for upgrade"
                                : win.detailContext === "browse" ? "Select for install"
                                : (win.detailSelectedOption?.is_installed ? "Reinstall selected" : "Install selected")
                            primary: win.detailContext === "installed"
                            enabled: win.detailSelectedOption !== null
                            onClicked: win.chooseDetailRepo()
                        }
                    }
                }
            }

            Shortcut {
                sequence: "Escape"
                enabled: win.detailVisible
                onActivated: win.detailVisible = false
            }
        }
    }
}
