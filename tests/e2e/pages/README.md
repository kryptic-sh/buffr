# Edit-mode e2e pages

Each page isolates one thing a real site does that can break the
click-an-input-enters-Insert path. They are plain static HTML so they can be
loaded over `file://` with no server.

The harness (`buffr-app --e2e-script`) loads a page, clicks a selector through
the real OSR mouse path, and records whether the engine reached
`PageMode::Insert`. See `tests/e2e/README.md` for how to run them.

| Page                          | Trick                                             | Expected after click       |
| ----------------------------- | ------------------------------------------------- | -------------------------- |
| `plain.html`                  | bare `input` / `textarea` / `contenteditable`     | Insert                     |
| `shadow_open.html`            | input inside an **open** shadow root              | Insert                     |
| `shadow_closed.html`          | input inside a **closed** shadow root             | Insert                     |
| `shadow_nested.html`          | shadow root inside a shadow root                  | Insert                     |
| `shadow_delegates_focus.html` | `attachShadow({delegatesFocus:true})`             | Insert                     |
| `iframe_same_origin.html`     | input inside a same-origin `iframe`               | Insert                     |
| `iframe_srcdoc.html`          | input inside a `srcdoc` iframe                    | Insert                     |
| `async_focus.html`            | click handler focuses the input 600 ms later      | Insert                     |
| `modal_late.html`             | click opens a dialog, focuses its input in rAF    | Insert                     |
| `react_rerender.html`         | the input node is replaced during the click       | Insert                     |
| `label_click.html`            | click lands on a `<label>`, not the input         | Insert                     |
| `dynamic_input.html`          | the input is created by the click handler         | Insert                     |
| `contenteditable_rich.html`   | click lands on a child of the editable root       | Insert                     |
| `designmode.html`             | `document.designMode = 'on'`                      | Insert                     |
| `stop_propagation.html`       | page swallows `focusin` on `window` capture       | Insert                     |
| `already_focused.html`        | click the field that already has focus            | Insert                     |
| `non_text_inputs.html`        | checkbox / radio / button / range / colour / file | **Normal** (must not fire) |
| `autofocus.html`              | page autofocuses on load, no user gesture         | **Normal** (must not fire) |

The last two are the inverse assertion: entering Insert there is the bug. A
suite that only checks "did we enter Insert" would pass while the browser
trapped the user in Insert every time they ticked a checkbox.
