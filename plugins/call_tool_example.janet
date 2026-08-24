# harness/call-tool end to end.
#
# Registers `/deps <file>`, which reads a file through dirge's own `read`
# tool and reports its import lines — the small case the bridge exists for:
# run a real tool, keep the part that matters, throw the rest away.
#
# Note what is NOT here: no file reading, no path resolution, no permission
# handling. All of that belongs to the `read` tool and is inherited.

(defn- json-escape [s]
  (def out @"")
  (each byte s
    (cond
      (= byte 34) (buffer/push-string out `\"`)
      (= byte 92) (buffer/push-string out `\\`)
      (= byte 10) (buffer/push-string out `\n`)
      (= byte 13) (buffer/push-string out `\r`)
      (= byte 9) (buffer/push-string out `\t`)
      (< byte 32) (buffer/push-string out (string/format `\u%04x` byte))
      (buffer/push-byte out byte)))
  (string out))

(defn- import-lines [text]
  (def hits @[])
  (each line (string/split "\n" text)
    (def trimmed (string/trim line))
    (when (or (string/has-prefix? "use " trimmed)
              (string/has-prefix? "import " trimmed)
              (string/has-prefix? "from " trimmed)
              (string/has-prefix? "#include" trimmed))
      (array/push hits trimmed)))
  hits)

(defn deps-command [args]
  (def path (string/trim (or args "")))
  (when (empty? path)
    (break "usage: /deps <file>"))

  (unless (harness/tools?)
    (break "the tool bridge is not available in this build"))

  # args are a JSON string: the plugin env has no JSON encoder, and the
  # host re-parses whatever we hand it.
  (def result
    (harness/call-tool "read" (string `{"path":"` (json-escape path) `"}`)))

  (cond
    (nil? result) "the tool bridge went away mid-call"
    # `:ok false` is the tool's own failure — a missing file, a denied
    # permission — and its text is already user-facing.
    (not (result :ok)) (string "read failed: " (result :output))
    (let [hits (import-lines (result :output))]
      (if (empty? hits)
        (string "no imports found in " path)
        (string path " imports:\n" (string/join hits "\n"))))))

(harness/register-command "deps" "deps-command")
