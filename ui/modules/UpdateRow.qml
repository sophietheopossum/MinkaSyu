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
RowLayout {
    id: root

    required property var update

    // Unticked = ignored for this run only (--ignore); the window
    // owns the set, this row only shows and reports.
    property bool selected: true
    signal toggled

    readonly property bool repoChanged: update.transition === "changed"
    readonly property bool unknownSource: update.transition === "unknown_source"

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
            elide: Text.ElideRight
            Layout.fillWidth: true
        }

        RowLayout {
            spacing: 6

            Text {
                text: root.update.old_version + " → " + root.update.new_version
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
        }
    }

    // The badge: a repo switch is the one thing in this list that
    // deserves attention at a glance, so it gets a filled marker
    // instead of relying on the amber text alone.
    Rectangle {
        visible: root.repoChanged
        radius: 4
        color: Theme.warnAmber
        implicitWidth: badgeText.implicitWidth + 12
        implicitHeight: badgeText.implicitHeight + 6

        Text {
            id: badgeText
            anchors.centerIn: parent
            text: "repo change"
            color: Theme.ground
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 3
            font.bold: true
        }
    }
}
