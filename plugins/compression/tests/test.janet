# Runnable checks for the compression plugin's truncation behavior.
#
#   janet plugins/compression/tests/test.janet   (from repo root)
#
# Lives in tests/ so the plugin loader (which scans only top-level
# *.janet files in the plugin dir) doesn't load it as plugin code.
# Loads the plugin files in the host's load order into one env (no
# `harness/*` needed — the compressor functions are pure), then asserts
# the head+tail truncation invariants. Not a cargo test (these run in
# the Janet worker, not Rust), but keeps the behavior pinned and
# locally verifiable. The PR that added compression shipped its tests
# in an external gist; this brings coverage in-repo.

(def here (string (os/cwd) "/plugins/compression"))
(defn load-plugin [name] (dofile (string here "/" name) :env (curenv)))

(load-plugin "00-regex.janet")
(load-plugin "05-util.janet")
(load-plugin "10-git.janet")
(load-plugin "50-misc.janet")

(var failures 0)
(defn check [name cond]
  (if cond
    (print "ok   - " name)
    (do (set failures (+ failures 1)) (print "FAIL - " name))))

(defn lines-of [s] (string/split "\n" s))

# ── truncate-lines: the core invariant ──────────────────────────────

(let [small (map string (range 5))]
  (check "short input is unchanged"
         (= (truncate-lines small 18 8 "x") (string/join small "\n"))))

(let [big (map string (range 100))
      out (truncate-lines big 18 8 "matching lines")]
  (check "long input keeps the HEAD" (string/has-prefix? "0\n1\n2" out))
  (check "long input keeps the TAIL (never silently dropped)"
         (string/find "\n99" out))
  (check "marker states hidden count + head/tail sizes"
         (and (string/find "74 of 100 matching lines hidden" out)
              (string/find "first 18 + last 8" out)))
  (check "exactly head+tail+1 marker line"
         (= (length (lines-of out)) (+ 18 8 1))))

# Boundary: head+tail == n → no truncation.
(let [exact (map string (range 26))]
  (check "head+tail == n is not truncated"
         (= (truncate-lines exact 18 8 "x") (string/join exact "\n"))))

# ── grep: head AND tail, both ends visible ──────────────────────────

(let [g (string/join (map (fn [i] (string "src/f" i ".rs:1:hit")) (range 50)) "\n")
      out (compress-grep-output g)]
  (check "grep shows the first match" (string/find "src/f0.rs" out))
  (check "grep shows the LAST match (regression fix)" (string/find "src/f49.rs" out))
  (check "grep marks hidden matches" (string/find "matching lines hidden" out)))

(let [tiny (string/join (map (fn [i] (string "m" i)) (range 10)) "\n")]
  (check "grep under threshold passes through" (= (compress-grep-output tiny) tiny)))

# ── find: honest total, no bogus file/dir split ─────────────────────

(let [paths (string/join (map (fn [i] (string "./a/b" i "/Makefile")) (range 40)) "\n")
      out (compress-find-output paths)]
  (check "find shows head paths" (string/find "./a/b0/Makefile" out))
  (check "find shows tail paths" (string/find "./a/b39/Makefile" out))
  (check "find reports paths-hidden, not a guessed file/dir count"
         (and (string/find "paths hidden" out)
              (not (string/find "dirs" out)))))

# ── ls: actual names head+tail, not a bare count ────────────────────

(let [entries (string/join (map (fn [i] (string "file" i ".txt")) (range 60)) "\n")
      out (compress-ls-output entries)]
  (check "ls shows real entry names (not just a count)"
         (and (string/find "file0.txt" out) (string/find "file59.txt" out)))
  (check "ls marks hidden entries" (string/find "entries hidden" out)))

# ── curl: head+tail unchanged in spirit, shared marker ──────────────

(let [body (string/join (map (fn [i] (string "{\"k\":" i "}")) (range 40)) "\n")
      out (compress-curl-output body)]
  (check "curl keeps head and tail"
         (and (string/find "\"k\":0" out) (string/find "\"k\":39" out)))
  (check "curl uses the shared marker" (string/find "lines hidden" out)))

# ── git diff: multi-file truncation keeps both ends of EVERY file ───

(let [mk (fn [name n] (string "diff --git a/" name " b/" name "\n"
                              (string/join (map (fn [i] (string "+" name i)) (range n)) "\n")))
      # 300 lines/file × 2 = 600 (>500 total, each file >250) → per-file
      # head+tail truncation path.
      out (git-compress "git diff" (string (mk "x" 300) "\n" (mk "y" 300)))]
  (check "git diff keeps first file head" (string/find "+x0" out))
  (check "git diff keeps first file's LAST line (off-by-one fix)"
         (string/find "+x299" out))
  (check "git diff keeps second file's last line" (string/find "+y299" out))
  (check "git diff marker names each file"
         (and (string/find "diff lines in x" out)
              (string/find "diff lines in y" out))))

# A small multi-file diff (<500 total) must pass through with every
# line of every file intact.
(let [mk (fn [name] (string "diff --git a/" name " b/" name "\n+" name "0\n+" name "1\n+" name "2"))
      out (git-compress "git diff" (string (mk "p") "\n" (mk "q")))]
  (check "small git diff keeps all of first file" (string/find "+p2" out))
  (check "small git diff keeps all of second file" (string/find "+q2" out)))

(print)
(if (= failures 0)
  (print "all checks passed")
  (do (print failures " check(s) FAILED") (os/exit 1)))
