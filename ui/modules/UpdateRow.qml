pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import "../services"

// One pending update: name, old -> new version, and its repository.
// Provenance is the point of the second line, so it is never hidden
// behind the section header alone:
//   - same repo: just the repo name, quiet;
//   - repo CHANGES: "cachyos-extra-v4 -> minka" in amber with a badge;
//   - origin unknown (no %INSTALLED_DB% in the local database):
//     "unknown source -> <candidate>", rendered faint and stated
//     plainly rather than guessed.
// A replacement (the candidate's %REPLACES% names an installed
// package, which -Syu then removes) gets its own line and badge.
RowLayout {
    id: root

    required property var update

    // Unticked = ignored for this run only (--ignore); the window
    // owns the set, this row only shows and reports.
    property bool selected: true
    signal toggled
    signal nameClicked

    readonly property bool repoChanged: update.transition === "changed"
    readonly property bool unknownSource: update.transition === "unknown_source"
    // A strictly newer copy in a lower-ranked repo than pacman's own
    // candidate: chosen by MinkaSyu's cross-repository scan and
    // applied as an explicit repo/name target.
    readonly property bool laterRepo: update.explicit_target === true
    // States from cores before replacements were detected lack the key.
    readonly property var replaced: update.replaces ?? []
    readonly property bool replacement: replaced.length > 0
    // old_version is the replaced package's unless the replacer is
    // installed itself; "9.4 -> 0.12" across two packages would read
    // like a downgrade, so then only the new version is shown.
    readonly property bool replacerInstalled: replacement && update.old_version !== replaced[0].version
    readonly property string badgeLabel: replacement ? "replaces"
        : repoChanged ? "repo change"
        : laterRepo ? "later repo"
        : ""

    spacing: 10
    Layout.fillWidth: true

    CheckBox {
        checked: root.selected
        Layout.alignment: Qt.AlignTop
        Layout.topMargin: 2
        onToggled: root.toggled()
    }

    ColumnLayout {
        Layout.fillWidth: true
        spacing: 1
        opacity: root.selected ? 1.0 : 0.45

        Text {
            text: root.update.name
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize
            font.underline: nameArea.containsMouse
            elide: Text.ElideRight
            Layout.fillWidth: true

            MouseArea {
                id: nameArea
                anchors.fill: parent
                hoverEnabled: true
                cursorShape: Qt.PointingHandCursor
                onClicked: root.nameClicked()
            }
        }

        RowLayout {
            spacing: 6

            Text {
                text: root.replacement && !root.replacerInstalled
                    ? root.update.new_version
                    : root.update.old_version + " → " + root.update.new_version
                color: Theme.textMuted
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }

            Text {
                visible: root.repoChanged
                text: root.update.installed_repo + " → " + root.update.candidate_repo
                color: Theme.warnAmber
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                visible: root.unknownSource
                text: "unknown source → " + root.update.candidate_repo
                color: Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                visible: !root.repoChanged && !root.unknownSource
                text: root.update.candidate_repo
                color: Theme.textFaint
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
            }
            Text {
                visible: root.laterRepo
                text: "newer than " + (root.update.first_repo ?? "?") + " " + (root.update.first_version ?? "?")
                color: Theme.purple
                font.family: Theme.monoFamily
                font.pixelSize: Theme.fontSize - 2
                elide: Text.ElideRight
            }
        }

        Text {
            visible: root.replacement
            text: "replaces " + root.replaced.map(r => r.name + " " + r.version).join(", ")
            color: Theme.red
            font.family: Theme.monoFamily
            font.pixelSize: Theme.fontSize - 2
            elide: Text.ElideRight
            Layout.fillWidth: true
        }
    }

    // The badge: a repo switch, or a package that removes another, is
    // what in this list deserves attention at a glance, so it gets a
    // filled marker instead of relying on the coloured text alone.
    Rectangle {
        visible: root.badgeLabel.length > 0
        radius: 4
        color: root.replacement ? Theme.redDim : root.repoChanged ? Theme.warnAmber : Theme.purpleDim
        implicitWidth: badgeText.implicitWidth + 12
        implicitHeight: badgeText.implicitHeight + 6

        Text {
            id: badgeText
            anchors.centerIn: parent
            text: root.badgeLabel
            color: root.repoChanged && !root.replacement ? Theme.ground : Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
            font.bold: true
        }
    }
}
