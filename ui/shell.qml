pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import Quickshell
import "services"
import "modules"

// MinkaSyu — the update window.
//
// A thin view over the core's state: checking and listing happen in
// the Rust core, applying happens in paru inside a terminal. Nothing
// is computed here beyond grouping and phrasing; every update row's
// repository provenance comes straight from the state document.
ShellRoot {
    FloatingWindow {
        id: win
        title: "MinkaSyu"

        // Closing the window ENDS the app (a Quickshell config otherwise
        // outlives its windows as an invisible process per launch).
        onClosed: Qt.quit()
        implicitWidth: 680
        implicitHeight: 560
        color: Theme.ground

        readonly property var updates: Syu.state ? (Syu.state.updates ?? []) : []
        readonly property int checkedAt: Syu.state ? (Syu.state.checked_at ?? 0) : 0
        readonly property int repoChanges: Syu.state ? (Syu.state.repo_change_count ?? 0) : 0
        readonly property var repoUpdates: updates.filter(u => u.candidate_repo !== "aur")
        readonly property var aurUpdates: updates.filter(u => u.candidate_repo === "aur")

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

            // Header
            RowLayout {
                Layout.fillWidth: true
                spacing: 12

                ColumnLayout {
                    Layout.fillWidth: true
                    spacing: 2

                    // Layout.fillWidth on the texts as well as the
                    // column: a nested layout's maximum width is its
                    // widest child's, so without these the column
                    // cannot stretch and Recheck hugs the title.
                    Text {
                        Layout.fillWidth: true
                        text: "System updates"
                        color: Theme.text
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize + 4
                        font.bold: true
                    }
                    Text {
                        Layout.fillWidth: true
                        text: {
                            if (Syu.checking)
                                return "checking…";
                            if (win.updates.length === 0)
                                return win.checkedAt > 0 ? "everything is up to date" : "no check yet";
                            let s = win.updates.length + " update" + (win.updates.length === 1 ? "" : "s") + " available";
                            if (win.repoChanges > 0)
                                s += " — " + win.repoChanges + " change" + (win.repoChanges === 1 ? "s" : "") + " repository";
                            return s;
                        }
                        color: win.repoChanges > 0 ? Theme.warnAmber : Theme.textMuted
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 1
                    }
                    Text {
                        Layout.fillWidth: true
                        text: win.checkedLabel
                        color: Theme.textFaint
                        font.family: Theme.fontFamily
                        font.pixelSize: Theme.fontSize - 2
                    }
                }

                PushButton {
                    label: Syu.checking ? "Checking…" : "Recheck"
                    enabled: !Syu.checking
                    onClicked: Syu.recheck()
                }
            }

            // Error bar: a failed check keeps the old list on screen, so
            // the failure has to be visible somewhere that is not the list.
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

            // The list
            Flickable {
                Layout.fillWidth: true
                Layout.fillHeight: true
                clip: true
                contentHeight: listColumn.implicitHeight
                boundsBehavior: Flickable.StopAtBounds

                ColumnLayout {
                    id: listColumn
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

            // Footer
            RowLayout {
                Layout.fillWidth: true
                spacing: 12

                Text {
                    Layout.fillWidth: true
                    text: "Opens rio running paru -Syu --sudoloop — review and confirm there."
                    color: Theme.textFaint
                    font.family: Theme.fontFamily
                    font.pixelSize: Theme.fontSize - 2
                    wrapMode: Text.Wrap
                }

                PushButton {
                    label: "Update…"
                    primary: true
                    enabled: win.updates.length > 0 && !Syu.checking
                    onClicked: Syu.apply()
                }
            }
        }
    }
}
