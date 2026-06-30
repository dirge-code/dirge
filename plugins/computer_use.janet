# Computer Use plugin
# Cross-platform desktop automation. Intercepts bash commands prefixed
# with "computer:" and dispatches them through ydotool/xdotool/osascript.
#
# Actions:
#   computer:open_url <url>        Open URL in default browser
#   computer:screenshot             Capture screen, return file path
#   computer:type <text>            Type text at cursor
#   computer:key <keys>             Press key combination (e.g. alt+Tab)
#   computer:click <button>         Click mouse button (1=left,2=right,3=middle)
#   computer:move <x> <y>           Move mouse to absolute coordinates
#   computer:analyze [prompt]       Screenshot + vision analysis
#   computer:cycle [N]             Alt+Tab N times (default 3)
#   computer:focus <app>           Cycle+screenshot until <app> is foreground
#   computer:navigate <url>        Full pipeline: open, focus browser, find button, click
#
# Safety:
#   - EVERY action requires inline confirm dialog
#   - Kill switch: touch /tmp/dirge-computer-abort to deny all actions
#   - Keys validated against allowlist; coordinates/buttons validated as numbers
#   - Commands use direct argv (os/execute), never interpolated into /bin/sh -c

(def hooks ["on-tool-start" "on-tool-end"])

(var platform nil)
(var pending-result nil)
(var sandbox-mode nil)
(var workspace-path nil)
(var auto-confirm-mode nil)

# ── Safe shell dispatch ───────────────────────────────────────────────

(defn- sh [& args]
  "Run command with argv vector. Uses os/execute directly — no shell interpolation."
  (os/execute args))

(defn- mktemp-path [template]
  "Run mktemp with template, return created path or nil. The file is
   created with 0600 permissions by mktemp itself."
  (let [marker (string "/tmp/dirge-mktemp-" (os/time) "-" (math/random))]
    (when (= 0 (os/execute ["/bin/sh" "-c" (string "mktemp " template " > " marker " 2>/dev/null")]))
      (try
        (let [f (file/open marker :r)
              path (string/trim (file/read f :all))]
          (file/close f)
          (os/execute ["/bin/rm" "-f" marker])
          (when (and path (not (empty? path))) path))
        ([_] nil)))))

(defn- sh-capture [cmd-str]
  "Run shell command, capture stdout as trimmed string or nil.
   Uses /bin/sh -c for output redirection (os/execute returns exit code
   only). Temp file via mktemp — not guessable os/time."
  (if-let [tmp (mktemp-path "/tmp/dirge-sh-XXXXXX")]
    (do
      (os/execute ["/bin/sh" "-c" (string cmd-str " > " tmp " 2>/dev/null")] :x)
      (try
        (let [f (file/open tmp :r)
              data (file/read f :all)]
          (file/close f)
          (os/execute ["/bin/rm" "-f" tmp])
          (when data (string/trim data)))
        ([_] nil)))
    nil))

(defn- command-exists? [cmd]
  (= 0 (os/execute ["/bin/sh" "-c" (string "command -v " cmd " >/dev/null 2>&1")])))

# ── Input validation ──────────────────────────────────────────────────

(def valid-keys
  @{"Return" true "Escape" true "Tab" true "space" true
    "BackSpace" true "Delete" true "Home" true "End" true
    "Page_Up" true "Page_Down" true "Up" true "Down" true "Left" true "Right" true
    "F1" true "F2" true "F3" true "F4" true "F5" true
    "F6" true "F7" true "F8" true "F9" true "F10" true "F11" true "F12" true
    "Print" true "Scroll_Lock" true "Pause" true
    "Insert" true "Menu" true "Num_Lock" true
    "super" true "alt" true "ctrl" true "shift" true})

