# npm/yarn/pnpm output compressors — ported from lean-ctx
#
# Functions: npm-compress [command output] → compressed string or nil

(defn- npm-compress-install [output]
  (var packages @[])
  (var dep-count 0)
  (var time "")
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (if-let [c (match "\\+ ([^ ]+)@([^ ]+)" t)]
      (array/push packages (string (in c 0) "@" (in c 1))))
    (if-let [c (match "added ([0-9]+) packages?" t)]
      (set dep-count (scan-number (in c 0))))
    (if-let [c (match "in ([0-9]+\\.?[0-9]*\\s*[ms]+)" t)]
      (set time (in c 0))))
  (def pkg-str (if (empty? packages) "" (string "+" (string/join packages ", +"))))
  (def dep-str (if (> dep-count 0) (string " (" dep-count " deps") " ("))
  (def time-str (if (empty? time) ")" (string ", " time ")")))
  (if (and (empty? packages) (= dep-count 0))
    (string/join (take 5 (string/split "\n" output)) "\n")
    (if (and (empty? pkg-str) (> dep-count 0))
      (string "ok (" dep-count " deps" (if (empty? time) ")" (string ", " time ")")))
      (string pkg-str dep-str time-str))))

(defn- npm-compress-run [output]
  (def lines (filter
               (fn [l]
                 (def t (string/trim l))
                 (and (not (empty? t))
                      (not (string/has-prefix? ">" t))
                      (not (string/find "npm warn" (string/ascii-lower t)))
                      (not (string/find "npm fund" (string/ascii-lower t)))
                      (not (string/find "looking for funding" (string/ascii-lower t)))))
               (string/split "\n" output)))
  (if (<= (length lines) 15)
    (string/join lines "\n")
    (let [tail (take 10 (drop (- (length lines) 10) lines))]
      (string "...(" (length lines) " lines)\n" (string/join tail "\n")))))

(defn- npm-compress-test [output]
  (var passed 0)
  (var failed 0)
  (each line (string/split "\n" output)
    (def t (string/trim line))
    (def lo (string/ascii-lower t))
    (def up (string/ascii-upper t))
    # Try to extract test counts from summary line
    (if (string/find "Tests:" t)
      (do
        (if-let [c (match ".*([0-9]+) passed" lo)] (set passed (scan-number (in c 0))))
        (if-let [c (match ".*([0-9]+) failed" lo)] (set failed (scan-number (in c 0))))))
    # Count individual pass/fail indicators
    (when (or (string/has-prefix? "✓" t) (string/has-prefix? "PASS" up))
      (++ passed))
    (when (or (string/has-prefix? "✗" t) (string/has-prefix? "FAIL" up))
      (++ failed)))
  (if (or (> passed 0) (> failed 0))
    (string "tests: " passed " pass, " failed " fail")
    (string/join (take 10 (string/split "\n" output)) "\n")))

(defn- npm-compress-audit [output]
  (var total-vulns 0)
  (var critical 0)
  (var high 0)
  (var moderate 0)
  (var low 0)
  (each line (string/split "\n" output)
    (def lo (string/ascii-lower line))
    (if-let [c (match "([0-9]+) +(critical|high|moderate|low)" lo)]
      (let [count (scan-number (in c 0))
            sev (in c 1)]
        (+= total-vulns count)
        (if (= sev "critical") (+= critical count)
          (if (= sev "high") (+= high count)
            (if (= sev "moderate") (+= moderate count)
              (+= low count)))))))
  (if (= total-vulns 0)
    (if (or (string/find "no vulnerabilities" (string/ascii-lower output))
            (empty? (string/trim output)))
      "ok (0 vulnerabilities)"
      (string/join (take 5 (string/split "\n" output)) "\n"))
    (string total-vulns " vulnerabilities: "
            critical " critical, " high " high, "
            moderate " moderate, " low " low")))

(defn- npm-compress-outdated [output]
  (def lines (filter (fn [l] (not (empty? (string/trim l)))) (string/split "\n" output)))
  (if (<= (length lines) 1) (break "all up-to-date"))
  (var packages @[])
  (each line (tuple/slice lines 1)
    (def parts (filter (fn [p] (not (empty? p))) (string/split " " line)))
    (when (>= (length parts) 4)
      (array/push packages (string (in parts 0) ": " (in parts 1)
                                   " → " (in parts 3)
                                   " (wanted: " (in parts 2) ")"))))
  (if (empty? packages) "all up-to-date"
    (string (length packages) " outdated:\n" (string/join packages "\n"))))

(defn- npm-compress-list [output]
  (def lines (string/split "\n" output))
  (if (<= (length lines) 5) (break output))
  (def tree-chars @["├──" "└──" "+--" "`--"])
  (def top-level (filter
                   (fn [l] (or (string/has-prefix? (in tree-chars 0) l)
                              (string/has-prefix? (in tree-chars 1) l)
                              (string/has-prefix? (in tree-chars 2) l)
                              (string/has-prefix? (in tree-chars 3) l)))
                   lines))
  (if (empty? top-level)
    (string/join (take 10 lines) "\n")
    (let [cleaned (map (fn [l]
                         (def s1 (string/replace (in tree-chars 0) "" l))
                         (def s2 (string/replace (in tree-chars 1) "" s1))
                         (def s3 (string/replace (in tree-chars 2) "" s2))
                         (string/trim (string/replace (in tree-chars 3) "" s3)))
                       top-level)]
      (string (length cleaned) " packages:\n" (string/join cleaned "\n")))))

(defn npm-compress [command output]
  (if (not (or (string/find "npm " command)
               (string/find "yarn " command)
               (string/find "pnpm " command)))
    (break nil))
  (cond
    (or (string/find "install" command) (string/find " add " command) (string/find "ci" command))
    (npm-compress-install output)
    (string/find "run " command) (npm-compress-run output)
    (string/find "test" command) (npm-compress-test output)
    (string/find "audit" command) (npm-compress-audit output)
    (string/find "outdated" command) (npm-compress-outdated output)
    (or (string/find "list" command) (string/find "ls" command)) (npm-compress-list output)
    nil))

nil
