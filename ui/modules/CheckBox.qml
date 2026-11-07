import QtQuick
import "../services"

// A small square tick box for list rows. The visible square is 18px
// but the hit area is the full 26px item -- an 18px target at row
// pitch was easy to miss by a few pixels (found headless 7/11/2026,
// clicking between rows did nothing). Selection state itself lives
// in the window (per-name sets); this only draws and reports clicks.
Item {
    id: root

    property bool checked: false
    signal toggled

    implicitWidth: 26
    implicitHeight: 26

    Rectangle {
        anchors.centerIn: parent
        width: 18
        height: 18
        radius: 4
        color: root.checked ? Theme.purpleDim : Theme.surface
        border.width: 1
        border.color: root.checked ? Theme.purple : (area.containsMouse ? Theme.textMuted : Theme.line)

        Text {
            anchors.centerIn: parent
            visible: root.checked
            text: "✓"
            color: Theme.text
            font.family: Theme.fontFamily
            font.pixelSize: Theme.fontSize - 2
            font.bold: true
        }
    }

    MouseArea {
        id: area
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: root.toggled()
    }
}