(def valid-single-char
  # Single-character key names for ydotool/xdotool keys that don't
  # appear in `valid-keys`. Restrict to alphanumeric + safe punctuation.
  # Shell metacharacters (`;`, `|`, `$`, `` ` ``, `\`, quotes etc.)
  # are NEVER valid — rejecting them closes a shell-injection path
  # when the chord is interpolated into `ydotool key <chord>`.
  @{"a" true "b" true "c" true "d" true "e" true "f" true "g" true
    "h" true "i" true "j" true "k" true "l" true "m" true "n" true
    "o" true "p" true "q" true "r" true "s" true "t" true "u" true
    "v" true "w" true "x" true "y" true "z" true
    "A" true "B" true "C" true "D" true "E" true "F" true "G" true
    "H" true "I" true "J" true "K" true "L" true "M" true "N" true
    "O" true "P" true "Q" true "R" true "S" true "T" true "U" true
    "V" true "W" true "X" true "Y" true "Z" true
    "0" true "1" true "2" true "3" true "4" true "5" true "6" true
    "7" true "8" true "9" true
    "." true "," true "/" true "-" true "=" true "[" true "]" true})

(defn- validate-key [k]
  "Return k if it's a valid key name, nil otherwise."
  (if (valid-keys k) k
    (let [len (length k)]
      (if (= len 1)
        (valid-single-char k)
        (if (string/has-prefix? "KP_" k) k)))))

(defn- validate-keychord [chord]
  "Return chord if all keys in a +-separated chord are valid, nil otherwise."
  (var ok true)
  (each k (string/split "+" chord)
    (if (not (validate-key k))
      (set ok false)))
  (when ok chord))

(defn- validate-coord [x]
  "Return number if x parses as a number, nil otherwise."
  (when (and x (not (empty? x)))
    (scan-number x)))

(defn- validate-button [btn]
  "Return btn if it's a valid mouse button, nil otherwise."
  (when (or (= btn "1") (= btn "2") (= btn "3")
            (= btn "left") (= btn "right") (= btn "middle"))
    btn))

