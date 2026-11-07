import QtQuick
import "../services"

// A single-line text field in the house style, for search/filter
// boxes. Enter fires `accepted`; text changes stream via `edited`
// for live filters that want them.
Rectangle {
    id: root

    property alias text: input.text
    property string placeholder: ""
    signal accepted
    signal edited(string text)

    implicitHeight: 30
    radius: 5
    color: Theme.surface
    border.width: 1
    border.color: input.activeFocus ? Theme.purple : Theme.line

    TextInput {
        id: input
        anchors.fill: parent
        anchors.leftMargin: 9
        anchors.rightMargin: 9
        verticalAlignment: TextInput.AlignVCenter
        color: Theme.text
        selectionColor: Theme.purpleDim
        selectedTextColor: Theme.text
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 1
        clip: true
        onAccepted: root.accepted()
        onTextEdited: root.edited(text)
    }

    Text {
        anchors.fill: parent
        anchors.leftMargin: 9
        verticalAlignment: Text.AlignVCenter
        visible: input.text.length === 0
        text: root.placeholder
        color: Theme.textFaint
        font.family: Theme.fontFamily
        font.pixelSize: Theme.fontSize - 1
    }

    MouseArea {
        anchors.fill: parent
        cursorShape: Qt.IBeamCursor
        onClicked: input.forceActiveFocus()
    }
}
