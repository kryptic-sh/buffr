// Pause every HTML media element in the document. Injected by the CEF
// backend when a tab is closed but stashed for undo (backlog §11 item 4):
// was_hidden(1) does not cut audio, and stopping the streams is what makes
// CEF fire OnAudioStreamStopped so the audio-state indicator clears.
(function () {
    'use strict';

    try {
        var els = document.querySelectorAll('video,audio');
        for (var i = 0; i < els.length; i++) {
            if (!els[i].paused) {
                els[i].pause();
            }
        }
    } catch (e) {
        // Never let a hostile page's DOM break the close path.
    }
})();