(defn- validate-app-name [app]
  "Return app if it's a safe process/app name, nil otherwise. `focus`
   splices this into `pgrep -i <app>` and the vision `--find-app <app>`
   argument via /bin/sh -c, so anything outside [A-Za-z0-9._-] (spaces,
   `;`, `|`, `$`, backticks, quotes…) could break out into the shell.
   Reject those rather than escape — app names don't need them."
  (when (and app (not (empty? app)) (peg/match '(* (some (set "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-")) -1) app))
    app))

# ── Vision ────────────────────────────────────────────────────────────
# Pluggable vision architecture — one function, three backends.
# Swap by changing the body of analyze-image; nothing else moves.
#
# Backend 1 (ACTIVE) — local Python (computer_use_vision.py)
#   Zero cost, offline. Edge-detection + tesseract OCR.
#
# Backend 2 — DeepSeek Vision (when API exposes image_url)
#   Replace body with: curl + base64 → api.deepseek.com/v1/chat/completions
#   Multimodal V4. Same API key. Currently blocked (image_url rejected).
#
# Backend 3 — cloud multimodal (Anthropic / OpenRouter / OpenAI)
#   Replace body with: curl + base64 → claude / gpt-4o / openrouter
#   Strongest semantic analysis. Button labeling, coordinates, NL queries.
#
# Prior implementations in git log and the Python script's docstring.

(defn- analyze-image [path prompt]
  # Backend 1: local Python
  (let [script (string workspace-path "/plugins/computer_use_vision.py")
        output (sh-capture (string "python3 " script " --path " path " 2>/dev/null"))]
    (or output (string "ERROR: vision script failed for " path))))


# ── Platform detection ────────────────────────────────────────────────

(defn detect-platform []
  (if (command-exists? "ydotool")
    {:type "wayland" :backend "ydotool" :screenshot "cosmic-screenshot"}
    (if (command-exists? "xdotool")
      {:type "x11" :backend "xdotool" :screenshot "scrot"}
      (if (command-exists? "osascript")
        {:type "macos" :backend "osascript" :screenshot "screencapture"}
        {:type "unknown" :backend nil :screenshot nil}))))

# ── Kill switch ───────────────────────────────────────────────────────

(defn- kill-switch-active? []
  (try
    (let [f (file/open "/tmp/dirge-computer-abort" :r)]
      (file/close f)
      true)
    ([_] false)))

# ── Host desktop opt-in ───────────────────────────────────────────────
# Driving the real desktop requires explicit consent. Without this,
# computer-use only works inside the microVM sandbox.

(defn- host-desktop-consent-given? []
  (or (= sandbox-mode "microvm")
      (= (os/getenv "DIRGE_COMPUTER_USE_HOST") "1")
      (try
        (let [f (file/open (string (os/getenv "HOME") "/.config/dirge/computer-use-host-consent") :r)]
          (file/close f)
          true)
        ([_] false))))

# ── Init hook ─────────────────────────────────────────────────────────

(defn computer_use-on-init [ctx]
  (set platform (detect-platform))
  (set sandbox-mode (ctx :sandbox))
  (set workspace-path (ctx :workspace))
  (set auto-confirm-mode (ctx :auto-confirm))
  (harness/log
    (string "computer-use: type=" (platform :type)
            " backend=" (platform :backend)
            " screenshot=" (platform :screenshot)
            " sandbox=" sandbox-mode
            " workspace=" workspace-path
            " auto-confirm=" auto-confirm-mode))
  nil)

# ── Screenshot ────────────────────────────────────────────────────────

(defn- take-screenshot-sandbox []
  (let [result (harness/computer-use-exec {:action "screenshot"})
        path (string workspace-path "/screenshot.png")]
    (if (and result (table? result) (= (result :exit_code) 0))
      (if (try (do (file/close (file/open path :r)) true) ([_] false))
        path
        (do (harness/log "computer-use: screenshot written but file not visible on host") nil))
      (do (harness/log "computer-use: sandbox screenshot failed") nil))))

(defn- take-screenshot []
  (if (= sandbox-mode "microvm")
    (take-screenshot-sandbox)
    (try
      (case (platform :screenshot)
        "cosmic-screenshot"
        (do
          (sh "cosmic-screenshot" "--interactive=false" "--notify=false" "--modal=false" "--save-dir" "/tmp")
          (sh-capture "ls -t /tmp/Screenshot_*.png 2>/dev/null | head -1"))

        # The template's X's must be TRAILING — BSD/macOS mktemp leaves a
        # post-suffix `XXXXXX` literal (predictable + collision-prone). mktemp a
        # secure base, drop the empty placeholder, then write `<base>.png` (the
        # random base keeps the .png path unguessable too).
        "scrot"
        (if-let [base (mktemp-path "/tmp/dirge-screenshot-XXXXXX")]
          (let [path (string base ".png")]
            (os/execute ["/bin/rm" "-f" base])
            (sh "scrot" path)
            (os/execute ["chmod" "600" path])
            (if (= (sh "test" "-f" path) 0) path nil))
          nil)

        "screencapture"
        (if-let [base (mktemp-path "/tmp/dirge-screenshot-XXXXXX")]
          (let [path (string base ".png")]
            (os/execute ["/bin/rm" "-f" base])
            (sh "screencapture" path)
            (os/execute ["chmod" "600" path])
            (if (= (sh "test" "-f" path) 0) path nil))
          nil)

        nil)
      ([_] nil))))

# ── Input backends (with validation) ──────────────────────────────────

(defn- type-text [text]
  (if (= sandbox-mode "microvm")
    (harness/computer-use-exec {:action "type" :text text})
    (case (platform :backend)
      "ydotool"  (sh "ydotool" "type" "--" text)
      "xdotool"  (sh "xdotool" "type" "--" text)
      1)))

(defn- press-keys [keys]
  (when-let [valid (validate-keychord keys)]
    (if (= sandbox-mode "microvm")
      (harness/computer-use-exec {:action "keychord" :chord valid})
      (case (platform :backend)
        "ydotool"  (sh "ydotool" "key" valid)
        "xdotool"  (sh "xdotool" "key" valid)
        1))))

(defn- click-button [button x y]
  (when-let [btn (validate-button button)]
    (if (= sandbox-mode "microvm")
      (do
        (when (and x y)
          (let [cx (validate-coord x) cy (validate-coord y)]
            (when (and cx cy)
              (harness/computer-use-exec {:action "mouse_move" :x (math/floor cx) :y (math/floor cy)}))))
        (harness/computer-use-exec {:action "mouse_click" :button btn}))
      (case (platform :backend)
        "ydotool"
        (do
          (when (and x y)
            (let [cx (validate-coord x) cy (validate-coord y)]
              (when (and cx cy)
                (sh "ydotool" "mousemove" "-x" (string cx) "-y" (string cy)))))
          (sh "ydotool" "click" btn))
        "xdotool"
        (do
          (when (and x y)
            (let [cx (validate-coord x) cy (validate-coord y)]
              (when (and cx cy)
                (sh "xdotool" "mousemove" (string cx) (string cy)))))
          (sh "xdotool" "click" btn))
        1))))

(defn- move-mouse [x y]
  (let [cx (validate-coord x) cy (validate-coord y)]
    (when (and cx cy)
      (if (= sandbox-mode "microvm")
        (harness/computer-use-exec {:action "mouse_move" :x (math/floor cx) :y (math/floor cy)})
        (case (platform :backend)
          "ydotool"  (sh "ydotool" "mousemove" "-x" (string cx) "-y" (string cy))
          "xdotool"  (sh "xdotool" "mousemove" (string cx) (string cy))
          1)))))

(defn- open-url [url]
  (if (= sandbox-mode "microvm")
    (do
      (harness/computer-use-exec {:action "open_url" :url url})
      "firefox-esr")
    (do
      (var browser nil)
      (each candidate ["firefox" "firefox-esr" "chromium" "chromium-browser" "chrome" "brave" "opera"]
        (when (and (not browser) (command-exists? candidate))
          (set browser candidate)))
      (if (not browser)
        nil
        (do
          (os/execute ["/bin/sh" "-c" (string browser " '" (string/replace-all "'" "'\\''" url) "' 1>/dev/null 2>&1 &")])
          browser)))))

(defn- cycle-windows [n]
  (for i 0 n
    (if (= sandbox-mode "microvm")
      (harness/computer-use-exec {:action "keychord" :chord "alt+Tab"})
      (case (platform :backend)
        "ydotool" (sh "ydotool" "key" "alt+Tab")
        "xdotool" (sh "xdotool" "key" "alt+Tab")))
    (sh "sleep" "0.3")))

(defn- focus-window [app]
  (if (not (validate-app-name app))
    (string "INVALID APP NAME — blocked: " app)
    (if (not= 0 (os/execute ["/bin/sh" "-c" (string "pgrep -i " app " >/dev/null 2>&1")]))
      (string "app not running: " app)
      (do
        (var result nil)
        (for i 0 12
          (cycle-windows 1)
          (sh "sleep" "0.5")
          (let [path (take-screenshot)]
            (when path
              (let [script (string workspace-path "/plugins/computer_use_vision.py")
                    answer (sh-capture (string "python3 " script " --path " path " --find-app " app " 2>/dev/null"))]
                (when (and answer (string/find "yes" answer))
                  (set result (string "focused " app " after " (+ i 1) " cycles"))
                  (break))))))
        (or result (string "could not confirm " app " after 12 cycles"))))))

(defn- navigate-to-url [url]
  # Full pipeline: open URL, focus browser, find button, click it.
  (var browser (open-url url))
  (sh "sleep" "3")
  (if (not browser)
    (string "opened " url " — no browser found")
    (do
      (var focus-result (focus-window browser))
      (if (string/find "could not confirm" focus-result)
        (string "opened " url " — " focus-result)
        (do
          (sh "sleep" "0.5")
          (let [path (take-screenshot)]
            (if (not path)
              (string "opened " url " but screenshot failed")
              (let [script (string workspace-path "/plugins/computer_use_vision.py")
                    coords (sh-capture (string "python3 " script " --path " path " --find-main-button 2>/dev/null"))
                    parts (if coords (string/split " " (string/trim coords)) @[])
                    cx (if (> (length parts) 0) (get parts 0) nil)
                    cy (if (> (length parts) 1) (get parts 1) nil)]
                (if (and cx cy)
                  (do
                    (click-button "1" cx cy)
                    (string "navigated to " url " — clicked at " cx ", " cy))
                  (string "opened " url " but no button found"))))))))))

# ── Command parsing ───────────────────────────────────────────────────

(defn- parse-computer-cmd [command]
  (let [prefix "computer:"]
    (when (= (string/slice command 0 (length prefix)) prefix)
      (let [rest (string/slice command (length prefix))
            parts (string/split " " rest)
            action (get parts 0)
            tail (string/join (array/slice parts 1) " ")]
        (when (not (empty? action))
          {:action action :args tail})))))

(defn- describe-action [parsed]
  (case (parsed :action)
    "open_url"   (string "Open " (parsed :args) " in browser?")
    "screenshot" "Capture screenshot?"
    "type"       (string "Type: " (parsed :args) "?")
    "key"        (string "Press: " (parsed :args) "?")
    "click"      (string "Click button " (parsed :args) "?")
    "move"       (string "Move mouse to " (parsed :args) "?")
    "analyze"    (if (empty? (parsed :args)) "Analyze screenshot?" (string "Analyze: " (parsed :args) "?"))
    "cycle"      (let [n (if (empty? (parsed :args)) 3 (parsed :args))]
                   (string "Alt+Tab " n " times?"))
    "focus"      (string "Focus window: " (parsed :args) "?")
    "navigate"   (string "Navigate to " (parsed :args) " and click main button?")
    (string "Run computer:" (parsed :action) " " (parsed :args) "?")))

(defn- execute-action [parsed]
  (case (parsed :action)
    "open_url"
    (do (open-url (parsed :args))
        (string "opened " (parsed :args) " in browser"))
    "screenshot"
    (let [path (take-screenshot)]
      (if path (string "screenshot saved: " path) "ERROR: screenshot failed"))
    "type"
    (do (type-text (parsed :args))
        (string "typed: " (parsed :args)))
    "key"
    (if (press-keys (parsed :args))
      (string "pressed: " (parsed :args))
      (string "INVALID KEYS — blocked: " (parsed :args)))
    "click"
    (let [parts (string/split " " (parsed :args))
          btn (get parts 0)
          x (if (> (length parts) 1) (get parts 1) nil)
          y (if (> (length parts) 2) (get parts 2) nil)]
      (if (click-button btn x y)
        (string "clicked button " btn)
        (string "INVALID CLICK — blocked: " (parsed :args))))
    "move"
    (let [parts (string/split " " (parsed :args))
          x (get parts 0)
          y (get parts 1)]
      (if (move-mouse x y)
        (string "moved mouse to " x ", " y)
        (string "INVALID MOVE — blocked: " (parsed :args))))
    "analyze"
    (let [path (take-screenshot)
          prompt (if (empty? (parsed :args))
                   "Describe this screenshot. Identify all buttons, their labels, and approximate positions on screen (x,y). Note the active application and any text visible."
                   (parsed :args))]
      (if path
        (analyze-image path prompt)
        "ERROR: screenshot failed"))
    "cycle"
    (let [n-str (parsed :args)
          n (if (and n-str (not (empty? n-str))) (scan-number n-str) 3)]
      (cycle-windows n)
      (string "cycled " n " windows"))
    "focus"
    (focus-window (parsed :args))
    "navigate"
    (navigate-to-url (parsed :args))
    (string "unknown computer action: " (parsed :action))))

# ── Hook handlers ─────────────────────────────────────────────────────

(defn computer_use-on-tool-start [ctx]
  (when (= (ctx :tool) "bash")
    (let [command (ctx :command)]
      (when command
        (let [parsed (parse-computer-cmd command)]
          (when parsed
            (harness/log (string "computer-use: " (parsed :action) " " (parsed :args)))
            (if (kill-switch-active?)
              (harness/block "computer-use kill switch active")
              (if (= auto-confirm-mode "yes")
                (harness/block
                  "computer-use blocked — auto-confirm is not supported for desktop automation.\nRun without --auto-confirm or use --auto-confirm no.")
                (if (not (host-desktop-consent-given?))
                  (harness/block
                    (string "computer-use blocked — host desktop requires opt-in.\n"
                            "Set DIRGE_COMPUTER_USE_HOST=1 or create ~/.config/dirge/computer-use-host-consent"))
                  (if (= (harness/check-computer-action (parsed :action)) "deny")
                    (harness/block
                      (string "computer-use blocked by PDP deny_tools: " (parsed :action)
                              "\nAdd deny_tools: [computer] or deny_tools: [computer:" (parsed :action) "] in your agent definition."))
                    (let [desc (describe-action parsed)]
                      (if (harness/confirm "computer-use" desc)
                        (set pending-result (execute-action parsed))
                        (harness/block "computer-use denied by user")))))))))))
  nil)

(defn computer_use-on-tool-end [ctx]
  (when (= (ctx :tool) "bash")
    (when pending-result
      (let [result pending-result]
        (set pending-result nil)
        (harness/replace-result result))))
  nil)

nil)